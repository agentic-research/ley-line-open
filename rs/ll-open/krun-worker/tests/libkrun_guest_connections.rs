//! Declared connections reach an unmodified guest workload as TCP on the
//! guest's loopback, both directions, and nothing undeclared does
//! (ADR-0046 Q7, bead `ley-line-open-be3711`).
//!
//! The contract is the Q7 problem statement on `ley-line-open-e2c5ce`:
//! invariants I1–I9 and acceptance tests 1–9. Each test below names the ones
//! it closes.
//!
//! ## The mechanism under test
//!
//! A confinement/v1 §6 grant carrying `guestPort` is presented on the
//! guest's loopback by LLO's guest init (libkrun patch 0010):
//!
//! - a `connect` grant (dial): init listens on `127.0.0.1:<guestPort>` before
//!   the workload starts and joins each connection to the grant's dial vsock
//!   port, which the VMM joins to the host socket the grant names;
//! - a `bind` grant (serve): a host peer connecting to the grant's socket is
//!   joined, through the serve vsock port, to the workload's listener on
//!   `127.0.0.1:<guestPort>`.
//!
//! The worker computes the list from the attested document alone
//! (`guest_loopback_channels`) and hands it to init in a variable a request
//! cannot set. Init also refuses every datagram, raw and packet socket.
//!
//! ## Synchronization
//!
//! By event, never by sleep. The guest prints one report line on its
//! console after it has done what the host waits for (dialed, or bound); the
//! host reads it. Timeouts below only bound a failure, turning a guest that
//! never reports into a failed test instead of a hung one. A run whose guest
//! prints nothing within the bound is reported as a boot stall (bead
//! `ley-line-open-c4480a`) rather than as a pass or a retry.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use leyline_runtime::authorization::AuthorizationPolicy;
use leyline_runtime::backends::libkrun::backend::{KrunWorkerBackend, KrunWorkerConfig};
use leyline_runtime::backends::libkrun::confinement::{
    LoopbackDirection, VSOCK_UNIX_BASE_PORT, confinement_manifest, guest_loopback_channels,
};
use leyline_runtime::confinement::{ConfinementManifest, UnixSocketGrant};
use leyline_runtime::{
    BackendClass, DigestRef, ExecutionRequest, ExecutionService, ResourceLimits, RunState,
};
use tempfile::TempDir;

#[path = "../../runtime/tests/common/mod.rs"]
mod common;
#[path = "../../runtime/tests/conformance/mod.rs"]
mod conformance;
mod hypervisor;

use conformance::{GrantSpec, GuestResolver, alive, group_alive, peer_pid, run_roots};

/// The prefix of a guest's one report line.
const REPORT: &str = "connections-probe: ";

/// Only a bound on a failure, as in `libkrun_guest_listener`.
const REPORT_BOUND: Duration = Duration::from_secs(60);

/// The guest port a dial presentation listens on, and the one a served
/// workload binds.
const DIAL_PORT: u16 = 13306;
const SERVE_PORT: u16 = 8080;

// ---------------------------------------------------------------------------
// Guest programs. CLIENT and SERVER are the unmodified-workload row: ordinary
// std::net sockets, an address from the environment or argv, no AF_VSOCK and
// nothing that knows it runs in a microVM. The probes are test instruments.

/// A stock TCP client. Each argument is `env:<NAME>` (dial the address in
/// that variable) or an address; for each it connects, sends
/// `hello-<label>`, and reports the reply line (spaces as `_`), `eof`, or the
/// error kind. Then it holds the VM until the host ends it.
const CLIENT: &str = r#"use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

fn main() {
    println!("connections-probe-start");
    let mut report = String::new();
    for target in std::env::args().skip(1) {
        let (label, address) = match target.strip_prefix("env:") {
            Some(name) => (name.to_owned(), std::env::var(name).unwrap_or_default()),
            None => (target.clone(), target.clone()),
        };
        let outcome = match TcpStream::connect(&address) {
            Ok(mut stream) => {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                let _ = writeln!(stream, "hello-{label}");
                let mut reply = String::new();
                match BufReader::new(&stream).read_line(&mut reply) {
                    Ok(0) => "eof".to_owned(),
                    Ok(_) => reply.trim_end().replace(' ', "_"),
                    Err(error) => format!("read-{:?}", error.kind()),
                }
            }
            Err(error) => format!("{:?}", error.kind()),
        };
        report.push_str(&format!(" {label}={outcome}"));
    }
    println!("connections-probe: dialed{report}");
    loop {
        std::thread::park();
    }
}
"#;

/// A stock TCP server: binds `$LISTEN_ADDR`, reports `listening`, and answers
/// one line per connection as `guest#<n>:<line>`, `n` counting connections.
const SERVER: &str = r#"use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

fn main() {
    println!("connections-probe-start");
    let address = std::env::var("LISTEN_ADDR").unwrap_or_default();
    let listener = match TcpListener::bind(&address) {
        Ok(listener) => listener,
        Err(error) => {
            println!("connections-probe: bind-failed {:?}", error.kind());
            loop {
                std::thread::park();
            }
        }
    };
    println!("connections-probe: listening {address}");
    let mut served = 0u32;
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        served += 1;
        let mut line = String::new();
        let _ = BufReader::new(&stream).read_line(&mut line);
        let _ = write!(stream, "guest#{served}:{line}");
    }
}
"#;

/// The negative sweep (acceptance test 3): every TCP port on the guest's
/// loopback, a set of host vsock ports, the routes off the guest, and its
/// interfaces. Reports `tcp=<accepting ports>`, `vsock=<accepting ports>`,
/// each remote address's outcome, and each interface as
/// `if:<name>:<operstate>:<flags>`.
const SWEEP: &str = r#"use std::ffi::c_int;
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

#[repr(C)]
struct SockaddrVm {
    svm_family: u16,
    svm_reserved1: u16,
    svm_port: u32,
    svm_cid: u32,
    svm_zero: [u8; 4],
}

extern "C" {
    fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    fn connect(fd: c_int, address: *const SockaddrVm, length: u32) -> c_int;
    fn close(fd: c_int) -> c_int;
}

fn vsock_accepts(port: u32) -> bool {
    let address = SockaddrVm { svm_family: 40, svm_reserved1: 0, svm_port: port, svm_cid: 2, svm_zero: [0; 4] };
    // SAFETY: a descriptor this process owns and a live address of the size passed.
    let fd = unsafe { socket(40, 1, 0) };
    if fd < 0 {
        return false;
    }
    let connected = unsafe { connect(fd, &address, std::mem::size_of::<SockaddrVm>() as u32) } == 0;
    unsafe { close(fd) };
    connected
}

