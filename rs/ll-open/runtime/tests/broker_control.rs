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
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use leyline_runtime::ExecutionError;
use leyline_runtime::broker::control::{
    Args, BrokerProcess, Event, Request, WireOutcome, line, parse_args, serve,
};
use leyline_runtime::broker::relay::Limits;
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
                None,
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
            peer_pid: None
        })
    );
    assert_eq!(
        parse(&["--peer-pid", "42", "--lifeline-fd", "3"]),
        Ok(Args {
            lifeline_fd: 3,
            peer_pid: Some(42)
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
            connections_digest: format!("blake3-256:{}", doc.digest_hex())
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
    broker.to_broker.shutdown(Shutdown::Write).unwrap();
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
    broker.to_broker.shutdown(Shutdown::Write).unwrap();
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
    // The endpoint the refused start bound is the supervisor's to unlink.
    std::fs::remove_file(&doc.connections()[0].endpoint_unix).unwrap();

    let blocker = real.join("b.api.sock");
    std::fs::write(&blocker, b"").unwrap();
    assert!(refused(&start(&canonical(&doc)), false).contains("bind endpoint"));
}

#[test]
fn the_broker_process_exits_when_its_lifeline_ends() {
    let (_dir, real) = channel_dir();
    let doc = document(&real, "127.0.0.1", 9);
    let mut broker = BrokerProcess::spawn(Path::new(BROKER), &doc, None).expect("spawn");
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
    let mut broker = BrokerProcess::spawn(Path::new(BROKER), &doc, None).expect("spawn");
    let digest = format!("blake3-256:{}", doc.digest_hex());
    let result = broker.start(&doc, &digest, Duration::from_secs(5));
    if cfg!(target_os = "macos") {
        match result {
            Ok(()) => {}
            // An outer sandbox can refuse the profile; broker_profile owns
            // the LEYLINE_REQUIRE_SANDBOX gate for that.
            Err(error) => assert!(error.detail.contains("profile"), "{error:?}"),
        }
    } else {
        let error = result.expect_err("not delivered off macOS");
        assert!(error.detail.contains("macOS only"), "{error:?}");
    }
    broker.shutdown();
    assert!(!Path::new(&doc.connections()[0].endpoint_unix).exists());
}
