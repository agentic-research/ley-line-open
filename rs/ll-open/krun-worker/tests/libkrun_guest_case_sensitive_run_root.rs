//! A rootfs whose names differ only by case boots from a case-sensitive CAS
//! volume, its per-run copy made in the volume's own run root (bead
//! `ley-line-open-741388`).
//!
//! Debian's perl ships `usr/share/perl/Pod/` beside `usr/share/perl/pod/`.
//! `--cas-volume` gives such an image a case-sensitive CAS root, but each
//! run's rootfs is copied beneath the run root; on the host's default
//! (case-insensitive) APFS the second directory's creation failed "File
//! exists (os error 17)" and the run never started. The volume's run root
//! is on the same case-sensitive filesystem, so the copy keeps both trees
//! and the guest sees both.
//!
//! On macOS this provisions a real sparsebundle with `/usr/bin/hdiutil`
//! (detached before the test's directory is removed); on Linux the state
//! directory must be on a case-sensitive filesystem. It needs a hypervisor
//! (`LEYLINE_LIBKRUNFW_PATH`).

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use leyline_runtime::backends::libkrun::worker::inherit_only;
use leyline_runtime::cas_volume::{CasVolume, MIN_SIZE_CAP_BYTES, probe_sensitivity};
use leyline_runtime::{DigestRef, ExecutionRequest, ResourceLimits};
use tempfile::TempDir;
use wait_timeout::ChildExt;

#[path = "../../runtime/tests/common/mod.rs"]
mod common;
#[path = "../../runtime/tests/conformance/mod.rs"]
#[allow(dead_code)]
mod conformance;
mod hypervisor;

/// Prints each of the two files the image holds at names that differ only
/// by case, then exits 0.
const PROBE: &str = r#"fn main() {
    for path in ["/usr/share/perl/Pod/Usage.pm", "/usr/share/perl/pod/perlfunc.pod"] {
        let read = std::fs::read_to_string(path).unwrap_or_else(|error| format!("{error}"));
        println!("case-probe: {path}={}", read.trim());
    }
}
"#;

/// A provisioned volume, detached before its directory is removed.
struct Volume {
    volume: CasVolume,
    _state: TempDir,
}

impl Drop for Volume {
    fn drop(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if self.volume.detach().is_ok() {
                return;
            }
        }
        eprintln!("the test volume is still attached: detach it with hdiutil");
    }
}

/// Run `bin/probe` from `rootfs` in `cas`, its copy made beneath
/// `run_root`; the worker's exit status and output.
fn run(
    worker: &Path,
    libkrunfw: &Path,
    cas: &Path,
    rootfs: &str,
    run_root: &Path,
) -> std::process::Output {
    let request = ExecutionRequest {
        run_id: "live-guest-case-sensitive".into(),
        replay_key: "live-guest-case-sensitive-replay".into(),
        rootfs: DigestRef {
            algorithm: "blake3-256".into(),
            value: rootfs.into(),
        },
        executable: "bin/probe".into(),
        arguments: vec!["probe".into()],
        public_environment: BTreeMap::new(),
        allowed_egress: Vec::new(),
        confinement_digest: String::new(),
        confinement_manifest: None,
        workspaces: Vec::new(),
        limits: ResourceLimits {
            wall_time_ms: 60_000,
            memory: leyline_runtime::MemoryLimits { limit: 256 << 20 },
            cpu: leyline_runtime::CpuLimits {
                quota: 100_000,
                period: 100_000,
            },
            ..ResourceLimits::default()
        },
    };
    let mut command = Command::new(worker);
    inherit_only(&mut command, &[]).expect("list this process's descriptors");
    let (library_var, library_dir) = hypervisor::library_path_env(libkrunfw);
    let mut child = command
        .arg("--cas-root")
        .arg(cas)
        .arg("--run-root")
        .arg(run_root)
        .arg("--runtime-file")
        .arg(libkrunfw)
        .env(library_var, library_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn libkrun worker");
    serde_json::to_writer(child.stdin.as_mut().expect("worker stdin"), &request)
        .expect("write request");
    child.stdin.take().expect("close stdin").flush().ok();
    if child
        .wait_timeout(Duration::from_secs(60))
        .expect("wait for worker")
        .is_none()
    {
        let _ = child.kill();
        panic!("the guest did not exit within 60 seconds");
    }
    child.wait_with_output().expect("worker output")
}

#[test]
fn a_rootfs_with_names_differing_by_case_boots_from_the_volumes_run_root() {
    let _serial = common::serial();
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    let fixture = TempDir::new().expect("fixture");
    let root = fixture.path().canonicalize().expect("canonical fixture");
    let source = root.join("probe.rs");
    fs::write(&source, PROBE).expect("guest source");
    let binary = root.join("probe");
    hypervisor::compile_guest(&source, &binary);
    let probe = fs::read(&binary).expect("guest bytes");
    let worker = hypervisor::worker_under_test(
        Path::new(env!("CARGO_BIN_EXE_leyline-krun-worker")),
        &root.join("leyline-krun-worker"),
    );

    let state = TempDir::new().expect("state directory");
    let volume = Volume {
        volume: CasVolume::new(state.path(), MIN_SIZE_CAP_BYTES).expect("volume"),
        _state: state,
    };
    let provisioned = volume.volume.provision().expect("provision the CAS volume");
    let cas = provisioned.cas_root;
    let rootfs = conformance::publish_rootfs(
        &cas,
        &[
            ("bin/probe", &probe, 0o755),
            ("usr/share/perl/Pod/Usage.pm", b"package Pod::Usage;", 0o644),
            ("usr/share/perl/pod/perlfunc.pod", b"=head1 perlfunc", 0o644),
        ],
    );

    // Where the bead's failure came from: a run root on the host's own
    // filesystem. Where that is case-insensitive (default APFS), the copy
    // cannot hold both directories and the run never starts.
    let host_runs = root.join("host-runs");
    fs::create_dir(&host_runs).expect("host run root");
    if !probe_sensitivity(&host_runs).unwrap().case_sensitive {
        let output = run(&worker, &libkrunfw, &cas, &rootfs, &host_runs);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{stderr}");
        assert!(stderr.contains("File exists"), "{stderr}");
    }

    // The volume's own run root, made by provisioning: the guest sees both.
    let run_root = volume.volume.run_root();
    assert!(probe_sensitivity(&run_root).unwrap().case_sensitive);
    let this_run = run_root.join("live-guest-case-sensitive");
    fs::create_dir(&this_run).expect("run directory");
    let output = run(&worker, &libkrunfw, &cas, &rootfs, &this_run);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "worker failed: {stderr}\n{stdout}");
    let seen: Vec<&str> = stdout
        .lines()
        .filter_map(|line| line.split_once("case-probe: ").map(|(_, rest)| rest))
        .collect();
    assert_eq!(
        seen,
        [
            "/usr/share/perl/Pod/Usage.pm=package Pod::Usage;",
            "/usr/share/perl/pod/perlfunc.pod==head1 perlfunc",
        ],
        "{stdout}"
    );
}
