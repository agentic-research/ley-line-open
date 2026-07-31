use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use leyline_runtime::backends::libkrun::worker::WorkerEvent;
use leyline_runtime::{DigestRef, ExecutionRequest, ResourceLimits};
use tempfile::TempDir;

#[test]
#[ignore = "requires macOS/ARM64 with libkrun, libkrunfw, and the Linux musl Rust target"]
fn guest_writes_the_ephemeral_root_without_mutating_cas() {
    let libkrun = std::env::var_os("LEYLINE_LIBKRUN_PATH").expect("LEYLINE_LIBKRUN_PATH");
    let libkrunfw = std::env::var_os("LEYLINE_LIBKRUNFW_PATH").expect("LEYLINE_LIBKRUNFW_PATH");
    let fixture = TempDir::new().expect("fixture");
    let staging = fixture.path().join("staging");
    let staging_bin = staging.join("bin");
    fs::create_dir_all(&staging_bin).expect("staging bin");
    let guest_source = fixture.path().join("guest-write.rs");
    fs::write(
        &guest_source,
        r#"fn main() {
    std::fs::write("/guest-write", b"written inside libkrun guest\n").unwrap();
}"#,
    )
    .expect("guest source");
    let guest_binary = staging_bin.join("guest-write");
    let compile = Command::new("rustc")
        .args([
            "--target",
            "aarch64-unknown-linux-musl",
            "-C",
            "target-feature=+crt-static",
            "-C",
            "linker=rust-lld",
            "-C",
            "strip=symbols",
            "-o",
        ])
        .arg(&guest_binary)
        .arg(&guest_source)
        .status()
        .expect("compile static Linux guest");
    assert!(compile.success(), "guest compilation failed: {compile}");
    fs::set_permissions(&guest_binary, fs::Permissions::from_mode(0o755)).expect("guest mode");

    let guest_bytes = fs::read(&guest_binary).expect("guest bytes");
    let guest_digest = blake3::hash(&guest_bytes).to_hex().to_string();
    let manifest = format!(
        "{{\"version\":1,\"files\":[{{\"path\":\"bin/guest-write\",\"mode\":493,\"blake3\":\"{guest_digest}\"}}]}}"
    );
    let root_digest = blake3::hash(manifest.as_bytes()).to_hex().to_string();
    let cas = fixture.path().join("cas");
    let immutable_root = cas.join(&root_digest);
    fs::create_dir_all(immutable_root.join("bin")).expect("CAS rootfs");
    fs::copy(&guest_binary, immutable_root.join("bin/guest-write")).expect("install guest");
    fs::set_permissions(
        immutable_root.join("bin/guest-write"),
        fs::Permissions::from_mode(0o755),
    )
    .expect("CAS guest mode");
    fs::write(immutable_root.join("rootfs.manifest.json"), manifest).expect("manifest");
    let run_root = fixture.path().join("run-root");
    fs::create_dir(&run_root).expect("run root");
    let signed_worker = fixture.path().join("leyline-krun-worker");
    fs::copy(env!("CARGO_BIN_EXE_leyline-krun-worker"), &signed_worker)
        .expect("copy worker for ad-hoc signing");
    let entitlements = fixture.path().join("libkrun-worker-entitlements.plist");
    fs::write(
        &entitlements,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>com.apple.security.hypervisor</key><true/>
</dict></plist>
"#,
    )
    .expect("write test entitlements");
    let sign = Command::new("codesign")
        .args(["--force", "--sign", "-", "--entitlements"])
        .arg(&entitlements)
        .arg(&signed_worker)
        .status()
        .expect("ad-hoc sign libkrun worker");
    assert!(sign.success(), "worker codesign failed: {sign}");

    let request = ExecutionRequest {
        run_id: "live-guest-write".into(),
        replay_key: "live-guest-write-replay".into(),
        rootfs: DigestRef {
            algorithm: "blake3-256".into(),
            value: root_digest,
        },
        executable: "bin/guest-write".into(),
        arguments: vec!["guest-write".into()],
        public_environment: BTreeMap::new(),
        allowed_egress: Vec::new(),
        limits: ResourceLimits {
            vcpus: 1,
            memory_mib: 256,
            wall_time_ms: 10_000,
        },
    };
    let mut child = Command::new(&signed_worker)
        .arg("--cas-root")
        .arg(&cas)
        .arg("--libkrun")
        .arg(&libkrun)
        .arg("--run-root")
        .arg(&run_root)
        .arg("--runtime-file")
        .arg(&libkrunfw)
        .env(
            "DYLD_LIBRARY_PATH",
            std::path::Path::new(&libkrunfw)
                .parent()
                .expect("libkrunfw parent"),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn libkrun worker");
    serde_json::to_writer(child.stdin.as_mut().expect("worker stdin"), &request)
        .expect("write request");
    child.stdin.take().expect("close stdin").flush().ok();

    let deadline = Instant::now() + Duration::from_secs(15);
    let (_delay_tx, delay_rx) = mpsc::channel::<()>();
    loop {
        if child.try_wait().expect("poll worker").is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("libkrun guest did not exit within 15 seconds");
        }
        let _ = delay_rx.recv_timeout(Duration::from_millis(10));
    }
    let output = child.wait_with_output().expect("worker output");
    let stderr = String::from_utf8(output.stderr).expect("worker stderr UTF-8");
    let event: WorkerEvent = stderr
        .lines()
        .find_map(|line| serde_json::from_str(line).ok())
        .unwrap_or_else(|| panic!("missing readiness JSON in worker stderr: {stderr}"));
    assert_eq!(
        event,
        WorkerEvent::Ready {
            run_id: "live-guest-write".into()
        },
        "worker stderr: {stderr}"
    );
    assert!(output.status.success(), "worker failed: {stderr}");
    assert_eq!(
        fs::read(run_root.join("guest-write")).expect("guest output"),
        b"written inside libkrun guest\n"
    );
    assert!(!immutable_root.join("guest-write").exists());
}
