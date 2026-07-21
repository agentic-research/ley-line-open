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
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier};
use leyline_core::{Hash, RootSigner};

// Re-exported so verify-side consumers (the CLI read path, a wasm verifier)
// can name a trust set without taking their own `ed25519-dalek` dependency.
pub use ed25519_dalek::VerifyingKey;

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

/// Parse a hex-encoded 32-byte Ed25519 public key into a verifying key.
///
/// Re-exported from here so consumers that only need to *verify* a head — the
/// CLI's read path, a browser verifier — can build a trust set without taking
/// a direct `ed25519-dalek` dependency.
///
/// Rejects, rather than panics on, a 32-byte string that is not a valid curve
/// point: trust-set material is operator-supplied configuration.
pub fn verifying_key_from_hex(hex: &str) -> Result<VerifyingKey> {
    let hex = hex.trim();
    if hex.len() != 64 || !hex.is_ascii() {
        anyhow::bail!(
            "Ed25519 public key must be 64 hex chars (32 bytes), got {}",
            hex.len()
        );
    }
    let mut raw = [0u8; 32];
    for (i, b) in raw.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|e| anyhow::anyhow!("Ed25519 public key is not valid hex: {e}"))?;
    }
    VerifyingKey::from_bytes(&raw)
        .map_err(|e| anyhow::anyhow!("not a valid Ed25519 public key: {e}"))
}

/// Outcome of checking an at-rest `Head`'s signature (workstream S2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadVerdict {
    /// Signature present and valid under one of the trusted keys.
    Valid,
    /// No signature present. Whether this is acceptable is the caller's
    /// policy — existing arenas predate signing.
    Unsigned,
    /// A signature is present and does not verify under any trusted key:
    /// tampering, corruption, or a signer this reader does not trust.
    Invalid,
}

