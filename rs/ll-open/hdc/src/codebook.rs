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
/// The encoder calls `base_vector(item)` for the node itself and
/// `role_vector(role_index)` for each child position. XOR-binding the
/// child's hypervector with its role vector encodes "this child fills
/// this slot." The same role index always produces the same vector
/// (deterministic), so unbind works.
pub trait BaseCodebook: Send + Sync {
    /// What this codebook accepts. For the AST codebook it's
    /// `AstNodeFingerprint`; for module/semantic/etc. it'll be
    /// per-layer types.
    type Item;

    /// Map the input fingerprint to a base hypervector. Must be a
    /// pure function of the input — same item, same vector, on
    /// every call, every machine, every version.
    fn base_vector(&self, item: &Self::Item) -> Hypervector;

    /// Map a child position to a role hypervector. Used to bind a
    /// child's hypervector to its slot. Like `base_vector`, must be
    /// a pure function. Different role indices must produce
    /// different vectors (otherwise positional information is lost).
    fn role_vector(&self, role_index: usize) -> Hypervector;
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
pub struct AstNodeFingerprint<'a> {
    pub canonical_kind: CanonicalKind,
    pub arity_bucket: u8,
    /// Canonical kinds of named children, sorted to be order-invariant
    /// at the codebook level. Order-sensitivity comes back via role
    /// permutation when the encoder XOR-binds each child's hypervector
    /// to its slot.
    pub child_canonical_kinds: &'a [CanonicalKind],
}

pub mod ast;
pub use ast::AstCodebook;
