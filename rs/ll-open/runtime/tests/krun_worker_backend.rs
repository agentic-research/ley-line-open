use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Barrier};
use std::time::Duration;

use leyline_runtime::backends::libkrun::backend::{KrunWorkerBackend, KrunWorkerConfig};
use leyline_runtime::backends::libkrun::output::GUEST_OUTPUT_TAIL_BYTES;
use leyline_runtime::backends::libkrun::share::{FsService, Reply, Request, WorkspaceShares};
use leyline_runtime::{
    AttachedWorkspace, Backend, BackendClass, BackendRunStatus, CpuLimits, DigestRef, ErrorCode,
    ExecutionError, ExecutionRequest, GuestOutput, MemoryLimits, ResourceLimits, ServedWorkspaces,
    WorkerChannel, WorkspaceGrant, WorkspaceOperation,
};
use tempfile::TempDir;

mod common;
// Signs a worker the way the build, install, and release paths do.
#[allow(dead_code)]
#[path = "../../krun-worker/tests/hypervisor/mod.rs"]
mod hypervisor;

/// A backend over a FAKE worker: a shell script that speaks the worker
/// protocol and never enters a hypervisor. A script cannot carry the
/// hypervisor entitlement, so these supervision-contract tests vouch for it;
/// `a_worker_without_the_hypervisor_entitlement_is_unavailable` below is
/// the test of the check they skip.
fn fake_worker_backend(config: KrunWorkerConfig) -> KrunWorkerBackend {
    KrunWorkerBackend::new(config).with_unverified_hypervisor_access()
}

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
        confinement_digest: String::new(),
        confinement_manifest: None,
        workspaces: Vec::new(),
        limits: ResourceLimits {
            wall_time_ms: 10_000,
            memory: MemoryLimits { limit: 512 << 20 },
            cpu: CpuLimits {
                quota: 100_000,
                period: 100_000,
            },
            ..ResourceLimits::default()
        },
    }
}

fn backend_with_worker(
    fixture: &TempDir,
    worker_body: &str,
) -> (KrunWorkerBackend, std::path::PathBuf) {
    let worker = fixture.path().join("leyline-krun-worker");
    fs::write(&worker, worker_body).expect("fake worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).expect("worker mode");
    let cas_root = fixture.path().join("cas");
    let ephemeral_root = fixture.path().join("runs");
    fs::create_dir(&cas_root).expect("CAS");
    fs::create_dir(&ephemeral_root).expect("ephemeral root");
    let backend = fake_worker_backend(KrunWorkerConfig {
        worker,
        cas_root,
        ephemeral_root: ephemeral_root.clone(),
        runtime_files: Vec::new(),
        devices: Vec::new(),
        ready_timeout: READY_TIMEOUT,
        tsi_hijack_inet: false,
    });
    (backend, ephemeral_root)
}

#[test]
fn backend_spawns_the_explicit_first_party_worker_and_waits_for_ready() {
    let _serial = common::serial();
    // Catches reporting a run as started before the confined worker has
    // prepared libkrun, and catches PATH-based backend selection.
    let fixture = TempDir::new().expect("fixture");
    let request_log = fixture.path().join("request.json");
    let worker = fixture.path().join("leyline-krun-worker");
    // Every fake-worker script here `exec`s its final sleeper so the worker
    // PID IS the sleeper. Without exec, tail is a grandchild under sh: the
    // backend's kill reaches only sh, and on the rejection paths (readiness
    // for a foreign run, unauthorized policy) the orphaned tail reparents to
    // launchd and lives forever — 1,869 of them accumulated over 18 days of
    // CI on a maintainer machine (bead rs-a1e8d0).
    fs::write(
        &worker,
        format!(
            "#!/bin/sh\n/bin/cat > '{}'\nprintf '%s\\n' '{{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}}' >&2\nexec /usr/bin/tail -f /dev/null\n",
            request_log.display()
        ),
    )
    .expect("fake worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).expect("worker mode");
    let cas = fixture.path().join("cas");
    fs::create_dir(&cas).expect("CAS");
    let ephemeral_root = fixture.path().join("runs");
    fs::create_dir(&ephemeral_root).expect("ephemeral root");
    let backend = fake_worker_backend(KrunWorkerConfig {
        worker,
        cas_root: cas,
        ephemeral_root: ephemeral_root.clone(),
        runtime_files: Vec::new(),
        devices: Vec::new(),
        ready_timeout: READY_TIMEOUT,
        tsi_hijack_inet: false,
    });

    let capabilities = backend.capabilities();
    assert!(capabilities.available);
    assert_eq!(capabilities.backend_class, BackendClass::MicroVm);
    let started = backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect("ready worker");

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
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let worker = fixture.path().join("leyline-krun-worker");
    fs::write(
        &worker,
        "#!/bin/sh\nrun_root=\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = \"--run-root\" ]; then run_root=\"$2\"; shift 2; else shift; fi\ndone\nIFS= read -r _\ncontrol=\"$run_root/control\"\n/usr/bin/mkfifo \"$control\"\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}' >&2\nIFS= read -r _ < \"$control\"\n",
    )
    .expect("fake worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).expect("worker mode");
    let cas_root = fixture.path().join("cas");
    let ephemeral_root = fixture.path().join("runs");
    fs::create_dir(&cas_root).expect("CAS");
    fs::create_dir(&ephemeral_root).expect("ephemeral root");
    let backend = fake_worker_backend(KrunWorkerConfig {
        worker,
        cas_root,
        ephemeral_root: ephemeral_root.clone(),
        runtime_files: Vec::new(),
        devices: Vec::new(),
        ready_timeout: READY_TIMEOUT,
        tsi_hijack_inet: false,
    });

    backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect("ready worker");
    assert_eq!(run_root_count(&ephemeral_root), 1);
    let run_root = fs::read_dir(&ephemeral_root)
        .expect("run roots")
        .next()
        .expect("one run root")
        .expect("run root entry")
        .path();
    fs::write(run_root.join("control"), b"exit\n").expect("release worker");
    let completion = backend
        .wait_for_completion("run-backend-01")
        .expect("worker completion");
    assert!(matches!(completion, BackendRunStatus::Succeeded));

    assert_eq!(run_root_count(&ephemeral_root), 0);
}

#[test]
fn backend_reports_a_nonzero_worker_exit_as_failed() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let (backend, ephemeral_root) = backend_with_worker(
        &fixture,
        "#!/bin/sh\nIFS= read -r _\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}' >&2\nexit 7\n",
    );

    backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect("ready worker");
    let completion = backend
        .wait_for_completion("run-backend-01")
        .expect("worker completion");
    match completion {
        BackendRunStatus::Failed(error) => {
            assert!(error.detail.contains("libkrun worker exited"));
        }
        other => panic!("expected failed worker, got {other:?}"),
    }
    assert_eq!(run_root_count(&ephemeral_root), 0);
}

/// A worker that fails after readiness has its reported cause and what the
/// guest printed in the run's failure (bead ley-line-open-a26ad0): the
/// terminal `failed` event as unverified text, the guest's standard output
/// tail, and the rest of the standard error, with the guest's control bytes
/// replaced. The code and `retryable` stay the supervisor's.
#[test]
fn a_failed_run_reports_its_terminal_event_and_what_the_guest_printed() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let (backend, _ephemeral_root) = backend_with_worker(
        &fixture,
        concat!(
            "#!/bin/sh\n",
            "IFS= read -r _\n",
            "printf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}' >&2\n",
            "printf 'guest says hello\\n\\033[31mred\\n'\n",
            "printf 'libkrun: something went wrong\\n' >&2\n",
            "printf '%s\\n' '{\"type\":\"failed\",\"error\":{\"code\":\"resource-exhausted\",\"retryable\":true,\"detail\":\"krun_start_enter failed: -12\"}}' >&2\n",
            "exit 1\n",
        ),
    );

    backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect("ready worker");
    let outcome = backend
        .wait_for_outcome("run-backend-01")
        .expect("worker completion");
    let output = outcome.output.clone().expect("typed guest output");
    assert!(output.untrusted);
    assert_eq!(output.stdout_tail, "guest says hello\n\u{FFFD}[31mred\n");
    assert!(
        output
            .stderr_tail
            .contains("libkrun: something went wrong\n"),
        "{output:?}"
    );
    assert_eq!(
        output.reported_failure.map(|reported| reported.code),
        Some(ErrorCode::ResourceExhausted)
    );
    let BackendRunStatus::Failed(error) = outcome.status else {
        panic!("expected a failed run, got {outcome:?}");
    };
    // The guest writes the worker's standard error too, so the reported
    // event is text, and the classification stays the supervisor's.
    assert_eq!(
        (error.code, error.retryable),
        (ErrorCode::BackendFailed, false),
        "{error:?}"
    );
    assert!(
        error.detail.starts_with("libkrun worker exited"),
        "the supervisor's observation leads the detail: {}",
        error.detail
    );
    assert!(
        error.detail.contains(
            "(unverified): ResourceExhausted, retryable true: krun_start_enter failed: -12"
        ),
        "the reported event is named: {}",
        error.detail
    );
    assert!(
        error.detail.contains("guest says hello\n"),
        "{}",
        error.detail
    );
    assert!(
        error.detail.contains("\u{FFFD}[31mred"),
        "the guest's escape byte is replaced: {}",
        error.detail
    );
    assert!(!error.detail.contains('\u{1b}'), "{}", error.detail);
    assert!(
        error.detail.contains("libkrun: something went wrong"),
        "{}",
        error.detail
    );
}

/// A reported event's detail is decoded JSON, where `\u001b` becomes a raw
/// escape byte. It is sanitized like every other guest byte, both in the
/// failure and in the active run's `output`.
#[test]
fn a_reported_event_cannot_carry_control_bytes() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let root = fixture.path().join("gate");
    let (backend, ephemeral_root) = backend_with_worker(
        &fixture,
        &format!(
            concat!(
                "#!/bin/sh\n",
                "IFS= read -r _\n",
                "printf '%s\\n' '{{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}}' >&2\n",
                "printf '%s\\n' '{{\"type\":\"failed\",\"error\":{{\"code\":\"backend-failed\",\"retryable\":false,\"detail\":\"\\u001b[31mx\"}}}}' >&2\n",
                "printf 'reported\\n'\n",
                "/usr/bin/mkfifo '{gate}'\n",
                "IFS= read -r _ < '{gate}'\n",
                "exit 1\n",
            ),
            gate = root.display()
        ),
    );
    let _ = ephemeral_root;

    backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect("ready worker");
    let seen = backend
        .wait_for_output(
            "run-backend-01",
            &|output: &GuestOutput| {
                output.reported_failure.is_some() && !output.stdout_tail.is_empty()
            },
            READY_TIMEOUT,
        )
        .expect("an active run");
    let reported = seen.reported_failure.expect("the reported event");
    assert_eq!(reported.detail, "\u{FFFD}[31mx");
    fs::write(&root, b"go\n").expect("release the worker");
    let BackendRunStatus::Failed(error) = backend
        .wait_for_completion("run-backend-01")
        .expect("worker completion")
    else {
        panic!("expected a failed run");
    };
    assert!(!error.detail.contains('\u{1b}'), "{}", error.detail);
    assert!(error.detail.contains("\u{FFFD}[31mx"), "{}", error.detail);
}

