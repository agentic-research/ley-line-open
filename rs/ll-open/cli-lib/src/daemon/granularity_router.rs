//! Sheaf-driven granularity dispatcher (bead `ley-line-open-5b58ff`).
//!
//! For a given query touching a set of sheaf regions, recommend which
//! storage layer serves the read:
//!   - [`Granularity::PerNode`]: fetch specific AST subtree hashes from
//!     `node_content`.
//!   - [`Granularity::PerFile`]: fetch whole-file AST blobs from
//!     `capnp_blobs` (ADR-0026 Phase 1).
//!
//! The recommendation is based on the touched-regions' δ⁰ distribution
//! (per-edge squared violation norm from
//! [`CellComplex::edge_violation_squared`](leyline_sheaf::CellComplex::edge_violation_squared)):
//!   - **Low δ⁰ across the touched neighbourhood** → PerNode (sharp
//!     scope; each fetch is small).
//!   - **High δ⁰ concentrated in few cells** → PerFile (correlated
//!     change; fetch the whole file once, skip the node walk).
//!   - **High δ⁰ distributed across many cells** → PerFile (broad
//!     scope; file fetch wins the per-node walk on wall time).
//!   - **Empty / unknown sheaf state** → PerNode (safe default —
//!     the smallest fetch that always works).
//!
//! Consumers CAN ignore the recommendation — this is advisory during
//! the measurement window. Once ADR-0026 Phase 2 dual-read logs the
//! ACTUAL granularity + wall-time alongside the RECOMMENDED, the
//! measurement study (bead `ley-line-open-5b58ff` measurement-half)
//! correlates: does the sheaf's recommendation land on the faster
//! path?
//!
//! ## Thresholds
//!
//! [`LOW_DELTA0_THRESHOLD`] and [`MANY_CELLS_THRESHOLD`] are `const`
//! defaults chosen to be reasonable-not-tuned. Once the measurement
//! study lands they get retuned against real workload. Log lines
//! carry both the observed `max_delta0` and the touched-cell count so
//! a post-hoc replay can pick better cut-offs without re-running the
//! query workload.
//!
//! ## Counters
//!
//! Every call to [`route_query`] increments one of
//! [`ROUTED_PER_NODE`] / [`ROUTED_PER_FILE`]. A future `op_metrics`
//! op reads these totals so operators can watch the router's
//! decision distribution shift as thresholds are retuned.
//!
//! ## Consumer wiring — NOT here
//!
//! The router SHIPS as a callable module + logging + counters. Wiring
//! it into an actual read path waits for ADR-0026 Phase 2 dual-read
//! (bead `ley-line-open-8201de` execution plan) — until then any
//! consumer forced to make a granularity choice can't measure whether
//! the recommendation landed on the faster path, so wiring now would
//! either add complexity Phase 2 refactors away or push consumers to
//! guess.

use std::sync::atomic::{AtomicU64, Ordering};

use super::sheaf_ops::SheafState;

// ---------------------------------------------------------------------------
// Thresholds (retunable — starting defaults, log-driven refinement path)
// ---------------------------------------------------------------------------

/// δ⁰ squared-norm cut-off below which the touched neighbourhood is
/// treated as "sharp" — routing goes PerNode.
///
/// Starting default; the measurement study retunes against real
/// workload. Emitted in the `LEYLINE_PROFILE=1` log so a post-hoc
/// replay can pick a better cut-off without re-running the queries.
pub const LOW_DELTA0_THRESHOLD: f64 = 0.1;

/// Touched-cell-count cut-off above which "many cells" fires. Distinct
/// only from `HighDelta0ConcentratedCell` at the reason level — both
/// buckets recommend PerFile. Preserving the distinction feeds the
/// measurement study's decomposition: does routing benefit come from
/// concentration signal or from breadth signal?
pub const MANY_CELLS_THRESHOLD: usize = 8;

