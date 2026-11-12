//! The broker's control protocol and process lifecycle, on every platform
//! (ADR-0046 S5, S7; bead `ley-line-open-52b5b3`).
//!
//! `serve` runs in-process here with an injected `confine` (the binary passes
//! the macOS profile), so start, ready attestation, relay records, drain and
//! every start refusal are exercised where the Linux mutation job runs. The
//! real binary's lifeline and its off-macOS refusal are checked too; the
//! confined binary is `broker_profile` (macOS).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use leyline_runtime::ExecutionError;
use leyline_runtime::broker::control::{
    Args, BrokerProcess, Event, Request, WireOutcome, line, parse_args, serve,
};
use leyline_runtime::broker::relay::{Limits, PeerPid};
use leyline_schema_spec::connections::{Connection, ConnectionsDocument};

const BROKER: &str = env!("CARGO_BIN_EXE_leyline-conn-broker");

fn channel_dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("ch")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let real = std::fs::canonicalize(dir.path()).unwrap();
    (dir, real)
}

fn document(dir: &Path, host: &str, port: u16) -> ConnectionsDocument {
    ConnectionsDocument::new(vec![Connection {
        name: "api".into(),
        endpoint_unix: dir.join("b.api.sock").to_str().unwrap().into(),
        host: host.parse().unwrap(),
        port,
    }])
    .unwrap()
}

