//! A rootfs manifest v2 symbolic link is a real link in the guest (bead
//! `ley-line-open-4f08ea`; closes `ley-line-open-18a7a2`).
//!
//! Manifest v1 could not express a link, so `import-oci` dropped
//! `etc/mtab -> /proc/mounts` and the guest's `open("/etc/mtab")` failed
//! `ENOENT`. Under v2 the link's target bytes are authenticated, never
//! followed on the host, and recreated in the per-run copy, so the guest's
//! kernel follows it into its own procfs. The executable is reached through
//! a link too (`bin -> usr/bin`, as a merged-usr image has it).
//!
//! Synchronization is by the guest's exit: the worker exits with it, and its
//! report lines are read from the console it wrote.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use leyline_runtime::artifacts::rootfs_manifest::{FileSpec, RootfsSpec, cas_directory_mode};
use leyline_runtime::backends::libkrun::worker::inherit_only;
use leyline_runtime::{DigestRef, ExecutionRequest, ResourceLimits};
use tempfile::TempDir;
use wait_timeout::ChildExt;

#[path = "../../runtime/tests/common/mod.rs"]
mod common;
mod hypervisor;

/// Reports what the guest sees at /etc/mtab and its directory's mode, then
/// exits 0.
const PROBE: &str = r#"use std::os::unix::fs::PermissionsExt;

fn main() {
    let link = std::fs::read_link("/etc/mtab")
        .map(|target| target.display().to_string())
        .unwrap_or_else(|error| format!("{:?}", error.kind()));
    let opened = std::fs::read_to_string("/etc/mtab")
        .map(|text| text.lines().any(|line| line.split_whitespace().nth(1) == Some("/proc")))
        .unwrap_or(false);
    let open_error = std::fs::File::open("/etc/mtab")
        .err()
        .map(|error| format!("{:?}", error.kind()))
        .unwrap_or_else(|| "none".into());
    let spool = std::fs::metadata("/var/spool")
        .map(|meta| format!("{:o}", meta.permissions().mode() & 0o7777))
        .unwrap_or_else(|error| format!("{:?}", error.kind()));
    let empty = std::fs::read_dir("/var/empty").map(|entries| entries.count()).unwrap_or(99);
    println!("link-probe: link={link} opened={opened} open_error={open_error} spool={spool} empty={empty}");
}
"#;

/// Write `spec`'s tree beneath `cas` with `files`' bytes, and the canonical
/// manifest; return the digest.
fn publish_v2(cas: &Path, spec: &RootfsSpec, files: &BTreeMap<&str, Vec<u8>>) -> String {
    let manifest = spec.to_canonical_v2();
    let digest = blake3::hash(&manifest).to_hex().to_string();
    let root = cas.join(&digest);
    fs::create_dir_all(&root).expect("rootfs root");
    for path in spec.directories.keys().filter(|path| !path.is_empty()) {
        fs::create_dir_all(root.join(path)).expect("rootfs directory");
    }
    for (path, file) in &spec.files {
        let target = root.join(path);
        fs::write(&target, &files[path.as_str()]).expect("rootfs file");
        fs::set_permissions(&target, fs::Permissions::from_mode(file.mode)).expect("file mode");
    }
    for (path, target) in &spec.symlinks {
        std::os::unix::fs::symlink(target, root.join(path)).expect("rootfs link");
    }
    // Deepest first. A CAS entry holds each directory at
    // `cas_directory_mode` of its declared mode; the run's copy gets the
    // declared one (`var/spool` 1777 is asserted in the guest).
    for (path, mode) in spec.directories.iter().rev() {
        fs::set_permissions(
            root.join(path),
            fs::Permissions::from_mode(cas_directory_mode(*mode)),
        )
        .expect("directory mode");
    }
    fs::write(root.join("rootfs.manifest.json"), &manifest).expect("rootfs manifest");
    digest
}

#[test]
fn a_v2_link_to_proc_mounts_opens_in_the_guest() {
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

    let mut spec = RootfsSpec::empty_v2();
    for (path, mode) in [
        ("etc", 0o755),
        ("usr", 0o755),
        ("usr/bin", 0o755),
        ("var", 0o755),
        ("var/empty", 0o700),
        ("var/spool", 0o1777),
    ] {
        spec.directories.insert(path.into(), mode);
    }
    spec.files.insert(
        "usr/bin/probe".into(),
        FileSpec {
            mode: 0o755,
            blake3: blake3::hash(&probe).to_hex().to_string(),
        },
    );
    spec.symlinks.insert("etc/mtab".into(), "/proc/mounts".into());
    spec.symlinks.insert("bin".into(), "usr/bin".into());
    let cas = root.join("cas");
    let rootfs = publish_v2(&cas, &spec, &BTreeMap::from([("usr/bin/probe", probe)]));
    let run_root = root.join("run-root");
    fs::create_dir(&run_root).expect("run root");

    let worker = hypervisor::worker_under_test(
        Path::new(env!("CARGO_BIN_EXE_leyline-krun-worker")),
        &root.join("leyline-krun-worker"),
    );
    let request = ExecutionRequest {
        run_id: "live-guest-symlink".into(),
        replay_key: "live-guest-symlink-replay".into(),
        rootfs: DigestRef {
            algorithm: "blake3-256".into(),
            value: rootfs.clone(),
        },
        // Through the `bin -> usr/bin` link, as the guest's kernel reaches it.
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
    let report = stdout
        .lines()
        .find_map(|line| line.split_once("link-probe: ").map(|(_, rest)| rest))
        .unwrap_or_else(|| panic!("no report: {stdout}"));
    let fields: BTreeMap<&str, &str> = report
        .split_whitespace()
        .filter_map(|field| field.split_once('='))
        .collect();
    assert_eq!(fields.get("link"), Some(&"/proc/mounts"), "{report}");
    assert_eq!(fields.get("opened"), Some(&"true"), "open(\"/etc/mtab\"): {report}");
    assert_eq!(fields.get("open_error"), Some(&"none"), "{report}");
    // Directory modes and an empty directory reach the guest as declared.
    assert_eq!(fields.get("spool"), Some(&"1777"), "{report}");
    assert_eq!(fields.get("empty"), Some(&"0"), "{report}");
    // The per-run copy holds the link as a link, never followed on the host.
    let copied = run_root.join("rootfs/etc/mtab");
    assert_eq!(
        fs::read_link(&copied).expect("the copy holds a link").to_str(),
        Some("/proc/mounts")
    );
}