fn main() {
    println!("connections-probe-start");
    let started = std::time::Instant::now();
    let mut tcp = Vec::new();
    for port in 1..=65535u16 {
        // A connect whose ephemeral source port happens to equal the
        // destination joins itself (TCP simultaneous open): no listener is
        // involved, so it is not counted.
        if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) {
            if stream.local_addr().ok() != stream.peer_addr().ok() {
                tcp.push(port.to_string());
            }
        }
    }
    println!("sweep: tcp done in {:?}", started.elapsed());
    // The VMM answers a dial to an unmapped vsock port with nothing at all,
    // so each such connect ends at the guest kernel's connect timeout (2 s,
    // measured); a mapped port answers in about a millisecond. The ports are
    // therefore dialed 256 at a time, each from its own thread.
    let base = 0x1_0000u32;
    let mut probed: Vec<u32> = (0..=1100).collect();
    probed.extend(base - 8..base + 64);
    probed.extend([0x7fff_ffff, u32::MAX - 1]);
    let mut vsock: Vec<String> = Vec::new();
    for chunk in probed.chunks(256) {
        let probes: Vec<_> = chunk
            .iter()
            .map(|&port| std::thread::spawn(move || (port, vsock_accepts(port))))
            .collect();
        for probe in probes {
            let (port, accepted) = probe.join().unwrap();
            if accepted {
                vsock.push(port.to_string());
            }
        }
    }
    println!("sweep: vsock done in {:?}", started.elapsed());
    let mut remote = String::new();
    for address in ["203.0.113.2:80", "1.1.1.1:443", "10.0.2.2:80", "[2001:db8::1]:80"] {
        let target: SocketAddr = address.parse().unwrap();
        let outcome = match TcpStream::connect_timeout(&target, Duration::from_secs(5)) {
            Ok(_) => "connected".to_owned(),
            Err(error) => format!("{:?}", error.kind()),
        };
        remote.push_str(&format!(" {address}={outcome}"));
    }
    let mut interfaces = String::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let read = |file: &str| {
                std::fs::read_to_string(entry.path().join(file))
                    .map(|text| text.trim().to_owned())
                    .unwrap_or_else(|_| "?".to_owned())
            };
            interfaces.push_str(&format!(" if:{name}:{}:{}", read("operstate"), read("flags")));
        }
    }
    println!(
        "connections-probe: swept tcp={} vsock={}{remote}{interfaces}",
        tcp.join(","),
        vsock.join(",")
    );
    loop {
        std::thread::park();
    }
}
"#;

/// The datagram probe: every way the guest might open a UDP-capable socket,
/// and two controls that must still work (a TCP listener, an AF_UNIX
/// datagram pair). Reports `<what>=ok|<errno>`.
const DATAGRAM: &str = r#"use std::ffi::{c_int, c_long};

extern "C" {
    fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    fn socketpair(domain: c_int, kind: c_int, protocol: c_int, fds: *mut c_int) -> c_int;
    fn syscall(number: c_long, ...) -> c_long;
    fn close(fd: c_int) -> c_int;
}

