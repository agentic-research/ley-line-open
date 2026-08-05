use capnp::message::Builder;
use capnp::message::ReaderOptions;
use leyline_public_schema::execution_capnp;
use leyline_runtime::authorization::{
    AuthorizationPolicy, EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION, EvidenceBinding,
    EvidenceField, EvidenceRef, EvidenceStore, EvidenceVerifier, GRANT_SIGNATURE_PAYLOAD_TYPE,
    MetadataOnlyEvidenceVerifier, SignedGrant, authorize_with_verifier, canonical_digest,
    derive_run_id, grant_signing_bytes,
};
use leyline_runtime::transport::{
    cancel_json, capabilities_json, cleanup_json, collect_json, inspect_json, start_json,
    start_json_with_verifier, status_json,
};
use leyline_runtime::{
    Backend, BackendCapabilities, BackendClass, BackendRun, BackendRunStatus, ExecutionError,
    ExecutionRequest, ExecutionResolver, ExecutionService, ResourceLimits,
};
use serde_json::json;
use std::collections::BTreeMap;

fn spec_bytes() -> Vec<u8> {
    spec_bytes_with_interface(None, 0)
}

fn spec_bytes_with_interface(interface: Option<&str>, wall_time_ms: u64) -> Vec<u8> {
    spec_bytes_with_details(interface, wall_time_ms, &[])
}

fn spec_bytes_with_details(
    interface: Option<&str>,
    wall_time_ms: u64,
    workspaces: &[(&str, &str)],
) -> Vec<u8> {
    let mut message = Builder::new_default();
    let mut spec = message.init_root::<execution_capnp::run_spec::Builder<'_>>();
    spec.set_schema_version(EXECUTION_SCHEMA_VERSION);
    let mut executable = spec.reborrow().init_executable();
    executable.set_media_type("application/test-executable");
    set_digest(executable.reborrow().init_digest(), &"c".repeat(64));
    if let Some(interface) = interface {
        let mut interfaces = spec.reborrow().init_requested_interfaces(1);
        interfaces.set(0, interface);
    }
    if wall_time_ms != 0 {
        spec.reborrow()
            .init_requested_limits()
            .set_wall_time_ms(wall_time_ms);
    }
    let mut workspace_inputs = spec
        .reborrow()
        .init_workspace_inputs(workspaces.len() as u32);
    for (index, (name, graph_root)) in workspaces.iter().enumerate() {
        let mut workspace = workspace_inputs.reborrow().get(index as u32);
        workspace.set_name(name);
        set_digest(workspace.init_graph_root(), graph_root);
    }
    let mut bytes = Vec::new();
    capnp::serialize::write_message(&mut bytes, &message).expect("serialize spec");
    bytes
}

fn set_digest(mut digest: execution_capnp::digest_ref::Builder<'_>, value: &str) {
    digest.set_algorithm("blake3-256");
    digest.set_value(value);
}

/// What a grant fixture's three `EvidenceRef`s point at.
enum EvidenceFixture<'a> {
    /// Unsigned placeholder bytes; only `MetadataOnlyEvidenceVerifier` accepts
    /// them.
    Placeholder,
    /// All three references naming one CAS digest of in-toto DSSE bytes —
    /// finding 3's attack shape, and the shape a legitimate issuer uses when
    /// one envelope asserts every role.
    SharedInToto(&'a str),
}

fn set_evidence(
    mut evidence: execution_capnp::evidence_ref::Builder<'_>,
    fixture: &EvidenceFixture<'_>,
) {
    match fixture {
        EvidenceFixture::Placeholder => {
            evidence.set_media_type("application/test-evidence");
            set_digest(evidence.init_digest(), &"a".repeat(64));
        }
        EvidenceFixture::SharedInToto(digest) => {
            evidence.set_media_type("application/vnd.in-toto+json");
            set_digest(evidence.init_digest(), digest);
        }
    }
}

/// One APAS Handoff/v1 envelope signed by `signer`, asserting each
/// `(role, run_id)` pair as an in-toto subject.
fn signed_evidence(
    signer: &leyline_envelope::Ed25519RootSigner,
    subjects: &[(&str, &str)],
) -> Vec<u8> {
    let statement = leyline_envelope::Statement::new(
        subjects
            .iter()
            .map(|(role, run_id)| leyline_envelope::Subject::with_digest(*role, "blake3", *run_id))
            .collect(),
        "https://rosary.dev/Handoff/v1",
        serde_json::json!({"dispatchId": "run-01"}),
    );
    leyline_envelope::Envelope::sign(&statement, signer).to_json_vec()
}

fn unauthenticated(detail: String) -> ExecutionError {
    ExecutionError {
        code: leyline_runtime::ErrorCode::Unauthenticated,
        retryable: false,
        detail,
    }
}

/// Refuses evidence, accepts grants.
///
/// `authorize` runs two independent fail-closed gates — the grant signature
/// first, then each evidence reference. A fixture that refuses both makes
/// either gate sufficient to keep a test green, so a test named for one of
/// them passes when only the *other* fires. These two fixtures isolate the
/// gates so a name and its reason cannot drift apart.
struct RejectEvidenceOnly;

impl EvidenceVerifier for RejectEvidenceOnly {
    fn verify(
        &self,
        field: EvidenceField,
        _binding: &EvidenceBinding,
        _evidence: &EvidenceRef,
    ) -> Result<(), ExecutionError> {
        Err(unauthenticated(format!("unverified evidence: {field}")))
    }

    fn verify_grant(&self, _grant: &SignedGrant) -> Result<(), ExecutionError> {
        Ok(())
    }
}

/// Accepts evidence, refuses grants — the mirror of [`RejectEvidenceOnly`].
struct RejectGrantOnly;

impl EvidenceVerifier for RejectGrantOnly {
    fn verify(
        &self,
        _field: EvidenceField,
        _binding: &EvidenceBinding,
        _evidence: &EvidenceRef,
    ) -> Result<(), ExecutionError> {
        Ok(())
    }

    fn verify_grant(&self, _grant: &SignedGrant) -> Result<(), ExecutionError> {
        Err(unauthenticated("unverified grant signature".into()))
    }
}

struct FixtureEvidenceStore(Vec<u8>);

impl EvidenceStore for FixtureEvidenceStore {
    fn load(&self, _digest: &str) -> Result<Vec<u8>, ExecutionError> {
        Ok(self.0.clone())
    }
}

struct GrantFixture<'a> {
    capability: Option<(&'a str, &'a str)>,
    expires_at: u64,
    wall_time_ms: u64,
    confinement_algorithm: &'a str,
    confinement_value: &'a str,
    /// The confinement/v1 document the grant carries, if any.
    confinement_manifest: &'a str,
    workspaces: Vec<(&'a str, &'a str, Vec<execution_capnp::WorkspaceOperation>)>,
    evidence: EvidenceFixture<'a>,
    /// Issuer that signs the finished grant; `None` leaves it unsigned.
    signer: Option<&'a leyline_envelope::Ed25519RootSigner>,
    /// Isolation class the grant demands. Most fixtures want `MicroVm`; a
    /// tier-capability test needs `Native`, because the two enforce
    /// different ceilings.
    backend_class: execution_capnp::BackendClass,
}

fn grant_bytes(spec_bytes: &[u8], capability: bool, expires_at: u64, wall_time_ms: u64) -> Vec<u8> {
    let confinement_value = "b".repeat(64);
    grant_bytes_with_fixture(
        spec_bytes,
        GrantFixture {
            capability: capability.then_some((EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION)),
            expires_at,
            wall_time_ms,
            confinement_algorithm: "blake3-256",
            confinement_manifest: "",
            confinement_value: &confinement_value,
            workspaces: Vec::new(),
            evidence: EvidenceFixture::Placeholder,
            signer: None,
            backend_class: execution_capnp::BackendClass::MicroVm,
        },
    )
}

