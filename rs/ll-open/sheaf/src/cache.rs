//! Sheaf cache: structurally-aware cache invalidation, heuristic proxy for δ⁰.
//!
//! ## What this cache actually does
//!
//! Invalidation is driven by **XOR of endpoint Merkle roots** compared against
//! a stored boundary hash, plus a **restriction-graph BFS** (depth-bounded in
//! heuristic mode; gate-terminated in δ⁰ mode). This
//! is a fast structural proxy, NOT the Čech coboundary operator δ⁰. In
//! particular:
//!
//! - The boundary check (see [`SheafCache::check_boundary_changed`]) flags an
//!   edge as "changed" whenever `H(stalk_a) ⊕ H(stalk_b)` differs from the
//!   stored hash. It does **not** apply the restriction map, so it cannot
//!   distinguish content changes that fall outside the agreement subspace
//!   from genuine sheaf disagreements.
//! - In heuristic mode the cascade is bounded by the hardcoded
//!   `HEURISTIC_CASCADE_DEPTH` (3 hops) — a blast-radius heuristic, not a
//!   sheaf-derived reach. In δ⁰ mode (complex attached) the cascade runs to
//!   the per-edge convergence gate's fixed point with no depth cap; a
//!   configurable safety-valve budget ([`SheafCache::set_cascade_budget`],
//!   default unbounded) can truncate it, and every truncation with pending
//!   work is counted in [`SheafCache::cascade_truncations`].
//!
//! For a real δ⁰-driven invalidation contract — "evict iff the new section
//! sits outside ker(δ⁰)" — wire through [`crate::complex::CellComplex::detect_violations`]
//! (which applies the restriction maps and operates on the f32 stalk values).
//! The path forward is folding the `CellComplex` into the cache so `on_change`
//! consults the actual coboundary instead of the hash proxy. Tracked by the
//! daemon-wiring bead.
//!
//! ## Cache contract
//!
//! Structurally-aware BFS-bounded hash invalidation. Health monitoring uses
//! the sheaf-derived defect metric `Σ‖δ⁰‖²` (see
//! [`crate::complex::CellComplex::consistency_analysis`]); eviction uses the
//! hash-comparison BFS cascade. Co-change-learned edge weights are NOT used
//! to weight the cascade frontier: `on_change` / `check_boundary_changed`
//! never read `RestrictionEdge::weights`, `co_change_rate`, or
//! `revert_rate`. The learned rates feed [`SheafCache::defect`] (in-crate)
//! and are exported over the wire by the daemon for external consumers;
//! wiring them into the cascade as a coupling prior is a separate design
//! decision, not current behavior (bead `ley-line-open-4f9553`). No code
//! path here computes ker(δ⁰) — see "What this cache actually does" above
//! for the proxy details and the daemon-wiring bead for the δ⁰-driven
//! upgrade path.
//!
//! ## `on_change` return semantics
//!
//! [`SheafCache::on_change`] returns a list that always contains the
//! `changed_regions` the caller passed in (the cascade roots), plus any
//! BFS-reachable neighbors whose boundary projection moved beyond
//! `DELTA0_EPS` in norm space (or whose XOR pre-filter fired, in
//! heuristic-only mode). The cascade roots appear unconditionally — they are the
//! caller's assertion about what changed, not a measurement — so the
//! list is never empty for a non-empty `changed_regions` input.
//!
//! Neighbor entries reflect the boundary check on each (root, neighbor)
//! edge; a root whose stalk did not move propagates no neighbors. This is
//! a **structural answer about the sheaf section** — it is NOT "regions
//! to evict from this cache". In particular, regions are reported even
//! when this cache holds no entry for them, because UDS / MCP consumers
//! own their own caches and need the full cascade list to evict on their
//! side.
//!
//! The local `entries.valid = false` side-effect still happens for
//! in-process callers that DO have entries — but it is a side-effect on
//! the local map, not a filter on the returned list. Consumers in
//! process and consumers over the wire see the same answer.
//!
//! ## Restriction weight learning
//!
//! Weights on restriction edges encode coupling strength between regions.
//! Derived from co-change history (not configured):
//! - High co-change variance → low weight (dimensions that naturally differ)
//! - Low co-change variance → high weight (dimensions that should agree)
//!
//! Co-change correlates with — but does not derive — sheaf-level coupling.
//! Treat learned weights as a noisy prior, not as a first-principles
//! restriction map. Pair with structural edge labels (`"import"`,
//! `"shared_token"`) when available.
//!

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::complex::CellComplex;
use crate::topology::RegionId;

/// Heuristic-mode (no attached complex) cascade depth bound.
///
/// A blast-radius heuristic, NOT a sheaf invariant — no sheaf quantity
/// names 3. The XOR gate cannot distinguish projected-away content
/// changes from genuine disagreements, so heuristic mode trades cascade
/// completeness for bounded work.
///
/// δ⁰ mode does NOT use this: there the per-edge convergence gate is the
/// mathematically correct termination criterion, and the BFS runs to its
/// fixed point (see [`SheafCache::on_change`]). One constant serves both
/// former copies (`on_change` + `reap`) so they cannot silently drift
/// (bead `ley-line-open-4eef8d`).
const HEURISTIC_CASCADE_DEPTH: u32 = 3;

/// Norm-space threshold below which δ⁰ movement is treated as zero.
/// Matches `complex::EPS` (1e-4); the stage-2 check compares
/// `|√current − √baseline|` against this, one `sqrt` per edge.
///
/// The comparison is deliberately in NORM space, not squared-norm space:
/// `|a² − b²| = |a − b|·(a + b)`, so a squared threshold ε² makes the
/// effective norm sensitivity `ε²/(2·baseline)` — scale-dependent, and at
/// O(1) baselines below the f32 ulp (every representable change fires,
/// i.e. no noise rejection at all). Norm-space comparison keeps the
/// sensitivity uniform across baseline magnitudes. Bead
/// `ley-line-open-4f3f6e` (math-friend audit P4).
const DELTA0_EPS: f32 = 1e-4;

/// A content hash summarizing a region's current state.
pub trait StalkHash {
    /// Compute the Merkle root (or content hash) for this region.
    fn merkle_root(&self) -> [u8; 32];
}

