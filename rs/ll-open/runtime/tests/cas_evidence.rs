//! The operator-configured execution/v1 verifier: [`CasEvidenceStore`] over
//! LLO's filesystem CAS blob store, operator trust keys, and
//! [`CasDsseEvidenceVerifier`] — and the typed reason each rejection carries
//! (bead `ley-line-open-410921`).
//!
//! Three identity domains meet here and stay separate:
//!
//! - **kid** — a trust key's public name, ADR-012's
//!   `lowercasehex(SHA-256(canonical SPKI DER)[:16])`. A rejection label only;
//!   it never selects the key a signature is checked under.
//! - **evidence digest** — `blake3-256:<hex>` over the exact DSSE envelope
//!   bytes held in the CAS (the blob-hash domain).
//! - **run binding** — the in-toto subject's `blake3` digest, the derived
//!   `run-<hex>` identity the evidence must authorize.

use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use capnp::message::{Builder, ReaderOptions};
use leyline_public_schema::execution_capnp;
use leyline_runtime::authorization::{
    AuthorizationPolicy, EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION, EvidenceRejection,
    EvidenceStore, GRANT_SIGNATURE_PAYLOAD_TYPE, authorize_with_verifier, canonical_digest,
    derive_run_id, grant_signing_bytes,
};
use leyline_runtime::cas_evidence::{
    CasEvidenceStore, EvidenceConfigError, TrustKey, operator_verifier, parse_trust_key,
};
use leyline_runtime::{BackendClass, CasDsseEvidenceVerifier, ErrorCode, ExecutionError};

const GRANT_ID: &str = "grant-cas";
const REPLAY_KEY: &str = "replay-cas";

fn signer(seed: u8) -> leyline_envelope::Ed25519RootSigner {
    leyline_envelope::Ed25519RootSigner::from_seed(&[seed; 32])
}

fn hex_key(signer: &leyline_envelope::Ed25519RootSigner) -> String {
    hex::encode(signer.verifying_key().as_bytes())
}

fn set_digest(mut digest: execution_capnp::digest_ref::Builder<'_>, value: &str) {
    digest.set_algorithm("blake3-256");
    digest.set_value(value);
}

fn spec_bytes() -> Vec<u8> {
    let mut message = Builder::new_default();
    let mut spec = message.init_root::<execution_capnp::run_spec::Builder<'_>>();
    spec.set_schema_version(EXECUTION_SCHEMA_VERSION);
    let mut executable = spec.reborrow().init_executable();
    executable.set_media_type("application/test-executable");
    set_digest(executable.reborrow().init_digest(), &"c".repeat(64));
    let mut bytes = Vec::new();
    capnp::serialize::write_message(&mut bytes, &message).expect("serialize spec");
    bytes
}

fn run_id(spec: &[u8]) -> String {
    derive_run_id(
        &canonical_digest(spec).expect("spec digest"),
        GRANT_ID,
        REPLAY_KEY,
    )
}

/// One APAS Handoff/v1 DSSE envelope asserting all three roles for `run_id`.
fn evidence_envelope(signer: &leyline_envelope::Ed25519RootSigner, run_id: &str) -> Vec<u8> {
    let statement = leyline_envelope::Statement::new(
        [
            "issuerEvidence",
            "workloadIdentityEvidence",
            "actorProvenanceEvidence",
        ]
        .into_iter()
        .map(|role| leyline_envelope::Subject::with_digest(role, "blake3", run_id))
        .collect(),
        "https://rosary.dev/Handoff/v1",
        serde_json::json!({"dispatchId": "dispatch-cas"}),
    );
    leyline_envelope::Envelope::sign(&statement, signer).to_json_vec()
}

/// The hex half of a `blake3-256:<hex>` evidence digest.
fn evidence_hex(digest: &str) -> &str {
    digest
        .strip_prefix("blake3-256:")
        .expect("evidence digest domain prefix")
}

