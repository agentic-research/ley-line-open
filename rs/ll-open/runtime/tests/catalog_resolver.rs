use std::collections::BTreeMap;

use leyline_runtime::{
    ArtifactIdentity, AuthorizedExecution, CatalogResolver, DigestRef, ErrorCode,
    ExecutionResolver, SchemaIntent, SchemaLimits, WorkspaceInput,
};

fn authorized() -> AuthorizedExecution {
    AuthorizedExecution {
        run_id: "run-1".into(),
        grant_id: "grant-1".into(),
        replay_key: "replay-1".into(),
        spec_digest: "blake3-256:spec".into(),
        grant_digest: "blake3-256:grant".into(),
        confinement_digest: "blake3-256:confinement".into(),
        backend: leyline_runtime::BackendClass::Native,
        allowed_egress: Vec::new(),
        intent: SchemaIntent {
            executable: ArtifactIdentity {
                digest: "blake3-256:artifact".into(),
                media_type: "application/vnd.leyline.executable".into(),
            },
            arguments: vec!["--audit".into()],
            public_environment: BTreeMap::from([("MODE".into(), "audit".into())]),
            workspace_inputs: vec![WorkspaceInput {
                name: "repo".into(),
                graph_root: "blake3-256:graph".into(),
            }],
            requested_limits: SchemaLimits {
                wall_time_ms: 2_000,
                memory_bytes: 2 * 1024 * 1024,
                cpu_millis: 1_001,
                output_bytes: 0,
            },
        },
    }
}

fn resolver() -> CatalogResolver {
    CatalogResolver::builder()
        .entry(
            "blake3-256:artifact",
            "application/vnd.leyline.executable",
            DigestRef {
                algorithm: "blake3-256".into(),
                value: "a".repeat(64),
            },
            "bin/agent",
            vec![WorkspaceInput {
                name: "repo".into(),
                graph_root: "blake3-256:graph".into(),
            }],
        )
        .build()
        .expect("catalog entry is valid")
}

#[test]
fn catalog_resolver_binds_artifact_and_workspace_to_guest_request() {
    let request = resolver().resolve(&authorized()).expect("resolve request");

    assert_eq!(request.run_id, "run-1");
    assert_eq!(request.replay_key, "replay-1");
    assert_eq!(request.executable, "bin/agent");
    assert_eq!(request.arguments, vec!["--audit"]);
    assert_eq!(request.public_environment["MODE"], "audit");
    assert_eq!(request.limits.vcpus, 2);
    assert_eq!(request.limits.memory_mib, 2);
    assert_eq!(request.limits.wall_time_ms, 2_000);
}

#[test]
fn catalog_resolver_rejects_unknown_artifacts_before_host_resolution() {
    let mut intent = authorized();
    intent.intent.executable.digest = "blake3-256:unknown".into();

    let error = resolver()
        .resolve(&intent)
        .expect_err("unknown artifact must fail closed");
    assert_eq!(error.code, ErrorCode::IdentityPolicyMismatch);
}

#[test]
fn catalog_resolver_rejects_workspace_identity_drift() {
    let mut intent = authorized();
    intent.intent.workspace_inputs[0].graph_root = "blake3-256:other".into();

    let error = resolver()
        .resolve(&intent)
        .expect_err("workspace identity drift must fail closed");
    assert_eq!(error.code, ErrorCode::IdentityPolicyMismatch);
}

#[test]
fn catalog_resolver_rejects_unenforced_output_limit() {
    let mut intent = authorized();
    intent.intent.requested_limits.output_bytes = 1;

    let error = resolver()
        .resolve(&intent)
        .expect_err("unenforced output limit must fail closed");
    assert_eq!(error.code, ErrorCode::UnsupportedBackend);
}

#[test]
fn catalog_builder_rejects_duplicate_artifact_identities() {
    let error = CatalogResolver::builder()
        .entry(
            "blake3-256:artifact",
            "application/vnd.leyline.executable",
            DigestRef {
                algorithm: "blake3-256".into(),
                value: "a".repeat(64),
            },
            "bin/agent",
            Vec::new(),
        )
        .entry(
            "blake3-256:artifact",
            "application/vnd.leyline.executable",
            DigestRef {
                algorithm: "blake3-256".into(),
                value: "b".repeat(64),
            },
            "bin/other",
            Vec::new(),
        )
        .build()
        .expect_err("ambiguous catalog identity must fail closed");
    assert_eq!(error.code, ErrorCode::ResourceConflict);
}
