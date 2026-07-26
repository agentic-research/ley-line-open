# SQLite CDC BlobStore Conformance Design

**Date:** 2026-07-24 · **Beads:** `ley-line-open-ef5c1d`, `ley-line-open-ef5c84`

## Decision
Production chunk-backed reads in `leyline-fs` will obey the formal
`leyline_core::BlobStore` contract. SQLite remains the physical backend so an
arena stays portable as one `.db`, but SQLite will no longer have a second
range-reconstruction algorithm or a weaker integrity contract.

It will add a transaction-scoped `SqliteBlobStore`, retain an
indexed SQLite selector for the requested manifest spans, and pass the selected
spans to one shared CDC reconstruction function.

## Problem

There are two distinct integrity domains:

1. `verify_arena_root` authenticates the serialized SQLite snapshot before
   deserialization.
2. `BlobStore::get(hash)` authenticates an individual blob when it is read.
The current production CDC path conflates them. It verifies the outer arena
once, opens a writable in-memory SQLite database, and later reads
`content_chunks.chunk_bytes` directly. A valid SQLite update can therefore
replace a chunk after the snapshot was accepted while leaving its
`chunk_hash`, manifest, size, and freshness witness unchanged. The public range
reader will return the substituted bytes.

The module documents this weaker behavior, but it conflicts with CDC's use of
the formal `BlobStore` contract and with the claim that content hashes identify
the bytes consumers receive.

## Falsifiable Claims

| Claim | Falsifiable requirement |
|---|---|
| C1: Snapshot integrity | Serialized arena mutation without a `current_root` advance makes open fail before SQLite deserialization. |
| C2: Blob integrity | Every successful selected fetch satisfies `BLAKE3(returned_bytes) == requested_chunk_hash`; mismatch is an integrity error, never absence or fallback. |
| C3: Coherent range | Freshness, indexed manifest selection, and selected fetches observe one SQLite transaction snapshot. |
| C4: Subfile work | Metadata and blob work are proportional to overlapping chunks; a corrupt non-overlapping chunk is not fetched. |
| C5: Atomic output | The destination changes only after complete validation and reconstruction; every error leaves it unchanged. |

## Architecture

### `SqliteBlobStore`

`SqliteBlobStore<'txn>` wraps the same SQLite read transaction used by the
public range read.

It implements:

- `put(bytes)`: compute the canonical BLAKE3 hash and insert the immutable
  `(chunk_hash, chunk_bytes)` pair. Existing-key behavior follows the clarified
  `BlobStore` contract.
- `get(hash)`: select bytes by key, recompute BLAKE3, and return an integrity
  error if the bytes do not match the key.
- `contains(hash)`: test physical key existence only. It does not vouch for
  bytes; only `get` does.

Backend-specific SQL, transaction handling, insertion, and GC remain in
`leyline-fs`. “One implementation” means one hash-verification contract and one
range-reconstruction algorithm, not one physical storage backend.

### Indexed manifest selection

SQLite retains a backend-specific selector because it can use
`content_manifest_span` to select only spans overlapping the requested range.
Loading the complete manifest and then filtering in Rust is forbidden because
it regresses metadata work to `O(file)`.

The selector returns a `ValidatedRangeManifest`:

- spans are ordered by `byte_offset`, then `seq`;
- offsets and lengths are non-negative and checked before conversion;
- lengths are non-zero;
- arithmetic is checked for overflow;
- selected spans exactly tile the in-file portion of the requested range;
- the selected blob length must equal the manifest length.

Gap, overlap, dangling reference, malformed length, and inconsistent span
errors fail closed.

### Shared reconstruction

`leyline_cdc` gains `read_range_into`, which owns:

- overlap arithmetic;
- checked slicing;
- `BlobStore::get` calls;
- blob-length validation;
- ordered output assembly.

It reconstructs into a private temporary buffer and copies into the caller's
buffer only after success. The existing `read_range` becomes a convenience
wrapper over this function rather than a second algorithm.

`leyline-fs::read_content_at_traced` performs:

1. Begin one SQLite read transaction.
2. Evaluate manifest freshness in that transaction.
3. If no fresh manifest exists, read authoritative `nodes.record` in that same
   transaction.
4. Select and validate only overlapping manifest spans.
5. Construct `SqliteBlobStore` over the transaction.
6. Reconstruct through `leyline_cdc::read_range_into`.
7. Commit the output to the caller's buffer.

