# ADR-0026 — SQL projection as a content-addressed pointer store

**Status:** Proposed (2026-07-07)
**Bead:** `ley-line-open-5ff6aa`
**Related:**
- ADR-0014 (capnp as protocol; T8.3 producer contract)
- ADR-0015 (lazy on-access ingestion)
- ADR-0016 (AI-native query surface)
- `ley-line/docs/design/014-cdc-fountain-composition.md` — CDC + fountain-code composition (transport substrate)
- `ley-line/docs/design/015-restriction-hierarchy-machinery.md` — sub-sheaf hierarchy (transport source-symbol layout)
- `ley-line-open-569beb` — daemon admission-control bug (contaminated the initial profile with I/O contention)

---

## 0. One-line claim

**LLO's SQL projection today re-materializes every AST node as a normalized row and as a per-node canonical capnp record. This violates LLO's own Σ / BLAKE3-Merkle-CAS philosophy, which everywhere else addresses content by hash and never re-materializes it. The SQL projection should become a lightweight index (`_ast_pointer`) into content-addressed capnp blobs held in the Σ CAS. Every layer talks to Σ; no layer rebuilds Σ.**

---

## 1. Context

### 1.1 What the profile showed

Instrumented v0.5.9 cold parse of `mache/internal` (207 Go files) — sub-phase timing gated on `LEYLINE_PROFILE=1`, immune to background I/O contention:

```
insert=6905ms
  buffer+capnp_write=1236ms  (18%)  main-thread file loop + 76MB canonical capnp writes
  sql_flush=1544ms            (22%)  multi-row VALUES INSERTs (~500k rows)
  capnp_flush=8ms             (<1%)  BufWriter tail flush
  commit=2911ms               (42%)  SQLite WAL fsync at COMMIT
  index_build=1204ms          (17%)  post-load secondary index build
```

Warm-cache commit drops to 1882ms (34% of insert). Every other phase is stable.

### 1.2 The load-bearing observation

All four dominant phases converge on the same root cause:

- **COMMIT (42%)** fsyncs 76 MB capnp + 296 MB SQLite because every AST node = row + capnp record
- **sql_flush (22%)** — 500k rows × ~3 µs each. INSERT mechanic is fine. **Row count is downstream of the row-projection strategy.**
- **buffer+capnp_write (18%)** — 500k canonical serializations. Per-file blobs = ~200 canonical writes for the same byte-identity property. **Granularity is downstream of the per-node capnp choice.**
- **index_build (17%)** — 500k rows × ~5 indexes. `create_post_load_indexes_skip_unused` literally acknowledges some indexes are unused. **Same root: schema designed as "faithful projection" not as "index for a query workload."**

SQLite handles hundreds of millions of rows fine. 500k rows is not the problem. **The projection strategy is.**

### 1.3 Why parse is a production hot path, not bootstrap

`parse_into_conn` is called from:
- `cli-lib/src/cmd_parse.rs::cmd_parse` (CLI)
- `cli-lib/src/daemon/enrichment.rs:340` (daemon enrichment pass)
- `cli-lib/src/daemon/ops.rs:546` (daemon IPC op mache invokes on-demand)

Cold-parse cost is felt every time mache asks the daemon to scan an un-indexed directory. Not just first-run.

### 1.4 Why this ADR now

- The transport substrate work in `ley-line/docs/design/014-cdc-fountain-composition.md` (CDC + fountain-code composition, sheaf-block granularity) assumes that content in Σ is content-addressed all the way down. LLO's SQL projection is currently the sole layer that violates that assumption.
- ADR-0016's "AI-native query surface" articulated what consumers need from LLO. This ADR is the substrate change that makes those queries cheap.
- The measurement above quantified the cost of the row-projection strategy for the first time; before this we were guessing.

---

## 2. Decision — content-addressed pointer store

### 2.1 Schema shape

