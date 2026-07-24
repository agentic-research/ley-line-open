# Incremental CDC Write Path Design

Status: approved
Bead: `ley-line-open-bd8d33`
Follow-up: `ley-line-open-0330c7`

## Problem

v0.10.2 shipped both halves needed for incremental content-defined chunking,
but only connected the read half:

- `leyline-cdc::rechunk_with_stats` is proven exact against full chunking and
  exposes the work needed to falsify its sublinear-scan claim.
- `leyline-fs` stores chunk manifests and serves range reads by selecting only
  overlapping chunks.
- `SqliteGraphAdapter::write_content` still calls
  `refresh_chunked_content`, which calls `store_content_chunked`, which runs a
  full `leyline_cdc::chunk` over the new file.

Consequently a small write stores only the few new content-addressed chunks,
but still hashes the entire file. The previous session explicitly documented
this limit after an adversarial review caught the earlier, inflated claim that
the write path was already incremental.

The design must wire the existing incremental primitive into the real graph
write path without weakening the structural freshness guarantee. It must also
make the optimization observable: exact output alone cannot distinguish an
incremental scan from a full scan.

## Existing invariants

The implementation preserves these contracts:

1. `nodes.record` remains authoritative.
1. A manifest is readable only when `content_manifest_meta.(source_len, source_mtime)` matches the live `nodes` row.
1. A missing, stale, or unusable manifest must not be used as an incremental
   base.
1. Replacing manifest spans and their freshness witness is atomic.
1. Foreign arenas without chunk tables are not silently upgraded.
1. Chunk bytes remain content-addressed and are inserted with
   `INSERT OR IGNORE`.
1. The result of incremental re-chunking must equal `chunk(new_data)` exactly.

The explicit invalidation calls remain useful hygiene, but the freshness
witness is the structural correctness guard. The separate
`ley-line-open-0330c7` bead remains responsible for any broader writer
abstraction or invalidation-policy cleanup.

## Considered approaches

### 1. Compute a byte diff inside the refresh function

Read the old `nodes.record`, compare it to the new bytes, derive an edit range,
then call `rechunk_with_stats`.

This keeps the graph call site small, but finding the edit requires scanning
the old and new whole-file values. It would replace one O(file) scan with
another and defeat the write-latency goal.

### 2. Pass the known write edit and a verified old-manifest snapshot

Before updating `nodes.record`, capture the old manifest only if its freshness
witness matches the old row. The graph writer already knows the write offset,
old length, and overwrite length, so it can describe the edit in old
coordinates without diffing. After the authoritative row update, refresh the
manifest using `rechunk_with_stats`.

This is the selected approach. It adds a small amount of call-site plumbing,
but preserves sublinear work and makes the freshness boundary explicit.

### 3. Introduce a mutable chunk tree or SQLite trigger/UDF write engine

Make chunk maintenance the primary write representation and derive
`nodes.record`, or push chunk maintenance into SQLite triggers.

This could eventually make chunking structural across every writer, but it is
a storage-contract redesign. It crosses into `ley-line-open-0330c7`, foreign
writers, and schema compatibility. It is unnecessary to connect the already
proven primitive.

## Components

### Verified old-manifest snapshot

`chunked.rs` gains a crate-private snapshot type containing:

- ordered `leyline_cdc::Chunk` entries;
- the witnessed old source length;

The capture query verifies the witnessed source mtime against the live row but
does not retain it after validation; incremental rechunking only consumes the
old chunks and source length.

The capture function returns a snapshot only when:

- the chunk tables exist;
- the node, manifest metadata, and manifest spans exist;
- metadata matches the current node `(size, mtime)`;
- hashes decode to 32-byte BLAKE3 values;
- ordered spans tile `[0, source_len)` without gaps or overlaps.

No chunk bytes need to be materialized. Re-chunking needs only old hashes and
spans plus the new authoritative bytes.

A missing or stale manifest is an expected `None`. SQL failures propagate.
Malformed manifest data is reported explicitly and cannot become an
incremental base.

### Edit coordinates

For `write_content(id, data, offset)`, define the edit in old coordinates:

- `edit_offset = min(offset, old_len)`;
- `old_edit_end = min(offset + data.len(), old_len)`;
- `old_len = existing.len()`.

Clamping `edit_offset` to `old_len` covers writes beyond EOF: the zero-filled
gap and appended data are one insertion at the previous EOF. Checked arithmetic
must reject an offset/length overflow rather than wrapping.