/// A guest that prints more than the tail holds costs the tail, not what it
/// printed: the failure keeps the last GUEST_OUTPUT_TAIL_BYTES and says how
/// many earlier bytes it dropped.
#[test]
fn a_failed_run_keeps_only_the_tail_of_a_large_output() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let (backend, _ephemeral_root) = backend_with_worker(
        &fixture,
        concat!(
            "#!/bin/sh\n",
            "IFS= read -r _\n",
            "printf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}' >&2\n",
            "/usr/bin/head -c 100000 /dev/zero | /usr/bin/tr '\\000' 'a'\n",
            "printf 'THE-END'\n",
            "exit 3\n",
        ),
    );

    backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect("ready worker");
    let BackendRunStatus::Failed(error) = backend
        .wait_for_completion("run-backend-01")
        .expect("worker completion")
    else {
        panic!("expected a failed run");
    };
    let dropped = 100_000 + "THE-END".len() - GUEST_OUTPUT_TAIL_BYTES;
    assert!(
        error
            .detail
            .contains(&format!("tail; {dropped} earlier bytes dropped")),
        "{}",
        &error.detail[..error.detail.len().min(400)]
    );
    assert!(
        error.detail.ends_with("THE-END"),
        "the tail is the last bytes"
    );
    assert!(error.detail.len() < GUEST_OUTPUT_TAIL_BYTES + 1024);
}

/// While a run is active, what it has printed is readable by its embedder,
/// event-driven: `wait_for_output` returns once the console holds the
/// marker, and the trait's `output` returns the same tail. A cancelled run
/// keeps nothing.
#[test]
fn an_active_run_exposes_its_output_to_the_embedder() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    // The worker announces only after the wait has begun: the condition's
    // first evaluation opens the gate, so the announcement must be waited
    // for, not found already there.
    let gate = fixture.path().join("gate");
    let made = std::process::Command::new("/usr/bin/mkfifo")
        .arg(&gate)
        .status()
        .expect("mkfifo");
    assert!(made.success());
    let (backend, _ephemeral_root) = backend_with_worker(
        &fixture,
        &format!(
            concat!(
                "#!/bin/sh\n",
                "IFS= read -r _\n",
                "printf '%s\\n' '{{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}}' >&2\n",
                "IFS= read -r _ < '{gate}'\n",
                "printf 'server listening on 3306\\n'\n",
                "exec /usr/bin/tail -f /dev/null\n",
            ),
            gate = gate.display()
        ),
    );

    backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect("ready worker");
    let opened = std::cell::Cell::new(false);
    let seen = backend
        .wait_for_output(
            "run-backend-01",
            &|output: &GuestOutput| {
                if !opened.replace(true) {
                    fs::write(&gate, b"go\n").expect("open the gate");
                }
                output.stdout_tail.contains("listening")
            },
            READY_TIMEOUT,
        )
        .expect("an active run");
    assert_eq!(seen.stdout_tail, "server listening on 3306\n");
    assert_eq!(
        Backend::output(&backend, "run-backend-01").map(|output| output.stdout_tail),
        Some("server listening on 3306\n".to_owned())
    );
    assert_eq!(Backend::output(&backend, "no-such-run"), None);
    assert!(backend.cancel("run-backend-01").expect("cancel"));
    assert_eq!(Backend::output(&backend, "run-backend-01"), None);
    assert!(
        backend
            .wait_for_output("run-backend-01", &|_| true, READY_TIMEOUT)
            .is_err()
    );
}

