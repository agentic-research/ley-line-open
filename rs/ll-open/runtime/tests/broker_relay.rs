//! The broker's relay against a UNIX client and a TCP echo on loopback
//! (ADR-0046 S5, S11 "Relay"; bead `ley-line-open-52b5b3`).
//!
//! These drive `broker::relay` in-process, unconfined, on every platform. The
//! confined broker binary is `broker_profile` (macOS).

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use leyline_runtime::broker::control::{Event, WireOutcome, summarize};
use leyline_runtime::broker::relay::{self, Limits, Outcome, PeerPid, PeerPolicy, Record, Report};
use leyline_runtime::broker::{MAX_CONNECTIONS, check_delivered};
use leyline_schema_spec::connections::{Connection, ConnectionsDocument};

/// A private channel directory, realpath'd, as cloister makes one.
fn channel_dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("ch")
        .tempdir_in("/tmp")
        .expect("tempdir");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let real = std::fs::canonicalize(dir.path()).unwrap();
    (dir, real)
}

fn document(dir: &Path, targets: &[(&str, u16)]) -> ConnectionsDocument {
    ConnectionsDocument::new(
        targets
            .iter()
            .map(|(name, port)| Connection {
                name: (*name).into(),
                endpoint_unix: dir.join(format!("b.{name}.sock")).to_str().unwrap().into(),
                host: "127.0.0.1".parse().unwrap(),
                port: *port,
            })
            .collect(),
    )
    .expect("document")
}

fn euid() -> u32 {
    // SAFETY: geteuid cannot fail.
    unsafe { libc::geteuid() }
}

struct Running {
    _dir: tempfile::TempDir,
    path: PathBuf,
    records: mpsc::Receiver<Report>,
    handle: Option<relay::RelayHandle>,
}

impl Running {
    fn connect(&self) -> UnixStream {
        let stream = UnixStream::connect(&self.path).expect("connect endpoint");
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        stream
    }

    fn record(&self) -> Record {
        match self
            .records
            .recv_timeout(Duration::from_secs(20))
            .expect("a record")
        {
            Report::Record(record) => record,
            Report::Drained => panic!("drained before the expected record"),
        }
    }
}

fn run(port: u16, peers: Option<PeerPolicy>, limits: Limits) -> Running {
    let (dir, real) = channel_dir();
    let doc = document(&real, &[("api", port)]);
    let endpoints = relay::bind(&doc).expect("bind");
    let (sender, records) = mpsc::channel();
    let handle = relay::start(
        endpoints,
        peers.unwrap_or(PeerPolicy {
            uid: euid(),
            pid: PeerPid::Any,
        }),
        limits,
        sender,
    )
    .expect("start");
    Running {
        path: PathBuf::from(&doc.connections()[0].endpoint_unix),
        _dir: dir,
        records,
        handle: Some(handle),
    }
}

/// A TCP echo: each connection echoes until end-of-file, then half-closes.
fn echo() -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let thread = thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            thread::spawn(move || {
                let mut buffer = [0u8; 4096];
                loop {
                    match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if stream.write_all(&buffer[..n]).is_err() {
                                break;
                            }
                        }
                    }
                }
                let _ = stream.shutdown(Shutdown::Write);
            });
        }
    });
    (port, thread)
}

/// A port nothing listens on (bound and released).
fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn limits(hold: Duration) -> Limits {
    Limits {
        max_connections: MAX_CONNECTIONS,
        hold,
    }
}

#[test]
fn bytes_round_trip_and_the_record_counts_them() {
    let (port, _echo) = echo();
    let running = run(port, None, Limits::default());
    let mut client = running.connect();
    client.write_all(b"hello broker").unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    let mut back = Vec::new();
    client.read_to_end(&mut back).unwrap();
    assert_eq!(back, b"hello broker");
    let record = running.record();
    assert_eq!(
        record,
        Record {
            name: "api".into(),
            outcome: Outcome::Dialed,
            bytes_to_target: 12,
            bytes_from_target: 12,
            open: false,
        }
    );
}