fn outcome(rc: i64) -> String {
    if rc >= 0 {
        "ok".to_owned()
    } else {
        format!("errno{}", std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    }
}

fn open(domain: c_int, kind: c_int, protocol: c_int) -> String {
    // SAFETY: a plain syscall; a descriptor it returns is closed at once.
    let fd = unsafe { socket(domain, kind, protocol) };
    let result = outcome(fd as i64);
    if fd >= 0 {
        unsafe { close(fd) };
    }
    result
}

fn main() {
    println!("connections-probe-start");
    const AF_UNIX: c_int = 1;
    const AF_INET: c_int = 2;
    const AF_INET6: c_int = 10;
    const AF_PACKET: c_int = 17;
    const SOCK_STREAM: c_int = 1;
    const SOCK_DGRAM: c_int = 2;
    const SOCK_RAW: c_int = 3;
    const SOCK_CLOEXEC: c_int = 0o2000000;
    let mut report = Vec::new();
    report.push(format!("std-udp4={}", match std::net::UdpSocket::bind("127.0.0.1:0") {
        Ok(_) => "ok".to_owned(),
        Err(error) => format!("{:?}", error.kind()),
    }));
    report.push(format!("std-udp6={}", match std::net::UdpSocket::bind("[::1]:0") {
        Ok(_) => "ok".to_owned(),
        Err(error) => format!("{:?}", error.kind()),
    }));
    report.push(format!("inet-dgram={}", open(AF_INET, SOCK_DGRAM, 0)));
    report.push(format!("inet-dgram-cloexec={}", open(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0)));
    report.push(format!("inet6-dgram={}", open(AF_INET6, SOCK_DGRAM, 0)));
    report.push(format!("inet-raw-udp={}", open(AF_INET, SOCK_RAW, 17)));
    report.push(format!("inet-stream-udp={}", open(AF_INET, SOCK_STREAM, 17)));
    report.push(format!("packet={}", open(AF_PACKET, SOCK_RAW, 0)));
    let mut params = [0u8; 120];
    // SAFETY: io_uring_setup(1, params) with a zeroed params block it may write.
    let ring = unsafe { syscall(425, 1 as c_long, params.as_mut_ptr()) };
    report.push(format!("io_uring={}", outcome(ring as i64)));
    if ring >= 0 {
        unsafe { close(ring as c_int) };
    }
    report.push(format!("control-tcp={}", open(AF_INET, SOCK_STREAM, 0)));
    let mut pair = [0 as c_int; 2];
    // SAFETY: an out-parameter of two descriptors.
    let rc = unsafe { socketpair(AF_UNIX, SOCK_DGRAM, 0, pair.as_mut_ptr()) };
    report.push(format!("control-unix-dgram={}", outcome(rc as i64)));
    println!("connections-probe: datagrams {}", report.join(" "));
    loop {
        std::thread::park();
    }
}
"#;

// ---------------------------------------------------------------------------
// Harness.

/// What the guest's report and the worker's readiness said.
struct Booted {
    guest: Guest,
    /// The text after [`REPORT`].
    report: String,
    /// The confinement digest the worker attested in its readiness event.
    digest: String,
}

/// One running worker and its guest.
struct Guest {
    child: Child,
    stderr: Option<JoinHandle<String>>,
}

impl Guest {
    fn end(&mut self) -> String {
        let _ = self.child.kill();
        let status = self.child.wait();
        let stderr = self
            .stderr
            .take()
            .map(|stderr| stderr.join().unwrap_or_default())
            .unwrap_or_default();
        format!("{stderr}(worker ended: {status:?})")
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// How a boot ended before the guest reported. Its fields are read through
/// `Debug`, in the panic that reports it.
#[derive(Debug)]
#[allow(dead_code)]
enum BootFailure {
    /// The worker refused the run before readiness: its failure event.
    Refused(String),
    /// The worker was ready, and the guest printed nothing at all within the
    /// bound: the known pre-guest boot stall (bead `ley-line-open-c4480a`).
    Stalled { stderr: String },
    /// The guest started and printed, but never reported.
    NoReport { console: String, stderr: String },
}

/// A run's inputs.
struct Run<'a> {
    root: &'a Path,
    libkrunfw: &'a Path,
    /// The guest program's source.
    source: &'a str,
    arguments: Vec<String>,
    environment: BTreeMap<String, String>,
    manifest: Option<&'a ConfinementManifest>,
    hijack: bool,
    run_id: &'a str,
}

impl<'a> Run<'a> {
    fn new(root: &'a Path, libkrunfw: &'a Path, source: &'a str, run_id: &'a str) -> Self {
        Self {
            root,
            libkrunfw,
            source,
            arguments: Vec::new(),
            environment: BTreeMap::new(),
            manifest: None,
            hijack: false,
            run_id,
        }
    }

    fn env(mut self, key: &str, value: &str) -> Self {
        self.environment.insert(key.into(), value.into());
        self
    }

    fn args(mut self, arguments: &[&str]) -> Self {
        self.arguments = arguments.iter().map(|value| (*value).to_owned()).collect();
        self
    }

    fn manifest(mut self, manifest: &'a ConfinementManifest) -> Self {
        self.manifest = Some(manifest);
        self
    }

    fn hijack(mut self) -> Self {
        self.hijack = true;
        self
    }

    /// Boot and wait for the guest's report, panicking on anything else.
    fn boot(self) -> Booted {
        match self.try_boot() {
            Ok(booted) => booted,
            Err(failure) => panic!("the guest did not report: {failure:#?}"),
        }
    }

    /// Boot, expecting the worker to refuse before readiness; returns its
    /// failure event.
    fn refused(self) -> String {
        match self.try_boot() {
            Err(BootFailure::Refused(event)) => event,
            Ok(mut booted) => {
                let stderr = booted.guest.end();
                panic!(
                    "the worker started a run it should have refused; guest said {}\n{stderr}",
                    booted.report
                )
            }
            Err(other) => panic!("expected a refusal before readiness, got {other:#?}"),
        }
    }

    fn try_boot(self) -> Result<Booted, BootFailure> {
        let staging = self.root.join(format!("{}-src", self.run_id));
        fs::create_dir_all(&staging).expect("staging");
        let source = staging.join("guest.rs");
        fs::write(&source, self.source).expect("guest source");
        let binary = staging.join("guest");
        hypervisor::compile_guest(&source, &binary);
        let bytes = fs::read(&binary).expect("guest bytes");
        let cas = self.root.join("cas");
        let rootfs = conformance::publish_rootfs(&cas, &[("bin/guest", &bytes, 0o755)]);
        let run_root = self.root.join(format!("{}-runs", self.run_id));
        fs::create_dir_all(&run_root).expect("run root");

        let worker = hypervisor::worker_under_test(
            Path::new(env!("CARGO_BIN_EXE_leyline-krun-worker")),
            &self.root.join(format!("{}-worker", self.run_id)),
        );
        let (confinement_digest, confinement_manifest) = match self.manifest {
            Some(document) => (
                document.confinement_digest().expect("digest"),
                Some(document.to_canonical_json().expect("canonical document")),
            ),
            None => (String::new(), None),
        };
        // libkrun passes these as argv[1..]; the guest's argv[0] is its path.
        let arguments = self.arguments;
        let request = ExecutionRequest {
            run_id: self.run_id.to_owned(),
            replay_key: format!("{}-replay", self.run_id),
            rootfs: DigestRef {
                algorithm: "blake3-256".into(),
                value: rootfs,
            },
            executable: "bin/guest".into(),
            arguments,
            public_environment: self.environment,
            allowed_egress: Vec::new(),
            confinement_digest,
            confinement_manifest,
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

        let mut command = Command::new(&worker);
        leyline_runtime::backends::libkrun::worker::inherit_only(&mut command, &[])
            .expect("list this process's descriptors");
        if self.hijack {
            command.arg("--tsi-hijack-inet");
        }
        let (library_var, library_dir) = hypervisor::library_path_env(self.libkrunfw);
        let mut child = command
            .arg("--cas-root")
            .arg(&cas)
            .arg("--run-root")
            .arg(&run_root)
            .arg("--runtime-file")
            .arg(self.libkrunfw)
            .env(library_var, library_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn worker");
        serde_json::to_writer(child.stdin.as_mut().expect("stdin"), &request)
            .expect("write request");
        child.stdin.take().expect("close stdin").flush().ok();

        let mut stderr = BufReader::new(child.stderr.take().expect("stderr"));
        let mut readiness = String::new();
        stderr.read_line(&mut readiness).expect("read readiness");
        if !readiness.contains("\"ready\"") {
            let _ = child.kill();
            let _ = child.wait();
            return Err(BootFailure::Refused(readiness));
        }
        let ready: serde_json::Value =
            serde_json::from_str(&readiness).expect("readiness is JSON");
        let digest = ready["confinement_digest"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let stderr = std::thread::spawn(move || {
            let mut rest = String::new();
            let _ = stderr.read_to_string(&mut rest);
            rest
        });

        let (report_tx, report_rx) = mpsc::channel();
        let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
        std::thread::spawn(move || {
            let mut console = Vec::new();
            let mut raw = Vec::new();
            loop {
                raw.clear();
                match stdout.read_until(b'\n', &mut raw) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let line = String::from_utf8_lossy(&raw).into_owned();
                        if let Some(at) = line.find(REPORT) {
                            let _ = report_tx
                                .send(Ok(line[at + REPORT.len()..].trim_end().to_owned()));
                            return;
                        }
                        console.push(line.trim_end().to_owned());
                    }
                }
            }
            let _ = report_tx.send(Err(console));
        });
        let mut guest = Guest {
            child,
            stderr: Some(stderr),
        };
        let console = match report_rx.recv_timeout(REPORT_BOUND) {
            Ok(Ok(report)) => {
                eprintln!("[{}] guest: {report}", self.run_id);
                return Ok(Booted {
                    guest,
                    report,
                    digest,
                });
            }
            Ok(Err(console)) => console,
            Err(_) => {
                let stderr = guest.end();
                return match report_rx.recv() {
                    Ok(Err(console)) if !console.is_empty() => {
                        Err(BootFailure::NoReport {
                            console: console.join("\n"),
                            stderr,
                        })
                    }
                    _ => Err(BootFailure::Stalled { stderr }),
                };
            }
        };
        let stderr = guest.end();
        Err(BootFailure::NoReport {
            console: console.join("\n"),
            stderr,
        })
    }
}

/// LLO's compiled policy for a worker granted only libkrunfw: the document a
/// grant must extend (the tier refuses a carried document that differs).
fn base_manifest(libkrunfw: &Path) -> ConfinementManifest {
    confinement_manifest(&[libkrunfw.to_path_buf()], &[], None).expect("the compiled policy")
}

fn with_grants(libkrunfw: &Path, grants: Vec<UnixSocketGrant>) -> ConfinementManifest {
    grants
        .into_iter()
        .try_fold(base_manifest(libkrunfw), |manifest, grant| {
            manifest.with_unix_socket(grant)
        })
        .expect("the run's confinement document")
}

fn dial_grant(path: &Path, port: u16) -> UnixSocketGrant {
    UnixSocketGrant::connect(path.display().to_string())
        .with_guest_port(port)
        .expect("a dial presentation")
}

fn serve_grant(path: &Path, port: u16) -> UnixSocketGrant {
    UnixSocketGrant::bind(path.display().to_string())
        .with_guest_port(port)
        .expect("a serve presentation")
}

/// The host peer of a dial channel: answers each connection's one line as
/// `host:<line>`, and sends every accepted stream's peer pid back.
fn answer_dials(listener: UnixListener) -> mpsc::Receiver<i32> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let _ = tx.send(peer_pid(&stream));
            let mut line = String::new();
            if BufReader::new(&stream).read_line(&mut line).is_ok() && !line.is_empty() {
                let _ = write!(stream, "host:{line}");
            }
        }
    });
    rx
}

