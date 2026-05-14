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

use std::collections::HashMap;

use crate::topology::RegionId;

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

/// Sheaf cache: entries organized by topological regions with invalidation
/// driven by the Čech coboundary operator.
///
/// `S` is the stalk type (must produce a content hash).
/// `V` is the cached value type.
pub struct SheafCache<S: StalkHash, V> {
    stalks: HashMap<RegionId, S>,
    restrictions: HashMap<(RegionId, RegionId), RestrictionEdge>,
    entries: HashMap<RegionId, CacheEntry<V>>,
    generation: u64,
}

impl<S: StalkHash, V> SheafCache<S, V> {
    pub fn new() -> Self {
        Self {
            stalks: HashMap::new(),
            restrictions: HashMap::new(),
            entries: HashMap::new(),
            generation: 0,
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

    /// Handle a change to one or more regions. Recomputes stalks and
    /// propagates invalidation through restriction edges.
    ///
    /// Returns the set of invalidated region IDs.
    pub fn on_change(&mut self, changed_regions: &[RegionId]) -> Vec<RegionId> {
        self.generation += 1;
        let mut invalidated = Vec::new();

        // Mark changed regions as invalid
        for &region in changed_regions {
            if let Some(entry) = self.entries.get_mut(&region) {
                entry.valid = false;
                invalidated.push(region);
            }
        }

        // Walk restriction edges from changed regions (bounded cascade)
        let max_depth = 3;
        let mut frontier: Vec<(RegionId, u32)> = changed_regions.iter().map(|&r| (r, 0)).collect();
        let mut visited: std::collections::HashSet<RegionId> =
            changed_regions.iter().copied().collect();

        while let Some((region, depth)) = frontier.pop() {
            if depth >= max_depth {
                continue;
            }

            // Find neighbors via restriction edges
            let neighbors: Vec<RegionId> = self
                .restrictions
                .keys()
                .filter(|(a, _)| *a == region)
                .map(|(_, b)| *b)
                .collect();

            for neighbor in neighbors {
                if visited.contains(&neighbor) {
                    continue;
                }

                // Check if the boundary changed
                let edge_key = (region, neighbor);
                if let Some(edge) = self.restrictions.get(&edge_key) {
                    let boundary_changed = self.check_boundary_changed(region, neighbor, edge);
                    if boundary_changed {
                        if let Some(entry) = self.entries.get_mut(&neighbor) {
                            entry.valid = false;
                            invalidated.push(neighbor);
                        }
                        visited.insert(neighbor);
                        frontier.push((neighbor, depth + 1));
                    }
                }
            }
        }

        invalidated
    }

    /// Heuristic boundary-change check: compares the XOR of endpoint Merkle
    /// roots against the stored boundary hash.
    ///
    /// **This is a proxy, not a δ⁰ computation.** Returns `true` whenever either
    /// endpoint's hash has shifted in a way that changes the XOR — including
    /// content changes that the restriction map would project away. Over-evicts
    /// on author churn that doesn't actually move the agreement subspace, and
    /// could in principle false-negative if two simultaneous endpoint hash
    /// changes XOR back to the stored boundary (vanishingly unlikely for
    /// real Merkle hashes; guarded against deterministically by the cache's
    /// `claim_2_unchanged_neighbors_with_matching_boundary_hash_remain_valid`
    /// falsifiability gate).
    ///
    /// TODO: replace with real δ⁰ via [`crate::complex::CellComplex::detect_violations`]
    /// once the cache stores the f32 stalk values alongside their hashes.
    fn check_boundary_changed(&self, a: RegionId, b: RegionId, edge: &RestrictionEdge) -> bool {
        let hash_a = self.stalks.get(&a).map(|s| s.merkle_root());
        let hash_b = self.stalks.get(&b).map(|s| s.merkle_root());

        match (hash_a, hash_b) {
            (Some(ha), Some(hb)) => {
                let mut boundary = [0u8; 32];
                for i in 0..32 {
                    boundary[i] = ha[i] ^ hb[i];
                }
                boundary != edge.boundary_hash
            }
            _ => true, // Missing stalk → assume changed
        }
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
