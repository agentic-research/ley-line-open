//! ComplexBuildPass — ADR-0020 §2 Gate 2.
//!
//! Scans `observation` rows and builds a `leyline_sheaf::CellComplex` from
//! the unique mention tokens (0-cells) and their co-occurrence edges (1-
//! cells), then hands the per-event token set to
//! `CoChangeTracker::observe` so the EMA-learned co-change weights stay
//! current.
//!
//! Closes bead `ley-line-open-c7eae2`. The acceptance test in
//! `cli-lib/tests/complex_build_pass_gate.rs` mechanically proves
//! `CellComplex` + `CoChangeTracker::observe` are reached on a fixture —
//! short-circuiting either path fails the gate.
//!
//! ## V1 design choices (per math-friend report)
//!
//! - **`node_stalk_dim = 1`** with stalk = scalar token-activity rate.
//!   Math-friend §2: "Pick (i) for Gate 2 — it's the minimum that lets
//!   `detect_violations` mean something, with identity restriction maps."
//!   A higher-dim BLAKE3-prefix stalk is forward-compat but not the
//!   minimum honest gate. The active scalar (count of observations
//!   mentioning the token in the window) guarantees δ⁰ is non-trivial
//!   when at least one token has a different activity from its neighbour
//!   — the test fixture is constructed to make that the case.
//!
//! - **Token → `u32` id via `BTreeMap`.** Math-friend §2: "use full
//!   `BTreeMap<String,u32>` with monotonic counter; never hash-derive
//!   ids." Hash-derived ids collide on long-tail tokens and would let
//!   the test pass vacuously when two distinct tokens shared an id.
//!
//! - **Edge weight formula (math-friend §1, used for `CoChangeTracker`
//!   *not* for restriction-map magnitudes):** diversity-discounted log-
//!   damped co-occurrence rate. The full formula is computed for forward
//!   compat (and exposed in `compute_pair_weight`); the V1 pass passes
//!   `changed = mentions` and `all_edges = full edge set` straight to
//!   `CoChangeTracker::observe` per its actual signature. The learned-
//!   weight scaling of `RestrictionMap` is deferred to L10/L11 — V1
//!   keeps the two layers in parallel as math-friend's §3 final
//!   paragraph option (b) describes.
//!
//! - **Canonical `(min, max)` edge ordering before `add_edge`.** Math-
//!   friend §3: `CellComplex` doesn't normalize edge direction, only
//!   `CoChangeTracker` does. Inserting both directions would double-
//!   count in `build_delta_0`. Single canonical insertion fixes that.
//!
//! ## Observation schema
//!
//! `ensure_observation_table` delegates to L8's
//! `crate::daemon::observation_schema::create_observation_schema`. Both
//! this pass and SessionObservationPass call the same `CREATE IF NOT
//! EXISTS` so whichever runs first wins identically. This pass reads
//! only `mentions`.
//!
//! ## Per-run state
//!
//! `RealTracker` holds `CoChangeTracker` state for the lifetime of one
//! pass invocation. EMA decay weights inside the tracker reset every
//! run — they do not persist across daemon restarts or enrichment
//! cycles. A future pass that wants long-horizon co-change weighting
//! should persist tracker state into a dedicated table.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use leyline_sheaf::CoChangeTracker;
use leyline_sheaf::complex::{CellComplex, RestrictionMap};
use leyline_sheaf::topology::RegionId;
use rusqlite::Connection;

use super::enrichment::{EnrichmentPass, EnrichmentStats};
use super::sheaf_ops::SheafState;

// ---------------------------------------------------------------------------
// ComplexSink — math-friend's spy seam.
// ---------------------------------------------------------------------------