```
capnp_blobs:
  blob_hash    BLAKE3     PRIMARY KEY
  blob_bytes   BLOB       NOT NULL   -- canonical capnp of one blob unit

_ast_pointer:
  node_id       TEXT      PRIMARY KEY
  blob_hash     BLAKE3    REFERENCES capnp_blobs(blob_hash)
  offset_in_blob INTEGER
  kind          INTEGER   -- semantic kind for query filter (function, method, type, import, ...)
  source_id     TEXT      -- file the node belongs to
```

**Row width**: `_ast_pointer` is ~40 bytes vs the current `_ast` row at ~200 bytes. **Storage drop is 5×** at the row level even before the write-amp savings from CAS dedup.

**Query resolution**: consumer that wants a node's AST bytes: pointer row → `blob_hash` → `capnp_blobs` blob → decode fragment at `offset_in_blob`. One join, one blob lookup, one capnp decode.

### 2.2 Blob unit — per-file or per-semantic-unit

The blob unit is the load-bearing measurement input. Two candidates:

- **Per-file blob**: one `capnp_blobs` row per source file. `blob_hash = BLAKE3(canonical(List(AstNode)))` for the file. Simple. Coarse. Any edit to any node in a file rewrites the whole file blob.
- **Per-semantic-unit blob**: one blob per function / method / type / import (or whichever grouping matches consumer queries). Fine. Small-edit locality. Requires the "which AST subtrees are the semantic units" question answered from real query patterns.

**Deferred to measurement.** The design doc closing this ADR must answer with data:

- What AST node kinds do consumers actually query? Hypothesis: 90%+ of queries hit functions / methods / types / imports (semantic surface), not every identifier or expression node.
- If the hypothesis holds, per-semantic-unit blobs give ~10× storage savings AND small-edit locality.
- If it fails (consumers really do query every node), per-file blobs are the fallback — still ~2500× fewer canonical writes than status-quo.

Both flavors preserve the byte-identity property that ADR-0014 T8.3 requires.

### 2.3 What NOT to do — pointer into raw source file

Rejected alternative: `_ast_pointer.(source_path, byte_offset, byte_len)` pointing into the raw source file on disk.

Why rejected:
- The filesystem already exposes raw source bytes via `ls` / `cat`. Adding an SQL byte-offset pointer just re-exposes what's accessible.
- Raw source bytes live **outside** Σ. That breaks the composition chain: no CAS dedup (each source-file byte is uniquely addressed by filesystem path), no CDC chunking (source files aren't in the chunk arena), no RaptorQ transport (raw files aren't blob-hashable).
- Cross-generation dedup fails: same file content across two parse generations still has two independent filesystem inodes.

The content-addressed pointer model preserves all of these because blobs ARE in Σ.

---

## 3. Composition with the transport substrate (ADR-014 / ADR-015)

The pointer model isn't a local cold-parse win. It's the missing composition piece that makes ADR-014's substrate story land end-to-end:

```
Parse produces:     capnp blobs (per-file or per-semantic-unit)
Blobs land in:      Σ / BLAKE3-Merkle-CAS               (existing LLO substrate)
Blobs chunk via:    CDC (Gearhash, per ley-line ADR-012 / ADR-014)
Chunks encode via:  RaptorQ sheaf-block (per ADR-014)
SQL projection:     lightweight pointer rows into the CAS
Sub-file writes:    only rewrite changed CDC chunks → new blob_hash → new pointer row
Cross-file dedup:   identical ASTs share blob_hash automatically
Cross-gen dedup:    unchanged files share blob_hash automatically
Transport:          ADR-014 substrate ships the blobs; no re-serialization, no separate wire format
```

Each layer takes a hash-addressed input from the layer below and hands a hash-addressed output to the layer above. **The SQL projection is the only current layer that violates this. Fixing it aligns the whole stack.**

Explicit knock-on effects:

