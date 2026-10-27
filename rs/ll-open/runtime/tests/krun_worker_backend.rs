use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Barrier};
use std::time::Duration;

use leyline_runtime::backends::libkrun::backend::{KrunWorkerBackend, KrunWorkerConfig};
use leyline_runtime::backends::libkrun::share::{FsService, Reply, Request, WorkspaceShares};
use leyline_runtime::{
    Backend, BackendClass, BackendRunStatus, DigestRef, ExecutionError, ExecutionRequest,
    ResourceLimits, WorkerChannel, WorkspaceGrant, WorkspaceOperation,
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
            vcpus: 1,
            memory_mib: 512,
            wall_time_ms: 10_000,
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

    backend.start(&request()).expect("ready worker");
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

    backend.start(&request()).expect("ready worker");
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

    backend.start(&request()).expect("ready worker");
    assert_eq!(run_root_count(&ephemeral_root), 1);
    assert!(Backend::cancel(&backend, "run-backend-01").expect("cancel worker"));

    assert_eq!(run_root_count(&ephemeral_root), 0);
    assert!(!Backend::cancel(&backend, "run-backend-01").expect("repeat cancel"));
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

    let error = backend.start(&request()).expect_err("worker setup failure");

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

    backend.start(&request()).expect("first worker");
    let error = backend.start(&request()).expect_err("duplicate run ID");

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
        backend.start(&request()).expect("worker readiness");

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
        .start(&request)
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
        .start(&request())
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
        .start(&request())
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
        assert!(
            !fake_worker_backend(configuration).capabilities().available,
            "a missing {resource} must make the backend unavailable"
        );
    }
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
        .start(&request())
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
    }
}

fn request_with(workspaces: Vec<WorkspaceGrant>) -> ExecutionRequest {
    ExecutionRequest {
        workspaces,
        ..request()
    }
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
    let backend = backend.with_workspace_shares(shares.clone());

    let started = backend
        .start(&request_with(vec![workspace("repo")]))
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
    // The run names the share it holds, for the receipt.
    assert_eq!(
        started.channels,
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
    let backend = backend.with_workspace_shares(shares);
    let error = backend
        .start(&request_with(vec![workspace("repo")]))
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
        .start(&request())
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
    let started = backend.start(&request());
    drop(leaked);
    started.expect("the worker did not inherit the backend's open descriptor");
}

#[test]
fn a_run_without_workspaces_is_handed_no_share() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let args_log = fixture.path().join("args.log");
    let (backend, _ephemeral_root) = backend_with_worker(&fixture, &arg_logging_worker(&args_log));
    let (shares, _seen) = Shares::new(None);
    let backend = backend.with_workspace_shares(shares.clone());

    let started = backend.start(&request()).expect("ready worker");
    let args = fs::read_to_string(&args_log).expect("worker arguments");
    assert!(
        !args.contains("--workspace-share-fd"),
        "no workspace, no share: {args}"
    );
    assert_eq!(started.channels, Vec::new(), "no workspace, no channel");
    assert!(shares.opened.lock().unwrap().is_empty());
}

#[test]
fn a_granted_workspace_is_refused_before_spawn_where_no_share_can_serve_it() {
    let _serial = common::serial();
    let fixture = TempDir::new().expect("fixture");
    let args_log = fixture.path().join("args.log");

    // No provider: the grant cannot be delivered, so the run does not start
    // without it.
    let (backend, ephemeral_root) = backend_with_worker(&fixture, &arg_logging_worker(&args_log));
    let error = backend
        .start(&request_with(vec![workspace("repo")]))
        .expect_err("a workspace grant with nothing to serve it");
    assert_eq!(error.code, leyline_runtime::ErrorCode::UnsupportedBackend);
    assert!(error.detail.contains("repo"), "{error}");

    // Two workspaces: the tier attaches one.
    let (shares, _seen) = Shares::new(None);
    let backend = backend.with_workspace_shares(shares.clone());
    let error = backend
        .start(&request_with(vec![workspace("repo"), workspace("docs")]))
        .expect_err("two workspaces");
    assert_eq!(error.code, leyline_runtime::ErrorCode::UnsupportedBackend);
    assert!(shares.opened.lock().unwrap().is_empty());

    // The provider refuses: its error is the run's.
    let refusal = ExecutionError {
        code: leyline_runtime::ErrorCode::IdentityPolicyMismatch,
        retryable: false,
        detail: "the store no longer holds the granted root".into(),
    };
    let (shares, _seen) = Shares::new(Some(refusal.clone()));
    let backend = backend.with_workspace_shares(shares);
    assert_eq!(
        backend
            .start(&request_with(vec![workspace("repo")]))
            .expect_err("the provider refused"),
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

    if !cfg!(target_os = "macos") {
        // No entitlement exists to hold: KVM access is a device grant.
        for worker in [&unsigned, &linker_signed, &signed] {
            let capabilities = backend_for(worker).capabilities();
            assert!(capabilities.available, "{capabilities:?}");
            assert_eq!(capabilities.unavailable_reason, None);
        }
        return;
    }

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

    for (label, worker) in [("unsigned", &unsigned), ("linker-signed", &linker_signed)] {
        let backend = backend_for(worker);
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
        let error = backend.start(&request()).expect_err("unentitled start");
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

    // The verdict follows the binary: signing the same path makes it
    // available without rebuilding the backend.
    let backend = backend_for(&linker_signed);
    assert!(!backend.capabilities().available);
    hypervisor::sign_krun_worker(&linker_signed);
    assert!(backend.capabilities().available);
}
