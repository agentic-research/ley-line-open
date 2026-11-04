//! Canonical binding that a signed `Head` commits to (workstreams S1, b68fa6).
//!
//! [`crate::substrate::RootSigner`] signs a [`Hash`] — a single content
//! address. A `Head` is not a single hash: it is a claim made of several
//! identities from incommensurable domains (ADR-0032 D3), so the signable
//! object is a digest that binds every identity the claim makes.
//!
//! A [`HeadClaim`] binds five fields:
//!
//! - `generation` — without it a signature replays at another generation;
//! - `rootHash` — the run receipt: the tagged fold over the segments one
//!   parse run wrote;
//! - `parentHash` — the previous run receipt ([`Hash::ZERO`] for the first
//!   head, the convention `head.capnp` documents); without it a signed head
//!   grafts onto a forked chain;
//! - `treeRoot` — the tree identity (ADR-0040 D1), the root that signatures
//!   cite;
//! - `parserId` — the derivation `treeRoot` was computed under.
//!
//! Canonical bytes (v2): `BLAKE3_derive_key(HEAD_DIGEST_CONTEXT,
//! generation_le8 ‖ rootHash ‖ parentHash ‖ treeRoot ‖ parserId)`.
//! Every field is fixed width, so the concatenation is injective without
//! length prefixes. Little-endian is fixed by this module and must not vary
//! by host.
//!
//! **Absence is unrepresentable.** A claim cannot be built without a stamped
//! `treeRoot` and `parserId`: [`HeadClaim::new`] returns `None` when either
//! is [`Hash::ZERO`], the value an unstamped head reads as. A head that does
//! not carry both cannot be signed and cannot verify; there is no ZERO-filled
//! "not stamped" digest to confuse with a stamped one.
//!
//! **Version history.** v1 (`"leyline head digest v1"`) bound only
//! `generation ‖ rootHash ‖ parentHash`, so a head whose `treeRoot` or
//! `parserId` was rewritten after signing still verified. v1 is removed, not
//! deprecated: nothing in the tree computes it, and a v1 signature cannot
//! verify under v2 because the derive-key context differs. The context string
//! is the version discriminator; it is inside the preimage, so it cannot be
//! downgraded by editing an unsigned field.
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
/// signature ever produced, so a change means a new version, not an edit.
pub const HEAD_DIGEST_CONTEXT: &str = "leyline head digest v2";

/// Every identity a signed `Head` attests, in one value.
///
/// Fields are private so the only constructor is [`HeadClaim::new`], which
/// refuses an unstamped `treeRoot` or `parserId`. [`head_digest`]
/// destructures the claim exhaustively, so a field added here does not
/// compile until the digest takes a position on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadClaim {
    generation: u64,
    root_hash: Hash,
    parent_hash: Hash,
    tree_root: Hash,
    parser_id: Hash,
}

impl HeadClaim {
    /// Build a claim. `None` when `tree_root` or `parser_id` is
    /// [`Hash::ZERO`]: an unstamped head attests no tree, so it has no claim
    /// to sign. `parent_hash` is [`Hash::ZERO`] for the first head.
    pub fn new(
        generation: u64,
        root_hash: Hash,
        parent_hash: Hash,
        tree_root: Hash,
        parser_id: Hash,
    ) -> Option<Self> {
        if tree_root == Hash::ZERO || parser_id == Hash::ZERO {
            return None;
        }
        Some(Self {
            generation,
            root_hash,
            parent_hash,
            tree_root,
            parser_id,
        })
    }
}

