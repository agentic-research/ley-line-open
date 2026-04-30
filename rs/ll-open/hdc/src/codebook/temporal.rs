//! Temporal-layer codebook: time-decayed co-edit matrix → simhash.
//!
//! Per math-friend review E. The right primitive for "functions that
//! change together" isn't raw commit-set membership (which churns badly
//! every time a new commit lands) — it's a time-decayed co-edit matrix
//! simhashed per row.
//!
//! Algorithm:
//!
//!   For each pair (i, j) of scopes, weight w_ij is:
//!       w_ij = Σ_{commits c containing i, j} exp(-(t_now − t_c) / τ)
//!
//!   τ defaults to 90 days. Older commits decay exponentially; recent
//!   commits dominate the weight.
//!
//!   The temporal "embedding" for scope i is the row W[i, :] — a
//!   real-valued vector indexed by every other scope.
//!
//!   Project this row via Charikar simhash (same machinery as
//!   `SemanticCodebook`) to produce the temporal hypervector for i.
//!
//! Properties:
//! - Append-only: a new commit nudges weights for affected pairs;
//!   ~1% bit churn per new commit (Charikar stability).
//! - Reproducible across machines given the same git history + same
//!   seed: all daemon instances produce identical temporal HVs.
//! - Sparse: most scope-pairs never co-edit. The matrix is stored
//!   as a HashMap; projection iterates only non-zero entries.

use std::collections::HashMap;

use crate::util::Hypervector;
#[cfg(test)]
use crate::D_BITS;

/// Default decay constant: commits older than τ contribute weight
/// `exp(-1) ≈ 0.37` of a fresh commit. 90 days expressed in seconds.
pub const DEFAULT_TAU_SECONDS: f64 = 90.0 * 86_400.0;

/// Domain seed for the temporal codebook's hyperplane matrix. NEVER
/// change once production data is encoded.
pub const TEMPORAL_HYPERPLANE_SEED: &str = "hdc-temporal-v1";

/// Sparse co-edit matrix: maps `(scope_id, scope_id)` ordered pairs to
/// time-decayed co-occurrence weight. Append-only — `add_commit` adds
/// to existing weights; weights only ever increase, with the "decay"
/// captured by the time of the call.
///
/// Internally indexes scope_ids to integers so the simhash projection
/// can treat the row as a sparse `(index, weight)` collection without
/// allocating a dense O(N) vector per projection.
pub struct TemporalCoEditMatrix {
    /// scope_id → dense index. Assigned in order of first appearance.
    /// Stable across the matrix's lifetime once a scope is registered.
    scope_index: HashMap<String, usize>,
    /// Symmetric weights: `weights[(i, j)]` for i < j (canonical
    /// ordering avoids double-storage and lets `co_edit(i, j)` find
    /// the entry regardless of which scope was passed first).
    weights: HashMap<(usize, usize), f64>,
    tau_seconds: f64,
}

impl TemporalCoEditMatrix {
    pub fn new() -> Self {
        Self::with_tau(DEFAULT_TAU_SECONDS)
    }

    pub fn with_tau(tau_seconds: f64) -> Self {
        TemporalCoEditMatrix {
            scope_index: HashMap::new(),
            weights: HashMap::new(),
            tau_seconds,
        }
    }

    fn intern(&mut self, scope_id: &str) -> usize {
        if let Some(&idx) = self.scope_index.get(scope_id) {
            return idx;
        }
        let idx = self.scope_index.len();
        self.scope_index.insert(scope_id.to_string(), idx);
        idx
    }

    fn lookup(&self, scope_id: &str) -> Option<usize> {
        self.scope_index.get(scope_id).copied()
    }

