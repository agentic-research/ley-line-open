//! ADR-0020 Gate 2 falsifiability test for `ComplexBuildPass`.
//!
//! Closes bead `ley-line-open-c7eae2`. The gate exists specifically to
//! prove the math layer is load-bearing — that the pass mechanically
//! reaches `leyline_sheaf::CellComplex::new` + `add_node` + `add_edge`
//! and `CoChangeTracker::observe`, and that δ⁰ actually executes on
//! the produced complex.
//!
//! Math-friend §3 (spy pattern): "Production impl forwards directly to
//! `CellComplex`. Test impl is a `RecordingSink` that records every
//! call AND constructs the real `CellComplex` underneath." Plus the
//! simplest sufficient post-condition: `cx.build_delta_0().nnz() > 0`,
//! which is impossible to satisfy without `leyline-sheaf` machinery
//! actually running.
//!
//! ## Falsifiability
//!
//! Each of these assertions FAILS if the pass were short-circuited:
//!
//! - `RecordingSink::open_calls == 1` — fails if `CellComplex::new`
//!   never gets constructed.
//! - `RecordingSink::node_calls.len() == expected_unique_tokens` —
//!   fails if `add_node` is skipped or token deduplication breaks.
//! - `RecordingSink::edge_calls.len() == expected_cooccurrence_pairs` —
//!   fails if `add_edge` is skipped, edges aren't canonicalized, or
//!   the co-occurrence formula collapses to no pairs.
//! - `cx.build_delta_0().nnz() > 0` — fails if the complex is empty,
//!   if stalks aren't seeded, or if restriction maps are missing.
//!   Math-friend §3: "impossible to satisfy without leyline-sheaf
//!   machinery actually running."
//! - `tracker_calls.len() == observations.len()` — fails if
//!   `CoChangeTracker::observe` is never invoked or called with the
//!   wrong shape.
//! - `tracker_call_with_nonempty_changes_exists` — fails on the empty-
//!   `all_edges` silent-failure mode math-friend §4 names.

use leyline_cli_lib::daemon::complex_build_pass::{
    BuildOutcome, ComplexSink, ObservationRow, RealTracker, TrackerSink, build_complex,
};
use leyline_sheaf::complex::{CellComplex, RestrictionMap};
use leyline_sheaf::topology::RegionId;

/// Records every `ComplexSink` call while also constructing a real
/// `CellComplex` underneath. The real complex is what the post-
/// condition `nnz > 0` is checked against — proving operator-level
/// reach, not just trait-level.
struct RecordingSink {
    inner: Option<CellComplex>,
    open_calls: usize,
    node_calls: Vec<(RegionId, Vec<f32>)>,
    edge_calls: Vec<(u32, RegionId, RegionId, usize)>,
}

impl RecordingSink {
    fn new() -> Self {
        Self {
            inner: None,
            open_calls: 0,
            node_calls: Vec::new(),
            edge_calls: Vec::new(),
        }
    }
}

impl ComplexSink for RecordingSink {
    fn open(&mut self, node_stalk_dim: usize) {
        self.open_calls += 1;
        self.inner = Some(CellComplex::new(node_stalk_dim));
    }

    fn add_node(&mut self, id: RegionId, data: Vec<f32>) {
        self.node_calls.push((id, data.clone()));
        self.inner
            .as_mut()
            .expect("RecordingSink::add_node before open()")
            .add_node(id, data);
    }

    fn add_edge(
        &mut self,
        edge_id: u32,
        source: RegionId,
        target: RegionId,
        agreement_dim: usize,
        label: Option<String>,
        map_source: RestrictionMap,
        map_target: RestrictionMap,
    ) {
        self.edge_calls
            .push((edge_id, source, target, agreement_dim));
        self.inner
            .as_mut()
            .expect("RecordingSink::add_edge before open()")
            .add_edge(
                edge_id,
                source,
                target,
                agreement_dim,
                label,
                map_source,
                map_target,
                false,
            );
    }

    fn finalize(self: Box<Self>) -> CellComplex {
        self.inner.expect("RecordingSink::finalize before open()")
    }
}

