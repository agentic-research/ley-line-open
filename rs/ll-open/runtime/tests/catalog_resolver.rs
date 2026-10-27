use std::collections::BTreeMap;

use leyline_runtime::{
    ArtifactIdentity, AuthorizedExecution, CatalogBuilder, CatalogResolver, CpuLimits, DigestRef,
    ErrorCode, ExecutionResolver, MemoryLimits, ResourceLimits, SchemaIntent, WorkspaceGrant,
    WorkspaceInput, WorkspaceOperation,
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
            requested_limits: ResourceLimits {
                wall_time_ms: 2_000,
                memory: MemoryLimits { limit: 4 << 20 },
                cpu: CpuLimits {
                    quota: 200_000,
                    period: 100_000,
                },
                ..ResourceLimits::default()
            },
        },
        // Narrower than the request on memory and CPU.
        limits: ResourceLimits {
            wall_time_ms: 2_000,
            memory: MemoryLimits { limit: 2 << 20 },
            cpu: CpuLimits {
                quota: 100_000,
                period: 100_000,
            },
            ..ResourceLimits::default()
        },
        workspaces: vec![WorkspaceGrant {
            name: "repo".into(),
            graph_root: "blake3-256:graph".into(),
            expected_generation: 4,
            operations: vec![WorkspaceOperation::Read, WorkspaceOperation::Mutate],
        }],
    }
}

#[test]
fn catalog_resolver_holds_the_run_to_the_grant_not_the_request() {
    // `authorize` verified the grant narrows the request; the resolver once
    // built the run from the request anyway, so a narrowed grant ran wide.
    let mut intent = authorized();
    intent.intent.requested_limits.memory.limit = 1 << 30;
    intent.limits.memory.limit = 512 << 20;
    let request = resolver().resolve(&intent).expect("resolve");
    assert_eq!(request.limits.memory.limit, 512 << 20);
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
    // The grant's ceilings, not the spec's request.
    assert_eq!(request.limits, authorized().limits);
    assert_ne!(request.limits, authorized().intent.requested_limits);
    // Carried, never chosen: the grant's workspace authority, operations
    // and generation included, reaches the backend unchanged.
    assert_eq!(request.workspaces, authorized().workspaces);
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
fn catalog_resolver_refuses_a_run_without_a_wall_clock() {
    let mut intent = authorized();
    intent.limits.wall_time_ms = 0;
    let error = resolver()
        .resolve(&intent)
        .expect_err("the supervisor needs a deadline");
    assert_eq!(error.code, ErrorCode::InvalidSpec);
}

#[test]
fn catalog_resolver_accepts_a_run_with_no_memory_or_cpu_ceiling() {
    // Zero means "no ceiling" now, so a grant that sets none resolves; it is
    // the selected tier that decides whether it needs one (the microVM sizes
    // its guest from both), not the resolver.
    let mut intent = authorized();
    intent.limits.memory = MemoryLimits::default();
    intent.limits.cpu = CpuLimits::default();
    let request = resolver()
        .resolve(&intent)
        .expect("no ceiling is a valid grant");
    assert_eq!(request.limits.memory.limit, 0);
    assert_eq!(request.limits.cpu.quota, 0);
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
