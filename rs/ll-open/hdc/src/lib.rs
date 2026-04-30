//! Hyperdimensional Computing (HDC) for structural code search.
//!
//! Provides per-scope hypervectors over multiple topology layers (AST, module,
//! semantic, temporal, optionally HIR) plus a combined-view BLOB for fast
//! prefilter queries. Hypervectors live in `_hdc*` sidecar tables; queries are
//! popcount-distance over BLOBs.
//!
//! See `bead ley-line-open-96b1a9` for the full design rationale and the
//! per-layer codebook plan. This crate ships the storage substrate (`hdc-1`);
//! codebooks and encoders land in subsequent beads.
//!
//! ## Hypervector dimensionality
//!
//! D = 8192 bits per vector — 1024 bytes BLOB, byte-aligned for SIMD popcount.
//! Math-friend review: D=8192 leaves ~7× capacity margin per layer for typical
//! AST function sizes (50-150 nodes), so flat bundles stay discriminable. For
//! deeper trees the encoder uses recursive (hierarchical) bundling, which
//! sidesteps the saturation ceiling on flat bundles.

pub mod canonical;
pub mod codebook;
pub mod encoder;
pub mod schema;
pub mod util;

pub use encoder::{encode_fresh, encode_tree, distance, EncoderNode, SubtreeCache};
pub use util::{Hypervector, ZERO_HV};

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
    pub fn parse_str(s: &str) -> Option<Self> {
        match s {
            "ast" => Some(LayerKind::Ast),
            "module" => Some(LayerKind::Module),
            "semantic" => Some(LayerKind::Semantic),
            "temporal" => Some(LayerKind::Temporal),
            "hir" => Some(LayerKind::Hir),
            "lex" => Some(LayerKind::Lex),
            "fs" => Some(LayerKind::Fs),
            _ => None,
        }
    }
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
        // surfaces the gap.
        for k in [
            LayerKind::Ast,
            LayerKind::Module,
            LayerKind::Semantic,
            LayerKind::Temporal,
            LayerKind::Hir,
            LayerKind::Lex,
            LayerKind::Fs,
        ] {
            assert_eq!(LayerKind::parse_str(k.as_str()), Some(k), "round-trip failed for {k:?}");
        }
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
