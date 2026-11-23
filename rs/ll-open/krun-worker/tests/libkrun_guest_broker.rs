//! A microVM guest reaches a host API through the run's connection broker,
//! end to end (ADR-0046 Slice 1, S11 "Hypervisor-gated"; bead
//! `ley-line-open-5308b7`).
//!
//! Everything is the shipped path: `ExecutionService` authorizes a RunGrant
//! carrying `connectionsDigest @16`, `connectionsDocument @17` and the
//! marker, starts the confined `leyline-conn-broker`, and boots a real guest
//! through `KrunWorkerBackend` and the real `leyline-krun-worker`. The guest
//! is a stock TCP client: it dials `127.0.0.1:<guestPort>` and nothing in it
//! knows about a broker, a socket path or a microVM.
//!
//! - the guest round-trips with a host echo on `127.0.0.1:E`;
//! - an undeclared guest port gives `ECONNREFUSED`;
//! - non-loopback addresses of the host fail;
//! - a second host listener receives nothing;
//! - a target that comes up after the guest dials still serves the held
//!   connection, and one that never comes up ends it within the hold;
//! - two concurrent runs with distinct channel directories both reach the
//!   echo, and a run reusing a live run's directory is refused by name;
//! - the receipt records each connection.
//!
//! ## Synchronization
//!
//! By event. The guest reports over a second declared connection, `ctl`,
//! whose target is a listener in this test: a line arriving there is the
//! event. Timeouts only bound a failure. A guest that reports nothing within
//! the bound is a boot stall (bead `ley-line-open-c4480a`), which is a
//! failure here, never a pass.
//!
//! Gated like every real-guest test: without `LEYLINE_LIBKRUNFW_PATH` each
//! test prints `SKIPPED (NOT a pass)`; `LEYLINE_REQUIRE_HYPERVISOR=1` makes
//! that a failure. Off macOS the presentation is not delivered, and the
//! refusal by name is the test.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, UdpSocket};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use leyline_runtime::authorization::AuthorizationPolicy;
use leyline_runtime::backends::libkrun::backend::{KrunWorkerBackend, KrunWorkerConfig};
use leyline_runtime::backends::libkrun::confinement::confinement_manifest;
use leyline_runtime::confinement::{ConfinementManifest, UnixSocketGrant};
use leyline_runtime::{
    BackendClass, BrokerConfig, ErrorCode, ExecutionError, ExecutionService, ResourceLimits,
    RunReceiptData, RunRecord, RunState,
};
use tempfile::TempDir;

#[path = "../../runtime/tests/common/mod.rs"]
mod common;
#[path = "../../runtime/tests/conformance/mod.rs"]
mod conformance;
mod hypervisor;

use conformance::{GrantSpec, GuestResolver};

/// Only a bound on a failure: a guest that boots reports in a few seconds.
const REPORT_BOUND: Duration = Duration::from_secs(90);

/// The guest loopback ports the two declared connections are presented on,
/// and one nothing declares.
const API_PORT: u16 = 13306;
const CTL_PORT: u16 = 13307;
const UNDECLARED_PORT: u16 = 13308;

/// The broker's hold on a connection whose target refuses (ADR-0046 S5), and
/// the slack a loaded host gets on top of it.
const HOLD: Duration = leyline_runtime::broker::HOLD;
const HOLD_SLACK: Duration = Duration::from_secs(8);

/// A stock TCP client. It dials `$CTL_ADDR` and writes `start`, then for
/// each `label=address` in `$TARGETS` (space-separated) connects with a
/// bound, sends `hello-<label>`, and records the reply line, `eof`, or the
/// error kind, with the milliseconds it took. It writes the whole record to
/// the control connection as one line and holds the VM until the host ends
/// it.
const CLIENT: &str = r#"use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

fn main() {
    let control = std::env::var("CTL_ADDR").unwrap_or_default();
    let mut control = match TcpStream::connect(&control) {
        Ok(stream) => stream,
        Err(error) => {
            println!("broker-client: control {control}: {error}");
            std::process::exit(71);
        }
    };
    let _ = writeln!(control, "start");
    let mut report = String::from("report");
    for target in std::env::var("TARGETS").unwrap_or_default().split(' ') {
        let Some((label, address)) = target.split_once('=') else {
            continue;
        };
        let began = Instant::now();
        let outcome = match address.parse::<SocketAddr>() {
            Err(_) => "unparsed".to_owned(),
            Ok(address) => match TcpStream::connect_timeout(&address, Duration::from_secs(8)) {
                Ok(mut stream) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(40)));
                    let _ = writeln!(stream, "hello-{label}");
                    let mut reply = String::new();
                    match BufReader::new(&stream).read_line(&mut reply) {
                        Ok(0) => "eof".to_owned(),
                        Ok(_) => reply.trim_end().replace(' ', "_"),
                        Err(error) => format!("read-{:?}", error.kind()),
                    }
                }
                Err(error) => format!("{:?}", error.kind()),
            },
        };
        report.push_str(&format!(" {label}={outcome}@{}", began.elapsed().as_millis()));
    }
    let _ = writeln!(control, "{report}");
    loop {
        std::thread::park();
    }
}
"#;

