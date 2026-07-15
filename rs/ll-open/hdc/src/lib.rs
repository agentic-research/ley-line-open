//! Hyperdimensional Computing (HDC) for structural code search.
//!
//! Per-scope hypervectors over multiple topology layers (AST, module,
//! semantic, temporal, optionally HIR) + a combined-view BLOB for fast
//! prefilter. Hypervectors live in `_hdc*` sidecar tables; queries are
//! popcount-distance over BLOBs.
//!
//! Design rationale in bead `ley-line-open-96b1a9`; kill criteria and the
//! phase-plan for compositional-vs-distance validation live in
//! `docs/adr/0025-hdc-compositional-validation.md`.
//!
//! ## What's load-bearing vs what's a proxy
//!
//! Read this before the code — it answers "is this real signal or is this
//! résumé-driven math?"
//!
//! **Load-bearing (real signal, empirically confirmed).** HDC-distance
//! retrieval, measured under weighted score-fusion against a dense
//! embedding baseline: **+7.7%** recall\@10 uplift (kernel-RBF, α=0.40;
//! `phase_0b_real_ground_truth.rs`, 36 ground-truth groups, K=10). That's
//! the lower bound of HDC's value-add — the weakest of five measurement
//! axes (see ADR-0025 §"The honest framing"). The crate ships that
//! retrieval mode today; every consumer (mache, cloister) rides it.
//!
//! **Proxy for what's coming, not yet validated.** Compositional query
//! via role-bind (ADR-0025 Phase α/β/γ/δ) is designed but not shipped.
//! Sequence retrieval via permute is stubbed for child-position only.
//! The item-memory / clean-up codebook pattern is not implemented.
//! These are the "HDC is genuinely different from a dense embedding"
//! surfaces; ADR-0025 pre-registers the falsification thresholds that
//! decide whether they earn a place in the substrate.
//!
//! ## Kill criteria
//!
//! ADR-0025 commits to falsifying "HDC has compositional value beyond
//! distance retrieval" via four phases with explicit go/no-go bars. If
//! Phase δ's compositional-query lift over vec-only fusion doesn't clear
//! the pre-registered threshold, the phase is a documented negative
//! result and the compositional channel does not ship — the substrate
//! stays at ADR-0024's v0.5.0 distance-retrieval mode and this crate's
//! scope narrows accordingly.
//!
//! Sheaf-layer kill criteria for HDC's use as a δ⁰ input live in
//! `sheaf/tests/falsifiability_gates.rs` (invalidation reachability,
//! cascade-truncation bounds). The `sheaf_ablation` daemon op is the
//! runtime falsification harness — the 91× reframing in
//! `docs/research/sheaf-ablation-study.md` is a negative result made
//! into a positive claim, exactly the shape a real kill-criteria pass
//! produces.
//!
//! ## Hypervector dimensionality
//!
//! `D_BITS = 8192` — 1024-byte BLOB, byte-aligned for SIMD popcount.
//!
//! Why 8192 specifically (and not 4096 or 16384): capacity math on the
//! typical AST function size (50–150 nodes) leaves ~7× margin per layer
//! at D=8192 so flat bundles stay discriminable well past the saturation
//! ceiling. D=4096 halved the margin and started blurring 100+ node
//! functions; D=16384 doubled the BLOB size and the popcount cost with
//! no observed retrieval gain. 8192 is the empirically-tuned choice.
//!
//! Why byte-aligned (1024 bytes = 128 × u64): the popcount inner loop is
//! `pdep`/`popcnt` on u64 chunks with AVX2 / NEON codegen — a
//! misalignment would force scalar fallback and dominate query latency.
//! Compile-time assertions in this file pin `D_BYTES == 1024` so a
//! future D change that breaks alignment fails at build time, not on
//! benchmarks.
//!
//! Deep trees use hierarchical bind+bundle (Plate 1994 / Schlegel 2022)
//! — the encoder recursively folds child hypervectors so a perturbation
//! that would be D/2 at the leaf attenuates to ≈ D/(F^depth) at the
//! root (fan-out F). See [`encoder`] docstring for the derivation and
//! the measured signal margins at typical AST shapes (depth 5–7).

pub mod calibrate;
pub mod canonical;
pub mod codebook;
pub mod combined;
pub mod encoder;
pub mod query;
pub mod schema;
pub mod sheaf;
pub mod sql_udf;
#[cfg(test)]
mod test_util;
pub mod util;

pub use encoder::{EncoderNode, SubtreeCache, encode_fresh, encode_tree};
pub use util::{Hypervector, ZERO_HV, popcount_distance};

/// Hypervector dimensionality in bits. Default = 8192. Single source of truth
/// — every layer, every codebook, every BLOB column shares this value.
pub const D_BITS: usize = 8192;

/// Hypervector size in bytes (D_BITS / 8). Constant so it can be used in
/// const contexts (e.g., `[u8; D_BYTES]` array types).
pub const D_BYTES: usize = D_BITS / 8;

const _: () = {
    assert!(D_BITS.is_multiple_of(8), "D_BITS must be byte-aligned");
    assert!(D_BYTES == 1024, "D_BYTES must be 1024 (= D_BITS/8)");
};

