# ADR-0033: CDC chunk-backed content — a derived chunk index over `source_blobs`

- **Status**: accepted (retroactive — records decisions shipped incrementally since 0.10.x; written under bead `ley-line-open-b6653a`). **Amended 2026-11-04** (bead `ley-line-open-afa952`): the per-node `nodes` target was removed; `source_blobs` is the one target. See D5.
- **Date**: 2026-07-29
- **Beads**: `ley-line-open-9989d2` (origin), `ley-line-open-b6653a` (this record), `ley-line-open-baa57f` (`source_blobs` target), `ley-line-open-afa952` (`nodes` target removed)
- **Related**: [ADR-0026 (content-addressed pointer store)](./0026-content-addressed-pointer-store.md), [ADR-0028 (content-addressed source blobs)](./0028-content-addressed-source-blobs.md), [ADR-0032 §D4 (authority arrows)](./0032-declared-decompositions.md), [ley-line ADR-014 (CDC + fountain-code composition)](https://github.com/agentic-research/ley-line/blob/main/docs/design/014-cdc-fountain-composition.md)

---

## 0. One-line claim

The SQL projection carries a **derived, private chunk index** (`content_chunks` + `blob_manifest`) built by content-defined chunking over the content-addressed, immutable `source_blobs` table, so a range read touches only the chunks that overlap it, and identical content is stored once.

---

## 1. Context

### 1.1 The read-shape problem

`SqliteGraphAdapter::read_content` served a range read by `SELECT record` — loading the **entire** file — and then slicing. A 4 KiB mount read of a 100 MB file materialized 100 MB. That is a storage-shape problem, not a reader problem, so the fix belongs at the SQL layer.

### 1.2 The gleaning (bead `9989d2`)

Rather than invent a chunking scheme, LLO composes HuggingFace's [`gearhash`](https://crates.io/crates/gearhash) crate (their SIMD-accelerated CDC rolling hash — the proven hard part) and gleans only xet's published boundary rule: target ~64 KiB, clamped to `[8, 128]` KiB. Each chunk is addressed by σ = BLAKE3 (`leyline_core::ContentAddressed`), which is xet's `MerkleHash` base — LLO chunk identity aligns with xet's rather than introducing a foreign scheme. The load-bearing, test-falsified property is **boundary stability**: an edit changes only the chunks in its own region, so a small edit re-stores O(1) chunks rather than O(file).

### 1.3 Where it sits

This is ADR-0026's thesis ("the SQL projection should be a lightweight index into content-addressed blobs, never re-materialize") applied at **chunk** granularity, using the same arena-local blob-table pattern as `source_blobs` / `capnp_blobs`, so an arena stays one portable `.db`.

---

## 2. The shipped design (summary; `rs/ll-open/fs/src/chunk_pool.rs` and `rs/ll-open/fs/src/blob_chunked.rs` module docs are normative)

- **`content_chunks`** — the content-addressed chunk pool (σ → bytes), so identical chunks are stored once. `SqliteBlobStore` verifies σ on every read and fails closed on mismatch.
- **`blob_manifest`** — per-blob ordered spans into the pool. A range read becomes a SQL `WHERE` clause: only rows whose span overlaps `[offset, offset+len)` are selected, and only those chunks' bytes are read. No witness tables — see D3.
- **Explicit lifecycle** — activation (`leyline cdc enable`, `daemon --cdc`) and reachability GC (`leyline cdc gc`) are explicit operator commands, off the write path. GC reaps manifests whose `source_blobs` row is gone first (a dead manifest still *pins* its chunks, bead `b5e56f`), then deletes chunks no surviving manifest references.

## D1 — The dual store is permanent; the derived index never replaces the authoritative record

`source_blobs` holds the authoritative bytes, and ADR-0032 §D4 assigns the SQL projection no identity authority at all. The chunk tables are therefore a DERIVED accelerator: no root, no cross-process contract, never the canonical substrate. Arenas without chunk tables are a permanent, valid input — not a migration state to be finished and deleted. CDC may never be described as replacing the authoritative record.

## D2 — The target is explicit, not heuristic

The derived index is built over exactly one authoritative table, `source_blobs` (whole-file content, ADR-0028), and only when an operator asks. ADR-0028 §2.2 chose per-file blobs and named CDC as the anticipated downstream refinement for sub-file dedup; the `source_blobs` target (bead `baa57f`) is that refinement. No heuristic ("chunk whatever table looks big") is permitted: a target must state its freshness regime (D3), and a new one needs its own argument and its own ADR amendment.

## D3 — `source_blobs` manifests carry no freshness witness, and activation skips sub-floor rows

**Witness-free.** `source_blobs.blob_hash` = BLAKE3(`blob_bytes`), populated by `INSERT OR IGNORE`: no writer can change the bytes under a key, so "a new version" is a different row. A blob manifest row's existence IS its freshness proof. The obligations move to the two edges that remain: the store path refuses a blob whose bytes do not hash to its claimed key and proves complete tiling (`Manifest::parse`) before inserting a single row, and GC reaps manifests whose `source_blobs` row was deleted — garbage, never wrong bytes, because reads resolve chunks by content address.

**Sub-floor rows are skipped by design.** A row below the 8 KiB chunking floor (`leyline_cdc::MIN_CHUNK`) chunks to exactly one chunk identical to itself: zero dedup beyond what `source_blobs`' own `INSERT OR IGNORE` already provides, at the cost of a manifest row, a pool row, and two index entries. Activation skips sub-floor rows and COUNTS them (`skipped_sub_floor_blobs` in every report), so the policy is operator-visible rather than silent.

## D4 — Superseded

D4 recorded that switching a database between the `nodes` and `source_blobs` targets was additive and that each activation report counted the other target's stranded manifest rows (`ley-line-open-1869d0`). With one target there is nothing to switch between; the stranded-row counts were removed with the `nodes` target (D5).

## D5 — The `nodes` target was removed (2026-11-04, bead `ley-line-open-afa952`)

Until `afa952` the index could also be built over construct-granular `nodes.record` rows, with a freshness witness (`content_manifest_meta`, a trigger-maintained `content_generation`, and the `WITNESS_FRESH_PREDICATE` read gate) because those rows mutate in place, and with incremental re-chunking on graph writes. It was removed because it did not pay: on a real mache projection (bead `baa57f`), **395,173 of 395,173** leaf nodes sat below the chunking floor, so every manifest described one chunk identical to its source, and activating them added **+21% database size — 440 MB of manifest and witness overhead for 1.9 MB of dedup**. Its only production caller was the `leyline cdc enable` / `daemon --cdc` default, which mache had already stopped using (mache spawns `daemon --cdc --cdc-target source-blobs`).

What was removed: `fs/src/chunked.rs` (the node manifest, witness tables, generation triggers, freshness predicate, chunked graph reads and incremental write refresh), the `nodes` activation walk, GC's node-manifest reaper, `cdc enable --target`, the execution daemon's `--cdc-target`, and the `nodes` value of the daemon's `--cdc-target`. The daemon keeps `--cdc-target source-blobs` because mache passes it. `SqliteGraphAdapter` reads always serve `nodes.record`.

A database activated under the old `nodes` target keeps its `content_manifest`, `content_manifest_meta` and `content_generation` tables and the triggers on `nodes`. Nothing reads them; `leyline cdc gc` no longer counts `content_manifest` as a reference, so it collects the chunks only that table named. Rebuilding the database (`daemon --reset-arena`) drops the rest.

---

## 3. Falsifiability (as shipped, not planned)

The load-bearing claims are pinned by counted tests, not thresholds:

- boundary stability and xet-bound chunk sizes (`rs/ll-open/cdc`), with the mean chunk size held in a two-sided band (bead `ae432c`);
- range reads select exactly the overlapping manifest rows (`BLOB_OVERLAP_PREDICATE` has one definition and the seek floor is sound and tight: `seek_floor_is_sound_and_tight`);
- GC never deletes a chunk a live blob manifest references (`gc_keeps_chunks_referenced_only_by_blob_manifests`), reclaims exactly the unshared chunks of a deleted blob, and does not count a legacy `content_manifest` as a reference (`a_legacy_node_manifest_does_not_pin_chunks`);
- sub-floor rows are skipped with exact counts (`sub_floor_blobs_are_skipped_not_manifested`);
- `blob_chunked.rs`, `chunk_pool.rs` and `gc.rs` are on the mutation-testing allowlist (`task mutants`).

---

## 4. Non-goals

- No wire/transport composition (fountain codes, xet CAS protocol) — ley-line ADR-014 territory, out of scope for the projection-local index.
- No second target without its own freshness argument (D2).
- No automatic activation. Opening a writable graph never creates or populates CDC tables; activation and GC stay explicit operator commands.

---

## 5. Provenance

- **2026-07-22** (`9989d2`, PR #254): xet gleaning — compose `gearhash`, glean the boundary rule, address chunks by σ.
- **0.10.2 → 0.10.3** (2026-07-23/24): chunk store + manifest + freshness witness; incremental CDC writes; explicit activation and GC.
- **2026-07-29** (`b82f56`): the `(size, mtime)` witness heuristic replaced by trigger-maintained generations.
- **2026-07-29** (`f8ebe7`, 0.13.0): carried-over manifest rows verified by existence probe.
- **2026-07-29** (`baa57f`): the `source_blobs` target, after measurement showed the `nodes` walk pays the floor penalty on 100% of a real projection's rows.
- ADR-0033 was a reserved gap between 0032 and 0034 until this record was written (bead `b6653a`).
- **2026-11-04** (`afa952`): the `nodes` target removed (D5).
