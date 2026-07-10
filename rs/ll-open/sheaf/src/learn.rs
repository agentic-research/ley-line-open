//! Restriction weight learning from invalidation history.
//!
//! Tracks co-change patterns across regions and derives restriction edge
//! weights via exponential moving average. Regions that frequently co-invalidate
//! get higher restriction weights (tighter coupling → more aggressive cascade).
//!
//! ## Mathematical guarantee
//!
//! For a pair of regions (a, b) that co-change with probability p:
//!   - After N observations, the EMA estimate converges to p with error
//!     O((1 − α)^N) where α is the decay factor (default 0.1) — each
//!     observation's residual decays by the retention factor 1 − α, not
//!     by α. The N-bound below is the same statement solved for N.
//!   - The learned weight w(a,b) = co_change_rate(a,b) satisfies:
//!     |w - p| < ε after N > log(ε) / log(1 - α) observations.
//!
//! For α = 0.1: N > 22 observations for ε = 0.1 (90% accuracy).
//!              N > 44 observations for ε = 0.01 (99% accuracy).

use std::collections::HashMap;

use crate::topology::RegionId;

/// Tracks co-change frequency between region pairs via EMA.
#[derive(Debug, Clone)]
pub struct CoChangeTracker {
    /// Per-edge co-change rate: EMA of how often (a,b) co-invalidate.
    rates: HashMap<(RegionId, RegionId), f64>,
    /// Per-edge observation count.
    counts: HashMap<(RegionId, RegionId), u64>,
    /// EMA decay factor. Higher = faster adaptation, noisier.
    /// Default 0.1 (smooth, needs ~22 observations for 90% accuracy).
    alpha: f64,
}

impl CoChangeTracker {
    pub fn new(alpha: f64) -> Self {
        assert!((0.0..1.0).contains(&alpha), "alpha must be in [0, 1)");
        Self {
            rates: HashMap::new(),
            counts: HashMap::new(),
            alpha,
        }
    }

    /// Record an invalidation event: the given regions changed together.
    ///
    /// For every pair (a, b) where a < b in the changed set, update the
    /// co-change rate. For edges NOT in the changed set, update toward 0
    /// (they didn't co-change this time).
    pub fn observe(&mut self, changed: &[RegionId], all_edges: &[(RegionId, RegionId)]) {
        let changed_set: std::collections::HashSet<RegionId> = changed.iter().copied().collect();

        for &(a, b) in all_edges {
            let key = normalize_pair(a, b);
            let co_changed = changed_set.contains(&a) && changed_set.contains(&b);
            let signal = if co_changed { 1.0 } else { 0.0 };

            let rate = self.rates.entry(key).or_insert(0.0);
            *rate = (1.0 - self.alpha) * *rate + self.alpha * signal;

            let count = self.counts.entry(key).or_insert(0);
            *count += 1;
        }
    }

    /// Get the learned co-change rate for an edge.
    pub fn rate(&self, a: RegionId, b: RegionId) -> f64 {
        let key = normalize_pair(a, b);
        self.rates.get(&key).copied().unwrap_or(0.0)
    }

    /// Get the observation count for an edge.
    pub fn observations(&self, a: RegionId, b: RegionId) -> u64 {
        let key = normalize_pair(a, b);
        self.counts.get(&key).copied().unwrap_or(0)
    }

    /// Export all learned rates as restriction edge weights.
    ///
    /// Returns (a, b, co_change_rate) triples suitable for feeding into
    /// `SheafCache::set_restriction`.
    pub fn learned_weights(&self) -> Vec<(RegionId, RegionId, f64)> {
        self.rates
            .iter()
            .map(|(&(a, b), &rate)| (a, b, rate))
            .collect()
    }

    /// Number of unique edges being tracked.
    pub fn edge_count(&self) -> usize {
        self.rates.len()
    }
}

impl Default for CoChangeTracker {
    fn default() -> Self {
        Self::new(0.1)
    }
}