/// Verify a head's signature against a set of trusted keys.
///
/// The split is deliberate. A *present but invalid* signature is always
/// [`HeadVerdict::Invalid`] — no policy should ever accept it, because the
/// only ways to get here are tampering, corruption, or an untrusted signer.
/// An *absent* signature is reported as [`HeadVerdict::Unsigned`] and left to
/// the caller, so unsigned arenas keep working until a deployment opts into
/// requiring signatures. Collapsing those two cases would either break every
/// existing arena or silently accept tampered ones.
pub fn verify_head(
    generation: u64,
    root: Hash,
    parent: Hash,
    signature: &[u8],
    trusted: &[VerifyingKey],
) -> HeadVerdict {
    if signature.is_empty() {
        return HeadVerdict::Unsigned;
    }
    let Ok(raw) = <[u8; 64]>::try_from(signature) else {
        // Wrong length is a malformed signature, not an absent one.
        return HeadVerdict::Invalid;
    };
    let sig = Signature::from_bytes(&raw);
    let digest = leyline_core::head_digest(generation, root, parent);
    // Re-deriving the digest here is what binds the verdict to the head the
    // caller actually holds: a signature lifted from another generation or
    // another chain verifies against *its* digest, never this one.
    if trusted
        .iter()
        .any(|pk| <Ed25519RootSigner as RootSigner>::verify(digest, &sig, pk))
    {
        HeadVerdict::Valid
    } else {
        HeadVerdict::Invalid
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

    // ── S2: trust-set parsing ─────────────────────────────────────────

    #[test]
    fn hex_key_round_trips_from_a_signer() {
        let pk = signer().verifying_key();
        let hex: String = pk.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(verifying_key_from_hex(&hex).expect("parse"), pk);
    }

    #[test]
    fn hex_key_accepts_uppercase_and_surrounding_space() {
        let pk = signer().verifying_key();
        let hex: String = pk.as_bytes().iter().map(|b| format!("{b:02X}")).collect();
        assert_eq!(
            verifying_key_from_hex(&format!("  {hex}  ")).expect("parse"),
            pk
        );
    }

    #[test]
    fn hex_key_rejects_wrong_length() {
        assert!(verifying_key_from_hex("abcd").is_err());
    }

    #[test]
    fn hex_key_rejects_non_hex() {
        assert!(verifying_key_from_hex(&"zz".repeat(32)).is_err());
    }

    /// `ed25519-dalek` 3 defers point decompression to verification time, so
    /// `from_bytes` accepts any 32 bytes — parsing cannot reject a non-curve
    /// point. Verified empirically: `ff`*32, `00`*32, and `y = p` all parse.
    /// The property that matters is therefore that such a key fails *closed*
    /// downstream: it authorizes nothing rather than authorizing everything.
    #[test]
    fn a_garbage_trusted_key_authorizes_nothing() {
        let junk = verifying_key_from_hex(&"ff".repeat(32)).expect("parses");
        let sig = signed(1, ROOT, [0u8; 32]);
        assert_eq!(
            verify_head(1, Hash::from_bytes(ROOT), Hash::ZERO, &sig, &[junk]),
            HeadVerdict::Invalid
        );
    }

    // ── S2: verify-on-load ────────────────────────────────────────────

    const ROOT: [u8; 32] = [1u8; 32];

    /// Sign a head the way `write_head_for_path` does, returning raw bytes as
    /// they would be read back out of the capnp `signature` field.
    fn signed(generation: u64, root: [u8; 32], parent: [u8; 32]) -> Vec<u8> {
        let d = head_digest(generation, Hash::from_bytes(root), Hash::from_bytes(parent));
        signer().sign(d).expect("sign").to_bytes().to_vec()
    }

    #[test]
    fn verify_head_accepts_a_well_formed_signature() {
        let sig = signed(1, ROOT, [0u8; 32]);
        assert_eq!(
            verify_head(
                1,
                Hash::from_bytes(ROOT),
                Hash::ZERO,
                &sig,
                &[signer().verifying_key()]
            ),
            HeadVerdict::Valid
        );
    }

    /// The load-bearing case: swapping the root under a captured signature is
    /// exactly the tamper this workstream exists to catch.
    #[test]
    fn verify_head_rejects_a_swapped_root() {
        let sig = signed(1, ROOT, [0u8; 32]);
        assert_eq!(
            verify_head(
                1,
                Hash::from_bytes([2u8; 32]), // attacker's arena
                Hash::ZERO,
                &sig,
                &[signer().verifying_key()]
            ),
            HeadVerdict::Invalid
        );
    }

    /// Replaying generation 1's signature at generation 2 must not verify —
    /// this is why the digest binds `generation` rather than signing the root.
    #[test]
    fn verify_head_rejects_a_replayed_generation() {
        let sig = signed(1, ROOT, [0u8; 32]);
        assert_eq!(
            verify_head(
                2,
                Hash::from_bytes(ROOT),
                Hash::ZERO,
                &sig,
                &[signer().verifying_key()]
            ),
            HeadVerdict::Invalid
        );
    }

    /// Grafting a head onto a different parent forks the Σ chain.
    #[test]
    fn verify_head_rejects_a_regrafted_parent() {
        let sig = signed(2, ROOT, [9u8; 32]);
        assert_eq!(
            verify_head(
                2,
                Hash::from_bytes(ROOT),
                Hash::from_bytes([8u8; 32]),
                &sig,
                &[signer().verifying_key()]
            ),
            HeadVerdict::Invalid
        );
    }

    /// A valid signature from a signer we do not trust is still not authority.
    #[test]
    fn verify_head_rejects_an_untrusted_signer() {
        let sig = signed(1, ROOT, [0u8; 32]);
        let stranger = Ed25519RootSigner::from_seed(&[9u8; 32]);
        assert_eq!(
            verify_head(
                1,
                Hash::from_bytes(ROOT),
                Hash::ZERO,
                &sig,
                &[stranger.verifying_key()]
            ),
            HeadVerdict::Invalid
        );
    }

    /// An empty trust set can authorize nothing — it must not degrade to
    /// "accept anything" the way a `.all()` over an empty iterator would.
    #[test]
    fn verify_head_rejects_when_no_keys_are_trusted() {
        let sig = signed(1, ROOT, [0u8; 32]);
        assert_eq!(
            verify_head(1, Hash::from_bytes(ROOT), Hash::ZERO, &sig, &[]),
            HeadVerdict::Invalid
        );
    }

    /// Truncated/garbage signature bytes are tampering, not "unsigned".
    #[test]
    fn verify_head_rejects_a_malformed_signature() {
        assert_eq!(
            verify_head(
                1,
                Hash::from_bytes(ROOT),
                Hash::ZERO,
                &[0u8; 17],
                &[signer().verifying_key()]
            ),
            HeadVerdict::Invalid
        );
    }

    /// Absent signature is reported as `Unsigned`, never silently `Valid` —
    /// the caller's policy decides whether that is acceptable.
    #[test]
    fn verify_head_reports_an_absent_signature_as_unsigned() {
        assert_eq!(
            verify_head(
                1,
                Hash::from_bytes(ROOT),
                Hash::ZERO,
                &[],
                &[signer().verifying_key()]
            ),
            HeadVerdict::Unsigned
        );
    }

    /// One trusted key among several must be enough — this is the shape key
    /// rotation needs (old and new key both trusted during the overlap).
    #[test]
    fn verify_head_accepts_any_key_in_the_trust_set() {
        let sig = signed(1, ROOT, [0u8; 32]);
        let stranger = Ed25519RootSigner::from_seed(&[9u8; 32]);
        assert_eq!(
            verify_head(
                1,
                Hash::from_bytes(ROOT),
                Hash::ZERO,
                &sig,
                &[stranger.verifying_key(), signer().verifying_key()]
            ),
            HeadVerdict::Valid
        );
    }
}
