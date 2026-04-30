//! AST codebook: produces a base hypervector for an AST-node fingerprint
//! using Deckard-style production-signature hashing on the canonical-kind
//! alphabet.
//!
//! The seed for each base vector is `blake3((canonical_kind, arity_bucket,
//! sorted_child_kinds))`. SplitMix64 expands the seed to D bits. Two
//! functions with the same shape but different identifiers produce
//! identical hypervectors — that's the structural-equivalence-class
//! property the whole HDC stack rests on.

use crate::codebook::{AstNodeFingerprint, BaseCodebook};

#[cfg(test)]
use crate::canonical::CanonicalKind;
use crate::util::{blake3_seed, expand_seed, Hypervector};

/// Default AST codebook. Stateless — same input always produces same
/// output, no per-instance state to ship between machines.
pub struct AstCodebook;

impl Default for AstCodebook {
    fn default() -> Self {
        AstCodebook
    }
}

impl AstCodebook {
    /// Build the canonical-signature byte array for hashing. Format:
    /// `[kind_disc, arity_bucket, len_low, len_high, child_disc...]`.
    /// Sorted children → order-invariant signature; role-binding at the
    /// encoder restores order-sensitivity per slot.
    fn signature_bytes(item: &AstNodeFingerprint<'_>) -> Vec<u8> {
        let mut buf = Vec::with_capacity(4 + item.child_canonical_kinds.len());
        buf.push(item.canonical_kind.discriminant());
        buf.push(item.arity_bucket);
        // Length prefix so a (k, []) signature can never collide with
        // a (k, [k]) signature (defensive — unlikely with discriminants
        // in [0, 6] but cheap to be explicit).
        let len = item.child_canonical_kinds.len() as u16;
        buf.extend_from_slice(&len.to_le_bytes());
        let mut sorted: Vec<u8> = item
            .child_canonical_kinds
            .iter()
            .map(|k| k.discriminant())
            .collect();
        sorted.sort_unstable();
        buf.extend_from_slice(&sorted);
        buf
    }
}

impl BaseCodebook for AstCodebook {
    type Item = AstNodeFingerprint<'static>;

    fn base_vector(&self, item: &Self::Item) -> Hypervector {
        let bytes = Self::signature_bytes(item);
        let seed = blake3_seed(&bytes);
        expand_seed(seed)
    }

    fn role_vector(&self, role_index: usize) -> Hypervector {
        // Role vectors are independent of the codebook content — they
        // just need to be deterministic per index. Use a domain-tagged
        // hash so role-vectors never collide with base vectors.
        let bytes = format!("hdc-ast-role/{role_index}");
        let seed = blake3_seed(bytes.as_bytes());
        expand_seed(seed)
    }
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
    use crate::util::popcount_distance;

    fn fp(
        kind: CanonicalKind,
        arity: u8,
        children: &'static [CanonicalKind],
    ) -> AstNodeFingerprint<'static> {
        AstNodeFingerprint {
            canonical_kind: kind,
            arity_bucket: arity,
            child_canonical_kinds: children,
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
        let dist = popcount_distance(&cb.base_vector(&f_stmt), &cb.base_vector(&f_expr));
        assert!(
            dist > 3500,
            "Stmt vs Expr distance {dist} suspiciously low (expected ~4096 ± 200)",
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
        let d = popcount_distance(&cb.base_vector(&f1), &cb.base_vector(&f2));
        assert!(d > 3500, "arity 2 vs 3 distance {d} suspiciously low");
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
        let d = popcount_distance(&r0, &r1);
        assert!(d > 3500, "role 0 vs 1 distance {d} too small");
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
        let d = popcount_distance(&base, &role);
        assert!(d > 3500, "base/role collision distance {d} too small");
    }

    #[test]
    fn signature_byte_format_pin() {
        // Pin the signature byte layout — changing it breaks every
        // existing encoded vector. Format: kind_disc (1B), arity (1B),
        // child_count_le (2B), sorted_child_discs (NB).
        let f = fp(
            CanonicalKind::Stmt,         // disc=2
            3,
            &[CanonicalKind::Op, CanonicalKind::Block, CanonicalKind::Op], // discs 6, 3, 6 → sorted 3, 6, 6
        );
        let bytes = AstCodebook::signature_bytes(&f);
        assert_eq!(bytes, vec![2u8, 3, 3, 0, 3, 6, 6]);
    }
}