// ---------------------------------------------------------------------------
// Counters — read by a future op_metrics op (not wired yet)
// ---------------------------------------------------------------------------

/// Total number of routing decisions that returned [`Granularity::PerNode`].
///
/// Monotonic across the daemon lifetime; wraps at u64::MAX (astronomically
/// unreachable). Read via `load(Ordering::Relaxed)` — no ordering guarantee
/// is required, only that the counter observed by op_metrics matches what
/// the router incremented (single writer per call).
pub static ROUTED_PER_NODE: AtomicU64 = AtomicU64::new(0);

/// Total number of routing decisions that returned [`Granularity::PerFile`].
/// Companion to [`ROUTED_PER_NODE`]; same semantics.
pub static ROUTED_PER_FILE: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Which storage layer should serve a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granularity {
    /// Per-node fetch — specific AST subtree hashes from `node_content`.
    PerNode,
    /// Per-file fetch — whole-file AST blob from `capnp_blobs`.
    PerFile,
}

/// Structural label explaining why the router chose a given
/// [`Granularity`]. Emitted in profile logs and consumed by the
/// measurement study.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecommendationReason {
    /// Caller passed no touched cells → safe-default PerNode.
    NoCellsTouched,
    /// Cache has no attached [`CellComplex`] (heuristic-only mode) → no
    /// δ⁰ to measure → safe-default PerNode.
    NoSheafBaseline,
    /// `max_delta0 < LOW_DELTA0_THRESHOLD` → sharp scope, PerNode wins
    /// the walk.
    LowDelta0FewCells,
    /// `max_delta0 ≥ LOW_DELTA0_THRESHOLD` AND
    /// `touched_cell_count < MANY_CELLS_THRESHOLD` → concentrated
    /// disagreement, one-file fetch amortises across the neighbourhood.
    HighDelta0ConcentratedCell,
    /// `max_delta0 ≥ LOW_DELTA0_THRESHOLD` AND
    /// `touched_cell_count ≥ MANY_CELLS_THRESHOLD` → broad scope, per-
    /// file fetch beats the per-node walk on wall time.
    HighDelta0DistributedCells,
}

/// Router output. `max_delta0 = 0.0` when no cells touched, no
/// baseline, or every touched cell is missing from the complex — the
/// three cases where the router has no measurement to report.
#[derive(Debug, Clone, Copy)]
pub struct GranularityRecommendation {
    pub granularity: Granularity,
    pub reason: RecommendationReason,
    pub touched_cell_count: usize,
    pub max_delta0: f64,
}

// ---------------------------------------------------------------------------
// Route
// ---------------------------------------------------------------------------

/// Route a query touching `touched_cells` to a storage layer.
///
/// Pure function on the sheaf state (no writes; the atomic counters are
/// the only observable side-effect besides the profile log). Locks
/// [`SheafState::cache`] briefly to snapshot the per-edge δ⁰ readings
/// then drops the guard before returning — safe to call from consumer
/// sites without introducing a new lock ordering.
pub fn route_query(sheaf: &SheafState, touched_cells: &[u32]) -> GranularityRecommendation {
    let rec = compute_recommendation(sheaf, touched_cells);

    // Counter increment happens before the log emit so op_metrics reads
    // stay accurate even if profile output is being consumed via a
    // pipeline that eats stderr.
    match rec.granularity {
        Granularity::PerNode => ROUTED_PER_NODE.fetch_add(1, Ordering::Relaxed),
        Granularity::PerFile => ROUTED_PER_FILE.fetch_add(1, Ordering::Relaxed),
    };

    if profile_enabled() {
        eprintln!(
            "[profile] granularity_router: query touched_cells={} max_delta0={:.6} -> {:?} (reason={:?})",
            rec.touched_cell_count, rec.max_delta0, rec.granularity, rec.reason,
        );
    }

    rec
}

