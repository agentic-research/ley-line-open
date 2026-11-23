use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use leyline_runtime::backends::native_backend::{NativeWorkerBackend, NativeWorkerConfig};
use leyline_runtime::{
    Backend, BackendClass, DigestRef, ExecutionRequest, ResourceLimits, ServedWorkspaces,
};
use tempfile::TempDir;

mod common;

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
        confinement_manifest: None,
        workspaces: Vec::new(),
        // The wall clock only: without a delegated cgroup root this tier
        // enforces nothing else, and refuses a memory, CPU or pids ceiling.
        limits: ResourceLimits {
            wall_time_ms: 10_000,
            ..ResourceLimits::default()
        },
    }
}

fn publish_executable(path: &std::path::Path, body: &str) {
    fs::write(path, body).expect("worker fixture");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("worker mode");
}

fn backend(fixture: &TempDir, worker_body: &str) -> (NativeWorkerBackend, std::path::PathBuf) {
    let worker = fixture.path().join("leyline-native-worker");
    publish_executable(&worker, worker_body);
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
    let _serial = common::serial();
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
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let worker = fixture.path().join("worker");
    let cas_root = fixture.path().join("cas");
    let ephemeral_root = fixture.path().join("runs");
    let runtime_file = fixture.path().join("runtime");
    fs::write(&worker, "#!/bin/sh\n").expect("worker");
    fs::create_dir(&cas_root).expect("CAS");
    fs::create_dir(&ephemeral_root).expect("runs");
    fs::write(&runtime_file, "runtime").expect("runtime file");

    let complete = NativeWorkerBackend::new(NativeWorkerConfig {
        worker: worker.clone(),
        cas_root: cas_root.clone(),
        ephemeral_root: ephemeral_root.clone(),
        runtime_files: vec![runtime_file.clone()],
        ready_timeout: Duration::from_secs(1),
    })
    .capabilities();
    assert!(complete.available, "{complete:?}");
    assert_eq!(complete.unavailable_reason, None);

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
        let capabilities = NativeWorkerBackend::new(configuration).capabilities();
        assert!(!capabilities.available);
        let reason = capabilities.unavailable_reason.unwrap_or_default();
        assert!(reason.contains("not configured"), "{reason:?}");
    }
}

#[test]
fn worker_exit_is_observable_and_run_root_is_removed() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let (backend, runs) = backend(
        &fixture,
        "#!/bin/sh\nIFS= read -r _\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"native-run-01\"}' >&2\n",
    );
    backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect("worker readiness");
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
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let (backend, runs) = backend(
        &fixture,
        "#!/bin/sh\nIFS= read -r _\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"native-run-01\"}' >&2\nexit 7\n",
    );
    backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect("worker readiness");
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

/// Once completion is observed the run is no longer active, so a cancel
/// after it is a no-op, not a second report of the run's failure.
///
/// The supervisor used to publish completion, then hand its result to a
/// pending cancel, and only then drop the run from its active table. A
/// cleanup arriving in that window took the run as still active, collected
/// the supervisor's result — the worker's own failure — and returned it as
/// the cleanup's error: `cleanup` of a crashed run failed under load
/// (conformance row `a_killed_worker_is_failed_reaped_and_cleaned`, Linux
/// CI, bead `ley-line-open-f8ebcf`). Repeated because the window is a
/// scheduling race; one iteration rarely lands in it.
#[test]
fn a_cancel_after_observed_failure_is_a_no_op() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let (backend, runs) = backend(
        &fixture,
        "#!/bin/sh\nIFS= read -r _\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"native-run-01\"}' >&2\nexit 7\n",
    );
    for _ in 0..200 {
        backend
            .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
            .expect("worker readiness");
        assert!(matches!(
            backend
                .wait_for_completion("native-run-01")
                .expect("worker completion"),
            leyline_runtime::BackendRunStatus::Failed(_)
        ));
        assert!(
            !Backend::cancel(&backend, "native-run-01").expect("a cancel after completion"),
            "a completed run is no longer active"
        );
    }
    assert_eq!(fs::read_dir(runs).expect("runs").count(), 0);
}

#[test]
fn readiness_for_another_run_is_rejected_and_cleaned() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let (backend, runs) = backend(
        &fixture,
        "#!/bin/sh\nIFS= read -r _\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"other-run\"}' >&2\n",
    );
    let error = backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect_err("worker readiness must bind to the requested run");
    assert!(error.detail.contains("unexpected run"));
    assert_eq!(fs::read_dir(runs).expect("runs").count(), 0);
}