/// A grant over `spec` whose three evidence references name `evidence_hex`,
/// signed by `signer` under its canonical kid, or unsigned.
fn grant(
    spec: &[u8],
    evidence_hex: &str,
    signer: Option<&leyline_envelope::Ed25519RootSigner>,
    expires_at: u64,
) -> Vec<u8> {
    let spec_hex = evidence_hex_of_spec(spec);
    let mut message = Builder::new_default();
    let mut grant = message.init_root::<execution_capnp::run_grant::Builder<'_>>();
    grant.set_grant_id(GRANT_ID);
    grant.set_expires_at_unix_ms(expires_at);
    grant.set_replay_key(REPLAY_KEY);
    set_digest(grant.reborrow().init_run_spec_digest(), &spec_hex);
    let in_toto = |mut evidence: execution_capnp::evidence_ref::Builder<'_>| {
        evidence.set_media_type("application/vnd.in-toto+json");
        set_digest(evidence.init_digest(), evidence_hex);
    };
    in_toto(grant.reborrow().init_issuer_evidence());
    in_toto(grant.reborrow().init_workload_identity_evidence());
    in_toto(grant.reborrow().init_actor_provenance_evidence());
    set_digest(grant.reborrow().init_confinement_digest(), &"b".repeat(64));
    grant.set_backend_class(execution_capnp::BackendClass::MicroVm);
    let mut capability = grant.reborrow().init_capabilities(1).get(0);
    capability.set_grant(EXECUTION_CAPABILITY);
    capability.set_interface(EXECUTION_SCHEMA_VERSION);
    let mut bytes = Vec::new();
    capnp::serialize::write_message(&mut bytes, &message).expect("serialize grant");
    let Some(signer) = signer else {
        return bytes;
    };
    let value = leyline_envelope::sign_payload(
        GRANT_SIGNATURE_PAYLOAD_TYPE,
        &grant_signing_bytes(&bytes).expect("signing bytes"),
        signer,
    );
    let mut signature = message
        .get_root::<execution_capnp::run_grant::Builder<'_>>()
        .expect("grant root")
        .init_signature();
    signature.set_algorithm("ed25519");
    signature.set_key_id(&TrustKey::new(signer.verifying_key()).kid);
    signature.set_value(&value);
    let mut bytes = Vec::new();
    capnp::serialize::write_message(&mut bytes, &message).expect("serialize signed grant");
    bytes
}

fn evidence_hex_of_spec(spec: &[u8]) -> String {
    canonical_digest(spec)
        .expect("spec digest")
        .strip_prefix("blake3-256:")
        .expect("prefix")
        .to_owned()
}

/// Re-encode a signed grant with a later expiry, carrying the original
/// signature forward — the edit an interposed caller would make.
fn tamper_expiry(signed: &[u8], expires_at: u64) -> Vec<u8> {
    let message = capnp::serialize::read_message(&mut &signed[..], ReaderOptions::new())
        .expect("read signed grant");
    let reader = message
        .get_root::<execution_capnp::run_grant::Reader<'_>>()
        .expect("grant root");
    let mut copy = Builder::new_default();
    copy.set_root(reader).expect("copy grant");
    copy.get_root::<execution_capnp::run_grant::Builder<'_>>()
        .expect("grant root")
        .set_expires_at_unix_ms(expires_at);
    let mut bytes = Vec::new();
    capnp::serialize::write_message(&mut bytes, &copy).expect("serialize tampered grant");
    bytes
}

fn policy() -> AuthorizationPolicy {
    AuthorizationPolicy {
        now_unix_ms: Some(1_000),
        required_backend: BackendClass::MicroVm,
        required_confinement_digest: None,
    }
}

/// An operator configuration on disk: an evidence store directory and one
/// trust-key file per trusted signer.
struct Operator {
    dir: tempfile::TempDir,
}

impl Operator {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("evidence")).expect("evidence dir");
        Self { dir }
    }

    fn store_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("evidence")
    }

    fn store(&self) -> CasEvidenceStore {
        CasEvidenceStore::open(&self.store_dir()).expect("open evidence store")
    }

    fn trust_key_file(&self, name: &str, contents: &str) -> std::path::PathBuf {
        let path = self.dir.path().join(name);
        std::fs::write(&path, contents).expect("write trust key");
        path
    }

    fn verifier(
        &self,
        trusted: &[&leyline_envelope::Ed25519RootSigner],
    ) -> CasDsseEvidenceVerifier<CasEvidenceStore> {
        let paths: Vec<_> = trusted
            .iter()
            .enumerate()
            .map(|(index, signer)| {
                self.trust_key_file(&format!("trust-{index}.key"), &hex_key(signer))
            })
            .collect();
        operator_verifier(&self.store_dir(), &paths).expect("operator verifier")
    }
}

