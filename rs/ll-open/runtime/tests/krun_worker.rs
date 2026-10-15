use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use leyline_runtime::backends::libkrun::worker::{
    InheritedDescriptor, WORKSPACE_SHARE_FD_ARG, WorkerEvent, WorkerOptions, declared_descriptors,
};
use leyline_runtime::{
    DigestRef, ErrorCode, ExecutionRequest, ResourceLimits, WorkerChannel, WorkspaceGrant,
    WorkspaceOperation,
};
use tempfile::TempDir;

mod common;

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
        confinement_manifest: None,
        workspaces: Vec::new(),
        limits: ResourceLimits {
            vcpus: 1,
            memory_mib: 512,
            wall_time_ms: 10_000,
        },
    }
}

#[test]
fn first_party_worker_never_falls_back_to_the_krunvm_cli() {
    let _serial = common::serial();
    // Catches a regression to Cloister's old shell-wrapper architecture. A
    // PATH trap makes this behavioral rather than a source-text assertion.
    //
    // The worker must fail before it reaches the VMM, on any host: a relative
    // `--runtime-file` is refused when the confinement manifest is built,
    // after the rootfs is materialized and before `krun_create_ctx`.
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
            "--run-root",
            run_root.path().to_str().expect("run root UTF-8"),
            "--runtime-file",
            "relative/libkrunfw",
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
    assert_eq!(error.code, ErrorCode::InvalidSpec);
    assert!(
        error.detail.contains("relative/libkrunfw"),
        "the refusal must name the path: {error:?}"
    );
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

fn base_options() -> Vec<OsString> {
    ["--cas-root", "/cas", "--run-root", "/run-root"]
        .map(OsString::from)
        .to_vec()
}

#[test]
fn worker_options_name_the_inherited_workspace_share() {
    let _serial = common::serial();
    let options = WorkerOptions::parse(base_options()).expect("no share");
    assert_eq!(options.workspace_share_fd, None);

    let mut arguments = base_options();
    arguments.extend([WORKSPACE_SHARE_FD_ARG, "7"].map(OsString::from));
    let options = WorkerOptions::parse(arguments.clone()).expect("a share");
    assert_eq!(options.workspace_share_fd, Some(7));

    arguments.extend([WORKSPACE_SHARE_FD_ARG, "8"].map(OsString::from));
    let error = WorkerOptions::parse(arguments).expect_err("one share");
    assert!(error.detail.contains(WORKSPACE_SHARE_FD_ARG), "{error:?}");

    for bad in ["seven", "-1", ""] {
        let mut arguments = base_options();
        arguments.extend([WORKSPACE_SHARE_FD_ARG, bad].map(OsString::from));
        let error = WorkerOptions::parse(arguments).expect_err("not a descriptor");
        assert_eq!(error.code, ErrorCode::InvalidSpec);
        assert!(error.detail.contains("descriptor"), "{bad}: {error:?}");
    }
}

/// Runs the worker binary on `request` with `extra` arguments and answers
/// its structured failure.
fn worker_failure(request: &ExecutionRequest, extra: &[&str]) -> leyline_runtime::ExecutionError {
    worker_failure_inheriting(request, extra, &[])
}

/// [`worker_failure`], with each of `inherited` kept open across the
/// worker's `exec` (every descriptor this process opens is close-on-exec).
fn worker_failure_inheriting(
    request: &ExecutionRequest,
    extra: &[&str],
    inherited: &[RawFd],
) -> leyline_runtime::ExecutionError {
    let run_root = TempDir::new().expect("run root");
    let mut command = Command::new(env!("CARGO_BIN_EXE_leyline-krun-worker"));
    let inherited = inherited.to_vec();
    // SAFETY: runs in the forked child before exec and makes only fcntl
    // calls, which are async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            for &fd in &inherited {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut child = command
        .args(["--cas-root", "/nonexistent-cas", "--run-root"])
        .arg(run_root.path())
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn first-party worker");
    serde_json::to_writer(child.stdin.as_mut().expect("worker stdin"), request)
        .expect("write worker request");
    child.stdin.take().expect("close worker stdin").flush().ok();
    let output = child.wait_with_output().expect("worker output");
    assert!(!output.status.success());
    let event: WorkerEvent = serde_json::from_slice(&output.stderr).expect("structured error");
    let WorkerEvent::Failed { error } = event else {
        panic!("expected failed worker event, got {event:?}");
    };
    error
}

fn grant(name: &str) -> WorkspaceGrant {
    WorkspaceGrant {
        name: name.into(),
        graph_root: format!("blake3-256:{}", "e".repeat(64)),
        expected_generation: 0,
        operations: vec![WorkspaceOperation::Read],
    }
}

/// The share a worker is handed must be the one its run is granted, and a
/// socket: each mismatch is refused before the rootfs is touched (the CAS
/// root here does not exist, so any later failure would name it instead).
#[test]
fn a_worker_refuses_a_share_that_does_not_match_its_grant() {
    let _serial = common::serial();
    let cas = TempDir::new().expect("CAS");
    let plain = request_fixture(&cas);
    let granted = ExecutionRequest {
        workspaces: vec![grant("repo")],
        ..plain.clone()
    };

    let error = worker_failure(&granted, &[]);
    assert_eq!(error.code, ErrorCode::InvalidSpec);
    assert!(error.detail.contains("\"repo\""), "{error:?}");

    let error = worker_failure(&plain, &[WORKSPACE_SHARE_FD_ARG, "0"]);
    assert_eq!(error.code, ErrorCode::InvalidSpec);
    assert!(error.detail.contains("no workspace"), "{error:?}");

    // Descriptor 0 is the worker's stdin, a pipe.
    let error = worker_failure(&granted, &[WORKSPACE_SHARE_FD_ARG, "0"]);
    assert_eq!(error.code, ErrorCode::InvalidSpec);
    assert!(error.detail.contains("not a socket"), "{error:?}");

    let error = worker_failure(&granted, &[WORKSPACE_SHARE_FD_ARG, "917"]);
    assert_eq!(error.code, ErrorCode::InvalidSpec);
    assert!(error.detail.contains("917"), "{error:?}");

    let two = ExecutionRequest {
        workspaces: vec![grant("repo"), grant("docs")],
        ..plain.clone()
    };
    let error = worker_failure(&two, &[WORKSPACE_SHARE_FD_ARG, "0"]);
    assert_eq!(error.code, ErrorCode::UnsupportedBackend);

    // The guest init's mount list is the worker's to set: a workload's
    // environment may not name it.
    let mut reserved = plain;
    reserved
        .public_environment
        .insert("KRUN_VIRTIOFS_MOUNTS".into(), "llo:/".into());
    let error = worker_failure(&reserved, &[]);
    assert_eq!(error.code, ErrorCode::InvalidSpec);
    assert!(error.detail.contains("KRUN_VIRTIOFS_MOUNTS"), "{error:?}");
}

/// The worker holds exactly the descriptors its run declares: the standard
/// streams, and the workspace share when the run is granted one. Any other
/// inherited descriptor is authority no manifest names, and the worker
/// refuses to start with it, before the rootfs is touched (the CAS root
/// here does not exist, so any later failure would name it instead).
#[test]
fn a_worker_holding_an_undeclared_descriptor_refuses_to_start() {
    let _serial = common::serial();
    let cas = TempDir::new().expect("CAS");
    let plain = request_fixture(&cas);
    let granted = ExecutionRequest {
        workspaces: vec![grant("repo")],
        ..plain.clone()
    };
    let stray = fs::File::open("/dev/null").expect("a stray descriptor");
    let stray_fd = stray.as_raw_fd();

    // A run without a workspace declares no descriptor beyond the streams.
    let error = worker_failure_inheriting(&plain, &[], &[stray_fd]);
    assert_eq!(error.code, ErrorCode::IdentityPolicyMismatch, "{error:?}");
    assert!(
        error
            .detail
            .contains(&format!("undeclared inherited descriptor(s) [{stray_fd}]")),
        "{error:?}"
    );

    // A run with a workspace declares its share, and nothing else.
    let (_daemon_end, worker_end) = UnixStream::pair().expect("share socket pair");
    let share_fd = worker_end.as_raw_fd().to_string();
    let error = worker_failure_inheriting(
        &granted,
        &[WORKSPACE_SHARE_FD_ARG, &share_fd],
        &[worker_end.as_raw_fd(), stray_fd],
    );
    assert_eq!(error.code, ErrorCode::IdentityPolicyMismatch, "{error:?}");
    assert!(
        error.detail.contains(&format!(
            "undeclared inherited descriptor(s) [{stray_fd}]: it declares its standard \
             streams and [{share_fd}]"
        )),
        "only the stray descriptor is undeclared: {error:?}"
    );

    // The declared share alone passes the check: the worker goes on to the
    // rootfs and fails there, on the CAS root this test withholds.
    let error = worker_failure_inheriting(
        &granted,
        &[WORKSPACE_SHARE_FD_ARG, &share_fd],
        &[worker_end.as_raw_fd()],
    );
    assert!(!error.detail.contains("undeclared"), "{error:?}");
    let error = worker_failure_inheriting(&plain, &[], &[]);
    assert!(!error.detail.contains("undeclared"), "{error:?}");
}

/// What a worker declares: nothing for a run without a workspace, and the
/// share (its descriptor, workspace and graph root) for a run granted one.
#[test]
fn a_run_declares_its_workspace_share_and_nothing_else() {
    let _serial = common::serial();
    let cas = TempDir::new().expect("CAS");
    let plain = request_fixture(&cas);
    assert_eq!(declared_descriptors(&plain, None), Vec::new());

    let granted = ExecutionRequest {
        workspaces: vec![grant("repo")],
        ..plain
    };
    assert_eq!(
        declared_descriptors(&granted, Some(9)),
        vec![InheritedDescriptor {
            descriptor: 9,
            channel: WorkerChannel::WorkspaceShare {
                workspace: "repo".into(),
                graph_root: format!("blake3-256:{}", "e".repeat(64)),
            },
        }]
    );
}

#[test]
fn worker_options_require_an_explicit_run_root() {
    let _serial = common::serial();
    let error = WorkerOptions::parse([OsString::from("--cas-root"), OsString::from("/cas")])
        .expect_err("run root is required");
    assert!(error.detail.contains("missing --run-root option"));

    let options = WorkerOptions::parse([
        OsString::from("--cas-root"),
        OsString::from("/cas"),
        OsString::from("--run-root"),
        OsString::from("/run-root"),
    ])
    .expect("explicit run root");
    assert_eq!(options.run_root, std::path::PathBuf::from("/run-root"));
}

/// libkrun is linked into the worker: there is no library path to pass, and
/// the option that used to carry one is refused rather than ignored.
#[test]
fn worker_options_refuse_a_libkrun_path() {
    let _serial = common::serial();
    let error = WorkerOptions::parse([
        OsString::from("--cas-root"),
        OsString::from("/cas"),
        OsString::from("--run-root"),
        OsString::from("/run-root"),
        OsString::from("--libkrun"),
        OsString::from("/libkrun"),
    ])
    .expect_err("--libkrun is not a worker option");
    assert!(error.detail.contains("--libkrun"), "{error:?}");
}

/// Each path option names one path: a second `--cas-root` or `--run-root` is
/// refused rather than silently replacing the first.
#[test]
fn worker_options_refuse_a_repeated_path_option() {
    let _serial = common::serial();
    for repeated in ["--cas-root", "--run-root"] {
        let error = WorkerOptions::parse([
            OsString::from("--cas-root"),
            OsString::from("/cas"),
            OsString::from("--run-root"),
            OsString::from("/run-root"),
            OsString::from(repeated),
            OsString::from("/again"),
        ])
        .expect_err("a repeated path option is refused");
        assert!(
            error.detail.contains("unknown or duplicate worker option")
                && error.detail.contains(repeated),
            "{repeated}: {error:?}"
        );
    }
}