#[test]
fn half_close_from_the_client_keeps_the_upstream_direction_open() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut got = Vec::new();
        stream.read_to_end(&mut got).unwrap();
        stream.write_all(b"after your eof").unwrap();
        got
    });
    let running = run(port, None, Limits::default());
    let mut client = running.connect();
    client.write_all(b"done").unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    let mut back = Vec::new();
    client.read_to_end(&mut back).unwrap();
    assert_eq!(server.join().unwrap(), b"done");
    assert_eq!(back, b"after your eof");
}

#[test]
fn half_close_from_the_upstream_keeps_the_client_direction_open() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.write_all(b"banner").unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        let mut got = Vec::new();
        stream.read_to_end(&mut got).unwrap();
        got
    });
    let running = run(port, None, Limits::default());
    let mut client = running.connect();
    let mut banner = [0u8; 6];
    client.read_exact(&mut banner).unwrap();
    assert_eq!(&banner, b"banner");
    let mut rest = Vec::new();
    client.read_to_end(&mut rest).unwrap();
    assert!(
        rest.is_empty(),
        "the upstream's half-close reaches the client"
    );
    client.write_all(b"still talking").unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    assert_eq!(server.join().unwrap(), b"still talking");
}

#[test]
fn an_upstream_that_comes_up_within_the_hold_is_reached() {
    let port = free_port();
    let running = run(port, None, Limits::default());
    let mut client = running.connect();
    client.write_all(b"early").unwrap();
    thread::sleep(Duration::from_millis(400));
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("rebind the port");
    let (mut stream, _) = listener.accept().unwrap();
    let mut buffer = [0u8; 5];
    stream.read_exact(&mut buffer).unwrap();
    assert_eq!(&buffer, b"early", "bytes written during the hold arrive");
    stream.write_all(b"late").unwrap();
    drop(stream);
    let mut back = Vec::new();
    client.read_to_end(&mut back).unwrap();
    assert_eq!(back, b"late");
    // The client side is still open (half-close): the record comes when it ends.
    drop(client);
    assert_eq!(running.record().outcome, Outcome::Dialed);
}

#[test]
fn a_hold_that_expires_ends_in_end_of_file_within_the_bound() {
    let hold = Duration::from_millis(600);
    let running = run(free_port(), None, limits(hold));
    let started = Instant::now();
    let mut client = running.connect();
    let mut back = Vec::new();
    client.read_to_end(&mut back).unwrap();
    let elapsed = started.elapsed();
    assert!(back.is_empty());
    assert!(elapsed >= hold, "held for the bound: {elapsed:?}");
    assert!(
        elapsed <= hold + Duration::from_millis(200) + Duration::from_millis(300),
        "no more than the hold plus one backoff step (and scheduling): {elapsed:?}"
    );
    assert_eq!(running.record().outcome, Outcome::Refused);
}

#[test]
fn a_client_that_hangs_up_ends_the_hold_early() {
    let running = run(free_port(), None, limits(Duration::from_secs(10)));
    let client = running.connect();
    thread::sleep(Duration::from_millis(100));
    let started = Instant::now();
    drop(client);
    let record = running.record();
    assert_eq!(record.outcome, Outcome::Refused);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "hang-up ended the hold: {:?}",
        started.elapsed()
    );
}

#[test]
fn an_upstream_reset_mid_stream_is_end_of_file_to_the_client() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.write_all(b"partial").unwrap();
        thread::sleep(Duration::from_millis(100));
        // SO_LINGER 0: close sends a reset.
        let linger = libc::linger {
            l_onoff: 1,
            l_linger: 0,
        };
        // SAFETY: valid option value and length.
        unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                (&linger as *const libc::linger).cast(),
                std::mem::size_of::<libc::linger>() as libc::socklen_t,
            );
        }
        drop(stream);
    });
    let running = run(port, None, Limits::default());
    let mut client = running.connect();
    let mut back = Vec::new();
    let result = client.read_to_end(&mut back);
    server.join().unwrap();
    assert!(
        result.is_ok(),
        "the client sees end-of-file, not an error: {result:?}"
    );
    assert_eq!(back, b"partial");
    assert_eq!(running.record().outcome, Outcome::Dialed);
}

