//! BaseCodebook trait + AST-fingerprint type that the AST codebook consumes.
//!
//! The trait is generic over the input `Item` so per-layer codebooks (AST,
//! Module, Semantic, Temporal, HIR) can each define their own input shape
//! while sharing the same encoder + storage infrastructure downstream.
//!
//! See bead `ley-line-open-96b1a9` for the per-layer codebook plan.

use crate::canonical::CanonicalKind;
use crate::util::{blake3_seed, tagged_seed_vector, Hypervector};
use crate::{D_BITS, D_BYTES};

/// Per-layer codebook: maps domain-specific node fingerprints to base
/// hypervectors. Implementors must be `Send + Sync` so the encoder can
/// be invoked from any tokio task without further wrapping.
///
/// `base_vector(item)` produces the deterministic per-node fingerprint;
/// the encoder XOR-bundles those with permuted child hypervectors to
/// build the function-level vector.
///
/// `role_vector(role_index)` produces a deterministic per-role vector
/// that codebooks may use for non-positional binding (e.g. the module
/// codebook will bind "this is the method set" vs "this is the field
/// set"). The AST encoder doesn't use it — AST positional encoding
/// is via circular bit-rotation in `encoder::encode_tree`, which
/// preserves order in a way XOR-binding can't (XOR is commutative).
///
/// `codebook_tag()` produces a stable identity string for the
/// codebook. Used in subtree cache keys to prevent cross-codebook
/// poisoning: a cache warmed by codebook A must NOT return entries
/// when queried by codebook B even on the same input tree. Skeptic
/// (bead 4ba0cf) caught this latent bug; the tag closes it.
pub trait BaseCodebook: Send + Sync {
    /// What this codebook accepts. For the AST codebook it's
    /// `AstNodeFingerprint`; for module/semantic/etc. it'll be
    /// per-layer types.
    type Item;

    /// Map the input fingerprint to a base hypervector. Must be a
    /// pure function of the input — same item, same vector, on
    /// every call, every machine, every version.
    fn base_vector(&self, item: &Self::Item) -> Hypervector;

    /// Stable identity string for this codebook. Used as part of
    /// subtree cache keys so a cache shared across codebooks doesn't
    /// silently return one codebook's hypervectors when queried by
    /// another. Must NEVER change once production data is encoded —
    /// changing the tag invalidates every cached entry.
    ///
    /// Convention: lowercase, hyphen-separated, prefixed with the
    /// crate domain (`hdc-ast`, `hdc-module`, etc.).
    fn codebook_tag(&self) -> &'static str;

    /// Map a logical role to a role hypervector. Used by codebooks that
    /// need non-positional binding (e.g. layer-tagged combined view,
    /// module method-set vs field-set). The AST encoder doesn't call
    /// this — positional encoding is via permutation.
    ///
    /// Default impl derives the role tag from `codebook_tag()` by
    /// appending `"-role"`, so AstCodebook ("hdc-ast") yields role
    /// tag "hdc-ast-role" and ModuleCodebook ("hdc-module") yields
    /// "hdc-module-role" — automatically domain-separated per
    /// codebook without each impl having to override. Impls only
    /// need to override if they want a non-standard tag scheme
    /// (skeptic 4bbc54: previously every impl wrote an identical
    /// override; now the default suffices and the dedup is real).
    fn role_vector(&self, role_index: usize) -> Hypervector {
        let tag = format!("{}-role", self.codebook_tag());
        tagged_seed_vector(&tag, role_index)
    }
}

/// AST-layer fingerprint: the minimum information the AST codebook needs
/// to derive a base hypervector for a node. Captures the canonical kind,
/// arity bucket, and the canonical kinds of the named children — the
/// "Deckard production signature" that's stable across grammar versions.
///
/// Deliberately erases:
/// - Identifier names (variables, function names) — leaf-erasure
/// - Literal values (numbers, strings) — leaf-erasure
/// - Specific parser-version kind names (replaced by canonical kind)
/// - Anonymous tree-sitter children — only named children contribute
///
/// What remains is the structural skeleton: an `if x { foo(y) }` and an
/// `if a { bar(b) }` produce the same fingerprint, hence the same
/// hypervector — they're a structural equivalence class.
///
/// Owns its `child_canonical_kinds` vec — adds one alloc per node, paid
/// at parse time (cold path), amortized by the encoder's content-hash
/// subtree cache. Trades a tiny amount of memory for trait-object
/// ergonomics across multiple codebook impls.
#[derive(Debug, Clone)]
pub struct AstNodeFingerprint {
    pub canonical_kind: CanonicalKind,
    pub arity_bucket: u8,
    /// Canonical kinds of named children, sorted to be order-invariant
    /// at the codebook level. Order-sensitivity comes back via role
    /// permutation when the encoder XOR-binds each child's hypervector
    /// to its slot.
    pub child_canonical_kinds: Vec<CanonicalKind>,
}

