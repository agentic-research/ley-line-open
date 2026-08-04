use std::collections::BTreeMap;

use leyline_runtime::{
    ArtifactIdentity, AuthorizedExecution, CatalogBuilder, CatalogResolver, DigestRef, ErrorCode,
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
        // Committed by digest alone — this exercises the resolver, not the
        // grant reader that verifies a carried document against its digest.
        confinement_manifest: None,
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
                cpu_millis: 2_000,
                output_bytes: 0,
            },
        },
    }
}

#[test]
fn catalog_resolver_rejects_limits_that_would_round_up() {
    let mut intent = authorized();
    intent.intent.requested_limits.cpu_millis = 1_001;
    let error = resolver()
        .resolve(&intent)
        .expect_err("fractional CPU units must not be widened");
    assert_eq!(error.code, ErrorCode::InvalidSpec);

    let mut intent = authorized();
    // This is divisible by the two obvious broken unit factors (`1024 + 1024`
    // and `1024 / 1024`) but not by one MiB. It therefore proves that this
    // check uses the exact backend unit rather than merely rejecting a small
    // value later as zero MiB.
    intent.intent.requested_limits.memory_bytes = 1024 * 1024 + 2048;
    let error = resolver()
        .resolve(&intent)
        .expect_err("fractional MiB units must not be widened");
    assert_eq!(error.code, ErrorCode::InvalidSpec);
}

fn resolver() -> CatalogResolver {
    CatalogBuilder::default()
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
    let error = CatalogBuilder::default()
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

#[test]
fn catalog_builder_rejects_each_empty_identity_component() {
    for (digest, media_type) in [
        ("", "application/vnd.leyline.executable"),
        ("blake3-256:artifact", ""),
    ] {
        let error = CatalogBuilder::default()
            .entry(
                digest,
                media_type,
                DigestRef {
                    algorithm: "blake3-256".into(),
                    value: "a".repeat(64),
                },
                "bin/agent",
                Vec::new(),
            )
            .build()
            .expect_err("partial artifact identity must fail closed");
        assert_eq!(error.code, ErrorCode::InvalidSpec);
    }
}

#[test]
fn catalog_builder_rejects_each_unsafe_guest_path_shape() {
    for executable in ["", "/bin/agent", "bin/../agent"] {
        let error = CatalogBuilder::default()
            .entry(
                "blake3-256:artifact",
                "application/vnd.leyline.executable",
                DigestRef {
                    algorithm: "blake3-256".into(),
                    value: "a".repeat(64),
                },
                executable,
                Vec::new(),
            )
            .build()
            .expect_err("guest executable must remain relative and traversal-free");
        assert_eq!(error.code, ErrorCode::InvalidSpec);
    }
}

#[test]
fn catalog_resolver_rejects_each_zero_backend_limit() {
    for mutate in [
        |limits: &mut SchemaLimits| limits.cpu_millis = 0,
        |limits: &mut SchemaLimits| limits.memory_bytes = 0,
        |limits: &mut SchemaLimits| limits.wall_time_ms = 0,
    ] {
        let mut intent = authorized();
        mutate(&mut intent.intent.requested_limits);
        let error = resolver()
            .resolve(&intent)
            .expect_err("every backend limit must be non-zero");
        assert_eq!(error.code, ErrorCode::InvalidSpec);
    }
}

#[test]
fn catalog_resolver_loads_the_explicit_json_document_shape() {
    let json = format!(
        r#"{{"entries":[{{
            "artifactDigest":"blake3-256:artifact",
            "mediaType":"application/vnd.leyline.executable",
            "rootfs":{{"algorithm":"blake3-256","value":"{}"}},
            "executable":"bin/agent",
            "workspaceInputs":[{{"name":"repo","graphRoot":"blake3-256:graph"}}]
        }}]}}"#,
        "a".repeat(64)
    );
    let resolver = CatalogResolver::from_json(json.as_bytes()).expect("catalog JSON");
    let request = resolver
        .resolve(&authorized())
        .expect("resolve JSON catalog");
    assert_eq!(request.executable, "bin/agent");
}