/// Compute the canonical v2 digest a signed `Head` commits to.
pub fn head_digest(claim: &HeadClaim) -> Hash {
    // Exhaustive on purpose: no `..` rest pattern.
    let HeadClaim {
        generation,
        root_hash,
        parent_hash,
        tree_root,
        parser_id,
    } = *claim;
    let mut hasher = blake3::Hasher::new_derive_key(HEAD_DIGEST_CONTEXT);
    hasher.update(&generation.to_le_bytes());
    hasher.update(root_hash.as_bytes());
    hasher.update(parent_hash.as_bytes());
    hasher.update(tree_root.as_bytes());
    hasher.update(parser_id.as_bytes());
    Hash::from_bytes(*hasher.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const R1: Hash = Hash::from_bytes([1u8; 32]);
    const R2: Hash = Hash::from_bytes([9u8; 32]);
    const P1: Hash = Hash::from_bytes([2u8; 32]);
    const P2: Hash = Hash::from_bytes([3u8; 32]);
    const T1: Hash = Hash::from_bytes([4u8; 32]);
    const T2: Hash = Hash::from_bytes([5u8; 32]);
    const I1: Hash = Hash::from_bytes([6u8; 32]);
    const I2: Hash = Hash::from_bytes([7u8; 32]);

    fn claim(generation: u64, root: Hash, parent: Hash, tree: Hash, parser: Hash) -> HeadClaim {
        HeadClaim::new(generation, root, parent, tree, parser).expect("stamped claim")
    }

    fn d(generation: u64, root: Hash, parent: Hash, tree: Hash, parser: Hash) -> Hash {
        head_digest(&claim(generation, root, parent, tree, parser))
    }

    fn hex(h: Hash) -> String {
        h.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Cross-runtime test vector: another runtime that verifies a head must
    /// reproduce these bytes. Inputs: generation 7, rootHash = 0x01×32,
    /// parentHash = 0x02×32, treeRoot = 0x04×32, parserId = 0x06×32.
    /// Recomputed below from the spec with `blake3::derive_key` over the
    /// literal preimage, and pinned as hex so a change to the construction
    /// fails here.
    #[test]
    fn digest_v2_test_vector() {
        let got = d(7, R1, P1, T1, I1);
        let mut preimage = Vec::with_capacity(8 + 4 * 32);
        preimage.extend_from_slice(&7u64.to_le_bytes());
        for byte in [1u8, 2, 4, 6] {
            preimage.extend_from_slice(&[byte; 32]);
        }
        let independent = blake3::derive_key("leyline head digest v2", &preimage);
        assert_eq!(got.as_bytes(), &independent);
        assert_eq!(
            hex(got),
            "74bf1248d561ec53004da0a9b98d769d72d2db7b07b036d31479d1c77227fee2"
        );
    }

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
        plain.update(T1.as_bytes());
        plain.update(I1.as_bytes());
        assert_ne!(
            d(7, R1, Hash::ZERO, T1, I1).as_bytes(),
            plain.finalize().as_bytes(),
            "head digest must be tagged, not a bare hash of the fields"
        );
    }

    /// A v1 signature must not verify as a v2 claim: the v1 preimage over the
    /// same leading fields, under the v1 context, is a different digest.
    #[test]
    fn digest_v2_differs_from_the_removed_v1_construction() {
        let mut v1 = blake3::Hasher::new_derive_key("leyline head digest v1");
        v1.update(&7u64.to_le_bytes());
        v1.update(R1.as_bytes());
        v1.update(P1.as_bytes());
        assert_ne!(d(7, R1, P1, T1, I1).as_bytes(), v1.finalize().as_bytes());
    }

    /// The context string is protocol-visible: changing it invalidates every
    /// existing signature, so it is pinned here as a canonical constant.
    #[test]
    fn digest_context_is_pinned() {
        assert_eq!(HEAD_DIGEST_CONTEXT, "leyline head digest v2");
    }

    /// Without this, a signature over generation N replays at generation M —
    /// an attacker re-publishes an old world as the current one.
    #[test]
    fn digest_binds_generation() {
        assert_ne!(d(1, R1, P1, T1, I1), d(2, R1, P1, T1, I1));
    }

    /// The run receipt must be covered.
    #[test]
    fn digest_binds_root() {
        assert_ne!(d(1, R1, P1, T1, I1), d(1, R2, P1, T1, I1));
    }

    /// Without this, a signed head grafts onto a forked chain: same root and
    /// generation, different history.
    #[test]
    fn digest_binds_parent() {
        assert_ne!(d(1, R1, P1, T1, I1), d(1, R1, P2, T1, I1));
    }

    /// ADR-0040 D1: treeRoot is the root signatures cite, so the signature
    /// must cover it (bead b68fa6).
    #[test]
    fn digest_binds_tree_root() {
        assert_ne!(d(1, R1, P1, T1, I1), d(1, R1, P1, T2, I1));
    }

    /// The same tree under another derivation is another claim.
    #[test]
    fn digest_binds_parser_id() {
        assert_ne!(d(1, R1, P1, T1, I1), d(1, R1, P1, T1, I2));
    }

    /// Field order is part of the construction: swapping treeRoot and
    /// parserId is a different claim.
    #[test]
    fn digest_is_positional() {
        assert_ne!(d(1, R1, P1, T1, I1), d(1, R1, P1, I1, T1));
    }

    /// Verification re-derives the digest, so it must be stable.
    #[test]
    fn digest_is_deterministic() {
        assert_eq!(d(42, R1, P1, T1, I1), d(42, R1, P1, T1, I1));
    }

    /// A first head (parent = ZERO) is still a distinct, non-zero commitment.
    #[test]
    fn digest_of_first_head_is_not_zero() {
        assert_ne!(d(1, R1, Hash::ZERO, T1, I1), Hash::ZERO);
    }

    /// Absence is explicit: an unstamped treeRoot or parserId yields no
    /// claim, never a ZERO-filled digest.
    #[test]
    fn an_unstamped_tree_root_or_parser_id_makes_no_claim() {
        assert_eq!(HeadClaim::new(1, R1, P1, Hash::ZERO, I1), None);
        assert_eq!(HeadClaim::new(1, R1, P1, T1, Hash::ZERO), None);
        assert_eq!(HeadClaim::new(1, R1, P1, Hash::ZERO, Hash::ZERO), None);
        assert!(HeadClaim::new(1, R1, Hash::ZERO, T1, I1).is_some());
    }
}
