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
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use leyline_runtime::{DigestRef, ExecutionRequest, ResourceLimits};
use tempfile::TempDir;

#[path = "../../runtime/tests/common/mod.rs"]
mod common;
mod hypervisor;

/// The port the guest binds. Above 1024 so `confinement/v1` §4 would permit
/// declaring it, and not a port anything else here uses.
const GUEST_PORT: u16 = 17536;

/// The prefix of the guest's one console line: `bound`, or `bind-failed: …`.
const REPORT: &str = "listener-probe: ";

/// How long the guest may take to boot and report. Only a bound on a failure:
/// a guest that reports is probed at once, and one that does not fails here.
const REPORT_BOUND: Duration = Duration::from_secs(60);

/// Whether a TCP connection to `127.0.0.1:port` succeeds. A refusal is the
/// answer "no"; anything else (a timeout, an unreachable route) means the probe
/// learned nothing, which fails the test rather than reading as "no".
fn probe(port: u16) -> bool {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    match TcpStream::connect_timeout(&address, Duration::from_secs(5)) {
        Ok(stream) => {
            let _ = stream.shutdown(Shutdown::Both);
            true
        }
        Err(error) if error.kind() == ErrorKind::ConnectionRefused => false,
        Err(error) => panic!("the probe of {address} was inconclusive: {error}"),
    }
}

#[test]
fn a_guest_listener_nothing_declared_is_not_reachable_from_the_host() {
    let _serial = common::serial();
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
    let _serial = common::serial();
    probe_listener_exposure(true);
}