    /// Record one commit. `touched` is the set of scope_ids modified
    /// in this commit. `now_seconds` is the current wall-clock time
    /// (Unix seconds); `commit_seconds` is the commit's timestamp.
    /// Older commits contribute decayed weight: `exp(-(now - t)/τ)`.
    pub fn add_commit(&mut self, touched: &[&str], now_seconds: f64, commit_seconds: f64) {
        let age = (now_seconds - commit_seconds).max(0.0);
        let weight = (-age / self.tau_seconds).exp();

        // Pair every scope with every other in the touched set, add
        // weight to their co-edit cell.
        let indices: Vec<usize> = touched.iter().map(|s| self.intern(s)).collect();
        for (a_pos, &i) in indices.iter().enumerate() {
            for &j in &indices[a_pos + 1..] {
                let key = if i < j { (i, j) } else { (j, i) };
                *self.weights.entry(key).or_insert(0.0) += weight;
            }
        }
    }

    /// Build the sparse row vector for `scope_id`: `(other_index,
    /// weight)` pairs. Empty if scope is unknown or has no co-editors.
    pub fn sparse_row(&self, scope_id: &str) -> Vec<(usize, f64)> {
        let Some(idx) = self.lookup(scope_id) else {
            return Vec::new();
        };
        let mut row = Vec::new();
        for (&(i, j), &w) in &self.weights {
            if i == idx {
                row.push((j, w));
            } else if j == idx {
                row.push((i, w));
            }
        }
        row
    }

    /// Number of scopes registered. Equal to the dense row length the
    /// codebook projection treats sparsely.
    pub fn scope_count(&self) -> usize {
        self.scope_index.len()
    }

    /// Number of non-zero co-edit cells. For diagnostics.
    pub fn nnz(&self) -> usize {
        self.weights.len()
    }
}

impl Default for TemporalCoEditMatrix {
    fn default() -> Self {
        Self::new()
    }
}

/// Temporal-layer codebook. Holds a Charikar hyperplane matrix sized
/// for the maximum scope_count we expect; sparse projection iterates
/// only the non-zero entries of a row.
///
/// Unlike SemanticCodebook (whose hyperplane width is the embedding
/// dimension), TemporalCodebook's hyperplane width is the scope_count
/// — i.e. the number of scopes in the matrix. Construction takes a
/// `max_scopes` cap so we can pre-allocate.
pub struct TemporalCodebook {
    /// `D × max_scopes` hyperplane matrix, row-major.
    hyperplanes: Vec<Vec<f32>>,
    max_scopes: usize,
}

impl TemporalCodebook {
    pub fn new(max_scopes: usize) -> Self {
        Self::new_with_seed(max_scopes, TEMPORAL_HYPERPLANE_SEED)
    }

    pub fn new_with_seed(max_scopes: usize, seed_tag: &str) -> Self {
        TemporalCodebook {
            hyperplanes: super::build_hyperplane_matrix(seed_tag, max_scopes),
            max_scopes,
        }
    }

    /// Project a sparse row to a D-bit hypervector. Bit `i` is
    /// `sign(Σ_{(idx, w) in row} hyperplanes[i][idx] * w)`.
    pub fn project_sparse(&self, row: &[(usize, f64)]) -> Hypervector {
        let max_scopes = self.max_scopes;
        super::simhash_signs(&self.hyperplanes, |plane| {
            row.iter()
                .filter(|(idx, _)| *idx < max_scopes)
                .map(|(idx, w)| plane[*idx] as f64 * w)
                .sum()
        })
    }

    /// Project a scope's temporal row through the simhash. Wrapper
    /// for `project_sparse(matrix.sparse_row(scope_id))`.
    pub fn project_scope(
        &self,
        matrix: &TemporalCoEditMatrix,
        scope_id: &str,
    ) -> Hypervector {
        self.project_sparse(&matrix.sparse_row(scope_id))
    }