fn rejection(error: &ExecutionError) -> EvidenceRejection {
    EvidenceRejection::of(error)
        .unwrap_or_else(|| panic!("error carries no typed evidence rejection: {error:?}"))
}

/// The accepted path, and the four rejections the bead names, each with its
/// own reason — so none of them can pass by tripping another's gate.
#[test]
fn signed_grant_with_stored_evidence_is_accepted_and_each_failure_has_its_own_reason() {
    let issuer = signer(41);
    let stranger = signer(42);
    let operator = Operator::new();
    let spec = spec_bytes();
    let store = operator.store();
    let digest = store
        .insert(&evidence_envelope(&issuer, &run_id(&spec)))
        .expect("insert evidence");
    let verifier = operator.verifier(&[&issuer]);

    let signed = grant(&spec, evidence_hex(&digest), Some(&issuer), 2_000);
    let authorized = authorize_with_verifier(&spec, &signed, &policy(), &verifier)
        .expect("a grant signed by a trusted key with stored, run-bound evidence");
    assert_eq!(authorized.run_id, run_id(&spec));

    let mut reasons = Vec::new();
    for (name, grant_bytes, want) in [
        (
            "unsigned",
            grant(&spec, evidence_hex(&digest), None, 2_000),
            EvidenceRejection::GrantUnsigned,
        ),
        (
            "wrong key",
            grant(&spec, evidence_hex(&digest), Some(&stranger), 2_000),
            EvidenceRejection::UntrustedSigner,
        ),
        (
            "tampered payload",
            tamper_expiry(&signed, u64::MAX),
            EvidenceRejection::SignatureInvalid,
        ),
        (
            "missing evidence",
            grant(&spec, &"e".repeat(64), Some(&issuer), 2_000),
            EvidenceRejection::EvidenceMissing,
        ),
    ] {
        let error =
            authorize_with_verifier(&spec, &grant_bytes, &policy(), &verifier).expect_err(name);
        assert_eq!(error.code, ErrorCode::Unauthenticated, "{name}: {error:?}");
        assert_eq!(rejection(&error), want, "{name}: {error:?}");
        assert!(
            error.detail.starts_with(want.as_str()),
            "{name}: the stable token leads the detail: {}",
            error.detail
        );
        reasons.push(want);
    }
    reasons.sort_by_key(|reason| reason.as_str());
    reasons.dedup();
    assert_eq!(reasons.len(), 4, "every failure mode has a distinct reason");
}

/// Evidence signed by a key outside the trust set is refused even when the
/// grant itself is signed by a trusted issuer.
#[test]
fn evidence_signed_by_an_untrusted_key_is_untrusted_signer() {
    let issuer = signer(43);
    let stranger = signer(44);
    let operator = Operator::new();
    let spec = spec_bytes();
    let digest = operator
        .store()
        .insert(&evidence_envelope(&stranger, &run_id(&spec)))
        .expect("insert");
    let signed = grant(&spec, evidence_hex(&digest), Some(&issuer), 2_000);
    let error = authorize_with_verifier(&spec, &signed, &policy(), &operator.verifier(&[&issuer]))
        .expect_err("evidence from an untrusted signer");
    assert_eq!(rejection(&error), EvidenceRejection::UntrustedSigner);
    assert!(error.detail.contains("issuerEvidence"), "{}", error.detail);
}

/// A DSSE envelope whose payload was edited after signing, re-stored under
/// its new content address: the CAS vouches for the bytes, the signature
/// does not.
#[test]
fn an_envelope_with_an_edited_payload_is_signature_invalid() {
    let issuer = signer(45);
    let operator = Operator::new();
    let spec = spec_bytes();
    let original = evidence_envelope(&issuer, &run_id(&spec));
    let mut wire: serde_json::Value = serde_json::from_slice(&original).expect("envelope json");
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let payload = b64
        .decode(wire["payload"].as_str().expect("payload"))
        .expect("decode payload");
    let edited = String::from_utf8(payload)
        .expect("utf8")
        .replace("dispatch-cas", "dispatch-xyz");
    wire["payload"] = b64.encode(edited.as_bytes()).into();
    let digest = operator
        .store()
        .insert(&serde_json::to_vec(&wire).expect("encode"))
        .expect("insert");
    let signed = grant(&spec, evidence_hex(&digest), Some(&issuer), 2_000);
    let error = authorize_with_verifier(&spec, &signed, &policy(), &operator.verifier(&[&issuer]))
        .expect_err("an edited envelope payload");
    assert_eq!(rejection(&error), EvidenceRejection::SignatureInvalid);
}

