use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

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
        confinement_digest: String::new(),
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
        // The fixture launches a shell worker and may share a loaded host
        // with the libkrun suite. Keep this test budget generous; readiness
        // is still synchronized by the event, never by a sleep.
        ready_timeout: Duration::from_secs(10),
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
fn every_native_resource_is_required_independently() {
    let fixture = TempDir::new().expect("fixture");
    let worker = fixture.path().join("worker");
    let cas_root = fixture.path().join("cas");
    let ephemeral_root = fixture.path().join("runs");
    let runtime_file = fixture.path().join("runtime");
    fs::write(&worker, "#!/bin/sh\n").expect("worker");
    fs::create_dir(&cas_root).expect("CAS");
    fs::create_dir(&ephemeral_root).expect("runs");
    fs::write(&runtime_file, "runtime").expect("runtime file");

    let configurations = [
        NativeWorkerConfig {
            worker: fixture.path().join("missing-worker"),
            cas_root: cas_root.clone(),
            ephemeral_root: ephemeral_root.clone(),
            runtime_files: vec![runtime_file.clone()],
            ready_timeout: Duration::from_secs(1),
        },
        NativeWorkerConfig {
            worker: worker.clone(),
            cas_root: fixture.path().join("missing-cas"),
            ephemeral_root: ephemeral_root.clone(),
            runtime_files: vec![runtime_file.clone()],
            ready_timeout: Duration::from_secs(1),
        },
        NativeWorkerConfig {
            worker: worker.clone(),
            cas_root: cas_root.clone(),
            ephemeral_root: fixture.path().join("missing-runs"),
            runtime_files: vec![runtime_file.clone()],
            ready_timeout: Duration::from_secs(1),
        },
        NativeWorkerConfig {
            worker,
            cas_root,
            ephemeral_root,
            runtime_files: vec![fixture.path().join("missing-runtime")],
            ready_timeout: Duration::from_secs(1),
        },
    ];
    for configuration in configurations {
        assert!(
            !NativeWorkerBackend::new(configuration)
                .capabilities()
                .available
        );
    }
}

#[test]
fn worker_exit_is_observable_and_run_root_is_removed() {
    let fixture = TempDir::new().expect("fixture");
    let (backend, runs) = backend(
        &fixture,
        "#!/bin/sh\n/bin/cat >/dev/null\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"native-run-01\"}' >&2\n",
    );
    backend.start(&request()).expect("worker readiness");
    let status = backend
        .wait_for_completion("native-run-01")
        .expect("worker completion");
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
    let status = backend
        .wait_for_completion("native-run-01")
        .expect("worker completion");
    match status {
        leyline_runtime::BackendRunStatus::Failed(error) => {
            assert!(error.detail.contains("native worker exited"));
        }
        other => panic!("expected failed worker, got {other:?}"),
    }
    assert_eq!(fs::read_dir(runs).expect("runs").count(), 0);
    let errors = backend.take_cleanup_errors();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].detail.contains("native worker exited"));
}

#[test]
fn readiness_for_another_run_is_rejected_and_cleaned() {
    let fixture = TempDir::new().expect("fixture");
    let (backend, runs) = backend(
        &fixture,
        "#!/bin/sh\n/bin/cat >/dev/null\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"other-run\"}' >&2\n",
    );
    let error = backend
        .start(&request())
        .expect_err("worker readiness must bind to the requested run");
    assert!(error.detail.contains("unexpected run"));
    assert_eq!(fs::read_dir(runs).expect("runs").count(), 0);
}

#[test]
fn native_cleanup_restores_guest_created_permissions() {
    let fixture = TempDir::new().expect("fixture");
    let (backend, runs) = backend(
        &fixture,
        "#!/bin/sh\n/bin/cat >/dev/null\nmkdir -p \"$4/locked\"\nprintf x > \"$4/locked/file\"\nchmod 000 \"$4/locked\"\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"native-run-01\"}' >&2\n",
    );
    backend.start(&request()).expect("worker readiness");
    backend
        .wait_for_completion("native-run-01")
        .expect("worker completion");
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

#[test]
fn backend_trait_cancel_delegates_and_drop_waits_for_cleanup() {
    let fixture = TempDir::new().expect("fixture");
    let (backend, runs) = backend(
        &fixture,
        "#!/bin/sh\n/bin/cat >/dev/null\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"native-run-01\"}' >&2\n/usr/bin/tail -f /dev/null\n",
    );
    backend.start(&request()).expect("worker readiness");
    assert!(Backend::cancel(&backend, "native-run-01").expect("trait cancel"));
    assert_eq!(fs::read_dir(&runs).expect("runs").count(), 0);

    backend.start(&request()).expect("second worker readiness");
    assert_eq!(fs::read_dir(&runs).expect("runs").count(), 1);
    drop(backend);
    assert_eq!(fs::read_dir(runs).expect("runs").count(), 0);
}

/// ADR-0035 finding 2's last link: the worker attests the policy it compiled,
/// and a run whose attestation disagrees with the grant never reaches
/// `Running`.
///
/// The comparison cannot be made daemon-side. The policy is compiled in the
/// worker, after fork — `apply_auto` runs there, and the rootfs path comes
/// from a resolver that canonicalizes a materialized tree. For the daemon to
/// compute the digest itself it would have to re-derive that path, which is
/// two implementations of one derivation: exactly the drift the single
/// manifest exists to prevent, one layer up.
///
/// So the worker reports, and the supervisor checks. This fixture worker
/// reports a policy nobody authorized.
#[test]
fn a_worker_attesting_an_unauthorized_policy_never_reaches_running() {
    let fixture = TempDir::new().expect("fixture");
    // Well-formed readiness, correct run id, wrong policy.
    let (backend, _root) = backend(
        &fixture,
        // Readiness travels on stderr — the backend nulls stdout.
        r#"#!/bin/sh
echo '{"type":"ready","run_id":"native-run-01","confinement_digest":"blake3-256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"}' >&2
sleep 30
"#,
    );

    let mut request = request();
    request.confinement_digest = format!("blake3-256:{}", "a".repeat(64));

    let error = backend
        .start(&request)
        .expect_err("a worker attesting an unauthorized policy must not start");
    assert!(
        format!("{error:?}").contains("confinement drift"),
        "the refusal must name drift, so an operator learns the worker applied \
         a policy the grant did not authorize: {error:?}"
    );
}
