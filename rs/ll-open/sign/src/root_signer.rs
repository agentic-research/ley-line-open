//! Concrete Ed25519 [`RootSigner`] over Σ roots (workstream S1).
//!
//! `leyline-core`'s `substrate` module declares the [`RootSigner`] trait
//! surface and implements nothing; the implementation lives here, in the
//! signing crate, alongside the certificate machinery (`cert_chain`) that the
//! Signet trust root will hang off.
//!
//! **What gets signed.** Not a bare root — the canonical head digest from
//! [`leyline_core::head_digest`], which binds `(generation, rootHash,
//! parentHash)`. Signing `rootHash` alone would let a signature be replayed at
//! another generation or grafted onto a forked chain.
//!
//! **Scheme alignment.** Ed25519, matching the frozen `leyline-net/v1`
//! manifest so the transport manifest and the at-rest head share one trust
//! root and one verification path. ML-DSA-44 is the reserved post-quantum
//! successor on the transport side; when that lands it should land here too.

use anyhow::Result;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use leyline_core::{Hash, RootSigner};

/// In-process Ed25519 signer over Σ roots.
///
/// Holds the secret in memory. A keystore- or HSM-backed implementor of
/// [`RootSigner`] can be swapped in without touching callers — the trait
/// deliberately hands the signer only a 32-byte [`Hash`], never content.
pub struct Ed25519RootSigner {
    key: SigningKey,
}

impl Ed25519RootSigner {
    /// Build from a raw 32-byte Ed25519 seed.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self {
            key: SigningKey::from_bytes(seed),
        }
    }

    /// The public half — embedded in a signed `Head` and distributed to
    /// verifiers, who need no signer state to check a signature.
    pub fn verifying_key(&self) -> VerifyingKey {
        self.key.verifying_key()
    }
}

impl RootSigner for Ed25519RootSigner {
    type Signature = Signature;
    type PublicKey = VerifyingKey;

    fn sign(&self, h: Hash) -> Result<Self::Signature> {
        Ok(self.key.sign(h.as_bytes()))
    }

    fn verify(h: Hash, sig: &Self::Signature, pk: &Self::PublicKey) -> bool {
        pk.verify(h.as_bytes(), sig).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leyline_core::head_digest;

    fn signer() -> Ed25519RootSigner {
        Ed25519RootSigner::from_seed(&[7u8; 32])
    }

    fn digest(generation: u64) -> Hash {
        head_digest(generation, Hash::from_bytes([1u8; 32]), Hash::ZERO)
    }

    #[test]
    fn sign_then_verify_roundtrips() {
        let s = signer();
        let d = digest(1);
        let sig = s.sign(d).expect("sign");
        assert!(Ed25519RootSigner::verify(d, &sig, &s.verifying_key()));
    }

    /// The whole point of binding the digest: a signature over generation 1
    /// must not authenticate generation 2's head.
    #[test]
    fn verify_rejects_a_different_head() {
        let s = signer();
        let sig = s.sign(digest(1)).expect("sign");
        assert!(!Ed25519RootSigner::verify(
            digest(2),
            &sig,
            &s.verifying_key()
        ));
    }

    /// A head signed by someone else is not authoritative for our signer.
    #[test]
    fn verify_rejects_a_foreign_key() {
        let s = signer();
        let other = Ed25519RootSigner::from_seed(&[9u8; 32]);
        let d = digest(1);
        let sig = s.sign(d).expect("sign");
        assert!(!Ed25519RootSigner::verify(d, &sig, &other.verifying_key()));
    }

    /// Verification needs no signer state — it is a static method by design.
    #[test]
    fn verification_requires_only_the_public_key() {
        let d = digest(3);
        let (sig, pk) = {
            let s = signer();
            (s.sign(d).expect("sign"), s.verifying_key())
        }; // signer dropped
        assert!(Ed25519RootSigner::verify(d, &sig, &pk));
    }
}