/// A workspace grant is Graph authority the native tier cannot deliver. It
/// used to start anyway, without the workspace, while the receipt named the
/// workspace's graph root as an input (found by the conformance matrix, bead
/// `ley-line-open-f8ebcf`). The refusal comes before the worker exists.
#[test]
fn a_granted_workspace_is_refused_before_the_worker_is_spawned() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let spawned = fixture.path().join("spawned");
    let (backend, runs) = backend(
        &fixture,
        &format!(
            "#!/bin/sh\n: > '{}'\nIFS= read -r _\nprintf '%s\\n' '{{\"type\":\"ready\",\"run_id\":\"native-run-01\"}}' >&2\n",
            spawned.display()
        ),
    );
    let mut request = request();
    request.workspaces = vec![leyline_runtime::WorkspaceGrant {
        name: "repo".into(),
        graph_root: format!("blake3-256:{}", "e".repeat(64)),
        expected_generation: 0,
        operations: vec![leyline_runtime::WorkspaceOperation::Read],
        write_scope: Vec::new(),
    }];
    let error = backend
        .start(&request, ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect_err("a workspace grant the native tier cannot serve");
    // Typed, not the generic `unsupportedBackend`: the code alone tells
    // Cloister to route the run to a tier that serves workspaces (bead
    // `ley-line-open-204a94`).
    assert_eq!(
        error.code,
        leyline_runtime::ErrorCode::WorkspaceUnserved,
        "{error:?}"
    );
    assert!(!error.retryable, "{error:?}");
    assert!(error.detail.contains("\"repo\""), "{error:?}");
    assert!(
        !spawned.exists(),
        "the worker was spawned for a refused run"
    );
    assert_eq!(fs::read_dir(runs).expect("runs").count(), 0);
}

#[test]
fn native_cleanup_restores_guest_created_permissions() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let (backend, runs) = backend(
        &fixture,
        "#!/bin/sh\nIFS= read -r _\nmkdir -p \"$4/locked\"\nprintf x > \"$4/locked/file\"\nchmod 000 \"$4/locked\"\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"native-run-01\"}' >&2\n",
    );
    backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect("worker readiness");
    backend
        .wait_for_completion("native-run-01")
        .expect("worker completion");
    assert_eq!(fs::read_dir(runs).expect("runs").count(), 0);
}

/// An environment key or value no environment can carry is refused by name,
/// with the same code on both tiers, before a worker exists (bead
/// `ley-line-open-9d75c9`). The native tier used to format `KEY=VALUE` and
/// let the platform split at the first `=`, so `{"A=B": "1"}` ran with
/// `A="B=1"` and an empty key ran too, while the microVM tier refused both.
#[test]
fn an_environment_no_tier_can_carry_is_refused_by_name_before_spawn() {
    let _serial = common::serial();
    for (key, value, named) in [
        ("A=B", "1", "`=`"),
        ("", "1", "empty"),
        ("K\0EY", "1", "NUL"),
        ("KEY", "v\0", "NUL"),
    ] {
        let fixture = TempDir::new().expect("fixture");
        let spawned = fixture.path().join("spawned");
        let (backend, _runs) = backend(
            &fixture,
            &format!(
                "#!/bin/sh\n: > '{}'\nIFS= read -r _\nprintf '%s\\n' '{{\"type\":\"ready\",\"run_id\":\"native-run-01\"}}' >&2\nexec /usr/bin/tail -f /dev/null\n",
                spawned.display()
            ),
        );
        let mut request = request();
        request
            .public_environment
            .insert(key.to_owned(), value.to_owned());
        let error = backend
            .start(&request, ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
            .expect_err("an environment no tier can carry is refused");
        assert_eq!(
            error.code,
            leyline_runtime::ErrorCode::InvalidSpec,
            "{key:?}"
        );
        assert!(
            error.detail.contains("publicEnvironment") && error.detail.contains(named),
            "{key:?} is refused by name: {}",
            error.detail
        );
        assert!(!spawned.exists(), "{key:?}: no worker was spawned");
    }
}

#[test]
fn cancel_kills_worker_and_removes_run_root() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    // Long-lived fake workers `exec` their sleeper so the worker PID IS the
    // sleeper — without exec, tail is a grandchild the backend's kill never
    // reaches, and it leaks past the test run (bead rs-a1e8d0; the krun
    // twin's rejection paths accumulated 1,869 orphans on a maintainer
    // machine).
    let (backend, runs) = backend(
        &fixture,
        "#!/bin/sh\nIFS= read -r _\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"native-run-01\"}' >&2\nexec /usr/bin/tail -f /dev/null\n",
    );
    backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect("worker readiness");
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

/// A reader of a run that is being cancelled never sees it complete (bead
/// `ley-line-open-9c6a8f`). The supervisor used to publish `Succeeded` for
/// a cancelled worker and the cancel removed it only afterwards, so a read
/// in between collected a success for a run that was cancelled. The reader
/// polls without pause for the whole cancel, so it sees anything published
/// during it; repeated to cover the scheduling.
#[test]
fn a_reader_of_a_cancelled_run_never_sees_it_complete() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let _serial = common::serial();
    for _ in 0..25 {
        let fixture = TempDir::new().expect("fixture");
        let (backend, _runs) = backend(
            &fixture,
            "#!/bin/sh\nIFS= read -r _\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"native-run-01\"}' >&2\nexec /usr/bin/tail -f /dev/null\n",
        );
        backend
            .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
            .expect("worker readiness");
        let done = AtomicBool::new(false);
        let seen = std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                let mut seen = Vec::new();
                while !done.load(Ordering::Acquire) {
                    if let Some(status) = backend.poll("native-run-01").expect("poll") {
                        seen.push(status);
                    }
                }
                seen
            });
            assert!(backend.cancel("native-run-01").expect("cancel"));
            done.store(true, Ordering::Release);
            reader.join().expect("reader")
        });
        assert!(
            seen.is_empty(),
            "a cancelled run was published as complete: {seen:?}"
        );
    }
}