#[test]
fn connections_past_the_cap_are_closed_without_a_dial() {
    let running = run(
        free_port(),
        None,
        Limits {
            max_connections: 2,
            hold: Duration::from_secs(10),
        },
    );
    let held: Vec<UnixStream> = (0..2).map(|_| running.connect()).collect();
    thread::sleep(Duration::from_millis(300));
    let mut third = running.connect();
    let mut back = Vec::new();
    third.read_to_end(&mut back).unwrap();
    assert!(back.is_empty());
    let record = running.record();
    assert_eq!(
        record.outcome,
        Outcome::Refused,
        "the third is refused at once"
    );
    drop(held);
    for _ in 0..2 {
        assert_eq!(running.record().outcome, Outcome::Refused);
    }
}

#[test]
fn a_second_loopback_listener_receives_nothing() {
    let (port, _echo) = echo();
    let bystander = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    bystander.set_nonblocking(true).unwrap();
    let running = run(port, None, Limits::default());
    let mut client = running.connect();
    client.write_all(b"only the target").unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    let mut back = Vec::new();
    client.read_to_end(&mut back).unwrap();
    assert_eq!(back, b"only the target");
    assert_eq!(
        bystander.accept().unwrap_err().kind(),
        ErrorKind::WouldBlock,
        "the broker dialed nothing but its target"
    );
}

#[test]
fn a_passed_descriptor_never_reaches_the_target_and_is_closed() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut got = Vec::new();
        stream.read_to_end(&mut got).unwrap();
        got
    });
    let running = run(port, None, Limits::default());
    let client = running.connect();

    let mut pipe = [0; 2];
    // SAFETY: a valid out array.
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    let (pipe_read, pipe_write) = (pipe[0], pipe[1]);
    let passed = identity(pipe_write);
    send_with_fd(&client, b"x", pipe_write);
    // SAFETY: closing our own copy of the write end.
    unsafe { libc::close(pipe_write) };
    client.shutdown(Shutdown::Write).unwrap();
    assert_eq!(server.join().unwrap(), b"x", "only the byte crosses");
    drop(client);
    assert_eq!(running.record().outcome, Outcome::Dialed);

    // The relay (this process) read the message: it must hold no descriptor
    // for the passed pipe end. `read(2)` never installs a passed right.
    for entry in std::fs::read_dir("/dev/fd").unwrap() {
        let Ok(fd) = entry.unwrap().file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if fd != pipe_read {
            assert_ne!(
                identity(fd),
                passed,
                "descriptor {fd} is the passed pipe end: the relay received the right"
            );
        }
    }
    // The relay closed the right it received, and ours is closed, so no
    // process holds the write end: the pipe reads end-of-file. (A plain
    // read(2) would not do: macOS installs the right in the reader anyway.)
    {
        let started = Instant::now();
        loop {
            let mut pollfd = libc::pollfd {
                fd: pipe_read,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd.
            if unsafe { libc::poll(&mut pollfd, 1, 100) } > 0 {
                let mut byte = 0u8;
                // SAFETY: one-byte read into a valid buffer.
                let n = unsafe { libc::read(pipe_read, (&mut byte as *mut u8).cast(), 1) };
                assert_eq!(n, 0, "end-of-file: no process holds the passed descriptor");
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the passed descriptor is still open somewhere"
            );
        }
    }
    // SAFETY: closing our read end.
    unsafe { libc::close(pipe_read) };
}

/// A descriptor passed while the connection is held for its dial is never
/// read (the hang-up check uses FIONREAD, not a peek, which would install it
/// on macOS), and is released when the broker closes the connection.
#[test]
fn a_descriptor_passed_during_the_hold_is_released_with_the_connection() {
    let running = run(free_port(), None, limits(Duration::from_secs(10)));
    let client = running.connect();
    let mut pipe = [0; 2];
    // SAFETY: a valid out array.
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    let (pipe_read, pipe_write) = (pipe[0], pipe[1]);
    send_with_fd(&client, b"x", pipe_write);
    // SAFETY: closing our own copy of the write end.
    unsafe { libc::close(pipe_write) };
    // Several hold rounds pass with the message queued.
    thread::sleep(Duration::from_millis(300));
    drop(client);
    assert_eq!(running.record().outcome, Outcome::Refused);
    let mut pollfd = libc::pollfd {
        fd: pipe_read,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    assert!(
        unsafe { libc::poll(&mut pollfd, 1, 5_000) } > 0,
        "the passed write end is still open somewhere"
    );
    let mut byte = 0u8;
    // SAFETY: one-byte read into a valid buffer.
    assert_eq!(
        unsafe { libc::read(pipe_read, (&mut byte as *mut u8).cast(), 1) },
        0
    );
    // SAFETY: closing our read end.
    unsafe { libc::close(pipe_read) };
}

/// `(st_dev, st_ino)` of an open descriptor, or `None` when it is not open.
fn identity(fd: i32) -> Option<(u64, u64)> {
    // SAFETY: a zeroed stat filled by fstat.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstat on a descriptor number with a valid out-pointer.
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)]
    Some((stat.st_dev as u64, stat.st_ino as u64))
}