/// A loopback TCP listener that answers every line with the same line and
/// hands each line it reads to the test.
struct Target {
    port: u16,
    lines: mpsc::Receiver<String>,
}

impl Target {
    fn listen() -> Self {
        Self::serve(TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("loopback listener"))
    }

    fn serve(listener: TcpListener) -> Self {
        let port = listener.local_addr().expect("address").port();
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let sender = sender.clone();
                std::thread::spawn(move || {
                    let mut writer = stream.try_clone().expect("clone");
                    for line in BufReader::new(stream).lines().map_while(Result::ok) {
                        let _ = sender.send(line.clone());
                        if writeln!(writer, "{line}").is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Self { port, lines }
    }

    /// The next line a client wrote here. EVENT; the bound is a failure's.
    fn line(&self, what: &str, bound: Duration) -> String {
        self.lines.recv_timeout(bound).unwrap_or_else(|_| {
            panic!(
                "TIMEOUT: {what} within {bound:?}. A guest that printed nothing is the \
                 boot stall of ley-line-open-c4480a, which is a failure, not a pass: rerun \
                 on a quiet host."
            )
        })
    }

    fn silent(&self) -> bool {
        self.lines.try_recv().is_err()
    }
}

/// A port nothing listens on: bound once, then released.
fn unused_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .expect("a free port")
        .local_addr()
        .expect("address")
        .port()
}

/// The issuer's per-run channel directory: private, named by its real path.
fn channel_dir() -> (TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("ch")
        .tempdir_in("/tmp")
        .expect("channel directory");
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).expect("mode");
    let real = dir.path().canonicalize().expect("real path");
    (dir, real)
}

/// The canonical two-connection document: `api` to `api_port` and `ctl` to
/// `ctl_port`, each endpoint in `dir`. Written out in its canonical form,
/// which authorization holds it to.
fn connections(dir: &Path, api_port: u16, ctl_port: u16) -> String {
    let entry = |name: &str, port: u16| {
        format!(
            "    {{\n      \"appProtocol\": \"art.tcp\",\n      \"endpoint\": {{\n        \
             \"unix\": \"{}\"\n      }},\n      \"name\": \"{name}\",\n      \"target\": {{\n        \
             \"tcp\": {{\n          \"host\": \"127.0.0.1\",\n          \"port\": {port}\n        }}\n      }}\n    }}",
            endpoint(dir, name)
        )
    };
    format!(
        "{{\n  \"connections\": [\n{},\n{}\n  ],\n  \"version\": \"cloister/connections/v1\"\n}}",
        entry("api", api_port),
        entry("ctl", ctl_port)
    )
}

fn endpoint(dir: &Path, name: &str) -> String {
    dir.join(format!("g.{name}.sock")).display().to_string()
}

struct Host {
    _fixture: TempDir,
    service: ExecutionService<KrunWorkerBackend>,
    rootfs: String,
    libkrunfw: PathBuf,
}

impl Host {
    /// A provisioned service with the shipped broker and the client guest.
    fn new(libkrunfw: PathBuf) -> Self {
        let fixture = TempDir::new().expect("fixture");
        let root = fixture.path().canonicalize().expect("canonical fixture");
        let source = root.join("guest.rs");
        fs::write(&source, CLIENT).expect("guest source");
        let staged = root.join("guest");
        hypervisor::compile_guest(&source, &staged);
        let bytes = fs::read(&staged).expect("guest bytes");
        let rootfs =
            conformance::publish_rootfs(&root.join("cas"), &[("bin/guest", &bytes, 0o755)]);
        let worker = hypervisor::worker_under_test(
            Path::new(env!("CARGO_BIN_EXE_leyline-krun-worker")),
            &root.join("worker"),
        );
        let (library_var, library_dir) = hypervisor::library_path_env(&libkrunfw);
        // SAFETY: tests here are serialized, and nothing reads the
        // environment concurrently.
        unsafe { std::env::set_var(library_var, library_dir) };
        fs::create_dir_all(root.join("runs")).expect("ephemeral root");
        let service = ExecutionService::new(KrunWorkerBackend::new(KrunWorkerConfig {
            worker,
            cas_root: root.join("cas"),
            ephemeral_root: root.join("runs"),
            runtime_files: vec![libkrunfw.clone()],
            devices: Vec::new(),
            ready_timeout: Duration::from_secs(30),
            tsi_hijack_inet: false,
        }))
        .with_connection_broker(BrokerConfig::new(hypervisor::connection_broker(Path::new(
            env!("CARGO_BIN_EXE_leyline-krun-worker"),
        ))));
        service
            .provision(BackendClass::MicroVm, "provision-broker")
            .expect("the worker can enter the hypervisor");
        Self {
            _fixture: fixture,
            service,
            rootfs,
            libkrunfw,
        }
    }

