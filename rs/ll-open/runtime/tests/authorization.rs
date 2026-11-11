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
    ExecutionRequest, ExecutionResolver, ExecutionService, ResourceLimits, ServedWorkspaces,
    WorkerChannel, WorkspaceGrant, WorkspaceOperation,
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
    grant_bytes_with_write_scope(spec_bytes, fixture, WriteScope::Default)
}

/// How a fixture grant states each workspace's `writeScope`.
#[derive(Clone, Copy)]
enum WriteScope<'a> {
    /// `["/"]` for a grant that permits a write, unset otherwise.
    Default,
    /// The field left unset.
    Omitted,
    /// The field set to exactly this list, empty included.
    Listed(&'a [&'a str]),
}

/// As [`grant_bytes_with_fixture`], with every workspace's `writeScope`
/// stated as `write_scope` says.
fn grant_bytes_with_write_scope(
    spec_bytes: &[u8],
    fixture: GrantFixture<'_>,
    write_scope: WriteScope<'_>,
) -> Vec<u8> {
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
        let mut granted_operations = workspace
            .reborrow()
            .init_operations(operations.len() as u32);
        for (operation_index, operation) in operations.iter().enumerate() {
            granted_operations.set(operation_index as u32, *operation);
        }
        // A grant that permits writes names where (bead
        // `ley-line-open-483134`). Unless a test states a scope, a writing
        // fixture is granted the whole workspace.
        let writes = operations.iter().any(|operation| {
            matches!(
                operation,
                execution_capnp::WorkspaceOperation::Mutate
                    | execution_capnp::WorkspaceOperation::Commit
            )
        });
        // `None` leaves the field unset, the one encoding of "no scope".
        let scope: Option<Vec<&str>> = match write_scope {
            WriteScope::Default if writes => Some(vec!["/"]),
            WriteScope::Default | WriteScope::Omitted => None,
            WriteScope::Listed(scope) => Some(scope.to_vec()),
        };
        if let Some(scope) = scope {
            let mut granted_scope = workspace.init_write_scope(scope.len() as u32);
            for (scope_index, entry) in scope.iter().enumerate() {
                granted_scope.set(scope_index as u32, entry);
            }
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

/// A spec requesting `repo` and `docs`, and a grant resolving both with the
/// given operations and generations, `docs` first.
fn two_workspace_grant(
    repo: Vec<execution_capnp::WorkspaceOperation>,
    docs: Vec<execution_capnp::WorkspaceOperation>,
) -> (Vec<u8>, Vec<u8>) {
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    let spec = spec_bytes_with_details(None, 0, &[("repo", &a), ("docs", &b)]);
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
            workspaces: vec![("docs", &b, docs), ("repo", &a, repo)],
            evidence: EvidenceFixture::Placeholder,
            signer: None,
            backend_class: execution_capnp::BackendClass::MicroVm,
        },
    );
    // Generations, set on a copy of the grant: 7 on `docs`, 9 on `repo`.
    let mut input = grant.as_slice();
    let reader = capnp::serialize::read_message(&mut input, ReaderOptions::new()).unwrap();
    let mut message = Builder::new_default();
    message
        .set_root(
            reader
                .get_root::<execution_capnp::run_grant::Reader<'_>>()
                .unwrap(),
        )
        .unwrap();
    {
        let root = message
            .get_root::<execution_capnp::run_grant::Builder<'_>>()
            .unwrap();
        let mut workspaces = root.get_workspaces().unwrap();
        workspaces.reborrow().get(0).set_expected_generation(7);
        workspaces.get(1).set_expected_generation(9);
    }
    let mut grant = Vec::new();
    capnp::serialize::write_message(&mut grant, &message).unwrap();
    (spec, grant)
}