/// Once completion is observed the run is no longer active, so a cancel
/// after it is a no-op rather than a second report of the worker's failure
/// (the native twin, `a_cancel_after_observed_failure_is_a_no_op`, explains
/// the race; bead `ley-line-open-f8ebcf`).
#[test]
fn a_cancel_after_observed_failure_is_a_no_op() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let (backend, ephemeral_root) = backend_with_worker(
        &fixture,
        "#!/bin/sh\nIFS= read -r _\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}' >&2\nexit 7\n",
    );
    for _ in 0..200 {
        backend
            .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
            .expect("ready worker");
        assert!(matches!(
            backend
                .wait_for_completion("run-backend-01")
                .expect("worker completion"),
            BackendRunStatus::Failed(_)
        ));
        assert!(
            !Backend::cancel(&backend, "run-backend-01").expect("a cancel after completion"),
            "a completed run is no longer active"
        );
    }
    assert_eq!(run_root_count(&ephemeral_root), 0);
}

/// The microVM twin of the native test: the same keys and values are refused
/// with the same code and the same wording, before a worker exists (bead
/// `ley-line-open-9d75c9`).
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
        let (backend, _runs) = backend_with_worker(
            &fixture,
            &format!(
                "#!/bin/sh\n: > '{}'\nIFS= read -r _\nprintf '%s\\n' '{{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}}' >&2\nexec /usr/bin/tail -f /dev/null\n",
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
        assert_eq!(error.code, ErrorCode::InvalidSpec, "{key:?}");
        assert!(
            error.detail.contains("publicEnvironment") && error.detail.contains(named),
            "{key:?} is refused by name: {}",
            error.detail
        );
        assert!(!spawned.exists(), "{key:?}: no worker was spawned");
    }
}