    /// The run's confinement document: LLO's compiled policy plus one §6
    /// connect grant per endpoint, each presented on its guest port.
    fn manifest(&self, dir: &Path) -> ConfinementManifest {
        [("api", API_PORT), ("ctl", CTL_PORT)].into_iter().fold(
            confinement_manifest(std::slice::from_ref(&self.libkrunfw), &[], None)
                .expect("the compiled policy"),
            |manifest, (name, port)| {
                manifest
                    .with_unix_socket(
                        UnixSocketGrant::connect(endpoint(dir, name))
                            .with_guest_port(port)
                            .expect("guestPort"),
                    )
                    .expect("the endpoint's grant")
            },
        )
    }

    /// Authorize and start one guest whose grant carries `declared`. The
    /// guest dials `targets` in order and reports over `ctl`.
    fn start(
        &self,
        name: &str,
        dir: &Path,
        declared: &str,
        targets: &[(&str, String)],
    ) -> Result<RunRecord, ExecutionError> {
        let manifest = self.manifest(dir);
        let document = manifest.to_canonical_json().expect("canonical document");
        let digest = manifest.confinement_digest().expect("digest");
        let mut authority = GrantSpec::new(
            BackendClass::MicroVm,
            &format!("{name}-{}", conformance::now_unix_ms()),
            &digest,
            &document,
        );
        authority.grant_id = format!("grant-{name}");
        authority.connections = Some(declared);
        let (spec, grant) = conformance::spec_and_grant(&authority);
        let targets = targets
            .iter()
            .map(|(label, address)| format!("{label}={address}"))
            .collect::<Vec<_>>()
            .join(" ");
        self.service.start_authorized(
            &spec,
            &grant,
            &AuthorizationPolicy {
                now_unix_ms: None,
                required_backend: BackendClass::MicroVm,
                required_confinement_digest: Some(digest),
            },
            &GuestResolver {
                rootfs: self.rootfs.clone(),
                executable: "bin/guest".into(),
                arguments: vec!["guest".into()],
                environment: [
                    ("CTL_ADDR".to_owned(), format!("127.0.0.1:{CTL_PORT}")),
                    ("TARGETS".to_owned(), targets),
                ]
                .into(),
                limits: ResourceLimits {
                    wall_time_ms: 600_000,
                    memory: leyline_runtime::MemoryLimits { limit: 256 << 20 },
                    cpu: leyline_runtime::CpuLimits {
                        quota: 100_000,
                        period: 100_000,
                    },
                    ..ResourceLimits::default()
                },
            },
        )
    }

    /// End the run and return its receipt.
    fn receipt(&self, run_id: &str) -> RunReceiptData {
        let cancelled = self.service.cancel(run_id).expect("cancel");
        assert_eq!(cancelled.state, RunState::Cancelled);
        self.service.collect(run_id).expect("collect")
    }
}

/// One `label=outcome@ms` entry of the guest's report.
fn outcome<'a>(report: &'a str, label: &str) -> (&'a str, u64) {
    let entry = report
        .split(' ')
        .find_map(|entry| entry.strip_prefix(&format!("{label}=")))
        .unwrap_or_else(|| panic!("no {label} in the guest's report {report:?}"));
    let (outcome, millis) = entry.rsplit_once('@').expect("outcome@ms");
    (outcome, millis.parse().expect("milliseconds"))
}

/// The receipt's entry for one connection.
fn recorded<'a>(
    receipt: &'a RunReceiptData,
    name: &str,
) -> &'a leyline_runtime::broker::control::ConnectionSummary {
    receipt
        .connections
        .as_ref()
        .expect("the receipt records the run's connections")
        .connections
        .iter()
        .find(|connection| connection.name == name)
        .unwrap_or_else(|| panic!("no {name} in {receipt:?}"))
}

