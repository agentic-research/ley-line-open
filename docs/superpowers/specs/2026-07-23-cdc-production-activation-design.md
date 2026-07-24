# CDC Production Activation Design

Date: 2026-07-23
Bead: `ley-line-open-f16e53`

## Purpose

Ley-line-open already has a tested content-defined chunker, chunk-backed range
reader, freshness witnesses, and incremental write refresh. The subsystem is
not production-active: merged `main` has no production caller of
`create_chunked_content_schema` or `store_content_chunked`. Consequently, a
real arena continues to materialize an entire `nodes.record` for a small range
read unless a library consumer manually creates and populates the private CDC
tables.

This change supplies the missing production boundary. Operators opt in through
an explicit command or daemon flag; a reusable Rust API performs the same
idempotent activation in both cases.

## Decision

CDC activation is explicit and resumable.

- `leyline cdc enable --db <path>` activates an existing SQLite projection in
  place.
- The daemon accepts `--cdc`. It activates its persistent WAL-backed living
  database after initialization and before the first arena snapshot.
- A reusable API performs schema creation and deterministic node backfill for
  both entry points.
- Opening a writable graph without either opt-in does not create CDC tables or
  duplicate content.
- Once activated, the existing graph write path keeps manifests current and
  uses bounded incremental rechunking where a fresh prior manifest exists.

The CLI and CLI library expose a small `cdc` Cargo feature that enables
`leyline-fs/cdc`. It is included in their default, `all`, and `full` feature
sets because activation remains runtime opt-in and the chunker is a small
pure-Rust dependency. `leyline-fs` itself keeps CDC opt-in so non-CLI library
consumers retain control of their dependency surface.

## Alternatives Considered

### Automatic activation on every writable open

This makes future writes chunk-backed without a flag, but silently changes
database size and startup behavior. It also does not backfill existing rows,
so it fails the immediate materialize-on-read goal. Rejected.

### Automatically create empty tables, explicitly backfill

This gradually activates files when they are written, while a command handles
old rows. It still changes every writable database merely by opening it and
makes the storage generation less observable. Rejected in favor of one
explicit transition.

### Explicit command/API plus daemon flag

This makes storage amplification and migration work deliberate, supports
operator-visible reporting, and provides one shared implementation for raw
databases and real daemon arenas. Selected.

## Core API

The activation implementation lives in a focused CDC activation module rather
than expanding `chunked.rs`, which already owns chunk storage and read/write
correctness.

The public shape is:

```rust
pub struct ActivationOptions {
    pub batch_size: usize,
}

pub struct ActivationReport {
    pub eligible_nodes: u64,
    pub populated_nodes: u64,
    pub already_fresh_nodes: u64,
    pub processed_source_bytes: u64,
    pub manifest_rows: u64,
    pub unique_chunk_rows: u64,
    pub unique_chunk_bytes: u64,
}

pub fn activate_chunked_content(
    conn: &rusqlite::Connection,
    options: ActivationOptions,
) -> anyhow::Result<ActivationReport>;
```

`batch_size` controls query and progress cadence, not correctness. A zero
value is rejected. `populated_nodes` and `processed_source_bytes` describe work
performed by this invocation; the other counts describe committed final
state. Reports use checked integer conversions and fail on overflow rather
than wrapping.

The function:

1. Creates the CDC tables idempotently.
2. Selects eligible readable structural leaves from `nodes` where `kind = 0`
   and `record IS NOT NULL`, ordered by `id`. In the parsed projection this
   does not mean “source file”: source-file roots may be directory-like while
   their readable content lives in descendant leaves.
3. Treats a node as complete only when `has_chunked_content` proves its
   freshness witness matches the current `(size, mtime)`.
4. Calls the existing atomic per-node store for missing or stale manifests.
5. Derives resume state from committed fresh manifests; no independent
   checkpoint can drift from the data.
6. Returns deterministic counts and storage accounting read from committed
   database state.

Empty leaf records are eligible. They receive a freshness witness and an empty
manifest representation that must be distinguishable from “not activated.”
The existing freshness/read API will be adjusted test-first so an activated
empty file reads correctly without being perpetually reprocessed.

Directory-like nodes are never chunked.

## Transaction and Resume Semantics

Each node transition is atomic. A crash may leave earlier nodes committed and
later nodes untouched, but never a partial manifest for one node. Re-running
activation skips every fresh committed node and continues deterministically.

The whole database is not wrapped in one transaction because that would make a
large activation non-resumable and could retain a long-running WAL transaction.
Schema creation is idempotent. Reporting is computed after processing, so a
successful report describes committed state.

For `--db`, the SQLite file itself is the durable resume substrate.