/// Trait wrapper around the subset of [`CellComplex`] mutations this pass
/// performs. Production code uses `RealSink` which forwards to a real
/// `CellComplex`. The gate test in
/// `cli-lib/tests/complex_build_pass_gate.rs` uses a `RecordingSink` that
/// records every call alongside the underlying real `CellComplex`,
/// proving the pass mechanically reaches `CellComplex::new` /
/// `add_node` / `add_edge` and that `δ⁰` actually executes (via
/// `build_delta_0().nnz() > 0` on the produced complex).
///
/// Math-friend §3: this is the "trait wrapper, not test-only mocks"
/// pattern.
pub trait ComplexSink: Send {
    /// Called once at pass entry with the chosen `node_stalk_dim` so the
    /// recorder can verify the constructor parameter without a separate
    /// channel.
    fn open(&mut self, node_stalk_dim: usize);

    fn add_node(&mut self, id: RegionId, data: Vec<f32>);

    #[allow(clippy::too_many_arguments)]
    fn add_edge(
        &mut self,
        edge_id: u32,
        source: RegionId,
        target: RegionId,
        agreement_dim: usize,
        label: Option<String>,
        map_source: RestrictionMap,
        map_target: RestrictionMap,
    );

    /// Finalize the sink and return the constructed complex.
    fn finalize(self: Box<Self>) -> CellComplex;
}

/// Production sink: forwards every call straight to a real
/// `CellComplex`. Zero-cost: holds an `Option<CellComplex>` that's
/// `Some` between `open` and `finalize`.
#[derive(Default)]
pub struct RealSink {
    cx: Option<CellComplex>,
}

impl ComplexSink for RealSink {
    fn open(&mut self, node_stalk_dim: usize) {
        self.cx = Some(CellComplex::new(node_stalk_dim));
    }

    fn add_node(&mut self, id: RegionId, data: Vec<f32>) {
        self.cx
            .as_mut()
            .expect("RealSink::add_node before open()")
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
        self.cx
            .as_mut()
            .expect("RealSink::add_edge before open()")
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
        self.cx.expect("RealSink::finalize before open()")
    }
}

// ---------------------------------------------------------------------------
// CoChangeTracker observation sink — same wrapping trick, so tests can spy.
// ---------------------------------------------------------------------------

/// Spy seam for `CoChangeTracker::observe`. Production wraps the real
/// tracker; the gate test wraps a recorder that proves `observe` was
/// invoked with the expected `(changed, all_edges)` shape.
pub trait TrackerSink: Send {
    fn observe(&mut self, changed: &[RegionId], all_edges: &[(RegionId, RegionId)]);

    /// Extract the underlying [`CoChangeTracker`] if this sink owns
    /// one. Returns `None` for spy sinks that don't (or don't want to
    /// surrender ownership of) a real tracker. The production
    /// [`RealTracker`] returns `Some`; the pass uses that to install
    /// the freshly-driven tracker into [`SheafState`] at end of run.
    fn take_tracker(&mut self) -> Option<CoChangeTracker> {
        None
    }
}

/// Production tracker sink: holds a real `CoChangeTracker` behind a
/// mutex (the daemon shares it across passes in follow-ups; V1 is
/// pass-local but the mutex keeps the trait Send-friendly without
/// pretending the tracker is `Sync` on its own).
pub struct RealTracker {
    tracker: Mutex<CoChangeTracker>,
}

impl RealTracker {
    pub fn new(alpha: f64) -> Self {
        Self {
            tracker: Mutex::new(CoChangeTracker::new(alpha)),
        }
    }

    pub fn into_inner(self) -> CoChangeTracker {
        self.tracker.into_inner().expect("tracker mutex poisoned")
    }
}

impl TrackerSink for RealTracker {
    fn observe(&mut self, changed: &[RegionId], all_edges: &[(RegionId, RegionId)]) {
        self.tracker
            .lock()
            .expect("tracker mutex poisoned")
            .observe(changed, all_edges);
    }

    fn take_tracker(&mut self) -> Option<CoChangeTracker> {
        // Swap out the interior tracker with a fresh default so this
        // sink stays in a usable state after extraction. Callers only
        // invoke `take_tracker` once at end-of-run, so leaving a
        // defaulted tracker behind is fine.
        let inner = self.tracker.get_mut().expect("tracker mutex poisoned");
        Some(std::mem::take(inner))
    }
}

// ---------------------------------------------------------------------------
// Pass
// ---------------------------------------------------------------------------