/// Send one line on `stream` and read the one-line reply.
fn exchange(stream: &mut (impl Read + Write), line: &str) -> String {
    if let Err(error) = writeln!(stream, "{line}") {
        return format!("<send failed: {error}>");
    }
    let mut reply = String::new();
    match BufReader::new(stream).read_line(&mut reply) {
        Ok(0) => "<eof>".to_owned(),
        Ok(_) => reply.trim_end().to_owned(),
        Err(error) => format!("<no reply: {error}>"),
    }
}

fn connect_unix(path: &Path) -> std::io::Result<UnixStream> {
    let stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    Ok(stream)
}

/// What `lsof` lists for process `pid`'s sockets: `(type, name)` pairs, the
/// type being `unix`, `IPv4` or `IPv6`. Every open file is listed and the
/// sockets kept, so no selection flag can narrow what is seen.
fn sockets_of(pid: u32) -> Vec<(String, String)> {
    let output = Command::new("lsof")
        .args(["-nP", "-F", "tn", "-p"])
        .arg(pid.to_string())
        .output()
        .expect("run lsof");
    let mut sockets = Vec::new();
    let mut kind = String::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(value) = line.strip_prefix('t') {
            kind = value.to_owned();
        } else if let Some(value) = line.strip_prefix('n')
            && matches!(kind.as_str(), "unix" | "IPv4" | "IPv6" | "systm" | "key")
        {
            sockets.push((kind.clone(), value.to_owned()));
        }
    }
    sockets
}

/// Off macOS the presentation has not run live, and the tier refuses it by
/// name (acceptance test 8). The live tests below return early there; this
/// is the test that holds the refusal.
fn presentation_is_live_here() -> bool {
    cfg!(target_os = "macos")
}

/// Acceptance test 8: off macOS a declared presentation refuses the run
/// before readiness, naming the platform. On macOS the same decision is
/// checked as the pure function the worker calls.
#[test]
fn off_macos_the_presentation_is_refused_by_name() {
    let _serial = common::serial();
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    let (_fixture, root) = fixture();
    let socket = root.join("db.sock");
    let _peer = UnixListener::bind(&socket).expect("the declared peer");
    let manifest = with_grants(&libkrunfw, vec![dial_grant(&socket, DIAL_PORT)]);
    if presentation_is_live_here() {
        let error = guest_loopback_channels(&manifest, 0, "linux").unwrap_err();
        assert!(error.detail.contains("linux"), "{error}");
        return;
    }
    let refusal = Run::new(&root, &libkrunfw, CLIENT, "refuse-platform")
        .manifest(&manifest)
        .refused();
    assert!(
        refusal.contains(std::env::consts::OS),
        "the refusal must name the platform: {refusal}"
    );
}

fn fixture() -> (TempDir, PathBuf) {
    let fixture = TempDir::new().expect("fixture");
    let root = fixture.path().canonicalize().expect("canonical fixture");
    (fixture, root)
}

// ---------------------------------------------------------------------------
// Acceptance test 1 (dial-out) and the unmodified client.

/// A stock client dialing the address in its environment reaches exactly
/// its declared peer, through a channel the attested document presents;
/// with the connection removed from the document, the same client and the
/// same environment reach nothing, and the attested digest differs.
#[test]
fn a_dialed_connection_reaches_exactly_its_declared_peer() {
    let _serial = common::serial();
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    if !presentation_is_live_here() {
        return;
    }
    let (_fixture, root) = fixture();
    let socket = root.join("db.sock");
    let peers = answer_dials(UnixListener::bind(&socket).expect("the declared peer"));

    let declared = with_grants(&libkrunfw, vec![dial_grant(&socket, DIAL_PORT)]);
    let address = format!("127.0.0.1:{DIAL_PORT}");
    let mut booted = Run::new(&root, &libkrunfw, CLIENT, "dial-declared")
        .env("DB_ADDR", &address)
        .args(&["env:DB_ADDR"])
        .manifest(&declared)
        .boot();
    let vmm = peers.recv_timeout(REPORT_BOUND).ok();
    let worker_pid = booted.guest.pid() as i32;
    let stderr = booted.guest.end();
    assert_eq!(
        booted.report, "dialed DB_ADDR=host:hello-DB_ADDR",
        "the stock client did not reach its declared peer through {address}. \
         worker stderr:\n{stderr}"
    );
    assert_eq!(
        vmm,
        Some(worker_pid),
        "the declared peer's connection did not come from this run's VMM"
    );
    assert_eq!(booted.digest, declared.confinement_digest().unwrap());

    // The same client, the same environment, no declared connection.
    let undeclared = base_manifest(&libkrunfw);
    let mut booted = Run::new(&root, &libkrunfw, CLIENT, "dial-undeclared")
        .env("DB_ADDR", &address)
        .args(&["env:DB_ADDR"])
        .manifest(&undeclared)
        .boot();
    let stderr = booted.guest.end();
    assert_eq!(
        booted.report, "dialed DB_ADDR=ConnectionRefused",
        "with nothing declared the guest's {address} must reach nothing. worker \
         stderr:\n{stderr}"
    );
    assert_ne!(
        booted.digest,
        declared.confinement_digest().unwrap(),
        "removing the connection must change the attested digest"
    );
    assert!(
        peers.try_recv().is_err(),
        "the declared peer was reached by the run that did not declare it"
    );
}

// ---------------------------------------------------------------------------
// Acceptance test 2 (serve), I6, and the unmodified server.

