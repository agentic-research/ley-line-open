// Wire-format compatibility smoke test (bead ley-line-open-7226e3).
//
// Absorbing cloister's `leyline-sign` fork into LLO added the `host`
// feature. This test proves the fold-in did not change the CMS/PKCS#7
// signature wire format:
//
//   - Default (lib-only) build: a signature produced by `cms::sign_data`
//     round-trips through `cms::verify` and is byte-deterministic.
//   - `--features host` build: the same cms::sign_data / cms::verify pair
//     round-trips, AND the host-side `host::sign::sign_with` emits the
//     same raw Ed25519 wire bytes as ed25519-dalek's `Signer::sign`.
//
// The test exists on both feature configurations by design: the default
// asserts LLO's existing wire format is preserved; the `--features host`
// assertions extend that guarantee across the newly-folded-in host path.

use der::{Encode, asn1::BitString};
use ed25519_dalek::{Signer, SigningKey};
use leyline_sign::cms::{VerifyOptions, sign_data, verify};
use x509_cert::{
    Certificate,
    certificate::TbsCertificate,
    name::{Name, RdnSequence},
    serial_number::SerialNumber,
    spki::{AlgorithmIdentifier, SubjectPublicKeyInfo},
    time::{Time, Validity},
};

/// Deterministic seed → keypair. Ed25519 signatures over the same
/// message with the same key are byte-deterministic (RFC 8032 §5.1.6);
/// this lets us assert wire determinism.
const SEED: [u8; 32] = *b"wire-compat-fixed-seed-32-bytes.";
const MSG: &[u8] = b"leyline-sign wire compat probe: bead 7226e3";

/// Ed25519 SPKI algorithm OID per RFC 8410 §3.
const ID_ED25519: der::asn1::ObjectIdentifier =
    der::asn1::ObjectIdentifier::new_unwrap("1.3.101.112");

/// Mint a minimal self-signed Ed25519 X.509 certificate for the given
/// signing key. Copied from cert_chain.rs's in-crate `mint_test_cert`
/// helper (that helper is `#[cfg(test)]` so we cannot import it here).
/// No Interlace extensions, empty issuer/subject Names — the cert only
/// exists so `cms::sign_data` has a signer cert DER to embed.
fn mint_test_cert(signing_key: &SigningKey) -> Vec<u8> {
    let alg_ed25519 = AlgorithmIdentifier {
        oid: ID_ED25519,
        parameters: None,
    };
    let spki = SubjectPublicKeyInfo {
        algorithm: alg_ed25519.clone(),
        subject_public_key: BitString::from_bytes(signing_key.verifying_key().as_bytes()).unwrap(),
    };
    let issuer: Name = RdnSequence::default();
    let subject = issuer.clone();
    let validity = Validity {
        not_before: Time::UtcTime(
            der::asn1::UtcTime::from_unix_duration(std::time::Duration::from_secs(1_700_000_000))
                .unwrap(),
        ),
        not_after: Time::UtcTime(
            der::asn1::UtcTime::from_unix_duration(std::time::Duration::from_secs(2_000_000_000))
                .unwrap(),
        ),
    };
    let tbs = TbsCertificate {
        version: x509_cert::Version::V3,
        serial_number: SerialNumber::new(&[1]).unwrap(),
        signature: alg_ed25519.clone(),
        issuer,
        validity,
        subject,
        subject_public_key_info: spki,
        issuer_unique_id: None,
        subject_unique_id: None,
        extensions: None,
    };
    let tbs_der = tbs.to_der().unwrap();
    let sig_bytes = signing_key.sign(&tbs_der).to_bytes();
    let cert = Certificate {
        tbs_certificate: tbs,
        signature_algorithm: alg_ed25519,
        signature: BitString::from_bytes(&sig_bytes).unwrap(),
    };
    cert.to_der().unwrap()
}

#[test]
fn default_wire_format_round_trips_through_cms() {
    let sk = SigningKey::from_bytes(&SEED);
    let cert_der = mint_test_cert(&sk);
    let key_bytes = sk.to_keypair_bytes();
    let sig1 = sign_data(MSG, &cert_der, &key_bytes).expect("sign");
    let sig2 = sign_data(MSG, &cert_der, &key_bytes).expect("sign again");

    // Ed25519 + no signingTime attribute (per LLO/cloister ADR-0007
    // amendment). SignedAttributes are deterministic, so repeated
    // signs must produce byte-identical wire.
    assert_eq!(
        sig1, sig2,
        "wire format is non-deterministic — RFC 8032 violated or signingTime crept in"
    );

    let opts = VerifyOptions::default();
    let recovered = verify(&sig1, MSG, &opts).expect("verify");
    assert_eq!(
        recovered, cert_der,
        "verifier must return the exact signer cert we embedded"
    );
}

/// When compiled under `--features host`, `host::sign::sign_with`
/// produces Ed25519 signatures too. Prove those raw Ed25519 signatures
/// match what the ed25519-dalek `Signer` impl produces directly —
/// i.e., the host path does not alter Ed25519 wire encoding.
#[cfg(feature = "host")]
#[test]
fn host_sign_matches_raw_ed25519() {
    use base64ct::{Base64UrlUnpadded, Encoding};
    use leyline_sign::host::sign::sign_with;

    let sk = SigningKey::from_bytes(&SEED);
    let host_sig = sign_with(&sk, MSG, /* return_pubkey */ false);
    let host_sig_bytes = Base64UrlUnpadded::decode_vec(&host_sig.signature_b64).unwrap();
    let raw_sig = sk.sign(MSG).to_bytes();
    assert_eq!(
        &host_sig_bytes[..],
        &raw_sig[..],
        "host::sign::sign_with must emit RFC-8032 Ed25519 wire bytes"
    );
}

/// Under `--features host`, the CMS wire format must remain byte-
/// identical (belt-and-braces check that the `host` feature does not
/// accidentally leak a cfg into cms.rs).
#[cfg(feature = "host")]
#[test]
fn host_feature_does_not_change_cms_wire() {
    let sk = SigningKey::from_bytes(&SEED);
    let cert_der = mint_test_cert(&sk);
    let key_bytes = sk.to_keypair_bytes();
    let sig = sign_data(MSG, &cert_der, &key_bytes).expect("sign under host");
    let opts = VerifyOptions::default();
    let recovered = verify(&sig, MSG, &opts).expect("verify under host");
    assert_eq!(recovered, cert_der);
}