fn grant_bytes_with_fixture(spec_bytes: &[u8], fixture: GrantFixture<'_>) -> Vec<u8> {
    let spec_digest = canonical_digest(spec_bytes)
        .expect("canonical spec digest")
        .strip_prefix("blake3-256:")
        .expect("digest prefix")
        .to_owned();
    let mut message = Builder::new_default();
    let mut grant = message.init_root::<execution_capnp::run_grant::Builder<'_>>();
    grant.set_grant_id("grant-01");
    grant.set_expires_at_unix_ms(fixture.expires_at);
    grant.set_replay_key("replay-01");
    set_digest(grant.reborrow().init_run_spec_digest(), &spec_digest);
    set_evidence(grant.reborrow().init_issuer_evidence(), &fixture.evidence);
    set_evidence(
        grant.reborrow().init_workload_identity_evidence(),
        &fixture.evidence,
    );
    set_evidence(
        grant.reborrow().init_actor_provenance_evidence(),
        &fixture.evidence,
    );
    let mut confinement = grant.reborrow().init_confinement_digest();
    confinement.set_algorithm(fixture.confinement_algorithm);
    confinement.set_value(fixture.confinement_value);
    if !fixture.confinement_manifest.is_empty() {
        grant
            .reborrow()
            .set_confinement_manifest(fixture.confinement_manifest);
    }
    if fixture.wall_time_ms != 0 {
        grant
            .reborrow()
            .init_limits()
            .set_wall_time_ms(fixture.wall_time_ms);
    }
    grant.set_backend_class(fixture.backend_class);
    let capabilities = grant
        .reborrow()
        .init_capabilities(u32::from(fixture.capability.is_some()));
    if let Some((name, interface)) = fixture.capability {
        let mut entry = capabilities.get(0);
        entry.set_grant(name);
        entry.set_interface(interface);
    }
    let mut workspaces = grant
        .reborrow()
        .init_workspaces(fixture.workspaces.len() as u32);
    for (index, (name, graph_root, operations)) in fixture.workspaces.iter().enumerate() {
        let mut workspace = workspaces.reborrow().get(index as u32);
        workspace.set_name(name);
        set_digest(workspace.reborrow().init_graph_root(), graph_root);
        let mut granted_operations = workspace.init_operations(operations.len() as u32);
        for (operation_index, operation) in operations.iter().enumerate() {
            granted_operations.set(operation_index as u32, *operation);
        }
    }
    let mut bytes = Vec::new();
    capnp::serialize::write_message(&mut bytes, &message).expect("serialize grant");

    let Some(signer) = fixture.signer else {
        return bytes;
    };
    // An issuer signs the grant it just built: the canonical bytes with the
    // signature field cleared, which for a never-signed grant is what the
    // bytes above already canonicalize to.
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
    signature.set_key_id("issuer-01");
    signature.set_value(&value);
    let mut bytes = Vec::new();
    capnp::serialize::write_message(&mut bytes, &message).expect("serialize signed grant");
    bytes
}

// Unsigned fixture evidence is only valid through an explicit test adapter.
// Production `authorize` and `start_json` fail closed without a trust-domain
// verifier supplied by the embedding application.
fn authorize(
    spec_bytes: &[u8],
    grant_bytes: &[u8],
    policy: &AuthorizationPolicy,
) -> Result<leyline_runtime::authorization::AuthorizedExecution, ExecutionError> {
    authorize_with_verifier(
        spec_bytes,
        grant_bytes,
        policy,
        &MetadataOnlyEvidenceVerifier,
    )
}

#[test]
fn binds_grant_to_spec_and_derives_run_id() {
    let spec = spec_bytes();
    let grant = grant_bytes(&spec, true, 2_000, 0);
    let authorized = authorize(
        &spec,
        &grant,
        &AuthorizationPolicy {
            now_unix_ms: Some(1_000),
            required_backend: BackendClass::MicroVm,
            required_confinement_digest: None,
        },
    )
    .expect("valid bound grant");

    assert_eq!(authorized.grant_id, "grant-01");
    assert_eq!(authorized.replay_key, "replay-01");
    assert!(authorized.run_id.starts_with("run-"));
    assert_eq!(authorized.backend, BackendClass::MicroVm);
}

#[test]
fn rejects_expired_grants_and_missing_capability() {
    let spec = spec_bytes();
    let expired = grant_bytes(&spec, true, 1_000, 0);
    let error = authorize(
        &spec,
        &expired,
        &AuthorizationPolicy {
            now_unix_ms: Some(1_000),
            required_backend: BackendClass::MicroVm,
            required_confinement_digest: None,
        },
    )
    .expect_err("expired grant must be rejected");
    assert!(error.detail.contains("expired"));

    let missing = grant_bytes(&spec, false, 2_000, 0);
    let error = authorize(
        &spec,
        &missing,
        &AuthorizationPolicy {
            now_unix_ms: Some(1_000),
            required_backend: BackendClass::MicroVm,
            required_confinement_digest: None,
        },
    )
    .expect_err("grant without execution capability must be rejected");
    assert!(error.detail.contains("capability"));
}

#[test]
fn execution_capability_requires_both_grant_and_interface() {
    let spec = spec_bytes();
    let confinement = "b".repeat(64);
    for capability in [
        (EXECUTION_CAPABILITY, "wrong/interface"),
        ("wrong/grant", EXECUTION_SCHEMA_VERSION),
    ] {
        let grant = grant_bytes_with_fixture(
            &spec,
            GrantFixture {
                capability: Some(capability),
                expires_at: 2_000,
                wall_time_ms: 0,
                confinement_algorithm: "blake3-256",
                confinement_manifest: "",
                confinement_value: &confinement,
                workspaces: Vec::new(),
                evidence: EvidenceFixture::Placeholder,
                signer: None,
                backend_class: execution_capnp::BackendClass::MicroVm,
            },
        );
        let error = authorize(
            &spec,
            &grant,
            &AuthorizationPolicy {
                now_unix_ms: Some(1_000),
                required_backend: BackendClass::MicroVm,
                required_confinement_digest: None,
            },
        )
        .expect_err("partial capability match must fail closed");
        assert!(error.detail.contains("capability"));
    }
}

#[test]
fn digest_validation_rejects_each_malformed_component_independently() {
    let spec = spec_bytes();
    let valid = "b".repeat(64);
    let short = "b".repeat(63);
    let uppercase = "B".repeat(64);
    for (algorithm, value) in [
        ("sha256", valid.as_str()),
        ("blake3-256", short.as_str()),
        ("blake3-256", uppercase.as_str()),
    ] {
        let grant = grant_bytes_with_fixture(
            &spec,
            GrantFixture {
                capability: Some((EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION)),
                expires_at: 2_000,
                wall_time_ms: 0,
                confinement_algorithm: algorithm,
                confinement_manifest: "",
                confinement_value: value,
                workspaces: Vec::new(),
                evidence: EvidenceFixture::Placeholder,
                signer: None,
                backend_class: execution_capnp::BackendClass::MicroVm,
            },
        );
        let error = authorize(
            &spec,
            &grant,
            &AuthorizationPolicy {
                now_unix_ms: Some(1_000),
                required_backend: BackendClass::MicroVm,
                required_confinement_digest: None,
            },
        )
        .expect_err("malformed digest component must fail closed");
        assert!(error.detail.contains("lowercase blake3-256 digest"));
    }
}

#[test]
fn unsupported_requested_interface_is_rejected_after_binding() {
    let spec = spec_bytes_with_interface(Some("unsupported/interface"), 0);
    let grant = grant_bytes(&spec, true, 2_000, 0);
    let error = authorize(
        &spec,
        &grant,
        &AuthorizationPolicy {
            now_unix_ms: Some(1_000),
            required_backend: BackendClass::MicroVm,
            required_confinement_digest: None,
        },
    )
    .expect_err("a bound but unsupported interface must fail closed");
    assert_eq!(error.code, leyline_runtime::ErrorCode::UnsupportedBackend);
}

#[test]
fn verifier_adapter_can_fail_closed_before_backend_resolution() {
    let spec = spec_bytes();
    let grant = grant_bytes(&spec, true, 2_000, 0);
    let error = leyline_runtime::authorization::authorize_with_verifier(
        &spec,
        &grant,
        &AuthorizationPolicy {
            now_unix_ms: Some(1_000),
            required_backend: BackendClass::MicroVm,
            required_confinement_digest: None,
        },
        &RejectEvidenceOnly,
    )
    .expect_err("unverified upstream evidence must fail closed");
    assert_eq!(error.code, leyline_runtime::ErrorCode::Unauthenticated);
    assert!(
        error.detail.contains("issuerEvidence"),
        "this test is named for the evidence gate; it must fail there: {}",
        error.detail
    );
}

/// The mirror: a trust domain that accepts the evidence but refuses to
/// vouch for the grant's own fields must still fail closed.
#[test]
fn a_refused_grant_signature_alone_fails_closed() {
    let spec = spec_bytes();
    let grant = grant_bytes(&spec, true, 2_000, 0);
    let error = leyline_runtime::authorization::authorize_with_verifier(
        &spec,
        &grant,
        &AuthorizationPolicy {
            now_unix_ms: Some(1_000),
            required_backend: BackendClass::MicroVm,
            required_confinement_digest: None,
        },
        &RejectGrantOnly,
    )
    .expect_err("a grant no issuer vouches for must fail closed");
    assert_eq!(error.code, leyline_runtime::ErrorCode::Unauthenticated);
    assert!(
        error.detail.contains("grant signature"),
        "must fail at the grant gate: {}",
        error.detail
    );
}