/// Bytes altered at rest in the CAS — the store's verify-on-read refuses to
/// vouch for them.
#[test]
fn evidence_altered_at_rest_is_evidence_integrity() {
    let operator = Operator::new();
    let store = operator.store();
    let digest = store.insert(b"{\"original\":true}").expect("insert");
    let hash = leyline_core::Hash::from_bytes(
        hex::decode(evidence_hex(&digest))
            .expect("hex")
            .try_into()
            .expect("32 bytes"),
    );
    let path = leyline_core::FsBlobStore::new(operator.store_dir())
        .expect("blob store")
        .path_for(&hash);
    std::fs::write(&path, b"{\"original\":false}").expect("tamper at rest");
    let error = store.load(&digest).expect_err("tampered blob");
    assert_eq!(rejection(&error), EvidenceRejection::EvidenceIntegrity);
}

#[test]
fn the_store_loads_what_it_inserted_and_refuses_other_digest_domains() {
    let operator = Operator::new();
    let store = operator.store();
    let digest = store.insert(b"evidence bytes").expect("insert");
    assert_eq!(
        digest,
        format!("blake3-256:{}", blake3::hash(b"evidence bytes").to_hex())
    );
    assert_eq!(store.load(&digest).expect("load"), b"evidence bytes");

    let missing = format!("blake3-256:{}", "0".repeat(64));
    assert_eq!(
        rejection(&store.load(&missing).expect_err("absent")),
        EvidenceRejection::EvidenceMissing
    );
    // A kid, a sha256 digest, an uppercase digest: none name a CAS blob.
    for bad in [
        TrustKey::new(signer(46).verifying_key()).kid,
        format!("sha256:{}", "0".repeat(64)),
        format!("blake3-256:{}", "A".repeat(64)),
        "blake3-256:abc".to_owned(),
    ] {
        assert_eq!(
            rejection(&store.load(&bad).expect_err(&bad)),
            EvidenceRejection::EvidenceDigestMalformed,
            "{bad}"
        );
    }
}

#[test]
fn a_store_directory_must_already_exist() {
    let dir = tempfile::tempdir().expect("tempdir");
    let absent = dir.path().join("absent");
    assert!(matches!(
        CasEvidenceStore::open(&absent),
        Err(EvidenceConfigError::EvidenceStore { .. })
    ));
    assert!(!absent.exists(), "opening never creates the store");
}