#[test]
fn backend_trait_cancel_delegates_and_drop_waits_for_cleanup() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let (backend, runs) = backend(
        &fixture,
        "#!/bin/sh\nIFS= read -r _\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"native-run-01\"}' >&2\nexec /usr/bin/tail -f /dev/null\n",
    );
    backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect("worker readiness");
    assert!(Backend::cancel(&backend, "native-run-01").expect("trait cancel"));
    assert_eq!(fs::read_dir(&runs).expect("runs").count(), 0);

    backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect("second worker readiness");
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
    let _serial = common::serial();
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
        .start(&request, ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect_err("a worker attesting an unauthorized policy must not start");
    assert!(
        format!("{error:?}").contains("confinement drift"),
        "the refusal must name drift, so an operator learns the worker applied \
         a policy the grant did not authorize: {error:?}"
    );
}

/// The other half of the same condition, and the one that makes it a
/// comparison rather than a blanket refusal: when the grant authorizes no
/// particular policy, whatever the worker attests is not drift.
///
/// Only reachable by embedding this backend directly — the service path always
/// carries a digest, because `read_digest` rejects a `RunGrant.confinementDigest`
/// that is not a lowercase blake3-256 value. It still needs pinning:
/// `replace && with ||` survived at `native_backend.rs:259`, and under `||` an
/// absent authorization would start refusing every worker that attested
/// anything. That is a fail-CLOSED break of direct embedding rather than a
/// security hole — the kind that surfaces as "it worked last release".
#[test]
fn a_grant_authorizing_no_policy_does_not_constrain_what_the_worker_attests() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let (backend, _runs) = backend(
        &fixture,
        r#"#!/bin/sh
echo '{"type":"ready","run_id":"native-run-01","confinement_digest":"blake3-256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"}' >&2
sleep 30
"#,
    );

    // `request()` leaves `confinement_digest` empty: nothing was authorized.
    backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect("an unauthorized-policy grant must not refuse an attesting worker");
    assert!(Backend::cancel(&backend, "native-run-01").expect("cancel"));
}

/// The accept branch — and until v0.15.1 there was no input that could reach it.
///
/// The manifest named the materialized rootfs, whose path is a per-run
/// temporary directory, so the digest changed every run and no grant could ever
/// carry a matching value. Both backends gate on
/// `!request.confinement_digest.is_empty()`, so the only reachable states were
/// "skipped" (empty) and "drift" (non-empty, never equal). The mechanism could
/// reject and could be switched off, and could not accept.
///
/// Every real-worker test passed an empty digest, which takes the skip path, so
/// nothing noticed. This asserts the third state exists: a worker attesting the
/// policy the grant authorized starts, and reaches `Running`.
#[test]
fn a_worker_attesting_the_authorized_policy_starts() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let authorized = format!("blake3-256:{}", "d".repeat(64));
    let (backend, _runs) = backend(
        &fixture,
        &format!(
            r#"#!/bin/sh
echo '{{"type":"ready","run_id":"native-run-01","confinement_digest":"{authorized}"}}' >&2
sleep 30
"#
        ),
    );

    let mut request = request();
    request.confinement_digest = authorized;

    backend
        .start(&request, ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect("a worker attesting exactly the authorized policy must start");
    assert!(Backend::cancel(&backend, "native-run-01").expect("cancel"));
}