/// `RejectUnverifiedEvidence` is what the shipped daemon installs unless the
/// operator passes `--allow-unverified-evidence`. Both of its refusals are
/// pinned here, individually: routing through `authorize` cannot distinguish
/// them, because whichever gate runs first satisfies the assertion and the
/// other is free to regress to `Ok(())` unnoticed.
#[test]
fn the_production_default_verifier_refuses_every_evidence_role() {
    for field in [
        EvidenceField::Issuer,
        EvidenceField::WorkloadIdentity,
        EvidenceField::ActorProvenance,
    ] {
        let error = leyline_runtime::RejectUnverifiedEvidence
            .verify(
                field,
                &binding_for(&other_run_id()),
                &EvidenceRef {
                    media_type: "application/vnd.in-toto+json".into(),
                    digest: format!("blake3-256:{}", "a".repeat(64)),
                },
            )
            .expect_err("the production default must not verify anything");
        assert_eq!(error.code, leyline_runtime::ErrorCode::Unauthenticated);
        assert!(error.detail.contains(field.as_str()), "{}", error.detail);
    }
}

#[test]
fn the_production_default_verifier_refuses_every_grant() {
    let error = leyline_runtime::RejectUnverifiedEvidence
        .verify_grant(&SignedGrant {
            signing_bytes: Vec::new(),
            signature: None,
        })
        .expect_err("the production default must not vouch for a grant");
    assert_eq!(error.code, leyline_runtime::ErrorCode::Unauthenticated);
    assert!(error.detail.contains("signature"), "{}", error.detail);
}

/// `current_unix_ms` is the seam that keeps grant expiry enforceable across a
/// daemon's lifetime. Every expiry test in this file derives both its
/// deadline and its comparison from this one call, so a constant would
/// satisfy all of them while making expiry unenforced in production —
/// exactly the frozen-clock defect the seam was introduced to fix.
#[test]
fn current_unix_ms_reads_a_real_wall_clock() {
    assert!(
        leyline_runtime::authorization::current_unix_ms() > 1_767_225_600_000,
        "expiry needs a real clock, not a fixed value"
    );
}

#[test]
fn default_authorization_fails_closed_without_embedding_trust() {
    let spec = spec_bytes();
    let grant = grant_bytes(&spec, true, 2_000, 0);
    let error = leyline_runtime::authorization::authorize(
        &spec,
        &grant,
        &AuthorizationPolicy {
            now_unix_ms: Some(1_000),
            required_backend: BackendClass::MicroVm,
            required_confinement_digest: None,
        },
    )
    .expect_err("unsigned fixture evidence must not pass the production default");
    assert_eq!(error.code, leyline_runtime::ErrorCode::Unauthenticated);
    // Both of the default's gates would refuse this grant, and the first one
    // reached decides the message — so this asserts only that the entry point
    // fails closed. Which gate refuses is pinned individually below, in
    // `the_production_default_verifier_refuses_*`.
    assert!(
        error.detail.contains("no trusted verifier"),
        "{}",
        error.detail
    );
}

/// A run identity that is well-formed but belongs to no fixture here.
fn other_run_id() -> String {
    format!("run-{}", "f".repeat(64))
}

fn binding_for(run_id: &str) -> EvidenceBinding {
    EvidenceBinding {
        run_id: run_id.to_owned(),
        spec_digest: format!("blake3-256:{}", "0".repeat(64)),
    }
}

fn cas_verifier(
    bytes: Vec<u8>,
    signer: &leyline_envelope::Ed25519RootSigner,
) -> leyline_runtime::CasDsseEvidenceVerifier<FixtureEvidenceStore> {
    leyline_runtime::CasDsseEvidenceVerifier::new(
        std::sync::Arc::new(FixtureEvidenceStore(bytes)),
        vec![signer.verifying_key()],
    )
}

fn in_toto_ref(bytes: &[u8]) -> EvidenceRef {
    EvidenceRef {
        media_type: "application/vnd.in-toto+json".into(),
        digest: format!("blake3-256:{}", blake3::hash(bytes).to_hex()),
    }
}

#[test]
fn cas_dsse_verifier_binds_and_verifies_apas_evidence() {
    let signer = leyline_envelope::Ed25519RootSigner::from_seed(&[11u8; 32]);
    let run_id = other_run_id();
    let bytes = signed_evidence(&signer, &[("actorProvenanceEvidence", &run_id)]);
    let evidence = in_toto_ref(&bytes);
    cas_verifier(bytes, &signer)
        .verify(
            EvidenceField::ActorProvenance,
            &binding_for(&run_id),
            &evidence,
        )
        .expect("valid APAS DSSE evidence naming this run and role");
}

#[test]
fn cas_dsse_verifier_rejects_a_signed_non_apas_statement() {
    let signer = leyline_envelope::Ed25519RootSigner::from_seed(&[12u8; 32]);
    let statement = leyline_envelope::Statement::new(
        Vec::new(),
        "https://example.test/Unrelated/v1",
        serde_json::json!({"dispatchId":"run-01"}),
    );
    let bytes = leyline_envelope::Envelope::sign(&statement, &signer).to_json_vec();
    let evidence = in_toto_ref(&bytes);
    let error = cas_verifier(bytes, &signer)
        .verify(
            EvidenceField::ActorProvenance,
            &binding_for(&other_run_id()),
            &evidence,
        )
        .expect_err("a signed unrelated statement is not APAS evidence");
    assert_eq!(error.code, leyline_runtime::ErrorCode::Unauthenticated);
    assert!(error.detail.contains("not APAS"));
}

/// Finding 3: a trusted envelope from some *other* run must not authorize
/// this one. Without a subject binding, every envelope in a trusted catalog
/// authorizes every run.
#[test]
fn cas_dsse_verifier_rejects_evidence_bound_to_another_run() {
    let signer = leyline_envelope::Ed25519RootSigner::from_seed(&[13u8; 32]);
    let bytes = signed_evidence(&signer, &[("actorProvenanceEvidence", &other_run_id())]);
    let evidence = in_toto_ref(&bytes);
    let this_run = format!("run-{}", "1".repeat(64));
    let error = cas_verifier(bytes, &signer)
        .verify(
            EvidenceField::ActorProvenance,
            &binding_for(&this_run),
            &evidence,
        )
        .expect_err("evidence naming another run must not authorize this one");
    assert_eq!(error.code, leyline_runtime::ErrorCode::Unauthenticated);
    assert!(error.detail.contains("does not authorize"));
}

/// The three references are structurally identical, so a statement that
/// asserts one role must not silently satisfy the other two.
#[test]
fn cas_dsse_verifier_rejects_a_statement_asserting_a_different_role() {
    let signer = leyline_envelope::Ed25519RootSigner::from_seed(&[14u8; 32]);
    let run_id = other_run_id();
    let bytes = signed_evidence(&signer, &[("issuerEvidence", &run_id)]);
    let evidence = in_toto_ref(&bytes);
    let error = cas_verifier(bytes, &signer)
        .verify(
            EvidenceField::WorkloadIdentity,
            &binding_for(&run_id),
            &evidence,
        )
        .expect_err("an issuer assertion is not a workload identity assertion");
    assert_eq!(error.code, leyline_runtime::ErrorCode::Unauthenticated);
    assert!(error.detail.contains("workloadIdentityEvidence"));
}

/// ADR-012 R4: the subject digest is gated against the run-identity shape at
/// the boundary. A malformed value is a malformed statement, reported
/// separately from one that names a different run.
#[test]
fn cas_dsse_verifier_rejects_a_subject_digest_that_is_not_a_run_identity() {
    let signer = leyline_envelope::Ed25519RootSigner::from_seed(&[15u8; 32]);
    for malformed in [
        &"a".repeat(64),                    // bare hex, unprefixed
        &format!("run-{}", "a".repeat(63)), // one nibble short
        &format!("run-{}", "A".repeat(64)), // uppercase
        &"run-".to_string(),
    ] {
        let bytes = signed_evidence(&signer, &[("issuerEvidence", malformed)]);
        let evidence = in_toto_ref(&bytes);
        let error = cas_verifier(bytes, &signer)
            .verify(
                EvidenceField::Issuer,
                &binding_for(&other_run_id()),
                &evidence,
            )
            .expect_err("a subject digest that is not a run identity must fail closed");
        assert_eq!(error.code, leyline_runtime::ErrorCode::Unauthenticated);
        assert!(
            error.detail.contains("not a run identity"),
            "unexpected detail for {malformed:?}: {}",
            error.detail
        );
    }
}