/// A stock server listening on its declared guest port is reached by the
/// declared peer through the declared socket, and by nothing else: no host
/// TCP address answers for it, the VMM holds no TCP socket, and a second
/// host process without the socket in its sandbox is refused and never
/// reaches the guest.
#[test]
fn a_served_port_is_reached_by_its_declared_peer_only() {
    let _serial = common::serial();
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    if !presentation_is_live_here() {
        return;
    }
    let (_fixture, root) = fixture();
    let socket = root.join("api.sock");
    let manifest = with_grants(&libkrunfw, vec![serve_grant(&socket, SERVE_PORT)]);
    let mut booted = Run::new(&root, &libkrunfw, SERVER, "serve-declared")
        .env("LISTEN_ADDR", &format!("127.0.0.1:{SERVE_PORT}"))
        .manifest(&manifest)
        .boot();
    assert_eq!(booted.report, format!("listening 127.0.0.1:{SERVE_PORT}"));

    // The declared peer, through the declared socket.
    let first = connect_unix(&socket).map(|mut stream| exchange(&mut stream, "first"));

    // Every other route a second host process could try.
    let host_tcp = TcpStream::connect_timeout(
        &SocketAddr::from((Ipv4Addr::LOCALHOST, SERVE_PORT)),
        Duration::from_secs(5),
    )
    .map(|_| ())
    .map_err(|error| error.kind());
    let sockets = sockets_of(booted.guest.pid());
    // Peer identity on the host is the filesystem's (ADR-0046 D5): the VMM
    // creates the socket owner-only (a 077 umask: no group or other access,
    // 0700 on macOS), so only the embedder's user may connect, and
    // the directory the issuer chose narrows that further. A process of
    // another user is refused by the mode (not run here: the test runs as
    // one user). A same-user process is refused only by its own confinement,
    // which LLO applies to every workload it runs: `sandbox-exec` without the
    // socket in its profile stands in for one.
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(&socket).expect("the served socket").permissions().mode() & 0o777
    };
    let second = Command::new("/usr/bin/sandbox-exec")
        .arg("-p")
        .arg(format!(
            "(version 1)(allow default)(deny network-outbound (literal \"{}\"))",
            socket.display()
        ))
        .args(["/usr/bin/nc", "-U"])
        .arg(&socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().expect("stdin").write_all(b"second\n")?;
            child.wait_with_output()
        });

    // The declared peer again: the count says whether anything reached the
    // guest's listener in between.
    let third = connect_unix(&socket).map(|mut stream| exchange(&mut stream, "third"));
    let stderr = booted.guest.end();

    assert_eq!(
        first.as_deref().ok(),
        Some("guest#1:first"),
        "the declared peer did not reach the guest's listener. worker stderr:\n{stderr}"
    );
    assert_eq!(
        mode & 0o077,
        0,
        "SECURITY: the served socket is mode {mode:o}; a user other than the \
         embedder's who can reach it reaches the guest's listener"
    );
    assert_eq!(
        host_tcp,
        Err(ErrorKind::ConnectionRefused),
        "SECURITY: host TCP 127.0.0.1:{SERVE_PORT} answered for the guest's served port"
    );
    assert!(
        sockets.iter().all(|(kind, _)| kind == "unix"),
        "SECURITY: the VMM holds a non-UNIX socket: {sockets:?}"
    );
    let second = second.expect("run the second host process");
    assert!(
        !second.status.success() && second.stdout.is_empty(),
        "SECURITY: a host process whose sandbox lacks the socket reached the guest: \
         {second:?}"
    );
    assert_eq!(
        third.as_deref().ok(),
        Some("guest#2:third"),
        "SECURITY: something other than the declared peer reached the guest's \
         listener between the declared peer's connections. worker stderr:\n{stderr}"
    );
}

// ---------------------------------------------------------------------------
// Acceptance tests 3 and 6: the negative sweep, against the pure function.

/// With one dial connection declared, exactly its guest port answers on the
/// guest's loopback, exactly its vsock port answers on the host's CID, no
/// route leaves the guest, and the VMM holds no socket but the declared
/// one's. The accepting sets are the ones `guest_loopback_channels` and
/// `vsock_unix_mappings` compute from the document without a VMM (test 6).
#[test]
fn only_the_declared_ports_exist_and_they_match_the_pure_function() {
    let _serial = common::serial();
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    if !presentation_is_live_here() {
        return;
    }
    let (_fixture, root) = fixture();
    let socket = root.join("db.sock");
    let peers = answer_dials(UnixListener::bind(&socket).expect("the declared peer"));
    let manifest = with_grants(&libkrunfw, vec![dial_grant(&socket, DIAL_PORT)]);

    // Test 6: the guest-visible set, computed without a VMM.
    let channels = guest_loopback_channels(&manifest, 0, "macos").expect("pure");
    let expected_tcp: Vec<String> = channels
        .iter()
        .filter(|channel| channel.direction == LoopbackDirection::Dial)
        .map(|channel| channel.guest_port.to_string())
        .collect();
    let expected_vsock: Vec<String> = channels
        .iter()
        .map(|channel| channel.vsock_port.to_string())
        .collect();
    assert_eq!(expected_tcp, [DIAL_PORT.to_string()]);
    assert_eq!(expected_vsock, [VSOCK_UNIX_BASE_PORT.to_string()]);

    let mut booted = Run::new(&root, &libkrunfw, SWEEP, "sweep")
        .manifest(&manifest)
        .boot();
    let sockets = sockets_of(booted.guest.pid());
    let stderr = booted.guest.end();
    let report = booted.report.clone();
    let field = |name: &str| -> String {
        report
            .split_whitespace()
            .find_map(|item| item.strip_prefix(&format!("{name}=")))
            .unwrap_or_else(|| panic!("no {name} in the report: {report}"))
            .to_owned()
    };
    assert_eq!(
        field("tcp"),
        expected_tcp.join(","),
        "SECURITY: the guest loopback ports that accept differ from the declared \
         set. report: {report}\nworker stderr:\n{stderr}"
    );
    assert_eq!(
        field("vsock"),
        expected_vsock.join(","),
        "SECURITY: the host vsock ports that accept differ from the declared set. \
         report: {report}"
    );
    for remote in ["203.0.113.2:80", "1.1.1.1:443", "10.0.2.2:80", "[2001:db8::1]:80"] {
        let outcome = field(remote);
        assert!(
            outcome == "NetworkUnreachable" || outcome == "HostUnreachable",
            "SECURITY: {remote} from the guest: {outcome} (I1: no route leaves the \
             guest). report: {report}"
        );
    }
    // Interfaces: loopback, and the kernel's dummy0, which init brings up only
    // with socket hijacking (patch-free upstream behaviour). Down and
    // unaddressed, it is no route (the remote probes above are the proof).
    let interfaces: Vec<&str> = report
        .split_whitespace()
        .filter_map(|item| item.strip_prefix("if:"))
        .collect();
    for interface in &interfaces {
        let name = interface.split(':').next().unwrap_or_default();
        assert!(
            name == "lo" || name == "dummy0",
            "SECURITY: an interface other than lo and dummy0 exists in the guest: \
             {interface}"
        );
        if name == "dummy0" {
            assert!(
                interface.starts_with("dummy0:down:"),
                "dummy0 is up without socket hijacking: {interface}"
            );
        }
    }
    eprintln!("guest interfaces: {interfaces:?}");
    // The VMM: no internet socket, and every named UNIX socket is the
    // declared one.
    for (kind, name) in &sockets {
        assert_eq!(kind, "unix", "SECURITY: the VMM holds {kind} {name}");
        assert!(
            !name.starts_with('/') || Path::new(name) == socket,
            "SECURITY: the VMM holds an undeclared UNIX socket {name}; all: {sockets:?}"
        );
    }
    eprintln!("VMM sockets: {sockets:?}");
    drop(peers);
}

// ---------------------------------------------------------------------------
// Acceptance test 4: the request cannot widen reach (I3).