/// The refusal a worker reports must survive foreign stderr ahead of it.
///
/// Measured under an outer Seatbelt sandbox (bead `ley-line-open-704853`):
/// macOS libsandbox itself writes `sandbox initialization failed: Operation
/// not permitted` to the worker's stderr, which is the event channel, and only
/// then does the worker emit the `failed` event that names the cause. The
/// backend took the OS line as the readiness event and refused the run as
/// "invalid native worker readiness event: expected value", so an operator
/// could not tell a host that cannot sandbox from a corrupt worker. The run
/// was still refused; the refusal must also name the reason.
#[test]
fn a_worker_refusal_after_foreign_stderr_keeps_its_cause() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let (backend, runs) = backend(
        &fixture,
        r#"#!/bin/sh
IFS= read -r _
echo 'sandbox initialization failed: Operation not permitted' >&2
echo '{"type":"failed","error":{"code":"backend-failed","retryable":false,"detail":"apply native nono confinement: Sandbox initialization failed: Operation not permitted"}}' >&2
exit 1
"#,
    );
    let error = backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect_err("a worker that could not confine must not start");
    assert!(
        error.detail.contains("apply native nono confinement"),
        "the refusal must carry the worker's cause, not the OS line before it: {error:?}"
    );
    assert_eq!(fs::read_dir(runs).expect("runs").count(), 0);
}

// ---------------------------------------------------------------------------
// A worker never outlives its supervisor (bead ley-line-open-62e3b1, the
// native tier's half of ley-line-open-f963e6).

/// A bound on a failure only: every wait below ends on an event first.
const EVENT_BOUND: Duration = Duration::from_secs(30);

/// Whether `pid` names a process (a zombie included).
fn alive(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 checks existence and delivers nothing.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn mkfifo(path: &std::path::Path) {
    let status = std::process::Command::new("/usr/bin/mkfifo")
        .arg(path)
        .status()
        .expect("mkfifo");
    assert!(status.success(), "mkfifo {}", path.display());
}

/// The first line written to the named pipe `path`, as a pid: the event a
/// fake worker raises once it exists. Fails by name past [`EVENT_BOUND`].
fn pid_from(path: &std::path::Path, what: &str) -> libc::pid_t {
    let path = path.to_path_buf();
    let (sender, line) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // Opening a named pipe for reading waits for its writer.
        let _ = sender.send(fs::read_to_string(path));
    });
    let line = line
        .recv_timeout(EVENT_BOUND)
        .unwrap_or_else(|_| panic!("TIMEOUT: {what} did not report its pid within {EVENT_BOUND:?}"))
        .expect("read the worker's pid");
    line.trim().parse().expect("a pid")
}

/// Kills `pid` on drop if it still exists, so a failing test leaves no
/// worker behind.
struct KillOnDrop(libc::pid_t);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if alive(self.0) {
            // SAFETY: a signal to a pid this test spawned (directly or not).
            unsafe { libc::kill(self.0, libc::SIGKILL) };
        }
    }
}

/// A cancel that arrives while the worker exists but has not reported
/// readiness stops it: `cancel` answers `true`, and returns only once the
/// worker is reaped and its run root removed; the start it raced ends
/// `Cancelled`. Before the fix it answered "nothing to cancel" while the
/// worker ran on.
#[test]
fn a_cancel_between_spawn_and_readiness_stops_the_worker_and_removes_its_run_root() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let spawned = fixture.path().join("spawned");
    mkfifo(&spawned);
    // Reads its request, says it exists, and never reports readiness.
    let (backend, ephemeral_root) = backend(
        &fixture,
        &format!(
            "#!/bin/sh\nIFS= read -r _\necho $$ > '{}'\nexec /usr/bin/tail -f /dev/null\n",
            spawned.display()
        ),
    );
    let backend = std::sync::Arc::new(backend);
    let starter = {
        let backend = std::sync::Arc::clone(&backend);
        std::thread::spawn(move || backend.start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none()))
    };

    // EVENT: the worker exists, and is starting.
    let worker = pid_from(&spawned, "the starting worker");
    let _cleanup = KillOnDrop(worker);
    assert!(alive(worker));
    assert_eq!(fs::read_dir(&ephemeral_root).unwrap().count(), 1);

    assert!(
        backend.cancel("native-run-01").expect("cancel"),
        "a starting run was reported as nothing to cancel"
    );
    // `cancel` has returned: the worker is reaped and its run root gone.
    assert!(
        !alive(worker),
        "the starting worker {worker} survived its cancel"
    );
    assert_eq!(
        fs::read_dir(&ephemeral_root).unwrap().count(),
        0,
        "the cancelled start left its run root"
    );
    let error = starter
        .join()
        .expect("the start thread")
        .expect_err("a cancelled start does not start");
    assert_eq!(
        error.code,
        leyline_runtime::ErrorCode::Cancelled,
        "{error:?}"
    );
    assert!(!backend.cancel("native-run-01").expect("second cancel"));
}

