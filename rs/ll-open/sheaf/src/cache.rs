//! Sheaf cache: structurally-aware cache invalidation, heuristic proxy for δ⁰.
//!
//! ## What this cache actually does
//!
//! Invalidation is driven by **XOR of endpoint Merkle roots** compared against
//! a stored boundary hash, plus a **bounded-depth restriction-graph BFS**. This
//! is a fast structural proxy, NOT the Čech coboundary operator δ⁰. In
//! particular:
//!
//! - The boundary check (see [`SheafCache::check_boundary_changed`]) flags an
//!   edge as "changed" whenever `H(stalk_a) ⊕ H(stalk_b)` differs from the
//!   stored hash. It does **not** apply the restriction map, so it cannot
//!   distinguish content changes that fall outside the agreement subspace
//!   from genuine sheaf disagreements.
//! - The cascade depth is a configurable heuristic budget, not a sheaf-derived
//!   reach. See [`SheafCache`] field documentation.
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
//! hash-comparison BFS cascade; co-change-learned edge weights weight the
//! cascade frontier as a coupling prior. No code path here computes ker(δ⁰)
//! — see "What this cache actually does" above for the proxy details and the
//! daemon-wiring bead for the δ⁰-driven upgrade path.
//!
//! ## `on_change` return semantics
//!
//! [`SheafCache::on_change`] returns the list of regions whose boundary
//! projection moved beyond `DELTA0_EPS_SQUARED` (or whose XOR pre-filter
//! fired, in heuristic-only mode). This is a **structural answer about
//! the sheaf section** — it is NOT "regions to evict from this cache". In
//! particular, regions are reported even when this cache holds no entry
//! for them, because UDS / MCP consumers own their own caches and need
//! the full cascade list to evict on their side.
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

use crate::complex::CellComplex;
use crate::topology::RegionId;

/// Squared-norm threshold below which δ⁰ output is treated as zero.
/// Matches `complex::EPS` for the unsquared coboundary check.
const DELTA0_EPS_SQUARED: f32 = 1e-8;

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
        }
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
    /// breadth-first through restriction edges, bounded by `max_cascade_budget`
    /// (heuristic depth, not a sheaf invariant — see module docs).
    ///
    /// Returns the set of region IDs whose boundary projection moved beyond
    /// `DELTA0_EPS_SQUARED` (or whose XOR pre-filter fired, in heuristic-only
    /// mode). This is a structural answer about the sheaf section, not a
    /// statement about the local `entries` map: regions are reported even
    /// when the in-process cache has no entry for them. UDS / MCP consumers
    /// own their own caches and need the full cascade list to evict on
    /// their side; the local `entries.valid = false` side-effect still
    /// happens for in-process callers that DO have entries.
    pub fn on_change(&mut self, changed_regions: &[RegionId]) -> Vec<RegionId> {
        self.generation += 1;
        let mut invalidated = Vec::new();

        for &region in changed_regions {
            if let Some(entry) = self.entries.get_mut(&region) {
                entry.valid = false;
            }
            invalidated.push(region);
        }

        let max_cascade_budget: u32 = 3;
        // VecDeque + pop_front gives genuine BFS; the prior Vec::pop produced
        // DFS, which still respects the depth bound but visits nodes in a
        // hash-seed-dependent order.
        let mut frontier: VecDeque<(RegionId, u32)> =
            changed_regions.iter().map(|&r| (r, 0)).collect();
        let mut visited: BTreeSet<RegionId> = changed_regions.iter().copied().collect();

        while let Some((region, depth)) = frontier.pop_front() {
            if depth >= max_cascade_budget {
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
    /// squared norm moved by more than `DELTA0_EPS_SQUARED` away from the
    /// baseline — i.e. the agreement subspace projection of the section
    /// actually shifted, not just that the absolute norm is non-zero.
    /// Content changes the restriction map projects away leave the squared
    /// norm at its baseline value, so the cache holds.
    ///
    /// Without an attached complex, stage 2 is skipped and the XOR pre-filter
    /// IS the answer (preserving prior heuristic behaviour for callers that
    /// have not opted into δ⁰-driven mode). Without a baseline (caller never
    /// called `refresh_baseline`), the check falls back to the prior
    /// behaviour — "current squared norm exceeds eps²" — which over-evicts
    /// on initially-non-consistent sections.
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
                    return (current_norm_sq - baseline).abs() > DELTA0_EPS_SQUARED;
                }
                return current_norm_sq > DELTA0_EPS_SQUARED;
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

    /// Verify that SheafCache BFS depth matches RestrictionGraph BFS depth
    /// for the same linear topology.
    #[test]
    fn cache_cascade_depth_matches_graph_bfs() {
        use crate::topology::RestrictionGraph;

        // Linear chain: 0 -- 1 -- 2 -- 3 -- 4
        let mut graph = RestrictionGraph::new();
        for i in 0..4u32 {
            graph.add_edge(i, i + 1, None);
        }

        let mut cache: SheafCache<TestStalk, String> = SheafCache::new();
        for i in 0..5u32 {
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

        // Graph BFS from 0, depth 3: should reach 0,1,2,3 (not 4)
        let bfs_reached = graph.bfs(0, 3);
        assert!(bfs_reached.contains(&0));
        assert!(bfs_reached.contains(&3));
        assert!(!bfs_reached.contains(&4));

        // Cache on_change from 0: bounded cascade depth is 3 (hardcoded),
        // so it should NOT reach region 4 (which is 4 hops away)
        cache.set_stalk(0, TestStalk(hash_from_byte(0xFF)));
        let invalidated = cache.on_change(&[0]);
        assert!(invalidated.contains(&0));
        // Region 4 should NOT be invalidated (beyond cascade depth)
        assert!(
            !invalidated.contains(&4),
            "region 4 should be beyond cascade depth 3"
        );
    }
}
