//! BaseCodebook trait + AST-fingerprint type that the AST codebook consumes.
//!
//! The trait is generic over the input `Item` so per-layer codebooks (AST,
//! Module, Semantic, Temporal, HIR) can each define their own input shape
//! while sharing the same encoder + storage infrastructure downstream.
//!
//! See bead `ley-line-open-96b1a9` for the per-layer codebook plan.

use crate::canonical::CanonicalKind;
use crate::util::{blake3_seed, tagged_seed_vector, Hypervector, ZERO_HV};
use crate::D_BITS;

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
    /// Construct a fingerprint from its three fields. Mirrors the
    /// struct-init shape but eliminates the `canonical_kind: ...,
    /// arity_bucket: ..., child_canonical_kinds: ...` boilerplate at
    /// every call site (encoder::fingerprint, query::explain_cluster_
    /// centroid, codebook/ast tests).
    pub fn new(canonical_kind: CanonicalKind, arity_bucket: u8, child_canonical_kinds: Vec<CanonicalKind>) -> Self {
        Self {
            canonical_kind,
            arity_bucket,
            child_canonical_kinds,
        }
    }

    /// Construct a leaf-node fingerprint: zero arity, no children.
    /// Convenience for the common case where cleanup-memory probes
    /// "what kind would a leaf at this position be" — replaces the
    /// `AstNodeFingerprint { kind, 0, vec![] }` literal pattern.
    pub fn leaf(kind: CanonicalKind) -> Self {
        Self::new(kind, 0, Vec::new())
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
    let mut out = ZERO_HV;
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
    fn canonical_signature_bytes_format_pin() {
        // Pin the canonical byte layout shared by AstCodebook,
        // ModuleCodebook, and any future codebook that hashes
        // (kind, arity_bucket, sorted_child_kinds). Format:
        //   tag_bytes + 0x00 separator
        //   + kind_disc(1B) + arity(1B) + child_count_le(2B)
        //   + sorted_child_discs(N B).
        //
        // Sister test to ast.rs::signature_byte_format_pin which
        // pins the same format with the "hdc-ast" tag. This one
        // uses an arbitrary tag to verify the format is
        // tag-agnostic.
        let bytes = canonical_signature_bytes(
            "test-tag",
            CanonicalKind::Stmt,        // disc=2
            3,
            &[CanonicalKind::Op, CanonicalKind::Block, CanonicalKind::Op], // discs 6, 3, 6 → sorted 3, 6, 6
        );
        let mut expected: Vec<u8> = b"test-tag".to_vec();
        expected.push(0); // tag/payload separator
        expected.extend_from_slice(&[2u8, 3, 3, 0, 3, 6, 6]);
        assert_eq!(bytes, expected);
    }

    #[test]
    fn canonical_signature_bytes_distinct_for_distinct_tags() {
        // Same `(kind, arity, children)` with different tags must
        // produce different bytes — this is what makes AstCodebook's
        // base_vector distinct from ModuleCodebook's even when the
        // structural payload matches (skeptic-review bead 4bb8a0).
        let payload = (CanonicalKind::Decl, 2u8, vec![CanonicalKind::Block, CanonicalKind::Ref]);
        let ast_bytes = canonical_signature_bytes("hdc-ast", payload.0, payload.1, &payload.2);
        let module_bytes =
            canonical_signature_bytes("hdc-module", payload.0, payload.1, &payload.2);
        assert_ne!(ast_bytes, module_bytes);
    }

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
    fn simhash_signs_thresholds_at_zero_inclusive() {
        // simhash_signs sets bit i iff dot(plane_i) >= 0. The >= 0
        // (not strict >) is the load-bearing convention shared with
        // `project_zero_embedding_yields_all_ones`. Encode the bit
        // index in plane[0] so the closure (which is `Fn`) can read
        // it without interior mutability.
        // plane[0] = i as f32; closure returns sign based on i:
        //   i < 100   → +1.0 (set)
        //   100..200  → 0.0  (set, by >= 0 convention)
        //   i >= 200  → -1.0 (clear)
        let hyperplanes: Vec<Vec<f32>> = (0..D_BITS).map(|i| vec![i as f32]).collect();
        let signs = simhash_signs(&hyperplanes, |plane| {
            let i = plane[0] as usize;
            if i < 100 {
                1.0
            } else if i < 200 {
                0.0
            } else {
                -1.0
            }
        });
        // Bit 50 (positive) → 1
        assert_eq!((signs[50 / 8] >> (50 % 8)) & 1, 1);
        // Bit 150 (exactly 0, >= 0 → positive in our convention) → 1
        assert_eq!((signs[150 / 8] >> (150 % 8)) & 1, 1);
        // Bit 300 (negative) → 0
        assert_eq!((signs[300 / 8] >> (300 % 8)) & 1, 0);
    }

    #[test]
    fn simhash_signs_fewer_planes_than_d_bits_clears_remainder() {
        // If hyperplanes has fewer than D_BITS rows, only the first
        // N bits can be set; the remaining D_BITS - N bits stay 0
        // (the for-loop never visits them). Production callers always
        // supply D_BITS rows via build_hyperplane_matrix, but the
        // function's signature accepts any &[Vec<f32>]. A refactor
        // that filled the remainder with default-bits ("zero-pad
        // wraparound") would silently shift the high bits of the
        // output for sub-D inputs.
        let n = 100; // arbitrary, < D_BITS
        let hyperplanes: Vec<Vec<f32>> = (0..n).map(|_| vec![1.0]).collect();
        let signs = simhash_signs(&hyperplanes, |_| 1.0);
        // Bits 0..n: set (positive dot, >= 0).
        for bit in 0..n {
            assert_eq!(
                (signs[bit / 8] >> (bit % 8)) & 1,
                1,
                "bit {bit} (within plane range) must be set",
            );
        }
        // Bits n..D_BITS: must remain 0.
        for bit in n..D_BITS {
            assert_eq!(
                (signs[bit / 8] >> (bit % 8)) & 1,
                0,
                "bit {bit} (beyond plane range) must remain 0",
            );
        }
    }

    #[test]
    fn simhash_signs_all_negative_dots_yield_zero_hv() {
        // Sister pin to simhash_signs_thresholds_at_zero_inclusive
        // (which covers the >= 0 inclusive case via per-bit probes)
        // and project_zero_embedding_yields_all_ones (which covers
        // the all-zero-dot case). Closes the gap: when every dot
        // is strictly negative, no bit is set — output is the all-
        // zero hypervector. Catches a refactor that flipped the
        // sign convention to `dot < 0 sets bit` or that initialized
        // the accumulator to all-ones instead of ZERO_HV.
        let hyperplanes: Vec<Vec<f32>> = (0..D_BITS).map(|_| vec![0.0]).collect();
        let signs = simhash_signs(&hyperplanes, |_| -1.0);
        let ones: u32 = signs.iter().map(|b| b.count_ones()).sum();
        assert_eq!(ones, 0, "all-negative dots must yield ZERO_HV");
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

        // Equivalence with the explicit `::new(kind, 0, vec![])` form:
        // leaf must produce byte-identical fields.
        let manual = AstNodeFingerprint::new(CanonicalKind::Op, 0, vec![]);
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