fn probe_listener_exposure(hijack_inet: bool) {
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    let fixture = TempDir::new().expect("fixture");

    // The guest reports on its console whether the bind itself succeeded,
    // because "unreachable" has two very different causes: the port map refused
    // to expose a live listener, or the guest had no network stack to bind on.
    // Only the report distinguishes them, and reporting the first when it was
    // the second would be a false all-clear. The report is printed after
    // `listen` returns, so it is also the event the host's probe waits for.
    // The guest then holds the listener until the host ends the VM.
    let guest_source = fixture.path().join("guest-listen.rs");
    fs::write(
        &guest_source,
        format!(
            r#"use std::io::Write;
fn main() {{
    // Printed before the bind, so a run that stalls says whether the guest
    // started at all.
    println!("listener-probe-start");
    std::io::stdout().flush().unwrap();
    let listener = match std::net::TcpListener::bind(("0.0.0.0", {GUEST_PORT})) {{
        Ok(listener) => listener,
        Err(error) => {{
            println!("{REPORT}bind-failed: {{error}}");
            return;
        }}
    }};
    println!("{REPORT}bound");
    std::io::stdout().flush().unwrap();
    for stream in listener.incoming() {{
        if let Ok(mut stream) = stream {{
            let _ = stream.write_all(b"reached\n");
        }}
    }}
}}"#
        ),
    )
    .expect("guest source");

    let staging_bin = fixture.path().join("staging/bin");
    fs::create_dir_all(&staging_bin).expect("staging bin");
    let guest_binary = staging_bin.join("guest-listen");
    hypervisor::compile_guest(&guest_source, &guest_binary);
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

    let signed_worker = hypervisor::worker_under_test(
        std::path::Path::new(env!("CARGO_BIN_EXE_leyline-krun-worker")),
        &fixture.path().join("leyline-krun-worker"),
    );

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
        workspaces: Vec::new(),
        limits: ResourceLimits {
            wall_time_ms: 20_000,
            memory: leyline_runtime::MemoryLimits { limit: 256 << 20 },
            cpu: leyline_runtime::CpuLimits {
                quota: 100_000,
                period: 100_000,
            },
            ..ResourceLimits::default()
        },
    };

    // Controls before the VM exists. The probe must see a live host listener
    // (otherwise "unreachable" below could be a broken probe), and GUEST_PORT
    // must be free on the host (otherwise a host process, not the guest, would
    // answer the probe).
    let control = TcpListener::bind("127.0.0.1:0").expect("control listener");
    let control_port = control.local_addr().expect("control address").port();
    assert!(
        probe(control_port),
        "the probe did not reach a live host listener on port {control_port}"
    );
    drop(control);
    assert!(
        !probe(GUEST_PORT),
        "port {GUEST_PORT} already answers on the host before the VM starts; \
         free it, or this run cannot attribute a connection to the guest"
    );

    let mut worker = Command::new(&signed_worker);
    // Its standard streams and nothing else, as the backend spawns it.
    leyline_runtime::backends::libkrun::worker::inherit_only(&mut worker, &[])
        .expect("list this process's descriptors");
    if hijack_inet {
        worker.arg("--tsi-hijack-inet");
    }
    let mut child = worker
        .arg("--cas-root")
        .arg(&cas)
        .arg("--run-root")
        .arg(&run_root)
        .arg("--runtime-file")
        .arg(&libkrunfw)
        .env(
            hypervisor::library_path_env(&libkrunfw).0,
            hypervisor::library_path_env(&libkrunfw).1,
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn worker");
    serde_json::to_writer(child.stdin.as_mut().expect("stdin"), &request).expect("write request");
    child.stdin.take().expect("close stdin").flush().ok();

    // Readiness is emitted after the policy is applied and the VM prepared, but
    // before the guest runs, so it says nothing about a listening socket. The
    // probe waits for the guest's own report instead.
    let mut stderr = BufReader::new(child.stderr.take().expect("stderr"));
    let mut readiness = String::new();
    stderr.read_line(&mut readiness).expect("read readiness");
    assert!(
        readiness.contains("ready"),
        "worker did not report readiness: {readiness}"
    );
    // Drain the rest of stderr so the worker never blocks on a full pipe, and
    // keep it for the failure messages.
    let stderr_rest = std::thread::spawn(move || {
        let mut rest = String::new();
        let _ = stderr.read_to_string(&mut rest);
        rest
    });

    // The guest console is the worker's stdout. A reader thread forwards the
    // guest's report the moment it is printed; the bound below only turns a
    // guest that never reports into a failure.
    let (report_tx, report_rx) = mpsc::channel();
    let stdout = BufReader::new(child.stdout.take().expect("stdout"));
    std::thread::spawn(move || {
        let mut console = Vec::new();
        for line in stdout.lines() {
            let Ok(line) = line else { break };
            if let Some(report) = line
                .find(REPORT)
                .map(|at| line[at + REPORT.len()..].to_owned())
            {
                let _ = report_tx.send(Ok(report));
                return;
            }
            console.push(line);
        }
        let _ = report_tx.send(Err(console.join("\n")));
    });
    let report = match report_rx.recv_timeout(REPORT_BOUND) {
        Ok(Ok(report)) => report,
        Ok(Err(console)) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "the guest console closed without a bind report.\nconsole:\n{console}\n\
                 worker stderr:\n{}",
                stderr_rest.join().unwrap_or_default()
            );
        }
        Err(_) => {
            // Ending the worker closes the console, so the reader hands back
            // everything the guest printed before it stalled.
            let _ = child.kill();
            let _ = child.wait();
            let console = match report_rx.recv() {
                Ok(Err(console)) => console,
                Ok(Ok(report)) => format!("(report arrived after the bound: {report})"),
                Err(_) => "(console reader ended without output)".to_owned(),
            };
            panic!(
                "FAILED: no bind report from the guest within {REPORT_BOUND:?}.\nconsole:\n{console}\n\
                 worker stderr:\n{}",
                stderr_rest.join().unwrap_or_default()
            );
        }
    };
    eprintln!("guest bind outcome: {report}");
    if report != "bound" {
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "the guest could not bind at all, so this run measured nothing about \
             host exposure. Fix the guest or the VM config before trusting a \
             negative result here. Guest said: {report}"
        );
    }

    // The guest's listener is live and stays live: it serves until the VM is
    // ended. One probe decides; on loopback a port nothing exposes refuses at
    // once.
    let reached = probe(GUEST_PORT);
    eprintln!("host reached guest listener: {reached}");

    // The probe only means something if the guest still held its listener
    // while it ran: a guest that had exited would make any refusal vacuous.
    let still_running = child.try_wait().expect("poll worker").is_none();
    let _ = child.kill();
    let _ = child.wait();
    let worker_stderr = stderr_rest.join().unwrap_or_default();
    assert!(
        still_running,
        "the worker exited before the host's probe finished, so the guest's \
         listener was not held across it. worker stderr:\n{worker_stderr}"
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