/// Addresses in the environment and in argv are the workload's
/// configuration, not authority: only the declared guest port answers, and
/// a request that names the init's channel list, directly or by smuggling a
/// quote onto the kernel command line, is refused before the worker is
/// ready.
#[test]
fn the_request_cannot_widen_reach() {
    let _serial = common::serial();
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    if !presentation_is_live_here() {
        return;
    }
    let (_fixture, root) = fixture();
    let socket = root.join("db.sock");
    let _peers = answer_dials(UnixListener::bind(&socket).expect("the declared peer"));
    let manifest = with_grants(&libkrunfw, vec![dial_grant(&socket, DIAL_PORT)]);

    let mut booted = Run::new(&root, &libkrunfw, CLIENT, "widen-env")
        .env("DB_ADDR", &format!("127.0.0.1:{DIAL_PORT}"))
        .env("OTHER_ADDR", "127.0.0.1:13307")
        .args(&["env:DB_ADDR", "env:OTHER_ADDR", "127.0.0.1:13308"])
        .manifest(&manifest)
        .boot();
    let stderr = booted.guest.end();
    assert_eq!(
        booted.report,
        "dialed DB_ADDR=host:hello-DB_ADDR OTHER_ADDR=ConnectionRefused \
         127.0.0.1:13308=ConnectionRefused",
        "worker stderr:\n{stderr}"
    );

    // Each would otherwise reach the guest kernel command line: the channel
    // list by name, by a quote breaking out of a value, and the guest init
    // itself replaced through the kernel's own `init=` parameter, which would
    // take the datagram filter and the relay with it (ley-line-open-eb5e4e).
    for (run_id, key, value, named) in [
        (
            "widen-named",
            "KRUN_LOOPBACK_CHANNELS",
            "dial:13307:65536",
            "KRUN_LOOPBACK_CHANNELS",
        ),
        (
            "widen-smuggled",
            "DB_ADDR",
            "x\" KRUN_LOOPBACK_CHANNELS=\"dial:13307:65536",
            "double quote",
        ),
        ("widen-init", "init", "/bin/guest", "\\\"init\\\""),
        ("widen-krun-init", "KRUN_INIT", "/bin/guest", "KRUN_INIT"),
    ] {
        let refusal = Run::new(&root, &libkrunfw, CLIENT, run_id)
            .env(key, value)
            .manifest(&manifest)
            .refused();
        assert!(
            refusal.contains("\"failed\"") && refusal.contains(named),
            "{run_id}: {refusal}"
        );
    }
}

// ---------------------------------------------------------------------------
// Acceptance test 5: fail closed, by name, before the VMM (I4, bb2143).

/// An undeliverable connection refuses the run before readiness, naming it:
/// a dial whose peer is not bound, a serve socket path no UNIX address can
/// carry (bead `ley-line-open-bb2143`), and a presentation under socket
/// hijacking. None of these reaches a VMM.
#[test]
fn an_undeliverable_connection_refuses_the_run_by_name() {
    let _serial = common::serial();
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    let (_fixture, root) = fixture();

    let unbound = root.join("nobody.sock");
    let long = root.join(format!("{}.sock", "l".repeat(120)));
    let cases: [(&str, ConfinementManifest, bool, String); 3] = [
        (
            "refuse-unbound",
            with_grants(&libkrunfw, vec![dial_grant(&unbound, DIAL_PORT)]),
            false,
            unbound.display().to_string(),
        ),
        (
            "refuse-long",
            with_grants(&libkrunfw, vec![serve_grant(&long, SERVE_PORT)]),
            false,
            long.display().to_string(),
        ),
        (
            "refuse-hijack",
            {
                let peer = root.join("p.sock");
                let _ = UnixListener::bind(&peer).map(std::mem::forget);
                with_grants(&libkrunfw, vec![dial_grant(&peer, DIAL_PORT)])
            },
            true,
            "hijack".to_owned(),
        ),
    ];
    for (run_id, manifest, hijack, named) in cases {
        let mut run = Run::new(&root, &libkrunfw, CLIENT, run_id).manifest(&manifest);
        if hijack {
            run = run.hijack();
        }
        let refusal = run.refused();
        assert!(
            refusal.contains("\"failed\"") && refusal.contains(&named),
            "{run_id}: the refusal must be a failure event naming {named}: {refusal}"
        );
        assert!(
            !long.exists(),
            "{run_id}: a refused run left a socket behind"
        );
    }
}

/// The same refusal through execution/v1: the run never starts, leaves no
/// run root, and has no receipt to collect.
#[test]
fn an_undeliverable_connection_leaves_no_run_root_and_no_receipt() {
    let _serial = common::serial();
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    let (_fixture, root) = fixture();
    let (service, ephemeral_root, rootfs) = service(&root, &libkrunfw, CLIENT);
    let long = root.join(format!("{}.sock", "l".repeat(120)));
    let manifest = with_grants(&libkrunfw, vec![serve_grant(&long, SERVE_PORT)]);
    let document = manifest.to_canonical_json().unwrap();
    let digest = manifest.confinement_digest().unwrap();
    let (spec, grant) = spec_and_grant(&digest, &document, "refused");
    let error = service
        .start_authorized(
            &spec,
            &grant,
            &AuthorizationPolicy {
                now_unix_ms: None,
                required_backend: BackendClass::MicroVm,
                required_confinement_digest: Some(digest.clone()),
            },
            &resolver(rootfs, &[]),
        )
        .expect_err("an undeliverable connection must refuse the start");
    assert!(
        error.detail.contains(&long.display().to_string()),
        "the refusal must name the socket: {error}"
    );
    assert_eq!(
        run_roots(&ephemeral_root),
        0,
        "a refused run left its run root"
    );
    assert!(!long.exists(), "a refused run left a socket behind");
}

// ---------------------------------------------------------------------------
// Acceptance test 7: teardown (I9).

