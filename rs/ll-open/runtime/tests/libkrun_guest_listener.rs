//! Does a guest listener reach the host when nothing declared it?
//!
//! ## Why (bead `ley-line-open-17536d`)
//!
//! `libkrun.h` documents `krun_set_port_map` this way:
//!
//! > Passing NULL (or not calling this function) as "port_map" has a different
//! > meaning than passing an empty array. The first one will instruct libkrun to
//! > attempt to expose all listening ports in the guest to the host, while the
//! > second means that no port from the guest will be exposed to host.
//!
//! LLO never calls it, which is the NULL case. Read alone, that says every port
//! a guest binds is reachable from the host — a fail-OPEN default, and the exact
//! inverse of `confinement/v1` §4, where an omitted `port` block means the
//! workload MUST NOT bind a listener at all.
//!
//! But LLO also calls `krun_disable_implicit_vsock`, which the same header says
//! "disables that behavior entirely - no vsock device will be created", and TSI
//! — the backend that would carry impersonated guest sockets — rides that vsock
//! device. So the header supports two readings, and which one holds is a
//! property of the running system rather than of the documentation.
//!
//! This test settles it by booting a guest that binds a port and having the host
//! try to reach it. The assertion is written as the property we want rather than
//! the behaviour we predict: an undeclared listener MUST NOT be reachable. If
//! that fails, the fail-open is real and `krun_set_port_map` needs an explicit
//! empty array. If it passes, the default is already closed and the empty array
//! is belt-and-braces — still worth adding, but not a security fix.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use leyline_runtime::{DigestRef, ExecutionRequest, ResourceLimits};
use tempfile::TempDir;

/// The port the guest binds. Above 1024 so `confinement/v1` §4 would permit
/// declaring it, and not a port anything else here uses.
const GUEST_PORT: u16 = 17536;

fn hypervisor_or_skip() -> Option<(std::ffi::OsString, std::path::PathBuf)> {
    let (Some(libkrun), Some(libkrunfw)) = (
        std::env::var_os("LEYLINE_LIBKRUN_PATH"),
        std::env::var_os("LEYLINE_LIBKRUNFW_PATH"),
    ) else {
        assert!(
            std::env::var_os("LEYLINE_REQUIRE_HYPERVISOR").is_none(),
            "LEYLINE_REQUIRE_HYPERVISOR is set but the libkrun paths are not; \
             refusing to report a pass for a microVM test that did not boot one."
        );
        eprintln!(
            "SKIPPED (NOT a pass): no hypervisor, so guest-listener exposure was \
             not measured. Set LEYLINE_LIBKRUN_PATH + LEYLINE_LIBKRUNFW_PATH."
        );
        return None;
    };
    let libkrunfw = std::path::PathBuf::from(libkrunfw)
        .canonicalize()
        .expect("canonical LEYLINE_LIBKRUNFW_PATH");
    Some((libkrun, libkrunfw))
}

#[test]
fn a_guest_listener_nothing_declared_is_not_reachable_from_the_host() {
    probe_listener_exposure(false);
}

/// The opt-in path, and the one that actually needs the empty port map.
///
/// With `--tsi-hijack-inet` the guest's `AF_INET` sockets ARE carried over
/// vsock, so an undeclared listener finally has a route to the host. What stops
/// it is `krun_set_port_map` called with an empty array — libkrun reads "never
/// called" as expose-every-listening-port, so the difference between this test
/// passing and failing is one unconditional call in `prepare_vm`.
///
/// Measured on the default path (`tsi_features: 0`), removing that call changes
/// nothing, because there is no INET path to govern. That is exactly why this
/// second probe exists: the call is only observable here, and a test that only
/// covered the default would let someone delete it as dead code.
#[test]
fn hijacked_guest_sockets_are_still_not_published_without_a_port_map() {
    probe_listener_exposure(true);
}