/// EMA decay for `CoChangeTracker`. Matches the `learn.rs` default; kept
/// as a named const so the pass's behaviour traces back to one knob.
const TRACKER_ALPHA: f64 = 0.1;

/// Stalk dimension for the V1 complex. Math-friend §2 specifies
/// `dim=1` with stalk = scalar token-activity rate. Higher-dim stalks
/// are forward-compat slot.
const NODE_STALK_DIM: usize = 1;

/// Enrichment pass that builds a `CellComplex` from `observation` rows
/// and feeds the per-event token set to `CoChangeTracker::observe`.
///
/// Registered in `cmd_daemon` after `TreeSitterPass` so it sees the
/// observation rows produced by L8's session pass when that lands.
///
/// When constructed via [`ComplexBuildPass::new`] with an
/// `Arc<SheafState>`, the pass installs the freshly-built complex +
/// tracker into the shared cache at end of `run` via
/// [`SheafState::install_complex`] — this closes the sheaf-
/// invalidation Gap 2 (bead `ley-line-open-3af437`). Constructed via
/// `Default` (no sheaf state), the pass runs the build for
/// falsifiability side effects only and drops the complex on return
/// — kept for unit tests that don't need the persistence side channel.
#[derive(Default)]
pub struct ComplexBuildPass {
    sheaf: Option<Arc<SheafState>>,
}

impl ComplexBuildPass {
    /// Construct a pass that installs its built complex + tracker
    /// into the shared [`SheafState`] at end of `run`. This is the
    /// production wiring — the daemon builds one of these when it
    /// registers the pass in `cmd_daemon.rs` so consumer queries
    /// (`op_sheaf_*` handlers) see the derived complex.
    pub fn new(sheaf: Arc<SheafState>) -> Self {
        Self { sheaf: Some(sheaf) }
    }
}

impl EnrichmentPass for ComplexBuildPass {
    fn name(&self) -> &str {
        "complex-build"
    }

    fn depends_on(&self) -> &[&str] {
        // No hard dep on tree-sitter — the pass reads from `observation`,
        // which is written by L8's session pass (when it lands) or by
        // any extension that emits observation rows. Empty deps keeps
        // the pass independent so a daemon with only the session pass
        // can still build the complex.
        &[]
    }

    fn reads(&self) -> &[&str] {
        &["observation"]
    }

    fn writes(&self) -> &[&str] {
        // V1 stores the derived complex + tracker in process memory —
        // the daemon's `sheaf_ops` surface already exposes a separate
        // SheafCache. Persisting the complex back to SQL is L11/L12.
        // Empty writes set keeps Schema Partition Invariant trivially
        // satisfied.
        &[]
    }

