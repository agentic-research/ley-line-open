use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

use leyline_runtime::backends::libkrun::worker::{WorkerEvent, WorkerOptions};
use leyline_runtime::{DigestRef, ErrorCode, ExecutionRequest, ResourceLimits};
use tempfile::TempDir;

fn request_fixture(cas: &TempDir) -> ExecutionRequest {
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

    ExecutionRequest {
        run_id: "run-worker-01".into(),
        replay_key: "replay-worker-01".into(),
        rootfs: DigestRef {
            algorithm: "blake3-256".into(),
            value: root_digest,
        },
        executable: "usr/bin/probe".into(),
        arguments: vec!["probe".into()],
        public_environment: BTreeMap::new(),
        allowed_egress: Vec::new(),
        confinement_digest: String::new(),
        limits: ResourceLimits {
            vcpus: 1,
            memory_mib: 512,
            wall_time_ms: 10_000,
        },
    }
}

#[test]
fn first_party_worker_never_falls_back_to_the_krunvm_cli() {
    // Catches a regression to Cloister's old shell-wrapper architecture. A
    // PATH trap makes this behavioral rather than a source-text assertion.
    let cas = TempDir::new().expect("CAS");
    let request = request_fixture(&cas);
    let run_root = TempDir::new().expect("run root");
    let trap = TempDir::new().expect("PATH trap");
    let marker = trap.path().join("krunvm-was-invoked");
    let krunvm = trap.path().join("krunvm");
    fs::write(
        &krunvm,
        format!("#!/bin/sh\nprintf invoked > '{}'\n", marker.display()),
    )
    .expect("krunvm trap");
    fs::set_permissions(&krunvm, fs::Permissions::from_mode(0o755)).expect("trap mode");

    let mut child = Command::new(env!("CARGO_BIN_EXE_leyline-krun-worker"))
        .args([
            "--cas-root",
            cas.path().to_str().expect("CAS UTF-8"),
            "--libkrun",
            "/definitely/missing/libkrun",
            "--run-root",
            run_root.path().to_str().expect("run root UTF-8"),
        ])
        .env("PATH", trap.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn first-party worker");
    serde_json::to_writer(child.stdin.as_mut().expect("worker stdin"), &request)
        .expect("write worker request");
    child.stdin.take().expect("close worker stdin").flush().ok();

    let output = child.wait_with_output().expect("worker output");

    assert!(!output.status.success());
    let event: WorkerEvent = serde_json::from_slice(&output.stderr).expect("structured error");
    let WorkerEvent::Failed { error } = event else {
        panic!("expected failed worker event, got {event:?}");
    };
    assert_eq!(error.code, ErrorCode::BackendFailed);
    assert!(error.detail.contains("load libkrun shared library"));
    assert!(!marker.exists(), "worker invoked the krunvm PATH trap");
    assert_eq!(
        fs::read(run_root.path().join("rootfs/usr/bin/probe")).expect("ephemeral executable"),
        b"probe-v1"
    );
    assert_eq!(
        fs::read(cas.path().join(&request.rootfs.value).join("usr/bin/probe"))
            .expect("immutable executable"),
        b"probe-v1"
    );
}

#[test]
fn worker_options_require_an_explicit_run_root() {
    let error = WorkerOptions::parse([
        OsString::from("--cas-root"),
        OsString::from("/cas"),
        OsString::from("--libkrun"),
        OsString::from("/libkrun"),
    ])
    .expect_err("run root is required");
    assert!(error.detail.contains("missing --run-root option"));

    let options = WorkerOptions::parse([
        OsString::from("--cas-root"),
        OsString::from("/cas"),
        OsString::from("--libkrun"),
        OsString::from("/libkrun"),
        OsString::from("--run-root"),
        OsString::from("/run-root"),
    ])
    .expect("explicit run root");
    assert_eq!(options.run_root, std::path::PathBuf::from("/run-root"));
}