fn probe_listener_exposure(hijack_inet: bool) {
    let Some((libkrun, libkrunfw)) = hypervisor_or_skip() else {
        return;
    };
    let fixture = TempDir::new().expect("fixture");

    // The guest records whether the bind itself succeeded, because "unreachable"
    // has two very different causes: the port map refused to expose a live
    // listener, or the guest had no network stack to bind on. Only the marker
    // distinguishes them, and reporting the first when it was the second would
    // be a false all-clear.
    let guest_source = fixture.path().join("guest-listen.rs");
    fs::write(
        &guest_source,
        format!(
            r#"use std::io::Write;
fn main() {{
    let outcome = match std::net::TcpListener::bind(("0.0.0.0", {GUEST_PORT})) {{
        Ok(listener) => {{
            // Hold it open across the host's probe window.
            std::thread::spawn(move || {{
                for stream in listener.incoming() {{
                    if let Ok(mut stream) = stream {{
                        let _ = stream.write_all(b"reached\n");
                    }}
                }}
            }});
            "bound".to_owned()
        }}
        Err(error) => format!("bind-failed: {{error}}"),
    }};
    let mut marker = std::fs::File::create("/listener-probe").unwrap();
    marker.write_all(outcome.as_bytes()).unwrap();
    marker.sync_all().unwrap();
    std::thread::sleep(std::time::Duration::from_secs(6));
}}"#
        ),
    )
    .expect("guest source");

    let staging_bin = fixture.path().join("staging/bin");
    fs::create_dir_all(&staging_bin).expect("staging bin");
    let guest_binary = staging_bin.join("guest-listen");
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

    let guest_digest = blake3::hash(&fs::read(&guest_binary).expect("guest bytes"))
        .to_hex()
        .to_string();
    let manifest = format!(
        "{{\"version\":1,\"files\":[{{\"path\":\"bin/guest-listen\",\"mode\":493,\"blake3\":\"{guest_digest}\"}}]}}"
    );
    let root_digest = blake3::hash(manifest.as_bytes()).to_hex().to_string();
    let cas = fixture.path().join("cas");
    let immutable_root = cas.join(&root_digest);
    fs::create_dir_all(immutable_root.join("bin")).expect("CAS rootfs");
    fs::copy(&guest_binary, immutable_root.join("bin/guest-listen")).expect("install guest");
    fs::set_permissions(
        immutable_root.join("bin/guest-listen"),
        fs::Permissions::from_mode(0o755),
    )
    .expect("CAS guest mode");
    fs::write(immutable_root.join("rootfs.manifest.json"), manifest).expect("manifest");

    let run_root = fixture.path().join("run-root");
    fs::create_dir(&run_root).expect("run root");

    let signed_worker = fixture.path().join("leyline-krun-worker");
    fs::copy(env!("CARGO_BIN_EXE_leyline-krun-worker"), &signed_worker).expect("copy worker");
    let entitlements = fixture.path().join("entitlements.plist");
    fs::write(
        &entitlements,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>com.apple.security.hypervisor</key><true/>
</dict></plist>
"#,
    )
    .expect("entitlements");
    let sign = Command::new("codesign")
        .args(["--force", "--sign", "-", "--entitlements"])
        .arg(&entitlements)
        .arg(&signed_worker)
        .status()
        .expect("ad-hoc sign worker");
    assert!(sign.success(), "worker codesign failed: {sign}");

    let request = ExecutionRequest {
        run_id: format!("listener-probe-hijack-{hijack_inet}"),
        replay_key: format!("listener-probe-replay-{hijack_inet}"),
        rootfs: DigestRef {
            algorithm: "blake3-256".into(),
            value: root_digest,
        },
        executable: "bin/guest-listen".into(),
        arguments: vec!["guest-listen".into()],
        public_environment: std::collections::BTreeMap::new(),
        allowed_egress: Vec::new(),
        confinement_digest: String::new(),
        confinement_manifest: None,
        limits: ResourceLimits {
            vcpus: 1,
            memory_mib: 256,
            wall_time_ms: 20_000,
        },
    };

    let mut worker = Command::new(&signed_worker);
    if hijack_inet {
        worker.arg("--tsi-hijack-inet");
    }
    let mut child = worker
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
            libkrunfw.parent().expect("libkrunfw parent"),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn worker");
    serde_json::to_writer(child.stdin.as_mut().expect("stdin"), &request).expect("write request");
    child.stdin.take().expect("close stdin").flush().ok();

    // Readiness is emitted after the policy is applied and the VM prepared, but
    // before the guest runs — so it marks the start of the probe window, not a
    // listening socket. Hence the retry loop below rather than a single attempt.
    let mut stderr = BufReader::new(child.stderr.take().expect("stderr"));
    let mut readiness = String::new();
    stderr.read_line(&mut readiness).expect("read readiness");
    assert!(
        readiness.contains("ready"),
        "worker did not report readiness: {readiness}"
    );

    let address: SocketAddr = format!("127.0.0.1:{GUEST_PORT}")
        .parse()
        .expect("probe address");
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut reached = false;
    while Instant::now() < deadline {
        if let Ok(stream) = TcpStream::connect_timeout(&address, Duration::from_millis(250)) {
            let _ = stream.shutdown(Shutdown::Both);
            reached = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }

    let _ = child.kill();
    let _ = child.wait();

    // Read the guest's own record before judging. An unreachable port proves
    // nothing about the port map if the guest never managed to bind.
    let marker = fs::read_to_string(run_root.join("rootfs/listener-probe"))
        .unwrap_or_else(|error| format!("marker unreadable: {error}"));

    eprintln!("guest bind outcome: {marker}");
    eprintln!("host reached guest listener: {reached}");

    assert!(
        marker.starts_with("bound"),
        "the guest could not bind at all, so this run measured nothing about \
         host exposure. Fix the guest or the VM config before trusting a \
         negative result here. Guest said: {marker}"
    );
    assert!(
        !reached,
        "SECURITY: the host reached a guest listener on port {GUEST_PORT} that \
         no manifest declared (tsi_hijack_inet={hijack_inet}). confinement/v1 \
         §4 says an omitted `port` block means the workload MUST NOT bind a \
         listener, and libkrun's NULL port_map default is to expose every \
         listening guest port. prepare_vm must call krun_set_port_map with an \
         EMPTY array."
    );
}