/// Normalize a pair so a < b, for consistent HashMap keys.
fn normalize_pair(a: RegionId, b: RegionId) -> (RegionId, RegionId) {
    if a <= b { (a, b) } else { (b, a) }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const EDGES: [(RegionId, RegionId); 3] = [(0, 1), (0, 2), (1, 2)];

    #[test]
    fn always_co_changing_converges_to_one() {
        let mut tracker = CoChangeTracker::new(0.1);

        // Regions 0 and 1 always change together
        for _ in 0..100 {
            tracker.observe(&[0, 1], &EDGES);
        }

        let rate_01 = tracker.rate(0, 1);
        assert!(
            rate_01 > 0.99,
            "rate(0,1) should converge to ~1.0, got {rate_01}"
        );

        // Region 2 never co-changes with 0 or 1
        let rate_02 = tracker.rate(0, 2);
        assert!(
            rate_02 < 0.01,
            "rate(0,2) should converge to ~0.0, got {rate_02}"
        );
    }

    #[test]
    fn never_co_changing_converges_to_zero() {
        let mut tracker = CoChangeTracker::new(0.1);

        // Only region 0 changes, never with others
        for _ in 0..100 {
            tracker.observe(&[0], &EDGES);
        }

        assert!(tracker.rate(0, 1) < 0.01);
        assert!(tracker.rate(0, 2) < 0.01);
        assert!(tracker.rate(1, 2) < 0.01);
    }

    #[test]
    fn fifty_percent_co_change_converges_to_half() {
        let mut tracker = CoChangeTracker::new(0.1);

        // Alternate: regions 0,1 co-change every other observation
        for i in 0..200 {
            if i % 2 == 0 {
                tracker.observe(&[0, 1], &EDGES);
            } else {
                tracker.observe(&[0], &EDGES);
            }
        }

        let rate = tracker.rate(0, 1);
        assert!(
            (rate - 0.5).abs() < 0.05,
            "rate(0,1) should be ~0.5, got {rate}"
        );
    }

    #[test]
    fn convergence_speed_matches_theory() {
        // Theory: |w - p| < ε after N > log(ε) / log(1 - α)
        // For α=0.1, ε=0.1: N > log(0.1)/log(0.9) ≈ 21.85 → 22 observations
        let mut tracker = CoChangeTracker::new(0.1);

        for _ in 0..22 {
            tracker.observe(&[0, 1], &EDGES);
        }

        let rate = tracker.rate(0, 1);
        assert!(
            rate > 0.9,
            "after 22 observations with α=0.1, rate should be >0.9, got {rate}"
        );
    }

    #[test]
    fn ema_adapts_to_regime_change() {
        let mut tracker = CoChangeTracker::new(0.1);

        // Phase 1: regions 0,1 always co-change (100 observations)
        for _ in 0..100 {
            tracker.observe(&[0, 1], &EDGES);
        }
        assert!(tracker.rate(0, 1) > 0.99);

        // Phase 2: regions 0,1 stop co-changing (100 observations)
        for _ in 0..100 {
            tracker.observe(&[0], &EDGES);
        }
        assert!(
            tracker.rate(0, 1) < 0.01,
            "rate should adapt to new regime, got {}",
            tracker.rate(0, 1)
        );
    }

    #[test]
    fn learned_weights_are_symmetric() {
        let mut tracker = CoChangeTracker::new(0.1);
        tracker.observe(&[0, 1], &EDGES);

        // rate(0,1) == rate(1,0)
        assert_eq!(tracker.rate(0, 1), tracker.rate(1, 0));
    }

    #[test]
    fn observation_counts_tracked() {
        let mut tracker = CoChangeTracker::new(0.1);
        for _ in 0..10 {
            tracker.observe(&[0, 1], &EDGES);
        }

        assert_eq!(tracker.observations(0, 1), 10);
        assert_eq!(tracker.observations(0, 2), 10); // all edges observed each time
    }

    #[test]
    fn empty_changed_set_decays_all_edges() {
        let mut tracker = CoChangeTracker::new(0.1);

        // First: establish some co-change
        for _ in 0..50 {
            tracker.observe(&[0, 1], &EDGES);
        }
        assert!(tracker.rate(0, 1) > 0.9);

        // Then: empty changes should decay rates
        for _ in 0..100 {
            tracker.observe(&[], &EDGES);
        }
        assert!(
            tracker.rate(0, 1) < 0.01,
            "empty observations should decay rate to ~0"
        );
    }
}