/// Finding 3's failure scenario end to end: a caller who knows the digest of
/// one trusted envelope points all three `EvidenceRef`s at it. The envelope
/// asserts only the issuer role, so authorization must stop at the next
/// field rather than accept the same bytes three times.
#[test]
fn one_trusted_envelope_cannot_satisfy_every_evidence_field() {
    let signer = leyline_envelope::Ed25519RootSigner::from_seed(&[16u8; 32]);
    let spec = spec_bytes();
    let spec_digest = canonical_digest(&spec).expect("canonical spec digest");
    let run_id = derive_run_id(&spec_digest, "grant-01", "replay-01");
    let bytes = signed_evidence(&signer, &[("issuerEvidence", &run_id)]);
    let hex = blake3::hash(&bytes).to_hex().to_string();
    let confinement = "b".repeat(64);
    let grant = grant_bytes_with_fixture(
        &spec,
        GrantFixture {
            capability: Some((EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION)),
            expires_at: 2_000,
            wall_time_ms: 0,
            confinement_algorithm: "blake3-256",
            confinement_manifest: "",
            confinement_value: &confinement,
            workspaces: Vec::new(),
            evidence: EvidenceFixture::SharedInToto(&hex),
            signer: Some(&signer),
            backend_class: execution_capnp::BackendClass::MicroVm,
        },
    );
    let error = authorize_with_verifier(
        &spec,
        &grant,
        &AuthorizationPolicy {
            now_unix_ms: Some(1_000),
            required_backend: BackendClass::MicroVm,
            required_confinement_digest: None,
        },
        &cas_verifier(bytes, &signer),
    )
    .expect_err("one issuer assertion must not stand in for all three roles");
    assert_eq!(error.code, leyline_runtime::ErrorCode::Unauthenticated);
    assert!(error.detail.contains("workloadIdentityEvidence"));
}

/// The binding is satisfiable: an issuer that asserts all three roles for
/// this run authorizes it, and the run it authorizes is the one the caller
/// can derive locally.
#[test]
fn evidence_asserting_every_role_for_this_run_authorizes_it() {
    let signer = leyline_envelope::Ed25519RootSigner::from_seed(&[17u8; 32]);
    let spec = spec_bytes();
    let spec_digest = canonical_digest(&spec).expect("canonical spec digest");
    let run_id = derive_run_id(&spec_digest, "grant-01", "replay-01");
    let bytes = signed_evidence(
        &signer,
        &[
            ("issuerEvidence", &run_id),
            ("workloadIdentityEvidence", &run_id),
            ("actorProvenanceEvidence", &run_id),
        ],
    );
    let hex = blake3::hash(&bytes).to_hex().to_string();
    let confinement = "b".repeat(64);
    let grant = grant_bytes_with_fixture(
        &spec,
        GrantFixture {
            capability: Some((EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION)),
            expires_at: 2_000,
            wall_time_ms: 0,
            confinement_algorithm: "blake3-256",
            confinement_manifest: "",
            confinement_value: &confinement,
            workspaces: Vec::new(),
            evidence: EvidenceFixture::SharedInToto(&hex),
            signer: Some(&signer),
            backend_class: execution_capnp::BackendClass::MicroVm,
        },
    );
    let authorized = authorize_with_verifier(
        &spec,
        &grant,
        &AuthorizationPolicy {
            now_unix_ms: Some(1_000),
            required_backend: BackendClass::MicroVm,
            required_confinement_digest: None,
        },
        &cas_verifier(bytes, &signer),
    )
    .expect("evidence asserting every role for this run");
    assert_eq!(authorized.run_id, run_id);
}

#[test]
fn rejects_grant_bound_to_different_spec() {
    let spec = spec_bytes();
    let other_spec = spec_bytes_with_interface(Some("different/interface"), 0);
    let grant = grant_bytes(&other_spec, true, 2_000, 0);
    let error = authorize(
        &spec,
        &grant,
        &AuthorizationPolicy {
            now_unix_ms: Some(1_000),
            required_backend: BackendClass::MicroVm,
            required_confinement_digest: None,
        },
    )
    .expect_err("digest from a different spec must be rejected");
    assert!(error.detail.contains("does not bind"));
}

#[test]
fn rejects_grant_that_widens_requested_limits() {
    let spec = spec_bytes_with_interface(None, 100);
    let grant = grant_bytes(&spec, true, 2_000, 101);
    let error = authorize(
        &spec,
        &grant,
        &AuthorizationPolicy {
            now_unix_ms: Some(1_000),
            required_backend: BackendClass::MicroVm,
            required_confinement_digest: None,
        },
    )
    .expect_err("widened grant must be rejected");
    assert!(error.detail.contains("widens"));
}

#[test]
fn grant_limit_equal_to_requested_boundary_is_valid_but_zero_is_not() {
    let spec = spec_bytes_with_interface(None, 100);
    let equal = grant_bytes(&spec, true, 2_000, 100);
    authorize(
        &spec,
        &equal,
        &AuthorizationPolicy {
            now_unix_ms: Some(1_000),
            required_backend: BackendClass::MicroVm,
            required_confinement_digest: None,
        },
    )
    .expect("an equal resolved limit does not widen authority");

    let unresolved = grant_bytes(&spec, true, 2_000, 0);
    let error = authorize(
        &spec,
        &unresolved,
        &AuthorizationPolicy {
            now_unix_ms: Some(1_000),
            required_backend: BackendClass::MicroVm,
            required_confinement_digest: None,
        },
    )
    .expect_err("zero must not erase an explicitly requested ceiling");
    assert!(error.detail.contains("widens"));
}

#[test]
fn workspace_grants_require_exact_identity_cardinality_and_unique_names() {
    let confinement = "b".repeat(64);
    let policy = AuthorizationPolicy {
        now_unix_ms: Some(1_000),
        required_backend: BackendClass::MicroVm,
        required_confinement_digest: None,
    };
    let read = vec![execution_capnp::WorkspaceOperation::Read];

    let spec = spec_bytes_with_details(None, 0, &[("repo", &"a".repeat(64))]);
    let exact = grant_bytes_with_fixture(
        &spec,
        GrantFixture {
            capability: Some((EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION)),
            expires_at: 2_000,
            wall_time_ms: 0,
            confinement_algorithm: "blake3-256",
            confinement_manifest: "",
            confinement_value: &confinement,
            workspaces: vec![("repo", &"a".repeat(64), read.clone())],
            evidence: EvidenceFixture::Placeholder,
            signer: None,
            backend_class: execution_capnp::BackendClass::MicroVm,
        },
    );
    authorize(&spec, &exact, &policy).expect("exact workspace grant");

    let extra = grant_bytes_with_fixture(
        &spec,
        GrantFixture {
            capability: Some((EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION)),
            expires_at: 2_000,
            wall_time_ms: 0,
            confinement_algorithm: "blake3-256",
            confinement_manifest: "",
            confinement_value: &confinement,
            workspaces: vec![
                ("repo", &"a".repeat(64), read.clone()),
                ("extra", &"b".repeat(64), read.clone()),
            ],
            evidence: EvidenceFixture::Placeholder,
            signer: None,
            backend_class: execution_capnp::BackendClass::MicroVm,
        },
    );
    authorize(&spec, &extra, &policy)
        .expect_err("an unrequested workspace must fail cardinality validation");

    let wrong_identity = grant_bytes_with_fixture(
        &spec,
        GrantFixture {
            capability: Some((EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION)),
            expires_at: 2_000,
            wall_time_ms: 0,
            confinement_algorithm: "blake3-256",
            confinement_manifest: "",
            confinement_value: &confinement,
            workspaces: vec![("repo", &"c".repeat(64), read.clone())],
            evidence: EvidenceFixture::Placeholder,
            signer: None,
            backend_class: execution_capnp::BackendClass::MicroVm,
        },
    );
    authorize(&spec, &wrong_identity, &policy)
        .expect_err("same-sized workspace sets must still match content identity");

    let duplicate_spec = spec_bytes_with_details(
        None,
        0,
        &[("repo", &"a".repeat(64)), ("repo", &"b".repeat(64))],
    );
    let duplicate = grant_bytes_with_fixture(
        &duplicate_spec,
        GrantFixture {
            capability: Some((EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION)),
            expires_at: 2_000,
            wall_time_ms: 0,
            confinement_algorithm: "blake3-256",
            confinement_manifest: "",
            confinement_value: &confinement,
            workspaces: vec![
                ("repo", &"a".repeat(64), read.clone()),
                ("repo", &"b".repeat(64), read),
            ],
            evidence: EvidenceFixture::Placeholder,
            signer: None,
            backend_class: execution_capnp::BackendClass::MicroVm,
        },
    );
    authorize(&duplicate_spec, &duplicate, &policy)
        .expect_err("duplicate workspace names are ambiguous authority");
}