/// The two accepted trust-key encodings name one key, and its kid is the
/// ADR-012 canonical kid — SHA-256 over canonical SPKI DER, not BLAKE3.
#[test]
fn hex_and_pem_trust_keys_parse_to_one_key_and_one_canonical_kid() {
    let signer = signer(47);
    let raw = signer.verifying_key();
    let mut spki = vec![
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    spki.extend_from_slice(raw.as_bytes());
    let pem = format!(
        "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
        base64::engine::general_purpose::STANDARD.encode(&spki)
    );
    let from_hex = parse_trust_key(format!("{}\n", hex_key(&signer)).as_bytes()).expect("hex");
    let from_pem = parse_trust_key(pem.as_bytes()).expect("pem");
    assert_eq!(from_hex, from_pem);
    assert_eq!(from_hex.key, raw);
    assert_eq!(
        from_hex.kid,
        hex::encode(&<sha2::Sha256 as sha2::Digest>::digest(&spki)[..16])
    );
    assert_ne!(from_hex.kid, blake3::hash(&spki).to_hex()[..32]);
}

#[test]
fn malformed_trust_keys_are_refused() {
    let raw = *signer(48).verifying_key().as_bytes();
    // NULL-parameters SPKI: a lenient encoder's non-canonical form (R2).
    let mut null_params = vec![
        0x30, 0x2c, 0x30, 0x07, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x05, 0x00, 0x03, 0x21, 0x00,
    ];
    null_params.extend_from_slice(&raw);
    let b64 = base64::engine::general_purpose::STANDARD;
    for (name, contents) in [
        ("empty", String::new()),
        ("short hex", "abcd".to_owned()),
        ("uppercase-free garbage", "zz".repeat(32)),
        (
            "private key label",
            format!(
                "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
                b64.encode([0u8; 48])
            ),
        ),
        (
            "non-canonical spki",
            format!(
                "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
                b64.encode(&null_params)
            ),
        ),
        (
            "two keys in one file",
            format!("{}\n{}\n", hex::encode(raw), hex::encode(raw)),
        ),
    ] {
        assert!(
            parse_trust_key(contents.as_bytes()).is_err(),
            "{name} must not parse as a trust key"
        );
    }
}

#[test]
fn operator_configuration_fails_closed() {
    let operator = Operator::new();
    let issuer = signer(49);
    let good = operator.trust_key_file("good.key", &hex_key(&issuer));
    let duplicate = operator.trust_key_file("dup.key", &hex_key(&issuer));
    let bad = operator.trust_key_file("bad.key", "not a key");

    assert!(matches!(
        operator_verifier(&operator.store_dir(), &[]),
        Err(EvidenceConfigError::NoTrustKeys)
    ));
    assert!(matches!(
        operator_verifier(&operator.store_dir(), &[bad]),
        Err(EvidenceConfigError::TrustKey { .. })
    ));
    assert!(matches!(
        operator_verifier(
            &operator.store_dir(),
            &[operator.dir.path().join("absent.key")]
        ),
        Err(EvidenceConfigError::TrustKey { .. })
    ));
    assert!(matches!(
        operator_verifier(&operator.store_dir(), &[good.clone(), duplicate]),
        Err(EvidenceConfigError::DuplicateTrustKey { .. })
    ));
    assert!(matches!(
        operator_verifier(
            Path::new("/nonexistent/evidence"),
            std::slice::from_ref(&good)
        ),
        Err(EvidenceConfigError::EvidenceStore { .. })
    ));
    let verifier = operator_verifier(&operator.store_dir(), &[good]).expect("valid config");
    assert_eq!(
        verifier.trusted_kids(),
        vec![TrustKey::new(issuer.verifying_key()).kid]
    );
}

/// A verifier constructed with no keys rejects everything, with its own
/// reason — the embedding constructor stays fail-closed too.
#[test]
fn a_verifier_without_trust_keys_rejects_with_no_trust_keys() {
    let operator = Operator::new();
    let issuer = signer(50);
    let spec = spec_bytes();
    let digest = operator
        .store()
        .insert(&evidence_envelope(&issuer, &run_id(&spec)))
        .expect("insert");
    let verifier = CasDsseEvidenceVerifier::new(Arc::new(operator.store()), Vec::new());
    let signed = grant(&spec, evidence_hex(&digest), Some(&issuer), 2_000);
    let error =
        authorize_with_verifier(&spec, &signed, &policy(), &verifier).expect_err("no trust keys");
    assert_eq!(rejection(&error), EvidenceRejection::NoTrustKeys);
}

/// Evidence in a format the verifier does not read keeps its own error code,
/// `UnsupportedBackend`, and still carries its typed reason; a token under
/// the wrong code is not that reason.
#[test]
fn a_non_in_toto_reference_is_evidence_media_type_under_unsupported_backend() {
    use leyline_runtime::authorization::{EvidenceBinding, EvidenceField, EvidenceVerifier};
    let operator = Operator::new();
    let verifier = operator.verifier(&[&signer(51)]);
    let error = verifier
        .verify(
            EvidenceField::Issuer,
            &EvidenceBinding {
                run_id: format!("run-{}", "0".repeat(64)),
                spec_digest: format!("blake3-256:{}", "0".repeat(64)),
            },
            &leyline_runtime::EvidenceRef {
                media_type: "application/x-not-in-toto".into(),
                digest: format!("blake3-256:{}", "0".repeat(64)),
            },
        )
        .expect_err("a non-in-toto media type");
    assert_eq!(error.code, ErrorCode::UnsupportedBackend, "{error:?}");
    assert_eq!(rejection(&error), EvidenceRejection::EvidenceMediaType);

    let relabelled = ExecutionError {
        code: ErrorCode::Unauthenticated,
        ..error
    };
    assert_eq!(EvidenceRejection::of(&relabelled), None);
}

#[test]
fn a_rejection_displays_as_its_stable_token() {
    for reason in [
        EvidenceRejection::GrantUnsigned,
        EvidenceRejection::UntrustedSigner,
        EvidenceRejection::EvidenceMediaType,
    ] {
        assert_eq!(reason.to_string(), reason.as_str());
    }
    assert_eq!(
        EvidenceRejection::EvidenceMissing.to_string(),
        "evidence-missing"
    );
}
