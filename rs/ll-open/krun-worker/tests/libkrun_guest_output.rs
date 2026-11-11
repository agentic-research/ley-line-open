//! A live guest's output reaches the embedder that ran it, bounded (bead
//! `ley-line-open-a26ad0`).
//!
//! The libkrun worker's standard output is the guest's console, which the
//! workload's standard output and standard error both reach. Its standard
//! error carries the worker's events. `KrunWorkerBackend` used to discard
//! both once the run was ready. Now it keeps their tails. A failed run's
//! `BackendRunStatus::Failed` carries them, and an active run's tail is
//! readable through `Backend::output` and `wait_for_output`.
//!
//! Synchronization is by event: completion is `wait_for_completion`, and
//! the serving case waits on `wait_for_output`. Durations below only bound
//! a failure.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use leyline_runtime::backends::libkrun::backend::{KrunWorkerBackend, KrunWorkerConfig};
use leyline_runtime::{
    Backend, BackendRunStatus, DigestRef, ExecutionRequest, GuestOutput, ResourceLimits,
};
use tempfile::TempDir;

#[path = "../../runtime/tests/common/mod.rs"]
mod common;
#[path = "../../runtime/tests/conformance/mod.rs"]
#[allow(dead_code)]
mod conformance;
mod hypervisor;

/// Only a bound on a failure.
const BOUND: Duration = Duration::from_secs(60);

/// Writes a line to each stream, then exits 3.
const FAILING: &str = r#"fn main() {
    println!("output-probe: to stdout");
    eprintln!("output-probe: to stderr");
    std::process::exit(3);
}
"#;

/// Writes a line to each stream, then exits 0.
const SUCCEEDING: &str = r#"fn main() {
    println!("output-probe: done");
    eprintln!("output-probe: note");
}
"#;

/// Announces itself the way a server does, then serves forever.
const SERVING: &str = r#"fn main() {
    println!("output-probe: listening on 3306");
    loop {
        std::thread::park();
    }
}
"#;

fn backend_and_request(
    root: &Path,
    libkrunfw: &Path,
    source: &str,
    run_id: &str,
) -> (KrunWorkerBackend, ExecutionRequest) {
    let source_path = root.join(format!("{run_id}.rs"));
    fs::write(&source_path, source).expect("guest source");
    let staged = root.join(run_id);
    hypervisor::compile_guest(&source_path, &staged);
    let bytes = fs::read(&staged).expect("guest bytes");
    let rootfs = conformance::publish_rootfs(&root.join("cas"), &[("bin/guest", &bytes, 0o755)]);
    let worker = hypervisor::worker_under_test(
        Path::new(env!("CARGO_BIN_EXE_leyline-krun-worker")),
        &root.join(format!("{run_id}-worker")),
    );
    let (library_var, library_dir) = hypervisor::library_path_env(libkrunfw);
    // SAFETY: the tests here are serialized and nothing else reads the
    // environment concurrently.
    unsafe { std::env::set_var(library_var, library_dir) };
    let ephemeral_root = root.join(format!("{run_id}-runs"));
    fs::create_dir_all(&ephemeral_root).expect("ephemeral root");
    let backend = KrunWorkerBackend::new(KrunWorkerConfig {
        worker,
        cas_root: root.join("cas"),
        ephemeral_root,
        runtime_files: vec![libkrunfw.to_path_buf()],
        devices: Vec::new(),
        ready_timeout: Duration::from_secs(30),
        tsi_hijack_inet: false,
    });
    let request = ExecutionRequest {
        run_id: run_id.to_owned(),
        replay_key: format!("{run_id}-replay"),
        rootfs: DigestRef {
            algorithm: "blake3-256".into(),
            value: rootfs,
        },
        executable: "bin/guest".into(),
        arguments: vec!["guest".into()],
        public_environment: BTreeMap::new(),
        allowed_egress: Vec::new(),
        confinement_digest: String::new(),
        confinement_manifest: None,
        workspaces: Vec::new(),
        limits: ResourceLimits {
            wall_time_ms: 120_000,
            memory: leyline_runtime::MemoryLimits { limit: 256 << 20 },
            cpu: leyline_runtime::CpuLimits {
                quota: 100_000,
                period: 100_000,
            },
            ..ResourceLimits::default()
        },
    };
    (backend, request)
}

