//! BaseCodebook trait + AST-fingerprint type that the AST codebook consumes.
//!
//! The trait is generic over the input `Item` so per-layer codebooks (AST,
//! Module, Semantic, Temporal, HIR) can each define their own input shape
//! while sharing the same encoder + storage infrastructure downstream.
//!
//! See bead `ley-line-open-96b1a9` for the per-layer codebook plan.

use crate::canonical::CanonicalKind;
use crate::util::Hypervector;

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
pub trait BaseCodebook: Send + Sync {
    /// What this codebook accepts. For the AST codebook it's
    /// `AstNodeFingerprint`; for module/semantic/etc. it'll be
    /// per-layer types.
    type Item;

    /// Map the input fingerprint to a base hypervector. Must be a
    /// pure function of the input — same item, same vector, on
    /// every call, every machine, every version.
    fn base_vector(&self, item: &Self::Item) -> Hypervector;

    /// Map a logical role to a role hypervector. Used by codebooks that
    /// need non-positional binding (e.g. layer-tagged combined view,
    /// module method-set vs field-set). The AST encoder doesn't call
    /// this — positional encoding is via permutation. Default impl
    /// uses the generic `"hdc-role"` tag; impls override with their
    /// own tag (e.g. `"hdc-ast-role"`) to avoid cross-codebook
    /// collisions.
    fn role_vector(&self, role_index: usize) -> Hypervector {
        crate::util::tagged_seed_vector("hdc-role", role_index)
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

/// Canonical byte layout for a node's structural signature, used by
/// every codebook that hashes "(kind, arity bucket, sorted child kinds)"
/// — currently AstCodebook and ModuleCodebook. Format:
///
///   `[kind_disc(1), arity_bucket(1), child_count_le(2), sorted_child_discs(N)]`
///
/// The length prefix prevents `(k, [])` from colliding with `(k, [k])`
/// (both would otherwise hash to the same bytes if a sub-pattern crosses
/// the boundary). Sorted children → order-invariant at the codebook
/// level; encoder restores positional order via rotation.
///
/// CHANGING THIS BREAKS EVERY ENCODED HYPERVECTOR. Bump a layer's seed
/// tag if you need to migrate, don't silently rewrite the format.
pub fn canonical_signature_bytes(
    kind: crate::canonical::CanonicalKind,
    arity_bucket: u8,
    child_kinds: &[crate::canonical::CanonicalKind],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 + child_kinds.len());
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
pub fn simhash_signs<F>(hyperplanes: &[Vec<f32>], dot: F) -> crate::util::Hypervector
where
    F: Fn(&[f32]) -> f64,
{
    let mut out = [0u8; crate::D_BYTES];
    for (i, plane) in hyperplanes.iter().enumerate().take(crate::D_BITS) {
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
    let base_seed = crate::util::blake3_seed(seed_tag.as_bytes());
    let mut hyperplanes = Vec::with_capacity(crate::D_BITS);
    for i in 0..crate::D_BITS {
        // Each hyperplane gets its own seed derived from base + index.
        // Box-Muller uses two uniforms per Gaussian; per-row PRNG state
        // keeps rows independent.
        let row_seed = crate::util::blake3_seed(
            &[
                base_seed.to_le_bytes().as_slice(),
                (i as u64).to_le_bytes().as_slice(),
            ]
            .concat(),
        );
        hyperplanes.push(crate::codebook::semantic::gaussian_row(row_seed, width));
    }
    hyperplanes
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