- **Federated transport (ADR-014)** ships blobs by hash. A receiver with the same blob_hash cached does zero network I/O. Today, receiving an updated `.db` file requires downloading the whole file even if 99% of its rows are unchanged.
- **Restriction hierarchy (ADR-015)** groups related blobs into sheaf-blocks for source-symbol layout. Per-semantic-unit blobs give the hierarchy machinery something finer than files to cluster on — the sub-sheaf lattice becomes semantically meaningful.
- **Signed manifests (ADR-004 in ley-line)** already publish per-chunk hashes. Blob hashes plug into the same manifest schema — no new signature machinery required.

---

## 4. Regenerable-data WAL model

**Why unusual WAL pragma choices are correct here:** LLO's SQL projection is regenerable from source files. The durability layer is git + the filesystem, not SQLite. Losing the last WAL frames costs at most a re-parse of the affected files.

This flips the risk model from typical OLTP:

| Typical OLTP | LLO SQL projection |
|---|---|
| Data is unique — customer orders, financial ledgers | Data is regenerable — indexed AST projection |
| Durability is the whole point | Durability provided by source files upstream |
| `synchronous=FULL` is correct | `synchronous=NORMAL` is correct |
| WAL fsync per commit | WAL fsync at checkpoint |
| Optimizing WAL is dangerous | Optimizing WAL is table stakes |

**Reasonable pragmas for this workload:**

- `PRAGMA synchronous=NORMAL` (default is FULL). Fsync at checkpoint only. Crash-safe for the DB structure. "Last WAL frames lost on power-fail" costs a re-parse.
- `PRAGMA wal_autocheckpoint=0` + explicit `PRAGMA wal_checkpoint(TRUNCATE)` at end of parse. Batches checkpoint out of the hot commit path.
- `PRAGMA journal_mode=MEMORY`. WAL in memory. Crash cost = re-parse.
- `:memory:` DB + `sqlite3_serialize` at end. Skip WAL entirely. LLO already uses `sqlite3_deserialize` for cache loading — the machinery exists.

**These are non-standard for OLTP but correct for regenerable-indexed-content workloads.** Stated explicitly in this ADR so future contributors don't optimize back toward OLTP defaults on the false assumption that WAL discipline is universally correct.

**Ordering with the pointer model:** the WAL wins are tactical, on top of the pointer-model win. Both compose but the pointer model is the strategic reframe. Don't bike-shed pragmas ahead of the structural decision.

---

## 5. Sub-file granularity — falls out for free

Today: file X changes → `delete_file_rows(X)` → reinsert every row. File-atomic. Huge WAL churn for a one-character edit.

Pointer model: file X changes → tree-sit X → for each AST node whose content hash changed, rewrite its pointer row + its blob. Unchanged nodes: untouched.

CDC boundaries (per ADR-014 / ley-line ADR-012) define natural update units. Small edits produce small WAL deltas because:
- Most nodes' content hashes don't change
- Their blob_hashes therefore don't change
- Their pointer rows therefore aren't rewritten
- The WAL sees only the delta

**Sub-file granularity isn't a separate feature to implement. It's what the CAS layer gives you when you point INTO it instead of copying FROM it.**

---

## 6. Falsifiability protocol

The design bet is: consumers benefit from a content-addressed pointer store more than they pay for the composition complexity. That's testable.

### F1: Round-trip integrity

**Claim:** every AST node reachable via the current row-projected schema is reachable via the pointer store, with identical field values.

**Test:** dual-write during the migration transition. For every query the daemon serves, run it against both schemas and compare results. Assert 100% agreement on ≥100k queries across the mache benchmark corpus.

**Pass:** 100% agreement over the full sample.

### F2: Cold-parse wall-time win

**Claim:** pointer store reduces cold-parse insert time by ≥40% on the mache/internal 207-file corpus.

**Test:** compare instrumented `LEYLINE_PROFILE=1` timings between current row-projected v0.5.9 and the pointer-store implementation on the same corpus, same hardware, same background load (or lack thereof).

**Pass:** insert phase ≤ 4100ms (vs current 6905ms cold).

### F3: Sub-file edit locality

**Claim:** editing a single function's body triggers a WAL delta proportional to the changed function's blob size, not to the file's total row count.