/// An edge in the restriction map between two regions.
#[derive(Debug, Clone)]
pub struct RestrictionEdge {
    /// Per-dimension learned weights. Higher = tighter coupling.
    pub weights: Vec<f64>,
    /// Hash of the shared boundary (symbols, interfaces, types).
    pub boundary_hash: [u8; 32],
    /// Co-change rate from history (how often these regions change together).
    pub co_change_rate: f64,
    /// Revert rate from history (how often co-changes are reverted).
    pub revert_rate: f64,
}

/// A cached value with generation tracking.
#[derive(Debug, Clone)]
pub struct CacheEntry<V> {
    pub value: V,
    /// Generation at which this entry was cached.
    pub generation: u64,
    /// Whether this entry is currently valid.
    pub valid: bool,
}

/// Sheaf cache: entries organized by topological regions.
///
/// Invalidation has two modes:
/// - **Heuristic** (default): the XOR-of-Merkle-roots boundary check drives a
///   bounded restriction-graph BFS. Fast, but over-evicts on content changes
///   that the restriction map would project away.
/// - **δ⁰-driven** (opt-in via [`Self::with_complex`]): the XOR check is a
///   pre-filter; when it says "changed" the cache calls
///   [`CellComplex::edge_violation_squared`] on the attached complex to
///   confirm the disagreement is real before invalidating. Stalk f32 values
///   are pushed into the complex via [`Self::set_stalk_value`].
///
/// `S` is the stalk type (must produce a content hash).
/// `V` is the cached value type.
pub struct SheafCache<S: StalkHash, V> {
    stalks: BTreeMap<RegionId, S>,
    restrictions: BTreeMap<(RegionId, RegionId), RestrictionEdge>,
    entries: BTreeMap<RegionId, CacheEntry<V>>,
    generation: u64,
    /// Optional δ⁰-driven invalidation backing. When `Some`,
    /// `check_boundary_changed` uses it as the authoritative answer after
    /// the XOR pre-filter. The cache pushes f32 stalk updates into the
    /// complex via [`Self::set_stalk_value`].
    complex: Option<CellComplex>,
    /// Per-edge baseline `‖δ⁰‖²` snapshot. Captures the *previous*
    /// agreement state of each edge so `check_boundary_changed` can flag
    /// "the disagreement just moved" rather than "the disagreement is
    /// currently non-zero in absolute terms". Keyed by canonical
    /// `(min, max)` pair to dedupe the undirected edges stored in both
    /// directions in `restrictions`.
    delta_zero_baseline: BTreeMap<(RegionId, RegionId), f32>,
    /// Safety-valve budget for the δ⁰-mode cascade BFS, counted in node
    /// expansions. NOT a correctness mechanism: δ⁰-mode termination comes
    /// from the per-edge convergence gate plus the `visited` set (worst
    /// case O(V+E)). Default `usize::MAX` — effectively unbounded. Lower
    /// it only as an emergency brake, and watch
    /// [`Self::cascade_truncations`] for the falsifiable signal that the
    /// budget ever bound. Configurable via [`Self::set_cascade_budget`].
    cascade_budget: usize,
    /// Number of cascades (`on_change` or `reap`) truncated by
    /// `cascade_budget` while the frontier was still non-empty. Any
    /// non-zero value means an invalidation answer was incomplete —
    /// stale entries may be served as valid. Atomic so the `&self`
    /// reaper can record truncations too.
    cascade_truncations: AtomicU64,
}

impl<S: StalkHash, V> SheafCache<S, V> {
    pub fn new() -> Self {
        Self {
            stalks: BTreeMap::new(),
            restrictions: BTreeMap::new(),
            entries: BTreeMap::new(),
            generation: 0,
            complex: None,
            delta_zero_baseline: BTreeMap::new(),
            cascade_budget: usize::MAX,
            cascade_truncations: AtomicU64::new(0),
        }
    }

    /// Configure the δ⁰-mode cascade safety-valve budget (node expansions
    /// per cascade). See the field doc: this is an emergency brake, not a
    /// correctness bound — the δ⁰ cascade's termination criterion is the
    /// per-edge convergence gate's fixed point. Heuristic mode ignores
    /// this (it uses [`HEURISTIC_CASCADE_DEPTH`]).
    pub fn set_cascade_budget(&mut self, budget: usize) {
        self.cascade_budget = budget;
    }

    /// Number of cascades truncated by [`Self::set_cascade_budget`] while
    /// work remained. Non-zero means at least one invalidation answer was
    /// incomplete; alert on this before trusting cache validity.
    pub fn cascade_truncations(&self) -> u64 {
        self.cascade_truncations.load(Ordering::Relaxed)
    }

    /// Snapshot the current per-edge `‖δ⁰‖²` as the baseline. Subsequent
    /// `on_change` calls treat "current squared norm matches baseline" as
    /// "this edge's agreement is unchanged" — even when the absolute
    /// norm is non-zero. Call this once after seeding the cache so the
    /// initial section becomes the reference state.
    ///
    /// No-op when no `CellComplex` is attached (the heuristic cache has
    /// no notion of per-edge δ⁰).
    pub fn refresh_baseline(&mut self) {
        let Some(cx) = self.complex.as_ref() else {
            return;
        };
        let edge_pairs: Vec<(RegionId, RegionId)> = self
            .restrictions
            .keys()
            .filter(|(a, b)| a <= b)
            .copied()
            .collect();
        for (a, b) in edge_pairs {
            let norm_sq = cx
                .edge_violation_squared(a, b)
                .or_else(|| cx.edge_violation_squared(b, a))
                .unwrap_or(0.0);
            self.delta_zero_baseline.insert((a, b), norm_sq);
        }
    }