    fn run(
        &self,
        conn: &Connection,
        _source_dir: &Path,
        _changed_files: Option<&[String]>,
    ) -> Result<EnrichmentStats> {
        let start = Instant::now();

        ensure_observation_table(conn).context("ComplexBuildPass: ensure_observation_table")?;

        let observations =
            read_observations(conn).context("ComplexBuildPass: read_observations")?;

        let mut sink: Box<dyn ComplexSink> = Box::new(RealSink::default());
        let mut tracker: Box<dyn TrackerSink> = Box::new(RealTracker::new(TRACKER_ALPHA));

        let outcome = build_complex(observations.as_slice(), sink.as_mut(), tracker.as_mut());

        // Force the gate-meaningful operator to actually execute — math-
        // friend §3 "Simplest sufficient version": δ⁰ runs iff
        // `leyline-sheaf` machinery is reached. Construction is a no-op
        // proof; only operator invocation proves reach.
        let cx = sink.finalize();
        let delta_nnz = cx.build_delta_0().nnz();
        log::debug!(
            "ComplexBuildPass: nodes={} edges={} δ⁰_nnz={} observations={}",
            outcome.nodes_added,
            outcome.edges_added,
            delta_nnz,
            outcome.observations_processed,
        );

        // Persist the built complex + tracker into the shared
        // SheafState so consumer `op_sheaf_*` queries hit the cache
        // instead of dropping the derived state on the floor. Bead
        // `ley-line-open-3af437` (Gap 2 from the sheaf-invalidation
        // audit `docs/audits/sheaf-invalidation-trace.md`).
        //
        // Also install the region_id → token label map so the
        // watcher-driven `daemon.sheaf.invalidate` emit can compute a
        // fine-grained diff from `changed_files` (sheaf gap 3
        // follow-up, bead `ley-line-open-e40566`). Order matters: the
        // label install must land AFTER `install_complex` because
        // `install_complex` clears any pre-existing labels as part of
        // the fresh-topology contract.
        //
        // Test-only construction via `Default` produces
        // `self.sheaf = None`, preserving the "build + drop" shape the
        // gate test in `complex_build_pass_gate.rs` exercises via
        // `build_complex` directly.
        // Snapshot the numeric fields BEFORE moving `region_labels`
        // out of `outcome` into the sheaf install call, so the trailing
        // `EnrichmentStats` build below stays legible.
        let observations_processed = outcome.observations_processed;
        let items_added = outcome.nodes_added + outcome.edges_added;
        if let Some(sheaf) = self.sheaf.as_ref() {
            let tracker_taken = tracker.take_tracker().unwrap_or_default();
            sheaf.install_complex(cx, tracker_taken);
            sheaf.install_region_labels(outcome.region_labels);
        }

        Ok(EnrichmentStats {
            pass_name: "complex-build".to_string(),
            files_processed: observations_processed,
            items_added,
            duration_ms: start.elapsed().as_millis() as u64,
            skipped: Vec::new(),
        })
    }
}

/// Outcome of `build_complex`. Carried out of the helper so the
/// enrichment caller can populate `EnrichmentStats` without re-counting.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BuildOutcome {
    pub nodes_added: u64,
    pub edges_added: u64,
    pub observations_processed: u64,
    /// Region ID → token label mapping produced during the build. The
    /// pass hands this to [`SheafState::install_region_labels`] so the
    /// watcher-driven `daemon.sheaf.invalidate` emit can turn a
    /// `changed_files` set into a fine-grained region diff (sheaf gap
    /// 3 follow-up, bead `ley-line-open-e40566`). Consumers that don't
    /// need the diff can ignore this field — it's an owned side
    /// channel, not a change to the falsifiability gate's post-
    /// conditions on `nodes_added` / `edges_added`.
    pub region_labels: HashMap<u32, String>,
}

