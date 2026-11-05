//! Σ indexed by construction: `Ref<D>` (ADR-0047 §1 and §2.3, step M0, bead
//! `ley-line-open-10d71a`).
//!
//! One value has one address per *construction*: a file's bytes have a
//! `blob_hash`, a CDC manifest address and an AST root `node_hash`. A bare
//! [`Hash`] carries all of them, so every cross-construction mix-up
//! compiles. A [`Ref<D>`] is the address of a value under one construction
//! `D`, and the type keeps the three laws of ADR-0047 §1.2 where the compiler
//! can see them:
//!
//! - **(a)** within a construction, address equality is value equality:
//!   `Ref<D>: Eq` compares addresses of one construction;
//! - **(b)** across constructions, addresses are never compared:
//!   `Ref<A> == Ref<B>` for `A ≠ B` does not compile, and no conversion from
//!   one construction's ref to another's exists;
//! - **(c)** construction separation is a property of the bytes, not of the
//!   type: every construction except σ_bytes is keyed (ADR-0047 D-i). The
//!   type cannot give (c) to a peer that receives 32 bare bytes, which is why
//!   `node_hash` and `run_id` still migrate (M2, M3).
//!
//! **Minted only beside the fold.** The one constructor is
//! [`Ref::derive`], which runs `D`'s fold over a witness. A claimed 32-byte
//! address becomes a `Ref` only through [`Ref::verified`], which re-derives
//! it from a witness and compares. There is no `from_bytes`, no `ZERO`, and
//! no `From<Hash>`: absence is `Option<Ref<D>>`, and a value read off a wire
//! or a table stays a [`Hash`] until a witness vouches for it. Only the crate
//! that defines a construction's marker type can implement [`Construction`]
//! for it (the orphan rule), so `Ref<D>` for a given `D` is only ever minted
//! by `D`'s own fold.
//!
//! **Nothing moves.** A `Ref<D>` is `repr(transparent)` over [`Hash`], and
//! every fold here or in a downstream crate delegates to the function that
//! computed the untyped address before this module existed; the known-answer
//! vectors in `schema-spec/sigma/v1` pin that. Wire and storage types keep
//! [`Hash`] (ADR-0047 §2.3: existing `Common.Hash` ordinals stay put behind
//! typed accessors); [`Ref::as_hash`] is the encoding boundary.
//!
//! # Law (b), as compile-fail tests
//!
//! The positive control: refs of one construction compare.
//!
//! ```
//! use leyline_core::construction::{Blob, Ref};
//! assert_eq!(Ref::<Blob>::derive(b"x"), Ref::<Blob>::derive(b"x"));
//! assert_ne!(Ref::<Blob>::derive(b"x"), Ref::<Blob>::derive(b"y"));
//! ```
//!
//! Refs of two constructions do not, even over the same bytes:
//!
//! ```compile_fail,E0308
//! use leyline_core::construction::{ArenaImage, Blob, Ref};
//! let _ = Ref::<Blob>::derive(b"x") == Ref::<ArenaImage>::derive(b"x");
//! ```
//!
//! No conversion exists between constructions:
//!
//! ```compile_fail,E0277
//! use leyline_core::construction::{ArenaImage, Blob, Ref};
//! let _: Ref<ArenaImage> = Ref::<Blob>::derive(b"x").into();
//! ```
//!
//! A bare hash is not a ref:
//!
//! ```compile_fail,E0277
//! use leyline_core::construction::{Blob, Ref};
//! let _: Ref<Blob> = leyline_core::Hash::from_bytes([1; 32]).into();
//! ```
//!
//! There is no `from_bytes` and no `ZERO`:
//!
//! ```compile_fail,E0599
//! use leyline_core::construction::{Blob, Ref};
//! let _ = Ref::<Blob>::from_bytes([0; 32]);
//! ```
//!
//! ```compile_fail,E0599
//! use leyline_core::construction::{Blob, Ref};
//! let _ = Ref::<Blob>::ZERO;
//! ```

use std::marker::PhantomData;

