use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use leyline_runtime::backends::native_backend::{NativeWorkerBackend, NativeWorkerConfig};
use leyline_runtime::{Backend, BackendClass, DigestRef, ExecutionRequest, ResourceLimits};
use tempfile::TempDir;

fn request() -> ExecutionRequest {
    ExecutionRequest {
        run_id: "native-run-01".into(),
        replay_key: "native-replay-01".into(),
        rootfs: DigestRef {
            algorithm: "blake3-256".into(),
            value: "a".repeat(64),
        },
        executable: "usr/bin/true".into(),
        arguments: vec!["true".into()],
        public_environment: BTreeMap::new(),
        allowed_egress: Vec::new(),
        limits: ResourceLimits {
            vcpus: 1,
            memory_mib: 64,
            wall_time_ms: 10_000,
        },
    }
}

fn backend(fixture: &TempDir, worker_body: &str) -> (NativeWorkerBackend, std::path::PathBuf) {
    let worker = fixture.path().join("leyline-native-worker");
    fs::write(&worker, worker_body).expect("worker fixture");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).expect("worker mode");
    let cas_root = fixture.path().join("cas");
    let ephemeral_root = fixture.path().join("runs");
    fs::create_dir(&cas_root).expect("CAS root");
    fs::create_dir(&ephemeral_root).expect("ephemeral root");
    let backend = NativeWorkerBackend::new(NativeWorkerConfig {
        worker,
        cas_root,
        ephemeral_root: ephemeral_root.clone(),
        runtime_files: Vec::new(),
        ready_timeout: Duration::from_secs(2),
    });
    (backend, ephemeral_root)
}

#[test]
fn capabilities_are_native_and_fail_closed_when_resources_are_missing() {
    let fixture = TempDir::new().expect("fixture");
    let backend = NativeWorkerBackend::new(NativeWorkerConfig {
        worker: fixture.path().join("missing-worker"),
        cas_root: fixture.path().join("missing-cas"),
        ephemeral_root: fixture.path().join("missing-runs"),
        runtime_files: Vec::new(),
        ready_timeout: Duration::from_secs(1),
    });
    let capabilities = backend.capabilities();
    assert_eq!(capabilities.backend_id, "native-nono/1");
    assert_eq!(capabilities.backend_class, BackendClass::Native);
    assert!(!capabilities.available);
}

#[test]
fn worker_exit_is_observable_and_run_root_is_removed() {
    let fixture = TempDir::new().expect("fixture");
    let (backend, runs) = backend(
        &fixture,
        "#!/bin/sh\n/bin/cat >/dev/null\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"native-run-01\"}' >&2\n",
    );
    backend.start(&request()).expect("worker readiness");
    let deadline = Instant::now() + Duration::from_secs(1);
    let status = loop {
        if let Some(status) = backend.poll("native-run-01").expect("poll") {
            break status;
        }
        assert!(Instant::now() < deadline, "worker did not finish");
        std::thread::yield_now();
    };
    assert!(matches!(
        status,
        leyline_runtime::BackendRunStatus::Succeeded
    ));
    assert_eq!(fs::read_dir(runs).expect("runs").count(), 0);
}

#[test]
fn failed_worker_is_reported_and_cleaned() {
    let fixture = TempDir::new().expect("fixture");
    let (backend, runs) = backend(
        &fixture,
        "#!/bin/sh\n/bin/cat >/dev/null\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"native-run-01\"}' >&2\nexit 7\n",
    );
    backend.start(&request()).expect("worker readiness");
    let deadline = Instant::now() + Duration::from_secs(1);
    let status = loop {
        if let Some(status) = backend.poll("native-run-01").expect("poll") {
            break status;
        }
        assert!(Instant::now() < deadline, "worker did not finish");
        std::thread::yield_now();
    };
    match status {
        leyline_runtime::BackendRunStatus::Failed(error) => {
            assert!(error.detail.contains("native worker exited"));
        }
        other => panic!("expected failed worker, got {other:?}"),
    }
    assert_eq!(fs::read_dir(runs).expect("runs").count(), 0);
}

#[test]
fn cancel_kills_worker_and_removes_run_root() {
    let fixture = TempDir::new().expect("fixture");
    let (backend, runs) = backend(
        &fixture,
        "#!/bin/sh\n/bin/cat >/dev/null\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"native-run-01\"}' >&2\n/usr/bin/tail -f /dev/null\n",
    );
    backend.start(&request()).expect("worker readiness");
    assert_eq!(fs::read_dir(&runs).expect("runs").count(), 1);
    assert!(backend.cancel("native-run-01").expect("cancel"));
    assert_eq!(fs::read_dir(runs).expect("runs").count(), 0);
    assert!(
        backend
            .poll("native-run-01")
            .expect("poll after cancel")
            .is_none()
    );
    assert!(!backend.cancel("native-run-01").expect("repeat cancel"));
}