    /// Re-snapshot the per-edge `‖δ⁰‖²` baseline only for edges incident to
    /// any region in `regions`. Mirrors [`Self::refresh_baseline`] but
    /// touches the subset — every other baseline entry is left
    /// byte-identical so consumers' cached values for untouched regions
    /// survive the update.
    ///
    /// No-op when no `CellComplex` is attached (the heuristic cache has no
    /// notion of per-edge δ⁰). Edges whose squared-norm can no longer be
    /// computed (e.g. one endpoint was removed by an incremental delta)
    /// have their stale baseline entry dropped — leaving it in place would
    /// silently feed the next `on_change` a stale comparison.
    pub fn refresh_baseline_subset(&mut self, regions: &[RegionId]) {
        let Some(cx) = self.complex.as_ref() else {
            return;
        };
        if regions.is_empty() {
            return;
        }
        let touched: BTreeSet<RegionId> = regions.iter().copied().collect();
        // Snapshot the keys first — re-using `self.restrictions.keys()` while
        // we mutate `delta_zero_baseline` would otherwise still be sound
        // (different fields), but materializing the list keeps the per-edge
        // refresh loop independent of the borrow.
        let edge_pairs: Vec<(RegionId, RegionId)> = self
            .restrictions
            .keys()
            .filter(|(a, b)| a <= b && (touched.contains(a) || touched.contains(b)))
            .copied()
            .collect();
        for (a, b) in edge_pairs {
            match cx
                .edge_violation_squared(a, b)
                .or_else(|| cx.edge_violation_squared(b, a))
            {
                Some(norm_sq) => {
                    self.delta_zero_baseline.insert((a, b), norm_sq);
                }
                None => {
                    // Endpoint missing from the complex after a removal — drop
                    // the stale baseline so the next on_change doesn't compare
                    // current state against a ghost edge.
                    self.delta_zero_baseline.remove(&(a, b));
                }
            }
        }
    }

    /// Drop the stalk, every restriction it touches, and any cache entry
    /// for the given region. Returns the set of *neighbours* the region
    /// was connected to so the caller can fold them into the affected set.
    ///
    /// Pair with [`Self::refresh_baseline_subset`] after a batch of
    /// `drop_region` / `set_restriction` calls so the δ⁰ baseline reflects
    /// the new topology.
    pub fn drop_region(&mut self, region: RegionId) -> Vec<RegionId> {
        let neighbours: Vec<RegionId> = self
            .restrictions
            .keys()
            .filter(|&&(a, _)| a == region)
            .map(|&(_, b)| b)
            .collect();
        self.stalks.remove(&region);
        self.entries.remove(&region);
        self.restrictions
            .retain(|&(a, b), _| a != region && b != region);
        // Wipe baseline entries that referenced the removed region; mirror
        // `refresh_baseline_subset`'s drop-on-missing-endpoint policy.
        self.delta_zero_baseline
            .retain(|&(a, b), _| a != region && b != region);
        neighbours
    }

    /// Direction-insensitive lookup of a region's current neighbours via
    /// the restriction map. Used by the daemon's `sheaf_update_topology`
    /// op to build the affected set (touched ∪ radius-1).
    pub fn neighbours(&self, region: RegionId) -> Vec<RegionId> {
        self.restrictions
            .keys()
            .filter(|&&(a, _)| a == region)
            .map(|&(_, b)| b)
            .collect()
    }

    /// Drop a single restriction edge in both directions (since the cache
    /// stores undirected edges as two map entries). Idempotent — returns
    /// `true` iff an edge was actually removed.
    pub fn drop_restriction(&mut self, a: RegionId, b: RegionId) -> bool {
        let removed = self.restrictions.remove(&(a, b)).is_some()
            | self.restrictions.remove(&(b, a)).is_some();
        let key = (a.min(b), a.max(b));
        self.delta_zero_baseline.remove(&key);
        removed
    }

    /// Mutable access to the attached [`CellComplex`], if any. Used by the
    /// daemon's incremental update handler so the same delta lands in both
    /// the cache's restriction map and the backing complex without two
    /// duplicated mutation surfaces.
    pub fn complex_mut(&mut self) -> Option<&mut CellComplex> {
        self.complex.as_mut()
    }