#[test]
fn backend_cancel_terminates_the_worker_and_removes_its_run_root() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let worker = fixture.path().join("leyline-krun-worker");
    fs::write(
        &worker,
        "#!/bin/sh\nIFS= read -r _\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}' >&2\nexec /usr/bin/tail -f /dev/null\n",
    )
    .expect("fake worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).expect("worker mode");
    let cas_root = fixture.path().join("cas");
    let ephemeral_root = fixture.path().join("runs");
    fs::create_dir(&cas_root).expect("CAS");
    fs::create_dir(&ephemeral_root).expect("ephemeral root");
    let backend = fake_worker_backend(KrunWorkerConfig {
        worker,
        cas_root,
        ephemeral_root: ephemeral_root.clone(),
        runtime_files: Vec::new(),
        devices: Vec::new(),
        ready_timeout: READY_TIMEOUT,
        tsi_hijack_inet: false,
    });

    backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect("ready worker");
    assert_eq!(run_root_count(&ephemeral_root), 1);
    assert!(Backend::cancel(&backend, "run-backend-01").expect("cancel worker"));

    assert_eq!(run_root_count(&ephemeral_root), 0);
    assert!(!Backend::cancel(&backend, "run-backend-01").expect("repeat cancel"));
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
        let (backend, _runs) = backend_with_worker(
            &fixture,
            "#!/bin/sh\nIFS= read -r _\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}' >&2\nexec /usr/bin/tail -f /dev/null\n",
        );
        backend
            .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
            .expect("ready worker");
        let done = AtomicBool::new(false);
        let seen = std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                let mut seen = Vec::new();
                while !done.load(Ordering::Acquire) {
                    if let Some(status) = Backend::poll(&backend, "run-backend-01").expect("poll") {
                        seen.push(status);
                    }
                }
                seen
            });
            assert!(Backend::cancel(&backend, "run-backend-01").expect("cancel"));
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
fn backend_enforces_the_wall_clock_limit_and_cleans_up() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let worker = fixture.path().join("leyline-krun-worker");
    fs::write(
        &worker,
        "#!/bin/sh\nIFS= read -r _\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}' >&2\nexec /usr/bin/tail -f /dev/null\n",
    )
    .expect("fake worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).expect("worker mode");
    let cas_root = fixture.path().join("cas");
    let ephemeral_root = fixture.path().join("runs");
    fs::create_dir(&cas_root).expect("CAS");
    fs::create_dir(&ephemeral_root).expect("ephemeral root");
    let backend = fake_worker_backend(KrunWorkerConfig {
        worker,
        cas_root,
        ephemeral_root: ephemeral_root.clone(),
        runtime_files: Vec::new(),
        devices: Vec::new(),
        ready_timeout: READY_TIMEOUT,
        tsi_hijack_inet: false,
    });
    let mut request = request();
    request.limits.wall_time_ms = 50;

    backend
        .start(&request, ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect("ready worker");
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
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let worker = fixture.path().join("leyline-krun-worker");
    fs::write(
        &worker,
        "#!/bin/sh\nIFS= read -r _\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}' >&2\nexec /usr/bin/tail -f /dev/null\n",
    )
    .expect("fake worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).expect("worker mode");
    let cas_root = fixture.path().join("cas");
    let ephemeral_root = fixture.path().join("runs");
    fs::create_dir(&cas_root).expect("CAS");
    fs::create_dir(&ephemeral_root).expect("ephemeral root");
    let backend = fake_worker_backend(KrunWorkerConfig {
        worker,
        cas_root,
        ephemeral_root: ephemeral_root.clone(),
        runtime_files: Vec::new(),
        devices: Vec::new(),
        ready_timeout: READY_TIMEOUT,
        tsi_hijack_inet: false,
    });

    backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect("ready worker");
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
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let worker = fixture.path().join("leyline-krun-worker");
    fs::write(
        &worker,
        r#"#!/bin/sh
run_root=
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--run-root" ]; then run_root="$2"; shift 2; else shift; fi
done
IFS= read -r _
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
    let backend = fake_worker_backend(KrunWorkerConfig {
        worker,
        cas_root,
        ephemeral_root: ephemeral_root.clone(),
        runtime_files: Vec::new(),
        devices: Vec::new(),
        ready_timeout: READY_TIMEOUT,
        tsi_hijack_inet: false,
    });

    let error = backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect_err("worker setup failure");

    assert!(error.detail.contains("injected setup failure"));
    assert_eq!(run_root_count(&ephemeral_root), 0);
}

#[test]
fn backend_rejects_a_duplicate_run_id_without_replacing_the_live_worker() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let worker = fixture.path().join("leyline-krun-worker");
    fs::write(
        &worker,
        "#!/bin/sh\nIFS= read -r _\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}' >&2\nexec /usr/bin/tail -f /dev/null\n",
    )
    .expect("fake worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).expect("worker mode");
    let cas_root = fixture.path().join("cas");
    let ephemeral_root = fixture.path().join("runs");
    fs::create_dir(&cas_root).expect("CAS");
    fs::create_dir(&ephemeral_root).expect("ephemeral root");
    let backend = fake_worker_backend(KrunWorkerConfig {
        worker,
        cas_root,
        ephemeral_root: ephemeral_root.clone(),
        runtime_files: Vec::new(),
        devices: Vec::new(),
        ready_timeout: READY_TIMEOUT,
        tsi_hijack_inet: false,
    });

    backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect("first worker");
    let error = backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect_err("duplicate run ID");

    assert!(error.detail.contains("run_id already active"));
    assert_eq!(run_root_count(&ephemeral_root), 1);
    assert!(backend.cancel("run-backend-01").expect("cancel worker"));
}

#[test]
fn concurrent_starts_reserve_a_run_id_before_spawning() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let worker = fixture.path().join("leyline-krun-worker");
    fs::write(
        &worker,
        "#!/bin/sh\nIFS= read -r _\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}' >&2\nexec /usr/bin/tail -f /dev/null\n",
    )
    .expect("fake worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).expect("worker mode");
    let cas_root = fixture.path().join("cas");
    let ephemeral_root = fixture.path().join("runs");
    fs::create_dir(&cas_root).expect("CAS");
    fs::create_dir(&ephemeral_root).expect("ephemeral root");
    let backend = Arc::new(fake_worker_backend(KrunWorkerConfig {
        worker,
        cas_root,
        ephemeral_root,
        runtime_files: Vec::new(),
        devices: Vec::new(),
        ready_timeout: READY_TIMEOUT,
        tsi_hijack_inet: false,
    }));
    let barrier = Arc::new(Barrier::new(3));
    let starts: Vec<_> = (0..2)
        .map(|_| {
            let backend = Arc::clone(&backend);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                backend.start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
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

/// The operator's socket-hijacking opt-in has to REACH the worker.
///
/// This was shipped broken once and caught in review: `WorkerOptions` parsed
/// `--tsi-hijack-inet`, and the backend never passed it. Operators do not spawn
/// the worker — this backend does — so the flag was settable by tests and by
/// nobody else, which is dead configuration wearing the shape of a feature.
///
/// Asserted against the worker's own view of its argv rather than against the
/// backend's config, because the config being right is exactly what was already
/// true when the bug existed.
#[test]
fn the_hijack_opt_in_reaches_the_worker_and_is_absent_by_default() {
    let _serial = common::serial();
    for hijack in [false, true] {
        let fixture = TempDir::new().expect("fixture");
        let argv_log = fixture.path().join("argv");
        let worker = fixture.path().join("leyline-krun-worker");
        fs::write(
            &worker,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nIFS= read -r _\nprintf '%s\\n' \
                 '{{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}}' >&2\nexec /usr/bin/tail -f /dev/null\n",
                argv_log.display()
            ),
        )
        .expect("fake worker");
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).expect("worker mode");
        let cas_root = fixture.path().join("cas");
        let ephemeral_root = fixture.path().join("runs");
        fs::create_dir(&cas_root).expect("CAS");
        fs::create_dir(&ephemeral_root).expect("ephemeral root");

        let backend = fake_worker_backend(KrunWorkerConfig {
            worker,
            cas_root,
            ephemeral_root,
            runtime_files: Vec::new(),
            devices: Vec::new(),
            ready_timeout: READY_TIMEOUT,
            tsi_hijack_inet: hijack,
        });
        backend
            .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
            .expect("worker readiness");

        let argv = fs::read_to_string(&argv_log).expect("worker argv");
        assert_eq!(
            argv.lines().any(|line| line == "--tsi-hijack-inet"),
            hijack,
            "tsi_hijack_inet={hijack} must decide whether the worker is told to \
             hijack; the worker saw: {argv:?}"
        );
        assert!(backend.cancel("run-backend-01").expect("cancel worker"));
    }
}

/// ADR-0035 finding 2, microVM tier. The native tier pins this in
/// `native_worker_backend.rs`; the same refusal on the libkrun backend had no
/// test at all, because every `libkrun_*.rs` test is `#[ignore]`-gated on
/// having a hypervisor. The backend's readiness handling does not need one —
/// it reads a worker's stderr — so the drift check is testable here with the
/// same shell-script worker the rest of this file uses.
///
/// Surfaced by cargo-mutants: `replace != with ==` and `delete !` at
/// `backend.rs:294-295` both survived, meaning nothing observed whether this
/// backend compared the attested policy to the authorized one at all.
#[test]
fn a_worker_attesting_an_unauthorized_policy_never_reaches_running() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    // Well-formed readiness, correct run id, wrong policy.
    let (backend, ephemeral_root) = backend_with_worker(
        &fixture,
        "#!/bin/sh\nIFS= read -r _\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"run-backend-01\",\"confinement_digest\":\"blake3-256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff\"}' >&2\nexec /usr/bin/tail -f /dev/null\n",
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
    assert_eq!(run_root_count(&ephemeral_root), 0);
}

/// The other half of the same condition, and the one that makes it a
/// comparison rather than a blanket refusal: when the grant authorizes no
/// particular policy, whatever the worker attests is not drift.
///
/// This is the branch reached only by embedding a backend directly — the
/// service path always carries a digest, because `read_digest` rejects a
/// `RunGrant.confinementDigest` that is not a lowercase blake3-256 value. It
/// still needs pinning: `replace && with ||` survived on both backends, and
/// under `||` an absent authorization would start refusing every worker that
/// attested anything, which is a fail-CLOSED break of direct embedding rather
/// than a security hole — the kind that shows up as "it worked last release".
#[test]
fn a_grant_authorizing_no_policy_does_not_constrain_what_the_worker_attests() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let (backend, _ephemeral_root) = backend_with_worker(
        &fixture,
        "#!/bin/sh\nIFS= read -r _\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"run-backend-01\",\"confinement_digest\":\"blake3-256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff\"}' >&2\nexec /usr/bin/tail -f /dev/null\n",
    );

    // `request()` leaves `confinement_digest` empty: nothing was authorized.
    backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect("an unauthorized-policy grant must not refuse an attesting worker");
    assert!(backend.cancel("run-backend-01").expect("cancel worker"));
}

/// Readiness must bind to the run that was asked for. `replace match guard
/// run_id == request.run_id with true` survived here, so this backend would
/// have accepted a readiness event announcing any run at all — including one
/// whose confinement digest was checked against the wrong request.
#[test]
fn readiness_announcing_another_run_is_rejected() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let (backend, ephemeral_root) = backend_with_worker(
        &fixture,
        "#!/bin/sh\nIFS= read -r _\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"some-other-run\"}' >&2\nexec /usr/bin/tail -f /dev/null\n",
    );

    let error = backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect_err("readiness must bind to the requested run");
    assert!(
        format!("{error:?}").contains("unexpected run"),
        "the refusal must name the binding that failed: {error:?}"
    );
    assert_eq!(run_root_count(&ephemeral_root), 0);
}

/// `configured()` is a five-term `&&` chain, and until now nothing observed any
/// individual term: every `&&` in it, plus the whole function's return, could
/// be mutated without a test noticing. A backend that reports itself available
/// while missing its worker binary or its CAS root fails at `start` instead of
/// at capability negotiation, which is the difference between "this host
/// cannot run microVMs" and "this run mysteriously died".
///
/// The native tier already pins this shape in
/// `every_native_resource_is_required_independently`; this is its microVM
/// counterpart. Each case removes exactly one resource and leaves the other
/// four intact, which is what makes it a test of that term rather than of the
/// conjunction.
#[test]
fn every_libkrun_resource_is_required_independently() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let worker = fixture.path().join("worker");
    let cas_root = fixture.path().join("cas");
    let ephemeral_root = fixture.path().join("runs");
    let runtime_file = fixture.path().join("runtime");
    let device = fixture.path().join("device");
    fs::write(&worker, "#!/bin/sh\n").expect("worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).expect("worker mode");
    fs::create_dir(&cas_root).expect("CAS");
    fs::create_dir(&ephemeral_root).expect("runs");
    fs::write(&runtime_file, b"runtime").expect("runtime file");
    fs::write(&device, b"device").expect("device");

    let complete = || KrunWorkerConfig {
        worker: worker.clone(),
        cas_root: cas_root.clone(),
        ephemeral_root: ephemeral_root.clone(),
        runtime_files: vec![runtime_file.clone()],
        devices: vec![device.clone()],
        ready_timeout: READY_TIMEOUT,
        tsi_hijack_inet: false,
    };

    assert!(
        fake_worker_backend(complete()).capabilities().available,
        "a fully provisioned host must report the backend available, or the \
         cases below would pass for the wrong reason"
    );

    let missing = fixture.path().join("missing");
    let incomplete = [
        ("worker", {
            let mut c = complete();
            c.worker = missing.clone();
            c
        }),
        ("cas root", {
            let mut c = complete();
            c.cas_root = missing.clone();
            c
        }),
        ("ephemeral root", {
            let mut c = complete();
            c.ephemeral_root = missing.clone();
            c
        }),
        ("runtime file", {
            let mut c = complete();
            c.runtime_files = vec![missing.clone()];
            c
        }),
        ("device", {
            let mut c = complete();
            c.devices = vec![missing.clone()];
            c
        }),
    ];
    for (resource, configuration) in incomplete {
        let capabilities = fake_worker_backend(configuration).capabilities();
        assert!(
            !capabilities.available,
            "a missing {resource} must make the backend unavailable"
        );
        let reason = capabilities.unavailable_reason.unwrap_or_default();
        assert!(
            reason.contains("not configured"),
            "a missing {resource} must say why: {reason:?}"
        );
    }
    assert_eq!(
        fake_worker_backend(complete())
            .capabilities()
            .unavailable_reason,
        None
    );
}

/// A rejection kill must reach the worker's DESCENDANTS, not just the
/// worker. The fake forks its sleeper FIRST and reports the grandchild pid,
/// so by the time readiness triggers the rejection the grandchild
/// deterministically exists — a pid-targeted kill (instead of the group)
/// fails this test. The in-flight-`fork()` escape this same bead exposed —
/// the group sweep returning success while the leader's mid-fork child
/// enrolls after enumeration — cannot be pinned deterministically from a
/// shell; `terminate_process_group`'s kill → await-leader-exit → re-sweep
/// closes it, verified by stress-running the originally-leaking script
/// shapes 20× with zero leaks (bead rs-a1e8d0).
#[test]
fn rejection_kills_the_workers_grandchildren_too() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let grandchild_pid_file = fixture.path().join("grandchild.pid");
    let script = format!(
        "#!/bin/sh\n/usr/bin/tail -f /dev/null &\nprintf '%s' \"$!\" > '{}'\nIFS= read -r _\nprintf '%s\\n' '{{\"type\":\"ready\",\"run_id\":\"some-other-run\"}}' >&2\nwait\n",
        grandchild_pid_file.display()
    );
    let (backend, _ephemeral_root) = backend_with_worker(&fixture, &script);

    backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect_err("readiness must bind to the requested run");

    let raw = fs::read_to_string(&grandchild_pid_file).expect("grandchild pid file");
    let grandchild: i32 = raw.trim().parse().expect("grandchild pid");
    // Bounded spin until the grandchild is gone — `kill(pid, 0)` returns 0
    // while the process (or its unreaped zombie, briefly) exists and ESRCH
    // once it does not. The group kill lands in microseconds; the deadline
    // only fails on a genuine miss. A spin, not a sleep: the wait is the
    // probe interval, not a synchronization guess.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if unsafe { libc::kill(grandchild, 0) } != 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "grandchild {grandchild} of a rejected worker is still alive — the \
             rejection kill did not reach the worker's descendants"
        );
        std::hint::spin_loop();
    }
}

