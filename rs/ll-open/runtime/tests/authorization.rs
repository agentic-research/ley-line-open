use capnp::message::Builder;
use capnp::message::ReaderOptions;
use leyline_public_schema::execution_capnp;
use leyline_runtime::authorization::{
    AuthorizationPolicy, EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION, authorize,
    canonical_digest,
};
use leyline_runtime::transport::{
    capabilities_json, cleanup_json, collect_json, start_json, status_json,
};
use leyline_runtime::{
    Backend, BackendCapabilities, BackendClass, BackendRun, ExecutionError, ExecutionRequest,
    ExecutionResolver, ExecutionService, ResourceLimits,
};
use serde_json::json;
use std::collections::BTreeMap;

fn spec_bytes() -> Vec<u8> {
    spec_bytes_with_interface(None, 0)
}

fn spec_bytes_with_interface(interface: Option<&str>, wall_time_ms: u64) -> Vec<u8> {
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

fn grant_bytes(spec_bytes: &[u8], capability: bool, expires_at: u64, wall_time_ms: u64) -> Vec<u8> {
    let spec_digest = canonical_digest(spec_bytes)
        .expect("canonical spec digest")
        .strip_prefix("blake3-256:")
        .expect("digest prefix")
        .to_owned();
    let mut message = Builder::new_default();
    let mut grant = message.init_root::<execution_capnp::run_grant::Builder<'_>>();
    grant.set_grant_id("grant-01");
    grant.set_expires_at_unix_ms(expires_at);
    grant.set_replay_key("replay-01");
    set_digest(grant.reborrow().init_run_spec_digest(), &spec_digest);
    set_evidence(grant.reborrow().init_issuer_evidence());
    set_evidence(grant.reborrow().init_workload_identity_evidence());
    set_evidence(grant.reborrow().init_actor_provenance_evidence());
    set_digest(grant.reborrow().init_confinement_digest(), &"b".repeat(64));
    if wall_time_ms != 0 {
        grant
            .reborrow()
            .init_limits()
            .set_wall_time_ms(wall_time_ms);
    }
    grant.set_backend_class(execution_capnp::BackendClass::MicroVm);
    let capabilities = grant.init_capabilities(u32::from(capability));
    if capability {
        let mut entry = capabilities.get(0);
        entry.set_grant(EXECUTION_CAPABILITY);
        entry.set_interface(EXECUTION_SCHEMA_VERSION);
    }
    let mut bytes = Vec::new();
    capnp::serialize::write_message(&mut bytes, &message).expect("serialize grant");
    bytes
}

#[test]
fn binds_grant_to_spec_and_derives_run_id() {
    let spec = spec_bytes();
    let grant = grant_bytes(&spec, true, 2_000, 0);
    let authorized = authorize(
        &spec,
        &grant,
        &AuthorizationPolicy {
            now_unix_ms: 1_000,
            required_backend: BackendClass::MicroVm,
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
            now_unix_ms: 1_000,
            required_backend: BackendClass::MicroVm,
        },
    )
    .expect_err("expired grant must be rejected");
    assert!(error.detail.contains("expired"));

    let missing = grant_bytes(&spec, false, 2_000, 0);
    let error = authorize(
        &spec,
        &missing,
        &AuthorizationPolicy {
            now_unix_ms: 1_000,
            required_backend: BackendClass::MicroVm,
        },
    )
    .expect_err("grant without execution capability must be rejected");
    assert!(error.detail.contains("capability"));
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
            now_unix_ms: 1_000,
            required_backend: BackendClass::MicroVm,
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
            now_unix_ms: 1_000,
            required_backend: BackendClass::MicroVm,
        },
    )
    .expect_err("widened grant must be rejected");
    assert!(error.detail.contains("widens"));
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
    let record = service
        .start_authorized(
            &spec,
            &grant,
            &AuthorizationPolicy {
                now_unix_ms: 1_000,
                required_backend: BackendClass::MicroVm,
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
        now_unix_ms: 1_000,
        required_backend: BackendClass::MicroVm,
    };
    let start = start_json(&service, &input, &policy, &TestResolver).expect("start JSON");
    assert!(start.contains("run-"));
    let status = status_json(&service, r#"{"runId":""}"#).expect("status JSON");
    assert!(status.contains("test/1"));
    let capabilities = capabilities_json(&service).expect("capabilities JSON");
    assert!(capabilities.contains("cloister/execution/v1"));

    let start_value: serde_json::Value = serde_json::from_str(&start).unwrap();
    let run_id = start_value["runId"].as_str().unwrap();
    service.cancel(run_id).expect("cancel for receipt");
    let receipt = collect_json(&service, &json!({"runId": run_id}).to_string())
        .expect("collect receipt JSON");
    assert!(receipt.contains("eventLogRoot"));
    let cleanup =
        cleanup_json(&service, &json!({"runId": run_id}).to_string()).expect("cleanup JSON");
    assert!(cleanup.contains("cleaned"));
}
