//! Interlace-profile golden vectors (bead `ley-line-open-9ac66d`, acceptance
//! (c)): cloister's committed cert fixtures must keep verifying with the
//! same claims and the same claims-JSON bytes after the profile
//! discriminator landed.
//!
//! Source: cloister `test/wire/fixtures/cert-chain.ts` at cloister commit
//! 557f113 (base64url, copied verbatim). These certs were minted by LLO's
//! `tests_helpers::mint_test_cert` shape from a fixed seed; cloister's
//! `signet-verify.test.ts` gates on the same bytes.

use base64::Engine;
use leyline_sign::cert_chain::{
    CertProfile, ChainError, claims_to_json, verify_cert_chain, verify_cert_path_expecting,
};

/// The reverse binding: cloister's anchor bound to Interlace keeps its
/// claims, and refuses to be read as a Signet-profile issuer.
#[test]
fn cloister_anchor_bound_to_interlace_profile() {
    let cert = d(CERT_FULL_B64);
    let anchor = d(MASTER_PUBKEY_B64);
    let c = verify_cert_path_expecting(&cert, &[], &anchor, Some(CertProfile::Interlace))
        .expect("Interlace cert under its own profile");
    assert_eq!(c.peer_fp.as_deref(), Some("sha256:abc123def456"));
    assert!(matches!(
        verify_cert_path_expecting(&cert, &[], &anchor, Some(CertProfile::Signet)),
        Err(ChainError::ConstraintViolation(_))
    ));
    // Unbound is the unchanged default.
    assert_eq!(
        claims_to_json(&verify_cert_path_expecting(&cert, &[], &anchor, None).unwrap()),
        claims_to_json(&verify(CERT_FULL_B64).unwrap())
    );
}

const MASTER_PUBKEY_B64: &str = "ebVWLo_mVPlAeLES6KmLp5AfhTrmlb7X4OORC60ElmQ";
const EPK_B64: &str = "zRSzf5VulTGU_3-3Oz2B3MVh1hp1OAlLfD4aZD7l86o";
const CERT_FULL_B64: &str = "MIIBEDCBw6ADAgECAgEBMAUGAytlcDAAMB4XDTIzMTExNDIyMTMyMFoXDTQ5MTIzMTIzNTk1OVowADAqMAUGAytlcAMhAM0Us3-VbpUxlP9_tzs9gdzFYdYadTgJS3w-GmQ-5fOqo2IwYDARBgorBgEEAYaNHwEEBAMCAQcwIwYKKwYBBAGGjR8BBQQVDBNzaGEyNTY6YWJjMTIzZGVmNDU2MCYGCisGAQQBho0fAQYEGAwWYmVhZF9jcmVhdGU6L3JlcG9zL2ZvbzAFBgMrZXADQQAke7lJkybGWHBxDUQujgF_qWaui0cUeUkmjfXFaJqnyjtRQX_WUfhKwhRXMIkAKDOMbic31bXNne6ijmdSppAD";
const CERT_ADMIN_B64: &str = "MIH7MIGuoAMCAQICAQEwBQYDK2VwMAAwHhcNMjMxMTE0MjIxMzIwWhcNNDkxMjMxMjM1OTU5WjAAMCowBQYDK2VwAyEAzRSzf5VulTGU_3-3Oz2B3MVh1hp1OAlLfD4aZD7l86qjTTBLMBEGCisGAQQBho0fAQQEAwIBBzAjBgorBgEEAYaNHwEFBBUME3NoYTI1NjphYmMxMjNkZWY0NTYwEQYKKwYBBAGGjR8BBgQDDAEqMAUGAytlcANBANNQU8iAOpLNG__Pz6FT9_qrZfeV_y4tZ5vJ-dJDyOoVyEfNoS3itMn-MCU0AqR7EF1WxmqWWI66oE0WlZz1qAM";
const CERT_MINIMAL_B64: &str = "MIGrMF-gAwIBAgIBATAFBgMrZXAwADAeFw0yMzExMTQyMjEzMjBaFw00OTEyMzEyMzU5NTlaMAAwKjAFBgMrZXADIQDNFLN_lW6VMZT_f7c7PYHcxWHWGnU4CUt8PhpkPuXzqjAFBgMrZXADQQAxSk4K9h2Daa-HTeI5aX1CFKkAWySMhVj7hvwId3w5uhZETPHGRy3EV-VDAiEWvnaFpmWvy-uIrdJvsmjSZ3gJ";
const CERT_WRONG_MASTER_B64: &str = "MIIBEDCBw6ADAgECAgEBMAUGAytlcDAAMB4XDTIzMTExNDIyMTMyMFoXDTQ5MTIzMTIzNTk1OVowADAqMAUGAytlcAMhAM0Us3-VbpUxlP9_tzs9gdzFYdYadTgJS3w-GmQ-5fOqo2IwYDARBgorBgEEAYaNHwEEBAMCAQcwIwYKKwYBBAGGjR8BBQQVDBNzaGEyNTY6YWJjMTIzZGVmNDU2MCYGCisGAQQBho0fAQYEGAwWYmVhZF9jcmVhdGU6L3JlcG9zL2ZvbzAFBgMrZXADQQCVKYCrsFsvCV9c0KDjaDyacj4byKm5Ci4fMkBBXKpL1-vuLIr0PfS-WiEH3JRlzOfzwyN2JRza8CYk2_FLdxUI";
const CERT_CRITICAL_UNKNOWN_EXT_B64: &str = "MIIBEjCBxaADAgECAgEBMAUGAytlcDAAMB4XDTIzMTExNDIyMTMyMFoXDTQ5MTIzMTIzNTk1OVowADAqMAUGAytlcAMhAM0Us3-VbpUxlP9_tzs9gdzFYdYadTgJS3w-GmQ-5fOqo2QwYjARBgorBgEEAYaNHwEEBAMCAQcwGwYKKwYBBAGGjR8BBQQNDAtzaGEyNTY6dGVzdDAaBgorBgEEAYaNHwEGBAwMCnRlc3Q6c2NvcGUwFAYKKwYBBAGGjR8qAQEB_wQDBAEBMAUGAytlcANBAIBamT6Ykq3cQd1tiTGBBXmufdbFfb3OPq-Blkd0l9jvx9vESZ7aQIuhd5wqWdEHWRpAxFf1pB2rEWa9UtLXawQ";

