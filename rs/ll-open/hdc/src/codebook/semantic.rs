//! Semantic-layer codebook: Charikar signed-projection (simhash) from
//! a dense embedding to a D-bit hypervector.
//!
//! Per math-friend review D: dense embeddings (sentence-transformers,
//! code embeddings, etc.) produce real-valued vectors that capture
//! lexical/contextual semantics but no structure. Projecting via
//! Charikar's signed random hyperplanes preserves the *angular* metric
//! of the source embedding space — collision probability `1 − θ/π` per
//! Charikar (2002). The Hamming distance between two binary projections
//! tracks the angle between the original embeddings.
//!
//! This is the layer that handles "semantic clones" — functions that
//! do the same thing under different surface syntax (e.g. `for` loop
//! vs `iter().fold()` if the dense embedder has been trained to
//! cluster them).
//!
//! Reproducible across machines: hyperplane matrix is derived from a
//! fixed seed string `"hdc-semantic-v1"`, so any daemon instance with
//! the same embedder produces identical projections.

use crate::util::{splitmix64, Hypervector, ZERO_HV};
#[cfg(test)]
use crate::D_BITS;

/// Domain seed for the semantic codebook's hyperplane matrix. NEVER
/// change once production data is encoded — bumping this orphans every
/// stored semantic hypervector. Versioned suffix so a future
/// algorithm change is a deliberate migration with a new seed.
pub const SEMANTIC_HYPERPLANE_SEED: &str = "hdc-semantic-v1";

/// Charikar signed-projection codebook. Stateful: holds the
/// pre-computed hyperplane matrix `R ∈ ℝ^{D × emb_dim}`. Construction
/// is deterministic — same `(emb_dim, seed)` produces the same matrix.
pub struct SemanticCodebook {
    /// `D × emb_dim` matrix, row-major. `hyperplanes[i][j]` = j-th
    /// component of the i-th hyperplane normal vector.
    hyperplanes: Vec<Vec<f32>>,
    emb_dim: usize,
}

impl SemanticCodebook {
    /// Build a codebook for embeddings of dimension `emb_dim`. The
    /// hyperplane matrix is derived from `SEMANTIC_HYPERPLANE_SEED`,
    /// stretched via SplitMix64 + Box-Muller to D × emb_dim Gaussian
    /// random values.
    pub fn new(emb_dim: usize) -> Self {
        Self::new_with_seed(emb_dim, SEMANTIC_HYPERPLANE_SEED)
    }

    /// Construct with a custom seed. Useful for test fixtures and
    /// migration scenarios. Production callers should use `new`.
    pub fn new_with_seed(emb_dim: usize, seed_tag: &str) -> Self {
        SemanticCodebook {
            hyperplanes: super::build_hyperplane_matrix(seed_tag, emb_dim),
            emb_dim,
        }
    }

    /// Project a dense embedding to a D-bit hypervector. Bit `i` is
    /// `sign(embedding · hyperplanes[i])`. Returns all-zero
    /// hypervector for an embedding with the wrong dimension (caller
    /// can detect via `len()` mismatch); we don't panic because this
    /// is on a query-hot path.
    pub fn project(&self, embedding: &[f32]) -> Hypervector {
        if embedding.len() != self.emb_dim {
            log::warn!(
                "SemanticCodebook::project: embedding dim mismatch (got {}, expected {})",
                embedding.len(),
                self.emb_dim,
            );
            return ZERO_HV;
        }
        super::simhash_signs(&self.hyperplanes, |plane| {
            embedding
                .iter()
                .zip(plane.iter())
                .map(|(a, b)| (a * b) as f64)
                .sum()
        })
    }

    pub fn embedding_dim(&self) -> usize {
        self.emb_dim
    }
}