/// The hypervisor gate and the platform gate. `None`: a named skip, or, off
/// macOS, the refusal by name that stands in for the live rows.
fn host() -> Option<Host> {
    let libkrunfw = hypervisor::libkrunfw_or_skip()?;
    let host = Host::new(libkrunfw);
    if cfg!(target_os = "macos") {
        return Some(host);
    }
    let (_keep, dir) = channel_dir();
    let error = host
        .start("off-macos", &dir, &connections(&dir, 4000, 4001), &[])
        .expect_err("slice 1 delivers the microVM tier on macOS only");
    assert_eq!(error.code, ErrorCode::UnsupportedBackend, "{error}");
    assert!(error.detail.contains("connection `api`"), "{error}");
    assert!(error.detail.contains("microVM tier on linux"), "{error}");
    assert!(!Path::new(&endpoint(&dir, "api")).exists());
    None
}

/// A non-loopback address of this host, when it has one: the address a
/// datagram socket would send from. No packet is sent.
fn lan_address() -> Option<String> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    let address = socket.local_addr().ok()?.ip();
    (!address.is_loopback() && !address.is_unspecified()).then(|| address.to_string())
}

#[test]
fn the_guest_reaches_exactly_its_declared_target_and_the_receipt_records_it() {
    let _serial = common::serial();
    let Some(host) = host() else { return };
    let api = Target::listen();
    let control = Target::listen();
    // A second listener on the host's loopback, and one on every interface:
    // neither is declared, and neither may hear from the guest.
    let other = Target::listen();
    let wide = Target::serve(TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("listener"));
    let (_keep, dir) = channel_dir();
    let declared = connections(&dir, api.port, control.port);

    let mut targets = vec![
        ("api", format!("127.0.0.1:{API_PORT}")),
        ("undeclared", format!("127.0.0.1:{UNDECLARED_PORT}")),
        // The host's other loopback listener, by the port it has on the host.
        ("other", format!("127.0.0.1:{}", other.port)),
        // The gateway address a NAT backend would give the host.
        ("gateway", format!("192.168.127.1:{}", wide.port)),
    ];
    match lan_address() {
        Some(address) => targets.push(("lan", format!("{address}:{}", wide.port))),
        None => eprintln!("this host has no non-loopback address; the lan row is not run"),
    }
    let record = host
        .start("reach", &dir, &declared, &targets)
        .expect("start");

    assert_eq!(
        control.line("the guest's start line", REPORT_BOUND),
        "start"
    );
    let report = control.line("the guest's report", REPORT_BOUND);
    eprintln!("guest: {report}");
    assert_eq!(
        outcome(&report, "api").0,
        "hello-api",
        "the echo, read back"
    );
    assert_eq!(
        api.line("the guest's line at the echo", REPORT_BOUND),
        "hello-api"
    );
    assert_eq!(
        outcome(&report, "undeclared").0,
        "ConnectionRefused",
        "an undeclared guest port has no listener"
    );
    assert_eq!(outcome(&report, "other").0, "ConnectionRefused");
    for label in ["gateway", "lan"] {
        if !report.contains(&format!(" {label}=")) {
            continue;
        }
        let (reached, _) = outcome(&report, label);
        assert!(
            !reached.starts_with("hello"),
            "the guest reached the host at a non-loopback address: {report}"
        );
        assert_ne!(reached, "eof", "{label} connected: {report}");
    }
    assert!(
        other.silent(),
        "the second loopback listener heard something"
    );
    assert!(wide.silent(), "the all-interfaces listener heard something");

    let receipt = host.receipt(&record.run_id);
    assert_eq!(
        receipt.connections.as_ref().unwrap().digest,
        format!("blake3-256:{}", blake3::hash(declared.as_bytes()).to_hex())
    );
    let connection = recorded(&receipt, "api");
    assert_eq!(connection.target, format!("127.0.0.1:{}", api.port));
    assert_eq!(
        (connection.accepted, connection.dialed, connection.refused),
        (1, 1, 0),
        "{connection:?}"
    );
    assert_eq!(connection.foreign_peers, 0);
    assert!(connection.complete);
    assert_eq!(connection.bytes_to_target, "hello-api\n".len() as u64);
    assert_eq!(connection.bytes_from_target, "hello-api\n".len() as u64);
    assert_eq!(recorded(&receipt, "ctl").dialed, 1);
    assert!(!Path::new(&endpoint(&dir, "api")).exists(), "unlinked");
}