// ── Workspace shares (ADR-0044 phase C, bead ley-line-open-f210c2) ──────

/// Records each request the worker's end of the share sends.
struct RecordingShare {
    seen: std::sync::Mutex<std::sync::mpsc::Sender<Request>>,
}

impl FsService for RecordingShare {
    fn call(&self, request: Request) -> Reply {
        let _ = self.seen.lock().unwrap().send(request);
        Reply::Done
    }
}

/// Opens [`RecordingShare`]s, or refuses with `refusal`, and records what it
/// was asked to open.
struct Shares {
    refusal: Option<ExecutionError>,
    opened: std::sync::Mutex<Vec<(String, WorkspaceGrant)>>,
    seen: std::sync::Mutex<std::sync::mpsc::Sender<Request>>,
}

impl Shares {
    fn new(refusal: Option<ExecutionError>) -> (Arc<Self>, std::sync::mpsc::Receiver<Request>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (
            Arc::new(Self {
                refusal,
                opened: std::sync::Mutex::new(Vec::new()),
                seen: std::sync::Mutex::new(tx),
            }),
            rx,
        )
    }
}

impl WorkspaceShares for Shares {
    fn open(
        &self,
        run_id: &str,
        grant: &WorkspaceGrant,
    ) -> Result<Arc<dyn FsService>, ExecutionError> {
        self.opened
            .lock()
            .unwrap()
            .push((run_id.to_owned(), grant.clone()));
        if let Some(refusal) = &self.refusal {
            return Err(refusal.clone());
        }
        Ok(Arc::new(RecordingShare {
            seen: std::sync::Mutex::new(self.seen.lock().unwrap().clone()),
        }))
    }
}

fn workspace(name: &str) -> WorkspaceGrant {
    WorkspaceGrant {
        name: name.into(),
        graph_root: format!("blake3-256:{}", "e".repeat(64)),
        expected_generation: 0,
        operations: vec![WorkspaceOperation::Read, WorkspaceOperation::Mutate],
        write_scope: vec!["/".into()],
    }
}

fn request_with(workspaces: Vec<WorkspaceGrant>) -> ExecutionRequest {
    ExecutionRequest {
        workspaces,
        ..request()
    }
}

/// The shares the execution service hands the backend for `request`: each of
/// its workspaces, opened from `shares` (bead `ley-line-open-1022a8`). Only
/// the service can build them in a shipping build; this file drives the
/// backend directly, through the `test-support` constructor.
fn served(shares: &Shares, request: &ExecutionRequest) -> ServedWorkspaces {
    ServedWorkspaces::materialize_unverified(shares, &request.run_id, &request.workspaces)
        .expect("the provider opens the granted workspaces")
}

/// A fake worker that logs its arguments to `args_log`, then reports ready
/// and sleeps.
fn arg_logging_worker(args_log: &std::path::Path) -> String {
    format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nIFS= read -r _\nprintf '%s\\n' '{{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}}' >&2\nexec /usr/bin/tail -f /dev/null\n",
        args_log.display()
    )
}

/// A fake worker that finds the descriptor named by `--workspace-share-fd`
/// and reports ready with `inherited` (shell text, `$fd` expanded) as its
/// declared descriptors, after running `before_ready`.
fn share_worker(before_ready: &str, inherited: &str) -> String {
    format!(
        r#"#!/bin/sh
fd=
prev=
for arg in "$@"; do
  if [ "$prev" = "--workspace-share-fd" ]; then fd="$arg"; fi
  prev="$arg"
done
IFS= read -r _
{before_ready}
printf '%s\n' "{{\"type\":\"ready\",\"run_id\":\"run-backend-01\",\"inherited\":[{inherited}]}}" >&2
exec /usr/bin/tail -f /dev/null
"#
    )
}

/// The worker's declaration of the share it was handed, as JSON with the
/// descriptor left to the shell (`$fd`).
const DECLARED_SHARE: &str = r#"{\"descriptor\":$fd,\"channel\":{\"role\":\"workspaceShare\",\"workspace\":\"repo\",\"graphRoot\":\"blake3-256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee\"}}"#;