An empty write keeps the graph method's current semantics. If it extends the
file because its offset is beyond EOF, it is still an insertion from old EOF;
otherwise it is a no-op. The incremental primitive may reuse every chunk, but
the manifest freshness witness is still advanced to the row's new mtime.

### Manifest refresh

The edit-aware refresh accepts:

- the optional verified snapshot;
- the new bytes;
- the edit coordinates.

If the arena has no chunk schema, it returns `Skipped`. If no verified snapshot
exists, it performs a full chunk and returns `Full`. Otherwise it calls
`rechunk_with_stats` and returns `Incremental(stats)`.

Both full and incremental paths feed one manifest-storage function. That
function atomically:

1. deletes the old spans;
1. inserts missing chunk blobs;
1. inserts the new ordered spans;
1. replaces the freshness witness from the now-current `nodes` row.

If refresh fails after `nodes.record` changed, the old witness no longer
matches the new row, so reads fall back to the authoritative record rather than
serving stale chunks.

### Observable outcome

The crate-private refresh result distinguishes:

- `Skipped`;
- `Full`;
- `Incremental(RechunkStats)`.

`SqliteGraphAdapter` gains a crate-private `write_content_traced` method that
returns the byte count plus this outcome. The existing `Graph::write_content`
implementation delegates to it and discards only the trace. Tests call the
traced method, so they cover both sides of the boundary: the incremental
storage function and the graph call site that previously remained unwired.

This is production observability, not a test-only hook. It also gives the graph
layer a future logging/counting seam without global counters that would make
parallel tests race. No new SQLite table, public trait method, or wire-format
field is required.

## Strict test-driven sequence

No production edit occurs before the first test is written and observed RED.

### Differential graph-write harness

One table-driven harness owns the byte-for-byte correctness proof. It keeps a
plain `Vec<u8>` model, applies the exact `Graph::write_content` overwrite and
zero-fill semantics to that model, and drives the same edit through the real
SQLite graph adapter. After every write it:

1. computes the oracle manifest with `leyline_cdc::chunk(&model)`;
1. reads the real manifest as ordered `(hash, offset, len)` tuples;
1. requires tuple-for-tuple equality with the oracle;
1. reconstructs the whole file through the chunk-backed reader and requires
   byte-for-byte equality with the model.

The case table covers a deep small overwrite, append at EOF, write beyond EOF
with a zero-filled gap, equal-length overwrite across chunk boundaries, and an
empty write. Deterministic pseudo-random source bytes ensure the file has many
content-defined boundaries without fixtures or timing thresholds.

The harness is deliberately an integration oracle, not another test of the
chunking algorithm. `leyline-cdc` already fuzzes
`rechunk(new) == chunk(new)` directly; this harness proves the graph/SQLite
wiring produces the same durable representation.

### RED/GREEN order

1. Add the harness with a deep three-byte edit calling the wished-for
   `write_content_traced` method. Run it and record RED because the edit-aware
   graph behavior is absent.
1. Add only the minimal outcome/API shell needed to compile. Re-run and record
   semantic RED: the current path reports `Full`, not `Incremental`.
1. Capture the verified old manifest and call `rechunk_with_stats`. Turn the
   first case GREEN only when the differential manifest and reconstructed bytes
   match and:
   - `prefix_kept` and `tail_reused` are non-zero;
   - `bytes_scanned` is bounded by a small multiple of `MAX_CHUNK`, not file
     length.
1. Add the remaining case-table rows one at a time. Each new row is run RED
   before any corresponding coordinate or fallback change.
1. Add a small fallback table for absent manifest (`Full`), stale witness
   (`Full`), and foreign arena (`Skipped`). Existing freshness tests already
   prove stale manifests cannot serve bytes; do not duplicate that suite.
1. Refactor shared full/incremental manifest storage only after the harness and
   fallback table are GREEN.

Every RED invocation and expected failure is recorded on the bead before the
corresponding production change.

## Validation

The implementation is complete only after:

- focused RED/GREEN evidence is recorded;
- `cargo test -p leyline-fs --no-default-features --features cdc,splice,validate` passes from `rs/`;
- the existing `leyline-cdc` exactness and work-bound tests pass;
- `task ci` passes;
- `task mutants:cdc` and `task mutants:fs` report no missed mutants;
- the configured pre-commit and pre-push Taskfile gates pass;
- `CHANGELOG.md` describes incremental write-side CDC under the next release
  section.

This change does not modify `leyline-schema`, Cap'n Proto schemas, SQLite DDL,
or wire-format versions. A later release may bump crate/package versions as
part of the normal release PR, but the CDC implementation itself requires no
schema-version change.
