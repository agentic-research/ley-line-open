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

pub mod ast;
pub use ast::AstCodebook;