#[test]
fn a_granted_workspace_is_served_to_the_worker_over_an_inherited_socket() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    // The worker sends one framed INIT request down its share: a 32-byte
    // header (one Cap'n Proto segment of three words: the root pointer, a
    // data word whose union tag 0 is INIT, a null pointer), no payload.
    let script = share_worker(
        r#"[ -n "$fd" ] || exit 3
eval "printf '\040\000\000\000\000\000\000\000\000\000\000\000\003\000\000\000\000\000\000\000\001\000\001\000\000\000\000\000\000\000\000\000\000\000\000\000\000\000\000\000' >&$fd" || exit 4"#,
        DECLARED_SHARE,
    );
    let (backend, _ephemeral_root) = backend_with_worker(&fixture, &script);
    let (shares, seen) = Shares::new(None);

    let request = request_with(vec![workspace("repo")]);
    let started = backend
        .start(&request, served(&shares, &request), leyline_runtime::RunConnections::none())
        .expect("a ready worker with its share");
    assert_eq!(
        seen.recv_timeout(READY_TIMEOUT)
            .expect("the worker's request reached the share"),
        Request::Init
    );
    assert_eq!(
        *shares.opened.lock().unwrap(),
        vec![("run-backend-01".to_owned(), workspace("repo"))],
        "the provider opens the run's one granted workspace"
    );
    // The run names the share it took, for the receipt.
    assert_eq!(
        started
            .attached
            .iter()
            .map(AttachedWorkspace::channel)
            .collect::<Vec<_>>(),
        vec![WorkerChannel::WorkspaceShare {
            workspace: "repo".into(),
            graph_root: format!("blake3-256:{}", "e".repeat(64)),
        }]
    );
}

/// The worker's declared descriptors must be exactly what the backend handed
/// it: a worker that does not declare its share, or declares one it was not
/// handed, never reaches running.
#[test]
fn a_worker_declaring_other_descriptors_than_it_was_handed_never_reaches_running() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");

    // Handed a share, declares none.
    let (backend, ephemeral_root) = backend_with_worker(&fixture, &share_worker(":", ""));
    let (shares, _seen) = Shares::new(None);
    let granted = request_with(vec![workspace("repo")]);
    let error = backend
        .start(&granted, served(&shares, &granted), leyline_runtime::RunConnections::none())
        .expect_err("an undeclared share");
    assert_eq!(
        error.code,
        leyline_runtime::ErrorCode::IdentityPolicyMismatch,
        "{error:?}"
    );
    assert!(error.detail.contains("descriptor"), "{error:?}");
    assert_eq!(run_root_count(&ephemeral_root), 0);

    // Handed nothing, declares a share.
    let fixture = TempDir::new().expect("fixture");
    let (backend, ephemeral_root) =
        backend_with_worker(&fixture, &share_worker("fd=7", DECLARED_SHARE));
    let error = backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect_err("a declared share nothing handed");
    assert_eq!(
        error.code,
        leyline_runtime::ErrorCode::IdentityPolicyMismatch,
        "{error:?}"
    );
    assert_eq!(run_root_count(&ephemeral_root), 0);
}

/// A descriptor this process holds without close-on-exec (inherited from
/// its own parent, as a build tool's jobserver pipes are) does not reach the
/// worker: the backend hands it its standard streams and declared channels
/// only, so the worker's own check finds nothing undeclared.
#[test]
fn a_descriptor_the_backend_holds_open_across_exec_does_not_reach_the_worker() {
    use std::os::fd::AsRawFd;
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let leaked = fs::File::open("/dev/null").expect("a descriptor");
    let fd = leaked.as_raw_fd();
    // SAFETY: fcntl on a descriptor this test owns.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        assert!(flags >= 0 && libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) >= 0);
    }
    let script = format!(
        "#!/bin/sh\nIFS= read -r _\n[ -e /dev/fd/{fd} ] && exit 9\nprintf '%s\\n' '{{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}}' >&2\nexec /usr/bin/tail -f /dev/null\n"
    );
    let (backend, _ephemeral_root) = backend_with_worker(&fixture, &script);
    let started = backend.start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none());
    drop(leaked);
    started.expect("the worker did not inherit the backend's open descriptor");
}

#[test]
fn a_run_without_workspaces_is_handed_no_share() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let args_log = fixture.path().join("args.log");
    let (backend, _ephemeral_root) = backend_with_worker(&fixture, &arg_logging_worker(&args_log));

    let started = backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect("ready worker");
    let args = fs::read_to_string(&args_log).expect("worker arguments");
    assert!(
        !args.contains("--workspace-share-fd"),
        "no workspace, no share: {args}"
    );
    assert!(started.attached.is_empty(), "no workspace, no channel");
}

#[test]
fn a_granted_workspace_is_refused_before_spawn_where_no_share_can_serve_it() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let args_log = fixture.path().join("args.log");

    // A request naming a workspace, and no share handed over: what a direct
    // embedder, who cannot build a share, gets. The run does not start
    // without its workspace.
    let (backend, ephemeral_root) = backend_with_worker(&fixture, &arg_logging_worker(&args_log));
    let one = request_with(vec![workspace("repo")]);
    let error = backend
        .start(&one, ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect_err("a workspace grant with no share behind it");
    assert_eq!(error.code, leyline_runtime::ErrorCode::WorkspaceUnserved);
    assert!(error.detail.contains("no workspace"), "{error}");

    // A share for a workspace the request does not name: the request is
    // what the worker is told, so the two must agree.
    let (shares, _seen) = Shares::new(None);
    let other = request_with(vec![workspace("docs")]);
    let error = backend
        .start(&one, served(&shares, &other), leyline_runtime::RunConnections::none())
        .expect_err("a share for another workspace");
    assert_eq!(error.code, leyline_runtime::ErrorCode::WorkspaceUnserved);
    let error = backend
        .start(&request(), served(&shares, &other), leyline_runtime::RunConnections::none())
        .expect_err("a share the request does not name");
    assert_eq!(error.code, leyline_runtime::ErrorCode::WorkspaceUnserved);

    // Two workspaces: the tier attaches one.
    let two = request_with(vec![workspace("repo"), workspace("docs")]);
    let error = backend
        .start(&two, served(&shares, &two), leyline_runtime::RunConnections::none())
        .expect_err("two workspaces");
    assert_eq!(error.code, leyline_runtime::ErrorCode::WorkspaceUnserved);
    assert!(error.detail.contains("attaches one"), "{error}");

    // The provider refuses: its error is the run's, raised where the shares
    // are materialized, before any backend is called.
    let refusal = ExecutionError {
        code: leyline_runtime::ErrorCode::IdentityPolicyMismatch,
        retryable: false,
        detail: "the store no longer holds the granted root".into(),
    };
    let (refusing, _seen) = Shares::new(Some(refusal.clone()));
    assert_eq!(
        ServedWorkspaces::materialize_unverified(refusing.as_ref(), &one.run_id, &one.workspaces)
            .err()
            .expect("the provider refused"),
        refusal
    );

    assert!(!args_log.exists(), "no worker was spawned");
    assert_eq!(run_root_count(&ephemeral_root), 0);
}

