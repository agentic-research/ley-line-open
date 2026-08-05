use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use leyline_runtime::backends::libkrun::worker::WorkerEvent;
use leyline_runtime::{DigestRef, ExecutionRequest, ResourceLimits};
use tempfile::TempDir;
use wait_timeout::ChildExt;

/// Resolve the hypervisor this test needs, or report that it did not run.
///
/// Previously the test carried a bare `#[ignore]`, which meant it ran NOWHERE —
/// not in CI, which lacks libkrun, and not on a developer machine that has it,
/// because running it required remembering `-- --ignored`. A gate that no
/// environment satisfies produces no signal anywhere, and this is the tier
/// whose confinement drift check turned out to be entirely untested
/// (`ley-line-open-17536d`) precisely because everything here was skipped by
/// construction.
///
/// So the gate is now the actual precondition rather than a blanket opt-out: it
/// runs wherever `LEYLINE_LIBKRUN_PATH` and `LEYLINE_LIBKRUNFW_PATH` are set,
/// and says so loudly when they are not. The message is deliberately worded as
/// NOT a pass — the same stance `mutants_diff.sh` takes about a skipped gate,
/// for the same reason: a green tick that tested nothing is worse than a red
/// one, because nobody investigates it.
fn hypervisor_or_skip() -> Option<(std::ffi::OsString, std::path::PathBuf)> {
    let (Some(libkrun), Some(libkrunfw)) = (
        std::env::var_os("LEYLINE_LIBKRUN_PATH"),
        std::env::var_os("LEYLINE_LIBKRUNFW_PATH"),
    ) else {
        // A soft skip is still a green tick, and `cargo test` captures this
        // message unless someone passes `--nocapture` — so on its own it is a
        // quieter version of the same false green the bare `#[ignore]` was.
        // `LEYLINE_REQUIRE_HYPERVISOR` is the escape: any environment that
        // believes it can run this asserts so, and absence becomes a hard
        // failure there instead of a silent pass. A runner that gains libkrun
        // and forgets to set it degrades to "skipped", not to "wrong".
        assert!(
            std::env::var_os("LEYLINE_REQUIRE_HYPERVISOR").is_none(),
            "LEYLINE_REQUIRE_HYPERVISOR is set, so this environment claims a \
             hypervisor, but LEYLINE_LIBKRUN_PATH / LEYLINE_LIBKRUNFW_PATH are \
             not both set. Refusing to report a pass for a microVM test that \
             did not boot a microVM."
        );
        eprintln!(
            "SKIPPED (NOT a pass): no hypervisor. This test exercises a real \
             libkrun guest and needs LEYLINE_LIBKRUN_PATH + \
             LEYLINE_LIBKRUNFW_PATH, plus the aarch64-unknown-linux-musl \
             target. Nothing about the microVM backend was verified by this \
             run. Set LEYLINE_REQUIRE_HYPERVISOR to make this absence fail."
        );
        return None;
    };
    let libkrunfw = std::path::PathBuf::from(libkrunfw)
        .canonicalize()
        .expect("canonical LEYLINE_LIBKRUNFW_PATH");
    Some((libkrun, libkrunfw))
}

#[test]
fn guest_writes_the_ephemeral_root_without_mutating_cas() {
    let Some((libkrun, libkrunfw)) = hypervisor_or_skip() else {
        return;
    };
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
        confinement_digest: String::new(),
        confinement_manifest: None,
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

    if child
        .wait_timeout(Duration::from_secs(15))
        .expect("wait for worker")
        .is_none()
    {
        let _ = child.kill();
        panic!("libkrun guest did not exit within 15 seconds");
    }
    let output = child.wait_with_output().expect("worker output");
    let stderr = String::from_utf8(output.stderr).expect("worker stderr UTF-8");
    let event: WorkerEvent = stderr
        .lines()
        .find_map(|line| serde_json::from_str(line).ok())
        .unwrap_or_else(|| panic!("missing readiness JSON in worker stderr: {stderr}"));
    // Matched rather than compared for equality: the attested confinement
    // digest is derived from the resolved rootfs path, which is a temp dir
    // this test cannot predict. What it can assert — and what matters — is
    // that a live worker attests a real policy rather than an empty string.
    match event {
        WorkerEvent::Ready {
            run_id,
            confinement_digest,
        } => {
            assert_eq!(run_id, "live-guest-write", "worker stderr: {stderr}");
            assert!(
                confinement_digest.starts_with("blake3-256:"),
                "a live worker must attest the policy it applied, got \
                 {confinement_digest:?} — worker stderr: {stderr}"
            );
        }
        other => panic!("expected readiness, got {other:?} — worker stderr: {stderr}"),
    }
    assert!(output.status.success(), "worker failed: {stderr}");
    assert_eq!(
        fs::read(run_root.join("rootfs/guest-write")).expect("guest output"),
        b"written inside libkrun guest\n"
    );
    assert!(!immutable_root.join("guest-write").exists());
}