/// Pure core of the router — no counters, no logging. Split out so
/// tests can assert on the decision without side-effects on the
/// process-wide counter statics.
fn compute_recommendation(sheaf: &SheafState, touched_cells: &[u32]) -> GranularityRecommendation {
    let touched_cell_count = touched_cells.len();

    if touched_cells.is_empty() {
        return GranularityRecommendation {
            granularity: Granularity::PerNode,
            reason: RecommendationReason::NoCellsTouched,
            touched_cell_count: 0,
            max_delta0: 0.0,
        };
    }

    // Snapshot the max δ⁰ under the cache lock, then drop the guard.
    // Holding the guard across the decision branch would extend the
    // critical section into `eprintln!` / atomic bumps, which is
    // needless — the numbers we care about are already local by the
    // time we release.
    let max_delta0 = {
        let cache = sheaf.cache().lock();
        let Some(complex) = cache.complex() else {
            // No complex attached → heuristic-only mode → no per-edge
            // δ⁰ to consult. Safe-default PerNode.
            return GranularityRecommendation {
                granularity: Granularity::PerNode,
                reason: RecommendationReason::NoSheafBaseline,
                touched_cell_count,
                max_delta0: 0.0,
            };
        };

        // Walk `incidence` once, retain edges incident to any touched
        // cell, take the max squared-δ⁰. `edge_violation_squared`
        // returns None when the edge or its restriction maps aren't
        // installed (e.g. touched cell isn't a complex node), which
        // silently contributes nothing to the max — matches the "no
        // measurement possible" semantic without inflating the count.
        //
        // Small `touched` sets are the common case (a query touches a
        // handful of regions), so a linear-in-edges scan with a
        // `contains` check per edge is cheaper than building a hash
        // set until touched-cell counts hit the tens. If the touched
        // set gets large in practice we can swap in a HashSet without
        // changing the observable behaviour.
        let touched_slice: &[u32] = touched_cells;
        let mut max_sq: f32 = 0.0;
        for &(source, target) in complex.incidence.values() {
            let incident = touched_slice.contains(&source) || touched_slice.contains(&target);
            if !incident {
                continue;
            }
            if let Some(v) = complex.edge_violation_squared(source, target)
                && v > max_sq
            {
                max_sq = v;
            }
        }
        max_sq as f64
    };

    let (granularity, reason) = if max_delta0 < LOW_DELTA0_THRESHOLD {
        // Low δ⁰ dominates the decision — PerNode regardless of
        // count. Naming "FewCells" reflects the hot path in the
        // measurement study; the router pins the low-δ⁰ branch to
        // PerNode across the count axis so the study can decompose
        // "did low-δ⁰ save us?" cleanly from "did few-cells save us?".
        (
            Granularity::PerNode,
            RecommendationReason::LowDelta0FewCells,
        )
    } else if touched_cell_count >= MANY_CELLS_THRESHOLD {
        (
            Granularity::PerFile,
            RecommendationReason::HighDelta0DistributedCells,
        )
    } else {
        (
            Granularity::PerFile,
            RecommendationReason::HighDelta0ConcentratedCell,
        )
    };

    GranularityRecommendation {
        granularity,
        reason,
        touched_cell_count,
        max_delta0,
    }
}