**Test:** parse a corpus. Modify one function's body. Re-parse (incremental). Measure WAL bytes written.

**Pass:** WAL bytes written ≤ 2 × changed_blob_size + fixed overhead. Not proportional to file's total AST rows.

### F4: Cross-generation dedup

**Claim:** re-parsing an unchanged corpus produces zero new blobs in `capnp_blobs` and zero new rows in `_ast_pointer`.

**Test:** parse corpus, snapshot DB. Re-parse without any source changes. Snapshot again. Assert `capnp_blobs` row count and `_ast_pointer` row count are unchanged.

**Pass:** exact-equal row counts. No new blobs. No new pointer rows.

### F5: Cross-file dedup on identical subtrees

**Claim:** two files with an identical semantic unit (e.g., copy-pasted helper function) share one blob in `capnp_blobs`.

**Test:** synthetic corpus with 100 files each containing a byte-identical `helper()` function. Parse. Assert `capnp_blobs` contains one blob for `helper()`, not 100.

**Pass:** one shared blob.

### F6: Composition with ADR-014 transport

**Claim:** ADR-014's sheaf-block transport substrate can ship pointer-store blobs without any additional serialization or wire format.

**Test:** encode a pointer-store DB's `capnp_blobs` via ADR-014's per-sheaf-block RaptorQ composition, transmit, decode. Verify received blob hashes match sent blob hashes.

**Pass:** 100% byte-identity on decode.

---

## 7. Kill criteria

Reasons to reject this ADR after implementation attempt:

1. **F1 fails**: pointer-store cannot serve the same queries as the row-projected schema. Design broken; refactor or abandon.
2. **F2 fails significantly** (insert time > 6000ms): the pointer store adds so much resolution overhead that the write-side savings don't matter. Reject; return to row projection with tactical WAL tuning only.
3. **F3 fails**: small edits still cause disproportionate WAL churn. Sub-file granularity claim broken; something is wrong with blob-boundary alignment or CAS lookup.
4. **F6 fails**: transport substrate cannot cleanly ship blobs. Design doesn't compose; the substrate composition claim is broken.

---

## 8. Non-goals

- Do NOT prescribe implementation details in this ADR. The design bet is the load-bearing decision. Implementation follow-ups (blob unit choice, migration strategy, exact schema DDL) are separate beads spawned by the design doc.
- Do NOT re-run micro-optimizations from `ley-line-open-9ccbc7` (deferred indexes, batched INSERT). Those helped but are local wins on top of a mis-aligned strategy.
- Do NOT touch WAL pragmas ahead of the pointer-model decision. Tactical, not strategic.
- Do NOT redesign consumer query surfaces here. ADR-0016 owns that; this ADR provides the substrate layer that ADR-0016 queries against.

---

## 9. Migration path — dual-write then cutover

**Phase 1**: implement `capnp_blobs` + `_ast_pointer` alongside existing tables. Dual-write during parse: existing row-projected tables AND new pointer tables. F1 test runs continuously; assert 100% agreement.

**Phase 2**: switch daemon read paths to the pointer store one query at a time. Row-projected tables still populated but read paths migrate incrementally. Detailed execution plan in §9.2.

**Phase 3**: stop populating row-projected tables. `_ast`, `nodes`, `node_refs`, `node_defs` become deprecated. `_source`, `_file_index` may survive if the pointer store doesn't subsume them.

**Phase 4**: drop deprecated tables. Migration complete.

The dual-write phase is where F1 has teeth. If ANY query returns different results between the two schemas during Phase 1, the pointer-store implementation has a bug that must be fixed before Phase 2 begins.

### 9.1 Phase 1 status (shipped)

