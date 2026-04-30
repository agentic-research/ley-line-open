//! AST codebook: produces a base hypervector for an AST-node fingerprint
//! using Deckard-style production-signature hashing on the canonical-kind
//! alphabet.
//!
//! The seed for each base vector is `blake3((canonical_kind, arity_bucket,
//! sorted_child_kinds))`. SplitMix64 expands the seed to D bits. Two
//! functions with the same shape but different identifiers produce
//! identical hypervectors — that's the structural-equivalence-class
//! property the whole HDC stack rests on.

use crate::codebook::{canonical_signature_bytes, AstNodeFingerprint, BaseCodebook};

#[cfg(test)]
use crate::canonical::CanonicalKind;
#[cfg(test)]
use crate::util::assert_far_apart;
use crate::util::{bytes_to_hv, Hypervector};

/// Default AST codebook. Stateless — same input always produces same
/// output, no per-instance state to ship between machines.
pub struct AstCodebook;

impl Default for AstCodebook {
    fn default() -> Self {
        AstCodebook
    }
}

impl AstCodebook {
    /// Wrapper for the shared `codebook::canonical_signature_bytes` —
    /// kept as a method so the test that pins the byte layout
    /// (`signature_byte_format_pin`) doesn't have to know which crate
    /// module owns the canonical format.
    fn signature_bytes(item: &AstNodeFingerprint) -> Vec<u8> {
        canonical_signature_bytes(
            "hdc-ast",
            item.canonical_kind,
            item.arity_bucket,
            &item.child_canonical_kinds,
        )
    }
}

impl BaseCodebook for AstCodebook {
    type Item = AstNodeFingerprint;

    fn codebook_tag(&self) -> &'static str {
        "hdc-ast"
    }

    fn base_vector(&self, item: &Self::Item) -> Hypervector {
        bytes_to_hv(&Self::signature_bytes(item))
    }

    // role_vector: uses the trait default (codebook_tag + "-role").
    // Default produces tag "hdc-ast-role" — byte-identical to the
    // previous explicit override (skeptic 4bbc54 dedup).
}

/// Convenience constructor for tests / examples — same as `Default`.
impl AstCodebook {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(
        kind: CanonicalKind,
        arity: u8,
        children: &[CanonicalKind],
    ) -> AstNodeFingerprint {
        AstNodeFingerprint {
            canonical_kind: kind,
            arity_bucket: arity,
            child_canonical_kinds: children.to_vec(),
        }
    }

    #[test]
    fn same_fingerprint_same_vector() {
        // The deterministic-encoding property: identical signatures
        // produce identical hypervectors. Cross-machine reproducibility
        // depends on this.
        let cb = AstCodebook::new();
        let f1 = fp(CanonicalKind::Stmt, 2, &[CanonicalKind::Op, CanonicalKind::Block]);
        let f2 = fp(CanonicalKind::Stmt, 2, &[CanonicalKind::Op, CanonicalKind::Block]);
        assert_eq!(cb.base_vector(&f1), cb.base_vector(&f2));
    }

    #[test]
    fn different_kind_different_vector() {
        // Different canonical-kind must yield far-apart vectors. If the
        // hash collapsed Stmt and Expr to the same seed (because of a
        // refactor that dropped the kind from the signature), we'd
        // silently lose the kind dimension of the equivalence class.
        let cb = AstCodebook::new();
        let f_stmt = fp(CanonicalKind::Stmt, 0, &[]);
        let f_expr = fp(CanonicalKind::Expr, 0, &[]);
        assert_far_apart(
            &cb.base_vector(&f_stmt),
            &cb.base_vector(&f_expr),
            "Stmt vs Expr base vectors",
        );
    }