#[test]
fn an_authorized_run_carries_each_workspace_grant_in_grant_order() {
    use execution_capnp::WorkspaceOperation as Op;
    let (spec, grant) = two_workspace_grant(
        vec![Op::Read, Op::List, Op::Mutate, Op::Commit],
        vec![Op::Read, Op::Query],
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
    .expect("an exact workspace grant authorizes");
    assert_eq!(
        authorized.workspaces,
        vec![
            WorkspaceGrant {
                name: "docs".into(),
                graph_root: format!("blake3-256:{}", "b".repeat(64)),
                expected_generation: 7,
                operations: vec![WorkspaceOperation::Read, WorkspaceOperation::Query],
                write_scope: Vec::new(),
            },
            WorkspaceGrant {
                name: "repo".into(),
                graph_root: format!("blake3-256:{}", "a".repeat(64)),
                expected_generation: 9,
                operations: vec![
                    WorkspaceOperation::Read,
                    WorkspaceOperation::List,
                    WorkspaceOperation::Mutate,
                    WorkspaceOperation::Commit,
                ],
                write_scope: vec!["/".into()],
            },
        ]
    );
}

/// A resolver that resolves as [`TestResolver`] and then drops the grant's
/// workspace authority.
struct DroppingResolver;

impl ExecutionResolver for DroppingResolver {
    fn resolve(
        &self,
        authorized: &leyline_runtime::authorization::AuthorizedExecution,
    ) -> Result<ExecutionRequest, ExecutionError> {
        let mut request = TestResolver.resolve(authorized)?;
        request.workspaces.clear();
        Ok(request)
    }
}

#[test]
fn the_service_refuses_a_resolver_that_changes_the_granted_workspaces() {
    use execution_capnp::WorkspaceOperation as Op;
    let (spec, grant) = two_workspace_grant(vec![Op::Read], vec![Op::Read]);
    let policy = AuthorizationPolicy {
        now_unix_ms: Some(1_000),
        required_backend: BackendClass::MicroVm,
        required_confinement_digest: None,
    };
    // A backend that serves the shares it is granted: one that served none
    // would be refused by the service in its own right (bead
    // `ley-line-open-204a94`), and the positive start below would fail.
    let (service, _shares) = sharing_service(SharingBackend);
    service
        .provision(BackendClass::MicroVm, "provision-workspaces")
        .expect("provision backend");
    let error = service
        .start_authorized(&spec, &grant, &policy, &DroppingResolver)
        .expect_err("a resolver must carry the granted workspaces");
    assert_eq!(
        error.code,
        leyline_runtime::ErrorCode::IdentityPolicyMismatch
    );
    assert!(error.detail.contains("workspaces"), "{error}");
    service
        .start_authorized(&spec, &grant, &policy, &TestResolver)
        .expect("a resolver that carries them starts the run");
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
            unavailable_reason: None,
            enforced: leyline_runtime::EnforcedCeilings::hypervisor_backed(),
        }
    }

    fn start(
        &self,
        request: &ExecutionRequest,
        _workspaces: ServedWorkspaces,
    ) -> Result<BackendRun, ExecutionError> {
        Ok(BackendRun {
            backend_id: format!("test/{}", request.run_id),
            attached: Vec::new(),
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
            unavailable_reason: None,
            enforced: leyline_runtime::EnforcedCeilings::hypervisor_backed(),
        }
    }

    fn start(
        &self,
        _request: &ExecutionRequest,
        _workspaces: ServedWorkspaces,
    ) -> Result<BackendRun, ExecutionError> {
        Ok(BackendRun {
            backend_id: "completing/1".into(),
            attached: Vec::new(),
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
            workspaces: authorized.workspaces.clone(),
            limits: ResourceLimits {
                wall_time_ms: 1_000,
                memory: leyline_runtime::MemoryLimits { limit: 128 << 20 },
                cpu: leyline_runtime::CpuLimits {
                    quota: 100_000,
                    period: 100_000,
                },
                ..ResourceLimits::default()
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

/// A share provider that opens every workspace it is asked for and records
/// each grant, in order. Its shares answer every request `ENOSYS`: these
/// tests are about who holds a share, not what it serves.
#[derive(Default)]
struct OpenShares {
    opened: std::sync::Mutex<Vec<(String, WorkspaceGrant)>>,
}

struct UnansweringShare;

impl leyline_runtime::backends::libkrun::share::FsService for UnansweringShare {
    fn call(
        &self,
        _request: leyline_runtime::backends::libkrun::share::Request,
    ) -> leyline_runtime::backends::libkrun::share::Reply {
        leyline_runtime::backends::libkrun::share::Reply::Error { errno: 38 }
    }
}

impl leyline_runtime::backends::libkrun::share::WorkspaceShares for OpenShares {
    fn open(
        &self,
        run_id: &str,
        grant: &WorkspaceGrant,
    ) -> Result<
        std::sync::Arc<dyn leyline_runtime::backends::libkrun::share::FsService>,
        ExecutionError,
    > {
        self.opened
            .lock()
            .unwrap()
            .push((run_id.to_owned(), grant.clone()));
        Ok(std::sync::Arc::new(UnansweringShare))
    }
}

/// A service over `backend` that serves workspaces from a fresh
/// [`OpenShares`], returned beside it.
fn sharing_service<B: Backend>(backend: B) -> (ExecutionService<B>, std::sync::Arc<OpenShares>) {
    let shares = std::sync::Arc::new(OpenShares::default());
    (
        ExecutionService::new(backend).with_workspace_shares(shares.clone()),
        shares,
    )
}

/// The shares [`SharingBackend`]'s runs hold, for the life of the test
/// binary.
static HELD_SHARES: std::sync::Mutex<
    Vec<std::sync::Arc<dyn leyline_runtime::backends::libkrun::share::FsService>>,
> = std::sync::Mutex::new(Vec::new());

/// Completes at once, attaching every workspace share the service hands it,
/// as the libkrun backend does for the one it mounts.
struct SharingBackend;

impl Backend for SharingBackend {
    fn capabilities(&self) -> BackendCapabilities {
        CompletingBackend.capabilities()
    }

    fn start(
        &self,
        _request: &ExecutionRequest,
        workspaces: ServedWorkspaces,
    ) -> Result<BackendRun, ExecutionError> {
        Ok(BackendRun {
            backend_id: "sharing/1".into(),
            attached: workspaces
                .into_shares()
                .into_iter()
                .map(|share| {
                    let (service, attached) = share.attach();
                    // Held as a worker's serving thread holds it: the
                    // service refuses a start whose share nothing holds.
                    HELD_SHARES.lock().unwrap().push(service);
                    attached
                })
                .collect(),
        })
    }

    fn workspace_capacity(&self) -> usize {
        usize::MAX
    }

    fn poll(&self, _run_id: &str) -> Result<Option<BackendRunStatus>, ExecutionError> {
        Ok(Some(BackendRunStatus::Succeeded))
    }

    fn cancel(&self, _run_id: &str) -> Result<bool, ExecutionError> {
        Ok(true)
    }
}

/// The receipt's backend evidence names every channel the run's worker held
/// beyond its confinement manifest: the workspace share for a run granted a
/// workspace, none for a run without one. The receipt carries the evidence
/// document's digest under the execution/v1 media type.
#[test]
fn the_receipt_names_the_workspace_share_the_run_held() {
    let policy = AuthorizationPolicy {
        now_unix_ms: Some(1_000),
        required_backend: BackendClass::MicroVm,
        required_confinement_digest: None,
    };
    let confinement = "b".repeat(64);
    let root = "a".repeat(64);
    let spec = spec_bytes_with_details(None, 0, &[("repo", &root)]);
    let grant = grant_bytes_with_fixture(
        &spec,
        GrantFixture {
            capability: Some((EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION)),
            expires_at: 2_000,
            wall_time_ms: 0,
            confinement_algorithm: "blake3-256",
            confinement_manifest: "",
            confinement_value: &confinement,
            workspaces: vec![(
                "repo",
                &root,
                vec![execution_capnp::WorkspaceOperation::Read],
            )],
            evidence: EvidenceFixture::Placeholder,
            signer: None,
            backend_class: execution_capnp::BackendClass::MicroVm,
        },
    );
    let plain_spec = spec_bytes();
    let plain_grant = grant_bytes(&plain_spec, true, 2_000, 0);

    let (service, _shares) = sharing_service(SharingBackend);
    service
        .provision(BackendClass::MicroVm, "provision-sharing")
        .expect("provision backend");
    let shared = service
        .start_authorized(&spec, &grant, &policy, &TestResolver)
        .expect("a run granted a workspace");
    // Its own service: both grants carry one replay key, and a replayed
    // start answers the first run.
    let (plain_service, _plain_shares) = sharing_service(SharingBackend);
    plain_service
        .provision(BackendClass::MicroVm, "provision-plain")
        .expect("provision backend");
    let plain = plain_service
        .start_authorized(&plain_spec, &plain_grant, &policy, &TestResolver)
        .expect("a run without a workspace");

    let receipt = service.collect(&shared.run_id).expect("collect");
    assert_eq!(
        receipt.backend_evidence.channels,
        vec![WorkerChannel::WorkspaceShare {
            workspace: "repo".into(),
            graph_root: format!("blake3-256:{root}"),
        }]
    );
    assert_eq!(receipt.backend_evidence.backend_id, "sharing/1");
    let wire: serde_json::Value = serde_json::from_str(
        &collect_json(&service, &json!({"runId": shared.run_id}).to_string())
            .expect("collect JSON"),
    )
    .expect("receipt JSON");
    let evidence = &wire["receipt"]["backend"]["evidence"];
    assert_eq!(
        evidence["mediaType"],
        "application/vnd.leyline.execution.backend-evidence+json"
    );
    assert_eq!(
        format!(
            "{}:{}",
            evidence["digest"]["algorithm"].as_str().unwrap(),
            evidence["digest"]["value"].as_str().unwrap()
        ),
        receipt.backend_evidence.digest()
    );
    assert_eq!(
        receipt.backend_evidence.digest(),
        format!(
            "blake3-256:{}",
            blake3::hash(&receipt.backend_evidence.canonical_bytes()).to_hex()
        )
    );

    assert_ne!(plain.run_id, shared.run_id);
    let receipt = plain_service.collect(&plain.run_id).expect("collect");
    assert_eq!(receipt.backend_evidence.channels, Vec::new());
}

/// A start that replays an existing run's replay key under other authority
/// (another spec, grant and input roots) answers the existing run, and leaves
/// that run's receipt as it was started: the receipt names the authority and
/// content the run ran under, never a later caller's (bead
/// `ley-line-open-799fbc`).
#[test]
fn a_replayed_start_never_rewrites_the_existing_runs_receipt() {
    let policy = AuthorizationPolicy {
        now_unix_ms: Some(1_000),
        required_backend: BackendClass::MicroVm,
        required_confinement_digest: None,
    };
    let (service, _shares) = sharing_service(SharingBackend);
    service
        .provision(BackendClass::MicroVm, "provision-replay-receipt")
        .expect("provision backend");
    let first_spec = spec_bytes();
    let first_grant = grant_bytes(&first_spec, true, 2_000, 0);
    let first = service
        .start_authorized(&first_spec, &first_grant, &policy, &TestResolver)
        .expect("the first run");
    let before = service.collect(&first.run_id).expect("collect");
    assert_eq!(
        before.context.input_roots,
        vec![format!("blake3-256:{}", "a".repeat(64))]
    );

    // Same replay key (every fixture grant carries one), other spec, other
    // grant, and a workspace input.
    let root = "e".repeat(64);
    let spec = spec_bytes_with_details(None, 0, &[("repo", &root)]);
    let grant = grant_bytes_with_fixture(
        &spec,
        GrantFixture {
            capability: Some((EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION)),
            expires_at: 2_000,
            wall_time_ms: 0,
            confinement_algorithm: "blake3-256",
            confinement_manifest: "",
            confinement_value: &"b".repeat(64),
            workspaces: vec![(
                "repo",
                &root,
                vec![execution_capnp::WorkspaceOperation::Read],
            )],
            evidence: EvidenceFixture::Placeholder,
            signer: None,
            backend_class: execution_capnp::BackendClass::MicroVm,
        },
    );
    let replayed = service
        .start_authorized(&spec, &grant, &policy, &TestResolver)
        .expect("a replayed start answers the existing run");
    assert_eq!(replayed.run_id, first.run_id);

    let after = service.collect(&first.run_id).expect("collect again");
    assert_eq!(after.context, before.context);
    assert_eq!(
        after.context.run_spec_digest,
        canonical_digest(&first_spec).expect("spec digest")
    );
}

/// `RunReceipt.inputRoots` is "Content roots materialized for the run"
/// (execution.capnp): the rootfs the resolver chose, which every backend
/// verifies against its digest before the workload runs, and then each
/// granted workspace's graph root. A receipt that left the rootfs out named
/// nothing about what ran (bead `ley-line-open-68a55d`).
#[test]
fn the_receipt_input_roots_name_the_rootfs_then_each_granted_workspace() {
    let policy = AuthorizationPolicy {
        now_unix_ms: Some(1_000),
        required_backend: BackendClass::MicroVm,
        required_confinement_digest: None,
    };
    let confinement = "b".repeat(64);
    // Distinct from TestResolver's rootfs ("a" x 64), so the order is seen.
    let root = "e".repeat(64);
    let spec = spec_bytes_with_details(None, 0, &[("repo", &root)]);
    let grant = grant_bytes_with_fixture(
        &spec,
        GrantFixture {
            capability: Some((EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION)),
            expires_at: 2_000,
            wall_time_ms: 0,
            confinement_algorithm: "blake3-256",
            confinement_manifest: "",
            confinement_value: &confinement,
            workspaces: vec![(
                "repo",
                &root,
                vec![execution_capnp::WorkspaceOperation::Read],
            )],
            evidence: EvidenceFixture::Placeholder,
            signer: None,
            backend_class: execution_capnp::BackendClass::MicroVm,
        },
    );
    let rootfs = format!("blake3-256:{}", "a".repeat(64));

    let (service, _shares) = sharing_service(SharingBackend);
    service
        .provision(BackendClass::MicroVm, "provision-roots")
        .expect("provision backend");
    let shared = service
        .start_authorized(&spec, &grant, &policy, &TestResolver)
        .expect("a run granted a workspace");
    let receipt = service.collect(&shared.run_id).expect("collect");
    assert_eq!(
        receipt.context.input_roots,
        vec![rootfs.clone(), format!("blake3-256:{root}")]
    );
    let wire: serde_json::Value = serde_json::from_str(
        &collect_json(&service, &json!({"runId": shared.run_id}).to_string())
            .expect("collect JSON"),
    )
    .expect("receipt JSON");
    assert_eq!(
        wire["receipt"]["inputRoots"],
        json!([
            {"algorithm": "blake3-256", "value": "a".repeat(64)},
            {"algorithm": "blake3-256", "value": root},
        ])
    );

    let plain_spec = spec_bytes();
    let plain_grant = grant_bytes(&plain_spec, true, 2_000, 0);
    let (plain_service, _plain_shares) = sharing_service(SharingBackend);
    plain_service
        .provision(BackendClass::MicroVm, "provision-plain-roots")
        .expect("provision backend");
    let plain = plain_service
        .start_authorized(&plain_spec, &plain_grant, &policy, &TestResolver)
        .expect("a run without a workspace");
    let receipt = plain_service.collect(&plain.run_id).expect("collect");
    assert_eq!(receipt.context.input_roots, vec![rootfs]);
}

/// Starts every run and serves none of its workspaces: a tier that accepts a
/// workspace grant and drops it. Records each run it is told to cancel.
#[derive(Clone, Default)]
struct DroppingBackend {
    cancelled: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    starts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Backend for DroppingBackend {
    fn capabilities(&self) -> BackendCapabilities {
        CompletingBackend.capabilities()
    }

    fn start(
        &self,
        _request: &ExecutionRequest,
        _workspaces: ServedWorkspaces,
    ) -> Result<BackendRun, ExecutionError> {
        self.starts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(BackendRun {
            backend_id: "dropping/1".into(),
            attached: Vec::new(),
        })
    }

    fn workspace_capacity(&self) -> usize {
        usize::MAX
    }

    fn poll(&self, _run_id: &str) -> Result<Option<BackendRunStatus>, ExecutionError> {
        Ok(Some(BackendRunStatus::Succeeded))
    }

    fn cancel(&self, run_id: &str) -> Result<bool, ExecutionError> {
        self.cancelled.lock().unwrap().push(run_id.to_owned());
        Ok(true)
    }
}

/// A spec and grant that grant the `repo` workspace at `root` (64 hex).
fn workspace_spec_and_grant(root: &str) -> (Vec<u8>, Vec<u8>) {
    let confinement = "b".repeat(64);
    let spec = spec_bytes_with_details(None, 0, &[("repo", root)]);
    let grant = grant_bytes_with_fixture(
        &spec,
        GrantFixture {
            capability: Some((EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION)),
            expires_at: 2_000,
            wall_time_ms: 0,
            confinement_algorithm: "blake3-256",
            confinement_manifest: "",
            confinement_value: &confinement,
            workspaces: vec![(
                "repo",
                root,
                vec![execution_capnp::WorkspaceOperation::Read],
            )],
            evidence: EvidenceFixture::Placeholder,
            signer: None,
            backend_class: execution_capnp::BackendClass::MicroVm,
        },
    );
    (spec, grant)
}

/// Bead `ley-line-open-204a94`: no backend can run a granted workspace
/// workspace-less. Each tier refuses a grant it cannot serve before it
/// spawns anything, but `Backend` is a public trait; the service holds the
/// line at the seam. A backend that reports `Ok` without a `WorkspaceShare`
/// channel for every granted workspace has started the workload without
/// it: the service cancels that run, forgets it, and refuses the start with
/// the typed `workspaceUnserved` code on the same path as every other
/// execution/v1 refusal, including the JSON surface the daemon, CLI and MCP
/// adapters project.
#[test]
fn a_backend_that_drops_a_granted_workspace_is_cancelled_and_refused() {
    let policy = AuthorizationPolicy {
        now_unix_ms: Some(1_000),
        required_backend: BackendClass::MicroVm,
        required_confinement_digest: None,
    };
    let root = "a".repeat(64);
    let (spec, grant) = workspace_spec_and_grant(&root);

    let backend = DroppingBackend::default();
    // The service opens the share; the backend is handed it and drops it.
    let (service, _shares) = sharing_service(backend.clone());
    service
        .provision(BackendClass::MicroVm, "provision-dropping")
        .expect("provision backend");
    let error = service
        .start_authorized(&spec, &grant, &policy, &TestResolver)
        .expect_err("a backend that served no share for the granted workspace");
    assert_eq!(
        error.code,
        leyline_runtime::ErrorCode::WorkspaceUnserved,
        "{error:?}"
    );
    assert!(!error.retryable, "{error:?}");
    assert!(error.detail.contains("\"repo\""), "{error:?}");
    let run_id = derive_run_id(
        &canonical_digest(&spec).expect("spec digest"),
        "grant-01",
        "replay-01",
    );
    assert_eq!(
        *backend.cancelled.lock().unwrap(),
        vec![run_id.clone()],
        "the workspace-less run is cancelled, once"
    );
    assert_eq!(
        service.status(&run_id).expect("status"),
        None,
        "the refused run never became a run"
    );

    // The JSON surface the daemon, CLI and MCP adapters call carries the
    // same typed code.
    let input = json!({
        "spec": serde_json::from_str::<serde_json::Value>(&spec_json(&spec)).unwrap(),
        "grant": serde_json::from_str::<serde_json::Value>(&grant_json(&grant)).unwrap(),
    })
    .to_string();
    let error = start_json_with_verifier(
        &service,
        &input,
        &policy,
        &TestResolver,
        &MetadataOnlyEvidenceVerifier,
    )
    .expect_err("the JSON surface refuses the same start");
    assert_eq!(
        error.code,
        leyline_runtime::ErrorCode::WorkspaceUnserved,
        "{error:?}"
    );

    // A run with no workspaces on the same backend is unaffected.
    let plain_spec = spec_bytes();
    let plain_grant = grant_bytes(&plain_spec, true, 2_000, 0);
    let plain = service
        .start_authorized(&plain_spec, &plain_grant, &policy, &TestResolver)
        .expect("a run without a workspace starts");
    assert_eq!(plain.state, leyline_runtime::RunState::Running);
    assert_eq!(backend.cancelled.lock().unwrap().len(), 2);
}

/// A cancel that also fails does not turn the refusal into something else:
/// the start is still `workspaceUnserved`, and the cancel's failure is named
/// in it rather than lost. The workload may still be live, so the run is not
/// forgotten: it is recorded `Failed` (never receipted), its run_id refuses
/// a second start, and `cleanup` retries the cancel.
#[test]
fn a_dropped_workspace_whose_cancel_fails_is_still_refused_and_says_so() {
    /// Its first cancel fails; later ones succeed.
    #[derive(Clone, Default)]
    struct Unstoppable {
        cancels: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        starts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Backend for Unstoppable {
        fn capabilities(&self) -> BackendCapabilities {
            CompletingBackend.capabilities()
        }
        fn start(
            &self,
            _request: &ExecutionRequest,
            _workspaces: ServedWorkspaces,
        ) -> Result<BackendRun, ExecutionError> {
            self.starts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(BackendRun {
                backend_id: "unstoppable/1".into(),
                attached: Vec::new(),
            })
        }
        fn workspace_capacity(&self) -> usize {
            usize::MAX
        }
        fn cancel(&self, _run_id: &str) -> Result<bool, ExecutionError> {
            if self
                .cancels
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                == 0
            {
                Err(ExecutionError::backend("the worker would not stop"))
            } else {
                Ok(true)
            }
        }
    }

    let policy = AuthorizationPolicy {
        now_unix_ms: Some(1_000),
        required_backend: BackendClass::MicroVm,
        required_confinement_digest: None,
    };
    let (spec, grant) = workspace_spec_and_grant(&"a".repeat(64));
    let backend = Unstoppable::default();
    let (service, _shares) = sharing_service(backend.clone());
    service
        .provision(BackendClass::MicroVm, "provision-unstoppable")
        .expect("provision backend");
    let error = service
        .start_authorized(&spec, &grant, &policy, &TestResolver)
        .expect_err("a backend that served no share and would not stop");
    assert_eq!(
        error.code,
        leyline_runtime::ErrorCode::WorkspaceUnserved,
        "{error:?}"
    );
    assert!(error.detail.contains("\"repo\""), "{error:?}");
    assert!(
        error.detail.contains("the worker would not stop"),
        "{error:?}"
    );

    let run_id = derive_run_id(
        &canonical_digest(&spec).expect("spec digest"),
        "grant-01",
        "replay-01",
    );
    let record = service
        .status(&run_id)
        .expect("status")
        .expect("a run that may still be live stays addressable");
    assert_eq!(record.state, leyline_runtime::RunState::Failed);
    let states: Vec<_> = service
        .inspect(&run_id, 0)
        .expect("inspect")
        .events
        .into_iter()
        .map(|event| event.state)
        .collect();
    assert_eq!(
        states.last(),
        Some(&leyline_runtime::RunState::Failed),
        "{states:?}"
    );
    assert!(
        service.collect(&run_id).is_err(),
        "a refused start is never receipted"
    );

    // The same authority cannot start a second workload beside it.
    let again = service
        .start_authorized(&spec, &grant, &policy, &TestResolver)
        .expect_err("its run_id is still held");
    assert_eq!(
        again.code,
        leyline_runtime::ErrorCode::ResourceConflict,
        "{again:?}"
    );
    assert_eq!(
        backend.starts.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the second start never reached the backend"
    );

    // `cleanup` retries the cancel, which now succeeds.
    let cleaned = service
        .cleanup(&run_id)
        .expect("cleanup retries the cancel");
    assert_eq!(cleaned.state, leyline_runtime::RunState::Cleaned);
    assert_eq!(backend.cancels.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// A `repo` workspace grant with `operations` and `write_scope`, and its spec.
fn scoped_workspace_grant(
    operations: Vec<execution_capnp::WorkspaceOperation>,
    write_scope: &[&str],
) -> (Vec<u8>, Vec<u8>) {
    let root = "a".repeat(64);
    let confinement = "b".repeat(64);
    let spec = spec_bytes_with_details(None, 0, &[("repo", &root)]);
    let grant = grant_bytes_with_write_scope(
        &spec,
        GrantFixture {
            capability: Some((EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION)),
            expires_at: 2_000,
            wall_time_ms: 0,
            confinement_algorithm: "blake3-256",
            confinement_manifest: "",
            confinement_value: &confinement,
            workspaces: vec![("repo", &root, operations)],
            evidence: EvidenceFixture::Placeholder,
            signer: None,
            backend_class: execution_capnp::BackendClass::MicroVm,
        },
        // An empty slice leaves the field unset; the explicit empty list is
        // `explicit_empty_scope_grant`'s.
        if write_scope.is_empty() {
            WriteScope::Omitted
        } else {
            WriteScope::Listed(write_scope)
        },
    );
    (spec, grant)
}

/// A `repo` grant with `operations` whose `writeScope` is set to an empty
/// list, as a JSON client that writes `"writeScope": []` produces.
fn explicit_empty_scope_grant(
    operations: Vec<execution_capnp::WorkspaceOperation>,
) -> (Vec<u8>, Vec<u8>) {
    let root = "a".repeat(64);
    let confinement = "b".repeat(64);
    let spec = spec_bytes_with_details(None, 0, &[("repo", &root)]);
    let grant = grant_bytes_with_write_scope(
        &spec,
        GrantFixture {
            capability: Some((EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION)),
            expires_at: 2_000,
            wall_time_ms: 0,
            confinement_algorithm: "blake3-256",
            confinement_manifest: "",
            confinement_value: &confinement,
            workspaces: vec![("repo", &root, operations)],
            evidence: EvidenceFixture::Placeholder,
            signer: None,
            backend_class: execution_capnp::BackendClass::MicroVm,
        },
        WriteScope::Listed(&[]),
    );
    (spec, grant)
}

/// "No scope" has one encoding: the field left unset. An explicit empty list
/// canonicalizes to different bytes than an unset field (Cap'n Proto keeps a
/// zero-length list as a non-null pointer), so accepting both would give one
/// grant two digests and two signatures. The empty list is refused, on a
/// read-only grant and a writing one alike, naming the workspace and field.
#[test]
fn an_explicit_empty_write_scope_is_refused() {
    use execution_capnp::WorkspaceOperation as Op;
    for operations in [vec![Op::Read], vec![Op::Read, Op::Mutate]] {
        let (spec, grant) = explicit_empty_scope_grant(operations);
        let error =
            authorize(&spec, &grant, &microvm_policy()).expect_err("an explicit empty writeScope");
        assert_eq!(error.code, leyline_runtime::ErrorCode::InvalidSpec);
        assert!(error.detail.contains("writeScope"), "{error}");
        assert!(error.detail.contains("repo"), "{error}");
        assert!(error.detail.contains("empty list"), "{error}");
    }
    // The same read-only grant with the field unset authorizes, and the two
    // are different signed bytes.
    let (spec, unset) = scoped_workspace_grant(vec![Op::Read], &[]);
    authorize(&spec, &unset, &microvm_policy()).expect("an unset writeScope");
    let (_, empty) = explicit_empty_scope_grant(vec![Op::Read]);
    assert_ne!(
        grant_signing_bytes(&unset).expect("signing bytes"),
        grant_signing_bytes(&empty).expect("signing bytes"),
        "an empty list and an unset field canonicalize differently"
    );
}

fn microvm_policy() -> AuthorizationPolicy {
    AuthorizationPolicy {
        now_unix_ms: Some(1_000),
        required_backend: BackendClass::MicroVm,
        required_confinement_digest: None,
    }
}

/// Bead `ley-line-open-483134`: a workspace grant names where the run may
/// write, as exact paths and `/`-terminated directory prefixes, and the
/// authorized run carries that scope as granted.
#[test]
fn an_authorized_workspace_grant_carries_its_write_scope() {
    use execution_capnp::WorkspaceOperation as Op;
    let (spec, grant) = scoped_workspace_grant(vec![Op::Read, Op::Mutate], &["src/a.rs", "docs/"]);
    let authorized = authorize(&spec, &grant, &microvm_policy()).expect("a scoped grant");
    assert_eq!(
        authorized.workspaces[0].write_scope,
        vec!["src/a.rs".to_owned(), "docs/".to_owned()]
    );

    // The whole workspace is one explicit entry.
    let (spec, grant) = scoped_workspace_grant(vec![Op::Read, Op::Commit], &["/"]);
    let authorized = authorize(&spec, &grant, &microvm_policy()).expect("a whole-tree grant");
    assert_eq!(authorized.workspaces[0].write_scope, vec!["/".to_owned()]);

    // A read-only grant has no write scope.
    let (spec, grant) = scoped_workspace_grant(vec![Op::Read, Op::List], &[]);
    let authorized = authorize(&spec, &grant, &microvm_policy()).expect("a read-only grant");
    assert!(authorized.workspaces[0].write_scope.is_empty());
}

/// Write authority is never implied in either direction: a grant that
/// permits `mutate` or `commit` without naming a scope is refused (it would
/// otherwise mean "anywhere"), and so is a scope on a grant that permits no
/// write (a scope that grants nothing).
#[test]
fn a_write_scope_is_required_exactly_when_the_grant_permits_writes() {
    use execution_capnp::WorkspaceOperation as Op;
    for operations in [vec![Op::Read, Op::Mutate], vec![Op::Commit]] {
        let (spec, grant) = scoped_workspace_grant(operations, &[]);
        let error = authorize(&spec, &grant, &microvm_policy())
            .expect_err("a writing grant with no write scope");
        assert_eq!(error.code, leyline_runtime::ErrorCode::InvalidSpec);
        assert!(error.detail.contains("writeScope"), "{error}");
        assert!(error.detail.contains("repo"), "{error}");
    }
    let (spec, grant) = scoped_workspace_grant(vec![Op::Read, Op::Query], &["docs/"]);
    let error = authorize(&spec, &grant, &microvm_policy())
        .expect_err("a write scope on a grant that permits no write");
    assert_eq!(error.code, leyline_runtime::ErrorCode::InvalidSpec);
    assert!(error.detail.contains("writeScope"), "{error}");
}

/// A scope entry is a workspace-relative path with nothing to interpret: no
/// absolute path, empty component, `.` or `..`, and no entry twice. `/` is
/// the whole workspace and stands alone.
#[test]
fn a_malformed_write_scope_is_refused_by_entry() {
    use execution_capnp::WorkspaceOperation as Op;
    for scope in [
        &[""][..],
        &["/etc/passwd"],
        &["src/../secrets/"],
        &["./src/"],
        &["src//a.rs"],
        &[".."],
        &["src/a.rs", "src/a.rs"],
        &["/", "docs/"],
        &["docs/", "/"],
        &["src/\0a.rs"],
    ] {
        let (spec, grant) = scoped_workspace_grant(vec![Op::Mutate], scope);
        let error =
            authorize(&spec, &grant, &microvm_policy()).expect_err("a malformed write scope");
        assert_eq!(
            error.code,
            leyline_runtime::ErrorCode::InvalidSpec,
            "{scope:?}: {error}"
        );
        assert!(error.detail.contains("writeScope"), "{scope:?}: {error}");
    }
    for scope in [&["a"][..], &["a/"], &["a/b/c.rs", "a/b/"], &[".hidden/x"]] {
        let (spec, grant) = scoped_workspace_grant(vec![Op::Mutate], scope);
        authorize(&spec, &grant, &microvm_policy())
            .unwrap_or_else(|error| panic!("{scope:?} is a legal scope: {error}"));
    }
}

/// Bead `ley-line-open-1022a8`: the service materializes a run's workspace
/// shares itself, before it calls the backend. A service with nothing to
/// serve a granted workspace from refuses the start without the backend ever
/// being asked to start anything: no worker exists to cancel.
#[test]
fn a_service_with_no_share_provider_refuses_a_granted_workspace_before_the_backend_is_called() {
    let policy = AuthorizationPolicy {
        now_unix_ms: Some(1_000),
        required_backend: BackendClass::MicroVm,
        required_confinement_digest: None,
    };
    let (spec, grant) = workspace_spec_and_grant(&"a".repeat(64));
    let backend = DroppingBackend::default();
    let service = ExecutionService::new(backend.clone());
    service
        .provision(BackendClass::MicroVm, "provision-no-provider")
        .expect("provision backend");
    let error = service
        .start_authorized(&spec, &grant, &policy, &TestResolver)
        .expect_err("nothing can serve the granted workspace");
    assert_eq!(
        error.code,
        leyline_runtime::ErrorCode::WorkspaceUnserved,
        "{error:?}"
    );
    assert!(error.detail.contains("\"repo\""), "{error:?}");
    assert_eq!(
        backend.starts.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the backend was asked to start a run whose workspace nothing serves"
    );
    assert!(
        backend.cancelled.lock().unwrap().is_empty(),
        "nothing was started, so nothing is cancelled"
    );
}

/// The backend receives its shares as input: by the time `Backend::start` is
/// called the service has opened the granted workspace, and what the backend
/// is handed is exactly the grant, write scope included.
///
/// The other direction 204a94 tested at run time, a backend claiming a share
/// the grant never named, no longer has a test because it no longer has a
/// spelling: a backend cannot construct an `AttachedWorkspace` (the
/// `compile_fail` examples on `ServedWorkspaces` and `AttachedWorkspace`).
#[test]
fn the_service_opens_each_granted_workspace_and_hands_it_to_the_backend() {
    /// Records the grants it is handed and how many shares the provider had
    /// opened when it was called, then attaches them.
    #[derive(Clone)]
    struct Receiving {
        shares: std::sync::Arc<OpenShares>,
        handed: std::sync::Arc<std::sync::Mutex<Vec<(usize, Vec<WorkspaceGrant>)>>>,
    }
    impl Backend for Receiving {
        fn capabilities(&self) -> BackendCapabilities {
            CompletingBackend.capabilities()
        }
        fn start(
            &self,
            request: &ExecutionRequest,
            workspaces: ServedWorkspaces,
        ) -> Result<BackendRun, ExecutionError> {
            assert!(workspaces.serve_exactly(request));
            self.handed.lock().unwrap().push((
                self.shares.opened.lock().unwrap().len(),
                workspaces.grants().cloned().collect(),
            ));
            SharingBackend.start(request, workspaces)
        }
        fn workspace_capacity(&self) -> usize {
            1
        }
        fn cancel(&self, _run_id: &str) -> Result<bool, ExecutionError> {
            Ok(true)
        }
    }

    use execution_capnp::WorkspaceOperation as Op;
    let (spec, grant) = scoped_workspace_grant(vec![Op::Read, Op::Mutate], &["src/a.rs", "docs/"]);
    let shares = std::sync::Arc::new(OpenShares::default());
    let backend = Receiving {
        shares: shares.clone(),
        handed: Default::default(),
    };
    let service = ExecutionService::new(backend.clone()).with_workspace_shares(shares.clone());
    service
        .provision(BackendClass::MicroVm, "provision-receiving")
        .expect("provision backend");
    let record = service
        .start_authorized(&spec, &grant, &microvm_policy(), &TestResolver)
        .expect("a served workspace starts");

    let granted = WorkspaceGrant {
        name: "repo".into(),
        graph_root: format!("blake3-256:{}", "a".repeat(64)),
        expected_generation: 0,
        operations: vec![WorkspaceOperation::Read, WorkspaceOperation::Mutate],
        write_scope: vec!["src/a.rs".into(), "docs/".into()],
    };
    assert_eq!(
        *shares.opened.lock().unwrap(),
        vec![(record.run_id.clone(), granted.clone())],
        "the service opens the run's one granted workspace, once, with its scope"
    );
    assert_eq!(
        *backend.handed.lock().unwrap(),
        vec![(1, vec![granted])],
        "the share was open before the backend was called, and is the grant"
    );
}

/// A proof names the run it was opened for and the whole grant, so a share a
/// backend kept from one start cannot stand in for another's. Run A is
/// granted `repo` with `mutate` over the whole workspace; the backend keeps
/// A's share and refuses A. Run B is granted the same workspace read-only.
/// Presenting A's share for B is refused: B's worker would hold a share that
/// lets it write.
#[test]
fn a_share_kept_from_another_grant_cannot_stand_in_for_this_runs() {
    #[derive(Clone, Default)]
    struct Stashing {
        stash: std::sync::Arc<std::sync::Mutex<Vec<leyline_runtime::ServedWorkspace>>>,
        cancelled: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }
    impl Backend for Stashing {
        fn capabilities(&self) -> BackendCapabilities {
            CompletingBackend.capabilities()
        }
        fn start(
            &self,
            _request: &ExecutionRequest,
            workspaces: ServedWorkspaces,
        ) -> Result<BackendRun, ExecutionError> {
            let mut stash = self.stash.lock().unwrap();
            if stash.is_empty() {
                stash.extend(workspaces.into_shares());
                return Err(ExecutionError::backend("the first start is refused"));
            }
            // This run's own shares are dropped; the kept ones are attached.
            Ok(BackendRun {
                backend_id: "stashing/1".into(),
                attached: stash
                    .drain(..)
                    .map(|share| {
                        let (service, attached) = share.attach();
                        HELD_SHARES.lock().unwrap().push(service);
                        attached
                    })
                    .collect(),
            })
        }
        fn workspace_capacity(&self) -> usize {
            1
        }
        fn cancel(&self, run_id: &str) -> Result<bool, ExecutionError> {
            self.cancelled.lock().unwrap().push(run_id.to_owned());
            Ok(true)
        }
    }

    use execution_capnp::WorkspaceOperation as Op;
    let backend = Stashing::default();
    let (service, shares) = sharing_service(backend.clone());
    service
        .provision(BackendClass::MicroVm, "provision-stashing")
        .expect("provision backend");
    let (spec, wide) = scoped_workspace_grant(vec![Op::Read, Op::Mutate], &["/"]);
    service
        .start_authorized(&spec, &wide, &microvm_policy(), &TestResolver)
        .expect_err("the backend refuses run A and keeps its share");
    let (spec, narrow) = scoped_workspace_grant(vec![Op::Read], &[]);
    let error = service
        .start_authorized(&spec, &narrow, &microvm_policy(), &TestResolver)
        .expect_err("run A's share must not serve run B");
    assert_eq!(
        error.code,
        leyline_runtime::ErrorCode::WorkspaceUnserved,
        "{error:?}"
    );
    assert!(error.detail.contains("another grant"), "{error:?}");
    assert_eq!(shares.opened.lock().unwrap().len(), 2);
    assert_eq!(
        backend.cancelled.lock().unwrap().len(),
        1,
        "the run started on a kept share is cancelled"
    );
}

/// The proof stands for a share that is being served. A backend that takes
/// the share and lets its service go has nothing answering its worker, so
/// the start is refused even though it holds a proof for the right grant.
#[test]
fn a_share_the_backend_took_and_let_go_is_not_served() {
    struct LettingGo(DroppingBackend);
    impl Backend for LettingGo {
        fn capabilities(&self) -> BackendCapabilities {
            self.0.capabilities()
        }
        fn start(
            &self,
            _request: &ExecutionRequest,
            workspaces: ServedWorkspaces,
        ) -> Result<BackendRun, ExecutionError> {
            Ok(BackendRun {
                backend_id: "letting-go/1".into(),
                attached: workspaces
                    .into_shares()
                    .into_iter()
                    .map(|share| share.attach().1)
                    .collect(),
            })
        }
        fn workspace_capacity(&self) -> usize {
            1
        }
        fn cancel(&self, run_id: &str) -> Result<bool, ExecutionError> {
            self.0.cancel(run_id)
        }
    }

    let dropping = DroppingBackend::default();
    let (service, _shares) = sharing_service(LettingGo(dropping.clone()));
    service
        .provision(BackendClass::MicroVm, "provision-letting-go")
        .expect("provision backend");
    let (spec, grant) = workspace_spec_and_grant(&"a".repeat(64));
    let error = service
        .start_authorized(&spec, &grant, &microvm_policy(), &TestResolver)
        .expect_err("a share nothing holds");
    assert_eq!(
        error.code,
        leyline_runtime::ErrorCode::WorkspaceUnserved,
        "{error:?}"
    );
    assert!(error.detail.contains("nothing holds it"), "{error:?}");
    assert_eq!(dropping.cancelled.lock().unwrap().len(), 1);
}

/// The backend is asked whether it serves workspaces before any share is
/// opened: a tier that serves none (the default) is refused with the
/// provider never asked and the backend never started.
#[test]
fn a_backend_that_serves_no_workspace_is_refused_before_a_share_is_opened() {
    let (service, shares) = sharing_service(RecordingBackend);
    service
        .provision(BackendClass::MicroVm, "provision-no-capacity")
        .expect("provision backend");
    let (spec, grant) = workspace_spec_and_grant(&"a".repeat(64));
    let error = service
        .start_authorized(&spec, &grant, &microvm_policy(), &TestResolver)
        .expect_err("the backend serves no workspace");
    assert_eq!(
        error.code,
        leyline_runtime::ErrorCode::WorkspaceUnserved,
        "{error:?}"
    );
    assert!(error.detail.contains("\"repo\""), "{error:?}");
    assert!(
        shares.opened.lock().unwrap().is_empty(),
        "a share was opened for a run the backend cannot serve"
    );
}

/// A request no grant was verified for holds no workspace: the direct
/// `ExecutionService::start` refuses one that names a workspace, before the
/// provider or the backend is asked anything.
#[test]
fn a_request_without_a_verified_grant_is_never_served_a_workspace() {
    let backend = DroppingBackend::default();
    let (service, shares) = sharing_service(backend.clone());
    service
        .provision(BackendClass::MicroVm, "provision-unverified")
        .expect("provision backend");
    let (spec, grant) = workspace_spec_and_grant(&"a".repeat(64));
    let authorized = authorize(&spec, &grant, &microvm_policy()).expect("authorize");
    let request = TestResolver.resolve(&authorized).expect("resolve");
    assert_eq!(request.workspaces.len(), 1);
    let error = service
        .start(request)
        .expect_err("a workspace with no verified grant behind it");
    assert_eq!(
        error.code,
        leyline_runtime::ErrorCode::WorkspaceUnserved,
        "{error:?}"
    );
    assert!(error.detail.contains("verified grant"), "{error:?}");
    assert!(shares.opened.lock().unwrap().is_empty());
    assert_eq!(backend.starts.load(std::sync::atomic::Ordering::SeqCst), 0);
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
                unavailable_reason: Some("fixture backend is unavailable".into()),
                enforced: leyline_runtime::EnforcedCeilings::wall_clock_only(),
            }
        }

        fn start(
            &self,
            _request: &ExecutionRequest,
            _workspaces: ServedWorkspaces,
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

/// `execution/v1/test-vectors/run-grant-signing.json` pins the bytes an
/// issuer signs and the signature over them, so a second implementation
/// checks its canonicalization and its DSSE framing without an LLO checkout.
/// Its cases differ only in `writeScope`, pinning that an unset field and an
/// explicit empty list sign different bytes.
#[test]
fn run_grant_signing_vector_pins_the_signed_bytes() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../ll-core/schema-spec/execution/v1/test-vectors/run-grant-signing.json");
    let vector: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("the signing vector ships"))
            .expect("the signing vector is JSON");
    assert_eq!(
        vector["payloadType"].as_str(),
        Some(GRANT_SIGNATURE_PAYLOAD_TYPE)
    );
    let seed: [u8; 32] = hex::decode(vector["signerSeedHex"].as_str().expect("seed"))
        .expect("seed hex")
        .try_into()
        .expect("a 32-byte seed");
    let signer = leyline_envelope::Ed25519RootSigner::from_seed(&seed);
    let cases = vector["cases"].as_array().expect("cases");
    assert_eq!(cases.len(), 3, "a vector with no cases gates nothing");

    let mut signed = std::collections::BTreeMap::new();
    for case in cases {
        let name = case["name"].as_str().expect("name");
        // The grant a case signs: the vector's `grant` with `workspaces`
        // set to the case's one workspace.
        let mut grant = vector["grant"].clone();
        grant["workspaces"] = json!([case["workspace"]]);
        let mut message = Builder::new_default();
        capnp_json::from_json(
            &grant.to_string(),
            message.init_root::<execution_capnp::run_grant::Builder<'_>>(),
        )
        .unwrap_or_else(|error| panic!("{name}: the grant decodes: {error}"));
        let mut bytes = Vec::new();
        capnp::serialize::write_message(&mut bytes, &message).expect("serialize");

        let signing = grant_signing_bytes(&bytes).expect("signing bytes");
        let signature =
            leyline_envelope::sign_payload(GRANT_SIGNATURE_PAYLOAD_TYPE, &signing, &signer);
        // The covered bytes are the canonical grant with `signature` set to
        // an EMPTY struct, not omitted: 24 bytes longer than the canonical
        // form of the grant as built above, which has no signature at all.
        let canonical = |empty_signature: bool| {
            let mut copy = Builder::new_default();
            copy.set_root(
                message
                    .get_root_as_reader::<execution_capnp::run_grant::Reader<'_>>()
                    .expect("grant root"),
            )
            .expect("copy");
            if empty_signature {
                copy.get_root::<execution_capnp::run_grant::Builder<'_>>()
                    .expect("grant root")
                    .init_signature();
            }
            capnp::Word::words_to_bytes(&copy.into_reader().canonicalize().expect("canonicalize"))
                .to_vec()
        };
        let (omitted, empty_struct) = (canonical(false), canonical(true));
        assert_eq!(signing, empty_struct, "{name}: the empty-struct form");
        assert_ne!(signing, omitted, "{name}: not the omitted form");
        assert_eq!(signing.len(), omitted.len() + 24, "{name}");

        assert_eq!(
            hex::encode(&signing),
            case["signingBytesHex"].as_str().unwrap_or_default(),
            "{name}: signing bytes drifted"
        );
        assert_eq!(
            signing.len() as u64,
            case["signingBytesLength"].as_u64().expect("length"),
            "{name}: signing bytes changed length"
        );
        assert_eq!(
            blake3::hash(&signing).to_hex().as_str(),
            case["signingBytesBlake3"]
                .as_str()
                .expect("signingBytesBlake3"),
            "{name}: signing bytes drifted"
        );
        assert_eq!(
            hex::encode(&signature),
            case["signatureHex"].as_str().expect("signatureHex"),
            "{name}: signature drifted"
        );

        // What an authorizer makes of it.
        let spec = spec_bytes_with_details(None, 0, &[("repo", &"a".repeat(64))]);
        let authorized = authorize(&spec, &bytes, &microvm_policy());
        assert_eq!(
            authorized.is_ok(),
            case["authorizes"].as_bool().expect("authorizes"),
            "{name}: {:?}",
            authorized.err()
        );
        signed.insert(name.to_owned(), signing);
    }
    assert_ne!(
        signed["read-only-write-scope-unset"], signed["read-only-write-scope-explicit-empty"],
        "list presence is signed"
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
/// libkrun applies `memory.limit` and `cpu` through `krun_set_vm_config`, so
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
                unavailable_reason: None,
                enforced: leyline_runtime::EnforcedCeilings::wall_clock_only(),
            }
        }
        fn start(
            &self,
            _request: &ExecutionRequest,
            _workspaces: ServedWorkspaces,
        ) -> Result<BackendRun, ExecutionError> {
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

    // TestResolver resolves memory.limit 128 MiB — a real ceiling under a
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
    // `TestResolver` requests both cpu and memory, and this tier applies
    // neither, so either name is a correct report — but it must name one, so
    // an operator learns which ceiling to drop rather than that "something"
    // was wrong.
    assert!(
        error.detail.contains("memory.limit") || error.detail.contains("cpu.quota"),
        "the rejection must name the ceiling: {}",
        error.detail
    );
}

/// The mirror, and the reason §4 is a rejection rather than a blanket ban:
/// the same ceiling on the tier that *can* apply it must still run.
/// libkrun passes `memory.limit` and `cpu` to `krun_set_vm_config`, so a microVM
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

/// A resolver that resolves as [`TestResolver`] and then drops the grant's
/// carried confinement document.
struct ManifestDroppingResolver;

impl ExecutionResolver for ManifestDroppingResolver {
    fn resolve(
        &self,
        authorized: &leyline_runtime::authorization::AuthorizedExecution,
    ) -> Result<ExecutionRequest, ExecutionError> {
        let mut request = TestResolver.resolve(authorized)?;
        request.confinement_manifest = None;
        Ok(request)
    }
}

/// The carried document is held to the same rule as the digest: carried, never
/// chosen (bead `ley-line-open-7a3109`). A resolver that dropped it would have
/// the worker compile LLO's own policy in place of the one the issuer signed;
/// the drift comparison would refuse it only when the two digests differ, and
/// only after the backend had started a worker for it.
#[test]
fn the_service_refuses_a_resolver_that_changes_the_carried_confinement_document() {
    use leyline_runtime::confinement::{ConfinementManifest, FsGrant};

    let policy_document = ConfinementManifest::new()
        .with_fs_grant(FsGrant::read_write("/run/rootfs/"))
        .expect("valid grant");
    let document = policy_document.to_canonical_json().expect("canonical");
    let digest = policy_document.confinement_digest().expect("digest");
    let value = digest
        .strip_prefix("blake3-256:")
        .expect("algorithm-prefixed digest")
        .to_owned();
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
    let policy = AuthorizationPolicy {
        now_unix_ms: Some(1_000),
        required_backend: BackendClass::MicroVm,
        required_confinement_digest: None,
    };
    let service = ExecutionService::new(RecordingBackend);
    service
        .provision(BackendClass::MicroVm, "provision-manifest")
        .expect("provision backend");
    let error = service
        .start_authorized(&spec, &grant, &policy, &ManifestDroppingResolver)
        .expect_err("a resolver must carry the grant's confinement document");
    assert_eq!(
        error.code,
        leyline_runtime::ErrorCode::IdentityPolicyMismatch
    );
    assert!(error.detail.contains("confinement document"), "{error}");
    service
        .start_authorized(&spec, &grant, &policy, &TestResolver)
        .expect("a resolver that carries it starts the run");
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