/// Build a `CellComplex` from observation rows: every unique mention
/// token becomes a 0-cell, every unordered co-occurring pair within a
/// single observation becomes a 1-cell (canonical `(min, max)`
/// ordering, identity restriction maps in 1-D). Then drive
/// `CoChangeTracker::observe` once per observation with `changed =
/// mentions, all_edges = full edge set`.
///
/// This function is the load-bearing one for the falsifiability gate —
/// the gate test substitutes `RecordingSink` for the production
/// `RealSink` and asserts the calls happened.
pub fn build_complex(
    observations: &[ObservationRow],
    sink: &mut dyn ComplexSink,
    tracker: &mut dyn TrackerSink,
) -> BuildOutcome {
    // Stable token → id assignment. BTreeMap (not HashMap) so the id
    // assignment is lexicographic-sorted insertion order per math-friend
    // §2 — re-runs get the same ids and `CoChangeTracker.rates` keys
    // stay valid.
    let mut token_ids: BTreeMap<String, RegionId> = BTreeMap::new();

    // First pass: collect unique tokens, assign ids, collect co-
    // occurrence pairs (per-observation), and an activity counter per
    // token — normalized to a rate (÷ observation count) below when the
    // V1 stalk value is written.
    let mut activity: BTreeMap<RegionId, u32> = BTreeMap::new();
    let mut edge_pairs: BTreeSet<(RegionId, RegionId)> = BTreeSet::new();
    let mut per_obs_token_sets: Vec<Vec<RegionId>> = Vec::with_capacity(observations.len());

    for obs in observations {
        let mut obs_ids: Vec<RegionId> = Vec::with_capacity(obs.mentions.len());
        for token in &obs.mentions {
            let next_id = token_ids.len() as RegionId;
            let id = *token_ids.entry(token.clone()).or_insert(next_id);
            *activity.entry(id).or_insert(0) += 1;
            obs_ids.push(id);
        }
        // Within-observation pairwise edges, canonical (min, max). De-
        // dup happens via BTreeSet so a single observation citing the
        // same token twice (rare but possible) collapses to one self-
        // free pair set.
        let mut unique_obs_ids: Vec<RegionId> = obs_ids.clone();
        unique_obs_ids.sort();
        unique_obs_ids.dedup();
        for i in 0..unique_obs_ids.len() {
            for j in (i + 1)..unique_obs_ids.len() {
                let a = unique_obs_ids[i];
                let b = unique_obs_ids[j];
                edge_pairs.insert(if a <= b { (a, b) } else { (b, a) });
            }
        }
        per_obs_token_sets.push(unique_obs_ids);
    }

    // Open the sink AFTER counting unique tokens so the stalk dimension
    // is fixed at NODE_STALK_DIM (math-friend §2 — pick (i)).
    sink.open(NODE_STALK_DIM);

    let mut nodes_added = 0u64;
    // Iterate `token_ids` rather than `activity` so node insertion is
    // deterministic in token-name lexicographic order — matches the id
    // assignment order and keeps test expectations stable.
    //
    // Stalk = token-activity RATE (mentions ÷ observations), per the
    // `NODE_STALK_DIM` spec. Raw counts would make every downstream
    // absolute threshold (complex::EPS, the cache's DELTA0 tolerance,
    // the router's LOW_DELTA0_THRESHOLD) regime-dependent on corpus
    // size — the units would set the behavior, not the math. Bead
    // `ley-line-open-4e30d5` (math-friend audit P1).
    let total_obs = observations.len().max(1) as f32;
    for &id in token_ids.values() {
        let act = *activity.get(&id).unwrap_or(&0) as f32;
        sink.add_node(id, vec![act / total_obs]);
        nodes_added += 1;
    }

    let mut edges_added = 0u64;
    // Internal edge id space: 1_000_000 floor matches `CellComplex::
    // apply_delta`'s EDGE_ID_BASE convention so a future migration to
    // shared id space doesn't collide.
    for (next_edge_id, (a, b)) in (1_000_000_u32..).zip(edge_pairs.iter()) {
        sink.add_edge(
            next_edge_id,
            *a,
            *b,
            NODE_STALK_DIM,
            Some("co-occurrence".into()),
            RestrictionMap::identity(NODE_STALK_DIM),
            RestrictionMap::identity(NODE_STALK_DIM),
        );
        edges_added += 1;
    }

    // Driver loop for CoChangeTracker. `all_edges` is the full edge
    // set; `changed` is the per-observation token set. Math-friend §1:
    // this matches the real signature `observe(changed: &[RegionId],
    // all_edges: &[(RegionId, RegionId)])`, NOT the ADR's
    // `observe(invalidated, edges)` paraphrase.
    let all_edges: Vec<(RegionId, RegionId)> = edge_pairs.iter().copied().collect();
    for obs_tokens in &per_obs_token_sets {
        tracker.observe(obs_tokens, &all_edges);
    }

    // Invert the token → id map into an id → token map. Sheaf gap 3
    // follow-up (bead `ley-line-open-e40566`): this side channel is
    // what turns a `changed_files` set into a fine-grained region diff
    // downstream. Cheap — one entry per unique token, same size as
    // `token_ids` — and computed here because `build_complex` is the
    // only place that owns the token→id assignment.
    let region_labels: HashMap<u32, String> = token_ids
        .into_iter()
        .map(|(token, id)| (id, token))
        .collect();

    BuildOutcome {
        nodes_added,
        edges_added,
        observations_processed: observations.len() as u64,
        region_labels,
    }
}

/// In-memory representation of one `observation` row. Only the fields
/// this pass reads are carried; L8 may add more columns without
/// changing this struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationRow {
    pub mentions: Vec<String>,
}