For `daemon --cdc`, the existing `<control>.live.db` WAL database is the
durable resume substrate. Activation runs before `snapshot_to_arena`; that
publisher already grows the dual-buffer arena before advertising the new size
and advances `current_root` only after the inactive buffer is complete.
Killing the daemon during activation leaves committed WAL work available to
the next `--cdc` start and leaves the previously published arena root intact.

## CLI Behavior

`leyline cdc enable --db <path>`:

- Opens the database read/write.
- Refuses a database without the `nodes` table and names the expected
  projection contract.
- Emits bounded progress to stderr.
- Prints one stable human-readable summary on success.
- Supports `--json` for machine-readable `ActivationReport` output.
- Accepts `--batch-size`, with a documented nonzero default.
- Is idempotent; a second run reports all eligible rows already fresh and
  performs no manifest rewrites.

When the binary is built without the `cdc` feature, dispatch returns an
actionable compile-feature error instead of hiding the command.

`daemon --cdc` uses the same default options. The startup status remains
Initializing until activation and the first snapshot succeed; an activation
error transitions startup to Error and the daemon does not serve a
half-advertised generation.

## Compatibility and Ownership

`nodes.record` remains authoritative and byte-for-byte unchanged. CDC tables
are a private derived index owned by ley-line-open:

- `content_chunks`
- `content_manifest`
- `content_manifest_meta`

Therefore this change does not bump `leyline-schema`. Foreign and older arenas
without these tables remain valid and use the existing record fallback.
Mache and rosary need no schema writer change to consume an activated arena.
The private table shape has no separate version marker in this change; adding
one before an actual incompatible private migration would create state without
a present consumer.

## Error Handling

- A missing or malformed `nodes` contract fails before processing.
- SQLite errors retain node context.
- A stale manifest is rebuilt from authoritative `record`; it is never served
  or counted as complete.
- Per-node transaction failure rolls back that node and preserves previously
  committed nodes.
- Arena capacity is not estimated in the activation layer. The daemon's
  existing snapshot publisher measures serialized bytes and safely grows the
  arena.
- The raw `--db` command does not publish an arena. Operators can load the
  completed database using the existing load workflow, whose capacity error is
  explicit.

## Test-Driven Delivery

Every production behavior begins with a test that is observed failing for the
missing behavior.

The first RED test is a real consumer-path harness:

1. Parse a fixture into a mache-shaped SQLite projection.
2. Activate CDC through the wished-for public API.
3. Publish it through the real arena snapshot path.
4. Reopen it through the verified arena reader.
5. Read a 4 KiB interior range.
6. Assert the bytes equal `nodes.record` byte-for-byte.
7. Assert the traced source is `ContentSource::Chunked`.
8. Assert only overlapping chunk rows are touched.

Additional RED-GREEN cycles cover:

- schema creation and deterministic full backfill;
- second-run idempotence;
- injected interruption after a committed node followed by successful resume;
- stale witness rebuild;
- empty files and directories;
- subsequent small writes taking the incremental refresh path;
- foreign/unactivated database fallback;
- malformed database diagnostics;
- CLI human and JSON output;
- daemon `--cdc` activation before first publication;
- arena auto-growth when CDC duplication exceeds the initial capacity.

The existing byte-for-byte differential corpus, seeded CDC cases, mutation
gates, all-features check, and full `task ci` remain regression gates.

## Falsifiable Acceptance Gates

Activation is complete only when all of the following are demonstrated:

- A production command and daemon path call the shared activation API.
- The real consumer harness reports `ContentSource::Chunked`, not merely equal
  output.
- Every tested range matches authoritative bytes exactly.
- An interrupted run resumes without rewriting already-fresh manifests.
- A second complete run changes neither manifest hashes nor chunk-row counts.
- A post-activation small write uses `WriteRefreshOutcome::Incremental`.
- A foreign database remains readable through `ContentSource::Record`.
- The full documented CI task, including all features, is green.

Performance is reported but not used to weaken correctness. Activation records
wall time, source MiB/s, source bytes, and unique chunk bytes on the existing
mache-scale fixture. The later SIMD/affine scanner bead may replace the scalar
scanner only if it preserves boundary and hash identity byte-for-byte and
passes its separately predefined throughput gate.

## Documentation and Release

The implementation updates:

- CLI help and install documentation;
- architecture documentation describing explicit activation and ownership;
- the changelog, removing the stale claim that CDC writes are not wired;
- the release checklist with the consumer harness and all-features gate.

The activation, reachability GC, ownership contract, and consumer smoke test
must land before the next patch release. Reachability GC is a separate TDD
change under `ley-line-open-035363`; activation does not silently delete
unreachable chunks. The SIMD spike can be accepted or rejected by its gate
without blocking the patch release.