#[test]
fn rejects_grant_with_a_confinement_policy_the_backend_will_not_enforce() {
    let spec = spec_bytes();
    let grant = grant_bytes(&spec, true, 2_000, 0);
    let error = authorize(
        &spec,
        &grant,
        &AuthorizationPolicy {
            now_unix_ms: Some(1_000),
            required_backend: BackendClass::MicroVm,
            required_confinement_digest: Some(format!("blake3-256:{}", "c".repeat(64))),
        },
    )
    .expect_err("confinement digest mismatch must fail before resolution");
    assert_eq!(
        error.code,
        leyline_runtime::ErrorCode::IdentityPolicyMismatch
    );
}

struct RecordingBackend;

impl Backend for RecordingBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            backend_id: "test/1".into(),
            backend_class: BackendClass::MicroVm,
            available: true,
            enforced: leyline_runtime::EnforcedCeilings {
                wall_time: leyline_runtime::CeilingMechanism::Supervisor,
                vcpus: leyline_runtime::CeilingMechanism::Hypervisor,
                memory: leyline_runtime::CeilingMechanism::Hypervisor,
            },
        }
    }

    fn start(&self, request: &ExecutionRequest) -> Result<BackendRun, ExecutionError> {
        Ok(BackendRun {
            backend_id: format!("test/{}", request.run_id),
        })
    }

    fn cancel(&self, _run_id: &str) -> Result<bool, ExecutionError> {
        Ok(true)
    }
}

struct TestResolver;

struct CompletingBackend;

impl Backend for CompletingBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            backend_id: "completing/1".into(),
            backend_class: BackendClass::MicroVm,
            available: true,
            enforced: leyline_runtime::EnforcedCeilings {
                wall_time: leyline_runtime::CeilingMechanism::Supervisor,
                vcpus: leyline_runtime::CeilingMechanism::Hypervisor,
                memory: leyline_runtime::CeilingMechanism::Hypervisor,
            },
        }
    }

    fn start(&self, _request: &ExecutionRequest) -> Result<BackendRun, ExecutionError> {
        Ok(BackendRun {
            backend_id: "completing/1".into(),
        })
    }

    fn poll(&self, _run_id: &str) -> Result<Option<BackendRunStatus>, ExecutionError> {
        Ok(Some(BackendRunStatus::Succeeded))
    }

    fn cancel(&self, _run_id: &str) -> Result<bool, ExecutionError> {
        Ok(true)
    }
}

impl ExecutionResolver for TestResolver {
    fn resolve(
        &self,
        authorized: &leyline_runtime::authorization::AuthorizedExecution,
    ) -> Result<ExecutionRequest, ExecutionError> {
        Ok(ExecutionRequest {
            run_id: authorized.run_id.clone(),
            replay_key: authorized.replay_key.clone(),
            rootfs: leyline_runtime::DigestRef {
                algorithm: "blake3-256".into(),
                value: "a".repeat(64),
            },
            executable: "usr/bin/true".into(),
            arguments: vec![],
            public_environment: BTreeMap::new(),
            allowed_egress: authorized.allowed_egress.clone(),
            // Carried, not chosen. This fixture used to write `String::new()`
            // here, which is precisely the resolver behaviour `service.rs` now
            // refuses: dropping the digest silently disabled ADR-0035's drift
            // check for the run, because both backends gate that comparison on
            // the field being non-empty. Five tests in this file passed
            // vacuously as a result.
            confinement_digest: authorized.confinement_digest.clone(),
            confinement_manifest: authorized.confinement_manifest.clone(),
            limits: ResourceLimits {
                vcpus: 1,
                memory_mib: 128,
                wall_time_ms: 1_000,
            },
        })
    }
}

#[test]
fn service_schema_entrypoint_authorizes_before_resolving() {
    let spec = spec_bytes();
    let grant = grant_bytes(&spec, true, 2_000, 0);
    let service = ExecutionService::new(RecordingBackend);
    service
        .provision(BackendClass::MicroVm, "provision-01")
        .expect("provision backend");
    let record = service
        .start_authorized(
            &spec,
            &grant,
            &AuthorizationPolicy {
                now_unix_ms: Some(1_000),
                required_backend: BackendClass::MicroVm,
                required_confinement_digest: None,
            },
            &TestResolver,
        )
        .expect("schema request should enter shared lifecycle");
    assert!(record.run_id.starts_with("run-"));
    assert_eq!(record.state, leyline_runtime::RunState::Running);
    let inspection = service.inspect(&record.run_id, 0).expect("inspect events");
    assert_eq!(inspection.events.len(), 4);
    assert_eq!(
        inspection.events[0].state,
        leyline_runtime::RunState::Accepted
    );
    assert_eq!(
        inspection.events[3].state,
        leyline_runtime::RunState::Running
    );
}

#[test]
fn schema_start_requires_explicit_backend_provisioning() {
    let spec = spec_bytes();
    let grant = grant_bytes(&spec, true, 2_000, 0);
    let service = ExecutionService::new(RecordingBackend);
    let error = service
        .start_authorized(
            &spec,
            &grant,
            &AuthorizationPolicy {
                now_unix_ms: Some(1_000),
                required_backend: BackendClass::MicroVm,
                required_confinement_digest: None,
            },
            &TestResolver,
        )
        .expect_err("unprovisioned backend must fail closed");
    assert_eq!(error.code, leyline_runtime::ErrorCode::NotProvisioned);
}

#[test]
fn natural_backend_completion_can_collect_a_schema_receipt() {
    let spec = spec_bytes();
    let grant = grant_bytes(&spec, true, 2_000, 0);
    let service = ExecutionService::new(CompletingBackend);
    service
        .provision(BackendClass::MicroVm, "provision-completing")
        .expect("provision backend");
    let record = service
        .start_authorized(
            &spec,
            &grant,
            &AuthorizationPolicy {
                now_unix_ms: Some(1_000),
                required_backend: BackendClass::MicroVm,
                required_confinement_digest: None,
            },
            &TestResolver,
        )
        .expect("schema request should start");

    let receipt = service
        .collect(&record.run_id)
        .expect("collect terminal receipt");
    assert_eq!(receipt.terminal_state, leyline_runtime::RunState::Succeeded);
    assert!(receipt.event_log_root.starts_with("blake3-256:"));
}

fn spec_json(bytes: &[u8]) -> String {
    let mut input = bytes;
    let message = capnp::serialize::read_message(&mut input, ReaderOptions::new()).unwrap();
    capnp_json::to_json(
        message
            .get_root::<execution_capnp::run_spec::Reader<'_>>()
            .unwrap(),
    )
    .unwrap()
}