    /// Bump the generation counter without invalidating anything. The
    /// incremental update op uses this to advertise a new snapshot of the
    /// topology while preserving every cache entry for untouched regions.
    /// `on_change` still increments on its own — this is the analogue for
    /// topology mutations that don't go through `on_change`.
    pub fn bump_generation(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    /// Attach a [`CellComplex`] for δ⁰-driven invalidation. Replaces any
    /// previously attached complex. The cache pushes f32 stalk updates into
    /// the complex via [`Self::set_stalk_value`]; the caller is responsible
    /// for adding nodes + edges to the complex (with restriction maps) ahead
    /// of time so the edge geometry matches the cache's restriction edges.
    pub fn with_complex(mut self, complex: CellComplex) -> Self {
        self.complex = Some(complex);
        self
    }

    /// Mutable equivalent of [`Self::with_complex`] — attach (or replace) the
    /// backing complex on an existing cache. Use when the cache is built
    /// heuristic-only and δ⁰ mode is opted into at a later point (e.g. the
    /// daemon's `sheaf_set_topology` op when the request carries f32 stalk
    /// data).
    pub fn set_complex(&mut self, complex: CellComplex) {
        self.complex = Some(complex);
        self.delta_zero_baseline.clear();
    }

    /// Borrow the attached [`CellComplex`], if any.
    pub fn complex(&self) -> Option<&CellComplex> {
        self.complex.as_ref()
    }

    /// Push the current f32 stalk for `region` into the attached complex so
    /// the next `on_change` sees the updated section. No-op if no complex is
    /// attached or `region` has not been added to the complex yet.
    pub fn set_stalk_value(&mut self, region: RegionId, data: Vec<f32>) {
        if let Some(cx) = self.complex.as_mut()
            && cx.cells.contains_key(&region)
        {
            cx.set_node_stalk(region, data);
        }
    }

    /// Register a region with its current stalk.
    pub fn set_stalk(&mut self, region: RegionId, stalk: S) {
        self.stalks.insert(region, stalk);
    }

    /// Add or update a restriction edge between two regions.
    pub fn set_restriction(&mut self, a: RegionId, b: RegionId, edge: RestrictionEdge) {
        // Store both directions for undirected lookup
        self.restrictions.insert((a, b), edge.clone());
        self.restrictions.insert((b, a), edge);
    }

    /// Cache a value for a region.
    pub fn put(&mut self, region: RegionId, value: V) {
        self.entries.insert(
            region,
            CacheEntry {
                value,
                generation: self.generation,
                valid: true,
            },
        );
    }

    /// Get a cached value if it exists and is valid.
    pub fn get(&self, region: &RegionId) -> Option<&V> {
        self.entries
            .get(region)
            .filter(|e| e.valid)
            .map(|e| &e.value)
    }

    /// Handle a change to one or more regions. Propagates invalidation
    /// breadth-first through restriction edges.
    ///
    /// Termination:
    /// - **δ⁰ mode** (complex attached): the BFS runs to the fixed point of
    ///   the per-edge convergence gate — an edge that moved keeps the
    ///   cascade going; an edge at its baseline stops it. This IS the
    ///   mathematically correct criterion; no depth cap applies
    ///   (bead `ley-line-open-4eef8d`). Termination is guaranteed by the
    ///   `visited` set — worst case O(V+E). A configurable safety-valve
    ///   budget ([`Self::set_cascade_budget`], default unbounded) can
    ///   truncate a runaway cascade; truncations are counted in
    ///   [`Self::cascade_truncations`] because a truncated answer is
    ///   incomplete.
    /// - **Heuristic mode** (no complex): bounded by
    ///   [`HEURISTIC_CASCADE_DEPTH`] hops. The XOR gate over-fires on
    ///   projected-away content changes, so the depth bound trades
    ///   completeness for bounded work. A heuristic, not a sheaf invariant.
    ///
    /// Returns a list that always contains the `changed_regions` the caller
    /// passed in (the cascade roots — they appear even when their own
    /// boundary is unchanged, because the caller's assertion that the
    /// region changed is taken as input), plus any BFS-reachable neighbors
    /// whose boundary projection moved beyond `DELTA0_EPS` in norm space
    /// (or whose XOR pre-filter fired, in heuristic-only mode). This is a
    /// structural answer about the sheaf section, not a statement about
    /// the local `entries` map: regions are reported even when the
    /// in-process cache has no entry for them. UDS / MCP consumers own
    /// their own caches and need the full cascade list to evict on their
    /// side; the local `entries.valid = false` side-effect still happens
    /// for in-process callers that DO have entries.
    pub fn on_change(&mut self, changed_regions: &[RegionId]) -> Vec<RegionId> {
        self.generation += 1;
        let mut invalidated = Vec::new();

        for &region in changed_regions {
            if let Some(entry) = self.entries.get_mut(&region) {
                entry.valid = false;
            }
            invalidated.push(region);
        }

        let delta_zero_mode = self.complex.is_some();
        // VecDeque + pop_front gives genuine BFS; the prior Vec::pop produced
        // DFS, which still respects the depth bound but visits nodes in a
        // hash-seed-dependent order.
        let mut frontier: VecDeque<(RegionId, u32)> =
            changed_regions.iter().map(|&r| (r, 0)).collect();
        let mut visited: BTreeSet<RegionId> = changed_regions.iter().copied().collect();
        let mut expansions: usize = 0;

        while let Some((region, depth)) = frontier.pop_front() {
            if delta_zero_mode {
                // δ⁰ mode: no depth cap — run to the per-edge gate's fixed
                // point. The safety-valve budget is the only bound, and
                // hitting it with pending work is an incomplete answer:
                // record it so the truncation is observable, never silent.
                if expansions >= self.cascade_budget {
                    self.cascade_truncations.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                expansions += 1;
            } else if depth >= HEURISTIC_CASCADE_DEPTH {
                continue;
            }

            let neighbors: Vec<RegionId> = self
                .restrictions
                .keys()
                .filter(|(a, _)| *a == region)
                .map(|(_, b)| *b)
                .collect();

            for neighbor in neighbors {
                if !visited.insert(neighbor) {
                    continue;
                }
                let edge_key = (region, neighbor);
                if let Some(edge) = self.restrictions.get(&edge_key)
                    && self.check_boundary_changed(region, neighbor, edge)
                {
                    if let Some(entry) = self.entries.get_mut(&neighbor) {
                        entry.valid = false;
                    }
                    invalidated.push(neighbor);
                    frontier.push_back((neighbor, depth + 1));
                }
            }
        }

        invalidated
    }

    /// Two-stage boundary-change check.
    ///
    /// Stage 1 is the XOR-of-Merkle-roots pre-filter: if `H(stalk_a) ⊕ H(stalk_b)`
    /// still matches `edge.boundary_hash` the cache short-circuits and reports
    /// "unchanged" — no δ⁰ application needed.
    ///
    /// Stage 2 fires only when the XOR pre-filter says "changed" AND a
    /// [`CellComplex`] is attached. It compares the current
    /// [`CellComplex::edge_violation_squared`] against the baseline snapshot
    /// captured by [`Self::refresh_baseline`]. The edge is "changed" iff the
    /// norm moved by more than `DELTA0_EPS` away from the baseline —
    /// `|√current − √baseline| > DELTA0_EPS` — i.e. the agreement subspace
    /// projection of the section actually shifted, not just that the
    /// absolute norm is non-zero. The comparison is in norm space so the
    /// tolerance is scale-uniform (see `DELTA0_EPS`). Content changes the
    /// restriction map projects away leave the norm at its baseline value,
    /// so the cache holds.
    ///
    /// Without an attached complex, stage 2 is skipped and the XOR pre-filter
    /// IS the answer (preserving prior heuristic behaviour for callers that
    /// have not opted into δ⁰-driven mode). Without a baseline (caller never
    /// called `refresh_baseline`), the check falls back to the prior
    /// behaviour — "current norm exceeds eps" — which over-evicts on
    /// initially-non-consistent sections.
    fn check_boundary_changed(&self, a: RegionId, b: RegionId, edge: &RestrictionEdge) -> bool {
        let hash_a = self.stalks.get(&a).map(|s| s.merkle_root());
        let hash_b = self.stalks.get(&b).map(|s| s.merkle_root());

        let xor_changed = match (hash_a, hash_b) {
            (Some(ha), Some(hb)) => {
                let mut boundary = [0u8; 32];
                for i in 0..32 {
                    boundary[i] = ha[i] ^ hb[i];
                }
                boundary != edge.boundary_hash
            }
            _ => true,
        };
        if !xor_changed {
            return false;
        }

        if let Some(cx) = self.complex.as_ref() {
            let current = cx
                .edge_violation_squared(a, b)
                .or_else(|| cx.edge_violation_squared(b, a));
            if let Some(current_norm_sq) = current {
                let key = (a.min(b), a.max(b));
                if let Some(&baseline) = self.delta_zero_baseline.get(&key) {
                    return (current_norm_sq.sqrt() - baseline.sqrt()).abs() > DELTA0_EPS;
                }
                return current_norm_sq.sqrt() > DELTA0_EPS;
            }
        }
        true
    }

    /// Compute the total defect: sum of boundary disagreements across all
    /// restriction edges. Zero means the entire cache is globally consistent.
    pub fn defect(&self) -> f64 {
        let mut total = 0.0;
        // Only count each edge once (a < b)
        for (&(a, b), edge) in &self.restrictions {
            if a >= b {
                continue;
            }
            let ha = self.stalks.get(&a).map(|s| s.merkle_root());
            let hb = self.stalks.get(&b).map(|s| s.merkle_root());

            if let (Some(ha), Some(hb)) = (ha, hb) {
                let mut boundary = [0u8; 32];
                for i in 0..32 {
                    boundary[i] = ha[i] ^ hb[i];
                }
                if boundary != edge.boundary_hash {
                    // Weight by co-change rate (tighter coupling = larger defect)
                    total += 1.0 - edge.co_change_rate;
                }
            }
        }
        total
    }

    /// Derive restriction weights from co-change history.
    ///
    /// For each dimension d:
    ///   variance(d) = mean(stalk[*][d]²) - mean(stalk[*][d])²
    ///   weight[d] = 1.0 / (1.0 + variance(d) * scale)
    ///
    /// High-variance dimensions get low weights (they naturally differ).
    /// Low-variance dimensions get high weights (should agree).
    pub fn derive_restriction_weights(stalk_values: &[Vec<f64>], scale: f64) -> Vec<f64> {
        if stalk_values.is_empty() {
            return Vec::new();
        }
        let ndims = stalk_values[0].len();
        let n = stalk_values.len() as f64;

        let mut weights = vec![0.0; ndims];
        for d in 0..ndims {
            let mean = stalk_values.iter().map(|s| s[d]).sum::<f64>() / n;
            let mean_sq = stalk_values.iter().map(|s| s[d] * s[d]).sum::<f64>() / n;
            let variance = mean_sq - mean * mean;
            weights[d] = 1.0 / (1.0 + variance * scale);
        }
        weights
    }

    /// Number of valid (non-invalidated) cache entries.
    pub fn valid_count(&self) -> usize {
        self.entries.values().filter(|e| e.valid).count()
    }

    /// Number of total cache entries (valid + invalid).
    pub fn total_count(&self) -> usize {
        self.entries.len()
    }

    /// Current generation counter.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Iterate over restriction edges: ((a, b), edge).
    pub fn restriction_edges(
        &self,
    ) -> impl Iterator<Item = (&(RegionId, RegionId), &RestrictionEdge)> {
        self.restrictions.iter()
    }

    /// Identify regions whose cached values are eligible for eviction —
    /// the GC half of the sheaf-as-cache-coherence story (bead
    /// `ley-line-open-9c867f`).
    ///
    /// Unlike [`Self::on_change`] which acts on caller-asserted changes,
    /// `reap` is a pure observation of the current sheaf section: it
    /// asks "given today's stalks vs the last baseline, which regions'
    /// boundary signal has moved?" and returns that set plus its
    /// gate-terminated BFS expansion through the restriction graph
    /// (same fixed-point policy as `on_change` in δ⁰ mode).
    ///
    /// Returns `(reclaimable, defect_snapshot)`:
    /// - `reclaimable` — sorted list of region IDs whose downstream
    ///   cached values the consumer should evict. Payload-blind: this
    ///   crate never inspects the cached `V` values, only the
    ///   structural signal.
    /// - `defect_snapshot` — `Σ‖δ⁰‖²` evaluated against the current
    ///   section at the moment of reap, for diagnostics. NaN if no
    ///   `CellComplex` is attached (the heuristic-only cache can't
    ///   compute the real metric).
    ///
    /// The reaper is `&self`. Generation bumping is the handler's job
    /// — consumers may want to call reap multiple times during a long
    /// query without each call advancing the generation cursor.
    ///
    /// No-op when no `CellComplex` is attached: heuristic-only mode
    /// has no δ⁰ to measure, so we return `(empty, NaN)` rather than
    /// guess from the XOR pre-filter (which is a CHANGE detector, not
    /// a STALE detector — reap should only return things the consumer
    /// can safely evict).
    pub fn reap(&self) -> (Vec<RegionId>, f32) {
        let Some(cx) = self.complex.as_ref() else {
            return (Vec::new(), f32::NAN);
        };

        // Phase 1: seed set — every region incident to an edge whose
        // current ‖δ⁰‖² has moved away from baseline by more than the
        // tolerance. Iterating unordered pairs (a < b) once per edge.
        let mut seeds: BTreeSet<RegionId> = BTreeSet::new();
        let mut total_defect: f32 = 0.0;
        for &(a, b) in self.restrictions.keys() {
            if a >= b {
                continue;
            }
            let current = cx
                .edge_violation_squared(a, b)
                .or_else(|| cx.edge_violation_squared(b, a));
            if let Some(c) = current {
                total_defect += c;
                let key = (a, b);
                let baseline = self.delta_zero_baseline.get(&key).copied().unwrap_or(0.0);
                if (c.sqrt() - baseline.sqrt()).abs() > DELTA0_EPS {
                    seeds.insert(a);
                    seeds.insert(b);
                }
            }
        }

        // Phase 2: BFS expansion. Same termination policy as `on_change`
        // in δ⁰ mode (reap only runs in δ⁰ mode): the per-edge gate's
        // fixed point, no depth cap, shared safety-valve budget — so the
        // two stay consistent. A region the cascade would evict on
        // assertion is also a region the reaper would evict on
        // observation, given matching topology + stalks (bead
        // `ley-line-open-4eef8d`).
        let mut reclaim = seeds.clone();
        let mut frontier: VecDeque<RegionId> = seeds.iter().copied().collect();
        let mut visited: BTreeSet<RegionId> = seeds.clone();
        let mut expansions: usize = 0;

        while let Some(region) = frontier.pop_front() {
            if expansions >= self.cascade_budget {
                // Non-empty frontier at truncation: incomplete answer,
                // recorded so the budget's bite is observable.
                self.cascade_truncations.fetch_add(1, Ordering::Relaxed);
                break;
            }
            expansions += 1;
            for n in self.neighbours(region) {
                if !visited.insert(n) {
                    continue;
                }
                // Same per-edge check as the seed phase — only expand
                // to neighbours whose own boundary also moved. Without
                // this gate, BFS would walk the entire graph from any
                // single seed.
                let current = cx
                    .edge_violation_squared(region, n)
                    .or_else(|| cx.edge_violation_squared(n, region));
                if let Some(c) = current {
                    let key = (region.min(n), region.max(n));
                    let baseline = self.delta_zero_baseline.get(&key).copied().unwrap_or(0.0);
                    if (c.sqrt() - baseline.sqrt()).abs() > DELTA0_EPS {
                        reclaim.insert(n);
                        frontier.push_back(n);
                    }
                }
            }
        }

        (reclaim.into_iter().collect(), total_defect)
    }
}

impl<S: StalkHash, V> Default for SheafCache<S, V> {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Simple stalk that wraps a hash directly.
    #[derive(Clone)]
    struct TestStalk([u8; 32]);

    impl StalkHash for TestStalk {
        fn merkle_root(&self) -> [u8; 32] {
            self.0
        }
    }

    fn hash_from_byte(b: u8) -> [u8; 32] {
        let mut h = [0u8; 32];
        h[0] = b;
        h
    }

    #[test]
    fn put_and_get() {
        let mut cache: SheafCache<TestStalk, String> = SheafCache::new();
        cache.set_stalk(0, TestStalk(hash_from_byte(1)));
        cache.put(0, "hello".into());

        assert_eq!(cache.get(&0), Some(&"hello".to_string()));
        assert_eq!(cache.get(&1), None);
    }

    #[test]
    fn on_change_invalidates_changed_region() {
        let mut cache: SheafCache<TestStalk, String> = SheafCache::new();
        cache.set_stalk(0, TestStalk(hash_from_byte(1)));
        cache.put(0, "value".into());

        let invalidated = cache.on_change(&[0]);
        assert!(invalidated.contains(&0));
        assert_eq!(cache.get(&0), None); // invalid now
    }

    #[test]
    fn on_change_cascades_through_restrictions() {
        let mut cache: SheafCache<TestStalk, String> = SheafCache::new();
        cache.set_stalk(0, TestStalk(hash_from_byte(1)));
        cache.set_stalk(1, TestStalk(hash_from_byte(2)));
        cache.set_stalk(2, TestStalk(hash_from_byte(3)));

        // 0 -- 1 -- 2 (linear chain)
        let edge_01 = RestrictionEdge {
            weights: vec![1.0],
            boundary_hash: hash_from_byte(1 ^ 2), // XOR of stalks
            co_change_rate: 0.5,
            revert_rate: 0.0,
        };
        let edge_12 = RestrictionEdge {
            weights: vec![1.0],
            boundary_hash: hash_from_byte(2 ^ 3),
            co_change_rate: 0.5,
            revert_rate: 0.0,
        };
        cache.set_restriction(0, 1, edge_01);
        cache.set_restriction(1, 2, edge_12);

        cache.put(0, "a".into());
        cache.put(1, "b".into());
        cache.put(2, "c".into());

        // Change region 0's stalk (boundary hash no longer matches)
        cache.set_stalk(0, TestStalk(hash_from_byte(99)));
        let invalidated = cache.on_change(&[0]);

        // Region 0 directly invalidated
        assert!(invalidated.contains(&0));
        // Region 1 invalidated via cascade (boundary 0-1 changed)
        assert!(invalidated.contains(&1));
    }

    #[test]
    fn derive_weights_from_variance() {
        let stalks = vec![vec![1.0, 10.0], vec![1.0, 20.0], vec![1.0, 30.0]];
        let weights = SheafCache::<TestStalk, ()>::derive_restriction_weights(&stalks, 10.0);

        // Dimension 0: zero variance → weight = 1.0
        assert!((weights[0] - 1.0).abs() < 1e-6);
        // Dimension 1: high variance → weight << 1.0
        assert!(weights[1] < 0.1);
    }

    // -----------------------------------------------------------------------
    // δ⁰ stage-2 tolerance shape (bead ley-line-open-4f3f6e)
    // -----------------------------------------------------------------------

    /// Build a two-region δ⁰-mode cache with 1-D stalks `[0.0]` / `[1.0]`,
    /// identity restrictions, and a refreshed baseline (‖δ⁰‖² = 1.0 on the
    /// single edge).
    fn order_one_baseline_cache() -> SheafCache<TestStalk, String> {
        use crate::complex::{CellComplex, RestrictionMap};

        let mut cx = CellComplex::new(1);
        cx.add_node(0, vec![0.0]);
        cx.add_node(1, vec![1.0]);
        cx.add_edge(
            100,
            0,
            1,
            1,
            Some("t".into()),
            RestrictionMap::identity(1),
            RestrictionMap::identity(1),
            false,
        );

        let mut cache: SheafCache<TestStalk, String> = SheafCache::new().with_complex(cx);
        cache.set_stalk(0, TestStalk(hash_from_byte(1)));
        cache.set_stalk(1, TestStalk(hash_from_byte(2)));
        cache.set_restriction(
            0,
            1,
            RestrictionEdge {
                weights: vec![1.0],
                boundary_hash: hash_from_byte(1 ^ 2),
                co_change_rate: 0.5,
                revert_rate: 0.0,
            },
        );
        cache.put(0, "a".into());
        cache.put(1, "b".into());
        cache.refresh_baseline();
        cache
    }

    /// P4 falsification (bead ley-line-open-4f3f6e): the stage-2 tolerance
    /// must be scale-uniform in NORM space. |a² − b²| = |a − b|·(a + b), so
    /// comparing the squared norms against a squared epsilon makes the
    /// effective norm sensitivity ε²/(2·baseline) — at an O(1) baseline
    /// that is below the f32 ulp and EVERY representable wiggle fires,
    /// i.e. no noise rejection at all.
    ///
    /// Here: Δ‖δ⁰‖ = 1e-5, well under the norm-space EPS = 1e-4, so the
    /// edge must be treated as unchanged. The old squared check saw
    /// Δ(‖δ⁰‖²) ≈ 2e-5 > 1e-8 and invalidated.
    #[test]
    fn delta0_noise_at_order_one_baseline_does_not_invalidate() {
        let mut cache = order_one_baseline_cache();

        // Norm-space noise: 1.0 → 1.00001 (Δnorm = 1e-5 < 1e-4).
        cache.set_stalk_value(1, vec![1.00001]);
        // Content hash moved, so the XOR pre-filter fires and stage 2
        // is the deciding check.
        cache.set_stalk(1, TestStalk(hash_from_byte(99)));

        let invalidated = cache.on_change(&[1]);
        assert!(
            !invalidated.contains(&0),
            "sub-EPS norm movement (1e-5 < 1e-4) must not cascade; got {invalidated:?}",
        );
        assert!(
            cache.get(&0).is_some(),
            "region 0's entry must survive a sub-tolerance wiggle on the shared edge",
        );
    }

    /// Companion sanity: a genuine shift (Δ‖δ⁰‖ = 1e-2 > EPS) at the same
    /// O(1) baseline still cascades. Holds before and after the shape fix.
    #[test]
    fn delta0_genuine_shift_at_order_one_baseline_invalidates() {
        let mut cache = order_one_baseline_cache();

        cache.set_stalk_value(1, vec![1.01]);
        cache.set_stalk(1, TestStalk(hash_from_byte(99)));

        let invalidated = cache.on_change(&[1]);
        assert!(
            invalidated.contains(&0),
            "super-EPS norm movement (1e-2 > 1e-4) must cascade; got {invalidated:?}",
        );
        assert!(cache.get(&0).is_none());
    }

    // -----------------------------------------------------------------------
    // δ⁰-mode cascade fixed point (bead ley-line-open-4eef8d)
    // -----------------------------------------------------------------------

    /// Build an n-node chain 0–1–…–(n−1) in δ⁰ mode: 1-D stalks all `[0.0]`,
    /// identity restrictions, every region cached, baseline refreshed.
    fn delta_zero_chain_cache(n: u32) -> SheafCache<TestStalk, String> {
        use crate::complex::{CellComplex, RestrictionMap};

        let mut cx = CellComplex::new(1);
        for i in 0..n {
            cx.add_node(i, vec![0.0]);
        }
        for i in 0..(n - 1) {
            cx.add_edge(
                1_000_000 + i,
                i,
                i + 1,
                1,
                Some("t".into()),
                RestrictionMap::identity(1),
                RestrictionMap::identity(1),
                false,
            );
        }

        let mut cache: SheafCache<TestStalk, String> = SheafCache::new().with_complex(cx);
        for i in 0..n {
            cache.set_stalk(i, TestStalk(hash_from_byte(i as u8)));
            cache.put(i, format!("v{i}"));
        }
        for i in 0..(n - 1) {
            let mut boundary = [0u8; 32];
            let (ha, hb) = (hash_from_byte(i as u8), hash_from_byte((i + 1) as u8));
            for k in 0..32 {
                boundary[k] = ha[k] ^ hb[k];
            }
            cache.set_restriction(
                i,
                i + 1,
                RestrictionEdge {
                    weights: vec![1.0],
                    boundary_hash: boundary,
                    co_change_rate: 0.5,
                    revert_rate: 0.0,
                },
            );
        }
        cache.refresh_baseline();
        cache
    }

    /// Move every stalk in the chain: fresh f32 values (all edge
    /// baselines shift far beyond tolerance) and fresh content hashes
    /// (the XOR pre-filter fires on every edge). Hash bytes are `i² + 5`
    /// rather than `i + shift`: a common additive shift cancels in the
    /// pairwise XOR (both endpoints move identically), leaving the
    /// pre-filter blind; the quadratic spacing makes every adjacent XOR
    /// differ from the seeded `i ⊕ (i+1)` boundary.
    fn move_every_stalk(cache: &mut SheafCache<TestStalk, String>, n: u32) {
        for i in 0..n {
            cache.set_stalk_value(i, vec![(i as f32 + 1.0) * 10.0]);
            cache.set_stalk(i, TestStalk(hash_from_byte((i * i + 5) as u8)));
        }
    }

    /// F3 falsification (bead ley-line-open-4eef8d): in δ⁰ mode the
    /// per-edge convergence gate IS the termination criterion; truncating
    /// the BFS at a fixed depth under-invalidates whenever a genuinely
    /// moved defect chain sits more than that many hops from the cascade
    /// root. Chain 0–…–6, every edge's agreement provably moved,
    /// `on_change(&[0])`: sheaf semantics demands invalidation of all of
    /// 1..=6. A depth-3 cap stops at region 3 and then SERVES REGION 5'S
    /// STALE ENTRY AS VALID.
    #[test]
    fn delta_zero_cascade_runs_to_fixed_point_beyond_depth_three() {
        let mut cache = delta_zero_chain_cache(7);
        move_every_stalk(&mut cache, 7);

        let invalidated = cache.on_change(&[0]);
        for r in 1..7u32 {
            assert!(
                invalidated.contains(&r),
                "δ⁰-mode cascade must reach the per-edge gate's fixed point; \
                 region {r} (graph distance {r} from root) missing from {invalidated:?}",
            );
        }
        assert!(
            cache.get(&5).is_none(),
            "stale-serve smoking gun: region 5's entry is backed by a stalk \
             whose agreement provably moved, yet it is served as valid",
        );
    }

    // -----------------------------------------------------------------------
    // Integration: RestrictionGraph drives SheafCache invalidation
    // -----------------------------------------------------------------------

    /// Build a RestrictionGraph, populate SheafCache from it, verify
    /// on_change cascade follows the graph topology.
    #[test]
    fn topology_drives_cache_invalidation() {
        use crate::topology::RestrictionGraph;

        // Build topology: diamond graph  0 -- 1 -- 3
        //                                 \- 2 -/
        let mut graph = RestrictionGraph::new();
        graph.add_edge(0, 1, Some("shared_token".into()));
        graph.add_edge(0, 2, Some("shared_token".into()));
        graph.add_edge(1, 3, Some("shared_token".into()));
        graph.add_edge(2, 3, Some("shared_token".into()));

        // Populate cache from graph topology
        let mut cache: SheafCache<TestStalk, String> = SheafCache::new();
        for &region in graph.regions() {
            cache.set_stalk(region, TestStalk(hash_from_byte(region as u8)));
            cache.put(region, format!("value_{region}"));
        }

        // Add restrictions matching graph edges
        for edge in graph.edges() {
            let boundary = hash_from_byte(edge.source as u8 ^ edge.target as u8);
            cache.set_restriction(
                edge.source,
                edge.target,
                RestrictionEdge {
                    weights: vec![1.0],
                    boundary_hash: boundary,
                    co_change_rate: 0.5,
                    revert_rate: 0.0,
                },
            );
        }

        assert_eq!(cache.valid_count(), 4);

        // Change region 0's stalk → should cascade to neighbors 1 and 2
        cache.set_stalk(0, TestStalk(hash_from_byte(0xFF)));
        let invalidated = cache.on_change(&[0]);

        // Region 0 directly invalidated
        assert!(invalidated.contains(&0));
        // Regions 1 and 2 are direct neighbors — boundary hash changed
        assert!(invalidated.contains(&1));
        assert!(invalidated.contains(&2));
        // Region 3 may or may not cascade (depends on depth limit and
        // whether 1/2's stalks changed). At minimum 0,1,2 are invalidated.
        assert!(invalidated.len() >= 3);
    }

    /// Heuristic-mode chain, single root moved: the XOR gate is the
    /// PRIMARY limiter, not the depth cap. Only region 0's stalk changed,
    /// so edge (0,1) fires but (1,2) does not — the cascade stops at
    /// region 1 well before any depth bound is consulted.
    ///
    /// This replaces `cache_cascade_depth_matches_graph_bfs`, whose intent
    /// (depth-3 truncation as a correctness property) was wrong: in δ⁰
    /// mode the correct termination is the per-edge gate's fixed point
    /// (see `delta_zero_cascade_runs_to_fixed_point_beyond_depth_three`),
    /// and even in heuristic mode the original fixture never exercised
    /// the cap — the gate stopped the walk at depth 1.
    #[test]
    fn heuristic_cascade_gate_self_limits_on_chain() {
        // Linear chain: 0 -- 1 -- 2 -- 3 -- 4, heuristic mode (no complex).
        let mut cache: SheafCache<TestStalk, String> = SheafCache::new();
        for i in 0..5u32 {
            cache.set_stalk(i, TestStalk(hash_from_byte(i as u8)));
            cache.put(i, format!("v{i}"));
        }
        for i in 0..4u32 {
            let boundary = hash_from_byte(i as u8 ^ (i + 1) as u8);
            cache.set_restriction(
                i,
                i + 1,
                RestrictionEdge {
                    weights: vec![1.0],
                    boundary_hash: boundary,
                    co_change_rate: 0.5,
                    revert_rate: 0.0,
                },
            );
        }

        cache.set_stalk(0, TestStalk(hash_from_byte(0xFF)));
        let invalidated = cache.on_change(&[0]);
        assert!(invalidated.contains(&0));
        assert!(
            invalidated.contains(&1),
            "edge (0,1)'s boundary moved — region 1 must be invalidated",
        );
        assert!(
            !invalidated.contains(&2),
            "edge (1,2)'s boundary is unchanged — the gate must stop the \
             cascade at region 1; got {invalidated:?}",
        );
    }

    /// Heuristic-mode blast-radius bound: when EVERY edge's boundary
    /// moved (worst case for the XOR gate), the hardcoded
    /// `HEURISTIC_CASCADE_DEPTH = 3` bounds the walk. This is a
    /// bounded-work heuristic, not a sheaf invariant — the same scenario
    /// in δ⁰ mode runs to the gate's fixed point instead.
    #[test]
    fn heuristic_cascade_depth_bounds_blast_radius() {
        use crate::topology::RestrictionGraph;

        // Linear chain 0 -- … -- 6, heuristic mode.
        let mut graph = RestrictionGraph::new();
        for i in 0..6u32 {
            graph.add_edge(i, i + 1, None);
        }

        let mut cache: SheafCache<TestStalk, String> = SheafCache::new();
        for i in 0..7u32 {
            cache.set_stalk(i, TestStalk(hash_from_byte(i as u8)));
            cache.put(i, format!("v{i}"));
        }
        for edge in graph.edges() {
            let boundary = hash_from_byte(edge.source as u8 ^ edge.target as u8);
            cache.set_restriction(
                edge.source,
                edge.target,
                RestrictionEdge {
                    weights: vec![1.0],
                    boundary_hash: boundary,
                    co_change_rate: 0.5,
                    revert_rate: 0.0,
                },
            );
        }

        // Move every stalk hash (quadratic spacing so no pairwise XOR
        // accidentally matches its seeded boundary) — every edge fires.
        for i in 0..7u32 {
            cache.set_stalk(i, TestStalk(hash_from_byte((i * i + 5) as u8)));
        }

        let invalidated = cache.on_change(&[0]);
        let graph_reach = graph.bfs(0, 3);
        for r in 0..7u32 {
            assert_eq!(
                invalidated.contains(&r),
                graph_reach.contains(&r),
                "heuristic cascade must match a depth-3 graph BFS when \
                 every edge fires; disagreement at region {r} \
                 (cache: {invalidated:?}, graph: {graph_reach:?})",
            );
        }
    }

    /// The δ⁰-mode safety-valve budget must be INSTRUMENTED: truncating
    /// with a non-empty frontier is an incomplete answer, and
    /// `cascade_truncations` is the falsifiable signal that the budget
    /// ever bound. Silent truncation is exactly the bug class the
    /// fixed-point fix removed.
    #[test]
    fn cascade_budget_truncation_is_counted() {
        let mut cache = delta_zero_chain_cache(7);
        cache.set_cascade_budget(2);
        move_every_stalk(&mut cache, 7);

        assert_eq!(cache.cascade_truncations(), 0);
        let invalidated = cache.on_change(&[0]);
        assert!(
            cache.cascade_truncations() >= 1,
            "budget-bound cascade with pending frontier must be counted",
        );
        assert!(
            !invalidated.contains(&6),
            "budget 2 cannot have reached region 6 — if it did, the \
             truncation accounting is measuring the wrong thing",
        );

        // Unbounded default: same scenario reaches the fixed point and
        // records no truncation.
        let mut cache = delta_zero_chain_cache(7);
        move_every_stalk(&mut cache, 7);
        let invalidated = cache.on_change(&[0]);
        assert!(invalidated.contains(&6));
        assert_eq!(cache.cascade_truncations(), 0);
    }
}