/// Bead ley-line-open-831bf1, I1-I3: a microVM backend reports
/// `available=true` only for a worker that can enter the hypervisor, and says
/// why not BEFORE any run is accepted. On macOS that is the worker binary's
/// `com.apple.security.hypervisor` entitlement, read from its signature.
///
/// The binaries are real Mach-O copies of this test executable: one with no
/// signature at all, one with the linker's ad-hoc signature and no
/// entitlements (what `cargo build` leaves), and one signed by
/// `tools/sign_krun_worker.sh`, the path every build, install, and release
/// takes. Nothing is executed, so the copies need not be workers.
#[test]
fn a_worker_without_the_hypervisor_entitlement_is_unavailable() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let this_binary = std::env::current_exe().expect("test binary");
    let cas_root = fixture.path().join("cas");
    let ephemeral_root = fixture.path().join("runs");
    fs::create_dir(&cas_root).expect("CAS");
    fs::create_dir(&ephemeral_root).expect("runs");
    let backend_for = |worker: &std::path::Path| {
        KrunWorkerBackend::new(KrunWorkerConfig {
            worker: worker.to_path_buf(),
            cas_root: cas_root.clone(),
            ephemeral_root: ephemeral_root.clone(),
            runtime_files: Vec::new(),
            devices: Vec::new(),
            ready_timeout: READY_TIMEOUT,
            tsi_hijack_inet: false,
        })
    };

    let unsigned = fixture.path().join("unsigned-worker");
    fs::copy(&this_binary, &unsigned).expect("copy");
    let linker_signed = fixture.path().join("linker-signed-worker");
    fs::copy(&this_binary, &linker_signed).expect("copy");
    let signed = fixture.path().join("signed-worker");
    fs::copy(&this_binary, &signed).expect("copy");
    hypervisor::sign_krun_worker(&signed);

    // Off macOS no entitlement exists to hold (KVM access is a device grant),
    // so the host's rule admits every copy. The macOS rule is then applied
    // explicitly: with no codesign to read a signature, it admits none.
    let macos = cfg!(target_os = "macos");
    if !macos {
        for worker in [&unsigned, &linker_signed, &signed] {
            let capabilities = backend_for(worker).capabilities();
            assert!(capabilities.available, "{capabilities:?}");
            assert_eq!(capabilities.unavailable_reason, None);
        }
    } else {
        let status = std::process::Command::new("/usr/bin/codesign")
            .arg("--remove-signature")
            .arg(&unsigned)
            .status()
            .expect("codesign --remove-signature");
        assert!(status.success(), "remove signature: {status}");

        let entitled = backend_for(&signed).capabilities();
        assert!(
            entitled.available,
            "a worker signed by tools/sign_krun_worker.sh must be available, or \
             the cases below would pass for the wrong reason: {entitled:?}"
        );
        assert_eq!(entitled.unavailable_reason, None);
    }

    for (label, worker) in [("unsigned", &unsigned), ("linker-signed", &linker_signed)] {
        let backend = backend_for(worker).with_entitlement_platform("macos");
        let capabilities = backend.capabilities();
        assert!(
            !capabilities.available,
            "an {label} worker cannot enter the hypervisor, so the backend \
             must not report itself available"
        );
        let reason = capabilities
            .unavailable_reason
            .unwrap_or_else(|| panic!("an unavailable {label} worker must say why"));
        assert!(
            reason.contains("com.apple.security.hypervisor"),
            "the {label} reason must name the entitlement: {reason}"
        );

        // A run is never accepted: neither by direct start, which would
        // otherwise spawn the worker and die inside krun_start_enter, nor
        // by capability negotiation through the service.
        let error = backend
            .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
            .expect_err("unentitled start");
        assert!(
            error.detail.contains("com.apple.security.hypervisor"),
            "{error}"
        );
        assert_eq!(run_root_count(&ephemeral_root), 0, "no run root was made");
        let service = leyline_runtime::ExecutionService::new(backend);
        let error = service
            .provision(BackendClass::MicroVm, "provision-unentitled")
            .expect_err("unentitled provision");
        assert_eq!(error.code, leyline_runtime::ErrorCode::UnsupportedBackend);
        assert!(
            error.detail.contains("com.apple.security.hypervisor"),
            "{error}"
        );
    }

    // A caller that vouches for its worker (a fake in a supervision test)
    // skips the rule, even the macOS one; that is the only way around it.
    let vouched = backend_for(&unsigned)
        .with_entitlement_platform("macos")
        .with_unverified_hypervisor_access()
        .capabilities();
    assert!(vouched.available, "{vouched:?}");
    assert_eq!(vouched.unavailable_reason, None);

    if macos {
        // The verdict follows the binary: signing the same path makes it
        // available without rebuilding the backend.
        let backend = backend_for(&linker_signed);
        assert!(!backend.capabilities().available);
        hypervisor::sign_krun_worker(&linker_signed);
        assert!(backend.capabilities().available);
    }
}

// ---------------------------------------------------------------------------
// A worker never outlives its supervisor (bead ley-line-open-f963e6).

/// A bound on a failure only: every wait below ends on an event first.
const EVENT_BOUND: Duration = Duration::from_secs(30);

/// Whether `pid` names a process (a zombie included).
fn alive(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 checks existence and delivers nothing.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// A named pipe at `path`.
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
/// worker is reaped and its ephemeral run root removed; the start it raced
/// ends `Cancelled`. Before the fix it answered "nothing to cancel" while
/// the worker ran on.
#[test]
fn a_cancel_between_spawn_and_readiness_stops_the_worker_and_removes_its_run_root() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let spawned = fixture.path().join("spawned");
    mkfifo(&spawned);
    // Reads its request, says it exists, and never reports readiness.
    let (backend, ephemeral_root) = backend_with_worker(
        &fixture,
        &format!(
            "#!/bin/sh\nIFS= read -r _\necho $$ > '{}'\nexec /usr/bin/tail -f /dev/null\n",
            spawned.display()
        ),
    );
    let backend = Arc::new(backend);
    let starter = {
        let backend = Arc::clone(&backend);
        std::thread::spawn(move || backend.start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none()))
    };

    // EVENT: the worker exists, and is starting.
    let worker = pid_from(&spawned, "the starting worker");
    let _cleanup = KillOnDrop(worker);
    assert!(alive(worker));
    assert_eq!(run_root_count(&ephemeral_root), 1);

    assert!(
        backend.cancel("run-backend-01").expect("cancel"),
        "a starting run was reported as nothing to cancel"
    );
    // `cancel` has returned: the worker is reaped and its run root gone.
    assert!(
        !alive(worker),
        "the starting worker {worker} survived its cancel"
    );
    assert_eq!(
        run_root_count(&ephemeral_root),
        0,
        "the cancelled start left its ephemeral run root"
    );
    let error = starter
        .join()
        .expect("the start thread")
        .expect_err("a cancelled start does not start");
    assert_eq!(error.code, ErrorCode::Cancelled, "{error:?}");
    // Retired: nothing is left to cancel.
    assert!(!backend.cancel("run-backend-01").expect("second cancel"));
}

/// Set in the embedding process [`lifeline_embedder`] runs as: the fixture
/// directory holding its fake worker, CAS and run roots.
const EMBEDDER_ENV: &str = "LEYLINE_TEST_LIFELINE_EMBEDDER";

/// The process embedding the backend, for
/// `a_worker_ends_when_the_process_embedding_its_backend_is_killed`: starts
/// one run and supervises it until it is killed. Not a test on its own.
#[test]
#[ignore = "the embedding process of a lifeline test; run by that test"]
fn lifeline_embedder() {
    let Some(fixture) = std::env::var_os(EMBEDDER_ENV) else {
        return;
    };
    let fixture = std::path::PathBuf::from(fixture);
    let backend = fake_worker_backend(KrunWorkerConfig {
        worker: fixture.join("leyline-krun-worker"),
        cas_root: fixture.join("cas"),
        ephemeral_root: fixture.join("runs"),
        runtime_files: Vec::new(),
        devices: Vec::new(),
        ready_timeout: READY_TIMEOUT,
        tsi_hijack_inet: false,
    });
    backend
        .start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none())
        .expect("ready worker");
    // Supervising, until this process is killed.
    loop {
        std::thread::park();
    }
}