impl AstNodeFingerprint {
    /// Construct a leaf-node fingerprint: zero arity, no children.
    /// Replaces the verbatim `AstNodeFingerprint { kind, 0, vec![] }`
    /// pattern that was duplicated across multiple call sites
    /// (4 in query.rs alone). Stable shape — used by cleanup-memory
    /// to probe for "what kind would a leaf at this position be".
    pub fn leaf(kind: CanonicalKind) -> Self {
        Self {
            canonical_kind: kind,
            arity_bucket: 0,
            child_canonical_kinds: Vec::new(),
        }
    }
}

/// Canonical byte layout for a node's structural signature, used by
/// every codebook that hashes "(kind, arity bucket, sorted child kinds)"
/// — currently AstCodebook and ModuleCodebook. Format:
///
///   `[codebook_tag, 0x00, kind_disc(1), arity_bucket(1), child_count_le(2), sorted_child_discs(N)]`
///
/// The codebook tag prefix is what makes ModuleCodebook's base_vector
/// distinct from AstCodebook's for the same fingerprint — without it,
/// both codebooks produce IDENTICAL hypervectors per fingerprint (per
/// skeptic-review bead 4bb8a0; same fix as the cache_key issue).
///
/// The length prefix prevents `(k, [])` from colliding with `(k, [k])`
/// (both would otherwise hash to the same bytes if a sub-pattern crosses
/// the boundary). Sorted children → order-invariant at the codebook
/// level; encoder restores positional order via rotation.
///
/// CHANGING THIS BREAKS EVERY ENCODED HYPERVECTOR. Bump a layer's seed
/// tag if you need to migrate, don't silently rewrite the format.
pub fn canonical_signature_bytes(
    codebook_tag: &str,
    kind: CanonicalKind,
    arity_bucket: u8,
    child_kinds: &[CanonicalKind],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(codebook_tag.len() + 5 + child_kinds.len());
    buf.extend_from_slice(codebook_tag.as_bytes());
    buf.push(0u8); // separator between tag and structural payload
    buf.push(kind.discriminant());
    buf.push(arity_bucket);
    let len = child_kinds.len() as u16;
    buf.extend_from_slice(&len.to_le_bytes());
    let mut sorted: Vec<u8> = child_kinds.iter().map(|k| k.discriminant()).collect();
    sorted.sort_unstable();
    buf.extend_from_slice(&sorted);
    buf
}

/// Charikar simhash threshold step: for each hyperplane, compute its
/// dot product with the input via the caller-supplied closure, set
/// bit `i` in the output if the dot ≥ 0.
///
/// Centralizes the bit-loop shared by every signed-projection codebook
/// (Semantic dense, Temporal sparse, future ones). Each codebook
/// supplies its own dot-product semantics via the closure; the
/// thresholding + bit-packing logic is uniform and lives here.
pub fn simhash_signs<F>(hyperplanes: &[Vec<f32>], dot: F) -> Hypervector
where
    F: Fn(&[f32]) -> f64,
{
    let mut out = [0u8; D_BYTES];
    for (i, plane) in hyperplanes.iter().enumerate().take(D_BITS) {
        if dot(plane) >= 0.0 {
            out[i / 8] |= 1 << (i % 8);
        }
    }
    out
}