/// The target is down when the guest dials and comes up while the broker
/// holds the connection: the connection works.
#[test]
fn a_target_that_comes_up_after_the_dial_serves_the_held_connection() {
    let _serial = common::serial();
    let Some(host) = host() else { return };
    let control = Target::listen();
    let port = unused_port();
    let (_keep, dir) = channel_dir();
    let record = host
        .start(
            "late",
            &dir,
            &connections(&dir, port, control.port),
            &[("api", format!("127.0.0.1:{API_PORT}"))],
        )
        .expect("start");
    // EVENT: the guest is up and about to dial a target nothing serves yet.
    assert_eq!(
        control.line("the guest's start line", REPORT_BOUND),
        "start"
    );
    let api = Target::serve(TcpListener::bind((Ipv4Addr::LOCALHOST, port)).expect("the target"));
    let report = control.line("the guest's report", REPORT_BOUND);
    eprintln!("guest: {report}");
    assert_eq!(outcome(&report, "api").0, "hello-api");
    assert_eq!(
        api.line("the held connection's line", REPORT_BOUND),
        "hello-api"
    );
    let receipt = host.receipt(&record.run_id);
    let connection = recorded(&receipt, "api");
    assert_eq!(
        (connection.accepted, connection.dialed, connection.refused),
        (1, 1, 0),
        "{connection:?}"
    );
}

/// The target never comes up: the guest reads end-of-file within the hold,
/// and the receipt counts one refusal.
#[test]
fn a_target_that_never_comes_up_ends_the_connection_within_the_hold() {
    let _serial = common::serial();
    let Some(host) = host() else { return };
    let control = Target::listen();
    let (_keep, dir) = channel_dir();
    let record = host
        .start(
            "never",
            &dir,
            &connections(&dir, unused_port(), control.port),
            &[("api", format!("127.0.0.1:{API_PORT}"))],
        )
        .expect("start");
    assert_eq!(
        control.line("the guest's start line", REPORT_BOUND),
        "start"
    );
    let report = control.line("the guest's report", HOLD + HOLD_SLACK + REPORT_BOUND);
    eprintln!("guest: {report}");
    let (ended, millis) = outcome(&report, "api");
    assert!(
        ended == "eof" || ended == "read-ConnectionReset",
        "no byte arrives from a target that never listened: {report}"
    );
    let held = Duration::from_millis(millis);
    assert!(
        held >= HOLD - Duration::from_secs(1) && held <= HOLD + HOLD_SLACK,
        "the connection was held {held:?}; the hold is {HOLD:?}"
    );
    let receipt = host.receipt(&record.run_id);
    let connection = recorded(&receipt, "api");
    assert_eq!(
        (connection.accepted, connection.dialed, connection.refused),
        (1, 0, 1),
        "{connection:?}"
    );
    assert!(connection.complete);
}

/// Two runs of one bundle at once, each with its own channel directory, both
/// reach the echo; a third naming a live run's directory is refused by name.
#[test]
fn two_concurrent_runs_each_reach_the_echo_and_a_shared_directory_is_refused() {
    let _serial = common::serial();
    let Some(host) = host() else { return };
    let api = Target::listen();
    let targets = [("api", format!("127.0.0.1:{API_PORT}"))];
    let mut runs = Vec::new();
    for name in ["first", "second"] {
        let control = Target::listen();
        let (keep, dir) = channel_dir();
        let declared = connections(&dir, api.port, control.port);
        let record = host.start(name, &dir, &declared, &targets).expect("start");
        runs.push((record, control, keep, dir, declared));
    }
    // Both guests are live at once before either is ended.
    for (_, control, ..) in &runs {
        assert_eq!(control.line("a guest's start line", REPORT_BOUND), "start");
    }
    for (_, control, ..) in &runs {
        let report = control.line("a guest's report", REPORT_BOUND);
        assert_eq!(outcome(&report, "api").0, "hello-api", "{report}");
    }
    assert_eq!(api.line("one guest's line", REPORT_BOUND), "hello-api");
    assert_eq!(
        api.line("the other guest's line", REPORT_BOUND),
        "hello-api"
    );

    // The first run is still live: its directory is taken.
    let (_, _, _, dir, declared) = &runs[0];
    let error = host
        .start("third", dir, declared, &targets)
        .expect_err("a live run holds the directory");
    assert_eq!(error.code, ErrorCode::ResourceConflict, "{error}");
    assert!(error.detail.contains("connection `api`"), "{error}");
    assert!(error.detail.contains("endpoint exists"), "{error}");

    for (record, ..) in &runs {
        let receipt = host.receipt(&record.run_id);
        let connection = recorded(&receipt, "api");
        assert_eq!(
            (
                connection.accepted,
                connection.dialed,
                connection.foreign_peers
            ),
            (1, 1, 0),
            "each run's receipt counts its own connection only: {connection:?}"
        );
    }
}