/// A run with a dial and a serve channel, both carrying a live connection
/// when the cancel arrives: after `cancel` returns, the VMM is gone, both
/// connections read end of stream, and the served socket accepts nothing.
#[test]
fn cancel_closes_every_host_side_end() {
    let _serial = common::serial();
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    if !presentation_is_live_here() {
        return;
    }
    let (_fixture, root) = fixture();
    let (service, ephemeral_root, rootfs) = service(&root, &libkrunfw, TEARDOWN);
    let dial_socket = root.join("db.sock");
    let dial_listener = UnixListener::bind(&dial_socket).expect("the declared peer");
    let serve_socket = root.join("api.sock");
    let manifest = with_grants(
        &libkrunfw,
        vec![
            dial_grant(&dial_socket, DIAL_PORT),
            serve_grant(&serve_socket, SERVE_PORT),
        ],
    );
    let document = manifest.to_canonical_json().unwrap();
    let digest = manifest.confinement_digest().unwrap();
    let (spec, grant) = spec_and_grant(&digest, &document, "teardown");
    let record = service
        .start_authorized(
            &spec,
            &grant,
            &AuthorizationPolicy {
                now_unix_ms: None,
                required_backend: BackendClass::MicroVm,
                required_confinement_digest: Some(digest.clone()),
            },
            &resolver(
                rootfs,
                &[
                    ("DB_ADDR", format!("127.0.0.1:{DIAL_PORT}")),
                    ("LISTEN_ADDR", format!("127.0.0.1:{SERVE_PORT}")),
                ],
            ),
        )
        .expect("start");

    // EVENT: the guest listens on its served port, then dials its declared
    // peer and says so; the host's accept is the guest being up.
    let (accepted_tx, accepted_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = accepted_tx.send(dial_listener.accept().map(|(stream, _)| stream));
    });
    let dialed = accepted_rx
        .recv_timeout(REPORT_BOUND)
        .expect("the guest dialed its declared peer")
        .expect("accept");
    let vmm = peer_pid(&dialed);
    let mut dialed = BufReader::new(dialed);
    let mut line = String::new();
    dialed.read_line(&mut line).expect("the guest's line");
    assert_eq!(line.trim_end(), "teardown-listening");
    let mut served = connect_unix(&serve_socket).expect("the declared peer dials the guest");
    assert_eq!(exchange(&mut served, "held"), "guest:held");

    let cancelled = service.cancel(&record.run_id).expect("cancel");
    assert_eq!(cancelled.state, RunState::Cancelled);
    assert!(!alive(vmm), "the VMM {vmm} survived the cancel");
    assert!(!group_alive(vmm), "a process in the VMM's group survived");
    assert_eq!(run_roots(&ephemeral_root), 0);
    for (name, stream) in [
        ("dial", dialed.get_mut() as &mut UnixStream),
        ("serve", &mut served),
    ] {
        let mut rest = Vec::new();
        match stream.read_to_end(&mut rest) {
            Ok(_) => assert!(rest.is_empty(), "{name}: bytes after the cancel: {rest:?}"),
            Err(error) => assert_eq!(
                error.kind(),
                ErrorKind::ConnectionReset,
                "{name}: the channel did not end with its VMM: {error}"
            ),
        }
    }
    match UnixStream::connect(&serve_socket) {
        Ok(_) => panic!("the served socket still accepts after the cancel"),
        Err(error) => assert!(
            matches!(
                error.kind(),
                ErrorKind::ConnectionRefused | ErrorKind::NotFound
            ),
            "{error}"
        ),
    }
    let receipt = service.collect(&record.run_id).expect("receipt");
    assert_eq!(receipt.terminal_state, RunState::Cancelled);
    assert_eq!(receipt.context.confinement_digest, digest);
}

/// The teardown guest: listens on `$LISTEN_ADDR` (answering `guest:<line>`),
/// then dials `$DB_ADDR`, writes `teardown-listening`, and holds both.
const TEARDOWN: &str = r#"use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};

fn main() {
    let listener = TcpListener::bind(std::env::var("LISTEN_ADDR").unwrap()).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
                    let _ = write!(stream, "guest:{line}");
                    line.clear();
                }
            });
        }
    });
    let mut peer = TcpStream::connect(std::env::var("DB_ADDR").unwrap()).unwrap();
    peer.write_all(b"teardown-listening\n").unwrap();
    loop {
        std::thread::park();
    }
}
"#;

fn service(
    root: &Path,
    libkrunfw: &Path,
    source: &str,
) -> (ExecutionService<KrunWorkerBackend>, PathBuf, String) {
    let source_path = root.join("svc-guest.rs");
    fs::write(&source_path, source).expect("guest source");
    let staged = root.join("svc-guest");
    hypervisor::compile_guest(&source_path, &staged);
    let bytes = fs::read(&staged).expect("guest bytes");
    let rootfs = conformance::publish_rootfs(&root.join("cas"), &[("bin/guest", &bytes, 0o755)]);
    let worker = hypervisor::worker_under_test(
        Path::new(env!("CARGO_BIN_EXE_leyline-krun-worker")),
        &root.join("svc-worker"),
    );
    let (library_var, library_dir) = hypervisor::library_path_env(libkrunfw);
    // SAFETY: tests here are serialized, and nothing reads the environment
    // concurrently.
    unsafe { std::env::set_var(library_var, library_dir) };
    let ephemeral_root = root.join("runs");
    fs::create_dir_all(&ephemeral_root).expect("ephemeral root");
    let service = ExecutionService::new(KrunWorkerBackend::new(KrunWorkerConfig {
        worker,
        cas_root: root.join("cas"),
        ephemeral_root: ephemeral_root.clone(),
        runtime_files: vec![libkrunfw.to_path_buf()],
        devices: Vec::new(),
        ready_timeout: Duration::from_secs(30),
        tsi_hijack_inet: false,
    }));
    service
        .provision(BackendClass::MicroVm, "provision-connections")
        .expect("the worker can enter the hypervisor");
    (service, ephemeral_root, rootfs)
}

fn resolver(rootfs: String, environment: &[(&str, String)]) -> GuestResolver {
    GuestResolver {
        rootfs,
        executable: "bin/guest".into(),
        arguments: vec!["guest".into()],
        environment: environment
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect(),
        limits: ResourceLimits {
            wall_time_ms: 600_000,
            memory: leyline_runtime::MemoryLimits { limit: 256 << 20 },
            cpu: leyline_runtime::CpuLimits {
                quota: 100_000,
                period: 100_000,
            },
            ..ResourceLimits::default()
        },
    }
}

fn spec_and_grant(digest: &str, document: &str, name: &str) -> (Vec<u8>, Vec<u8>) {
    let mut authority = GrantSpec::new(
        BackendClass::MicroVm,
        &format!("{name}-{}", conformance::now_unix_ms()),
        digest,
        document,
    );
    authority.grant_id = format!("grant-{name}");
    conformance::spec_and_grant(&authority)
}

// ---------------------------------------------------------------------------
// UDP is denied in the guest (Q-c).

/// Every way to open a UDP-capable socket in the guest is refused with
/// EACCES (13), whatever the boundary, and the stream and local controls
/// still work. Run both without socket hijacking and with it: hijacking is
/// the operator downgrade under which a datagram could otherwise be carried
/// out (beads ley-line-open-0026d2, ley-line-open-381b1d).
#[test]
fn the_guest_cannot_open_a_datagram_socket() {
    let _serial = common::serial();
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    let (_fixture, root) = fixture();
    let expected = "datagrams std-udp4=PermissionDenied std-udp6=PermissionDenied \
                    inet-dgram=errno13 inet-dgram-cloexec=errno13 inet6-dgram=errno13 \
                    inet-raw-udp=errno13 inet-stream-udp=errno13 packet=errno13 \
                    io_uring=errno13 control-tcp=ok control-unix-dgram=ok";
    for hijack in [false, true] {
        let run_id = format!("datagram-hijack-{hijack}");
        let mut run = Run::new(&root, &libkrunfw, DATAGRAM, &run_id);
        if hijack {
            run = run.hijack();
        }
        let mut booted = run.boot();
        let stderr = booted.guest.end();
        assert_eq!(
            booted.report, expected,
            "SECURITY (hijack={hijack}): the guest opened a datagram-capable socket, \
             or a control failed. worker stderr:\n{stderr}"
        );
    }
}

// ---------------------------------------------------------------------------
// Full duplex through the relay.

