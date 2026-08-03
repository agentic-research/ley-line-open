use capnp::message::Builder;
use leyline_public_schema::execution_capnp;
use leyline_runtime::BackendClass;
use leyline_runtime::authorization::{
    AuthorizationPolicy, EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION, authorize,
    canonical_digest,
};

fn spec_bytes() -> Vec<u8> {
    spec_bytes_with_interface(None)
}

fn spec_bytes_with_interface(interface: Option<&str>) -> Vec<u8> {
    let mut message = Builder::new_default();
    let mut spec = message.init_root::<execution_capnp::run_spec::Builder<'_>>();
    spec.set_schema_version(EXECUTION_SCHEMA_VERSION);
    if let Some(interface) = interface {
        let mut interfaces = spec.reborrow().init_requested_interfaces(1);
        interfaces.set(0, interface);
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

fn grant_bytes(spec_bytes: &[u8], capability: bool, expires_at: u64) -> Vec<u8> {
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
    let grant = grant_bytes(&spec, true, 2_000);
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
    let expired = grant_bytes(&spec, true, 1_000);
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

    let missing = grant_bytes(&spec, false, 2_000);
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
    let other_spec = spec_bytes_with_interface(Some("different/interface"));
    let grant = grant_bytes(&other_spec, true, 2_000);
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