/// `SIGKILL` the process that embeds the backend mid-run: its worker ends
/// within a bound, observed as the kernel's exit event for the orphan.
///
/// The fake worker does what the real one does with its lifeline: it reads
/// the descriptor `--lifeline-fd` names until end-of-file and ends then. So
/// the test holds the backend to its half: the worker is handed an open read
/// end, and no process but the embedder holds a write end (a copy leaked into
/// the worker itself would keep it alive forever). The real worker's half is
/// `krun-worker/tests/krun_worker.rs`, and both together, with a booted
/// guest, `krun-worker/tests/libkrun_guest_lifeline.rs`.
#[test]
fn a_worker_ends_when_the_process_embedding_its_backend_is_killed() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let spawned = fixture.path().join("spawned");
    mkfifo(&spawned);
    let worker = fixture.path().join("leyline-krun-worker");
    fs::write(
        &worker,
        format!(
            concat!(
                "#!/bin/sh\n",
                "lifeline=\n",
                "while [ \"$#\" -gt 0 ]; do\n",
                "  if [ \"$1\" = --lifeline-fd ]; then lifeline=\"$2\"; shift 2; else shift; fi\n",
                "done\n",
                "IFS= read -r _\n",
                "printf '%s\\n' '{{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}}' >&2\n",
                "echo $$ > '{spawned}'\n",
                // Without a lifeline, nothing ends this worker: the test's
                // bound fails, as it would for the orphan the bead reports.
                "[ -n \"$lifeline\" ] || exec /usr/bin/tail -f /dev/null\n",
                "exec /bin/cat 0<&\"$lifeline\"\n",
            ),
            spawned = spawned.display()
        ),
    )
    .expect("fake worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).expect("worker mode");
    fs::create_dir(fixture.path().join("cas")).expect("CAS");
    fs::create_dir(fixture.path().join("runs")).expect("ephemeral root");

    let mut embedder = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "lifeline_embedder",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(EMBEDDER_ENV, fixture.path())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn the embedding process");
    let _embedder_cleanup = KillOnDrop(embedder.id() as libc::pid_t);

    // EVENT: the run is started, under the embedder's supervision.
    let worker = pid_from(&spawned, "the embedder's worker");
    let _worker_cleanup = KillOnDrop(worker);
    assert!(alive(worker));

    // SAFETY: a signal to the child this test spawned.
    assert_eq!(
        unsafe { libc::kill(embedder.id() as libc::pid_t, libc::SIGKILL) },
        0
    );
    embedder.wait().expect("reap the embedder");
    // EVENT: the orphaned worker's exit.
    common::await_exit(worker, EVENT_BOUND).unwrap_or_else(|error| {
        panic!("the worker outlived the process embedding its backend: {error}")
    });
}

/// What [`a_cancel_right_after_spawn_finds_the_run`] observes, in order.
#[derive(Debug)]
enum Seen {
    CancelWaiting,
    CancelReturned(Result<bool, ExecutionError>),
}

/// A cancel that arrives after the worker is spawned and before `start`
/// does anything else finds the run: the run is registered before the spawn,
/// so there is no moment with a worker and nothing to cancel. `start` is
/// paused right after the spawn (a `test-support` hook) while the cancel is
/// issued; before the fix the cancel answered `Ok(false)` there at once.
#[test]
fn a_cancel_right_after_spawn_finds_the_run() {
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
    let (backend, ephemeral_root) = backend_with_worker(
        &fixture,
        "#!/bin/sh\nIFS= read -r _\nexec /usr/bin/tail -f /dev/null\n",
    );
    let backend = Arc::new(backend.with_start_hook(move |point| match point {
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
        let backend = Arc::clone(&backend);
        std::thread::spawn(move || backend.start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none()))
    };
    spawned
        .recv_timeout(EVENT_BOUND)
        .expect("TIMEOUT: the worker was never spawned");
    let canceller = {
        let backend = Arc::clone(&backend);
        std::thread::spawn(move || {
            let _ = seen_tx.send(Seen::CancelReturned(backend.cancel("run-backend-01")));
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
    let error = starter
        .join()
        .unwrap()
        .expect_err("a cancelled start does not start");
    assert_eq!(error.code, ErrorCode::Cancelled, "{error:?}");
    assert_eq!(run_root_count(&ephemeral_root), 0);
}

/// A cancel before readiness whose run root cannot be removed says so: the
/// cleanup's failure is its answer, not `Ok(true)`.
#[test]
fn a_cancel_before_readiness_reports_a_run_root_it_could_not_remove() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let spawned = fixture.path().join("spawned");
    mkfifo(&spawned);
    let (backend, ephemeral_root) = backend_with_worker(
        &fixture,
        &format!(
            "#!/bin/sh\nIFS= read -r _\necho $$ > '{}'\nexec /usr/bin/tail -f /dev/null\n",
            spawned.display()
        ),
    );
    let backend = Arc::new(backend);
    let starter = {
        let backend = Arc::clone(&backend);
        std::thread::spawn(move || backend.start(&request(), ServedWorkspaces::none(), leyline_runtime::RunConnections::none()))
    };
    let worker = pid_from(&spawned, "the starting worker");
    let _cleanup = KillOnDrop(worker);
    // The run root's parent no longer lets an entry be removed.
    fs::set_permissions(&ephemeral_root, fs::Permissions::from_mode(0o555)).unwrap();
    let answer = backend.cancel("run-backend-01");
    fs::set_permissions(&ephemeral_root, fs::Permissions::from_mode(0o755)).unwrap();
    let error = answer.expect_err("a cancel whose cleanup failed reports it");
    assert!(error.detail.contains("ephemeral"), "{error:?}");
    assert!(!alive(worker), "the worker {worker} survived its cancel");
    let started = starter.join().unwrap();
    assert!(started.is_err(), "{started:?}");
}

/// Cancels queued at a hook point while `start` ends on its own failure: a
/// `test-support` hook issues the cancel from another thread at `queue_at`,
/// and returns once that cancel is waiting. Returns the cancel's answer and
/// the start's result.
fn cancel_queued_at(
    backend: KrunWorkerBackend,
    queue_at: leyline_runtime::backends::libkrun::backend::StartPoint,
    also_at_spawn: impl Fn() + Send + Sync + 'static,
) -> (
    Result<bool, leyline_runtime::ExecutionError>,
    Result<leyline_runtime::BackendRun, leyline_runtime::ExecutionError>,
) {
    use leyline_runtime::backends::libkrun::backend::StartPoint;
    use std::sync::Mutex as StdMutex;
    use std::sync::mpsc;
    let backend_slot: Arc<StdMutex<Option<Arc<KrunWorkerBackend>>>> = Arc::new(StdMutex::new(None));
    let (waiting_tx, waiting) = mpsc::channel();
    let waiting = StdMutex::new(waiting);
    let (answer_tx, answer) = mpsc::channel();
    let slot = Arc::clone(&backend_slot);
    let backend = Arc::new(backend.with_start_hook(move |point| {
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
                let _ = answer_tx.send(backend.cancel("run-backend-01"));
            });
            waiting
                .lock()
                .unwrap()
                .recv_timeout(EVENT_BOUND)
                .expect("TIMEOUT: the cancel never queued");
        }
    }));
    *backend_slot.lock().unwrap() = Some(Arc::clone(&backend));
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
    let (backend, ephemeral_root) = backend_with_worker(&fixture, "#!/bin/sh\nexit 0\n");
    // A worker that cannot be executed: the spawn fails.
    fs::set_permissions(
        fixture.path().join("leyline-krun-worker"),
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
    let (backend, ephemeral_root) = backend_with_worker(
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