fn send_with_fd(stream: &UnixStream, data: &[u8], fd: i32) {
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) } as usize;
    let mut control = vec![0u8; space];
    // SAFETY: a zeroed msghdr filled with valid pointers below.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = space as _;
    // SAFETY: the control buffer holds one cmsg carrying one fd.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<i32>(), fd);
        assert_eq!(
            libc::sendmsg(stream.as_raw_fd(), &message, 0),
            data.len() as isize
        );
    }
}

#[test]
fn a_peer_that_is_not_the_runs_is_closed_without_a_dial() {
    let (port, _echo) = echo();
    let running = run(
        port,
        Some(PeerPolicy {
            uid: euid(),
            // Not this test process: the microVM tier's worker-pid check.
            pid: PeerPid::Exactly(1),
        }),
        Limits::default(),
    );
    let mut client = running.connect();
    let mut back = Vec::new();
    client.read_to_end(&mut back).unwrap();
    assert!(back.is_empty());
    assert_eq!(running.record().outcome, Outcome::ForeignPeer);

    let other_uid = run(
        port,
        Some(PeerPolicy {
            uid: euid().wrapping_add(1),
            pid: PeerPid::Any,
        }),
        Limits::default(),
    );
    let mut client = other_uid.connect();
    client.read_to_end(&mut back).unwrap();
    assert_eq!(other_uid.record().outcome, Outcome::ForeignPeer);
}

#[test]
fn drain_reports_every_unfinished_connection_once() {
    let (port, _echo) = echo();
    let mut running = run(port, None, Limits::default());
    let mut open = running.connect();
    open.write_all(b"abc").unwrap();
    let mut buffer = [0u8; 3];
    open.read_exact(&mut buffer).unwrap();
    running.handle.take().unwrap().drain();
    let drained = running.record();
    assert_eq!(drained.outcome, Outcome::Dialed);
    assert!(drained.open);
    assert_eq!((drained.bytes_to_target, drained.bytes_from_target), (3, 3));
    assert_eq!(
        running
            .records
            .recv_timeout(Duration::from_secs(5))
            .unwrap(),
        Report::Drained
    );
    // Ending it now reports nothing more: drain took it.
    drop(open);
    assert!(
        running
            .records
            .recv_timeout(Duration::from_millis(500))
            .is_err()
    );
}

/// Connections ending while the drain runs are reported exactly once each,
/// and every report precedes `Drained`, so a drained receipt is complete.
#[test]
fn a_drain_racing_closes_reports_each_connection_exactly_once_before_drained() {
    for round in 0..10 {
        let (port, _echo) = echo();
        let mut running = run(port, None, Limits::default());
        let clients: Vec<UnixStream> = (0..16)
            .map(|_| {
                let mut client = running.connect();
                client.write_all(b"z").unwrap();
                let mut one = [0u8; 1];
                client.read_exact(&mut one).unwrap();
                client
            })
            .collect();
        let closer = thread::spawn(move || drop(clients));
        running.handle.take().unwrap().drain();
        closer.join().unwrap();
        let mut records = 0;
        loop {
            match running
                .records
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
            {
                Report::Record(record) => {
                    assert_eq!(record.outcome, Outcome::Dialed);
                    records += 1;
                }
                Report::Drained => break,
            }
        }
        assert_eq!(
            records, 16,
            "round {round}: one report per accepted connection"
        );
        assert!(
            running
                .records
                .recv_timeout(Duration::from_millis(300))
                .is_err(),
            "round {round}: nothing after Drained"
        );
    }
}

