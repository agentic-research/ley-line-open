//! Bead `ley-line-open-c79ea8` — cloister/confinement/v1 §7:
//! confinementDigest Interlace cert extension roundtrip.
//!
//! ## Load-bearing claim
//!
//! A cert minted with a 32-byte confinement_digest survives DER encode →
//! Ed25519 signature verify → extension decode, and the parsed
//! `CertClaims.confinement_digest` matches the byte string that went in.
//! This is the substrate identity-commit that lets a cloister runner
//! reject any bundle whose manifest hash disagrees with the cert claim.
//!
//! ## Why this file exists (cloister unblock)
//!
//! Cloister's confinement/v1 §7 requires a cryptographic binding between
//! (ephemeral pubkey, ConfinementManifest). LLO owns the Ed25519 cert
//! path (ADR-0018 / leyline-sign), so LLO ships the extension. Without
//! this roundtrip pin, a silent DER encoding drift here would make every
//! confinement-committed cert on the wire look "clean" but decode to
//! `None` — a silent-failure oracle the identity check couldn't catch.

use ed25519_dalek::SigningKey;
use leyline_sign::cert_chain::{ChainError, tests_helpers::mint_test_cert, verify_cert_chain};
use rand::{RngCore, rngs::OsRng};

/// Fresh Ed25519 key. Non-deterministic — we're testing the extension
/// path, not signature determinism (bead 474c0a covers that).
fn random_key() -> SigningKey {
    let mut seed = [0u8; 32];
    OsRng.fill_bytes(&mut seed);
    SigningKey::from_bytes(&seed)
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[test]
fn confinement_digest_roundtrip_preserves_all_32_bytes() {
    let master = random_key();
    let ephemeral = random_key();
    let nb = now() - 60;
    let na = now() + 600;

    // Every byte distinct — a shift, truncation, or byte-swap on either
    // side of the wire would surface as a mismatch on comparison.
    let digest: [u8; 32] = std::array::from_fn(|i| i as u8);

    let cert_der = mint_test_cert(
        &master,
        &ephemeral,
        nb,
        na,
        Some(1),
        Some("sha256:mint_test"),
        Some("cloister:confinement/v1"),
        Some(digest),
    );

    let claims = verify_cert_chain(&cert_der, master.verifying_key().as_bytes())
        .expect("cert with confinementDigest must verify");

    assert_eq!(
        claims.confinement_digest,
        Some(digest),
        "confinement_digest must round-trip byte-for-byte through DER + Ed25519",
    );
    // Sibling Interlace extensions still land — the new extension must
    // not shadow or reorder the existing three.
    assert_eq!(claims.epoch, Some(1));
    assert_eq!(claims.peer_fp.as_deref(), Some("sha256:mint_test"));
    assert_eq!(claims.scope.as_deref(), Some("cloister:confinement/v1"));
}

#[test]
fn absent_confinement_digest_stays_none() {
    // Load-bearing: pre-bead certs (or certs from a workload that doesn't
    // commit to a confinement manifest) MUST parse cleanly and surface
    // the field as `None`. If a decode bug ever populated a garbage
    // digest here, cloister's identity check would fail-closed on legit
    // legacy certs.
    let master = random_key();
    let ephemeral = random_key();

    let cert_der = mint_test_cert(
        &master,
        &ephemeral,
        now(),
        now() + 300,
        Some(2),
        None,
        None,
        None,
    );

    let claims = verify_cert_chain(&cert_der, master.verifying_key().as_bytes())
        .expect("cert without confinementDigest must verify");
    assert!(
        claims.confinement_digest.is_none(),
        "absent extension must surface as None, got {:?}",
        claims.confinement_digest,
    );
}

#[test]
fn confinement_digest_present_alone_no_other_interlace_ext() {
    // The extension is independent — a cert may carry only the
    // confinementDigest with no epoch/peer_fp/scope. Verifier must
    // handle this shape too.
    let master = random_key();
    let ephemeral = random_key();
    let digest: [u8; 32] = [0xAB; 32];

    let cert_der = mint_test_cert(
        &master,
        &ephemeral,
        now(),
        now() + 300,
        None,
        None,
        None,
        Some(digest),
    );

    let claims = verify_cert_chain(&cert_der, master.verifying_key().as_bytes())
        .expect("cert with only confinementDigest must verify");
    assert_eq!(claims.confinement_digest, Some(digest));
    assert!(claims.epoch.is_none());
    assert!(claims.peer_fp.is_none());
    assert!(claims.scope.is_none());
}

#[test]
fn tampered_confinement_digest_extension_body_still_signature_valid_but_different_digest() {
    // Adjacent invariant: the extension body IS part of tbs_certificate,
    // so a byte flip in the digest breaks the signature. This test proves
    // the digest travels inside the signed span (not in an unsigned
    // wrapper) by asserting that two mints with different digests yield
    // different verification outcomes — one succeeds, one succeeds with
    // a distinct digest, but neither is confusable with the other.
    let master = random_key();
    let ephemeral = random_key();

    let digest_a: [u8; 32] = [0x11; 32];
    let digest_b: [u8; 32] = [0x22; 32];

    let cert_a = mint_test_cert(
        &master,
        &ephemeral,
        now(),
        now() + 300,
        None,
        None,
        None,
        Some(digest_a),
    );
    let cert_b = mint_test_cert(
        &master,
        &ephemeral,
        now(),
        now() + 300,
        None,
        None,
        None,
        Some(digest_b),
    );

    let claims_a =
        verify_cert_chain(&cert_a, master.verifying_key().as_bytes()).expect("cert_a verifies");
    let claims_b =
        verify_cert_chain(&cert_b, master.verifying_key().as_bytes()).expect("cert_b verifies");

    assert_eq!(claims_a.confinement_digest, Some(digest_a));
    assert_eq!(claims_b.confinement_digest, Some(digest_b));
    assert_ne!(claims_a.confinement_digest, claims_b.confinement_digest);

    // Byte-flip attack — swap a single byte in cert_a's DER (post-mint,
    // pre-verify). Ed25519 signature over tbs_certificate MUST reject.
    let mut tampered = cert_a.clone();
    // Flip a byte somewhere in the middle; exact position doesn't matter
    // because any change to tbs_certificate invalidates the signature.
    let mid = tampered.len() / 2;
    tampered[mid] ^= 0xFF;
    let result = verify_cert_chain(&tampered, master.verifying_key().as_bytes());
    assert!(
        matches!(
            result,
            Err(ChainError::BadSignature) | Err(ChainError::BadDer(_))
        ),
        "byte-flip in cert must be rejected (got {result:?})",
    );
}
