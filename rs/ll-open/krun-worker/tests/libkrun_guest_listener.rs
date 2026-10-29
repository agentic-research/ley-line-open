//! Which guest listeners reach the host: none that nothing declared, and every
//! one that a grant did.
//!
//! ## Why the negative cases (bead `ley-line-open-17536d`)
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
//! The negative tests settle it by booting a guest that binds a port and having
//! the host try to reach it. The assertion is written as the property we want
//! rather than the behaviour we predict: an undeclared listener MUST NOT be
//! reachable. If that fails, the fail-open is real and `krun_set_port_map`
//! needs an explicit empty array. If it passes, the default is already closed
//! and the empty array is belt-and-braces — still worth adding, but not a
//! security fix.
//!
//! ## Why the positive cases (bead `ley-line-open-7d4d45`)
//!
//! A tier on which NO port is ever reachable passes every negative test above.
//! "Declared ports are honoured" and "no port is honoured" agree on every
//! undeclared-port case, so only a case that can pass solely if delivery works
//! tells them apart. And since §4 is a list (`ley-line-open-e2c33f`), one
//! declared port cannot tell "every declared port is published" from "only the
//! first is": the positive test declares two, reaches both, and in the same run
//! fails to reach a third the guest also listens on.
//!
//! §6 gets the same treatment on the channel the microVM tier delivers it by:
//! a `bind` grant becomes a vsock port muxed to a host UNIX socket, and the host
//! reaches the guest's vsock listener through that socket and nothing else.

use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use leyline_runtime::backends::libkrun::confinement::{VSOCK_UNIX_BASE_PORT, confinement_manifest};
use leyline_runtime::confinement::{ConfinementManifest, UnixSocketGrant};
use leyline_runtime::{DigestRef, ExecutionRequest, ResourceLimits};
use tempfile::TempDir;

#[path = "../../runtime/tests/common/mod.rs"]
mod common;
mod hypervisor;

/// The port the negative cases' guest binds. Above 1024 so `confinement/v1` §4
/// would permit declaring it, and not a port anything else here uses.
const GUEST_PORT: u16 = 17536;

/// The positive case: two declared listeners and one the guest binds without a
/// declaration.
const DECLARED_PORTS: [u16; 2] = [18080, 18081];
const UNDECLARED_PORT: u16 = 18082;

/// The prefix of the guest's one console report: `bound`, or `bind-failed: …`.
const REPORT: &str = "listener-probe: ";

/// How long the guest may take to boot and report. Only a bound on a failure:
/// a guest that reports is probed at once, and one that does not fails here.
const REPORT_BOUND: Duration = Duration::from_secs(60);

/// Whether a TCP connection to `127.0.0.1:port` succeeds. A refusal is the
/// answer "no"; anything else (a timeout, an unreachable route) means the probe
/// learned nothing, which fails the test rather than reading as "no".
fn probe(port: u16) -> bool {
    connect(port).is_some()
}

fn connect(port: u16) -> Option<TcpStream> {
    connect_at(Ipv4Addr::LOCALHOST, port)
}