/// Set in the embedding process [`native_lifeline_embedder`] runs as: the
/// fixture directory holding its fake worker, CAS and run roots.
const EMBEDDER_ENV: &str = "LEYLINE_TEST_NATIVE_LIFELINE_EMBEDDER";

/// Set in the fake worker [`native_fake_lifeline_worker`] runs as: the
/// fixture directory holding the named pipes it talks to the test through.
const FAKE_WORKER_ENV: &str = "LEYLINE_TEST_NATIVE_FAKE_LIFELINE_WORKER";

/// The process embedding the backend, for
/// `a_native_worker_ends_when_the_process_embedding_its_backend_is_killed`:
/// starts one run, says so on its stdout, and supervises it until it is
/// killed. Not a test on its own.
#[test]
#[ignore = "the embedding process of a lifeline test; run by that test"]
fn native_lifeline_embedder() {
    let Some(fixture) = std::env::var_os(EMBEDDER_ENV) else {
        return;
    };
    let fixture = std::path::PathBuf::from(fixture);
    let backend = NativeWorkerBackend::new(NativeWorkerConfig {
        worker: fixture.join("leyline-native-worker"),
        cas_root: fixture.join("cas"),
        ephemeral_root: fixture.join("runs"),
        runtime_files: Vec::new(),
        ready_timeout: Duration::from_secs(10),
    });
    backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect("ready worker");
    println!("EMBEDDER-STARTED");
    loop {
        std::thread::park();
    }
}

/// A fake native worker that does with its lifeline what the real one
/// does, and reports what it saw: it reads its request from standard input
/// (its lifeline, as `--lifeline-fd 0` says), reports readiness and its pid,
/// waits for the test's "check", reports whether the lifeline is still open
/// at that moment (a poll that does not wait), then reads it to end-of-file
/// and reports that. Not a test on its own.
#[test]
#[ignore = "the fake worker of a lifeline test; run through a script by that test"]
fn native_fake_lifeline_worker() {
    use std::io::{BufRead, Write};
    use std::os::fd::AsRawFd;
    let Some(dir) = std::env::var_os(FAKE_WORKER_ENV) else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let stdin = std::io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line).expect("the request");
    eprintln!("{{\"type\":\"ready\",\"run_id\":\"native-run-01\"}}");
    fs::write(dir.join("spawned"), format!("{}\n", std::process::id())).unwrap();
    // The test's check, after `start` has returned in the embedder.
    let _ = fs::read_to_string(dir.join("check"));
    let mut poll = libc::pollfd {
        fd: stdin.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one live pollfd; a zero timeout does not wait.
    let ready = unsafe { libc::poll(&mut poll, 1, 0) };
    let state = if ready == 0 { "open" } else { "ended" };
    fs::write(dir.join("state"), state).unwrap();
    // Every byte after the request is ignored; end-of-file is the event.
    let mut sink = Vec::new();
    let _ = std::io::Read::read_to_end(&mut stdin.lock(), &mut sink);
    let mut marker = fs::OpenOptions::new()
        .write(true)
        .open(dir.join("marker"))
        .unwrap();
    let _ = marker.write_all(b"lifeline-ended\n");
}