fn profile_enabled() -> bool {
    std::env::var("LEYLINE_PROFILE").ok().as_deref() == Some("1")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use leyline_sheaf::CoChangeTracker;
    use leyline_sheaf::complex::{CellComplex, RestrictionMap};

    /// Snapshot the counter statics so tests running in parallel don't
    /// clobber each other's increment assertions. Delta is computed
    /// against a per-test baseline rather than absolute values.
    fn snapshot_counters() -> (u64, u64) {
        (
            ROUTED_PER_NODE.load(Ordering::Relaxed),
            ROUTED_PER_FILE.load(Ordering::Relaxed),
        )
    }

    /// Build a two-node complex with an edge whose agreement subspace
    /// is the first `agreement_dim` coords. `stalk_a` / `stalk_b` are
    /// pushed straight in so `edge_violation_squared` returns their
    /// projected disagreement.
    fn install_two_node_complex(
        state: &SheafState,
        stalk_a: Vec<f32>,
        stalk_b: Vec<f32>,
        agreement_dim: usize,
    ) {
        let dim = stalk_a.len();
        assert_eq!(dim, stalk_b.len(), "test setup: stalks must share dim");
        let mut cx = CellComplex::new(dim);
        cx.add_node(0, stalk_a);
        cx.add_node(1, stalk_b);
        let f = RestrictionMap::project_dim_range(dim, agreement_dim);
        cx.add_edge(
            100,
            0,
            1,
            agreement_dim,
            Some("test".into()),
            f.clone(),
            f,
            false,
        );
        state.install_complex(cx, CoChangeTracker::default());
    }

    /// Wire a many-cell complex: `n` nodes chained 0-1, 1-2, …
    /// Every edge disagrees by `edge_delta` in the first coord (part
    /// of the agreement subspace), so `edge_violation_squared` returns
    /// approximately `edge_delta²` for every edge.
    fn install_chain_complex(state: &SheafState, n: u32, edge_delta: f32) {
        let dim = 1;
        let mut cx = CellComplex::new(dim);
        for i in 0..n {
            cx.add_node(i, vec![(i as f32) * edge_delta]);
        }
        let f = RestrictionMap::project_dim_range(dim, 1);
        let mut edge_id: u32 = 100;
        for i in 0..n.saturating_sub(1) {
            cx.add_edge(
                edge_id,
                i,
                i + 1,
                1,
                Some("test".into()),
                f.clone(),
                f.clone(),
                false,
            );
            edge_id += 1;
        }
        state.install_complex(cx, CoChangeTracker::default());
    }

    #[test]
    fn empty_touched_cells_recommends_per_node_no_cells_reason() {
        let state = SheafState::new();
        // Even with a complex installed, the empty-input path bails
        // before touching it — the reason is `NoCellsTouched`, not
        // `NoSheafBaseline`.
        install_two_node_complex(&state, vec![1.0, 0.0], vec![1.0, 0.0], 1);

        let rec = compute_recommendation(&state, &[]);
        assert_eq!(rec.granularity, Granularity::PerNode);
        assert_eq!(rec.reason, RecommendationReason::NoCellsTouched);
        assert_eq!(rec.touched_cell_count, 0);
        assert_eq!(rec.max_delta0, 0.0);
    }

    #[test]
    fn cells_touched_but_no_baseline_recommends_per_node_no_baseline_reason() {
        // Fresh SheafState with no complex installed → heuristic-only
        // mode. Any touched-cell list bails to PerNode with the
        // NoSheafBaseline reason, since there is no δ⁰ to measure.
        let state = SheafState::new();

        let rec = compute_recommendation(&state, &[1, 2, 3]);
        assert_eq!(rec.granularity, Granularity::PerNode);
        assert_eq!(rec.reason, RecommendationReason::NoSheafBaseline);
        assert_eq!(rec.touched_cell_count, 3);
        assert_eq!(rec.max_delta0, 0.0);
    }

    #[test]
    fn low_delta0_few_cells_recommends_per_node() {
        // Two-node complex with matching agreement-subspace stalks —
        // δ⁰ = 0 on the only edge, well below the LOW_DELTA0_THRESHOLD
        // cut-off. Touched count = 1 (well below MANY_CELLS_THRESHOLD).
        let state = SheafState::new();
        install_two_node_complex(&state, vec![1.0, 0.0], vec![1.0, 9.0], 1);

        let rec = compute_recommendation(&state, &[0]);
        assert_eq!(rec.granularity, Granularity::PerNode);
        assert_eq!(rec.reason, RecommendationReason::LowDelta0FewCells);
        assert_eq!(rec.touched_cell_count, 1);
        assert!(
            rec.max_delta0 < LOW_DELTA0_THRESHOLD,
            "test fixture must sit below the threshold; got {}",
            rec.max_delta0,
        );
    }

    #[test]
    fn high_delta0_concentrated_recommends_per_file() {
        // Two-node complex with agreement-subspace disagreement large
        // enough to exceed LOW_DELTA0_THRESHOLD. Touched count = 1
        // (below MANY_CELLS_THRESHOLD), so the reason must be
        // `HighDelta0ConcentratedCell`, not `Distributed`.
        let state = SheafState::new();
        install_two_node_complex(&state, vec![0.0, 0.0], vec![10.0, 0.0], 1);

        let rec = compute_recommendation(&state, &[0]);
        assert_eq!(rec.granularity, Granularity::PerFile);
        assert_eq!(rec.reason, RecommendationReason::HighDelta0ConcentratedCell);
        assert_eq!(rec.touched_cell_count, 1);
        assert!(
            rec.max_delta0 >= LOW_DELTA0_THRESHOLD,
            "test fixture must exceed the threshold; got {}",
            rec.max_delta0,
        );
    }

    #[test]
    fn high_delta0_many_cells_recommends_per_file_distributed() {
        // Chain of N > MANY_CELLS_THRESHOLD nodes, every edge above
        // threshold. Touched-cell set spans the whole chain — reason
        // must be `HighDelta0DistributedCells`.
        let state = SheafState::new();
        let n: u32 = (MANY_CELLS_THRESHOLD as u32) + 4;
        install_chain_complex(&state, n, 5.0);

        let touched: Vec<u32> = (0..n).collect();
        let rec = compute_recommendation(&state, &touched);
        assert_eq!(rec.granularity, Granularity::PerFile);
        assert_eq!(rec.reason, RecommendationReason::HighDelta0DistributedCells);
        assert!(rec.touched_cell_count >= MANY_CELLS_THRESHOLD);
        assert!(
            rec.max_delta0 >= LOW_DELTA0_THRESHOLD,
            "test fixture must exceed the threshold; got {}",
            rec.max_delta0,
        );
    }

    #[test]
    fn route_query_bumps_the_matching_counter() {
        // Two calls: first hits the PerNode counter (empty touched),
        // second hits the PerFile counter (concentrated high δ⁰).
        // Baseline snapshot lets other tests running in parallel not
        // pollute this assertion.
        let state = SheafState::new();
        install_two_node_complex(&state, vec![0.0], vec![10.0], 1);

        let (base_pn, base_pf) = snapshot_counters();

        let _ = route_query(&state, &[]);
        let (mid_pn, mid_pf) = snapshot_counters();
        assert_eq!(
            mid_pn - base_pn,
            1,
            "empty touched must bump PerNode counter exactly once",
        );
        assert_eq!(mid_pf - base_pf, 0, "PerFile counter unchanged");

        let _ = route_query(&state, &[0]);
        let (end_pn, end_pf) = snapshot_counters();
        assert_eq!(
            end_pn - base_pn,
            1,
            "PerNode counter unchanged on second call",
        );
        assert_eq!(
            end_pf - base_pf,
            1,
            "high-δ⁰ query must bump PerFile counter exactly once",
        );
    }

    #[test]
    fn touched_cells_missing_from_complex_treated_as_low_delta0() {
        // Complex knows nothing about the touched IDs — no edges are
        // incident to the touched set, so max_delta0 stays 0.0 and the
        // router lands on LowDelta0FewCells + PerNode. The safe-default
        // behaviour: an unknown region shouldn't force a PerFile fetch.
        let state = SheafState::new();
        install_two_node_complex(&state, vec![10.0], vec![0.0], 1);

        let rec = compute_recommendation(&state, &[42, 43]);
        assert_eq!(rec.granularity, Granularity::PerNode);
        assert_eq!(rec.reason, RecommendationReason::LowDelta0FewCells);
        assert_eq!(rec.touched_cell_count, 2);
        assert_eq!(rec.max_delta0, 0.0);
    }
}