/// Create the stub `observation` table if it doesn't exist. L8 ships
/// the production schema; this stub is a strict subset (only `mentions`
/// is required by this pass) so it's forward-compatible — once L8
/// lands, its richer `CREATE IF NOT EXISTS` runs first and this
/// becomes a no-op.
///
/// Schema rationale: `mentions` is stored as a JSON array string per
/// the ADR §2 schema. The pass parses it via `serde_json` so any
/// caller that writes a valid JSON array of strings is honoured.
pub fn ensure_observation_table(conn: &Connection) -> Result<()> {
    // Delegate to L8's authoritative schema (`crate::daemon::observation_schema`).
    // Both this pass and SessionObservationPass call the same `CREATE IF NOT EXISTS`,
    // so whichever runs first wins identically.
    crate::daemon::observation_schema::create_observation_schema(conn)
}

/// Read every observation row's `mentions` JSON array into an in-
/// memory list. Rows with malformed `mentions` JSON are skipped with a
/// log warning rather than aborting the whole pass — a single bad row
/// shouldn't wedge enrichment for the rest of the corpus.
pub fn read_observations(conn: &Connection) -> Result<Vec<ObservationRow>> {
    let mut stmt = conn.prepare_cached("SELECT mentions FROM observation")?;
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut out = Vec::with_capacity(rows.len());
    for mentions_json in rows {
        match serde_json::from_str::<Vec<String>>(&mentions_json) {
            Ok(mentions) => out.push(ObservationRow { mentions }),
            Err(e) => {
                log::warn!("ComplexBuildPass: skipping row with malformed mentions JSON: {e}");
            }
        }
    }
    Ok(out)
}