    #[test]
    fn child_order_does_not_change_base_vector() {
        // Order invariance at the codebook level — the encoder restores
        // order via role-permutation. If we accidentally became
        // order-sensitive at the codebook, a node with (Op, Block)
        // children would differ from one with (Block, Op), but we want
        // them to share the *base* vector (their structural template
        // is the same; only their role-binding differs).
        let cb = AstCodebook::new();
        let f1 = fp(CanonicalKind::Stmt, 2, &[CanonicalKind::Op, CanonicalKind::Block]);
        let f2 = fp(CanonicalKind::Stmt, 2, &[CanonicalKind::Block, CanonicalKind::Op]);
        assert_eq!(cb.base_vector(&f1), cb.base_vector(&f2));
    }

    #[test]
    fn arity_change_changes_vector() {
        // arity_bucket is part of the signature — different bucket
        // values must produce different base vectors. If a refactor
        // drops arity from the signature, an `if x { y }` and an
        // `if x { y } else { z }` would silently merge into the
        // same equivalence class.
        let cb = AstCodebook::new();
        let f1 = fp(CanonicalKind::Stmt, 2, &[CanonicalKind::Op, CanonicalKind::Block]);
        let f2 = fp(CanonicalKind::Stmt, 3, &[CanonicalKind::Op, CanonicalKind::Block]);
        assert_far_apart(
            &cb.base_vector(&f1),
            &cb.base_vector(&f2),
            "arity 2 vs 3 base vectors",
        );
    }

    #[test]
    fn role_vectors_are_distinct_per_index() {
        // Role vectors must be deterministically per-index AND
        // distinct across indices. If two indices collided, child
        // positions would mix at unbind time.
        let cb = AstCodebook::new();
        let r0 = cb.role_vector(0);
        let r0_again = cb.role_vector(0);
        let r1 = cb.role_vector(1);
        assert_eq!(r0, r0_again, "role_vector must be deterministic per index");
        assert_far_apart(&r0, &r1, "role 0 vs 1");
    }

    #[test]
    fn role_vector_default_uses_codebook_tag_plus_role_suffix() {
        // Skeptic 4bbc54: deleted the explicit override on AstCodebook,
        // relying on the trait default to derive "hdc-ast-role" from
        // codebook_tag(). Pin that the default produces byte-identical
        // output to the previous `tagged_seed_vector("hdc-ast-role", N)`
        // form so a future refactor of the default suffix scheme would
        // catch breakage immediately.
        use crate::util::tagged_seed_vector;
        let cb = AstCodebook::new();
        for i in [0usize, 1, 7, 42, 1024] {
            let actual = cb.role_vector(i);
            let expected = tagged_seed_vector("hdc-ast-role", i);
            assert_eq!(
                actual, expected,
                "role_vector({i}) must match tagged_seed_vector(\"hdc-ast-role\", {i})"
            );
        }
    }

    #[test]
    fn role_vector_does_not_collide_with_base_vector() {
        // Defensive: if someone refactors role_vector to use the same
        // hash domain as base_vector, an unbind followed by cleanup-
        // memory could match a role vector to a kind. Domain-tag
        // strings ("hdc-ast-role/N") prevent that. Pin the property.
        let cb = AstCodebook::new();
        // base_vector for the simplest signature
        let base = cb.base_vector(&fp(CanonicalKind::Stmt, 0, &[]));
        let role = cb.role_vector(0);
        assert_far_apart(&base, &role, "base vs role-0 must not collide");
    }

    #[test]
    fn signature_byte_format_pin() {
        // Pin the signature byte layout — changing it breaks every
        // existing encoded vector. Format:
        //   tag_bytes ("hdc-ast") + 0x00 separator
        //   + kind_disc (1B) + arity (1B) + child_count_le (2B)
        //   + sorted_child_discs (NB).
        let f = fp(
            CanonicalKind::Stmt,         // disc=2
            3,
            &[CanonicalKind::Op, CanonicalKind::Block, CanonicalKind::Op], // discs 6, 3, 6 → sorted 3, 6, 6
        );
        let bytes = AstCodebook::signature_bytes(&f);
        let mut expected: Vec<u8> = b"hdc-ast".to_vec();
        expected.push(0); // tag/payload separator
        expected.extend_from_slice(&[2u8, 3, 3, 0, 3, 6, 6]);
        assert_eq!(bytes, expected);
    }
}
