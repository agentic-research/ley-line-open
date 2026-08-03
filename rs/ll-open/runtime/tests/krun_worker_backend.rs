use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Barrier};
use std::time::Duration;

use leyline_runtime::backends::libkrun::backend::{KrunWorkerBackend, KrunWorkerConfig};
use leyline_runtime::{
    Backend, BackendClass, BackendRunStatus, DigestRef, ExecutionRequest, ResourceLimits,
};
use tempfile::TempDir;

fn run_root_count(path: &std::path::Path) -> usize {
    fs::read_dir(path).expect("enumerate run roots").count()
}

const READY_TIMEOUT: Duration = Duration::from_secs(15);

fn request() -> ExecutionRequest {
    ExecutionRequest {
        run_id: "run-backend-01".into(),
        replay_key: "replay-backend-01".into(),
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
            memory_mib: 512,
            wall_time_ms: 10_000,
        },
    }
}

#[test]
fn backend_spawns_the_explicit_first_party_worker_and_waits_for_ready() {
    // Catches reporting a run as started before the confined worker has
    // prepared libkrun, and catches PATH-based backend selection.
    let fixture = TempDir::new().expect("fixture");
    let request_log = fixture.path().join("request.json");
    let worker = fixture.path().join("leyline-krun-worker");
    fs::write(
        &worker,
        format!(
            "#!/bin/sh\n/bin/cat > '{}'\nprintf '%s\\n' '{{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}}' >&2\n/usr/bin/tail -f /dev/null\n",
            request_log.display()
        ),
    )
    .expect("fake worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).expect("worker mode");
    let cas = fixture.path().join("cas");
    fs::create_dir(&cas).expect("CAS");
    let libkrun = fixture.path().join("libkrun.dylib");
    fs::write(&libkrun, b"library").expect("library fixture");
    let ephemeral_root = fixture.path().join("runs");
    fs::create_dir(&ephemeral_root).expect("ephemeral root");
    let backend = KrunWorkerBackend::new(KrunWorkerConfig {
        worker,
        cas_root: cas,
        ephemeral_root: ephemeral_root.clone(),
        libkrun,
        runtime_files: Vec::new(),
        devices: Vec::new(),
        ready_timeout: READY_TIMEOUT,
    });

    let capabilities = backend.capabilities();
    assert!(capabilities.available);
    assert_eq!(capabilities.backend_class, BackendClass::MicroVm);
    let started = backend.start(&request()).expect("ready worker");

    assert_eq!(started.backend_id, "libkrun/1");
    let observed: ExecutionRequest =
        serde_json::from_slice(&fs::read(request_log).expect("request log")).expect("request JSON");
    assert_eq!(observed, request());
    assert_eq!(run_root_count(&ephemeral_root), 1);

    drop(backend);
    assert_eq!(run_root_count(&ephemeral_root), 0);
}

#[test]
fn backend_removes_the_run_root_when_a_worker_exits() {
    let fixture = TempDir::new().expect("fixture");
    let worker = fixture.path().join("leyline-krun-worker");
    fs::write(
        &worker,
        "#!/bin/sh\nrun_root=\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = \"--run-root\" ]; then run_root=\"$2\"; shift 2; else shift; fi\ndone\n/bin/cat >/dev/null\ncontrol=\"$run_root/control\"\n/usr/bin/mkfifo \"$control\"\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}' >&2\nIFS= read -r _ < \"$control\"\n",
    )
    .expect("fake worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).expect("worker mode");
    let cas_root = fixture.path().join("cas");
    let ephemeral_root = fixture.path().join("runs");
    fs::create_dir(&cas_root).expect("CAS");
    fs::create_dir(&ephemeral_root).expect("ephemeral root");
    let libkrun = fixture.path().join("libkrun.dylib");
    fs::write(&libkrun, b"library").expect("library fixture");
    let backend = KrunWorkerBackend::new(KrunWorkerConfig {
        worker,
        cas_root,
        ephemeral_root: ephemeral_root.clone(),
        libkrun,
        runtime_files: Vec::new(),
        devices: Vec::new(),
        ready_timeout: READY_TIMEOUT,
    });

    backend.start(&request()).expect("ready worker");
    assert_eq!(run_root_count(&ephemeral_root), 1);
    let run_root = fs::read_dir(&ephemeral_root)
        .expect("run roots")
        .next()
        .expect("one run root")
        .expect("run root entry")
        .path();
    fs::write(run_root.join("control"), b"exit\n").expect("release worker");
    backend
        .wait_for_completion("run-backend-01")
        .expect("worker completion");

    assert_eq!(run_root_count(&ephemeral_root), 0);
}

#[test]
fn backend_cancel_terminates_the_worker_and_removes_its_run_root() {
    let fixture = TempDir::new().expect("fixture");
    let worker = fixture.path().join("leyline-krun-worker");
    fs::write(
        &worker,
        "#!/bin/sh\n/bin/cat >/dev/null\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}' >&2\n/usr/bin/tail -f /dev/null\n",
    )
    .expect("fake worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).expect("worker mode");
    let cas_root = fixture.path().join("cas");
    let ephemeral_root = fixture.path().join("runs");
    fs::create_dir(&cas_root).expect("CAS");
    fs::create_dir(&ephemeral_root).expect("ephemeral root");
    let libkrun = fixture.path().join("libkrun.dylib");
    fs::write(&libkrun, b"library").expect("library fixture");
    let backend = KrunWorkerBackend::new(KrunWorkerConfig {
        worker,
        cas_root,
        ephemeral_root: ephemeral_root.clone(),
        libkrun,
        runtime_files: Vec::new(),
        devices: Vec::new(),
        ready_timeout: READY_TIMEOUT,
    });

    backend.start(&request()).expect("ready worker");
    assert_eq!(run_root_count(&ephemeral_root), 1);
    assert!(backend.cancel("run-backend-01").expect("cancel worker"));

    assert_eq!(run_root_count(&ephemeral_root), 0);
    assert!(!backend.cancel("run-backend-01").expect("repeat cancel"));
}

#[test]
fn backend_enforces_the_wall_clock_limit_and_cleans_up() {
    let fixture = TempDir::new().expect("fixture");
    let worker = fixture.path().join("leyline-krun-worker");
    fs::write(
        &worker,
        "#!/bin/sh\n/bin/cat >/dev/null\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}' >&2\n/usr/bin/tail -f /dev/null\n",
    )
    .expect("fake worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).expect("worker mode");
    let cas_root = fixture.path().join("cas");
    let ephemeral_root = fixture.path().join("runs");
    fs::create_dir(&cas_root).expect("CAS");
    fs::create_dir(&ephemeral_root).expect("ephemeral root");
    let libkrun = fixture.path().join("libkrun.dylib");
    fs::write(&libkrun, b"library").expect("library fixture");
    let backend = KrunWorkerBackend::new(KrunWorkerConfig {
        worker,
        cas_root,
        ephemeral_root: ephemeral_root.clone(),
        libkrun,
        runtime_files: Vec::new(),
        devices: Vec::new(),
        ready_timeout: READY_TIMEOUT,
    });
    let mut request = request();
    request.limits.wall_time_ms = 50;

    backend.start(&request).expect("ready worker");
    let completion = backend
        .wait_for_completion("run-backend-01")
        .expect("worker completion");

    assert_eq!(run_root_count(&ephemeral_root), 0);
    let errors = backend.take_cleanup_errors();
    assert!(matches!(completion, BackendRunStatus::Failed(_)));
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0].code,
        leyline_runtime::ErrorCode::ResourceExhausted
    );
}

#[test]
fn backend_cleanup_handles_guest_created_restrictive_directories() {
    let fixture = TempDir::new().expect("fixture");
    let worker = fixture.path().join("leyline-krun-worker");
    fs::write(
        &worker,
        "#!/bin/sh\n/bin/cat >/dev/null\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}' >&2\n/usr/bin/tail -f /dev/null\n",
    )
    .expect("fake worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).expect("worker mode");
    let cas_root = fixture.path().join("cas");
    let ephemeral_root = fixture.path().join("runs");
    fs::create_dir(&cas_root).expect("CAS");
    fs::create_dir(&ephemeral_root).expect("ephemeral root");
    let libkrun = fixture.path().join("libkrun.dylib");
    fs::write(&libkrun, b"library").expect("library fixture");
    let backend = KrunWorkerBackend::new(KrunWorkerConfig {
        worker,
        cas_root,
        ephemeral_root: ephemeral_root.clone(),
        libkrun,
        runtime_files: Vec::new(),
        devices: Vec::new(),
        ready_timeout: READY_TIMEOUT,
    });

    backend.start(&request()).expect("ready worker");
    let run_root = fs::read_dir(&ephemeral_root)
        .expect("run roots")
        .next()
        .expect("one run root")
        .expect("run root entry")
        .path();
    let locked = run_root.join("guest-locked");
    fs::create_dir(&locked).expect("guest directory");
    fs::write(locked.join("state"), b"guest").expect("guest state");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("restrict directory");

    assert!(backend.cancel("run-backend-01").expect("cancel worker"));
    assert_eq!(run_root_count(&ephemeral_root), 0);
    assert!(backend.take_cleanup_errors().is_empty());
}

#[test]
fn failed_start_removes_a_restrictive_worker_created_run_root() {
    let fixture = TempDir::new().expect("fixture");
    let worker = fixture.path().join("leyline-krun-worker");
    fs::write(
        &worker,
        r#"#!/bin/sh
run_root=
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--run-root" ]; then run_root="$2"; shift 2; else shift; fi
done
/bin/cat >/dev/null
/bin/mkdir "$run_root/guest-locked"
/bin/chmod 000 "$run_root/guest-locked"
printf '%s\n' '{"type":"failed","error":{"code":"backend-failed","retryable":false,"detail":"injected setup failure"}}' >&2
"#,
    )
    .expect("fake worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).expect("worker mode");
    let cas_root = fixture.path().join("cas");
    let ephemeral_root = fixture.path().join("runs");
    fs::create_dir(&cas_root).expect("CAS");
    fs::create_dir(&ephemeral_root).expect("ephemeral root");
    let libkrun = fixture.path().join("libkrun.dylib");
    fs::write(&libkrun, b"library").expect("library fixture");
    let backend = KrunWorkerBackend::new(KrunWorkerConfig {
        worker,
        cas_root,
        ephemeral_root: ephemeral_root.clone(),
        libkrun,
        runtime_files: Vec::new(),
        devices: Vec::new(),
        ready_timeout: READY_TIMEOUT,
    });

    let error = backend.start(&request()).expect_err("worker setup failure");

    assert!(error.detail.contains("injected setup failure"));
    assert_eq!(run_root_count(&ephemeral_root), 0);
}

#[test]
fn backend_rejects_a_duplicate_run_id_without_replacing_the_live_worker() {
    let fixture = TempDir::new().expect("fixture");
    let worker = fixture.path().join("leyline-krun-worker");
    fs::write(
        &worker,
        "#!/bin/sh\n/bin/cat >/dev/null\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}' >&2\n/usr/bin/tail -f /dev/null\n",
    )
    .expect("fake worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).expect("worker mode");
    let cas_root = fixture.path().join("cas");
    let ephemeral_root = fixture.path().join("runs");
    fs::create_dir(&cas_root).expect("CAS");
    fs::create_dir(&ephemeral_root).expect("ephemeral root");
    let libkrun = fixture.path().join("libkrun.dylib");
    fs::write(&libkrun, b"library").expect("library fixture");
    let backend = KrunWorkerBackend::new(KrunWorkerConfig {
        worker,
        cas_root,
        ephemeral_root: ephemeral_root.clone(),
        libkrun,
        runtime_files: Vec::new(),
        devices: Vec::new(),
        ready_timeout: READY_TIMEOUT,
    });

    backend.start(&request()).expect("first worker");
    let error = backend.start(&request()).expect_err("duplicate run ID");

    assert!(error.detail.contains("run_id already active"));
    assert_eq!(run_root_count(&ephemeral_root), 1);
    assert!(backend.cancel("run-backend-01").expect("cancel worker"));
}

#[test]
fn concurrent_starts_reserve_a_run_id_before_spawning() {
    let fixture = TempDir::new().expect("fixture");
    let worker = fixture.path().join("leyline-krun-worker");
    fs::write(
        &worker,
        "#!/bin/sh\n/bin/cat >/dev/null\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}' >&2\n/usr/bin/tail -f /dev/null\n",
    )
    .expect("fake worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).expect("worker mode");
    let cas_root = fixture.path().join("cas");
    let ephemeral_root = fixture.path().join("runs");
    fs::create_dir(&cas_root).expect("CAS");
    fs::create_dir(&ephemeral_root).expect("ephemeral root");
    let libkrun = fixture.path().join("libkrun.dylib");
    fs::write(&libkrun, b"library").expect("library fixture");
    let backend = Arc::new(KrunWorkerBackend::new(KrunWorkerConfig {
        worker,
        cas_root,
        ephemeral_root,
        libkrun,
        runtime_files: Vec::new(),
        devices: Vec::new(),
        ready_timeout: READY_TIMEOUT,
    }));
    let barrier = Arc::new(Barrier::new(3));
    let starts: Vec<_> = (0..2)
        .map(|_| {
            let backend = Arc::clone(&backend);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                backend.start(&request())
            })
        })
        .collect();

    barrier.wait();
    let results: Vec<_> = starts
        .into_iter()
        .map(|start| start.join().expect("start thread"))
        .collect();

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let conflict = results
        .iter()
        .find_map(|result| result.as_ref().err())
        .expect("one conflict");
    assert_eq!(conflict.code, leyline_runtime::ErrorCode::ResourceConflict);
    assert!(backend.cancel("run-backend-01").expect("cancel worker"));
}
