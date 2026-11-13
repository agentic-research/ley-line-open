//! The confined broker (ADR-0046 S5 "Bind, then confine"; S11 "Profile",
//! macOS): the real `leyline-conn-broker` binary relays under its profile (the
//! positive control), attests the digest it bound, dies with its lifeline, and
//! under the profile a process is refused every port but the declared ones,
//! every file, exec, fork and name resolution (the mDNSResponder socket
//! itself stays reachable: bead `ley-line-open-fb5c4b`).
//!
//! Fail closed like `confinement_local_connect_probe`: a host that cannot apply
//! the profile prints a named `SKIPPED (NOT a pass)`, and
//! `LEYLINE_REQUIRE_SANDBOX=1` makes that a failure.
#![cfg(target_os = "macos")]

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use leyline_runtime::broker::control::{BrokerProcess, Event, WireOutcome};
use leyline_runtime::broker::profile;
use leyline_runtime::broker::relay::PeerPid;
use leyline_schema_spec::connections::{Connection, ConnectionsDocument};

const BROKER: &str = env!("CARGO_BIN_EXE_leyline-conn-broker");
const PROBE_ENV: &str = "LLO_BROKER_PROFILE_PROBE";

fn sandbox_unavailable(reason: &str) {
    assert!(
        std::env::var_os("LEYLINE_REQUIRE_SANDBOX").is_none(),
        "LEYLINE_REQUIRE_SANDBOX is set, but the broker profile could not be applied: \
         {reason}. Refusing to report a pass for a confinement test that confined nothing."
    );
    eprintln!(
        "SKIPPED (NOT a pass): the broker profile could not be applied on this host \
         ({reason}); most often an outer sandbox (Seatbelt refuses to nest). Set \
         LEYLINE_REQUIRE_SANDBOX=1 to make this a failure."
    );
}

fn channel_dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("ch")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let real = std::fs::canonicalize(dir.path()).unwrap();
    (dir, real)
}

fn document(dir: &Path, ports: &[u16]) -> ConnectionsDocument {
    ConnectionsDocument::new(
        ports
            .iter()
            .enumerate()
            .map(|(i, port)| Connection {
                name: format!("c{i}"),
                endpoint_unix: dir.join(format!("b.c{i}.sock")).to_str().unwrap().into(),
                host: "127.0.0.1".parse().unwrap(),
                port: *port,
            })
            .collect(),
    )
    .unwrap()
}

fn echo() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            thread::spawn(move || {
                let mut buffer = [0u8; 4096];
                while let Ok(n) = stream.read(&mut buffer) {
                    if n == 0 || stream.write_all(&buffer[..n]).is_err() {
                        break;
                    }
                }
                let _ = stream.shutdown(Shutdown::Write);
            });
        }
    });
    port
}

fn digest(document: &ConnectionsDocument) -> String {
    format!("blake3-256:{}", document.digest_hex())
}

/// Spawn and start the real broker; `None` when the host cannot sandbox.
fn started(document: &ConnectionsDocument) -> Option<BrokerProcess> {
    let mut broker = BrokerProcess::spawn(Path::new(BROKER), PeerPid::Any).expect("spawn");
    match broker.start(document, &digest(document), Duration::from_secs(5)) {
        Ok(()) => Some(broker),
        Err(error) if error.detail.contains("profile") => {
            sandbox_unavailable(&error.detail);
            None
        }
        Err(error) => panic!("broker did not start: {error:?}"),
    }
}

fn round_trip(path: &str, bytes: &[u8]) -> Vec<u8> {
    let mut client = UnixStream::connect(path).expect("connect endpoint");
    client
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    client.write_all(bytes).unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    let mut back = Vec::new();
    client.read_to_end(&mut back).unwrap();
    back
}

#[test]
fn the_confined_broker_relays_every_connection_and_reports_it() {
    let (_dir, real) = channel_dir();
    let ports = [echo(), echo()];
    let doc = document(&real, &ports);
    let Some(mut broker) = started(&doc) else {
        return;
    };
    for connection in doc.connections() {
        assert_eq!(
            round_trip(&connection.endpoint_unix, b"confined"),
            b"confined"
        );
    }
    assert!(broker.drain(Duration::from_secs(5)), "the broker drained");
    let dialed = broker
        .events()
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::Connection {
                    outcome: WireOutcome::Dialed,
                    bytes_to_target: 8,
                    bytes_from_target: 8,
                    ..
                }
            )
        })
        .count();
    assert_eq!(dialed, 2, "{:?}", broker.events());
    broker.shutdown();
    for connection in doc.connections() {
        assert!(
            !Path::new(&connection.endpoint_unix).exists(),
            "the supervisor unlinks the endpoints"
        );
    }
}

