//! Every guest has a writable, bounded `/tmp`, whatever its rootfs holds
//! (bead `ley-line-open-db5899`).
//!
//! libkrun's guest init mounts a tmpfs on `/tmp` before the workload starts
//! (LLO patch `0012`): mode 1777, `nosuid`, `nodev`, sized at
//! `TMP_SIZE_PERCENT` (25) of the guest's RAM. The rootfs here has no `tmp/`
//! entry at all, as an OCI import of an image whose `tmp/` is empty does
//! not.
//!
//! Synchronization is by the guest's exit: the worker exits with it, and its
//! one report line is read from the console it wrote.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use leyline_runtime::backends::libkrun::worker::inherit_only;
use leyline_runtime::{DigestRef, ExecutionRequest, ResourceLimits};
use tempfile::TempDir;
use wait_timeout::ChildExt;

#[path = "../../runtime/tests/common/mod.rs"]
mod common;
#[path = "../../runtime/tests/conformance/mod.rs"]
#[allow(dead_code)]
mod conformance;
mod hypervisor;

/// The guest's RAM in this test, and the bound init's 25 % gives `/tmp`.
const GUEST_RAM: u64 = 256 << 20;
const TMP_BOUND: u64 = GUEST_RAM / 4;

/// Reports `/tmp`'s mount line, its mode, whether a small file writes and
/// reads back, then fills it in 1 MiB writes until a write fails and
/// reports how many bytes landed and the error's errno. It exits 0.
const PROBE: &str = r#"use std::io::Write;
use std::os::unix::fs::PermissionsExt;

fn main() {
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let line = mounts
        .lines()
        .find(|line| line.split_whitespace().nth(1) == Some("/tmp"))
        .unwrap_or("none")
        .to_owned();
    let mode = std::fs::metadata("/tmp")
        .map(|meta| format!("{:o}", meta.permissions().mode() & 0o7777))
        .unwrap_or_else(|error| format!("{:?}", error.kind()));
    let small = std::fs::write("/tmp/small", b"hello")
        .and_then(|()| std::fs::read("/tmp/small"))
        .map(|bytes| bytes == b"hello")
        .unwrap_or(false);
    let mut file = std::fs::File::create("/tmp/fill").unwrap();
    let chunk = vec![0x5au8; 1 << 20];
    let mut written: u64 = 0;
    let errno = loop {
        match file.write(&chunk) {
            Ok(0) => break 0,
            Ok(n) => written += n as u64,
            Err(error) => break error.raw_os_error().unwrap_or(-1),
        }
    };
    println!("tmp-probe: mount={line}");
    println!("tmp-probe: mode={mode} small={small} written={written} errno={errno}");
}
"#;

#[test]
fn every_guest_has_a_bounded_tmpfs_tmp() {
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
    let bytes = fs::read(&binary).expect("guest bytes");
    // One file, bin/probe: no tmp/ entry anywhere in the rootfs.
    let cas = root.join("cas");
    let rootfs = conformance::publish_rootfs(&cas, &[("bin/probe", &bytes, 0o755)]);
    assert!(!cas.join(&rootfs).join("tmp").exists());
    let run_root = root.join("run-root");
    fs::create_dir(&run_root).expect("run root");
    let worker = hypervisor::worker_under_test(
        Path::new(env!("CARGO_BIN_EXE_leyline-krun-worker")),
        &root.join("leyline-krun-worker"),
    );
    let request = ExecutionRequest {
        run_id: "live-guest-tmp".into(),
        replay_key: "live-guest-tmp-replay".into(),
        rootfs: DigestRef {
            algorithm: "blake3-256".into(),
            value: rootfs.clone(),
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
            memory: leyline_runtime::MemoryLimits { limit: GUEST_RAM },
            cpu: leyline_runtime::CpuLimits {
                quota: 100_000,
                period: 100_000,
            },
            ..ResourceLimits::default()
        },
    };
    let mut command = Command::new(&worker);
    inherit_only(&mut command, &[]).expect("list this process's descriptors");
    let (library_var, library_dir) = hypervisor::library_path_env(&libkrunfw);
    let mut child = command
        .arg("--cas-root")
        .arg(&cas)
        .arg("--run-root")
        .arg(&run_root)
        .arg("--runtime-file")
        .arg(&libkrunfw)
        .env(library_var, library_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn libkrun worker");
    serde_json::to_writer(child.stdin.as_mut().expect("worker stdin"), &request)
        .expect("write request");
    child.stdin.take().expect("close stdin").flush().ok();
    // Only a bound on a failure: the guest exits as soon as /tmp is full.
    if child
        .wait_timeout(Duration::from_secs(60))
        .expect("wait for worker")
        .is_none()
    {
        let _ = child.kill();
        panic!("the guest did not exit within 60 seconds");
    }
    let output = child.wait_with_output().expect("worker output");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "worker failed: {stderr}\n{stdout}");
    let report: Vec<&str> = stdout
        .lines()
        .filter_map(|line| line.split_once("tmp-probe: ").map(|(_, rest)| rest))
        .collect();
    let [mount, state] = report.as_slice() else {
        panic!("expected two report lines, got {report:?}: {stdout}");
    };
    let mount: Vec<&str> = mount
        .trim_start_matches("mount=")
        .split_whitespace()
        .collect();
    assert_eq!(
        mount.get(2),
        Some(&"tmpfs"),
        "/tmp is not a tmpfs: {mount:?}"
    );
    let options: Vec<&str> = mount
        .get(3)
        .copied()
        .unwrap_or_default()
        .split(',')
        .collect();
    for option in ["rw", "nosuid", "nodev"] {
        assert!(
            options.contains(&option),
            "/tmp lacks {option}: {options:?}"
        );
    }
    let fields: BTreeMap<&str, &str> = state
        .split_whitespace()
        .filter_map(|field| field.split_once('='))
        .collect();
    assert_eq!(fields.get("mode"), Some(&"1777"), "{state}");
    assert_eq!(fields.get("small"), Some(&"true"), "{state}");
    // ENOSPC is 28 on every Linux architecture.
    assert_eq!(
        fields.get("errno"),
        Some(&"28"),
        "a write past the bound: {state}"
    );
    let written: u64 = fields["written"].parse().expect("written bytes");
    assert!(
        // The kernel sizes the tmpfs from the RAM it manages, a little
        // under what the VM was given.
        written <= TMP_BOUND && written + (16 << 20) >= TMP_BOUND,
        "/tmp took {written} bytes; the bound is {TMP_BOUND} (25% of {GUEST_RAM})"
    );
    // Nothing the guest wrote to /tmp reached the host: the rootfs volume
    // holds at most the empty mount point, and the CAS is untouched.
    let host_tmp = run_root.join("rootfs/tmp");
    let left: Vec<_> = fs::read_dir(&host_tmp)
        .map(|entries| entries.flatten().map(|entry| entry.file_name()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "/tmp's files reached the host: {left:?}");
    assert!(!cas.join(&rootfs).join("tmp").exists());
}