/// Records every `TrackerSink::observe` call so the gate can prove
/// `CoChangeTracker::observe` is actually reached with the right
/// shape. Also forwards to a real `CoChangeTracker` so we can assert
/// the tracker's `edge_count() > 0` post-condition (math-friend §4
/// mitigation for the `all_edges = &[]` silent-failure mode).
struct RecordingTracker {
    inner: RealTracker,
    calls: Vec<(Vec<RegionId>, Vec<(RegionId, RegionId)>)>,
}

impl RecordingTracker {
    fn new() -> Self {
        Self {
            inner: RealTracker::new(0.1),
            calls: Vec::new(),
        }
    }
}

impl TrackerSink for RecordingTracker {
    fn observe(&mut self, changed: &[RegionId], all_edges: &[(RegionId, RegionId)]) {
        self.calls.push((changed.to_vec(), all_edges.to_vec()));
        self.inner.observe(changed, all_edges);
    }
}

/// Math-friend §3 fixture: 5–10 observations citing 3–4 distinct
/// tokens. Constructed so:
/// - 4 distinct tokens → 4 unique 0-cells
/// - At least one observation has ≥2 mentions → ≥1 edge created
/// - At least one token has a different `activity` count than another
///   so δ⁰ is non-trivial when stalks = activity counts (math-friend
///   §4 mitigation: "seed at least one disagreeing stalk in the gate
///   fixture")
fn fixture_observations() -> Vec<ObservationRow> {
    vec![
        ObservationRow {
            mentions: vec!["alpha".into(), "beta".into()],
        },
        ObservationRow {
            mentions: vec!["alpha".into(), "gamma".into()],
        },
        ObservationRow {
            mentions: vec!["beta".into(), "gamma".into()],
        },
        ObservationRow {
            mentions: vec!["delta".into()],
        },
        ObservationRow {
            mentions: vec!["alpha".into(), "delta".into()],
        },
    ]
}