/// Box-Muller transform: produce `n` independent Gaussian-distributed
/// f32 values from a SplitMix64 seed. Standard normal (mean 0, var 1).
/// Shared with TemporalCodebook (which uses the same simhash projection
/// machinery on a different input shape).
pub(super) fn gaussian_row(seed: u64, n: usize) -> Vec<f32> {
    use std::f32::consts::TAU;

    let mut state = seed;
    let mut out = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        // Two uniforms per Gaussian pair.
        let u1 = next_uniform(&mut state);
        let u2 = next_uniform(&mut state);
        let r = (-2.0 * u1.ln()).sqrt();
        let theta = TAU * u2;
        out.push(r * theta.cos());
        if i + 1 < n {
            out.push(r * theta.sin());
            i += 2;
        } else {
            i += 1;
        }
    }
    out
}

/// Uniform [eps, 1) sample from a SplitMix64 stream. Avoids 0 because
/// Box-Muller calls `ln(u)` and ln(0) = -infinity.
fn next_uniform(state: &mut u64) -> f32 {
    let bits = splitmix64(state);
    // Map to [eps, 1) by setting the high 24 bits as fraction; the
    // floor of 1ULP avoids the exact-zero pathology.
    let f = (bits >> 40) as f32 / (1u32 << 24) as f32;
    f.max(f32::EPSILON)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::{assert_far_apart, popcount_distance};

    /// Tolerance margin for distance assertions on small embedding
    /// dims — Box-Muller variance + small-sample-projection noise
    /// gives roughly ±0.05 around the theoretical Charikar curve.
    const CHARIKAR_TOLERANCE: f64 = 0.05;

    fn dot(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    fn norm(v: &[f32]) -> f32 {
        dot(v, v).sqrt()
    }

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        dot(a, b) / (norm(a) * norm(b))
    }

    // Determinism of `build_hyperplane_matrix` (same tag+width →
    // same matrix) is pinned once at the source in
    // `codebook::tests::build_hyperplane_matrix_is_deterministic_per_seed_and_width`.
    // SemanticCodebook::new just calls that helper, so its
    // determinism is implied; consumer-side test removed to avoid
    // duplicating the same property pin.

    #[test]
    fn project_identical_embeddings_produces_identical_hvs() {
        let cb = SemanticCodebook::new(32);
        let emb: Vec<f32> = (0..32).map(|i| (i as f32 * 0.1).sin()).collect();
        let hv1 = cb.project(&emb);
        let hv2 = cb.project(&emb);
        assert_eq!(hv1, hv2);
    }

    #[test]
    fn gaussian_row_outputs_are_always_finite() {
        // next_uniform's `f.max(f32::EPSILON)` floor (semantic.rs:121)
        // is what keeps Box-Muller's `ln(u)` from blowing up on the
        // exact-zero u case. If the floor is dropped or the bit-shift
        // reordered, any state that hashed to a high-zero result would
        // produce NaN/inf gaussians and downstream simhash projections
        // would set bits arbitrarily. Pin: drive gaussian_row over a
        // diverse set of seeds and a variety of lengths (including
        // odd lengths that exercise the truncate branch) and assert
        // every output is finite (not NaN, not inf).
        for &seed in &[0u64, 1, u64::MAX, 0xCAFE_BABE_DEAD_BEEF, 42] {
            for &n in &[1usize, 2, 7, 64, 128] {
                let row = gaussian_row(seed, n);
                assert_eq!(row.len(), n);
                for (i, &x) in row.iter().enumerate() {
                    assert!(
                        x.is_finite(),
                        "gaussian_row({seed:#x}, {n})[{i}] = {x} is not finite",
                    );
                }
            }
        }
    }

    #[test]
    fn gaussian_row_is_prefix_stable_across_lengths() {
        // Same seed, different n: the first n floats must match.
        // build_hyperplane_matrix derives per-row seeds via blake3
        // mixing, so this matters cross-time: if a future grammar
        // forces a wider matrix (D bumped), the first D_old rows
        // remain stable so existing encoded HVs don't drift.
        // A refactor that tied internal PRNG advancement to total n
        // (e.g. seeded the state with `seed ^ n`) would break this.
        let short = gaussian_row(0xCAFE, 4);
        let long = gaussian_row(0xCAFE, 8);
        assert_eq!(short.len(), 4);
        assert_eq!(long.len(), 8);
        assert_eq!(short.as_slice(), &long[..4]);

        // Odd-length boundary: 5 vs 4. The 5th uses the truncate
        // branch but the first 4 are still independent of n.
        let five = gaussian_row(0xCAFE, 5);
        assert_eq!(short.as_slice(), &five[..4]);
    }

    #[test]
    fn gaussian_row_zero_length_returns_empty() {
        // Boundary pin: gaussian_row(_, 0) must return an empty Vec
        // without panicking. The caller (build_hyperplane_matrix)
        // always passes width > 0 in production, but defensive: a
        // refactor that index-allocated `out[0] = ...` before the
        // bounds check would crash on zero. The while-loop entry
        // condition (line 97 `while i < n`) is what protects us.
        assert!(gaussian_row(0xDEAD, 0).is_empty());
        assert!(gaussian_row(0, 0).is_empty());
    }

    #[test]
    fn gaussian_row_returns_n_floats_deterministically() {
        // gaussian_row(seed, n) must produce exactly `n` floats
        // (Box-Muller pairs handle odd n via the truncate-to-cos
        // branch). Same seed → byte-identical output.
        let r1 = gaussian_row(0x42, 8);
        let r2 = gaussian_row(0x42, 8);
        assert_eq!(r1.len(), 8);
        assert_eq!(r1, r2, "same seed must produce byte-identical row");

        // Odd width exercises the truncate branch (line 107-109).
        let odd = gaussian_row(0x42, 7);
        assert_eq!(odd.len(), 7);
    }

    #[test]
    fn project_zero_embedding_yields_all_ones_per_simhash_convention() {
        // Zero embedding has zero dot product against every
        // hyperplane; sign(0) is non-negative in our convention so
        // all bits set to 1. Mirrors `temporal::project_empty_row_
        // yields_balanced_zero_dot` for the dense path.
        let cb = SemanticCodebook::new(16);
        let zero_emb: Vec<f32> = vec![0.0; 16];
        let hv = cb.project(&zero_emb);
        let ones: u32 = hv.iter().map(|b| b.count_ones()).sum();
        assert_eq!(ones as usize, D_BITS, "zero-embedding must produce all-1 HV");
    }

    #[test]
    fn project_dim_mismatch_returns_zero_hv() {
        // Wrong dimension → log + return ZERO_HV, no panic. A daemon
        // mid-query shouldn't crash on a single bad embedding.
        let cb = SemanticCodebook::new(16);
        let bad: Vec<f32> = vec![0.1; 32];
        assert_eq!(cb.project(&bad), ZERO_HV);
    }

    #[test]
    fn project_balanced_output_for_random_embedding() {
        // Random unit-vector embeddings should produce ~D/2 ones
        // (Charikar projection through random hyperplanes is balanced
        // by construction). Skew would shift the distance baseline
        // and break radius calibration.
        let cb = SemanticCodebook::new(64);
        // Synthesize a deterministic "random" embedding via SplitMix64.
        let mut state: u64 = 0xCAFE_BABE;
        let emb: Vec<f32> = (0..64)
            .map(|_| {
                let u = next_uniform(&mut state);
                u * 2.0 - 1.0 // map to [-1, 1)
            })
            .collect();
        let hv = cb.project(&emb);
        let ones: u32 = hv.iter().map(|b| b.count_ones()).sum();
        let expected = (D_BITS / 2) as u32;
        // ±3σ ≈ ±136 for D=8192. Generous tolerance ±300 since one
        // sample with this small emb_dim has more variance than
        // theory predicts.
        assert!(
            ones.abs_diff(expected) < 300,
            "Charikar projection unbalanced: {ones} ones (expected ~{expected})",
        );
    }

    #[test]
    fn charikar_collision_probability_matches_theory() {
        // Synthesize two embeddings with known cosine similarity.
        // Charikar's theorem: P(bit collision) = 1 - θ/π, where
        // θ = arccos(cosine). Hamming distance / D = 1 - P(collision)
        //                                          = θ/π.
        // For cosine ≈ 1: θ ≈ 0, expected d/D ≈ 0.
        // For cosine = 0: θ = π/2, expected d/D = 0.5.
        // For cosine = -1: θ = π, expected d/D = 1.
        //
        // Test three cosine targets and verify Hamming matches within
        // CHARIKAR_TOLERANCE. This pins the load-bearing semantic
        // property — a regression that broke the projection (e.g.
        // dropped sign, wrong dot product) would shift Hamming away
        // from the theoretical curve.
        let cb = SemanticCodebook::new(128);

        // Build two unit vectors with controlled cosine.
        // a = (1, 0, ..., 0)
        // b = (cos(theta), sin(theta), 0, ..., 0)
        // dot(a, b) = cos(theta) since both unit-norm.
        let mut a = vec![0.0_f32; 128];
        a[0] = 1.0;

        for &target_cos in &[0.95_f32, 0.5, 0.0] {
            let theta = target_cos.acos();
            let mut b = vec![0.0_f32; 128];
            b[0] = theta.cos();
            b[1] = theta.sin();

            // Sanity: actual cosine matches target.
            let actual_cos = cosine(&a, &b);
            assert!(
                (actual_cos - target_cos).abs() < 1e-4,
                "synthesized cosine drift: target {target_cos}, got {actual_cos}",
            );

            let ha = cb.project(&a);
            let hb = cb.project(&b);
            let d = popcount_distance(&ha, &hb) as f64 / D_BITS as f64;
            let expected = (theta as f64) / std::f64::consts::PI;

            assert!(
                (d - expected).abs() < CHARIKAR_TOLERANCE,
                "Charikar curve drift at cos={target_cos}: \
                 expected d/D≈{expected:.3}, got {d:.3} (tolerance {CHARIKAR_TOLERANCE})",
            );
        }
    }

    #[test]
    fn semantic_codebook_seed_tag_versioning_changes_hv() {
        // Different seed-tag produces a different hyperplane matrix
        // and therefore a different projection. This is the migration
        // pin — bumping `SEMANTIC_HYPERPLANE_SEED` is intentionally
        // breaking, and the test demonstrates that a well-known
        // embedding doesn't accidentally produce the same hypervector
        // under both seed versions.
        let cb_v1 = SemanticCodebook::new_with_seed(32, "hdc-semantic-v1");
        let cb_v2 = SemanticCodebook::new_with_seed(32, "hdc-semantic-v2");
        let emb: Vec<f32> = (0..32).map(|i| i as f32 - 16.0).collect();
        let hv_v1 = cb_v1.project(&emb);
        let hv_v2 = cb_v2.project(&emb);
        assert_far_apart(
            &hv_v1,
            &hv_v2,
            "semantic codebook v1 vs v2 must produce far-apart projections",
        );
    }

    #[test]
    fn semantic_hyperplane_seed_constant_literal() {
        // SEMANTIC_HYPERPLANE_SEED is hashed into the hyperplane
        // matrix via build_hyperplane_matrix. Bumping it orphans
        // every encoded semantic hypervector. The convenience
        // wrapper pin (semantic_new_matches_new_with_seed_at_default
        // _constant) confirms `new()` uses this constant, but the
        // literal value itself wasn't pinned. Pin so a typo or
        // unintentional version bump surfaces immediately.
        assert_eq!(SEMANTIC_HYPERPLANE_SEED, "hdc-semantic-v1");
    }

    #[test]
    fn semantic_new_matches_new_with_seed_at_default_constant() {
        // SemanticCodebook::new(dim) is documented as a convenience
        // wrapper for `new_with_seed(dim, SEMANTIC_HYPERPLANE_SEED)`.
        // Pin the equivalence so a refactor that bumped the default
        // seed without bumping the constant — or vice versa — would
        // surface immediately. Same emb input through both must
        // produce byte-identical projections.
        let cb_default = SemanticCodebook::new(32);
        let cb_explicit = SemanticCodebook::new_with_seed(32, SEMANTIC_HYPERPLANE_SEED);
        let emb: Vec<f32> = (0..32).map(|i| (i as f32 * 0.07).cos()).collect();
        assert_eq!(cb_default.project(&emb), cb_explicit.project(&emb));
    }

    #[test]
    fn embedding_dim_accessor() {
        let cb = SemanticCodebook::new(384);
        assert_eq!(cb.embedding_dim(), 384);
    }
}