fn grant_json(bytes: &[u8]) -> String {
    let mut input = bytes;
    let message = capnp::serialize::read_message(&mut input, ReaderOptions::new()).unwrap();
    capnp_json::to_json(
        message
            .get_root::<execution_capnp::run_grant::Reader<'_>>()
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn json_adapter_uses_generated_input_and_output_shapes() {
    let spec = spec_bytes();
    let grant = grant_bytes(&spec, true, 2_000, 0);
    let input = json!({
        "spec": serde_json::from_str::<serde_json::Value>(&spec_json(&spec)).unwrap(),
        "grant": serde_json::from_str::<serde_json::Value>(&grant_json(&grant)).unwrap(),
    })
    .to_string();
    let service = ExecutionService::new(RecordingBackend);
    let policy = AuthorizationPolicy {
        now_unix_ms: Some(1_000),
        required_backend: BackendClass::MicroVm,
        required_confinement_digest: None,
    };
    let provision = leyline_runtime::transport::provision_json(
        &service,
        r#"{"backendClass":"microVm","idempotencyKey":"provision-01"}"#,
    )
    .expect("provision JSON");
    assert!(provision.contains("provisioned"));
    let error = start_json(&service, &input, &policy, &TestResolver)
        .expect_err("the default JSON surface must reject unsigned evidence");
    assert_eq!(error.code, leyline_runtime::ErrorCode::Unauthenticated);
    let start = start_json_with_verifier(
        &service,
        &input,
        &policy,
        &TestResolver,
        &MetadataOnlyEvidenceVerifier,
    )
    .expect("start JSON");
    assert!(start.contains("run-"));
    let status = status_json(&service, r#"{"runId":""}"#).expect("status JSON");
    assert!(status.contains("test/1"));
    let capabilities = capabilities_json(&service).expect("capabilities JSON");
    assert!(capabilities.contains("cloister/execution/v1"));

    let start_value: serde_json::Value = serde_json::from_str(&start).unwrap();
    let run_id = start_value["runId"].as_str().unwrap();
    let inspection: serde_json::Value = serde_json::from_str(
        &inspect_json(
            &service,
            &json!({"runId": run_id, "afterSequence": 2}).to_string(),
        )
        .expect("inspect JSON"),
    )
    .expect("inspect output JSON");
    assert_eq!(inspection["runId"], run_id);
    assert_eq!(inspection["events"].as_array().unwrap().len(), 2);

    let cancellation: serde_json::Value = serde_json::from_str(
        &cancel_json(
            &service,
            &json!({"runId": run_id, "idempotencyKey": "cancel-01"}).to_string(),
        )
        .expect("cancel JSON"),
    )
    .expect("cancel output JSON");
    assert_eq!(cancellation["runId"], run_id);
    assert_eq!(cancellation["state"], "cancelled");
    let receipt = collect_json(&service, &json!({"runId": run_id}).to_string())
        .expect("collect receipt JSON");
    let receipt: serde_json::Value = serde_json::from_str(&receipt).expect("receipt JSON");
    assert_eq!(
        receipt["receipt"]["eventLogRoot"]["algorithm"],
        "blake3-256"
    );
    assert_eq!(
        receipt["receipt"]["eventLogRoot"]["value"]
            .as_str()
            .expect("event root value")
            .len(),
        64
    );
    let cleanup =
        cleanup_json(&service, &json!({"runId": run_id}).to_string()).expect("cleanup JSON");
    assert!(cleanup.contains("cleaned"));
}

#[test]
fn capabilities_projection_uses_declared_backend_class() {
    struct NativeBackend;
    impl Backend for NativeBackend {
        fn capabilities(&self) -> BackendCapabilities {
            BackendCapabilities {
                backend_id: "native-nono/1".into(),
                backend_class: BackendClass::Native,
                available: false,
                enforced: leyline_runtime::EnforcedCeilings {
                    wall_time: leyline_runtime::CeilingMechanism::Supervisor,
                    vcpus: leyline_runtime::CeilingMechanism::Unenforced,
                    memory: leyline_runtime::CeilingMechanism::Unenforced,
                },
            }
        }

        fn start(
            &self,
            _request: &ExecutionRequest,
        ) -> Result<leyline_runtime::BackendRun, ExecutionError> {
            unreachable!("unavailable backend must not start")
        }

        fn cancel(&self, _run_id: &str) -> Result<bool, ExecutionError> {
            Ok(false)
        }
    }

    let capabilities =
        capabilities_json(&ExecutionService::new(NativeBackend)).expect("capabilities JSON");
    assert!(capabilities.contains("backend/native"));
    assert!(!capabilities.contains("backend/microvm"));
    assert!(capabilities.contains("unavailable"));
}

// ── run identity is a content-addressed name ────────────────────────────────
//
// `run_id` is the handle a consumer derives locally to address a run —
// ideally before `start` returns, so inspect/collect/cancel can be pipelined
// over a stateless transport. That only works if the id is a function of the
// authorized *content*, computable by a second implementation. Two properties
// are load-bearing: framing independence, and unambiguous field boundaries.

/// Re-encode one RunSpec with a deliberately different segment layout. The
/// canonical form — and therefore the content identity — is unchanged.
fn reframed_spec(bytes: &[u8]) -> Vec<u8> {
    let reader = capnp::serialize::read_message(&mut &bytes[..], ReaderOptions::new())
        .expect("read spec message");
    let source = reader
        .get_root::<execution_capnp::run_spec::Reader<'_>>()
        .expect("spec root");
    let mut message = Builder::new(capnp::message::HeapAllocator::new().first_segment_words(1));
    message.set_root(source).expect("copy spec root");
    let mut out = Vec::new();
    capnp::serialize::write_message(&mut out, &message).expect("serialize reframed spec");
    out
}

#[test]
fn run_id_names_spec_content_not_its_capnp_framing() {
    let single = spec_bytes();
    let reframed = reframed_spec(&single);
    assert_ne!(
        single, reframed,
        "fixture must actually differ in wire framing"
    );
    assert_eq!(
        canonical_digest(&single).expect("canonical digest"),
        canonical_digest(&reframed).expect("canonical digest"),
        "fixture must remain one content identity"
    );

    let policy = AuthorizationPolicy {
        now_unix_ms: Some(1_000),
        required_backend: BackendClass::MicroVm,
        required_confinement_digest: None,
    };
    let from_single = authorize(&single, &grant_bytes(&single, true, 2_000, 0), &policy)
        .expect("authorize single-segment spec");
    let from_reframed = authorize(&reframed, &grant_bytes(&reframed, true, 2_000, 0), &policy)
        .expect("authorize reframed spec");

    assert_eq!(
        from_single.spec_digest, from_reframed.spec_digest,
        "spec digest already binds canonical content"
    );
    assert_eq!(
        from_single.run_id, from_reframed.run_id,
        "run_id must name the same content under a different encoding"
    );
}

#[test]
fn run_id_separates_the_grant_id_from_the_replay_key() {
    // ("grant-ab", "c…") and ("grant-a", "bc…") are distinct authorities. A
    // bare concatenation of the two fields would give them one run identity.
    let spec = spec_bytes();
    let policy = AuthorizationPolicy {
        now_unix_ms: Some(1_000),
        required_backend: BackendClass::MicroVm,
        required_confinement_digest: None,
    };
    let spec_digest = canonical_digest(&spec).expect("canonical digest");
    assert_ne!(
        leyline_runtime::authorization::derive_run_id(&spec_digest, "ab", "c"),
        leyline_runtime::authorization::derive_run_id(&spec_digest, "a", "bc"),
        "adjacent identity fields must not be ambiguous at their boundary"
    );
    // The pinned clock keeps this a pure identity assertion.
    let _ = policy;
}

// ── grant expiry is evaluated against the current clock ─────────────────────

#[test]
fn the_default_policy_does_not_capture_a_construction_time_clock() {
    // A daemon builds one policy at startup and reuses it for every request
    // (see `leyline execution-daemon`). If that policy carried the timestamp
    // it was constructed with, a grant that expired seconds after startup
    // would still authorize days later.
    assert_eq!(
        AuthorizationPolicy::default().now_unix_ms,
        None,
        "a reused policy must sample the wall clock per authorization"
    );
}

#[test]
fn a_reused_default_policy_rejects_a_grant_that_expired_before_now() {
    let spec = spec_bytes();
    let policy = AuthorizationPolicy {
        required_backend: BackendClass::MicroVm,
        ..Default::default()
    };
    let now = leyline_runtime::authorization::current_unix_ms();

    let expired = grant_bytes(&spec, true, now - 1, 0);
    let error = authorize(&spec, &expired, &policy).expect_err("expired grant must be rejected");
    assert!(error.detail.contains("RunGrant has expired"), "{error:?}");

    let live = grant_bytes(&spec, true, now + 600_000, 0);
    authorize(&spec, &live, &policy).expect("an unexpired grant must still authorize");
}

/// A grant fixture carrying signed, run-bound evidence for every role, so a
/// signature test exercises only the signature.
fn signed_grant_fixture<'a>(
    spec: &[u8],
    evidence_hex: &'a str,
    confinement: &'a str,
    signer: Option<&'a leyline_envelope::Ed25519RootSigner>,
    expires_at: u64,
) -> Vec<u8> {
    grant_bytes_with_fixture(
        spec,
        GrantFixture {
            capability: Some((EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION)),
            expires_at,
            wall_time_ms: 0,
            confinement_algorithm: "blake3-256",
            confinement_manifest: "",
            confinement_value: confinement,
            workspaces: Vec::new(),
            evidence: EvidenceFixture::SharedInToto(evidence_hex),
            signer,
            backend_class: execution_capnp::BackendClass::MicroVm,
        },
    )
}

/// Evidence bytes asserting all three roles for the run `grant-01` /
/// `replay-01` names over `spec`, plus their CAS digest hex.
fn all_roles_evidence(
    spec: &[u8],
    signer: &leyline_envelope::Ed25519RootSigner,
) -> (Vec<u8>, String) {
    let spec_digest = canonical_digest(spec).expect("canonical spec digest");
    let run_id = derive_run_id(&spec_digest, "grant-01", "replay-01");
    let bytes = signed_evidence(
        signer,
        &[
            ("issuerEvidence", &run_id),
            ("workloadIdentityEvidence", &run_id),
            ("actorProvenanceEvidence", &run_id),
        ],
    );
    let hex = blake3::hash(&bytes).to_hex().to_string();
    (bytes, hex)
}

fn signed_grant_policy() -> AuthorizationPolicy {
    AuthorizationPolicy {
        now_unix_ms: Some(1_000),
        required_backend: BackendClass::MicroVm,
        required_confinement_digest: None,
    }
}

/// A grant whose signature verifies under a trusted key is authority; the
/// same fields without one are not. Nothing else cryptographically ties
/// `capabilities`, `limits`, `backendClass` or `expiresAtUnixMs` to the
/// issuer.
#[test]
fn a_signed_grant_authorizes_and_an_unsigned_one_does_not() {
    let signer = leyline_envelope::Ed25519RootSigner::from_seed(&[31u8; 32]);
    let spec = spec_bytes();
    let (evidence, hex) = all_roles_evidence(&spec, &signer);
    let confinement = "b".repeat(64);

    let signed = signed_grant_fixture(&spec, &hex, &confinement, Some(&signer), 2_000);
    authorize_with_verifier(
        &spec,
        &signed,
        &signed_grant_policy(),
        &cas_verifier(evidence.clone(), &signer),
    )
    .expect("a grant signed by a trusted issuer is authority");

    let unsigned = signed_grant_fixture(&spec, &hex, &confinement, None, 2_000);
    let error = authorize_with_verifier(
        &spec,
        &unsigned,
        &signed_grant_policy(),
        &cas_verifier(evidence, &signer),
    )
    .expect_err("an unsigned grant carries no issuer authority");
    assert_eq!(error.code, leyline_runtime::ErrorCode::Unauthenticated);
    assert!(error.detail.contains("signature"), "{}", error.detail);
}

/// The signature covers the whole grant, so widening any field after signing
/// invalidates it. `expiresAtUnixMs` is the field finding 1 showed is worth
/// the most to an attacker.
#[test]
fn editing_a_signed_grant_invalidates_its_signature() {
    let signer = leyline_envelope::Ed25519RootSigner::from_seed(&[32u8; 32]);
    let spec = spec_bytes();
    let (evidence, hex) = all_roles_evidence(&spec, &signer);
    let confinement = "b".repeat(64);
    let signed = signed_grant_fixture(&spec, &hex, &confinement, Some(&signer), 2_000);

    // Re-encode the signed grant with a later expiry, carrying the original
    // signature forward — the edit an interposed caller would make.
    let message = capnp::serialize::read_message(&mut &signed[..], ReaderOptions::new())
        .expect("read signed grant");
    let reader = message
        .get_root::<execution_capnp::run_grant::Reader<'_>>()
        .expect("grant root");
    let mut forged = Builder::new_default();
    forged.set_root(reader).expect("copy grant");
    forged
        .get_root::<execution_capnp::run_grant::Builder<'_>>()
        .expect("forged root")
        .set_expires_at_unix_ms(9_999_999);
    let mut forged_bytes = Vec::new();
    capnp::serialize::write_message(&mut forged_bytes, &forged).expect("serialize forged grant");

    let error = authorize_with_verifier(
        &spec,
        &forged_bytes,
        &signed_grant_policy(),
        &cas_verifier(evidence, &signer),
    )
    .expect_err("an edited grant must not verify under the original signature");
    assert_eq!(error.code, leyline_runtime::ErrorCode::Unauthenticated);
    assert!(error.detail.contains("signature"), "{}", error.detail);
}

#[test]
fn a_grant_signed_by_an_untrusted_key_is_rejected() {
    let issuer = leyline_envelope::Ed25519RootSigner::from_seed(&[33u8; 32]);
    let impostor = leyline_envelope::Ed25519RootSigner::from_seed(&[34u8; 32]);
    let spec = spec_bytes();
    let (evidence, hex) = all_roles_evidence(&spec, &issuer);
    let confinement = "b".repeat(64);
    let grant = signed_grant_fixture(&spec, &hex, &confinement, Some(&impostor), 2_000);

    let error = authorize_with_verifier(
        &spec,
        &grant,
        &signed_grant_policy(),
        &cas_verifier(evidence, &issuer),
    )
    .expect_err("a signature by an untrusted key is not issuer authority");
    assert_eq!(error.code, leyline_runtime::ErrorCode::Unauthenticated);
}

/// The covered bytes are the grant with the signature field cleared, so a
/// grant's signing bytes do not change when the signature is attached — the
/// property that makes signing and verifying agree without a second encoding.
#[test]
fn grant_signing_bytes_ignore_the_carried_signature() {
    let signer = leyline_envelope::Ed25519RootSigner::from_seed(&[35u8; 32]);
    let spec = spec_bytes();
    let (_, hex) = all_roles_evidence(&spec, &signer);
    let confinement = "b".repeat(64);

    let unsigned = signed_grant_fixture(&spec, &hex, &confinement, None, 2_000);
    let signed = signed_grant_fixture(&spec, &hex, &confinement, Some(&signer), 2_000);
    assert_ne!(unsigned, signed, "the fixture must actually attach one");
    assert_eq!(
        grant_signing_bytes(&unsigned).expect("unsigned signing bytes"),
        grant_signing_bytes(&signed).expect("signed signing bytes"),
    );
}

/// Like `runSpecDigest`, the signature covers canonical content — not the
/// segment table and padding a particular encoder happened to emit.
#[test]
fn grant_signing_bytes_name_content_not_capnp_framing() {
    let signer = leyline_envelope::Ed25519RootSigner::from_seed(&[36u8; 32]);
    let spec = spec_bytes();
    let (_, hex) = all_roles_evidence(&spec, &signer);
    let confinement = "b".repeat(64);
    let grant = signed_grant_fixture(&spec, &hex, &confinement, Some(&signer), 2_000);

    // Re-frame the same content through a fresh builder: identical message,
    // different serialized bytes are permitted by the encoding.
    let message =
        capnp::serialize::read_message(&mut &grant[..], ReaderOptions::new()).expect("read grant");
    let mut reframed = Builder::new_default();
    reframed
        .set_root(
            message
                .get_root::<execution_capnp::run_grant::Reader<'_>>()
                .expect("grant root"),
        )
        .expect("copy grant");
    let mut reframed_bytes = Vec::new();
    capnp::serialize::write_message(&mut reframed_bytes, &reframed).expect("serialize reframed");

    assert_eq!(
        grant_signing_bytes(&grant).expect("signing bytes"),
        grant_signing_bytes(&reframed_bytes).expect("reframed signing bytes"),
    );
}

/// `execution/v1/test-vectors/run-id.json` is the cross-implementation gate
/// for run-identity derivation: a second implementation reproduces a run's
/// name from `(canonical spec digest, grantId, replayKey)` without an LLO
/// checkout. This test holds both halves of that claim — that the preimage
/// the vector documents is the one this crate hashes, built here from the
/// documented encoding rather than by calling `derive_run_id`, and that
/// `derive_run_id` still produces the pinned names.
#[test]
fn run_id_vector_pins_the_derivation_for_other_implementations() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../ll-core/schema-spec/execution/v1/test-vectors/run-id.json");
    let vector: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap_or_else(|error| {
            panic!(
                "run-id vector must ship with the schema: {}: {error}",
                path.display()
            )
        }))
        .expect("run-id vector is JSON");

    let derivation = &vector["derivation"];
    let domain = derivation["domain"].as_str().expect("domain");
    let prefix = derivation["prefix"].as_str().expect("prefix");
    let cases = vector["cases"].as_array().expect("cases");
    assert!(!cases.is_empty(), "a vector with no cases gates nothing");

    for case in cases {
        let spec_digest = case["canonicalSpecDigest"].as_str().expect("specDigest");
        let grant_id = case["grantId"].as_str().expect("grantId");
        let replay_key = case["replayKey"].as_str().expect("replayKey");

        // The documented encoding, reconstructed from the vector's own
        // description — this is what a second implementation writes.
        let mut preimage = Vec::from(domain.as_bytes());
        preimage.push(0);
        for field in [spec_digest, grant_id, replay_key] {
            preimage.extend_from_slice(&(field.len() as u64).to_le_bytes());
            preimage.extend_from_slice(field.as_bytes());
        }
        assert_eq!(
            hex::encode(&preimage),
            case["preimageHex"].as_str().expect("preimageHex"),
            "preimage drifted for case {}",
            case["name"]
        );

        let expected = format!("{prefix}{}", blake3::hash(&preimage).to_hex());
        assert_eq!(
            case["runId"].as_str(),
            Some(expected.as_str()),
            "pinned runId is not blake3 of the pinned preimage for case {}",
            case["name"]
        );
        assert_eq!(
            derive_run_id(spec_digest, grant_id, replay_key),
            expected,
            "derive_run_id drifted from the vector for case {}",
            case["name"]
        );
    }
}

