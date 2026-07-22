//! Canonical binding that a signed `Head` commits to (workstream S1).
//!
//! [`crate::substrate::RootSigner`] signs a [`Hash`] — a single content
//! address. A `Head` is not a single hash: it is the triple
//! `(generation, rootHash, parentHash)`. Signing `rootHash` alone would let
//! a signature be **replayed at a different generation** or **grafted onto a
//! different parent**, so the signable object is a digest that binds all
//! three.
//!
//! The construction mirrors the frozen `leyline-net/v1` manifest, which
//! signs over the canonical concatenation `sequence LE-8 ‖ contentHash`;
//! this extends the same shape with the parent link so the chain — not just
//! the epoch — is covered.
//!
//! Canonical bytes: `BLAKE3_derive_key(HEAD_DIGEST_CONTEXT,
//! generation_le8 ‖ rootHash ‖ parentHash)`.
//! Little-endian is fixed by this module and must not vary by host.
//!
//! **Domain separation.** The digest is keyed by [`HEAD_DIGEST_CONTEXT`]
//! rather than being a plain BLAKE3. [`crate::substrate::RootSigner::sign`]
//! accepts a bare 32-byte [`Hash`] with no notion of what it addresses, so an
//! untagged head digest would be indistinguishable from a content address:
//! one signature would read equally as "this head is authentic" and as "the
//! arena root is this value". The tag makes those two claims unforgeable in
//! each other's context under a shared key.

use crate::substrate::Hash;

/// Domain-separation context for the head digest.
///
/// Protocol-visible and versioned: changing this string invalidates every
/// signature ever produced, so a change means `v2`, not an edit.
pub const HEAD_DIGEST_CONTEXT: &str = "leyline head digest v1";

/// Compute the canonical digest a signed `Head` commits to.
///
/// `parent` is [`Hash::ZERO`] for the first head in a chain.
pub fn head_digest(generation: u64, root: Hash, parent: Hash) -> Hash {
    let mut hasher = blake3::Hasher::new_derive_key(HEAD_DIGEST_CONTEXT);
    hasher.update(&generation.to_le_bytes());
    hasher.update(root.as_bytes());
    hasher.update(parent.as_bytes());
    Hash::from_bytes(*hasher.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Domain separation: the head digest must not collide with a plain
    /// BLAKE3 over the same concatenation. `RootSigner::sign` takes a bare
    /// 32-byte `Hash` whatever its provenance, so without a context tag a
    /// head signature would be bit-identical to a bare-root signature
    /// asserting `root == digest` — the same key, two meanings.
    #[test]
    fn digest_is_domain_separated_from_plain_blake3() {
        let mut plain = blake3::Hasher::new();
        plain.update(&7u64.to_le_bytes());
        plain.update(R1.as_bytes());
        plain.update(Hash::ZERO.as_bytes());
        assert_ne!(
            head_digest(7, R1, Hash::ZERO).as_bytes(),
            plain.finalize().as_bytes(),
            "head digest must be tagged, not a bare hash of the fields"
        );
    }

    /// The context string is protocol-visible: changing it invalidates every
    /// existing signature, so it is pinned here as a canonical constant.
    #[test]
    fn digest_context_is_pinned() {
        assert_eq!(HEAD_DIGEST_CONTEXT, "leyline head digest v1");
    }

    const R1: Hash = Hash::from_bytes([1u8; 32]);
    const R2: Hash = Hash::from_bytes([9u8; 32]);
    const P1: Hash = Hash::from_bytes([2u8; 32]);
    const P2: Hash = Hash::from_bytes([3u8; 32]);

    /// Without this, a signature over generation N replays at generation M —
    /// an attacker re-publishes an old world as the current one.
    #[test]
    fn digest_binds_generation() {
        assert_ne!(head_digest(1, R1, P1), head_digest(2, R1, P1));
    }

    /// The root is the world identity; the digest must obviously cover it.
    #[test]
    fn digest_binds_root() {
        assert_ne!(head_digest(1, R1, P1), head_digest(1, R2, P1));
    }

    /// Without this, a signed head grafts onto a forked chain: same root and
    /// generation, different history.
    #[test]
    fn digest_binds_parent() {
        assert_ne!(head_digest(1, R1, P1), head_digest(1, R1, P2));
    }

    /// Verification re-derives the digest, so it must be stable.
    #[test]
    fn digest_is_deterministic() {
        assert_eq!(head_digest(42, R1, P1), head_digest(42, R1, P1));
    }

    /// A first head (parent = ZERO) is still a distinct, non-zero commitment.
    #[test]
    fn digest_of_first_head_is_not_zero() {
        assert_ne!(head_digest(1, R1, Hash::ZERO), Hash::ZERO);
    }
}