fn connect_at(ip: Ipv4Addr, port: u16) -> Option<TcpStream> {
    let address = SocketAddr::from((ip, port));
    match TcpStream::connect_timeout(&address, Duration::from_secs(5)) {
        Ok(stream) => Some(stream),
        Err(error) if error.kind() == ErrorKind::ConnectionRefused => None,
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
/// passing and failing is one unconditional call in `prepare_vm`, plus LLO
/// libkrun patch 0008, which makes a configured map the complete list. Before
/// that patch an empty map refused nothing; the VMM's own nono profile was
/// what kept this port closed, which held only until a §4 grant widened that
/// profile (`every_declared_guest_listener_is_reachable_and_an_undeclared_one_is_not`).
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

/// The guest listens on 0.0.0.0 at every `required` port (any failure is
/// reported as `bind-failed` and ends the guest) and attempts each `probed`
/// port, reporting `bound` followed by ` <port>=listening` or
/// ` <port>=<error kind>` for each probed one. It then echoes each line a
/// client sends back as `<port>:<line>`.
///
/// A probed port is one the host must not reach, and whether the guest's
/// own `listen(2)` succeeds there depends on the boundary: without socket
/// hijacking the guest kernel listens locally and nothing carries it out;
/// with hijacking and a port map, the VMM refuses the listen (LLO libkrun
/// patch 0008) and the guest sees EACCES. Either is closed; the host probe
/// is what the test asserts.
fn tcp_guest(required: &[u16], probed: &[u16]) -> String {
    format!(
        r#"use std::io::{{BufRead, BufReader, Write}};
fn main() {{
    // Printed before the bind, so a run that stalls says whether the guest
    // started at all.
    println!("listener-probe-start");
    std::io::stdout().flush().unwrap();
    let required: &[u16] = &{required:?};
    let probed: &[u16] = &{probed:?};
    let mut listeners = Vec::new();
    for &port in required {{
        match std::net::TcpListener::bind(("0.0.0.0", port)) {{
            Ok(listener) => listeners.push((port, listener)),
            Err(error) => {{
                println!("{REPORT}bind-failed: {{port}}: {{error}}");
                return;
            }}
        }}
    }}
    let mut outcomes = String::new();
    for &port in probed {{
        match std::net::TcpListener::bind(("0.0.0.0", port)) {{
            Ok(listener) => {{
                outcomes.push_str(&format!(" {{port}}=listening"));
                listeners.push((port, listener));
            }}
            Err(error) => outcomes.push_str(&format!(" {{port}}={{:?}}", error.kind())),
        }}
    }}
    println!("{REPORT}bound{{outcomes}}");
    std::io::stdout().flush().unwrap();
    let mut serving = Vec::new();
    for (port, listener) in listeners {{
        serving.push(std::thread::spawn(move || {{
            for stream in listener.incoming() {{
                let Ok(mut stream) = stream else {{ continue }};
                let mut line = String::new();
                let _ = BufReader::new(&stream).read_line(&mut line);
                let _ = write!(stream, "{{port}}:{{line}}");
            }}
        }}));
    }}
    for thread in serving {{
        let _ = thread.join();
    }}
}}"#
    )
}

fn probe_listener_exposure(hijack_inet: bool) {
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    let fixture = TempDir::new().expect("fixture");

    // Controls before the VM exists. The probe must see a live host listener
    // (otherwise "unreachable" below could be a broken probe), and GUEST_PORT
    // must be free on the host (otherwise a host process, not the guest, would
    // answer the probe).
    probe_controls(&[GUEST_PORT]);

    // The guest reports on its console whether the bind itself succeeded,
    // because "unreachable" has two very different causes: the boundary
    // refused to expose a listener, or the guest had no network stack to bind
    // on. Only the report distinguishes them, and reporting the first when it
    // was the second would be a false all-clear. The report is printed after
    // `listen` returns, so it is also the event the host's probe waits for.
    // The guest then holds any listener it got until the host ends the VM.
    //
    // Without hijacking the guest kernel listens locally, so the bind is
    // required to succeed. With it, the empty port map makes the VMM refuse
    // the listen outright (LLO libkrun patch 0008), so the port is probed:
    // listening-but-unreachable and refused are both closed outcomes.
    let (required, probed): (&[u16], &[u16]) = if hijack_inet {
        (&[], &[GUEST_PORT])
    } else {
        (&[GUEST_PORT], &[])
    };
    let mut guest = Guest::boot(
        fixture.path(),
        &libkrunfw,
        &tcp_guest(required, probed),
        hijack_inet,
        None,
        &format!("listener-probe-hijack-{hijack_inet}"),
    );
    guest.expect_bound();

    // The guest's listener is live and stays live: it serves until the VM is
    // ended. One probe decides; on loopback a port nothing exposes refuses at
    // once.
    let reached = probe(GUEST_PORT);
    eprintln!("host reached guest listener: {reached}");

    let worker_stderr = guest.end_still_running();
    assert!(
        !reached,
        "SECURITY: the host reached a guest listener on port {GUEST_PORT} that \
         no manifest declared (tsi_hijack_inet={hijack_inet}). confinement/v1 \
         §4 says an omitted `port` block means the workload MUST NOT bind a \
         listener, and libkrun's NULL port_map default is to expose every \
         listening guest port. prepare_vm must call krun_set_port_map with an \
         EMPTY array. worker stderr:\n{worker_stderr}"
    );
}

/// The positive proof (bead `ley-line-open-7d4d45`): a grant declaring TWO §4
/// listeners publishes both, and only those.
///
/// The guest listens on both declared ports and on a third the grant does not
/// name. The host exchanges bytes with each declared listener — a connection
/// alone could be a host-side proxy accepting on the guest's behalf; the reply
/// carries the guest's own port number and the line the host sent — and the
/// third must refuse, in the same run, under the same port map.
///
/// This test found that the third did NOT refuse on macOS. libkrun's port map
/// renamed ports and limited none: an unmapped guest listen was bound at the
/// guest's own address, and the VMM's Seatbelt profile — which a §4 grant
/// widens to an unqualified `network-bind` — let it. LLO libkrun patch 0008
/// makes a configured map the complete list (an unmapped listen is refused
/// with EACCES) and binds mapped ports on loopback, which the off-loopback
/// probe below holds it to.
///
/// `--tsi-hijack-inet` is on because the microVM tier refuses a §4 grant
/// without it (`guest_port_map`): without hijacking a guest `AF_INET` bind is
/// carried nowhere, so the declaration could never take effect.
#[test]
fn every_declared_guest_listener_is_reachable_and_an_undeclared_one_is_not() {
    let _serial = common::serial();
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    let fixture = TempDir::new().expect("fixture");
    probe_controls(&[DECLARED_PORTS[0], DECLARED_PORTS[1], UNDECLARED_PORT]);

    let mut manifest = base_manifest(&libkrunfw);
    for port in DECLARED_PORTS {
        manifest = manifest
            .with_port_bind(port, None)
            .expect("declare a §4 listener");
    }
    let mut guest = Guest::boot(
        fixture.path(),
        &libkrunfw,
        &tcp_guest(&DECLARED_PORTS, &[UNDECLARED_PORT]),
        true,
        Some(&manifest),
        "listener-declared-list",
    );
    guest.expect_bound();

    let mut replies = Vec::new();
    for port in DECLARED_PORTS {
        let Some(mut stream) = connect(port) else {
            let stderr = guest.end();
            panic!(
                "the declared guest listener on port {port} refused the host. A §4 \
                 grant names it, so the port map must publish it. worker \
                 stderr:\n{stderr}"
            );
        };
        stream
            .set_read_timeout(Some(REPLY_BOUND))
            .expect("bound the reply");
        replies.push((port, exchange_line(&mut stream, &format!("hello-{port}"))));
    }
    let undeclared_reached = probe(UNDECLARED_PORT);
    // §4 publishes on loopback unless an address is declared, and the guest
    // bound 0.0.0.0: a declared listener must not answer on any other host
    // address. Probed on the host's own outbound address when it has one.
    let off_loopback = host_address().map(|address| {
        let reached = DECLARED_PORTS
            .iter()
            .filter(|&&port| connect_at(address, port).is_some())
            .copied()
            .collect::<Vec<_>>();
        (address, reached)
    });

    let worker_stderr = guest.end_still_running();
    match off_loopback {
        Some((address, reached)) => assert!(
            reached.is_empty(),
            "SECURITY: declared listeners {reached:?} answered on host address \
             {address}. §4 declares them on loopback (no address), so the host \
             side must bind 127.0.0.1, not the 0.0.0.0 the guest asked for. \
             worker stderr:\n{worker_stderr}"
        ),
        None => eprintln!(
            "NOTE: this host has no non-loopback IPv4 address, so the loopback-only \
             half of §4 was not probed in this run"
        ),
    }
    for (port, reply) in replies {
        assert_eq!(
            reply,
            format!("{port}:hello-{port}\n"),
            "the guest listener on declared port {port} did not echo the host's \
             bytes. worker stderr:\n{worker_stderr}"
        );
    }
    assert!(
        !undeclared_reached,
        "SECURITY: the host reached the guest listener on port {UNDECLARED_PORT}, \
         which the grant does not declare, while ports {DECLARED_PORTS:?} were \
         declared. The port map must publish exactly the §4 list. worker \
         stderr:\n{worker_stderr}"
    );
}

/// §6 on the microVM tier: a `bind` grant on a host UNIX socket path is a
/// vsock port the guest serves and the host dials through that socket.
///
/// The grant at index 0 owns the vsock pair `VSOCK_UNIX_BASE_PORT + {0, 1}`; a
/// `bind` grant materializes only the serve half (`+1`, `listen=true`), and the
/// VMM creates the socket at the declared path. So the host reaches the guest's
/// vsock listener by connecting to the path, and exchanges bytes with it.
///
/// Two undeclared channels must stay closed, both probed by the guest because
/// the host has no way to name them: the dial half of the same grant (`+0`),
/// which `bind` withholds, and the next grant's dial port (`+2`), which no
/// grant declares. The guest's own serve port (`+1`) is probed too: serve
/// without dial is the meaning of `bind`, held by the muxer resetting a
/// guest-originated request on a listen port.
#[test]
fn a_declared_unix_socket_reaches_a_guest_vsock_listener_and_nothing_else_does() {
    let _serial = common::serial();
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    let fixture = TempDir::new().expect("fixture");
    let root = fixture.path().canonicalize().expect("canonical fixture");
    let socket = root.join("s.sock");

    let manifest = base_manifest(&libkrunfw)
        .with_unix_socket(UnixSocketGrant::bind(socket.display().to_string()))
        .expect("declare a §6 bind grant");
    let serve = VSOCK_UNIX_BASE_PORT + 1;
    let mut guest = Guest::boot(
        &root,
        &libkrunfw,
        &vsock_guest(
            serve,
            &[
                VSOCK_UNIX_BASE_PORT,
                VSOCK_UNIX_BASE_PORT + 1,
                VSOCK_UNIX_BASE_PORT + 2,
            ],
        ),
        false,
        Some(&manifest),
        "listener-unix-socket",
    );
    let report = guest.report.clone();

    // The guest listens BEFORE it reports, and the VMM created the socket at
    // prepare time, so a connect now reaches a live guest listener.
    let exchanged = UnixStream::connect(&socket).and_then(|mut stream| {
        stream.set_read_timeout(Some(REPLY_BOUND))?;
        Ok(exchange_line(&mut stream, "hello-vsock"))
    });
    let worker_stderr = guest.end_still_running();

    let dials = report.strip_prefix("bound").unwrap_or_else(|| {
        panic!(
            "the guest could not serve on vsock port {serve}, so this run \
             measured nothing. Guest said: {report}\nworker stderr:\n{worker_stderr}"
        )
    });
    match exchanged {
        Ok(reply) => assert_eq!(
            reply, "vsock:hello-vsock\n",
            "the guest's vsock listener did not echo the host's bytes through \
             the declared socket. worker stderr:\n{worker_stderr}"
        ),
        Err(error) => panic!(
            "the host could not exchange bytes with the guest through the \
             declared socket {}: {error}. worker stderr:\n{worker_stderr}",
            socket.display()
        ),
    }
    for dial in dials.split_whitespace() {
        let (port, outcome) = dial.split_once('=').expect("port=outcome");
        assert_ne!(
            outcome, "connected",
            "SECURITY: the guest dialed vsock port {port} to the host, and only \
             the serve port {serve} of a `bind` grant is declared. A guest dial \
             reaches the host only through a dial mapping a grant constructed. \
             guest report: {report}\nworker stderr:\n{worker_stderr}"
        );
    }
}

/// A guest that serves vsock port `serve`, then dials each of `dial` to the
/// host and reports how each went, then echoes one line per connection as
/// `vsock:<line>`.
fn vsock_guest(serve: u32, dial: &[u32]) -> String {
    format!(
        r#"use std::ffi::c_int;

#[repr(C)]
struct SockaddrVm {{
    svm_family: u16,
    svm_reserved1: u16,
    svm_port: u32,
    svm_cid: u32,
    svm_zero: [u8; 4],
}}

extern "C" {{
    fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    fn bind(fd: c_int, address: *const SockaddrVm, length: u32) -> c_int;
    fn listen(fd: c_int, backlog: c_int) -> c_int;
    fn accept(fd: c_int, address: *mut SockaddrVm, length: *mut u32) -> c_int;
    fn connect(fd: c_int, address: *const SockaddrVm, length: u32) -> c_int;
    fn read(fd: c_int, buffer: *mut u8, count: usize) -> isize;
    fn write(fd: c_int, buffer: *const u8, count: usize) -> isize;
    fn close(fd: c_int) -> c_int;
}}

const AF_VSOCK: c_int = 40;
const SOCK_STREAM: c_int = 1;
const VMADDR_CID_ANY: u32 = u32::MAX;
const VMADDR_CID_HOST: u32 = 2;

fn address(cid: u32, port: u32) -> SockaddrVm {{
    SockaddrVm {{ svm_family: AF_VSOCK as u16, svm_reserved1: 0, svm_port: port, svm_cid: cid, svm_zero: [0; 4] }}
}}

fn main() {{
    println!("listener-probe-start");
    let size = std::mem::size_of::<SockaddrVm>() as u32;
    // SAFETY, for every block below: plain syscalls on descriptors this
    // process owns, with live buffers and addresses of the sizes passed.
    let server = unsafe {{ socket(AF_VSOCK, SOCK_STREAM, 0) }};
    let bound = server >= 0
        && unsafe {{ bind(server, &address(VMADDR_CID_ANY, {serve}), size) }} == 0
        && unsafe {{ listen(server, 4) }} == 0;
    if !bound {{
        println!("{REPORT}bind-failed: {{}}", std::io::Error::last_os_error());
        return;
    }}
    let mut dials = String::new();
    for port in {dial:?} {{
        let fd = unsafe {{ socket(AF_VSOCK, SOCK_STREAM, 0) }};
        let outcome = if unsafe {{ connect(fd, &address(VMADDR_CID_HOST, port), size) }} == 0 {{
            "connected".to_owned()
        }} else {{
            format!("{{:?}}", std::io::Error::last_os_error().kind())
        }};
        unsafe {{ close(fd) }};
        dials.push_str(&format!(" {{port}}={{outcome}}"));
    }}
    println!("{REPORT}bound{{dials}}");
    loop {{
        let mut peer = address(0, 0);
        let mut length = size;
        let client = unsafe {{ accept(server, &mut peer, &mut length) }};
        if client < 0 {{
            continue;
        }}
        // One line: the host keeps its write side open until it has the reply.
        let mut request = Vec::new();
        let mut byte = 0u8;
        while unsafe {{ read(client, &mut byte, 1) }} == 1 {{
            request.push(byte);
            if byte == b'\n' {{
                break;
            }}
        }}
        let mut reply = b"vsock:".to_vec();
        reply.extend_from_slice(&request);
        unsafe {{ write(client, reply.as_ptr(), reply.len()) }};
        unsafe {{ close(client) }};
    }}
}}"#
    )
}

/// Send `line` and read the guest's one-line reply, keeping the write side
/// open: a half-close is not something every proxy on the path forwards, and
/// the guest answers at the newline. A reply cut short reads as what arrived,
/// for the caller's assertion to show. The caller sets a read timeout: only a
/// bound on a guest that never answers, which then fails instead of hanging.
fn exchange_line(stream: &mut (impl Read + Write), line: &str) -> String {
    if let Err(error) = writeln!(stream, "{line}") {
        return format!("<send failed: {error}>");
    }
    let mut reply = String::new();
    match BufReader::new(stream).read_line(&mut reply) {
        Ok(_) => reply,
        Err(error) => format!("{reply}<no reply: {error}>"),
    }
}

/// Only a bound on a failure, as [`REPORT_BOUND`] is.
const REPLY_BOUND: Duration = Duration::from_secs(30);

/// This host's own non-loopback IPv4 address: the source address the kernel
/// picks for an outbound route. A UDP `connect` only selects the route; no
/// packet is sent. `None` without such a route.
fn host_address() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    // TEST-NET-1 (RFC 5737): routable in form, never answered.
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

/// LLO's own compiled policy for a worker granted only `libkrunfw`: the
/// document a grant must extend, since the tier refuses a carried document
/// that differs from what it compiles.
fn base_manifest(libkrunfw: &Path) -> ConfinementManifest {
    confinement_manifest(&[libkrunfw.to_path_buf()], &[], None).expect("the compiled policy")
}

/// The probe must see a live host listener (otherwise "unreachable" could be a
/// broken probe), and every port the guest binds must be free on the host
/// (otherwise a host process, not the guest, would answer).
fn probe_controls(guest_ports: &[u16]) {
    let control = TcpListener::bind("127.0.0.1:0").expect("control listener");
    let control_port = control.local_addr().expect("control address").port();
    assert!(
        probe(control_port),
        "the probe did not reach a live host listener on port {control_port}"
    );
    drop(control);
    for &port in guest_ports {
        assert!(
            !probe(port),
            "port {port} already answers on the host before the VM starts; free \
             it, or this run cannot attribute a connection to the guest"
        );
    }
}

/// One booted guest, under the worker a user runs, past its console report.
struct Guest {
    child: Child,
    /// What followed [`REPORT`] on the guest console.
    report: String,
    stderr: Option<JoinHandle<String>>,
}

impl Guest {
    /// Compile `source`, publish it as a pinned CAS rootfs, start the worker on
    /// it, and wait for the guest's console report. `authorized` is the
    /// confinement document a grant carried, as the backend forwards it.
    fn boot(
        root: &Path,
        libkrunfw: &Path,
        source: &str,
        hijack_inet: bool,
        authorized: Option<&ConfinementManifest>,
        run_id: &str,
    ) -> Self {
        let guest_source = root.join("guest-listen.rs");
        fs::write(&guest_source, source).expect("guest source");
        let staging_bin = root.join("staging/bin");
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
        let cas = root.join("cas");
        let immutable_root = cas.join(&root_digest);
        fs::create_dir_all(immutable_root.join("bin")).expect("CAS rootfs");
        fs::copy(&guest_binary, immutable_root.join("bin/guest-listen")).expect("install guest");
        fs::set_permissions(
            immutable_root.join("bin/guest-listen"),
            fs::Permissions::from_mode(0o755),
        )
        .expect("CAS guest mode");
        fs::write(immutable_root.join("rootfs.manifest.json"), manifest).expect("manifest");

        let run_root = root.join("run-root");
        fs::create_dir(&run_root).expect("run root");

        let signed_worker = hypervisor::worker_under_test(
            Path::new(env!("CARGO_BIN_EXE_leyline-krun-worker")),
            &root.join("leyline-krun-worker"),
        );

        let (confinement_digest, confinement_manifest) = match authorized {
            Some(document) => (
                document.confinement_digest().expect("confinement digest"),
                Some(document.to_canonical_json().expect("canonical document")),
            ),
            None => (String::new(), None),
        };
        let request = ExecutionRequest {
            run_id: run_id.to_owned(),
            replay_key: format!("{run_id}-replay"),
            rootfs: DigestRef {
                algorithm: "blake3-256".into(),
                value: root_digest,
            },
            executable: "bin/guest-listen".into(),
            arguments: vec!["guest-listen".into()],
            public_environment: std::collections::BTreeMap::new(),
            allowed_egress: Vec::new(),
            confinement_digest,
            confinement_manifest,
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

        let mut worker = Command::new(&signed_worker);
        // Its standard streams and nothing else, as the backend spawns it.
        leyline_runtime::backends::libkrun::worker::inherit_only(&mut worker, &[])
            .expect("list this process's descriptors");
        if hijack_inet {
            worker.arg("--tsi-hijack-inet");
        }
        let (library_var, library_dir) = hypervisor::library_path_env(libkrunfw);
        let mut child = worker
            .arg("--cas-root")
            .arg(&cas)
            .arg("--run-root")
            .arg(&run_root)
            .arg("--runtime-file")
            .arg(libkrunfw)
            .env(library_var, library_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn worker");
        serde_json::to_writer(child.stdin.as_mut().expect("stdin"), &request)
            .expect("write request");
        child.stdin.take().expect("close stdin").flush().ok();

        // Readiness is emitted after the policy is applied and the VM
        // prepared, but before the guest runs, so it says nothing about a
        // listening socket. The probe waits for the guest's own report.
        let mut stderr = BufReader::new(child.stderr.take().expect("stderr"));
        let mut readiness = String::new();
        stderr.read_line(&mut readiness).expect("read readiness");
        if !readiness.contains("\"ready\"") {
            let _ = child.kill();
            let _ = child.wait();
            let mut rest = String::new();
            let _ = stderr.read_to_string(&mut rest);
            panic!("worker did not report readiness: {readiness}{rest}");
        }
        // Drain the rest of stderr so the worker never blocks on a full pipe,
        // and keep it for the failure messages.
        let stderr = std::thread::spawn(move || {
            let mut rest = String::new();
            let _ = stderr.read_to_string(&mut rest);
            rest
        });

        // The guest console is the worker's stdout. A reader thread forwards
        // the guest's report the moment it is printed; the bound below only
        // turns a guest that never reports into a failure.
        let (report_tx, report_rx) = mpsc::channel();
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        std::thread::spawn(move || {
            let mut console = Vec::new();
            for line in stdout.lines() {
                let Ok(line) = line else { break };
                if let Some(report) = line
                    .find(REPORT)
                    .map(|at| line[at + REPORT.len()..].trim_end().to_owned())
                {
                    let _ = report_tx.send(Ok(report));
                    return;
                }
                console.push(line);
            }
            let _ = report_tx.send(Err(console.join("\n")));
        });
        let mut guest = Self {
            child,
            report: String::new(),
            stderr: Some(stderr),
        };
        guest.report = match report_rx.recv_timeout(REPORT_BOUND) {
            Ok(Ok(report)) => report,
            Ok(Err(console)) => {
                let stderr = guest.end();
                panic!(
                    "the guest console closed without a bind report.\nconsole:\n{console}\n\
                     worker stderr:\n{stderr}"
                );
            }
            Err(_) => {
                // Ending the worker closes the console, so the reader hands
                // back everything the guest printed before it stalled.
                let stderr = guest.end();
                let console = match report_rx.recv() {
                    Ok(Err(console)) => console,
                    Ok(Ok(report)) => format!("(report arrived after the bound: {report})"),
                    Err(_) => "(console reader ended without output)".to_owned(),
                };
                panic!(
                    "FAILED: no bind report from the guest within {REPORT_BOUND:?}.\nconsole:\n{console}\n\
                     worker stderr:\n{stderr}"
                );
            }
        };
        eprintln!("guest bind outcome: {}", guest.report);
        guest
    }

    /// Fail unless the guest bound every required listener, and every probed
    /// port either listened or was refused by the boundary: an unreachable
    /// port means nothing if the guest never got as far as asking for it (no
    /// network stack, a bad address), so only those two outcomes count.
    fn expect_bound(&mut self) {
        let outcomes = self.report.strip_prefix("bound").map(|rest| {
            rest.split_whitespace()
                .map(|probe| probe.split_once('=').map_or("", |(_, outcome)| outcome))
                .all(|outcome| matches!(outcome, "listening" | "PermissionDenied"))
        });
        if outcomes != Some(true) {
            let stderr = self.end();
            panic!(
                "the guest could not bind as this test requires, so this run \
                 measured nothing about host exposure. Fix the guest or the VM \
                 config before trusting a result here. Guest said: {}\nworker \
                 stderr:\n{stderr}",
                self.report
            );
        }
    }

    /// End the VM and return the worker's stderr.
    fn end(&mut self) -> String {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.stderr
            .take()
            .map(|stderr| stderr.join().unwrap_or_default())
            .unwrap_or_default()
    }

    /// End the VM, failing unless it was still running: every probe above
    /// only means something if the guest held its listeners across it, and a
    /// guest that had exited would make any refusal vacuous.
    fn end_still_running(&mut self) -> String {
        let still_running = self.child.try_wait().expect("poll worker").is_none();
        let stderr = self.end();
        assert!(
            still_running,
            "the worker exited before the host's probes finished, so the guest's \
             listeners were not held across them. worker stderr:\n{stderr}"
        );
        stderr
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