/// ADR-0035 §4: a ceiling the selected tier cannot enforce is a rejection,
/// not a no-op.
///
/// libkrun applies `vcpus` and `memory_mib` through `krun_set_vm_config`, so
/// a microVM grant's memory ceiling is real. The native tier reads neither —
/// `native_backend.rs` consumes only `wall_time_ms` — so the identical grant
/// silently becomes a suggestion there. A caller cannot tell the two apart
/// from the outside, and the receipt attests a ceiling that was never
/// applied.
#[test]
fn a_memory_ceiling_the_native_tier_cannot_enforce_is_rejected() {
    struct NativeTier;
    impl Backend for NativeTier {
        fn capabilities(&self) -> BackendCapabilities {
            BackendCapabilities {
                backend_id: "native-nono/1".into(),
                backend_class: BackendClass::Native,
                available: true,
                enforced: leyline_runtime::EnforcedCeilings {
                    wall_time: leyline_runtime::CeilingMechanism::Supervisor,
                    vcpus: leyline_runtime::CeilingMechanism::Unenforced,
                    memory: leyline_runtime::CeilingMechanism::Unenforced,
                },
            }
        }
        fn start(&self, _request: &ExecutionRequest) -> Result<BackendRun, ExecutionError> {
            panic!("start must not be reached: the ceiling is unenforceable here");
        }
        fn cancel(&self, _run_id: &str) -> Result<bool, ExecutionError> {
            Ok(false)
        }
    }

    let spec = spec_bytes();
    let confinement = "b".repeat(64);
    let grant = grant_bytes_with_fixture(
        &spec,
        GrantFixture {
            capability: Some((EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION)),
            expires_at: 2_000,
            wall_time_ms: 0,
            confinement_algorithm: "blake3-256",
            confinement_manifest: "",
            confinement_value: &confinement,
            workspaces: Vec::new(),
            evidence: EvidenceFixture::Placeholder,
            signer: None,
            backend_class: execution_capnp::BackendClass::Native,
        },
    );
    let service = ExecutionService::new(NativeTier);
    service
        .provision(BackendClass::Native, "provision-native")
        .expect("provision backend");

    // TestResolver resolves memory_mib: 128 — a real ceiling under a
    // hypervisor and nothing at all under nono.
    let error = service
        .start_authorized(
            &spec,
            &grant,
            &AuthorizationPolicy {
                now_unix_ms: Some(1_000),
                required_backend: BackendClass::Native,
                required_confinement_digest: None,
            },
            &TestResolver,
        )
        .expect_err("an unenforceable memory ceiling must fail closed");

    assert_eq!(error.code, leyline_runtime::ErrorCode::UnsupportedBackend);
    assert!(
        error.detail.contains("cannot be enforced"),
        "the rejection must say the ceiling is unenforceable: {}",
        error.detail
    );
    // `TestResolver` requests both vcpus and memory, and this tier applies
    // neither, so either name is a correct report — but it must name one, so
    // an operator learns which ceiling to drop rather than that "something"
    // was wrong.
    assert!(
        error.detail.contains("vcpus") || error.detail.contains("memoryBytes"),
        "the rejection must name the ceiling: {}",
        error.detail
    );
}

