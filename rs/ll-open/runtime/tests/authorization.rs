use capnp::message::Builder;
use capnp::message::ReaderOptions;
use leyline_public_schema::execution_capnp;
use leyline_runtime::authorization::{
    AuthorizationPolicy, EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION, EvidenceRef,
    EvidenceStore, EvidenceVerifier, MetadataOnlyEvidenceVerifier, authorize_with_verifier,
    canonical_digest,
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

fn set_evidence(mut evidence: execution_capnp::evidence_ref::Builder<'_>) {
    evidence.set_media_type("application/test-evidence");
    set_digest(evidence.init_digest(), &"a".repeat(64));
}

struct RejectEvidence;

impl EvidenceVerifier for RejectEvidence {
    fn verify(&self, field: &str, _evidence: &EvidenceRef) -> Result<(), ExecutionError> {
        Err(ExecutionError {
            code: leyline_runtime::ErrorCode::Unauthenticated,
            retryable: false,
            detail: format!("unverified evidence: {field}"),
        })
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
    workspaces: Vec<(&'a str, &'a str, Vec<execution_capnp::WorkspaceOperation>)>,
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
            confinement_value: &confinement_value,
            workspaces: Vec::new(),
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
    set_evidence(grant.reborrow().init_issuer_evidence());
    set_evidence(grant.reborrow().init_workload_identity_evidence());
    set_evidence(grant.reborrow().init_actor_provenance_evidence());
    let mut confinement = grant.reborrow().init_confinement_digest();
    confinement.set_algorithm(fixture.confinement_algorithm);
    confinement.set_value(fixture.confinement_value);
    if fixture.wall_time_ms != 0 {
        grant
            .reborrow()
            .init_limits()
            .set_wall_time_ms(fixture.wall_time_ms);
    }
    grant.set_backend_class(execution_capnp::BackendClass::MicroVm);
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
                confinement_value: &confinement,
                workspaces: Vec::new(),
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
                confinement_value: value,
                workspaces: Vec::new(),
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
        &RejectEvidence,
    )
    .expect_err("unverified upstream evidence must fail closed");
    assert_eq!(error.code, leyline_runtime::ErrorCode::Unauthenticated);
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
}

#[test]
fn cas_dsse_verifier_binds_and_verifies_apas_evidence() {
    let signer = leyline_envelope::Ed25519RootSigner::from_seed(&[11u8; 32]);
    let statement = leyline_envelope::Statement::new(
        Vec::new(),
        "https://rosary.dev/Handoff/v1",
        serde_json::json!({"dispatchId":"run-01"}),
    );
    let bytes = leyline_envelope::Envelope::sign(&statement, &signer).to_json_vec();
    let digest = format!("blake3-256:{}", blake3::hash(&bytes).to_hex());
    let verifier = leyline_runtime::CasDsseEvidenceVerifier::new(
        std::sync::Arc::new(FixtureEvidenceStore(bytes)),
        vec![signer.verifying_key()],
    );
    verifier
        .verify(
            "actorProvenanceEvidence",
            &EvidenceRef {
                media_type: "application/vnd.in-toto+json".into(),
                digest,
            },
        )
        .expect("valid APAS DSSE evidence");
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
    let digest = format!("blake3-256:{}", blake3::hash(&bytes).to_hex());
    let verifier = leyline_runtime::CasDsseEvidenceVerifier::new(
        std::sync::Arc::new(FixtureEvidenceStore(bytes)),
        vec![signer.verifying_key()],
    );
    let error = verifier
        .verify(
            "actorProvenanceEvidence",
            &EvidenceRef {
                media_type: "application/vnd.in-toto+json".into(),
                digest,
            },
        )
        .expect_err("a signed unrelated statement is not APAS evidence");
    assert_eq!(error.code, leyline_runtime::ErrorCode::Unauthenticated);
    assert!(error.detail.contains("not APAS"));
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
            confinement_value: &confinement,
            workspaces: vec![("repo", &"a".repeat(64), read.clone())],
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
            confinement_value: &confinement,
            workspaces: vec![
                ("repo", &"a".repeat(64), read.clone()),
                ("extra", &"b".repeat(64), read.clone()),
            ],
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
            confinement_value: &confinement,
            workspaces: vec![("repo", &"c".repeat(64), read.clone())],
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
            confinement_value: &confinement,
            workspaces: vec![
                ("repo", &"a".repeat(64), read.clone()),
                ("repo", &"b".repeat(64), read),
            ],
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