#[test]
fn a_ready_digest_other_than_the_grants_refuses_the_start() {
    let (_dir, real) = channel_dir();
    let doc = document(&real, &[echo()]);
    let mut broker = BrokerProcess::spawn(Path::new(BROKER), PeerPid::Any).unwrap();
    let wrong = format!("blake3-256:{}", "0".repeat(64));
    let error = broker
        .start(&doc, &wrong, Duration::from_secs(5))
        .expect_err("mismatch");
    if error.detail.contains("profile") {
        sandbox_unavailable(&error.detail);
        return;
    }
    assert_eq!(
        error.code,
        leyline_runtime::ErrorCode::IdentityPolicyMismatch
    );
}

#[test]
fn the_confined_broker_exits_when_its_lifeline_ends() {
    let (_dir, real) = channel_dir();
    let doc = document(&real, &[echo()]);
    let Some(mut broker) = started(&doc) else {
        return;
    };
    broker.close_lifeline();
    let started = Instant::now();
    while !broker.exited() {
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the confined broker outlived its supervisor's lifeline"
        );
        thread::yield_now();
    }
}

/// The confined half, run by the parent below.
#[test]
#[ignore = "the confined child of the_profile_refuses_everything_but_the_declared_ports"]
fn profile_probe_child() {
    let spec = std::env::var(PROBE_ENV).expect("probe ports");
    let mut parts = spec.split(',');
    let declared: u16 = parts.next().unwrap().parse().unwrap();
    let undeclared: u16 = parts.next().unwrap().parse().unwrap();
    let other_socket = parts.next().unwrap().to_owned();
    let doc = ConnectionsDocument::new(vec![Connection {
        name: "c0".into(),
        endpoint_unix: "/private/tmp/unused.sock".into(),
        host: "127.0.0.1".parse().unwrap(),
        port: declared,
    }])
    .unwrap();
    if let Err(error) = profile::apply(&doc) {
        println!("PROBE:apply-refused {}", error.detail);
        return;
    }
    let outcome = |ok: bool| if ok { "ok" } else { "denied" };
    let mut report = vec![
        format!(
            "declared={}",
            outcome(TcpStream::connect(("127.0.0.1", declared)).is_ok())
        ),
        format!(
            "undeclared={}",
            match TcpStream::connect(("127.0.0.1", undeclared)) {
                Ok(_) => "ok".to_owned(),
                Err(e) if e.raw_os_error() == Some(libc::EPERM) => "eperm".to_owned(),
                Err(e) => format!("other:{e}"),
            }
        ),
        format!("read={}", outcome(std::fs::read("/etc/hosts").is_ok())),
        format!(
            "write={}",
            outcome(std::fs::write("/private/tmp/llo-broker-probe", b"x").is_ok())
        ),
        format!(
            "exec={}",
            outcome(Command::new("/usr/bin/true").status().is_ok())
        ),
        format!(
            "mdns={}",
            outcome(UnixStream::connect(profile::MDNS_RESPONDER).is_ok())
        ),
        format!(
            "unix={}",
            outcome(UnixStream::connect(&other_socket).is_ok())
        ),
        format!(
            "listen={}",
            outcome(TcpListener::bind(("127.0.0.1", 0)).is_ok())
        ),
        format!(
            "resolve={}",
            outcome(std::net::ToSocketAddrs::to_socket_addrs("example.com:80").is_ok())
        ),
    ];
    // SAFETY: if fork succeeds the child exits at once without touching state.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // SAFETY: immediate exit in the forked child.
        unsafe { libc::_exit(0) };
    }
    report.push(format!("fork={}", outcome(pid > 0)));
    println!("PROBE:{}", report.join(" "));
}

#[test]
fn the_profile_refuses_everything_but_the_declared_ports() {
    let declared = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let undeclared = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let (_dir, real) = channel_dir();
    let other_socket = real.join("other.sock");
    let _other = std::os::unix::net::UnixListener::bind(&other_socket).unwrap();
    let ports = format!(
        "{},{},{}",
        declared.local_addr().unwrap().port(),
        undeclared.local_addr().unwrap().port(),
        other_socket.display()
    );
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "profile_probe_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(PROBE_ENV, ports)
        .output()
        .expect("run the confined child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .find_map(|line| line.split_once("PROBE:").map(|(_, report)| report))
        .unwrap_or_else(|| {
            panic!(
                "the child reported nothing\nstdout:\n{stdout}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
    if let Some(reason) = line.strip_prefix("apply-refused ") {
        sandbox_unavailable(reason);
        return;
    }
    assert_eq!(
        line,
        // mdns=ok is measured, not intended: nono emits its DNS allow after
        // every platform rule (bead ley-line-open-fb5c4b). Resolution itself
        // is still refused, and the row flips here when that bead lands.
        "declared=ok undeclared=eperm read=denied write=denied exec=denied mdns=ok \
         unix=denied listen=denied resolve=denied fork=denied"
    );
}