/// The mirror, and the reason §4 is a rejection rather than a blanket ban:
/// the same ceiling on the tier that *can* apply it must still run.
/// libkrun passes `vcpus`/`memory_mib` to `krun_set_vm_config`, so a microVM
/// grant's ceilings are real and must be accepted.
#[test]
fn the_same_ceiling_is_accepted_by_the_tier_that_enforces_it() {
    let spec = spec_bytes();
    let grant = grant_bytes(&spec, true, 2_000, 0);
    let service = ExecutionService::new(RecordingBackend);
    service
        .provision(BackendClass::MicroVm, "provision-01")
        .expect("provision backend");

    service
        .start_authorized(
            &spec,
            &grant,
            &AuthorizationPolicy {
                now_unix_ms: Some(1_000),
                required_backend: BackendClass::MicroVm,
                required_confinement_digest: None,
            },
            &TestResolver,
        )
        .expect("a hypervisor-enforced ceiling must be accepted, not rejected");
}

/// A grant carrying a confinement document must have it agree with the digest.
///
/// cargo-mutants found this untested: `replace != with ==` in
/// `authorize_with_verifier` survived, meaning nothing observed whether the
/// carried document and the named digest were compared at all. The earlier test
/// exercised the manifest type and never the grant path — coverage of the parts
/// reading as coverage of the mechanism.
///
/// Both directions, because only the pair pins the comparison: a matching
/// document must be ACCEPTED and carried through, and a mismatching one must be
/// refused. With only the refusal, `== ` would still fail the accept case; with
/// only the accept, `==` would pass both.
#[test]
fn a_carried_confinement_manifest_is_verified_against_its_digest() {
    use leyline_runtime::confinement::{ConfinementManifest, FsGrant};

    let policy = ConfinementManifest::new()
        .with_fs_grant(FsGrant::read_write("/run/rootfs/"))
        .expect("valid grant");
    let document = policy.to_canonical_json().expect("canonical");
    let digest = policy.confinement_digest().expect("digest");
    let value = digest
        .strip_prefix("blake3-256:")
        .expect("algorithm-prefixed digest")
        .to_owned();

    // Agreeing: accepted, and the document reaches the authorized execution.
    let spec = spec_bytes();
    let grant = grant_bytes_with_fixture(
        &spec,
        GrantFixture {
            capability: Some((EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION)),
            expires_at: 2_000,
            wall_time_ms: 0,
            confinement_algorithm: "blake3-256",
            confinement_manifest: &document,
            confinement_value: &value,
            workspaces: Vec::new(),
            evidence: EvidenceFixture::Placeholder,
            signer: None,
            backend_class: execution_capnp::BackendClass::MicroVm,
        },
    );
    let authorized = authorize(
        &spec,
        &grant,
        &AuthorizationPolicy {
            now_unix_ms: Some(1_000),
            required_backend: BackendClass::MicroVm,
            required_confinement_digest: None,
        },
    )
    .expect("a grant whose carried manifest matches its digest must authorize");
    assert_eq!(
        authorized.confinement_manifest.as_deref(),
        Some(document.as_str()),
        "the carried policy must reach the runner, or the field is write-only"
    );

    // Disagreeing: refused. The digest names a different policy.
    let other = "c".repeat(64);
    let mismatched = grant_bytes_with_fixture(
        &spec,
        GrantFixture {
            capability: Some((EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION)),
            expires_at: 2_000,
            wall_time_ms: 0,
            confinement_algorithm: "blake3-256",
            confinement_manifest: &document,
            confinement_value: &other,
            workspaces: Vec::new(),
            evidence: EvidenceFixture::Placeholder,
            signer: None,
            backend_class: execution_capnp::BackendClass::MicroVm,
        },
    );
    let error = authorize(
        &spec,
        &mismatched,
        &AuthorizationPolicy {
            now_unix_ms: Some(1_000),
            required_backend: BackendClass::MicroVm,
            required_confinement_digest: None,
        },
    )
    .expect_err("a carried manifest that does not match its digest must be refused");
    assert!(
        format!("{error:?}").contains("confinementDigest names"),
        "the refusal must show both digests so an operator sees which \
         disagreed: {error:?}"
    );
}

/// Absence is a distinct, legitimate state: the issuer committed by digest
/// alone. It must authorize, and must not invent a document.
#[test]
fn a_grant_may_commit_by_digest_alone() {
    let spec = spec_bytes();
    let grant = grant_bytes(&spec, true, 2_000, 0);
    let authorized = authorize(
        &spec,
        &grant,
        &AuthorizationPolicy {
            now_unix_ms: Some(1_000),
            required_backend: BackendClass::MicroVm,
            required_confinement_digest: None,
        },
    )
    .expect("a digest-only grant must authorize");
    assert!(
        authorized.confinement_manifest.is_none(),
        "no document was carried, so none may be reported"
    );
}