use crate::head_digest::{HeadClaim, head_digest};
use crate::substrate::{ContentAddressed, Hash};

/// A declared construction `D`: what a `Ref<D>` addresses, and the fold that
/// computes the address from a witness (ADR-0047 §2.1).
pub trait Construction: 'static {
    /// The construction's name, for diagnostics only. Never hashed.
    const NAME: &'static str;

    /// What the fold reads: the value, or enough of it to recompute the
    /// address (ADR-0047 §2.1's `Witness<D>`).
    type Witness<'w>;

    /// `σ_D`: the address of the witnessed value under this construction.
    fn fold(witness: Self::Witness<'_>) -> Hash;
}

/// The address of a value under construction `D`.
///
/// `repr(transparent)` over [`Hash`]; `D` is a phantom. See the module docs
/// for what does not exist on purpose.
#[repr(transparent)]
pub struct Ref<D: Construction> {
    addr: Hash,
    construction: PhantomData<fn() -> D>,
}

impl<D: Construction> Ref<D> {
    /// The address of the witnessed value: `D`'s fold, the only way a
    /// `Ref<D>` comes to exist.
    pub fn derive(witness: D::Witness<'_>) -> Self {
        Ref {
            addr: D::fold(witness),
            construction: PhantomData,
        }
    }

    /// `claimed` as a `Ref<D>` when the witness re-derives exactly it, else
    /// `None`. Parse, don't validate: a claimed address read from a head, a
    /// table or a peer becomes typed only here.
    pub fn verified(claimed: Hash, witness: D::Witness<'_>) -> Option<Self> {
        let derived = Self::derive(witness);
        (derived.addr == claimed).then_some(derived)
    }

    /// `verify(r, w) ⇔ σ_D(w) = r` (ADR-0047 §2.1).
    pub fn verify(&self, witness: D::Witness<'_>) -> bool {
        D::fold(witness) == self.addr
    }

    /// The 32-byte address, for encoding into a wire or storage field, or
    /// for a signer. Leaving the type is explicit; nothing converts back.
    pub const fn as_hash(&self) -> &Hash {
        &self.addr
    }
}

impl<D: Construction> Clone for Ref<D> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<D: Construction> Copy for Ref<D> {}

impl<D: Construction> PartialEq for Ref<D> {
    fn eq(&self, other: &Self) -> bool {
        self.addr == other.addr
    }
}

impl<D: Construction> Eq for Ref<D> {}

impl<D: Construction> PartialOrd for Ref<D> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<D: Construction> Ord for Ref<D> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.addr.cmp(&other.addr)
    }
}

impl<D: Construction> std::hash::Hash for Ref<D> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.addr.hash(state);
    }
}

impl<D: Construction> std::fmt::Debug for Ref<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Ref<{}>({:?})", D::NAME, self.addr)
    }
}

impl<D: Construction> std::fmt::Display for Ref<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.addr, f)
    }
}

/// σ_bytes over a source file's bytes: `blob_hash` (`source_blobs`, `_tree`).
pub enum Blob {}

impl Construction for Blob {
    const NAME: &'static str = "blob";
    type Witness<'w> = &'w [u8];
    fn fold(bytes: &[u8]) -> Hash {
        bytes.hash()
    }
}

/// σ_bytes over one CDC chunk's bytes (`content_chunks`). The same function
/// as [`Blob`] and a different construction: a sub-floor blob's single chunk
/// has the blob's address, lawfully (ADR-0047 §3), and the type says which
/// one a slot holds.
pub enum Chunk {}

impl Construction for Chunk {
    const NAME: &'static str = "chunk";
    type Witness<'w> = &'w [u8];
    fn fold(bytes: &[u8]) -> Hash {
        bytes.hash()
    }
}

/// σ_bytes over one serialized arena image: `Controller.current_root`.
pub enum ArenaImage {}

impl Construction for ArenaImage {
    const NAME: &'static str = "arena-image";
    type Witness<'w> = &'w [u8];
    fn fold(image: &[u8]) -> Hash {
        image.hash()
    }
}