/// `SIGKILL` the process that embeds the backend mid-run: its worker sees
/// its lifeline end only then, and exits.
///
/// The fake worker (this binary, as [`native_fake_lifeline_worker`]) reports
/// whether its lifeline is still open after `start` has returned: a backend
/// that dropped the worker's standard input after the request (the lifeline
/// at fd 0) fails here, deterministically, since it drops it before `start`
/// returns. Its end-of-file arrives only after the kill, which the marker
/// pipe shows; the worker's exit is the kernel's exit event, never
/// `kill(pid, 0)`, which a zombie also answers.
#[test]
fn a_native_worker_ends_when_the_process_embedding_its_backend_is_killed() {
    use std::io::{BufRead, Write};
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    for name in ["spawned", "check", "state", "marker"] {
        mkfifo(&fixture.path().join(name));
    }
    publish_executable(
        &fixture.path().join("leyline-native-worker"),
        &format!(
            "#!/bin/sh\nexec '{}' --exact native_fake_lifeline_worker --ignored --nocapture --test-threads=1\n",
            std::env::current_exe().unwrap().display()
        ),
    );
    fs::create_dir(fixture.path().join("cas")).expect("CAS");
    fs::create_dir(fixture.path().join("runs")).expect("ephemeral root");

    let mut embedder = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "native_lifeline_embedder",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(EMBEDDER_ENV, fixture.path())
        .env(FAKE_WORKER_ENV, fixture.path())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the embedding process");
    let _embedder_cleanup = KillOnDrop(embedder.id() as libc::pid_t);
    let worker = pid_from(&fixture.path().join("spawned"), "the embedder's worker");
    let _worker_cleanup = KillOnDrop(worker);

    // EVENT: `start` has returned in the embedder.
    let started = {
        let stdout = embedder.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if line.contains("EMBEDDER-STARTED") {
                    let _ = tx.send(());
                    return;
                }
            }
        });
        rx.recv_timeout(EVENT_BOUND)
    };
    started.expect("TIMEOUT: the embedder's start never returned");

    // The lifeline is open after `start` returned.
    let (state_tx, state) = std::sync::mpsc::channel();
    let state_path = fixture.path().join("state");
    std::thread::spawn(move || {
        let _ = state_tx.send(fs::read_to_string(state_path));
    });
    fs::OpenOptions::new()
        .write(true)
        .open(fixture.path().join("check"))
        .unwrap()
        .write_all(b"check\n")
        .unwrap();
    let state = state
        .recv_timeout(EVENT_BOUND)
        .expect("TIMEOUT: the fake worker never reported its lifeline")
        .unwrap();
    assert_eq!(
        state, "open",
        "the worker's lifeline had already ended while its supervisor lived"
    );

    // Its end arrives with the kill, and not before.
    let (marker_tx, marker) = std::sync::mpsc::channel();
    let marker_path = fixture.path().join("marker");
    std::thread::spawn(move || {
        let _ = marker_tx.send(fs::read_to_string(marker_path));
    });
    assert!(
        marker.try_recv().is_err(),
        "the lifeline ended before the kill"
    );
    // SAFETY: a signal to the child this test spawned.
    assert_eq!(
        unsafe { libc::kill(embedder.id() as libc::pid_t, libc::SIGKILL) },
        0
    );
    embedder.wait().expect("reap the embedder");
    let ended = marker
        .recv_timeout(EVENT_BOUND)
        .expect("TIMEOUT: the worker never saw its lifeline end")
        .unwrap();
    assert_eq!(ended, "lifeline-ended\n");
    common::await_exit(worker, EVENT_BOUND).unwrap_or_else(|error| {
        panic!("the native worker outlived the process embedding its backend: {error}")
    });
}

/// The real worker refuses a lifeline that cannot be one, by name, before it
/// reads a rootfs or applies a sandbox (the CAS root here does not exist,
/// so any later failure would name it instead); and refuses to start
/// without one.
#[test]
fn the_native_worker_refuses_a_missing_or_unusable_lifeline() {
    use std::io::Write;
    let _serial = common::serial();
    let run_root = TempDir::new().expect("run root");
    let failure = |extra: &[&str]| {
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_leyline-native-worker"))
            .args(["--cas-root", "/nonexistent-cas", "--run-root"])
            .arg(run_root.path())
            .args(extra)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn the native worker");
        let mut stdin = child.stdin.take().expect("stdin");
        serde_json::to_writer(&mut stdin, &request()).expect("request");
        stdin.write_all(b"\n").expect("request");
        drop(stdin);
        let output = child.wait_with_output().expect("worker output");
        assert!(!output.status.success());
        let event: leyline_runtime::backends::native::WorkerEvent =
            serde_json::from_slice(&output.stderr).expect("one structured event");
        match event {
            leyline_runtime::backends::native::WorkerEvent::Failed { error } => error,
            other => panic!("expected a refusal, got {other:?}"),
        }
    };
    for extra in [&[][..], &["--lifeline-fd", "1048576"]] {
        let error = failure(extra);
        assert_eq!(
            error.code,
            leyline_runtime::ErrorCode::InvalidSpec,
            "{error:?}"
        );
        assert!(
            error.detail.contains("--lifeline-fd"),
            "{extra:?}: {error:?}"
        );
    }
}

/// What [`a_native_cancel_right_after_spawn_finds_the_run`] observes, in order.
#[derive(Debug)]
enum Seen {
    CancelWaiting,
    CancelReturned(Result<bool, leyline_runtime::ExecutionError>),
}

