use std::collections::BTreeMap;
use std::sync::Arc;

use capnp::message::{Builder, ReaderOptions};
use leyline_cli_lib::daemon::DaemonExt;
use leyline_cli_lib::daemon::execution::{ExecutionDaemonExt, RuntimeExecutionHandler};
use leyline_public_schema::execution_capnp;
use leyline_runtime::authorization::{
    AuthorizationPolicy, EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION, canonical_digest,
};
use leyline_runtime::{
    Backend, BackendCapabilities, BackendClass, BackendRun, BackendRunStatus, ExecutionError,
    ExecutionRequest, ExecutionResolver, ExecutionService, ResourceLimits,
};
use serde_json::{Value, json};

struct CompletingBackend;

impl Backend for CompletingBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            backend_id: "transport-test/1".into(),
            backend_class: BackendClass::MicroVm,
            available: true,
        }
    }

    fn start(&self, _request: &ExecutionRequest) -> Result<BackendRun, ExecutionError> {
        Ok(BackendRun {
            backend_id: "transport-test/1".into(),
        })
    }

    fn poll(&self, _run_id: &str) -> Result<Option<BackendRunStatus>, ExecutionError> {
        Ok(None)
    }

    fn cancel(&self, _run_id: &str) -> Result<bool, ExecutionError> {
        Ok(true)
    }
}

struct Resolver;

impl ExecutionResolver for Resolver {
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
            arguments: Vec::new(),
            public_environment: BTreeMap::new(),
            allowed_egress: authorized.allowed_egress.clone(),
            limits: ResourceLimits {
                vcpus: 1,
                memory_mib: 64,
                wall_time_ms: 1_000,
            },
        })
    }
}

fn digest(mut digest: execution_capnp::digest_ref::Builder<'_>, value: &str) {
    digest.set_algorithm("blake3-256");
    digest.set_value(value);
}

fn evidence(mut evidence: execution_capnp::evidence_ref::Builder<'_>) {
    evidence.set_media_type("application/test-evidence");
    digest(evidence.init_digest(), &"a".repeat(64));
}

fn spec_bytes() -> Vec<u8> {
    let mut message = Builder::new_default();
    let mut spec = message.init_root::<execution_capnp::run_spec::Builder<'_>>();
    spec.set_schema_version(EXECUTION_SCHEMA_VERSION);
    let mut executable = spec.reborrow().init_executable();
    executable.set_media_type("application/test-executable");
    digest(executable.reborrow().init_digest(), &"c".repeat(64));
    let mut bytes = Vec::new();
    capnp::serialize::write_message(&mut bytes, &message).expect("serialize spec");
    bytes
}

fn grant_bytes(spec: &[u8]) -> Vec<u8> {
    let spec_digest = canonical_digest(spec)
        .expect("spec digest")
        .strip_prefix("blake3-256:")
        .expect("digest prefix")
        .to_owned();
    let mut message = Builder::new_default();
    let mut grant = message.init_root::<execution_capnp::run_grant::Builder<'_>>();
    grant.set_grant_id("grant-transport");
    grant.set_expires_at_unix_ms(2_000);
    grant.set_replay_key("replay-transport");
    digest(grant.reborrow().init_run_spec_digest(), &spec_digest);
    evidence(grant.reborrow().init_issuer_evidence());
    evidence(grant.reborrow().init_workload_identity_evidence());
    evidence(grant.reborrow().init_actor_provenance_evidence());
    digest(grant.reborrow().init_confinement_digest(), &"b".repeat(64));
    grant.set_backend_class(execution_capnp::BackendClass::MicroVm);
    let capabilities = grant.init_capabilities(1);
    let mut capability = capabilities.get(0);
    capability.set_grant(EXECUTION_CAPABILITY);
    capability.set_interface(EXECUTION_SCHEMA_VERSION);
    let mut bytes = Vec::new();
    capnp::serialize::write_message(&mut bytes, &message).expect("serialize grant");
    bytes
}

fn spec_json(bytes: &[u8]) -> Value {
    let mut input = bytes;
    let message = capnp::serialize::read_message(&mut input, ReaderOptions::new()).unwrap();
    serde_json::from_str(
        &capnp_json::to_json(
            message
                .get_root::<execution_capnp::run_spec::Reader<'_>>()
                .unwrap(),
        )
        .unwrap(),
    )
    .unwrap()
}

fn grant_json(bytes: &[u8]) -> Value {
    let mut input = bytes;
    let message = capnp::serialize::read_message(&mut input, ReaderOptions::new()).unwrap();
    serde_json::from_str(
        &capnp_json::to_json(
            message
                .get_root::<execution_capnp::run_grant::Reader<'_>>()
                .unwrap(),
        )
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn canonical_fixture_uses_one_runtime_handler_for_lifecycle_operations() {
    let spec = spec_bytes();
    let grant = grant_bytes(&spec);
    let service = Arc::new(ExecutionService::new(CompletingBackend));
    let handler = RuntimeExecutionHandler::new_with_verifier(
        Arc::clone(&service),
        AuthorizationPolicy {
            now_unix_ms: Some(1_000),
            required_backend: BackendClass::MicroVm,
            required_confinement_digest: None,
        },
        Arc::new(Resolver),
        Arc::new(leyline_runtime::MetadataOnlyEvidenceVerifier),
    );

    let extension = ExecutionDaemonExt::new(Arc::new(handler));
    let handler = extension
        .execution_handler()
        .expect("execution extension must expose its handler");

    let capabilities: Value = serde_json::from_str(&handler.capabilities().unwrap()).unwrap();
    assert!(capabilities.to_string().contains("cloister/execution/v1"));
    let provision: Value = serde_json::from_str(
        &handler
            .provision(&json!({"backendClass":"microVm","idempotencyKey":"p-transport"}))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(provision["provisioned"], true);

    let start: Value = serde_json::from_str(
        &handler
            .start(&json!({
                "spec": spec_json(&spec),
                "grant": grant_json(&grant),
            }))
            .unwrap(),
    )
    .unwrap();
    let run_id = start["runId"].as_str().expect("run id").to_owned();
    let status: Value =
        serde_json::from_str(&handler.status(&json!({"runId": run_id})).unwrap()).unwrap();
    assert_eq!(status["state"], "running");

    let inspection: Value = serde_json::from_str(
        &handler
            .inspect(&json!({"runId": run_id, "afterSequence": 0}))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(inspection["runId"], run_id);
    assert!(inspection["events"].as_array().is_some());

    let cancel: Value = serde_json::from_str(
        &handler
            .cancel(&json!({"runId": run_id, "idempotencyKey":"c-transport"}))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(cancel["state"], "cancelled");
    let receipt: Value =
        serde_json::from_str(&handler.collect(&json!({"runId": run_id})).unwrap()).unwrap();
    assert_eq!(receipt["receipt"]["runId"], run_id);
    assert_eq!(receipt["receipt"]["terminalState"], "cancelled");

    let cleanup: Value = serde_json::from_str(
        &handler
            .cleanup(&json!({"runId": run_id, "idempotencyKey":"x-transport"}))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(cleanup["runId"], run_id);
    assert_eq!(cleanup["state"], "cleaned");
}