fn echo() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            thread::spawn(move || {
                let mut buffer = [0u8; 1024];
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

/// A broker served in-process: the supervisor's ends of the control pipe.
struct InProcess {
    to_broker: UnixStream,
    from_broker: BufReader<UnixStream>,
    served: thread::JoinHandle<Result<(), String>>,
}

impl InProcess {
    fn new<C>(confine: C) -> Self
    where
        C: FnOnce(&ConnectionsDocument) -> Result<(), ExecutionError> + Send + 'static,
    {
        let (to_broker, broker_in) = UnixStream::pair().unwrap();
        let (broker_out, from_broker) = UnixStream::pair().unwrap();
        from_broker
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let served = thread::spawn(move || {
            serve(
                BufReader::new(broker_in),
                broker_out,
                PeerPid::Any,
                Limits::default(),
                confine,
            )
        });
        Self {
            to_broker,
            from_broker: BufReader::new(from_broker),
            served,
        }
    }

    fn send(&mut self, request: &Request) {
        self.to_broker.write_all(line(request).as_bytes()).unwrap();
    }

    fn start(&mut self, text: &str) {
        self.send(&Request::Start {
            document: text.to_owned(),
        });
    }

    fn event(&mut self) -> Event {
        let mut text = String::new();
        self.from_broker.read_line(&mut text).expect("an event");
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{text:?}: {e}"))
    }
}

fn canonical(document: &ConnectionsDocument) -> String {
    String::from_utf8(document.canonical_bytes()).unwrap()
}

#[test]
fn arguments_parse_in_either_order_and_refuse_by_name() {
    let parse = |args: &[&str]| parse_args(args.iter().map(|a| (*a).to_owned()));
    assert_eq!(
        parse(&["--lifeline-fd", "3"]),
        Ok(Args {
            lifeline_fd: 3,
            peer: PeerPid::Any
        })
    );
    assert_eq!(
        parse(&["--peer-pid", "42", "--lifeline-fd", "3"]),
        Ok(Args {
            lifeline_fd: 3,
            peer: PeerPid::Exactly(42)
        })
    );
    assert_eq!(
        parse(&["--await-peer", "--lifeline-fd", "3"]),
        Ok(Args {
            lifeline_fd: 3,
            peer: PeerPid::Awaiting
        })
    );
    for (args, why) in [
        (&[][..], "--lifeline-fd is required"),
        (&["--peer-pid", "1"][..], "--lifeline-fd is required"),
        (&["--lifeline-fd"][..], "needs a value"),
        (&["--lifeline-fd", "x"][..], "is not a number"),
        (
            &["--lifeline-fd", "3", "--lifeline-fd", "4"][..],
            "given twice",
        ),
        (&["--other", "3"][..], "unknown argument"),
        (
            &["--lifeline-fd", "3", "--peer-pid", "0"][..],
            "not a process id",
        ),
        (
            &["--lifeline-fd", "3", "--peer-pid", "-4"][..],
            "not a process id",
        ),
        (
            &["--lifeline-fd", "3", "--await-peer", "--peer-pid", "7"][..],
            "name the peer once",
        ),
        (
            &["--lifeline-fd", "3", "--await-peer", "--await-peer"][..],
            "name the peer once",
        ),
    ] {
        let error = parse(args).expect_err(why);
        assert!(error.contains(why), "{args:?}: {error}");
    }
}

#[test]
fn serve_binds_then_confines_then_attests_relays_and_drains() {
    let (_dir, real) = channel_dir();
    let doc = document(&real, "127.0.0.1", echo());
    let endpoint = doc.connections()[0].endpoint_unix.clone();
    let confined_after_bind = {
        let endpoint = endpoint.clone();
        move |confined: &ConnectionsDocument| {
            assert!(
                Path::new(&endpoint).exists(),
                "every endpoint is bound before the broker confines itself"
            );
            assert_eq!(confined.connections()[0].name, "api");
            Ok(())
        }
    };
    let mut broker = InProcess::new(confined_after_bind);
    broker.start(&canonical(&doc));
    assert_eq!(
        broker.event(),
        Event::Ready {
            connections_digest: format!("blake3-256:{}", doc.digest_hex()),
            bound: vec![leyline_runtime::broker::control::Bound {
                path: endpoint.clone(),
                device: std::fs::symlink_metadata(&endpoint).unwrap().dev(),
                inode: std::fs::symlink_metadata(&endpoint).unwrap().ino(),
            }],
        }
    );

    let mut client = UnixStream::connect(&endpoint).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    client.write_all(b"ping").unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    let mut back = Vec::new();
    client.read_to_end(&mut back).unwrap();
    assert_eq!(back, b"ping");
    assert_eq!(
        broker.event(),
        Event::Connection {
            name: "api".into(),
            outcome: WireOutcome::Dialed,
            bytes_to_target: 4,
            bytes_from_target: 4,
            open: false,
        }
    );

    // A connection still open at drain is reported as open.
    let mut open = UnixStream::connect(&endpoint).unwrap();
    open.write_all(b"x").unwrap();
    let mut one = [0u8; 1];
    open.set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    open.read_exact(&mut one).unwrap();
    broker.send(&Request::Drain);
    assert_eq!(
        broker.event(),
        Event::Connection {
            name: "api".into(),
            outcome: WireOutcome::Dialed,
            bytes_to_target: 1,
            bytes_from_target: 1,
            open: true,
        }
    );
    assert_eq!(broker.event(), Event::Drained);
    assert_eq!(broker.served.join().unwrap(), Ok(()));
}

#[test]
fn closing_the_control_pipe_ends_serve_without_a_drain() {
    let (_dir, real) = channel_dir();
    let doc = document(&real, "127.0.0.1", echo());
    let mut broker = InProcess::new(|_| Ok(()));
    broker.start(&canonical(&doc));
    assert!(matches!(broker.event(), Event::Ready { .. }));
    // serve may already have refused and closed its end.
    let _ = broker.to_broker.shutdown(Shutdown::Write);
    assert_eq!(broker.served.join().unwrap(), Ok(()));
}

/// Every start refusal reports an `error` event and returns the same detail.
fn refused(input: &str, confine_fails: bool) -> String {
    let mut broker = InProcess::new(move |_| {
        if confine_fails {
            Err(ExecutionError::backend("profile refused"))
        } else {
            Ok(())
        }
    });
    broker.to_broker.write_all(input.as_bytes()).unwrap();
    // serve may already have refused and closed its end.
    let _ = broker.to_broker.shutdown(Shutdown::Write);
    let Event::Error { detail } = broker.event() else {
        panic!("expected an error event");
    };
    assert_eq!(broker.served.join().unwrap(), Err(detail.clone()));
    detail
}

#[test]
fn serve_refuses_every_bad_start_by_name() {
    let (_dir, real) = channel_dir();
    let doc = document(&real, "127.0.0.1", 9);
    let start = |text: &str| {
        line(&Request::Start {
            document: text.to_owned(),
        })
    };

    assert!(refused("", false).contains("closed before start"));
    assert!(refused("not json\n", false).contains("unreadable start request"));
    assert!(refused(&line(&Request::Drain), false).contains("expected start"));
    assert!(refused(&start("{}"), false).contains("missing-key"));
    let respaced = canonical(&doc).replacen("{\n", "{ \n", 1);
    assert!(refused(&start(&respaced), false).contains("not canonical"));
    let other = document(&real, "127.0.0.2", 9);
    assert!(
        refused(&start(&canonical(&other)), false)
            .contains("loopback address not delivered by this runtime")
    );
    assert!(refused(&start(&canonical(&doc)), true).contains("profile refused"));
    // A broker that bound and then could not confine removes its own sockets:
    // no ready report tells the supervisor what it bound.
    assert!(!Path::new(&doc.connections()[0].endpoint_unix).exists());

    let blocker = real.join("b.api.sock");
    std::fs::write(&blocker, b"").unwrap();
    assert!(refused(&start(&canonical(&doc)), false).contains("bind endpoint"));
}

#[test]
fn the_broker_process_exits_when_its_lifeline_ends() {
    let (_dir, real) = channel_dir();
    let _doc = document(&real, "127.0.0.1", 9);
    let mut broker = BrokerProcess::spawn(Path::new(BROKER), PeerPid::Any).expect("spawn");
    // Nothing sent: the broker sits in its first read until the lifeline ends.
    thread::sleep(Duration::from_millis(100));
    assert!(
        !broker.exited(),
        "the broker waits while its supervisor lives"
    );
    broker.close_lifeline();
    let started = Instant::now();
    while !broker.exited() {
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the broker outlived its supervisor's lifeline"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_broker_process_refuses_off_macos_and_reports_ready_on_it() {
    let (_dir, real) = channel_dir();
    let doc = document(&real, "127.0.0.1", echo());
    let mut broker = BrokerProcess::spawn(Path::new(BROKER), PeerPid::Any).expect("spawn");
    let digest = format!("blake3-256:{}", doc.digest_hex());
    let result = broker.start(&doc, &digest, Duration::from_secs(5));
    if cfg!(target_os = "macos") {
        match result {
            Ok(()) => {}
            // An outer sandbox can refuse the profile: a named skip, unless the
            // host claims it can sandbox.
            Err(error) => {
                assert!(
                    std::env::var_os("LEYLINE_REQUIRE_SANDBOX").is_none(),
                    "LEYLINE_REQUIRE_SANDBOX is set but the broker did not start: {error:?}"
                );
                assert!(error.detail.contains("profile"), "{error:?}");
                eprintln!("SKIPPED (NOT a pass): {}", error.detail);
            }
        }
    } else {
        let error = result.expect_err("not delivered off macOS");
        assert!(error.detail.contains("macOS only"), "{error:?}");
    }
    broker.shutdown();
    assert!(!Path::new(&doc.connections()[0].endpoint_unix).exists());
}

/// Dropping the handle kills and reaps the broker.
#[test]
fn dropping_the_handle_ends_the_broker() {
    let broker = BrokerProcess::spawn(Path::new(BROKER), PeerPid::Any).unwrap();
    let pid = broker.pid() as i32;
    assert!(pid > 1);
    // SAFETY: signal 0 only checks that the process exists.
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "the broker is running");
    drop(broker);
    // SAFETY: as above.
    assert_ne!(
        unsafe { libc::kill(pid, 0) },
        0,
        "the broker is gone and reaped"
    );
}

/// The supervisor unlinks only sockets the broker reported binding, and only
/// while the path is still that socket: a node someone else put at an endpoint
/// path survives teardown.
#[test]
fn teardown_unlinks_only_the_sockets_the_broker_bound() {
    let (_dir, real) = channel_dir();
    let doc = document(&real, "127.0.0.1", 9);
    let endpoint = doc.connections()[0].endpoint_unix.clone();

    // A squatter's node at the path: the broker's bind fails, it reports no
    // ready, and teardown leaves the node alone.
    std::fs::write(&endpoint, b"squatter").unwrap();
    let mut broker = BrokerProcess::spawn(Path::new(BROKER), PeerPid::Any).unwrap();
    let digest = format!("blake3-256:{}", doc.digest_hex());
    assert!(broker.start(&doc, &digest, Duration::from_secs(5)).is_err());
    broker.shutdown();
    assert_eq!(std::fs::read(&endpoint).unwrap(), b"squatter");
    std::fs::remove_file(&endpoint).unwrap();

    // The broker's own socket, replaced after ready by another node: kept.
    let mut broker = BrokerProcess::spawn(Path::new(BROKER), PeerPid::Any).unwrap();
    if broker.start(&doc, &digest, Duration::from_secs(5)).is_ok() {
        std::fs::remove_file(&endpoint).unwrap();
        std::fs::write(&endpoint, b"replaced").unwrap();
        broker.shutdown();
        assert_eq!(std::fs::read(&endpoint).unwrap(), b"replaced");
    }
}

// ---------------------------------------------------------------------------
// The process entry (`run_broker`) through the shipped binary, which refuses
// before its profile on every platform, and the supervisor's protocol through
// the test-only unconfined binary (`test-support`), which runs everywhere.

const UNCONFINED: &str = env!("CARGO_BIN_EXE_leyline-conn-broker-unconfined");

/// Run the shipped broker with descriptor limits (soft, hard) and a lifeline
/// held open; return its first stdout line, or None if it is still waiting
/// for a start request after 500 ms.
fn first_line_under_limits(args: &[&str], soft: u64, hard: u64) -> Option<String> {
    use std::os::unix::process::CommandExt;
    let mut fds = [0; 2];
    // SAFETY: a valid out array.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let (lifeline_read, lifeline_write) = (fds[0], fds[1]);
    let mut command = std::process::Command::new(BROKER);
    command
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    // SAFETY: async-signal-safe setrlimit/dup2 in the child before exec.
    unsafe {
        command.pre_exec(move || {
            let limit = libc::rlimit {
                rlim_cur: soft as libc::rlim_t,
                rlim_max: hard as libc::rlim_t,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0
                || libc::dup2(lifeline_read, 3) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    // SAFETY: the child holds its own copy now.
    unsafe { libc::close(lifeline_read) };
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = sender.send(line);
    });
    let line = receiver
        .recv_timeout(Duration::from_millis(500))
        .ok()
        .filter(|l| !l.is_empty());
    let _ = child.kill();
    let _ = child.wait();
    // SAFETY: closing our end.
    unsafe { libc::close(lifeline_write) };
    line
}

fn hard_limit() -> u64 {
    // SAFETY: a zeroed rlimit filled by getrlimit.
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: valid out-pointer.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) }, 0);
    #[allow(clippy::unnecessary_cast)]
    let hard = limit.rlim_max as u64;
    hard
}

#[test]
fn the_shipped_broker_refuses_bad_arguments_and_a_short_descriptor_limit() {
    use leyline_runtime::broker::control::FILE_LIMIT;
    let error = |line: Option<String>| match line.map(|l| serde_json::from_str::<Event>(&l)) {
        Some(Ok(Event::Error { detail })) => detail,
        other => panic!("expected an error event, got {other:?}"),
    };
    assert!(
        error(first_line_under_limits(&["--bogus"], 4096, hard_limit()))
            .contains("unknown argument")
    );
    let args = ["--lifeline-fd", "3"];
    // A hard limit below what the cap needs: refused by name, at once.
    assert!(
        error(first_line_under_limits(&args, FILE_LIMIT - 1, FILE_LIMIT - 1))
            .contains("descriptor limit")
    );
    // Exactly enough, or a low soft limit it can raise: it waits for start.
    assert_eq!(first_line_under_limits(&args, FILE_LIMIT, FILE_LIMIT), None);
    if hard_limit() > FILE_LIMIT {
        assert_eq!(first_line_under_limits(&args, 256, hard_limit()), None);
    }
}

fn unconfined(peer: PeerPid) -> BrokerProcess {
    BrokerProcess::spawn(Path::new(UNCONFINED), peer).expect("spawn")
}

#[test]
fn the_supervisor_handle_starts_relays_drains_and_unlinks() {
    let (_dir, real) = channel_dir();
    let doc = document(&real, "127.0.0.1", echo());
    let endpoint = doc.connections()[0].endpoint_unix.clone();
    let digest = format!("blake3-256:{}", doc.digest_hex());

    let mut broker = unconfined(PeerPid::Any);
    broker
        .start(&doc, &digest, Duration::from_secs(5))
        .expect("ready");
    let mut client = UnixStream::connect(&endpoint).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    client.write_all(b"hi").unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    let mut back = Vec::new();
    client.read_to_end(&mut back).unwrap();
    assert_eq!(back, b"hi");
    assert!(broker.drain(Duration::from_secs(5)), "drained");
    assert!(
        broker.events().iter().any(|e| matches!(
            e,
            Event::Connection {
                outcome: WireOutcome::Dialed,
                bytes_to_target: 2,
                ..
            }
        )),
        "{:?}",
        broker.events()
    );
    broker.shutdown();
    assert!(!Path::new(&endpoint).exists(), "the bound socket is unlinked");

    // A ready digest that is not the grant's refuses the start.
    let mut broker = unconfined(PeerPid::Any);
    let error = broker
        .start(
            &doc,
            &format!("blake3-256:{}", "0".repeat(64)),
            Duration::from_secs(5),
        )
        .expect_err("mismatch");
    assert_eq!(
        error.code,
        leyline_runtime::ErrorCode::IdentityPolicyMismatch
    );
    broker.shutdown();
    assert!(!Path::new(&endpoint).exists());
}

#[test]
fn an_awaiting_broker_refuses_every_peer_until_the_supervisor_names_it() {
    let (_dir, real) = channel_dir();
    let doc = document(&real, "127.0.0.1", echo());
    let endpoint = doc.connections()[0].endpoint_unix.clone();
    let mut broker = unconfined(PeerPid::Awaiting);
    broker
        .start(
            &doc,
            &format!("blake3-256:{}", doc.digest_hex()),
            Duration::from_secs(5),
        )
        .unwrap();
    let round_trip = || {
        let mut client = UnixStream::connect(&endpoint).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let _ = client.write_all(b"x");
        let _ = client.shutdown(Shutdown::Write);
        let mut back = Vec::new();
        let _ = client.read_to_end(&mut back);
        back
    };
    assert!(round_trip().is_empty(), "refused before the peer is named");
    broker.set_peer(std::process::id() as i32).unwrap();
    // The naming is asynchronous to the next connect: retry briefly.
    let started = Instant::now();
    while round_trip() != b"x" {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the named peer is never admitted"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert!(broker.drain(Duration::from_secs(5)));
    assert!(broker.events().iter().any(|e| matches!(
        e,
        Event::Connection {
            outcome: WireOutcome::ForeignPeer,
            ..
        }
    )));
}