/// Build a `D_BITS × width` Gaussian-random hyperplane matrix
/// deterministically from a seed tag. Used by every Charikar
/// simhash codebook (Semantic, Temporal). Each row gets a
/// blake3-derived per-row seed so rows are independent.
///
/// Returns row-major `Vec<Vec<f32>>` where `hyperplanes[i]` is the
/// i-th hyperplane normal vector of length `width`.
///
/// CHANGING THE LAYOUT (per-row seeding, Box-Muller transform, etc.)
/// breaks every encoded simhash hypervector. Bump a layer's seed tag
/// for migration; don't silently rewrite this function.
pub fn build_hyperplane_matrix(seed_tag: &str, width: usize) -> Vec<Vec<f32>> {
    let base_seed = blake3_seed(seed_tag.as_bytes());
    let mut hyperplanes = Vec::with_capacity(D_BITS);
    for i in 0..D_BITS {
        // Each hyperplane gets its own seed derived from base + index.
        // Box-Muller uses two uniforms per Gaussian; per-row PRNG state
        // keeps rows independent.
        let row_seed = blake3_seed(
            &[
                base_seed.to_le_bytes().as_slice(),
                (i as u64).to_le_bytes().as_slice(),
            ]
            .concat(),
        );
        hyperplanes.push(semantic::gaussian_row(row_seed, width));
    }
    hyperplanes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_hyperplane_matrix_is_deterministic_per_seed_and_width() {
        // Cross-machine reproducibility: same (seed_tag, width) → same
        // matrix on every call, every machine, every version. Both
        // SemanticCodebook and TemporalCodebook used to duplicate this
        // pin in their own test modules; consolidated here at the
        // source so a future change to build_hyperplane_matrix's
        // determinism is caught once.
        let m1 = build_hyperplane_matrix("test-seed-A", 16);
        let m2 = build_hyperplane_matrix("test-seed-A", 16);
        assert_eq!(m1.len(), m2.len(), "row count must match");
        assert_eq!(m1.len(), D_BITS, "row count must equal D_BITS");
        for (r1, r2) in m1.iter().zip(m2.iter()) {
            assert_eq!(r1, r2, "rows must match byte-for-byte");
        }
    }

    #[test]
    fn build_hyperplane_matrix_seed_tags_produce_distinct_matrices() {
        // Different seed_tags must yield different matrices — that's
        // the property both SemanticCodebook (SEMANTIC_HYPERPLANE_SEED)
        // and TemporalCodebook (TEMPORAL_HYPERPLANE_SEED) rely on so
        // their hypervectors don't collide. Sample one row to assert
        // distinctness without comparing all D_BITS rows.
        let m_a = build_hyperplane_matrix("hdc-test-tag-A", 8);
        let m_b = build_hyperplane_matrix("hdc-test-tag-B", 8);
        assert_eq!(m_a.len(), m_b.len());
        // At least the first row should differ — Box-Muller from
        // distinct seeds is overwhelmingly likely to give distinct
        // floats; a hash collision in row 0 alone is ≈ 2^-64.
        assert_ne!(m_a[0], m_b[0]);
    }

    #[test]
    fn ast_node_fingerprint_leaf_has_zero_arity_no_children() {
        // Pin the leaf constructor's contract: arity=0, empty child
        // vec, exact kind preserved. Catches a refactor that
        // accidentally promoted leaf to "1 child of itself" or some
        // other clever shortcut that would shift the produced
        // base_vector.
        let fp = AstNodeFingerprint::leaf(CanonicalKind::Lit);
        assert_eq!(fp.canonical_kind, CanonicalKind::Lit);
        assert_eq!(fp.arity_bucket, 0);
        assert!(fp.child_canonical_kinds.is_empty());

        // Equivalence with the literal struct construction it replaces:
        let manual = AstNodeFingerprint {
            canonical_kind: CanonicalKind::Op,
            arity_bucket: 0,
            child_canonical_kinds: vec![],
        };
        let via_helper = AstNodeFingerprint::leaf(CanonicalKind::Op);
        // Vec<CanonicalKind> doesn't impl PartialEq directly via derive
        // (the enum does), so compare fields explicitly.
        assert_eq!(manual.canonical_kind, via_helper.canonical_kind);
        assert_eq!(manual.arity_bucket, via_helper.arity_bucket);
        assert_eq!(manual.child_canonical_kinds, via_helper.child_canonical_kinds);
    }
}

pub mod ast;
pub use ast::AstCodebook;

pub mod module;
pub use module::{encode_module, module_distance, ModuleCodebook};

pub mod semantic;
pub use semantic::{SemanticCodebook, SEMANTIC_HYPERPLANE_SEED};

pub mod temporal;
pub use temporal::{
    TemporalCodebook, TemporalCoEditMatrix, DEFAULT_TAU_SECONDS, TEMPORAL_HYPERPLANE_SEED,
};