    pub fn max_scopes(&self) -> usize {
        self.max_scopes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::{assert_far_apart, popcount_distance};

    /// One day in seconds — convenience for time-decay tests.
    const DAY: f64 = 86_400.0;

    /// A "now" timestamp shared across temporal tests (Unix epoch
    /// Nov 14 2023). Replaces the literal `1_700_000_000.0` that
    /// appeared 11× in the test bodies. Any reasonable past time
    /// works; using the same value everywhere makes test output
    /// uniform and pins one canonical reference point.
    const NOW: f64 = 1_700_000_000.0;

    /// Add a "fresh" commit (commit_time == now, no decay applied).
    /// Replaces the `m.add_commit(&[...], NOW, NOW)` pattern that
    /// repeated 12× across the temporal tests. Tests that need a
    /// commit_time != now still call `m.add_commit` directly with
    /// their custom commit-age offset.
    fn fresh(m: &mut TemporalCoEditMatrix, scopes: &[&str]) {
        m.add_commit(scopes, NOW, NOW);
    }

    #[test]
    fn matrix_intern_assigns_stable_indices() {
        let mut m = TemporalCoEditMatrix::new();
        let now = 0.0;
        m.add_commit(&["a", "b", "c"], now, now);
        assert_eq!(m.scope_count(), 3);
        // Re-adding a commit with same scopes shouldn't grow the index.
        m.add_commit(&["a", "b"], now, now);
        assert_eq!(m.scope_count(), 3);
        // New scope grows the index.
        m.add_commit(&["d"], now, now);
        assert_eq!(m.scope_count(), 4);
    }

    #[test]
    fn fresh_commit_has_unit_weight() {
        let mut m = TemporalCoEditMatrix::new();
        fresh(&mut m, &["a", "b"]);
        let row = m.sparse_row("a");
        assert_eq!(row.len(), 1);
        let (_, w) = row[0];
        assert!(
            (w - 1.0).abs() < 1e-9,
            "fresh commit should contribute weight 1.0 (got {w})",
        );
    }

    #[test]
    fn old_commit_decays_exponentially() {
        let mut m = TemporalCoEditMatrix::with_tau(DAY * 90.0);
        let now = NOW;
        // Commit from τ seconds ago: weight should be exp(-1) ≈ 0.368.
        m.add_commit(&["a", "b"], now, now - DAY * 90.0);
        let row = m.sparse_row("a");
        let (_, w) = row[0];
        assert!(
            (w - (-1.0_f64).exp()).abs() < 1e-9,
            "commit at age τ should weight ≈ exp(-1), got {w}",
        );

        // Commit at age 2τ: weight ≈ exp(-2) ≈ 0.135.
        let mut m2 = TemporalCoEditMatrix::with_tau(DAY * 90.0);
        m2.add_commit(&["x", "y"], now, now - DAY * 180.0);
        let row2 = m2.sparse_row("x");
        let (_, w2) = row2[0];
        assert!(
            (w2 - (-2.0_f64).exp()).abs() < 1e-9,
            "commit at age 2τ should weight ≈ exp(-2), got {w2}",
        );
    }

    #[test]
    fn co_edit_weights_accumulate() {
        // Multiple commits touching the same pair accumulate weight.
        // Three fresh commits → weight = 3.0.
        let mut m = TemporalCoEditMatrix::new();
        for _ in 0..3 {
            fresh(&mut m, &["a", "b"]);
        }
        let row = m.sparse_row("a");
        let (_, w) = row[0];
        assert!(
            (w - 3.0).abs() < 1e-9,
            "three fresh commits on (a,b) should sum to 3.0, got {w}",
        );
    }

    #[test]
    fn unknown_scope_yields_empty_row() {
        let m = TemporalCoEditMatrix::new();
        assert!(m.sparse_row("never_seen").is_empty());
    }

    // Determinism of build_hyperplane_matrix is pinned once in
    // `codebook::tests` (see semantic.rs note for the same).

    #[test]
    fn project_empty_row_yields_balanced_zero_dot() {
        // An empty sparse row has zero dot product against every
        // hyperplane; sign(0) is non-negative in our convention so all
        // bits set to 1. (This is documented behavior — empty rows are
        // a corner case the radius calibration should treat as
        // "uninitialized" rather than "matches everything.")
        let cb = TemporalCodebook::new(16);
        let hv = cb.project_sparse(&[]);
        // All bits should be 1 (every dot is exactly 0, and 0 >= 0).
        let ones: u32 = hv.iter().map(|b| b.count_ones()).sum();
        assert_eq!(ones as usize, D_BITS);
    }

    #[test]
    fn temporal_codebook_distinguishes_co_edit_partners() {
        // Build a matrix where scope A co-edits with B (only) and
        // scope C co-edits with D (only). Project both rows; they
        // should land far apart in Hamming because the dot products
        // hit different hyperplane components.
        let mut m = TemporalCoEditMatrix::new();
        fresh(&mut m, &["a", "b"]);
        fresh(&mut m, &["c", "d"]);

        let cb = TemporalCodebook::new(8);
        let hv_a = cb.project_scope(&m, "a");
        let hv_c = cb.project_scope(&m, "c");

        let d = popcount_distance(&hv_a, &hv_c);
        // Different sparse-row support → different sign patterns →
        // significant Hamming distance. Shouldn't be exactly D/2 because
        // they share no entries, but should be substantially > 0.
        assert!(
            d > 100,
            "scopes with disjoint co-edit partners should differ significantly (got {d})",
        );
    }

    #[test]
    fn temporal_codebook_close_for_same_partners() {
        // Two scopes that BOTH co-edit with the same partner X share
        // most of their non-zero row support. Their projections should
        // therefore be much closer than two scopes with disjoint
        // partners.
        let mut m = TemporalCoEditMatrix::new();
        // a and c both co-edit with b.
        fresh(&mut m, &["a", "b"]);
        fresh(&mut m, &["c", "b"]);
        // d co-edits with e (disjoint).
        fresh(&mut m, &["d", "e"]);

        let cb = TemporalCodebook::new(16);
        let hv_a = cb.project_scope(&m, "a");
        let hv_c = cb.project_scope(&m, "c");
        let hv_d = cb.project_scope(&m, "d");

        let d_ac = popcount_distance(&hv_a, &hv_c);
        let d_ad = popcount_distance(&hv_a, &hv_d);

        // Shared partner → much closer than disjoint partner.
        assert!(
            d_ac < d_ad,
            "scopes with shared partner should be closer than disjoint: \
             d(a,c)={d_ac}, d(a,d)={d_ad}",
        );
    }

    #[test]
    fn temporal_codebook_seed_versioning_changes_hv() {
        // Bumping the seed produces a different hyperplane matrix,
        // and therefore a different projection. Migration safety pin.
        let mut m = TemporalCoEditMatrix::new();
        fresh(&mut m, &["a", "b"]);

        let cb_v1 = TemporalCodebook::new_with_seed(8, "hdc-temporal-v1");
        let cb_v2 = TemporalCodebook::new_with_seed(8, "hdc-temporal-v2");
        let hv_v1 = cb_v1.project_scope(&m, "a");
        let hv_v2 = cb_v2.project_scope(&m, "a");
        assert_far_apart(
            &hv_v1,
            &hv_v2,
            "temporal codebook v1 vs v2 must produce far-apart projections",
        );
    }

    #[test]
    fn incremental_stability_under_new_commits() {
        // Math-friend review E: after adding one new unrelated commit,
        // the projection of an unaffected scope should change minimally.
        // This is the load-bearing property that makes temporal HVs
        // useful for delta sync — if every commit shifted every scope's
        // HV substantially, we'd never converge on stable hotspots.
        let mut m = TemporalCoEditMatrix::new();
        fresh(&mut m, &["a", "b"]);
        fresh(&mut m, &["a", "c"]);

        // Use max_scopes=64 so any scopes added by future commits have
        // valid hyperplane columns.
        let cb = TemporalCodebook::new(64);
        let hv_before = cb.project_scope(&m, "a");

        // New commit on completely unrelated scopes.
        fresh(&mut m, &["x", "y"]);

        let hv_after = cb.project_scope(&m, "a");
        // Bit churn for an unaffected scope should be 0% — since "a"'s
        // sparse row is unchanged (no new co-edits with x or y), the
        // projection MUST be identical. Pin this exactly.
        assert_eq!(
            hv_before, hv_after,
            "unrelated commits must not change an untouched scope's HV",
        );
    }

    #[test]
    fn project_truncates_indices_above_max_scopes() {
        // If the matrix has more scopes than the codebook's max_scopes,
        // out-of-bounds indices in a sparse row should be silently
        // dropped (filter at projection time) rather than panic. This
        // is the "matrix grew past my pre-allocation" recovery path —
        // production callers should size max_scopes generously, but a
        // test fixture that overfills must not crash.
        let mut m = TemporalCoEditMatrix::new();
        // Force scope "x" to index 0.
        fresh(&mut m, &["x", "y"]);
        // Force scope "z" to index 2 (since x=0, y=1).
        fresh(&mut m, &["x", "z"]);

        // Codebook with max_scopes=2 — index 2 (scope "z") will be out
        // of bounds and must be filtered, not panic.
        let cb = TemporalCodebook::new(2);
        let hv = cb.project_scope(&m, "x");
        // Should produce a valid hypervector (no panic, deterministic).
        let hv_again = cb.project_scope(&m, "x");
        assert_eq!(hv, hv_again, "truncation path must be deterministic");
    }

    #[test]
    fn three_way_co_edit_creates_three_pairs() {
        // A commit touching N scopes should add `N choose 2` pairs to
        // the matrix. Pin the combinatorics so a refactor that switched
        // to "every pair against every later pair" or some other
        // off-by-one would be caught.
        let mut m = TemporalCoEditMatrix::new();
        fresh(&mut m, &["a", "b", "c", "d"]);
        // (a,b), (a,c), (a,d), (b,c), (b,d), (c,d) = 6 pairs
        assert_eq!(m.nnz(), 6);
    }

    #[test]
    fn add_commit_with_singleton_touched_set() {
        // A commit that touches one scope adds zero pairs (no co-edits
        // possible with self). Pin the corner case so a future refactor
        // doesn't accidentally count self-pairs.
        let mut m = TemporalCoEditMatrix::new();
        fresh(&mut m, &["a"]);
        // Scope is registered (so future commits can refer to it)…
        assert_eq!(m.scope_count(), 1);
        // …but no co-edit cells exist (no partner to pair with).
        assert_eq!(m.nnz(), 0);
        assert!(m.sparse_row("a").is_empty());
    }

    #[test]
    fn add_commit_with_empty_touched_set_is_noop() {
        let mut m = TemporalCoEditMatrix::new();
        m.add_commit(&[], 0.0, 0.0);
        assert_eq!(m.scope_count(), 0);
        assert_eq!(m.nnz(), 0);
    }

    #[test]
    fn future_commit_clamps_to_zero_age() {
        // If commit_seconds is in the future relative to now (clock
        // skew, test setup error), age clamps to 0 and weight is
        // 1.0 — not negative-age-implies-blowup.
        let mut m = TemporalCoEditMatrix::new();
        let now = 1_000.0;
        let future = 2_000.0;
        m.add_commit(&["a", "b"], now, future);
        let row = m.sparse_row("a");
        let (_, w) = row[0];
        assert!(
            (w - 1.0).abs() < 1e-9,
            "future-dated commit should clamp to weight 1.0, got {w}",
        );
    }

    #[test]
    fn nnz_grows_with_distinct_pairs() {
        let mut m = TemporalCoEditMatrix::new();
        // First commit: {a,b,c} → 3 distinct unordered pairs.
        fresh(&mut m, &["a", "b", "c"]);
        assert_eq!(m.nnz(), 3);
        // Second commit: {a,b} → existing pair, no new cells.
        fresh(&mut m, &["a", "b"]);
        assert_eq!(m.nnz(), 3);
        // Third commit: {a,d} → one new pair.
        fresh(&mut m, &["a", "d"]);
        assert_eq!(m.nnz(), 4);
    }
}