/// A cancel that arrives right after the spawn, before `start` does anything
/// else, finds the run: it is registered before the spawn (the native half
/// of the #594 review's COMMENT 1).
#[test]
fn a_native_cancel_right_after_spawn_finds_the_run() {
    use leyline_runtime::backends::libkrun::backend::StartPoint;
    use std::sync::Mutex as StdMutex;
    use std::sync::mpsc;
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let (spawned_tx, spawned) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let (seen_tx, seen) = mpsc::channel();
    let released = StdMutex::new(released);
    let hook_seen = seen_tx.clone();
    let (backend, ephemeral_root) = backend(
        &fixture,
        "#!/bin/sh\nIFS= read -r _\nexec /usr/bin/tail -f /dev/null\n",
    );
    let backend = std::sync::Arc::new(backend.with_start_hook(move |point| match point {
        StartPoint::Spawned => {
            let _ = spawned_tx.send(());
            let _ = released.lock().unwrap().recv();
        }
        StartPoint::CancelSignalled => {
            let _ = hook_seen.send(Seen::CancelWaiting);
        }
        _ => {}
    }));
    let starter = {
        let backend = std::sync::Arc::clone(&backend);
        std::thread::spawn(move || backend.start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none()))
    };
    spawned
        .recv_timeout(EVENT_BOUND)
        .expect("TIMEOUT: the worker was never spawned");
    let canceller = {
        let backend = std::sync::Arc::clone(&backend);
        std::thread::spawn(move || {
            let _ = seen_tx.send(Seen::CancelReturned(backend.cancel("native-run-01")));
        })
    };
    let first = seen
        .recv_timeout(EVENT_BOUND)
        .expect("TIMEOUT: the cancel neither waited nor returned");
    let _ = release.send(());
    assert!(
        matches!(first, Seen::CancelWaiting),
        "a cancel right after the spawn did not find the run: {first:?}"
    );
    match seen.recv_timeout(EVENT_BOUND) {
        Ok(Seen::CancelReturned(Ok(true))) => {}
        other => panic!("the cancel's answer: {other:?}"),
    }
    canceller.join().unwrap();
    let error = starter.join().unwrap().expect_err("a cancelled start");
    assert_eq!(
        error.code,
        leyline_runtime::ErrorCode::Cancelled,
        "{error:?}"
    );
    assert_eq!(fs::read_dir(&ephemeral_root).unwrap().count(), 0);
}

/// A cancel before readiness whose run root cannot be removed says so (the
/// native half of the #594 review's COMMENT 2).
#[test]
fn a_native_cancel_before_readiness_reports_a_run_root_it_could_not_remove() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let spawned = fixture.path().join("spawned");
    mkfifo(&spawned);
    let (backend, ephemeral_root) = backend(
        &fixture,
        &format!(
            "#!/bin/sh\nIFS= read -r _\necho $$ > '{}'\nexec /usr/bin/tail -f /dev/null\n",
            spawned.display()
        ),
    );
    let backend = std::sync::Arc::new(backend);
    let starter = {
        let backend = std::sync::Arc::clone(&backend);
        std::thread::spawn(move || backend.start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none()))
    };
    let worker = pid_from(&spawned, "the starting worker");
    let _cleanup = KillOnDrop(worker);
    fs::set_permissions(&ephemeral_root, fs::Permissions::from_mode(0o555)).unwrap();
    let answer = backend.cancel("native-run-01");
    fs::set_permissions(&ephemeral_root, fs::Permissions::from_mode(0o755)).unwrap();
    let error = answer.expect_err("a cancel whose cleanup failed reports it");
    assert!(error.detail.contains("ephemeral"), "{error:?}");
    assert!(!alive(worker), "the worker {worker} survived its cancel");
    assert!(starter.join().unwrap().is_err());
}