#[test]
fn gate_2_complex_build_pass_mechanically_reaches_cell_complex() {
    let observations = fixture_observations();

    let mut sink = Box::new(RecordingSink::new());
    let mut tracker = Box::new(RecordingTracker::new());

    let outcome = build_complex(&observations, sink.as_mut(), tracker.as_mut());

    // (a) Construction reached.
    assert_eq!(
        sink.open_calls, 1,
        "ComplexBuildPass must call CellComplex::new exactly once \
         (gate 2: math layer is load-bearing). open_calls={}",
        sink.open_calls,
    );

    // (b) 4 unique tokens (alpha, beta, gamma, delta) → 4 add_node
    // calls. Fails if token deduplication breaks or add_node is
    // skipped.
    let expected_unique_tokens = 4usize;
    assert_eq!(
        sink.node_calls.len(),
        expected_unique_tokens,
        "expected {expected_unique_tokens} add_node calls (one per \
         unique mention token), got {}: {:?}",
        sink.node_calls.len(),
        sink.node_calls,
    );

    // (c) Expected co-occurrence pairs from the fixture:
    //     {alpha,beta}, {alpha,gamma}, {beta,gamma}, {alpha,delta}
    // = 4 unique unordered pairs. Fails if add_edge is skipped or the
    // pair canonicalization regresses to duplicated (a,b)/(b,a) edges
    // (math-friend §4 direction-asymmetric edge insertion mitigation).
    let expected_edges = 4usize;
    assert_eq!(
        sink.edge_calls.len(),
        expected_edges,
        "expected {expected_edges} add_edge calls (one per unique \
         co-occurrence pair), got {}: {:?}",
        sink.edge_calls.len(),
        sink.edge_calls,
    );

    // Every recorded edge must have source < target — canonical
    // ordering. Mitigates the math-friend §4 direction-asymmetric
    // silent failure (CellComplex doesn't normalize pairs; only
    // CoChangeTracker does).
    for (eid, s, t, _) in &sink.edge_calls {
        assert!(
            s < t,
            "edge {eid} not canonicalized: source={s} not < target={t}",
        );
    }

    // (d) The produced complex's δ⁰ must have nonzero nnz — math-
    // friend §3 simplest sufficient: "impossible to satisfy without
    // leyline-sheaf machinery actually running." Pure type-only reach
    // never executes build_delta_0; this catches it.
    let cx = sink.finalize();
    let delta = cx.build_delta_0();
    assert!(
        delta.nnz() > 0,
        "δ⁰ must have nonzero nnz — proves CellComplex machinery \
         actually ran. Got nnz={}",
        delta.nnz(),
    );

    // Belt-and-braces: complex internal counts match recorded calls.
    assert_eq!(
        cx.nodes.len(),
        expected_unique_tokens,
        "CellComplex.nodes.len() must match add_node call count",
    );
    assert_eq!(
        cx.edges.len(),
        expected_edges,
        "CellComplex.edges.len() must match add_edge call count",
    );

    // (e) CoChangeTracker::observe was called once per observation
    // (5). Fails if the loop is skipped entirely or wired to the
    // wrong driver.
    assert_eq!(
        tracker.calls.len(),
        observations.len(),
        "tracker.observe must be called once per observation: \
         {} observations vs {} calls",
        observations.len(),
        tracker.calls.len(),
    );

    // (f) At least one tracker call must pass a non-empty `all_edges`
    // — math-friend §4 silent-failure mitigation: empty all_edges
    // means EMA touches nothing, no panic. The fixture has 4 edges,
    // so every call should carry all 4.
    for (i, (_changed, all_edges)) in tracker.calls.iter().enumerate() {
        assert_eq!(
            all_edges.len(),
            expected_edges,
            "tracker call {i}: all_edges must be the full edge set \
             ({expected_edges}); got {}",
            all_edges.len(),
        );
    }

    // (g) At least one tracker call must have a non-empty `changed`
    // set (else CoChangeTracker.rates never gets a positive signal).
    // 4 of our 5 fixture rows have ≥2 mentions, so at least 4 calls
    // pass non-empty changed.
    let nonempty_changed = tracker.calls.iter().filter(|(c, _)| !c.is_empty()).count();
    assert!(
        nonempty_changed >= 4,
        "at least 4 tracker calls must carry non-empty `changed` \
         (else tracker.rates can't update); got {nonempty_changed}",
    );

    // (i) Math-friend bead `ley-line-open-66095f` (LOW): δ⁰ structural
    // nnz alone is satisfied by restriction-map ±1 coefficients regardless
    // of stalk values — a regression that fed `vec![0.0]` to every node
    // would still pass (d). Tighten with `detect_violations()`: requires
    // the section actually carry the per-node activity counts the pass
    // computes, then runs full δ⁰ × x + magnitude filter. With our fixture
    // activities (alpha=3, beta=2, gamma=2, delta=2), three edges produce
    // non-zero margins: (alpha,beta), (alpha,gamma), (alpha,delta).
    let violations = cx.detect_violations();
    assert!(
        !violations.is_empty(),
        "detect_violations must produce >=1 violation on the fixture — \
         catches vacuous-stalk regressions that nnz(δ⁰) alone misses. \
         Got 0 violations from cx.nodes={} cx.edges={}",
        cx.nodes.len(),
        cx.edges.len(),
    );

    // (h) Outcome counters match the structural counts.
    assert_eq!(
        outcome,
        BuildOutcome {
            nodes_added: expected_unique_tokens as u64,
            edges_added: expected_edges as u64,
            observations_processed: observations.len() as u64,
        },
        "BuildOutcome counters drifted from sink-recorded counts",
    );
}

#[test]
fn gate_2_empty_observations_does_not_invoke_cell_complex_meaningfully() {
    // Negative control: with zero observations, the pass still calls
    // CellComplex::new (open is unconditional — math-friend §3 wants
    // construction to be a no-op proof of trait reach), but no
    // add_node / add_edge calls fire, and the complex's δ⁰ has
    // nnz=0. If a future refactor moved meaningful work outside the
    // observations loop, this test would catch it.
    let mut sink = Box::new(RecordingSink::new());
    let mut tracker = Box::new(RecordingTracker::new());
    let outcome = build_complex(&[], sink.as_mut(), tracker.as_mut());

    assert_eq!(sink.open_calls, 1, "open() must still be invoked");
    assert!(sink.node_calls.is_empty(), "no nodes from empty input");
    assert!(sink.edge_calls.is_empty(), "no edges from empty input");
    assert!(
        tracker.calls.is_empty(),
        "no tracker calls from empty input"
    );

    let cx = sink.finalize();
    assert_eq!(cx.build_delta_0().nnz(), 0);
    assert_eq!(outcome.nodes_added, 0);
    assert_eq!(outcome.edges_added, 0);
}
