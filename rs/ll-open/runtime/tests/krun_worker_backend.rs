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
        confinement_digest: String::new(),
        confinement_manifest: None,
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
        tsi_hijack_inet: false,
    });
    (backend, ephemeral_root)
}

#[test]
fn backend_spawns_the_explicit_first_party_worker_and_waits_for_ready() {
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
        tsi_hijack_inet: false,
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
        "#!/bin/sh\nIFS= read -r _\nprintf '%s\\n' '{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}' >&2\nexec /usr/bin/tail -f /dev/null\n",
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
        let libkrun = fixture.path().join("libkrun.dylib");
        fs::write(&libkrun, b"library").expect("library fixture");

        let backend = KrunWorkerBackend::new(KrunWorkerConfig {
            worker,
            cas_root,
            ephemeral_root,
            libkrun,
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

/// `configured()` is a six-term `&&` chain, and until now nothing observed any
/// individual term: every `&&` in it, plus the whole function's return, could
/// be mutated without a test noticing. A backend that reports itself available
/// while missing its worker binary or its CAS root fails at `start` instead of
/// at capability negotiation, which is the difference between "this host
/// cannot run microVMs" and "this run mysteriously died".
///
/// The native tier already pins this shape in
/// `every_native_resource_is_required_independently`; this is its microVM
/// counterpart. Each case removes exactly one resource and leaves the other
/// five intact, which is what makes it a test of that term rather than of the
/// conjunction.
#[test]
fn every_libkrun_resource_is_required_independently() {
    let fixture = TempDir::new().expect("fixture");
    let worker = fixture.path().join("worker");
    let cas_root = fixture.path().join("cas");
    let ephemeral_root = fixture.path().join("runs");
    let libkrun = fixture.path().join("libkrun.dylib");
    let runtime_file = fixture.path().join("runtime");
    let device = fixture.path().join("device");
    fs::write(&worker, "#!/bin/sh\n").expect("worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).expect("worker mode");
    fs::create_dir(&cas_root).expect("CAS");
    fs::create_dir(&ephemeral_root).expect("runs");
    fs::write(&libkrun, b"library").expect("library");
    fs::write(&runtime_file, b"runtime").expect("runtime file");
    fs::write(&device, b"device").expect("device");

    let complete = || KrunWorkerConfig {
        worker: worker.clone(),
        cas_root: cas_root.clone(),
        ephemeral_root: ephemeral_root.clone(),
        libkrun: libkrun.clone(),
        runtime_files: vec![runtime_file.clone()],
        devices: vec![device.clone()],
        ready_timeout: READY_TIMEOUT,
        tsi_hijack_inet: false,
    };

    assert!(
        KrunWorkerBackend::new(complete()).capabilities().available,
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
        ("libkrun library", {
            let mut c = complete();
            c.libkrun = missing.clone();
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
            !KrunWorkerBackend::new(configuration)
                .capabilities()
                .available,
            "a missing {resource} must make the backend unavailable"
        );
    }
}
