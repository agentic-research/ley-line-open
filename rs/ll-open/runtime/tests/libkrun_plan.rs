use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;

use leyline_runtime::backends::libkrun::plan::{DirectoryRootfsResolver, compile_plan};
use leyline_runtime::{DigestRef, ErrorCode, ExecutionRequest, ResourceLimits};
use tempfile::TempDir;

struct RootfsFixture {
    _cas: TempDir,
    request: ExecutionRequest,
    executable: std::path::PathBuf,
}

fn rootfs_fixture() -> RootfsFixture {
    let cas = TempDir::new().expect("temporary CAS");
    let content = b"probe-v1";
    let content_digest = blake3::hash(content).to_hex().to_string();
    let manifest = format!(
        "{{\"version\":1,\"files\":[{{\"path\":\"usr/bin/probe\",\"mode\":493,\"blake3\":\"{content_digest}\"}}]}}"
    );
    let root_digest = blake3::hash(manifest.as_bytes()).to_hex().to_string();
    let rootfs = cas.path().join(&root_digest);
    let executable = rootfs.join("usr/bin/probe");
    fs::create_dir_all(executable.parent().expect("executable parent")).expect("rootfs dirs");
    fs::write(&executable, content).expect("rootfs executable");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).expect("executable mode");
    fs::write(rootfs.join("rootfs.manifest.json"), manifest).expect("rootfs manifest");

    RootfsFixture {
        request: ExecutionRequest {
            run_id: "run-plan-01".into(),
            replay_key: "replay-plan-01".into(),
            rootfs: DigestRef {
                algorithm: "blake3-256".into(),
                value: root_digest,
            },
            executable: "usr/bin/probe".into(),
            arguments: vec!["probe".into(), "--json".into()],
            public_environment: BTreeMap::from([("CI".into(), "true".into())]),
            allowed_egress: Vec::new(),
            confinement_digest: String::new(),
            confinement_manifest: None,
            limits: ResourceLimits {
                vcpus: 2,
                memory_mib: 2048,
                wall_time_ms: 30_000,
            },
        },
        _cas: cas,
        executable,
    }
}

#[test]
fn compiles_only_a_manifest_verified_content_addressed_rootfs() {
    // Catches a resolver accepting a caller-selected host path or failing to
    // bind the materialized rootfs bytes to the requested content identity.
    let fixture = rootfs_fixture();
    let resolver = DirectoryRootfsResolver::new(fixture._cas.path());

    let plan = compile_plan(&resolver, &fixture.request).expect("verified plan");

    assert_eq!(plan.run_id, "run-plan-01");
    assert_eq!(plan.rootfs.digest, fixture.request.rootfs);
    assert_eq!(
        plan.rootfs.canonical_path,
        fixture
            ._cas
            .path()
            .join(&fixture.request.rootfs.value)
            .canonicalize()
            .expect("canonical fixture root")
    );
    assert_eq!(
        plan.executable.to_str().expect("exec text"),
        "usr/bin/probe"
    );
    assert_eq!(plan.vcpus, 2);
    assert_eq!(plan.ram_mib, 2048);
}

#[test]
fn modified_rootfs_content_is_rejected_before_vm_preparation() {
    // Catches a mutable directory retaining a once-valid manifest identity.
    let fixture = rootfs_fixture();
    fs::write(&fixture.executable, b"modified-after-resolution").expect("mutate fixture");
    let resolver = DirectoryRootfsResolver::new(fixture._cas.path());

    let error = compile_plan(&resolver, &fixture.request).expect_err("mutation must fail");

    assert_eq!(error.code, ErrorCode::InvalidSpec);
    assert!(error.detail.contains("content digest"), "{error}");
}

#[test]
fn argument_with_interior_nul_is_rejected_before_ffi() {
    // Catches truncation when Rust strings cross the C ABI.
    let mut fixture = rootfs_fixture();
    fixture.request.arguments.push("bad\0suffix".into());
    let resolver = DirectoryRootfsResolver::new(fixture._cas.path());

    let error = compile_plan(&resolver, &fixture.request).expect_err("NUL must fail");

    assert_eq!(error.code, ErrorCode::InvalidSpec);
    assert!(error.detail.contains("NUL"), "{error}");
}

#[test]
fn unlisted_rootfs_file_invalidates_the_content_identity() {
    // Catches a verifier authenticating listed files while silently exposing
    // additional mutable files to the guest.
    let fixture = rootfs_fixture();
    let extra = fixture
        ._cas
        .path()
        .join(&fixture.request.rootfs.value)
        .join("ambient-secret");
    fs::write(extra, b"not in manifest").expect("extra rootfs file");
    let resolver = DirectoryRootfsResolver::new(fixture._cas.path());

    let error = compile_plan(&resolver, &fixture.request).expect_err("extra file must fail");

    assert_eq!(error.code, ErrorCode::InvalidSpec);
    assert!(error.detail.contains("manifest"), "{error}");
}

#[test]
fn rootfs_symlink_is_rejected_even_when_its_target_bytes_match() {
    // Catches host namespace indirection introduced after content
    // verification.
    let fixture = rootfs_fixture();
    let target = fixture.executable.with_file_name("probe-target");
    fs::rename(&fixture.executable, &target).expect("move executable target");
    symlink(&target, &fixture.executable).expect("rootfs symlink");
    let resolver = DirectoryRootfsResolver::new(fixture._cas.path());

    let error = compile_plan(&resolver, &fixture.request).expect_err("symlink must fail");

    assert_eq!(error.code, ErrorCode::InvalidSpec);
    assert!(error.detail.contains("symbolic link"), "{error}");
}
