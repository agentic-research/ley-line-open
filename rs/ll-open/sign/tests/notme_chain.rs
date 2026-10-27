//! Real notme certificates through `leyline_sign::cert_chain` (bead
//! `ley-line-open-9ac66d`).
//!
//! The fixtures in `tests/fixtures/notme/` were minted by notme's own
//! `worker/src/cert-authority.ts`, unmodified; see
//! `tests/fixtures/notme/PROVENANCE.md`. They are not hand-built, so these
//! tests measure LLO against what notme actually emits:
//!
//! - critical BasicConstraints / KeyUsage / SubjectAltName, which LLO
//!   rejected as unknown critical extensions before this bead;
//! - `.1.5` authMethod as a DER UTF8String, which LLO would otherwise read
//!   as an Interlace peer fingerprint;
//! - `.1.6` as 32 raw binding bytes, which LLO would otherwise reject as a
//!   malformed Interlace scope.

use leyline_sign::cert_chain::{
    CertProfile, ChainError, claims_to_json, verify_cert_chain, verify_cert_path,
    verify_cert_path_expecting,
};

/// A caller that binds notme's anchor to the Signet profile accepts notme's
/// path and refuses to read it as Interlace.
#[test]
fn notme_anchor_bound_to_signet_profile() {
    let ok = verify_cert_path_expecting(
        TASK_SIGNING,
        &[ISSUING_CA],
        ROOT_PUB,
        Some(CertProfile::Signet),
    )
    .expect("notme path under its own profile");
    assert_eq!(ok.epoch, Some(EPOCH));
    assert!(matches!(
        verify_cert_path_expecting(
            TASK_SIGNING,
            &[ISSUING_CA],
            ROOT_PUB,
            Some(CertProfile::Interlace)
        ),
        Err(ChainError::ConstraintViolation(_))
    ));
}

const ROOT_PUB: &[u8] = include_bytes!("fixtures/notme/root.pub");
const ISSUING_CA: &[u8] = include_bytes!("fixtures/notme/issuing-ca.der");
const TASK_SIGNING: &[u8] = include_bytes!("fixtures/notme/task-signing.der");
const TASK_MTLS: &[u8] = include_bytes!("fixtures/notme/task-mtls.der");
const DIRECT_SIGNING: &[u8] = include_bytes!("fixtures/notme/direct-signing.der");

/// `expected.json` values, recorded by the mint script.
const EPOCH: u32 = 3;
const TASK_SIGNING_PUBKEY_B64: &str = "/qAi4I+Usk3iLUdVE4x9q+LOPDhiLn6j4SRpUHTp8Q4=";
const DIRECT_SIGNING_PUBKEY_B64: &str = "hKbnHD689uXpD06YtIW7dVyh+RocjQUOhef14/ZRfUo=";

fn b64(s: &str) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).unwrap()
}

#[test]
fn notme_root_issuing_ca_task_path_verifies_with_epoch() {
    let claims = verify_cert_path(TASK_SIGNING, &[ISSUING_CA], ROOT_PUB)
        .expect("root -> Issuing CA -> task leaf verifies");
    assert_eq!(claims.profile, CertProfile::Signet);
    assert_eq!(claims.epoch, Some(EPOCH));
    assert_eq!(
        claims.ephemeral_pubkey.to_vec(),
        b64(TASK_SIGNING_PUBKEY_B64)
    );
    // authMethod ("passkey") and the peer binding are never reported.
    assert_eq!(claims.peer_fp, None);
    assert_eq!(claims.scope, None);
    assert_eq!(claims.confinement_digest, None);
    assert!(claims.not_before <= claims.not_after);

    let json = claims_to_json(&claims);
    assert!(json.contains("\"pr\":\"signet\""), "{json}");
    assert!(json.contains(&format!("\"ep\":{EPOCH}")), "{json}");
    assert!(!json.contains("\"pf\""), "{json}");
    assert!(!json.contains("\"sc\""), "{json}");
}

#[test]
fn notme_root_signed_leaf_verifies_with_epoch() {
    let claims = verify_cert_chain(DIRECT_SIGNING, ROOT_PUB).expect("notme leaf verifies");
    assert_eq!(claims.profile, CertProfile::Signet);
    assert_eq!(claims.epoch, Some(EPOCH));
    assert_eq!(
        claims.ephemeral_pubkey.to_vec(),
        b64(DIRECT_SIGNING_PUBKEY_B64)
    );
    assert_eq!(
        claims.peer_fp, None,
        "authMethod must not be read as peer_fp"
    );
    assert_eq!(claims.scope, None, "peer binding must not be read as scope");
}

#[test]
fn notme_task_leaf_without_its_tier_does_not_reach_root() {
    assert!(matches!(
        verify_cert_chain(TASK_SIGNING, ROOT_PUB),
        Err(ChainError::BadSignature)
    ));
}

#[test]
fn notme_issuing_ca_is_not_an_end_entity() {
    assert!(matches!(
        verify_cert_chain(ISSUING_CA, ROOT_PUB),
        Err(ChainError::ConstraintViolation(_))
    ));
}

#[test]
fn notme_leaf_cannot_stand_in_for_the_tier() {
    // A CA=false leaf placed where the Issuing CA belongs.
    assert!(matches!(
        verify_cert_path(TASK_SIGNING, &[DIRECT_SIGNING], ROOT_PUB),
        Err(ChainError::BadSignature | ChainError::ConstraintViolation(_))
    ));
}

#[test]
fn notme_p256_mtls_cert_is_not_an_ed25519_leaf() {
    assert!(matches!(
        verify_cert_path(TASK_MTLS, &[ISSUING_CA], ROOT_PUB),
        Err(ChainError::BadSpki)
    ));
}

#[test]
fn notme_tampered_leaf_rejects() {
    // Flip a byte inside the `.1.5` authMethod value ("passkey").
    let pos = TASK_SIGNING
        .windows(7)
        .position(|w| w == b"passkey")
        .expect("authMethod present in the fixture");
    let mut tampered = TASK_SIGNING.to_vec();
    tampered[pos] ^= 0x01;
    assert!(matches!(
        verify_cert_path(&tampered, &[ISSUING_CA], ROOT_PUB),
        Err(ChainError::BadSignature)
    ));
}