/// The head digest v2 a signed `Head` commits to ([`head_digest`]).
pub enum HeadDigest {}

impl Construction for HeadDigest {
    const NAME: &'static str = "head-digest";
    type Witness<'w> = &'w HeadClaim;
    fn fold(claim: &HeadClaim) -> Hash {
        head_digest(claim)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAIM: (u64, Hash, Hash) = (3, Hash::from_bytes([1; 32]), Hash::from_bytes([2; 32]));

    fn claim() -> HeadClaim {
        HeadClaim::new(
            CLAIM.0,
            CLAIM.1,
            CLAIM.2,
            None,
            Hash::from_bytes([4; 32]),
            Hash::from_bytes([5; 32]),
        )
        .unwrap()
    }

    /// `Ref<D>` is the `Hash` it wraps: same size, same alignment.
    #[test]
    fn ref_is_transparent_over_hash() {
        assert_eq!(std::mem::size_of::<Ref<Blob>>(), 32);
        assert_eq!(std::mem::align_of::<Ref<Blob>>(), 1);
    }

    /// Nothing moves: every core construction's fold is the untyped
    /// function it names.
    #[test]
    fn every_fold_is_the_untyped_function() {
        for bytes in [&b""[..], b"x", &[0xff; 70_000]] {
            assert_eq!(*Ref::<Blob>::derive(bytes).as_hash(), bytes.hash());
            assert_eq!(*Ref::<Chunk>::derive(bytes).as_hash(), bytes.hash());
            assert_eq!(*Ref::<ArenaImage>::derive(bytes).as_hash(), bytes.hash());
        }
        let c = claim();
        assert_eq!(*Ref::<HeadDigest>::derive(&c).as_hash(), head_digest(&c));
    }

    #[test]
    fn verified_accepts_exactly_the_derived_address() {
        let r = Ref::<Blob>::derive(b"bytes");
        assert_eq!(Ref::<Blob>::verified(*r.as_hash(), b"bytes"), Some(r));
        assert_eq!(Ref::<Blob>::verified(*r.as_hash(), b"other"), None);
        assert_eq!(Ref::<Blob>::verified(Hash::ZERO, b"bytes"), None);
        assert!(r.verify(b"bytes"));
        assert!(!r.verify(b"bytez"));
    }

    #[test]
    fn eq_ord_and_hash_follow_the_address() {
        use std::collections::HashSet;
        let (a, b) = (Ref::<Blob>::derive(b"a"), Ref::<Blob>::derive(b"b"));
        assert_eq!(a.cmp(&b), a.as_hash().cmp(b.as_hash()));
        assert_eq!(a.partial_cmp(&b), Some(a.as_hash().cmp(b.as_hash())));
        assert_eq!(a.clone(), a);
        assert_ne!(a, b, "refs of distinct values differ");
        assert!(a != b && a == a);
        let set: HashSet<Ref<Blob>> = [a, a, b].into_iter().collect();
        assert_eq!(set.len(), 2);
        // `Hash` hashes the address, exactly as the bare `Hash` does.
        let digest = |h: &dyn Fn(&mut std::collections::hash_map::DefaultHasher)| {
            use std::hash::Hasher;
            let mut s = std::collections::hash_map::DefaultHasher::new();
            h(&mut s);
            s.finish()
        };
        use std::hash::Hash as _;
        assert_eq!(digest(&|s| a.hash(s)), digest(&|s| a.as_hash().hash(s)));
        assert_ne!(digest(&|s| a.hash(s)), digest(&|s| b.hash(s)));
    }

    #[test]
    fn debug_names_the_construction_and_display_is_full_hex() {
        let r = Ref::<HeadDigest>::derive(&claim());
        assert!(format!("{r:?}").starts_with("Ref<head-digest>("));
        assert_eq!(format!("{r}"), format!("{}", r.as_hash()));
        assert_eq!(format!("{r}").len(), 64);
        assert_eq!(Blob::NAME, "blob");
        assert_eq!(Chunk::NAME, "chunk");
        assert_eq!(ArenaImage::NAME, "arena-image");
    }
}