fn d(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .unwrap()
}

fn verify(cert_b64: &str) -> Result<leyline_sign::cert_chain::CertClaims, ChainError> {
    verify_cert_chain(&d(cert_b64), &d(MASTER_PUBKEY_B64))
}

#[test]
fn cloister_full_cert_claims_and_json_unchanged() {
    let c = verify(CERT_FULL_B64).unwrap();
    assert_eq!(c.profile, CertProfile::Interlace);
    assert_eq!(c.ephemeral_pubkey.to_vec(), d(EPK_B64));
    assert_eq!((c.not_before, c.not_after), (1700000000, 2524607999));
    assert_eq!(c.epoch, Some(7));
    assert_eq!(c.peer_fp.as_deref(), Some("sha256:abc123def456"));
    assert_eq!(c.scope.as_deref(), Some("bead_create:/repos/foo"));
    assert_eq!(
        claims_to_json(&c),
        format!(
            "{{\"epk\":\"{EPK_B64}\",\"nb\":1700000000,\"na\":2524607999,\"ep\":7,\
             \"pf\":\"sha256:abc123def456\",\"sc\":\"bead_create:/repos/foo\"}}"
        )
    );
}

#[test]
fn cloister_admin_cert_claims_unchanged() {
    let c = verify(CERT_ADMIN_B64).unwrap();
    assert_eq!(c.profile, CertProfile::Interlace);
    assert_eq!(c.scope.as_deref(), Some("*"));
    assert_eq!(c.peer_fp.as_deref(), Some("sha256:abc123def456"));
}

#[test]
fn cloister_minimal_cert_claims_and_json_unchanged() {
    let c = verify(CERT_MINIMAL_B64).unwrap();
    assert_eq!(
        claims_to_json(&c),
        format!("{{\"epk\":\"{EPK_B64}\",\"nb\":1700000000,\"na\":2524607999}}")
    );
}

#[test]
fn cloister_rejection_vectors_still_reject() {
    assert!(matches!(
        verify(CERT_WRONG_MASTER_B64),
        Err(ChainError::BadSignature)
    ));
    assert!(matches!(
        verify(CERT_CRITICAL_UNKNOWN_EXT_B64),
        Err(ChainError::UnknownCriticalExtension(_))
    ));
}