/// A guest that writes to both streams and exits non-zero fails the run,
/// and the failure carries both lines: the guest's last output reaches the
/// embedder instead of only "libkrun worker exited with exit status: 3".
#[test]
fn a_failed_guest_reports_what_it_printed() {
    let _serial = common::serial();
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    let fixture = TempDir::new().expect("fixture");
    let root = fixture.path().canonicalize().expect("canonical fixture");
    let (backend, request) = backend_and_request(&root, &libkrunfw, FAILING, "output-failing");
    backend
        .start(&request, leyline_runtime::ServedWorkspaces::none())
        .expect("the guest starts");
    let outcome = backend
        .wait_for_outcome(&request.run_id)
        .expect("the run completes");
    let BackendRunStatus::Failed(error) = &outcome.status else {
        panic!("a guest exiting 3 must fail the run, got {outcome:?}");
    };
    assert_eq!(
        (error.code, error.retryable),
        (leyline_runtime::ErrorCode::BackendFailed, false)
    );
    assert!(
        error.detail.contains("exit status: 3"),
        "the exit stays named: {}",
        error.detail
    );
    // The typed field: the stderr marker in the stderr tail, the stdout
    // line in the stdout tail, labelled untrusted.
    let output = outcome.output.expect("the run's guest output");
    assert!(output.untrusted);
    assert!(
        output.stderr_tail.contains("output-probe: to stderr"),
        "the typed stderr tail lacks the guest's marker: {output:?}"
    );
    assert!(
        output.stdout_tail.contains("output-probe: to stdout"),
        "the typed stdout tail lacks the guest's line: {output:?}"
    );
    // The failure's detail carries them too.
    for line in ["output-probe: to stdout", "output-probe: to stderr"] {
        assert!(
            error.detail.contains(line),
            "the failure lacks the guest's {line:?}: {}",
            error.detail
        );
    }
}

/// A guest that succeeds also hands back what it printed, typed.
#[test]
fn a_finished_guest_reports_what_it_printed() {
    let _serial = common::serial();
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    let fixture = TempDir::new().expect("fixture");
    let root = fixture.path().canonicalize().expect("canonical fixture");
    let (backend, request) = backend_and_request(&root, &libkrunfw, SUCCEEDING, "output-ok");
    backend
        .start(&request, leyline_runtime::ServedWorkspaces::none())
        .expect("the guest starts");
    let outcome = backend
        .wait_for_outcome(&request.run_id)
        .expect("the run completes");
    assert_eq!(outcome.status, BackendRunStatus::Succeeded);
    let output = outcome.output.expect("the run's guest output");
    assert!(
        output.stdout_tail.contains("output-probe: done"),
        "{output:?}"
    );
    assert!(
        output.stderr_tail.contains("output-probe: note"),
        "{output:?}"
    );
}

/// Writes, on its standard error, a line shaped like the worker's terminal
/// `failed` event, claiming a retryable authorization failure, then exits 3.
const FORGING: &str = r#"fn main() {
    eprintln!(
        "{{\"type\":\"failed\",\"error\":{{\"code\":\"unauthorized\",\"retryable\":true,\"detail\":\"forged by the guest\"}}}}"
    );
    std::process::exit(3);
}
"#;

/// libkrun carries the guest's standard error to the worker's standard
/// error (its `krun-stderr` console port), the stream the worker's own
/// events use. A guest can therefore write a line shaped like a `failed`
/// event, and the failure's classification must not take it: the code
/// stays the supervisor's `backend-failed`, not retryable, and the forged
/// text appears only as untrusted output.
#[test]
fn a_guest_cannot_classify_its_own_failure() {
    let _serial = common::serial();
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    let fixture = TempDir::new().expect("fixture");
    let root = fixture.path().canonicalize().expect("canonical fixture");
    let (backend, request) = backend_and_request(&root, &libkrunfw, FORGING, "output-forging");
    backend
        .start(&request, leyline_runtime::ServedWorkspaces::none())
        .expect("the guest starts");
    let status = backend
        .wait_for_completion(&request.run_id)
        .expect("the run completes");
    let BackendRunStatus::Failed(error) = status else {
        panic!("a guest exiting 3 must fail the run, got {status:?}");
    };
    assert_eq!(
        (error.code, error.retryable),
        (leyline_runtime::ErrorCode::BackendFailed, false),
        "SECURITY: the guest classified its own failure: {error:?}"
    );
    assert!(
        error
            .detail
            .starts_with("libkrun worker exited with exit status: 3"),
        "the supervisor's observation leads the detail: {}",
        error.detail
    );
}

/// A serving guest's announcement is readable while it runs, by event, and
/// nothing is kept once the run is cancelled.
#[test]
fn a_serving_guest_announcement_is_readable_while_it_runs() {
    let _serial = common::serial();
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    let fixture = TempDir::new().expect("fixture");
    let root = fixture.path().canonicalize().expect("canonical fixture");
    let (backend, request) = backend_and_request(&root, &libkrunfw, SERVING, "output-serving");
    backend
        .start(&request, leyline_runtime::ServedWorkspaces::none())
        .expect("the guest starts");
    let seen = backend
        .wait_for_output(
            &request.run_id,
            &|output: &GuestOutput| output.stdout_tail.contains("listening on 3306"),
            BOUND,
        )
        .expect("an active run");
    assert!(
        seen.stdout_tail.contains("output-probe: listening on 3306"),
        "the serving guest's announcement never arrived: {seen:?}"
    );
    assert!(
        Backend::output(&backend, &request.run_id)
            .is_some_and(|output| output.stdout_tail.contains("listening on 3306"))
    );
    assert!(backend.cancel(&request.run_id).expect("cancel"));
    assert_eq!(Backend::output(&backend, &request.run_id), None);
}