/// Cancels queued at a hook point while `start` ends on its own failure: a
/// `test-support` hook issues the cancel from another thread at `queue_at`,
/// and returns once that cancel is waiting. Returns the cancel's answer and
/// the start's result.
fn cancel_queued_at(
    backend: NativeWorkerBackend,
    queue_at: leyline_runtime::backends::libkrun::backend::StartPoint,
    also_at_spawn: impl Fn() + Send + Sync + 'static,
) -> (
    Result<bool, leyline_runtime::ExecutionError>,
    Result<leyline_runtime::BackendRun, leyline_runtime::ExecutionError>,
) {
    use leyline_runtime::backends::libkrun::backend::StartPoint;
    use std::sync::Mutex as StdMutex;
    use std::sync::mpsc;
    let backend_slot: std::sync::Arc<StdMutex<Option<std::sync::Arc<NativeWorkerBackend>>>> =
        std::sync::Arc::new(StdMutex::new(None));
    let (waiting_tx, waiting) = mpsc::channel();
    let waiting = StdMutex::new(waiting);
    let (answer_tx, answer) = mpsc::channel();
    let slot = std::sync::Arc::clone(&backend_slot);
    let backend = std::sync::Arc::new(backend.with_start_hook(move |point| {
        if point == StartPoint::Spawned {
            also_at_spawn();
        }
        if point == StartPoint::CancelSignalled {
            let _ = waiting_tx.send(());
        }
        if point == queue_at {
            let backend = slot.lock().unwrap().clone().expect("the backend");
            let answer_tx = answer_tx.clone();
            std::thread::spawn(move || {
                let _ = answer_tx.send(backend.cancel("native-run-01"));
            });
            waiting
                .lock()
                .unwrap()
                .recv_timeout(EVENT_BOUND)
                .expect("TIMEOUT: the cancel never queued");
        }
    }));
    *backend_slot.lock().unwrap() = Some(std::sync::Arc::clone(&backend));
    let started = backend.start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none());
    let answered = answer
        .recv_timeout(EVENT_BOUND)
        .expect("TIMEOUT: the queued cancel was never answered");
    *backend_slot.lock().unwrap() = None;
    (answered, started)
}

/// A cancel queued after the run is registered and before its spawn, which
/// then fails, answers that there was nothing to cancel: no worker was ever
/// started.
#[test]
fn a_cancel_queued_before_a_failed_spawn_answers_false() {
    use leyline_runtime::backends::libkrun::backend::StartPoint;
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let (backend, ephemeral_root) = backend(&fixture, "#!/bin/sh\nexit 0\n");
    // A worker that cannot be executed: the spawn fails.
    fs::set_permissions(
        fixture.path().join("leyline-native-worker"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let (answered, started) = cancel_queued_at(backend, StartPoint::Registered, || {});
    assert!(started.is_err(), "{started:?}");
    assert!(
        matches!(answered, Ok(false)),
        "a cancel with no worker ever spawned: {answered:?}"
    );
    assert_eq!(fs::read_dir(&ephemeral_root).unwrap().count(), 0);
}

/// A cancel queued while a start fails on its own (here a readiness line
/// that is not an event), with a run root that cannot be removed, answers
/// with an error naming that run root, not `Ok(true)`.
#[test]
fn a_cancel_queued_while_a_start_fails_reports_its_unremovable_run_root() {
    use leyline_runtime::backends::libkrun::backend::StartPoint;
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let (backend, ephemeral_root) = backend(
        &fixture,
        "#!/bin/sh\nIFS= read -r _\nprintf '{not an event\\n' >&2\nexit 0\n",
    );
    let root = ephemeral_root.clone();
    let (answered, started) = cancel_queued_at(backend, StartPoint::Ending, move || {
        // The run root exists now; its parent stops letting it go.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o555)).unwrap();
    });
    fs::set_permissions(&ephemeral_root, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(started.is_err(), "{started:?}");
    let error = answered.expect_err("the run root remains, and the answer says so");
    assert!(error.detail.contains("run root"), "{error:?}");
    let remaining = fs::read_dir(&ephemeral_root)
        .unwrap()
        .next()
        .expect("the run root")
        .unwrap();
    assert!(
        error
            .detail
            .contains(&remaining.path().display().to_string()),
        "{error:?}"
    );
}

/// `--lifeline-fd 0` names standard input, and a lifeline must be a pipe or
/// a socket: with a regular file as standard input (which would read
/// end-of-file at once), the real worker refuses it by name once the
/// request is read, before any rootfs or sandbox.
#[test]
fn the_native_worker_refuses_a_regular_file_as_its_stdin_lifeline() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let request_file = fixture.path().join("request.json");
    fs::write(
        &request_file,
        format!("{}\n", serde_json::to_string(&request()).unwrap()),
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_leyline-native-worker"))
        .args(["--cas-root", "/nonexistent-cas", "--run-root"])
        .arg(fixture.path())
        .args(["--lifeline-fd", "0"])
        .stdin(fs::File::open(&request_file).unwrap())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("run the native worker");
    assert!(!output.status.success());
    let event: leyline_runtime::backends::native::WorkerEvent =
        serde_json::from_slice(&output.stderr).expect("one structured event");
    let leyline_runtime::backends::native::WorkerEvent::Failed { error } = event else {
        panic!("expected a refusal, got {event:?}");
    };
    assert!(error.detail.contains("--lifeline-fd 0"), "{error:?}");
    assert!(error.detail.contains("pipe or socket"), "{error:?}");
}
