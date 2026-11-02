//! The canonical execution/v1 fixture the transport tests share: a backend
//! with deterministic lifecycle, a resolver that carries every grant-owned
//! field, and one generated RunSpec and RunGrant in both their Cap'n Proto
//! and JSON forms. Used by `execution_transport.rs` and
//! `execution_transport_parity.rs`.

#![allow(dead_code)] // each test binary uses a different subset

use std::collections::BTreeMap;
use std::sync::Arc;

use capnp::message::{Builder, ReaderOptions};
use leyline_cli_lib::daemon::execution::RuntimeExecutionHandler;
use leyline_public_schema::execution_capnp;
use leyline_runtime::authorization::{
    AuthorizationPolicy, EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION, canonical_digest,
};
use leyline_runtime::{
    Backend, BackendCapabilities, BackendClass, BackendRun, BackendRunStatus, ExecutionError,
    ExecutionRequest, ExecutionResolver, ExecutionService, ResourceLimits,
};
use serde_json::Value;

pub struct CompletingBackend;

impl Backend for CompletingBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            backend_id: "transport-test/1".into(),
            backend_class: BackendClass::MicroVm,
            available: true,
            unavailable_reason: None,
            enforced: leyline_runtime::EnforcedCeilings::hypervisor_backed(),
        }
    }

    fn start(&self, _request: &ExecutionRequest) -> Result<BackendRun, ExecutionError> {
        Ok(BackendRun {
            backend_id: "transport-test/1".into(),
            channels: Vec::new(),
        })
    }

    fn poll(&self, _run_id: &str) -> Result<Option<BackendRunStatus>, ExecutionError> {
        Ok(None)
    }

    fn cancel(&self, _run_id: &str) -> Result<bool, ExecutionError> {
        Ok(true)
    }
}

pub struct Resolver;

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
            // Carried, not chosen — like every other field above. Writing
            // `String::new()` here is what `service.rs` now refuses: both
            // backends gate the ADR-0035 drift comparison on the digest being
            // non-empty, so a resolver that merely dropped it silently disabled
            // the attestation for that run.
            //
            // This is the SECOND fixture found doing it, independently of the
            // one in `runtime/tests/authorization.rs`. Two test resolvers, two
            // silent opt-outs, no failing test between them — which is the
            // argument for the check living on the seam rather than in prose on
            // one implementation.
            confinement_digest: authorized.confinement_digest.clone(),
            confinement_manifest: authorized.confinement_manifest.clone(),
            workspaces: authorized.workspaces.clone(),
            limits: ResourceLimits {
                wall_time_ms: 1_000,
                memory: leyline_runtime::MemoryLimits { limit: 64 << 20 },
                cpu: leyline_runtime::CpuLimits {
                    quota: 100_000,
                    period: 100_000,
                },
                ..ResourceLimits::default()
            },
        })
    }
}

/// The clock the fixture grant is evaluated at: it expires at 2 000.
pub fn policy() -> AuthorizationPolicy {
    AuthorizationPolicy {
        now_unix_ms: Some(1_000),
        required_backend: BackendClass::MicroVm,
        required_confinement_digest: None,
    }
}

/// The first-party handler over `service`, as `leyline execution-daemon
/// --allow-unverified-evidence` builds it.
pub fn handler(
    service: &Arc<ExecutionService<CompletingBackend>>,
) -> RuntimeExecutionHandler<CompletingBackend, Resolver> {
    RuntimeExecutionHandler::new_with_verifier(
        Arc::clone(service),
        policy(),
        Arc::new(Resolver),
        Arc::new(leyline_runtime::MetadataOnlyEvidenceVerifier),
    )
}

fn digest(mut digest: execution_capnp::digest_ref::Builder<'_>, value: &str) {
    digest.set_algorithm("blake3-256");
    digest.set_value(value);
}

fn evidence(mut evidence: execution_capnp::evidence_ref::Builder<'_>) {
    evidence.set_media_type("application/test-evidence");
    digest(evidence.init_digest(), &"a".repeat(64));
}

pub fn spec_bytes() -> Vec<u8> {
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

pub fn grant_bytes(spec: &[u8]) -> Vec<u8> {
    grant_bytes_for(spec, BackendClass::MicroVm)
}

/// The fixture grant, authorizing `class`.
pub fn grant_bytes_for(spec: &[u8], class: BackendClass) -> Vec<u8> {
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
    grant.set_backend_class(match class {
        BackendClass::Native => execution_capnp::BackendClass::Native,
        BackendClass::MicroVm => execution_capnp::BackendClass::MicroVm,
    });
    let capabilities = grant.init_capabilities(1);
    let mut capability = capabilities.get(0);
    capability.set_grant(EXECUTION_CAPABILITY);
    capability.set_interface(EXECUTION_SCHEMA_VERSION);
    let mut bytes = Vec::new();
    capnp::serialize::write_message(&mut bytes, &message).expect("serialize grant");
    bytes
}

pub fn spec_json(bytes: &[u8]) -> Value {
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

pub fn grant_json(bytes: &[u8]) -> Value {
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