Landed on main 2026-07-08 (PR #145). `capnp_blobs` + `_ast_pointer` populated on every parse alongside the row-projected `_ast` schema. F1 round-trip integrity test asserts byte-identical field values across 200+ rows on a 5-file Go fixture. Per-file blob unit chosen as Phase 1's safe default; per-semantic-unit refinement deferred to Phase 2 or later based on measured mache query shapes.

### 9.2 Phase 2 execution — dual-read → measurement → flip

Phase 2 is NOT a single-shot rewrite. It's a staged migration with a falsifiable gate per stage. Each op-level migration follows the same discipline; the plan below applies to each independently.

**9.2.1 First op to migrate: `op_query`.**

`op_query` is the load-bearing consumer read path — mache and cloister route their AST reads through it. Migrating it first delivers the entire consumer story in one op. Alternatives (`op_reparse`, `op_enrich`) are producer-side; migrating them doesn't test the design bet against real query workload.

**9.2.2 Dual-read discipline (mirrors dual-write).**

Same shape as Phase 1's dual-write. For each op being migrated:

1. Add a pointer-store read path alongside the existing `_ast` read
2. Behind a config flag (`LEYLINE_POINTER_STORE_READ=off|shadow|primary`), route reads:
   - `off`: existing row-projected path only (default; safe rollback)
   - `shadow`: execute BOTH paths, compare results, log mismatches, return the row-projected result. Zero user-visible behavior change.
   - `primary`: execute pointer-store path first, fall back to row-projected on error, log fallbacks. This is the flip-to-new-default state.
3. Shadow mode is where F1 gets teeth on real workload — not just the Phase 1 F1 fixture. Deploy shadow, watch the mismatch log, fix bugs, iterate until mismatch rate is zero over N days.
4. Only after zero-mismatch soak does `primary` ship.

**9.2.3 F2 measurement infrastructure (does NOT exist yet).**

F2 currently measures cold-parse insert wall-time. Phase 2 needs the analog for READ wall-time:

- Per-op wall-time histogram (p50/p99) via existing `LEYLINE_PROFILE=1` sub-phase timing pattern
- Split by read path (`_ast` row-projected vs `capnp_blobs`/`_ast_pointer` pointer store)
- Persisted to a bench artifact so PRs can compare against a baseline

**Build this measurement infrastructure BEFORE any op migration.** Without it, the F2 gate is unmeasurable and the migration ships blind.

**9.2.4 F2 gate — 2× read wall-time win.**

For each op being migrated, before flipping from `shadow` to `primary`:

- Bench N=10k queries against a real corpus (mache/internal or larger)
- Pointer-store path p99 read time must be ≤ 50% of row-projected p99
- If it isn't, do NOT flip. Either optimize the pointer-store path, revisit the schema, or accept that this op doesn't benefit from the migration.

The 2× bar is deliberately conservative. WAL 15b landed with 306µs p99 (matching empirical 290-375µs target) — that's the substrate-layer wall clock. Pointer-store reads should measurably beat row-projected on the same substrate, or the design bet is falsified.

**9.2.5 Falsification path — what a failed Phase 2 tells us.**

If `op_query` migration hits F2 with <2× improvement (or, worse, a regression):

- The pointer-store schema isn't giving reads what they need. Two candidates:
  - **Per-file blob unit is wrong for this query shape** — most queries touch a handful of nodes; fetching the whole file's blob is over-fetching. Move to per-semantic-unit blobs (ADR §2.2, open question 1).
  - **The design bet is falsified** — content-addressed pointer store adds SQL indirection without offsetting the resolution overhead. Row-projected wins. Kill Phase 2 per §7 kill criteria.

Either outcome is real information. F2 is falsifiable; the point is to measure, not to force a migration through.

**9.2.6 Rollback plan.**

The dual-write invariant (Phase 1) keeps row-projected populated throughout Phase 2. At any point during Phase 2, setting `LEYLINE_POINTER_STORE_READ=off` returns to the pre-Phase-2 behavior with zero data loss. Phase 3 (stop populating row-projected) is the point of no return.

**9.2.7 Staged op-migration order (proposed).**

After `op_query` proves the discipline:

1. `op_query` (load-bearing consumer read; proves the design bet)
2. `op_agreement` (mache uses for graph reads)
3. `op_reparse` internal reads (incremental-parse tooling)
4. LSP enrichment reads (`daemon/enrichment.rs`; ADR §10 open question 5)

Each stage: shadow → F2 gate → primary → soak. No batching. Each stage produces a merged PR + closed sub-bead + measured F2 result.

### 9.3 Phase 2 sub-beads (to file)

- **Phase 2.0**: F2 read-side measurement infrastructure (blocking all subsequent stages)
- **Phase 2.1**: `op_query` dual-read path + shadow-mode wire-up
- **Phase 2.2**: `op_query` F2 bench + `primary` flip if 2× gate passes
- **Phase 2.3**: `op_agreement` migration (mirrors 2.1 + 2.2)
- **Phase 2.4**: `op_reparse` migration (mirrors 2.1 + 2.2)
- **Phase 2.5**: LSP enrichment migration + resolve open question 5

Sub-bead `ley-line-open-5b58ff` (sheaf-driven granularity dispatcher) becomes actionable after Phase 2.1 ships — that's when `capnp_blobs` has a consumer to dispatch to.

---

## 10. Open questions (deliverable of the design doc closing bead `5ff6aa`)

1. **Blob unit** — per-file or per-semantic-unit? Answered by measured mache query patterns.
2. **Blob hash algorithm** — BLAKE3 is the LLO substrate default. Confirm no reason to deviate.
3. **Semantic-unit boundaries** — if per-semantic-unit blobs win, what tree-sitter kinds are the units? Language-specific? Or a universal kind allowlist (function, method, type, class, interface, module)?
4. **Migration ordering** — dual-write is straightforward. Are there queries that inherently need both schemas simultaneously (e.g., cross-schema joins during transition)? If yes, Phase 2 is more complex than sketched.
5. **Interaction with LSP enrichment** — LSP enrichment (`daemon/enrichment.rs`) currently writes `binding_record` and hover cache alongside AST nodes. Do those become pointer-store blobs too? Or stay in their existing schema?
6. **Existing consumer contracts** — mache queries the current schema. When does mache migrate its query builder? Coordinated release with LLO, or LLO ships dual-schema for a version and mache picks a new version to migrate on?

---

## 11. Provenance

- **2026-07-07**: instrumented profile of v0.5.9 cold parse on mache/internal (207 files) established that insert=6905ms is dominated by four phases all downstream of the row-projection strategy. Initial samply profile showed "50% in File::open_c inside sibling_snapshot_writers" and "75% in aho-corasick" — both were symbol misresolution + background I/O contention artifacts from `ley-line-open-569beb`. Instrumented `LEYLINE_PROFILE=1` timings are the trustworthy source.
- **2026-07-07**: user's four-point reframe landed the design bet. Pointer-into-CAS-blob (flavor B) chosen over pointer-into-source-file (flavor A) because the latter breaks the Σ / CDC / RaptorQ composition chain. The unifying insight: "content addressing should go all the way down; the SQL projection is currently the only layer that violates this."
- Related work in ley-line: ADR-014 (CDC + fountain-code composition), ADR-015 (restriction hierarchy machinery) — the transport substrate this ADR aligns LLO's SQL projection with.

---

## 12. Depends-on / Relates-to matrix

| Doc | Role |
|---|---|
| ADR-0014 (capnp as protocol) | Provides the byte-identity guarantee this ADR preserves at a different granularity |
| ADR-0015 (lazy on-access ingestion) | Related philosophy: don't materialize until needed |
| ADR-0016 (AI-native query surface) | Query patterns this ADR serves; measurement input for blob-unit choice |
| ley-line ADR-014 (CDC + fountain-code) | Transport substrate; blob_hash is the identity this transport ships |
| ley-line ADR-015 (restriction hierarchy) | Groups blobs into sheaf-blocks for transport source-symbol layout |
| `ley-line-open-5ff6aa` | This bead; closes when the design doc lands |
| `ley-line-open-569beb` | Daemon admission-control bug that contaminated the initial profile |
| `ley-line-open-9ccbc7` | Prior perf bead for deferred indexes; local wins that don't address this reframe |