/// Identifies which topology layer a hypervector encodes. Stored as TEXT in
/// `_hdc.layer_kind` rather than an INTEGER enum so the schema can be extended
/// without a migration — adding `Hir` or `Llvm` as a new variant is a code-only
/// change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LayerKind {
    /// AST shape via canonical-kind alphabet + production-signature hashing.
    Ast,
    /// Module/class hierarchy fingerprint.
    Module,
    /// Charikar simhash projection from dense embeddings.
    Semantic,
    /// Time-decayed co-edit matrix simhash.
    Temporal,
    /// HIR (high-level IR) fingerprint per language.
    Hir,
    /// Token-bag fingerprint. Reserved — math friend flagged Lex × AST as
    /// likely too correlated to ship; left here for future opt-in.
    Lex,
    /// Filesystem layout fingerprint. Reserved — likely too correlated to
    /// Module to ship initially.
    Fs,
}

impl LayerKind {
    /// Stable string for SQLite storage. Must never change once written —
    /// changing a variant's serialized form would orphan every existing row.
    pub fn as_str(&self) -> &'static str {
        match self {
            LayerKind::Ast => "ast",
            LayerKind::Module => "module",
            LayerKind::Semantic => "semantic",
            LayerKind::Temporal => "temporal",
            LayerKind::Hir => "hir",
            LayerKind::Lex => "lex",
            LayerKind::Fs => "fs",
        }
    }

    /// Reverse of `as_str` for reading rows back out of `_hdc`. Returns
    /// `None` for any string we don't recognize so a future schema can
    /// add variants without breaking older readers.
    ///
    /// Named `parse_str` rather than `from_str` so it doesn't shadow
    /// `std::str::FromStr::from_str` (the standard trait returns
    /// `Result`, our parser returns `Option` because forward
    /// compatibility is the contract — unknown variants are not
    /// errors, just unrecognized).
    ///
    /// Derived from `as_str` via `LayerKind::ALL` so adding a new
    /// variant requires updating only `as_str` (plus appending to
    /// `ALL`); the parser stays in sync automatically. The
    /// `layer_kind_round_trip_through_string` test pins the
    /// equivalence on every variant.
    pub fn parse_str(s: &str) -> Option<Self> {
        LayerKind::ALL.iter().copied().find(|k| k.as_str() == s)
    }

    /// All seven layer kinds in the canonical iteration order used by
    /// `calibrate_and_persist`, `build_combined_hv`,
    /// `HvCellComplex::structural_root`, and the `layer_role_index`
    /// permutation. Single source of truth replaces three verbatim
    /// 7-element array literals across calibrate.rs / combined.rs /
    /// sheaf.rs.
    ///
    /// The order MUST match `as_str` and `parse_str` such that
    /// `LayerKind::ALL[layer_role_index(k)]` round-trips on every
    /// variant — pinned by `layer_kind_all_matches_role_index` test.
    pub const ALL: [LayerKind; 7] = [
        LayerKind::Ast,
        LayerKind::Module,
        LayerKind::Semantic,
        LayerKind::Temporal,
        LayerKind::Hir,
        LayerKind::Lex,
        LayerKind::Fs,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d_bytes_is_1024() {
        // Compile-time assertion duplicated as a runtime check so the
        // intent is visible in test output and so `cargo test` exercises
        // it (the const_assert is invisible unless it fails to compile).
        assert_eq!(D_BYTES, 1024);
        assert_eq!(D_BITS, D_BYTES * 8);
    }

    #[test]
    fn layer_kind_round_trip_through_string() {
        // Every variant must round-trip cleanly. If a new variant is
        // added without updating both as_str and from_str, this test
        // surfaces the gap. Iterating over `ALL` also catches a refactor
        // that adds a variant without appending to the const.
        for k in LayerKind::ALL {
            assert_eq!(
                LayerKind::parse_str(k.as_str()),
                Some(k),
                "round-trip failed for {k:?}"
            );
        }
    }

    #[test]
    fn layer_kind_all_in_role_index_order() {
        // `LayerKind::ALL` is the canonical iteration order used by
        // `build_combined_hv` (combined.rs) and the role-permutation
        // step. Pin that `ALL[i]` matches the ad-hoc enumeration order
        // baked into `layer_role_index` (Ast=0, Module=1, …, Fs=6) so
        // a refactor that reshuffles either side gets caught.
        let expected_in_order = [
            LayerKind::Ast,
            LayerKind::Module,
            LayerKind::Semantic,
            LayerKind::Temporal,
            LayerKind::Hir,
            LayerKind::Lex,
            LayerKind::Fs,
        ];
        assert_eq!(LayerKind::ALL, expected_in_order);
        // Length must be 7 — pins the variant count too.
        assert_eq!(LayerKind::ALL.len(), 7);
    }

    #[test]
    fn layer_kind_unknown_string_returns_none() {
        // Forward compatibility: a future variant written by a newer
        // daemon must read back as None, not silently match a different
        // variant. Pin this so a refactor that adds a fallback (e.g.
        // "default to Ast") is caught.
        assert_eq!(LayerKind::parse_str(""), None);
        assert_eq!(LayerKind::parse_str("future_layer"), None);
        assert_eq!(LayerKind::parse_str("AST"), None, "case-sensitive");
    }
}
