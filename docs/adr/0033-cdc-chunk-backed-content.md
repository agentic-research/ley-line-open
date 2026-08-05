# ADR-0033: CDC chunk-backed content — a derived chunk index over declared targets

- **Status**: accepted (retroactive — records decisions shipped incrementally since 0.10.x; written under bead `ley-line-open-b6653a`)
- **Date**: 2026-07-29
- **Beads**: `ley-line-open-9989d2` (origin), `ley-line-open-b6653a` (this record), `ley-line-open-baa57f` (`source_blobs` target)
- **Related**: [ADR-0026 (content-addressed pointer store)](./0026-content-addressed-pointer-store.md), [ADR-0028 (content-addressed source blobs)](./0028-content-addressed-source-blobs.md), [ADR-0032 §D4 (authority arrows)](./0032-declared-decompositions.md), [ley-line ADR-014 (CDC + fountain-code composition)](https://github.com/agentic-research/ley-line/blob/main/docs/design/014-cdc-fountain-composition.md)

---

## 0. One-line claim

The SQL projection carries a **derived, private chunk index** (`content_chunks` + per-target manifests) built by content-defined chunking over **explicitly declared** authoritative tables — `nodes` (mutable, witness-gated) and `source_blobs` (content-addressed, witness-free) — so a range read touches only the chunks that overlap it, and identical content is stored once.

---

## 1. Context

### 1.1 The read-shape problem

`SqliteGraphAdapter::read_content` served a range read by `SELECT record` — loading the **entire** file — and then slicing. A 4 KiB mount read of a 100 MB file materialized 100 MB. That is a storage-shape problem, not a reader problem, so the fix belongs at the SQL layer.

### 1.2 The gleaning (bead `9989d2`)

Rather than invent a chunking scheme, LLO composes HuggingFace's [`gearhash`](https://crates.io/crates/gearhash) crate (their SIMD-accelerated CDC rolling hash — the proven hard part) and gleans only xet's published boundary rule: target ~64 KiB, clamped to `[8, 128]` KiB. Each chunk is addressed by σ = BLAKE3 (`leyline_core::ContentAddressed`), which is xet's `MerkleHash` base — LLO chunk identity aligns with xet's rather than introducing a foreign scheme. The load-bearing, test-falsified property is **boundary stability**: an edit changes only the chunks in its own region, so a small edit re-stores O(1) chunks rather than O(file).

### 1.3 Where it sits

This is ADR-0026's thesis ("the SQL projection should be a lightweight index into content-addressed blobs, never re-materialize") applied at **chunk** granularity, using the same arena-local blob-table pattern as `source_blobs` / `capnp_blobs`, so an arena stays one portable `.db`.

---

## 2. The shipped design (summary; `rs/ll-open/fs/src/chunked.rs` module docs are normative)

- **`content_chunks`** — the shared, content-addressed chunk pool (σ → bytes). One pool for every target and every file, so identical chunks are stored once.
- **`content_manifest`** — per-node ordered spans into the pool. A range read becomes a SQL `WHERE` clause: only rows whose span overlaps `[offset, offset+len)` are selected, and only those chunks' bytes are read.
- **Freshness witness** (`content_manifest_meta` + `content_generation` + triggers) — a `nodes` manifest records the length and *mutation generation* of the row it was built from; a read compares them against the row's CURRENT values and REFUSES a manifest whose source moved on. A missed invalidation degrades to slow-but-correct, never to silently wrong. The generation is bumped by triggers on `nodes`, so foreign writers that have never heard of the CDC layer still invalidate correctly (bead `b82f56`).
- **`blob_manifest`** — per-blob ordered spans into the same pool (`rs/ll-open/fs/src/blob_chunked.rs`), with **no witness tables** — see D3.
- **Incremental refresh** — graph writes re-chunk only the edit's bounded rescan window (`leyline_cdc::rechunk_with_stats`); carried-over rows are verified by existence probe, never re-hashed (bead `f8ebe7`).
- **Explicit lifecycle** — activation (`leyline cdc enable`) and reachability GC (`leyline cdc gc`) are explicit operator commands, off the write path. GC reaps dead manifests first (a manifest every read refuses still *pins* its chunks, bead `b5e56f`), then deletes chunks no surviving manifest — in **either** table — references.

## D1 — The dual store is permanent; the derived index never replaces the authoritative record

`nodes.record` remains the cross-runtime ABI (mache writes it, leyline-fs reads it — `leyline-schema`'s contract), and ADR-0032 §D4 assigns the SQL projection no identity authority at all. The chunk tables are therefore a DERIVED accelerator for this crate's read path: no root, no cross-process contract, never the canonical substrate. Arenas without chunk tables are a permanent, valid input — not a migration state to be finished and deleted — and both read paths are pinned to return identical bytes. CDC may never be described as replacing `nodes.record`.

## D2 — Targets are explicit, not heuristic

The derived `content_*` index may be built over exactly two authoritative tables, each declared by the operator (`leyline cdc enable --target nodes|source-blobs`, a closed `ValueEnum`):

| Target | Rows | Mutability | Freshness design |
|---|---|---|---|
| `nodes` | construct-granular records | mutable in place | witness-gated (generation + length, trigger-maintained) |
| `source_blobs` | whole-file content (ADR-0028) | content-addressed, immutable | witness-free — existence is freshness (D3) |

ADR-0028 §2.2 chose per-file blobs and named CDC as the anticipated downstream refinement for sub-file dedup; the `source_blobs` target (bead `baa57f`) is that refinement landing. No heuristic ("chunk whatever table looks big") is permitted: the two targets have different freshness obligations, and a target that guessed wrong would either serve stale bytes (missing witness on a mutable table) or pay witness overhead nothing can ever trip (witness on an immutable table).

The chunk POOL is shared across targets deliberately: cross-target dedup — the same bytes reached as a `nodes` record and as a `source_blobs` row cost one pool row — is a property of the pool, not of either manifest. GC's unreachable-chunk predicate is correspondingly two-armed: a chunk is collectable only when NO manifest of EITHER target references it.

## D3 — `source_blobs` manifests carry no freshness witness, and activation skips sub-floor rows

**Witness-free.** `source_blobs.blob_hash` = BLAKE3(`blob_bytes`), populated by `INSERT OR IGNORE`: no writer can change the bytes under a key, so "a new version" is a different row. The entire witness apparatus the `nodes` target carries defends against a writer class that cannot exist here. A blob manifest row's existence IS its freshness proof. The obligations move to the two edges that remain: the store path refuses a blob whose bytes do not hash to its claimed key and proves complete tiling (`Manifest::parse`) before inserting a single row, and GC reaps manifests whose `source_blobs` row was deleted — garbage, never wrong bytes, because reads resolve chunks by content address.

**Sub-floor rows are skipped by design.** A row below the 8 KiB chunking floor (`leyline_cdc::MIN_CHUNK`) chunks to exactly one chunk identical to itself: zero dedup beyond what `source_blobs`' own `INSERT OR IGNORE` already provides, at the cost of a manifest row, a pool row, and two index entries. This is not hypothetical — it is the measured failure mode of pointing activation at the wrong granularity (bead `baa57f`): on a real mache projection, **395,173 of 395,173** leaf nodes sat below the floor, and activating them added **+21% database size — 440 MB of overhead for 1.9 MB of dedup**. `source_blobs` activation therefore skips sub-floor rows and COUNTS them (`skipped_sub_floor_blobs` in every report), so the policy is operator-visible rather than silent.

## D4 — Switching a database's target is additive, not a migration; the abandoned index is reported, not reclaimed

An operator MAY activate a target, later activate the other one on the SAME database (`ley-line-open-c3d746` made this reachable from the daemon, not only the standalone CLI), and expect the first target's manifest to still be sitting there afterward. Nothing removes it: each target's activation only writes its own tables (D2), and GC's dead-manifest reap (§2, bead `b5e56f`) only reaps rows with a stale witness or a deleted source row — a `nodes` manifest whose row still exists and has not mutated is FRESH by that predicate, not dead, whether or not anything still reads it. A target switch therefore leaves the arena carrying two indexes indefinitely, and GC keeps pinning the abandoned one's chunks right alongside the live one's.

This is deliberate scope, not an oversight: reclaiming an abandoned target on request is a real feature (an operator-invoked `--drop-target`, matching activation's own "explicit, never guessed" posture) that this ADR does not ship. What ships instead is the cheap half — the count is never silent. Every activation report carries the OTHER target's resident manifest row count (`ActivationReport::stranded_source_blobs_manifest_rows`, `BlobActivationReport::stranded_nodes_manifest_rows`; 0 when that target was never activated on this database), following the same "count it and print it" precedent as D3's `skipped_sub_floor_blobs` — the CLI (`leyline cdc enable`, both targets) and the daemon (`--cdc --cdc-target`) both print it. An operator planning a target switch on a long-lived arena should expect the storage cost of the prior target's index to persist until an explicit reclaim path exists (`ley-line-open-1869d0`).

---

## 3. Falsifiability (as shipped, not planned)

The load-bearing claims are pinned by counted tests, not thresholds:

- boundary stability and xet-bound chunk sizes (`rs/ll-open/cdc`), with the mean chunk size held in a two-sided band (bead `ae432c`);
- range reads select exactly the overlapping manifest rows (`OVERLAP_PREDICATE` has one definition; the selection is oracle-checked), for both targets;
- a stale `nodes` witness is refused (`same_shape_replacement_by_a_foreign_writer_is_not_served_stale`);
- cross-target dedup is real (`identical_content_in_nodes_and_source_blobs_shares_one_chunk_pool`);
- GC never deletes a chunk referenced only by a blob manifest (`gc_keeps_chunks_referenced_only_by_blob_manifests`) and reclaims exactly the unshared chunks of a deleted blob;
- sub-floor rows are skipped with exact counts (`sub_floor_blobs_are_skipped_not_manifested`);
- `chunked.rs` and `gc.rs` are on the mutation-testing allowlist (`task mutants`).

---

## 4. Non-goals

- No wire/transport composition (fountain codes, xet CAS protocol) — ley-line ADR-014 territory, out of scope for the projection-local index.
- No third target without its own freshness argument. The two shipped targets bracket the design space (mutable + witnessed, immutable + witness-free); a new table must state which regime it is in and why.
- No automatic activation. Opening a writable graph never creates or populates CDC tables; activation and GC stay explicit operator commands.

---

## 5. Provenance

- **2026-07-22** (`9989d2`, PR #254): xet gleaning — compose `gearhash`, glean the boundary rule, address chunks by σ.
- **0.10.2 → 0.10.3** (2026-07-23/24): chunk store + manifest + freshness witness; incremental CDC writes; explicit activation and GC.
- **2026-07-29** (`b82f56`): the `(size, mtime)` witness heuristic replaced by trigger-maintained generations — mutation identity, not change heuristic.
- **2026-07-29** (`f8ebe7`, 0.13.0): carried-over manifest rows verified by existence probe, making refresh cost honest.
- **2026-07-29** (`baa57f`): the `source_blobs` target, after measurement showed the `nodes` walk pays the floor penalty on 100% of a real projection's rows.
- ADR-0033 was a reserved gap between 0032 and 0034 until this record was written (bead `b6653a`); `chunked.rs` and the consumer test referenced D1 ahead of it.
