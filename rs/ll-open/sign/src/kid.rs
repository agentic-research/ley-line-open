//! ADR-012 canonical key identifier (`kid`) — the LLO-side conforming
//! implementation of the derivation signet ratified (signet ADR 012, #136,
//! bead signet-248d17).
//!
//! ```text
//! kid = lowercasehex( SHA-256( canonical SPKI_DER )[:16] )
//! ```
//!
//! LLO does not *own* this contract — signet does. This module is a
//! conforming implementation, gated against the pinned cross-language vector
//! in the ADR so LLO/notme/signet agree byte-for-byte without importing each
//! other's code (the "authority + pinned vectors" pattern).
//!
//! Three normative MUSTs from the trust-root review are load-bearing here:
//!
//! - **R2 (canonicalize-then-hash).** The bytes hashed are *canonical* SPKI
//!   DER, never SPKI as-received. LLO sidesteps the footgun by *constructing*
//!   the SPKI from the raw key rather than parsing external bytes: the
//!   RFC 8410 absent-parameters encoding is the only valid Ed25519 SPKI, so a
//!   constructor emits canonical DER by definition. The as-received hazard
//!   (absent-params vs a lenient encoder's NULL-params) is a *parser's*
//!   problem — notme's — not this constructor's.
//! - **R1 (parity, not lookup).** `kid` is only ever compared for equality
//!   against a key already authenticated against the trust set; it is never
//!   the sole selector into an attacker-seedable key set. See
//!   [`crate::root_signer::verify_head`], which still verifies the signature
//!   against every trusted key regardless of `kid`.
//! - **R4 (shape validation).** [`is_canonical_kid_shape`] rejects anything
//!   that is not exactly 32 lowercase-hex chars, so a `jkt` (43 base64url), a
//!   full 256-bit hash (64 hex), or a longer fingerprint cannot be smuggled in
//!   where a `kid` is expected.

use ed25519_dalek::VerifyingKey;
use sha2::{Digest, Sha256};

/// RFC 8410 Ed25519 `SubjectPublicKeyInfo` prefix — the 12 bytes preceding the
/// 32-byte raw key in canonical (absent-parameters) DER. Verified against the
/// ADR vector: `302a300506032b6570032100`.
const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// The canonical SPKI DER for an Ed25519 verifying key (44 bytes), constructed
/// — never parsed — so it is canonical by construction (R2).
fn ed25519_spki_der(pubkey: &VerifyingKey) -> [u8; 44] {
    let mut spki = [0u8; 44];
    spki[..12].copy_from_slice(&ED25519_SPKI_PREFIX);
    spki[12..].copy_from_slice(pubkey.as_bytes());
    spki
}

/// Compute the canonical `kid` for an Ed25519 verifying key: the lowercase-hex
/// encoding of the first 16 bytes of `SHA-256(canonical SPKI DER)` — 32 hex
/// chars, per ADR-012.
pub fn canonical_kid(pubkey: &VerifyingKey) -> String {
    let digest = Sha256::digest(ed25519_spki_der(pubkey));
    hex::encode(&digest[..16])
}

/// R4 shape gate: `^[0-9a-f]{32}$`. Every boundary that accepts a `kid` MUST
/// pass it through this before treating the value as one.
pub fn is_canonical_kid_shape(s: &[u8]) -> bool {
    s.len() == 32
        && s.iter()
            .all(|&b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ADR's pinned Ed25519 vector. Raw 32-byte key 000102…1e1f.
    const VECTOR_PUBKEY: [u8; 32] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
        0x1e, 0x1f,
    ];
    const VECTOR_SPKI_HEX: &str =
        "302a300506032b6570032100000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    const VECTOR_SHA256_HEX: &str =
        "9408457aefd071cec127c1f98539930861ad1ba94c940db975c972c09fc68b68";
    const VECTOR_KID: &str = "9408457aefd071cec127c1f985399308";
    const VECTOR_LEGACY_64: &str = "9408457aefd071ce";

    fn vector_key() -> VerifyingKey {
        VerifyingKey::from_bytes(&VECTOR_PUBKEY).expect("vector key parses")
    }

    /// The cross-language contract: LLO must reproduce the ADR vector kid
    /// byte-for-byte, or notme/signet/LLO disagree on which key signed a head.
    #[test]
    fn canonical_kid_matches_the_pinned_adr_vector() {
        assert_eq!(canonical_kid(&vector_key()), VECTOR_KID);
    }

    /// Guards the two intermediates independently, so a mismatch localizes to
    /// SPKI construction vs the hash rather than just "the kid is wrong".
    #[test]
    fn spki_and_digest_match_the_vector() {
        assert_eq!(
            hex::encode(ed25519_spki_der(&vector_key())),
            VECTOR_SPKI_HEX
        );
        assert_eq!(
            hex::encode(Sha256::digest(ed25519_spki_der(&vector_key()))),
            VECTOR_SHA256_HEX
        );
    }

    /// Leading truncation ⇒ the 64-bit legacy id is a prefix of the 128-bit id
    /// (ADR "Migration"): a widening, not a re-keying.
    #[test]
    fn legacy_64bit_id_is_a_prefix_of_the_canonical_kid() {
        assert!(canonical_kid(&vector_key()).starts_with(VECTOR_LEGACY_64));
    }

    #[test]
    fn kid_is_32_lowercase_hex_chars() {
        let kid = canonical_kid(&vector_key());
        assert_eq!(kid.len(), 32);
        assert_eq!(kid, kid.to_lowercase());
    }

    #[test]
    fn shape_gate_accepts_a_canonical_kid() {
        assert!(is_canonical_kid_shape(VECTOR_KID.as_bytes()));
    }

    #[test]
    fn shape_gate_rejects_wrong_length() {
        assert!(!is_canonical_kid_shape(&VECTOR_KID.as_bytes()[..31]));
        assert!(!is_canonical_kid_shape(format!("{VECTOR_KID}0").as_bytes()));
    }

    /// Uppercase hex is a *different* string under byte-equality, so it must be
    /// rejected — the ADR pins lowercase precisely so comparison is byte-eq.
    #[test]
    fn shape_gate_rejects_uppercase() {
        assert!(!is_canonical_kid_shape(
            VECTOR_KID.to_uppercase().as_bytes()
        ));
    }

    /// A full 256-bit hash (64 hex) and a base64url jkt must not pass as kids.
    #[test]
    fn shape_gate_rejects_other_identifiers() {
        assert!(!is_canonical_kid_shape(VECTOR_SHA256_HEX.as_bytes())); // 64 hex
        assert!(!is_canonical_kid_shape(
            b"WvNVDZ1p3xJ7l9J0oHqfQ0mFq7WkZ2xY3z4A5b6C7d8" // 43 base64url (jkt-shaped)
        ));
    }

    #[test]
    fn shape_gate_rejects_non_hex() {
        assert!(!is_canonical_kid_shape(&[b'z'; 32]));
    }
}