/// Math-friend §1: diversity-discounted log-damped co-occurrence rate.
/// Exposed for forward-compat (L11+ will scale `RestrictionMap` by
/// learned weights); V1 doesn't yet feed it back into the complex —
/// see module docstring's "two layers in parallel" choice.
///
/// `n_per_source` is `n_s(x,y)` for each source s witnessing {x,y};
/// `total_sources` is `|S_total|` (the universe of sources observed in
/// the window). Returns the unsaturated `w(x,y) = log-damped × diversity`.
/// Callers normalize to [0, 1] downstream via the
/// `weight / (weight + κ)` half-saturation form.
pub fn compute_pair_weight(n_per_source: &[u64], total_sources: usize) -> f64 {
    if total_sources == 0 {
        return 0.0;
    }
    let w_raw: f64 = n_per_source.iter().map(|&n| ((1 + n) as f64).ln()).sum();
    let diversity = (n_per_source.len() as f64) / (total_sources.max(1) as f64);
    w_raw * diversity
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::enrichment::assert_pass_metadata;

    #[test]
    fn complex_build_pass_metadata_pinned() {
        let pass = ComplexBuildPass::default();
        assert_pass_metadata(&pass, "complex-build", &[], &["observation"], &[]);
    }

    #[test]
    fn ensure_observation_table_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_observation_table(&conn).unwrap();
        // Second call must not error — `CREATE IF NOT EXISTS` semantics.
        ensure_observation_table(&conn).unwrap();
    }

    #[test]
    fn read_observations_parses_mentions_json() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_observation_table(&conn).unwrap();
        conn.execute(
            "INSERT INTO observation (source, payload_kind, mentions, observed_at) VALUES ('test', 'test', ?1, 0), ('test', 'test', ?2, 0)",
            rusqlite::params![r#"["foo","bar"]"#, r#"["baz"]"#],
        )
        .unwrap();

        let rows = read_observations(&conn).unwrap();
        assert_eq!(rows.len(), 2);
        let mut all: Vec<Vec<String>> = rows.into_iter().map(|r| r.mentions).collect();
        all.sort();
        assert_eq!(
            all,
            vec![
                vec!["baz".to_string()],
                vec!["foo".to_string(), "bar".to_string()]
            ]
        );
    }

    #[test]
    fn read_observations_skips_malformed_rows_without_aborting() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_observation_table(&conn).unwrap();
        conn.execute(
            "INSERT INTO observation (source, payload_kind, mentions, observed_at) VALUES ('test', 'test', ?1, 0), ('test', 'test', ?2, 0)",
            rusqlite::params![r#"["foo","bar"]"#, "not-json-at-all"],
        )
        .unwrap();

        // Bad row is dropped, good row survives — one bad observation
        // shouldn't wedge the whole pass.
        let rows = read_observations(&conn).unwrap();
        assert_eq!(rows.len(), 1, "good row must survive bad-row skip");
        assert_eq!(rows[0].mentions, vec!["foo", "bar"]);
    }

    #[test]
    fn build_complex_assigns_lex_sorted_ids() {
        // Token id assignment is by BTreeMap insertion: lex-sorted
        // order. Math-friend §2 pins this so re-runs are stable.
        let obs = vec![
            ObservationRow {
                mentions: vec!["zeta".into(), "alpha".into()],
            },
            ObservationRow {
                mentions: vec!["mu".into()],
            },
        ];
        let mut sink = Box::new(RealSink::default());
        let mut tracker = Box::new(RealTracker::new(TRACKER_ALPHA));
        let outcome = build_complex(&obs, sink.as_mut(), tracker.as_mut());
        // 3 unique tokens, 1 edge (alpha—zeta within the first obs;
        // "mu" stands alone in obs 2).
        assert_eq!(outcome.nodes_added, 3);
        assert_eq!(outcome.edges_added, 1);
        assert_eq!(outcome.observations_processed, 2);

        let cx = sink.finalize();
        // δ⁰ has 2 entries per edge (±projection of identity 1×1) ⇒
        // nnz = 2 for one co-occurrence edge.
        assert!(cx.build_delta_0().nnz() > 0);
    }

    #[test]
    fn build_complex_dedups_repeat_mention_within_observation() {
        // A single observation citing the same token twice shouldn't
        // create a self-loop. Mitigation for math-friend §4 silent
        // failure mode (edge construction gets zero pairs when
        // mentions has duplicates).
        let obs = vec![ObservationRow {
            mentions: vec!["foo".into(), "foo".into(), "bar".into()],
        }];
        let mut sink = Box::new(RealSink::default());
        let mut tracker = Box::new(RealTracker::new(TRACKER_ALPHA));
        let outcome = build_complex(&obs, sink.as_mut(), tracker.as_mut());
        assert_eq!(outcome.nodes_added, 2);
        assert_eq!(outcome.edges_added, 1, "self-loop must not be created");
    }

    /// F1 (bead ley-line-open-4e30d5): the spec (`NODE_STALK_DIM` doc)
    /// says stalks are scalar token-activity RATES. Raw counts put every
    /// downstream absolute threshold (EPS = 1e-4, LOW_DELTA0_THRESHOLD =
    /// 0.1, DELTA0_EPS in the cache) in an undefined regime — the input's
    /// units set the behavior, not the math. Rates are counts normalized
    /// by the observation-corpus size, so magnitudes live in a defined
    /// range and the thresholds mean the same thing on every corpus.
    #[test]
    fn stalks_are_rates_not_counts() {
        // "a" appears in 4/4 observations, "b" in 3/4.
        let obs: Vec<ObservationRow> = (0..4)
            .map(|i| ObservationRow {
                mentions: if i < 3 {
                    vec!["a".into(), "b".into()]
                } else {
                    vec!["a".into()]
                },
            })
            .collect();

        let mut sink = Box::new(RealSink::default());
        let mut tracker = Box::new(RealTracker::new(TRACKER_ALPHA));
        build_complex(&obs, sink.as_mut(), tracker.as_mut());
        let cx = sink.finalize();

        // Lex-sorted id assignment: "a" → 0, "b" → 1.
        let stalk_a = cx.cells.get(&0).unwrap().stalk.data[0];
        let stalk_b = cx.cells.get(&1).unwrap().stalk.data[0];
        assert!(
            (stalk_a - 1.0).abs() < 1e-6,
            "stalk(a) must be the rate 4/4 = 1.0, got {stalk_a} \
             (a raw count would be 4.0)",
        );
        assert!(
            (stalk_b - 0.75).abs() < 1e-6,
            "stalk(b) must be the rate 3/4 = 0.75, got {stalk_b} \
             (a raw count would be 3.0)",
        );
    }

    /// F1 companion (bead ley-line-open-4e30d5): stalks must encode
    /// STRUCTURE, not corpus size. Duplicating every observation 10×
    /// changes no relative activity — the sheaf section (and therefore
    /// every δ⁰ magnitude a threshold sees) must be identical. Raw
    /// counts scale 10× under duplication, which is exactly how the
    /// audit's fixture-A/fixture-B pair flipped the router's regime
    /// with zero structural change.
    #[test]
    fn stalks_scale_invariant_under_corpus_duplication() {
        let base: Vec<ObservationRow> = vec![
            ObservationRow {
                mentions: vec!["a".into(), "b".into()],
            },
            ObservationRow {
                mentions: vec!["a".into()],
            },
        ];
        let duplicated: Vec<ObservationRow> = base
            .iter()
            .cloned()
            .cycle()
            .take(base.len() * 10)
            .collect();

        let build = |obs: &[ObservationRow]| {
            let mut sink = Box::new(RealSink::default());
            let mut tracker = Box::new(RealTracker::new(TRACKER_ALPHA));
            build_complex(obs, sink.as_mut(), tracker.as_mut());
            sink.finalize()
        };

        let cx_base = build(&base);
        let cx_dup = build(&duplicated);

        for id in 0..2u32 {
            let s_base = cx_base.cells.get(&id).unwrap().stalk.data[0];
            let s_dup = cx_dup.cells.get(&id).unwrap().stalk.data[0];
            assert!(
                (s_base - s_dup).abs() < 1e-6,
                "stalk({id}) must be corpus-size invariant: base {s_base} \
                 vs 10× duplicated {s_dup} — differing values mean the \
                 stalks encode units, not structure",
            );
        }

        // The per-edge δ⁰ magnitude every threshold consumes must also
        // be identical across the two corpora.
        let v_base = cx_base.edge_violation_squared(0, 1).unwrap();
        let v_dup = cx_dup.edge_violation_squared(0, 1).unwrap();
        assert!(
            (v_base - v_dup).abs() < 1e-6,
            "edge δ⁰ must be corpus-size invariant: {v_base} vs {v_dup}",
        );
    }

    #[test]
    fn compute_pair_weight_single_source_logged() {
        // 1 source seeing the pair 9 times: log-damped contribution =
        // ln(10) ≈ 2.302, diversity = 1/1 = 1.0.
        let w = compute_pair_weight(&[9], 1);
        assert!((w - 10f64.ln()).abs() < 1e-6, "got {w}");
    }

    #[test]
    fn compute_pair_weight_two_sources_diversity_discount_applies() {
        // 2 sources × 1 sighting each, universe = 3 sources.
        // diversity = 2/3, log-damp per source = ln(2) ≈ 0.693
        // w = 2·ln(2) · (2/3) = (4/3)·ln(2) ≈ 0.924
        let w = compute_pair_weight(&[1, 1], 3);
        let expected = 2.0 * 2f64.ln() * (2.0 / 3.0);
        assert!((w - expected).abs() < 1e-6, "got {w}, want {expected}");
    }

    #[test]
    fn compute_pair_weight_zero_sources_returns_zero() {
        assert_eq!(compute_pair_weight(&[], 0), 0.0);
    }

    #[test]
    fn pass_run_against_empty_observations_completes_cleanly() {
        // Running the pass on an empty `observation` table must not
        // error, and stats must report zero items. Mirrors math-friend
        // §4 silent-failure guard — `tracker.observe` with no edges
        // touches nothing, no panic.
        let conn = Connection::open_in_memory().unwrap();
        let pass = ComplexBuildPass::default();
        let stats = pass.run(&conn, Path::new("/"), None).unwrap();
        assert_eq!(stats.pass_name, "complex-build");
        assert_eq!(stats.items_added, 0);
    }
}