/// Bytes the full-duplex case sends each way: far beyond every buffer on
/// the path (socket buffers, the vsock credit window, the relay's 64 KiB), so
/// a relay that stops reading one direction while a write in the other is
/// blocked deadlocks here.
const DUPLEX_BYTES: usize = 16 << 20;

/// The full-duplex guest: dials `$DB_ADDR`, writes DUPLEX_BYTES from one
/// thread while reading DUPLEX_BYTES on another, and reports both counts.
const DUPLEX: &str = r#"use std::io::{Read, Write};
use std::net::TcpStream;

fn main() {
    let total: usize = std::env::var("DUPLEX_BYTES").unwrap().parse().unwrap();
    let stream = TcpStream::connect(std::env::var("DB_ADDR").unwrap()).unwrap();
    let mut writer = stream.try_clone().unwrap();
    let sent = std::thread::spawn(move || {
        let chunk = vec![0x5au8; 65536];
        let mut sent = 0;
        while sent < total {
            let n = (total - sent).min(chunk.len());
            writer.write_all(&chunk[..n]).unwrap();
            sent += n;
        }
        sent
    });
    let mut reader = stream;
    let mut buffer = vec![0u8; 65536];
    let mut received = 0usize;
    while received < total {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => received += n,
        }
    }
    let sent = sent.join().unwrap();
    println!("connections-probe: duplex sent={sent} received={received}");
    loop {
        std::thread::park();
    }
}
"#;

/// A protocol that writes and reads at once (gRPC streams, a MySQL client
/// pipelining) crosses the relay as it would one TCP stream: both sides
/// send DUPLEX_BYTES at the same time, and both receive all of it.
#[test]
fn a_full_duplex_stream_crosses_the_relay() {
    let _serial = common::serial();
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    if !presentation_is_live_here() {
        return;
    }
    let (_fixture, root) = fixture();
    let socket = root.join("db.sock");
    let listener = UnixListener::bind(&socket).expect("the declared peer");
    let host = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("the guest dials");
        let mut writer = stream.try_clone().expect("clone");
        let sender = std::thread::spawn(move || {
            let chunk = vec![0xa5u8; 65536];
            let mut sent = 0;
            while sent < DUPLEX_BYTES {
                let n = (DUPLEX_BYTES - sent).min(chunk.len());
                if writer.write_all(&chunk[..n]).is_err() {
                    break;
                }
                sent += n;
            }
            sent
        });
        let mut reader = stream;
        let mut buffer = vec![0u8; 65536];
        let mut received = 0usize;
        while received < DUPLEX_BYTES {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => received += n,
            }
        }
        (sender.join().unwrap_or(0), received)
    });
    let manifest = with_grants(&libkrunfw, vec![dial_grant(&socket, DIAL_PORT)]);
    let mut booted = Run::new(&root, &libkrunfw, DUPLEX, "duplex")
        .env("DB_ADDR", &format!("127.0.0.1:{DIAL_PORT}"))
        .env("DUPLEX_BYTES", &DUPLEX_BYTES.to_string())
        .manifest(&manifest)
        .boot();
    let host = host.join().expect("host side");
    let stderr = booted.guest.end();
    assert_eq!(
        booted.report,
        format!("duplex sent={DUPLEX_BYTES} received={DUPLEX_BYTES}"),
        "worker stderr:\n{stderr}"
    );
    assert_eq!(host, (DUPLEX_BYTES, DUPLEX_BYTES));
}

// ---------------------------------------------------------------------------
// Acceptance test 9: stability.

/// Dial and serve in one run, `LEYLINE_CONNECTIONS_RUNS` times (default 1)
/// in a row. A boot stall (bead `ley-line-open-c4480a`: the guest prints
/// nothing at all) is counted and reported separately and never retried;
/// any other failure fails the test at once. The known stall rate is 1–3 %
/// with socket hijacking on; these runs have it off.
#[test]
fn dial_and_serve_hold_across_consecutive_runs() {
    let _serial = common::serial();
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    if !presentation_is_live_here() {
        return;
    }
    let runs: usize = std::env::var("LEYLINE_CONNECTIONS_RUNS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);
    let (_fixture, root) = fixture();
    let mut stalls = Vec::new();
    for run in 0..runs {
        let directory = root.join(format!("r{run}"));
        fs::create_dir(&directory).expect("run directory");
        let dial_socket = directory.join("db.sock");
        let _peers = answer_dials(UnixListener::bind(&dial_socket).expect("peer"));
        let serve_socket = directory.join("api.sock");
        let manifest = with_grants(
            &libkrunfw,
            vec![
                dial_grant(&dial_socket, DIAL_PORT),
                serve_grant(&serve_socket, SERVE_PORT),
            ],
        );
        let run_id = format!("stability-{run}");
        let attempt = Run::new(&directory, &libkrunfw, STABILITY, &run_id)
            .env("DB_ADDR", &format!("127.0.0.1:{DIAL_PORT}"))
            .env("LISTEN_ADDR", &format!("127.0.0.1:{SERVE_PORT}"))
            .manifest(&manifest)
            .try_boot();
        let mut booted = match attempt {
            Ok(booted) => booted,
            Err(BootFailure::Stalled { stderr }) => {
                eprintln!("run {run}: BOOT STALL (c4480a), not retried\n{stderr}");
                stalls.push(run);
                continue;
            }
            Err(other) => panic!("run {run} failed: {other:#?}"),
        };
        let served = connect_unix(&serve_socket).map(|mut stream| exchange(&mut stream, "s"));
        let stderr = booted.guest.end();
        assert_eq!(
            (booted.report.as_str(), served.as_deref().ok()),
            ("dialed DB_ADDR=host:hello-DB_ADDR", Some("guest#1:s")),
            "run {run}. worker stderr:\n{stderr}"
        );
        // Each run's directory goes with it, so 100 runs leave nothing.
        fs::remove_dir_all(&directory).ok();
    }
    eprintln!(
        "STABILITY: {runs} consecutive runs, {} passed, {} boot stall(s) {stalls:?}",
        runs - stalls.len(),
        stalls.len()
    );
    assert!(
        stalls.len() * 100 <= runs * 3,
        "boot stalls {stalls:?} exceed the known c4480a rate (3%) over {runs} runs"
    );
}

/// The stability guest: listens on `$LISTEN_ADDR` answering
/// `guest#<n>:<line>`, then dials `$DB_ADDR` as CLIENT does and reports.
const STABILITY: &str = r#"use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};

fn main() {
    let listener = TcpListener::bind(std::env::var("LISTEN_ADDR").unwrap()).unwrap();
    let mut peer = TcpStream::connect(std::env::var("DB_ADDR").unwrap()).unwrap();
    writeln!(peer, "hello-DB_ADDR").unwrap();
    let mut reply = String::new();
    BufReader::new(&peer).read_line(&mut reply).unwrap();
    println!("connections-probe: dialed DB_ADDR={}", reply.trim_end());
    let mut served = 0u32;
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        served += 1;
        let mut line = String::new();
        let _ = BufReader::new(&stream).read_line(&mut line);
        let _ = write!(stream, "guest#{served}:{line}");
    }
}
"#;
