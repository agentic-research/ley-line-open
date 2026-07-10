# Decade: `analysis-substrate`

**Scope:** LLO-side producer of `_cfg` / `_dfg` / `_taint` fact tables via an
incremental-view-maintenance engine (differential dataflow), driven by the
Σ substrate — content-addressed `node_hash` for input dedup, `_source.contentHash`
for coarse Bootstrap-vs-Update gating, and `daemon.sheaf.invalidate` as the
per-parse epoch/frontier tracker.

**Load-bearing thesis.** The three derived fact tables ADR-0024 (mache) asks
for are three projections of ONE differential-dataflow computation whose input
collections are the existing `_ast` / `node_content` / `node_defs` / `node_refs`
tables and whose progress tracker is `daemon.sheaf.invalidate`. If this is
true, the incremental engine is a thin `rs/ll-open/dataflow` crate. If it
is false, we need a real IDE-style progress tracker (much larger scope).

**Falsifiable claim about the substrate.** *For an intra-procedural workload
on the mache/internal corpus (207 Go files), an edit to one function body
produces a `daemon.sheaf.invalidate` payload whose `invalidated` region set
contains at most O(| callers-of-changed-function | + | callees-of-changed-function |)
regions, and the resulting differential-dataflow retraction+insertion touches
at most O(| changed function's DFG edges |) taint rows. If either is
O(| whole corpus |), the cascade is not doing meaningful work and the
"sheaf as progress tracker" claim is refuted.*

**Origin.** Producer-side of mache's
[ADR-0024: incremental-dataflow-taint-as-substrate-queries](../../../mache/docs/adr/0024-incremental-dataflow-taint-as-substrate-queries.md).
Scoped by theoretical-foundations-analyst (Fable) 2026-07-10 in response to
the "existing art in the shape of our CAS system" framing.

## 1. Existing art survey

| Prior art | One-line what it gives us | Verdict |
|---|---|---|
| **differential-dataflow** (McSherry, `differential-dataflow = "0.13"`) | Semi-naive fixpoint over collections with retraction (insertions AND deletions), `arrange` for shared indexes, `iterate` for monotone loops. Rust-idiomatic IVM engine, no CGO. | **Adopt.** New crate `rs/ll-open/dataflow`. |
| **timely-dataflow** (McSherry) | Partial-order timestamp lattice + progress tracker. Transitive dep of differential-dataflow. | **Use transitively, do not use directly.** Our time model is a scalar generation counter; sheaf cascade IS our frontier. |
| **Datafrog** (rust-lang polonius) | Tiny pure-Rust Datalog, semi-naive. Monotone only, no retractions. | **Reject.** Taint requires retraction when a sanitizer is added; edit-driven invalidation IS retraction. |
| **Soufflé / DDlog** | Datalog + lattices + incremental (DDlog compiles Datalog → Rust + differential-dataflow). Soufflé's *elastic* result: on high-impact changes, Bootstrap (recompute) beats incremental Update. | **Adopt the shape** (rule DSL + Bootstrap-vs-Update dispatch), not the crate. DDlog is archived; Soufflé is C++/CGO. Steal the pattern; write rules directly against differential-dataflow arrangements. |
| **SCCP** (Wegman-Zadeck sparse conditional constant propagation) | SSA lattice-propagation for constant folding + unreachable-branch pruning. Reduces DFG edges before taint sees them. | **Adopt the algorithm** in T2 only if taint precision demands it. Optional. |
| **Andersen / Steensgaard** (points-to) | Field-insensitive / field-sensitive alias analysis. P/Taint's minimalism is contingent on this pre-existing fixpoint. | **Defer** to a Tn thread outside this decade. |
| **CFL-reachability** (Reps) | Language-reachability formulation of interprocedural dataflow (context-free grammar over call/return balanced parens). Canonical framing of taint. | **Adopt the formulation** in T3. Intra-procedural first (regular reachability), interprocedural (CFL) as follow-up. |
| **salsa / rustc query system** | Memoized query, red-green invalidation. Rust-native. Not IVM — recompute-on-invalidation. | **Reject as engine, adopt the idea.** Our node_hash-keyed memo (`mache-238673`) already occupies this niche at subtree granularity. |
| **CodeQL DataFlow/TaintTracking API** | `isSource / isSink / isBarrier / isAdditionalFlowStep` — the canonical taint rule shape. | **Adopt the API shape** for T3's rule DSL. |

## 2. Substrate-fit analysis

What each existing LLO primitive gives us for the differential-dataflow build.

| Primitive | dd need | What we get for free | Gap |
|---|---|---|---|
| Σ / BLAKE3 CAS (ADR-0026, -0028) | Stable, dedupable content identity for rule inputs so `arrange` collapses byte-identical facts. | The whole content-identity layer. `node_hash` is a real subtree address; two byte-identical function bodies share one row in `node_content` and contribute one input tuple. dd's `arrange` win by construction. | None. |
| `node_hash` merkle-AST address (ADR-0027) | Cache key for memoized per-subtree analyses. | Every intra-procedural CFG/DFG for a function-body subtree is a pure function of `node_hash`. Rename an identifier → new node_hash → dd sees retraction+insertion. Exactly the delta stream dd wants. | Producer must actually key its emit on node_hash. `_ast_pointer` (ADR-0026) already does at AST level; extend to derived tables. |
| `_source.contentHash` (byte-level file address) | Coarse-grained "did file X change?" gate before per-node dd. | The file-level Bootstrap-vs-Update switch. Unchanged contentHash ⇒ no dd delta ⇒ skip. Amortizes over unchanged corpus. | None. |
| CellComplex + δ⁰ | Progress tracker (which regions still "in flight" for the current input epoch). | The frontier. `on_change(&changed_regions) → Vec<affected_region_ids>` gives dd "regions this parse epoch may still perturb." **Sheaf cascade == dd's per-region progress tracker restricted to a single-dimension epoch counter.** | dd wants a per-timestamp downgrade signal (`Antichain<T>`). We only emit `daemon.sheaf.invalidate` at end-of-parse. Fine for batch, insufficient for streaming — defer streaming to non-goals. |
| `daemon.sheaf.invalidate` event (v0.7.1 payload) | Wire format for dd input delta. | Directly usable. `{invalidated: [u32], generation, prior_generation}` maps 1:1 to `(collection_delta, timestamp_now, timestamp_prev)`. `changed_files` + `region_labels` map file→region. | Event fires at parse commit; dd wants deltas at operator granularity. **Solution: one coarse frontier tick per parse; dd internally fans out fine-grained.** |
| `_ast` + `_source` + LSP defs/refs | Ground-truth EDB rules join over. | The EDB, ready. | Def-use is declaration-granularity, not variable-level — T2 must build SSA before T3 taint runs. |
| HDC hypervectors (function-level `_hdc`) | Structural similarity for approximate rule matching. | Coarse-grained structural stalks. **In δ⁰ mode, HDC hypervector IS the region stalk in projected coordinates.** Useful for "does this function look like `strcpy`?" queries. | Function granularity only. HDC is Go+Rust today. |
| CoChangeTracker | Prior on which regions co-invalidate. | Learned edge weights, observation-only today. Could be repurposed as taint-source prior. | Not in cascade decision path today. Punt to research thread. |

### Missing primitives, called by name

- **Real timely progress tracker** (partial-order timestamp lattice, `Antichain<T>`). We have `generation: u64` + per-epoch region-invalidation set. Sufficient for analysis-as-batch-per-parse; insufficient for streaming. Deferred.
- **Per-instruction structural addressing** (HDC below function). Not needed for T1-T3.
- **Points-to fixpoint**. P/Taint's "add-on to points-to" minimalism does not apply. We build un-minimal taint (own reachability, no shared points-to relation).

## 3. Threads

- **T1 `cfg-emission`** — Can we build intra-procedural CFGs from `_ast` cheaply enough to run every parse, and is CFG semantics thin enough to κ-collapse (ADR-0027 §6) across grammars?
- **T2 `dfg-ssa`** — Can we build variable-level def-use (SSA-lite) over T1's CFG without adopting the rustc query system, and does κ survive SSA rename?
- **T3 `taint-fixpoint`** — Can taint be expressed as CFL-reachability over T2's `_dfg` in differential-dataflow with a rule shape matching CodeQL's four predicates, and does the Bootstrap fixpoint complete inside a single parse budget on mache/internal?
- **T4 `sheaf-cascade-integration`** — Does the sheaf cascade drive differential-dataflow deltas at O(affected-region) cost, or degrade to O(whole-corpus) recompute in the common case?

Beads sequence CFG → DFG → taint → incremental engine (per-thread, not per-operator) — each thread produces a distinct fact table (review-legible artifact), and Soufflé's Bootstrap→Update phasing requires all three tables to exist as batch computations before Update logic can be tested.

## 4. F-gates (falsifiable, per thread)

| F-gate | Thread | Claim | Test file (proposed) | Fails when |
|---|---|---|---|---|
| **F1_cfg_reflow_stable** | T1 | `_cfg` rows are content-addressed by `node_hash` and survive gofmt reflow byte-identically. | `rs/ll-open/cli-lib/tests/f1_cfg_reflow_stable.rs` | Reflow changes row hashes — CFG builder captures whitespace/positions instead of structure. |
| **F2_ssa_rename_deterministic** | T2 | SSA rename indices are byte-identical across two parses; cross-file dedup on identical function bodies collapses to one `_dfg` row set. | `rs/ll-open/cli-lib/tests/f2_ssa_rename_deterministic.rs` | Phi placement is HashMap-order-dependent, or rename indices leak parse-run node ids. |
| **F3_taint_bootstrap_terminates** | T3 | Cold `taint_bootstrap` on mache/internal + `HardcodedCredentialFlow` config terminates in ≤ 2× cold-parse wall time and finds ≥ 1 planted flow. | `rs/ll-open/dataflow/tests/f3_taint_bootstrap_terminates.rs` | Fixpoint doesn't terminate, or terminates but misses planted flow → rule DSL or CFL formulation broken. |
| **F4_edit_locality** | T4 | Editing one function body triggers a dd delta touching ≤ 2 × \|changed function's dfg edges\| `_taint` rows. | `rs/ll-open/cli-lib/tests/f4_edit_locality.rs` | Delta touches whole-corpus rows → sheaf cascade is NOT the progress tracker. |
| **F5_no_regression_on_HDC** | cross-cutting | HDC hypervectors continue to pass math gates (saturation, discriminability) after `_dfg`/`_taint` land in the same parse pass. | `rs/ll-open/cli-lib/tests/f5_no_regression_hdc.rs` | Analysis-substrate pass interferes with HDC — schema growth or index churn regresses HDC math. |

All-green ⇒ substrate-thesis not refuted. Any red ⇒ ADR-0024's producer-side claim falsified and we regroup.

## 5. Non-goals / deferrals

1. **Interprocedural taint via full CFL-reachability.** T3 ships regular reachability (intra-procedural). Balanced-parens call/return matching is a Tn thread.
2. **Points-to analysis** (Andersen / Steensgaard). Own bead epic when needed for field-level or heap taint.
3. **Streaming input** (a real `Antichain<T>` progress tracker). Commit to batch-per-parse. Future streaming daemon needs a proper frontier.
4. **Python/JS CFG coverage** unless `ley-line-open-e76959` (producer/consumer contract test discipline) closes and Python/JS extraction fidelity is verified. Go+Rust only in v1. (Note: Fable's original gate on `caf423` corrected — caf423 is closed/pruned; the actual class-of-gap follow-up is `e76959`.)
5. **HDC below function granularity.** T2.b5 optional, killable. Per-instruction structural stalks not on roadmap.
6. **Mangle / Datalog compiler in mache.** ADR-0024's fallback — hold in reserve.
7. **CoChangeTracker → cascade-weight coupling.** Learned weights stay observation-only. Wiring is research thread.
8. **`_ast_pointer` → `_dfg` / `_taint` pointer store analog.** ADR-0026 Phase 2 on its own timeline. This decade emits derived tables in row-projected style; later ADR migrates.

## 6. Where mache picks up

- `_cfg` / `_dfg` / `_taint` land as SQLite tables in the daemon's `.db` projection.
- mache reads via `WITH RECURSIVE` (positive recursion only if T3.b5 succeeds) or via new MCP tools `find_dataflow` / `find_taint` implemented in Go against the same tables.
- mache-463612's arms: (A) test-coverage, (B) fast-lint, (C) dataflow/taint. **This decade IS arm C producer-side.** Arms A/B are pure Go work against `_dfg`/`_taint` once they exist.

## 7. Reconciliation with HDC

HDC stalks are function-level structural signatures. `_dfg` is intra-function variable-level dataflow. **Orthogonal projections of the same subtree, not competitors.** Join key: `(source_id, name, span)` today, `node_hash` once ADR-0027 lands fully.

If T2.b5 ships (HDC↔DFG bridge), HDC gains a per-DFG-connected-component projection — a strictly finer stalk, not a replacement.

## 8. First dispatch

- **T1.b1** — κ CFG-kind extension (`rs/ll-open/ts/src/languages.rs`, `canonical.rs`). *Why first: leaf, no runtime deps, no schema change. If κ can't cleanly canonicalize control-flow kinds across Go+Rust, we learn that before investing in schema and builder.*
- **T1.b2** — `_cfg` schema DDL (`rs/ll-open/ts/src/schema.rs`). *Why second: parallelizable with T1.b1 (independent file). Landing schema first makes FK reference target explicit; catches ADR-0027 `PRAGMA foreign_keys=ON` impedance mismatch before CFG builder ships.*

Beads 3+ sequence naturally once these land.