An integrity error after choosing the chunked path is returned. It must not
silently retry through `nodes.record`, because fallback would hide corruption.

## BlobStore Contract Clarifications

`contains(hash)` means that the physical key exists. It is a cheap location
probe and does not imply integrity. `get(hash)` is the only operation that
vouches for `BLAKE3(bytes) == hash`.

The round-trip axiom is:

```text
Given an absent key or a valid pre-existing entry,
put(value) followed by get(hash(value)) returns value.
```

If `put` encounters an existing corrupt entry, it must either repair it
atomically or return an integrity error. It may not report successful storage
while leaving a subsequent `get` guaranteed to fail.

## Deterministic Falsification Matrix

Every test is binary: pass or fail. No statistical threshold is involved.

| Test | Required observation |
|---|---|
| Valid control | Public chunked read equals the authoritative byte slice and reports `ContentSource::Chunked`. |
| Pre-open corruption | Changing serialized arena bytes under the old root makes arena open fail. |
| Post-open corruption | A same-length bit flip in a selected `chunk_bytes` row makes the public read return an integrity error, leaves the destination unchanged, and does not fall back. |
| Non-overlap control | Corrupting a non-overlapping chunk does not affect the requested range and the corrupt chunk is not fetched. |
| Missing selected blob | A dangling selected hash returns an error. |
| Wrong-key blob | Valid bytes stored under a different selected hash return an integrity error. |
| Manifest topology | Gaps, overlaps, negative/zero/overflowing lengths, and blob/manifest length mismatches return errors without panic or short-read. |
| Range boundaries | Empty, exact-boundary, cross-boundary, EOF-straddling, wholly-past-EOF, and saturating ranges match authoritative slicing semantics. |
| Concurrent refresh/GC | Barrier-controlled races produce a coherent old snapshot, coherent new snapshot, or explicit error—never mixed bytes, fallback, or panic. |
| Work bound | The query plan uses `content_manifest_span`; fetched blob count equals exactly the overlapping selected chunks. |
| Shared conformance | `MemBlobStore`, `FsBlobStore`, and `SqliteBlobStore` pass the same round-trip, idempotence, absence, corruption, and `contains` semantics suite. |

The post-open corruption test is the characterization test expected to fail
against the current implementation. It is the direct counterexample to the
claim that outer arena verification is sufficient for subfile integrity.

## Identity Boundaries

The following names must remain distinct in code and documentation:

- **Segment root:** canonical Cap'n Proto segment identity used by `Head`.
- **Arena snapshot root:** BLAKE3 of serialized SQLite bytes published through
  `Controller.current_root`.
- **Blob hash:** BLAKE3 identity of one immutable chunk fetched through
  `BlobStore`.
- **SQL projection ABI:** table/query compatibility; not a content root.

None of these values is assumed equal unless a named derivation function and a
test establish that relationship.

## Explicit Boundary

This change authenticates selected blob bytes and validates manifest structure.
It does not make a semantically valid manifest authentic.

For example, replacing a manifest hash with another valid, same-length chunk
can satisfy blob verification and topology checks. End-to-end file
authenticity would require a per-node content or manifest root anchored in
trusted published state. That is a separate protocol decision and must not be
claimed by this implementation.

Likewise, the current `(size, mtime)` freshness witness is not a cryptographic
identity. This design preserves it as cache-coherence metadata and does not
describe it as authentication.

## Documentation Changes

The implementation must update:

- `rs/ll-open/fs/src/chunked.rs` to remove the “no verify-on-read” exception;
- `README.md`, `docs/ARCHITECTURE.md`, and `docs/TABLE_CONTRACT.md` to use the
  four identity names above instead of overloading “substrate”;
- `leyline_core::BlobStore` documentation to pin `contains` and existing-key
  `put` semantics.

Documentation contract tests will reject contradictory authority statements,
including simultaneously calling canonical segment bytes and the mutable
SQLite `.db` “the contract.”

## Non-Goals

- Moving chunk bytes out of SQLite.
- Making all SQLite tables content-addressed.
- Equating the Cap'n Proto segment root with the arena snapshot root.
- Cryptographically authenticating manifest meaning or `nodes.record`.
- Changing CDC boundary selection or incremental rechunking.