/// The microVM tier names its peer after the broker starts: until then every
/// peer is refused, and the pid can be named once, never changed.
#[test]
fn an_awaited_peer_is_refused_until_named_and_named_once() {
    let (port, _echo) = echo();
    let running = run(
        port,
        Some(PeerPolicy {
            uid: euid(),
            pid: PeerPid::Awaiting,
        }),
        Limits::default(),
    );
    let mut early = running.connect();
    let mut back = Vec::new();
    early.read_to_end(&mut back).unwrap();
    assert_eq!(running.record().outcome, Outcome::ForeignPeer);

    let handle = running.handle.as_ref().unwrap();
    assert!(handle.set_peer_pid(0).is_err(), "not a pid");
    handle.set_peer_pid(std::process::id() as i32).unwrap();
    assert!(handle.set_peer_pid(1).is_err(), "named once");
    let mut client = running.connect();
    client.write_all(b"now").unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    back.clear();
    client.read_to_end(&mut back).unwrap();
    assert_eq!(back, b"now");
    assert_eq!(running.record().outcome, Outcome::Dialed);

    let any = run(port, None, Limits::default());
    assert!(
        any.handle.as_ref().unwrap().set_peer_pid(1).is_err(),
        "a peer that was never awaited cannot be narrowed or widened later"
    );
}

/// The production cap: 256 held connections, and the 257th is refused.
#[test]
fn the_cap_is_256_simultaneous_connections() {
    let limit = leyline_runtime::broker::control::raise_fd_limit(
        leyline_runtime::broker::control::FILE_LIMIT,
    );
    assert!(limit >= leyline_runtime::broker::control::FILE_LIMIT);
    let running = run(free_port(), None, limits(Duration::from_secs(20)));
    let held: Vec<UnixStream> = (0..MAX_CONNECTIONS).map(|_| running.connect()).collect();
    thread::sleep(Duration::from_millis(500));
    let mut over = running.connect();
    let mut back = Vec::new();
    over.read_to_end(&mut back).unwrap();
    assert_eq!(running.record().outcome, Outcome::Refused, "the 257th");
    drop(held);
    for _ in 0..MAX_CONNECTIONS {
        assert_eq!(running.record().outcome, Outcome::Refused);
    }
}

#[test]
fn only_127_0_0_1_is_delivered() {
    let (_dir, real) = channel_dir();
    for host in ["127.0.0.2", "::1", "127.255.255.254"] {
        let doc = ConnectionsDocument::new(vec![Connection {
            name: "api".into(),
            endpoint_unix: real.join("a.sock").to_str().unwrap().into(),
            host: host.parse().unwrap(),
            port: 1,
        }])
        .unwrap();
        let error = check_delivered(&doc).expect_err(host);
        assert_eq!(error.code, leyline_runtime::ErrorCode::UnsupportedBackend);
        assert!(
            error
                .detail
                .contains("loopback address not delivered by this runtime"),
            "{}",
            error.detail
        );
        assert!(error.detail.contains("`api`"), "{}", error.detail);
    }
    check_delivered(&document(&real, &[("api", 1)])).expect("127.0.0.1");
}

#[test]
fn summaries_count_a_scripted_sequence() {
    let (_dir, real) = channel_dir();
    let doc = document(&real, &[("api", 30080), ("db", 30432)]);
    let connection = |name: &str, outcome, to, from| Event::Connection {
        name: name.into(),
        outcome,
        bytes_to_target: to,
        bytes_from_target: from,
        open: false,
    };
    let events = vec![
        connection("db", WireOutcome::Dialed, 10, 20),
        connection("db", WireOutcome::Refused, 0, 0),
        connection("db", WireOutcome::ForeignPeer, 0, 0),
        connection("db", WireOutcome::Dialed, 1, 2),
        connection("nobody", WireOutcome::Dialed, 99, 99),
        Event::Drained,
    ];
    let summaries = summarize(&doc, &events, true);
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0].name, "api");
    assert_eq!(summaries[0].accepted, 0);
    assert!(summaries[0].complete);
    let db = &summaries[1];
    assert_eq!(db.target, "127.0.0.1:30432");
    assert_eq!(
        (db.accepted, db.dialed, db.refused, db.foreign_peers),
        (3, 2, 1, 1)
    );
    assert_eq!((db.bytes_to_target, db.bytes_from_target), (11, 22));
    assert!(!summarize(&doc, &events, false)[1].complete);
}
