//! The relay (ADR-0046 S5): one listener per connection, one dial per accepted
//! connection to the committed target, bytes copied and every passed
//! descriptor closed on arrival ([`DiscardRights`]).
//!
//! The broker reads no address from the stream and parses nothing, so the
//! workload cannot steer it. Each listener's target is a `SocketAddr` built
//! from the authorized literal when the broker starts.

use std::collections::BTreeMap;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use leyline_schema_spec::connections::ConnectionsDocument;

use super::{BACKOFF_MAX, BACKOFF_START, BUFFER_BYTES, HOLD, MAX_CONNECTIONS};

/// One bound listener and the one address it dials.
#[derive(Debug)]
pub struct Endpoint {
    pub name: String,
    pub listener: UnixListener,
    pub target: SocketAddr,
}

/// Bind every endpoint in the document, in document order. Nothing is bound
/// unless everything binds: a failure drops (closes) the earlier listeners;
/// the caller unlinks the paths.
pub fn bind(document: &ConnectionsDocument) -> io::Result<Vec<Endpoint>> {
    document
        .connections()
        .iter()
        .map(|connection| {
            Ok(Endpoint {
                name: connection.name.clone(),
                listener: UnixListener::bind(&connection.endpoint_unix)?,
                target: SocketAddr::new(connection.host, connection.port),
            })
        })
        .collect()
}

/// Which process may connect, besides the uid check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerPid {
    /// Any process of the daemon's uid (native tier: the workload's processes
    /// vary).
    Any,
    /// The microVM tier before the worker exists: every peer is refused until
    /// [`RelayHandle::set_peer_pid`] names it (the broker is spawned at ADR-0046
    /// S7 step 3, the worker at step 5).
    Awaiting,
    /// Exactly this process: the run's worker, which hosts the VMM.
    Exactly(i32),
}

/// Who may connect: the daemon's effective uid always, and per [`PeerPid`].
#[derive(Debug, Clone, Copy)]
pub struct PeerPolicy {
    pub uid: u32,
    pub pid: PeerPid,
}

/// Tunables, fixed by the ADR in production and shortened only by tests.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_connections: usize,
    pub hold: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_connections: MAX_CONNECTIONS,
            hold: HOLD,
        }
    }
}

/// How one accepted connection ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The upstream dial succeeded.
    Dialed,
    /// Closed without an upstream: hold expired, dial error, cap, hang-up.
    Refused,
    /// Closed at accept: the peer was not this run's. Not an accepted connection.
    ForeignPeer,
}

/// One connection's final record, or, at drain, an open connection's record
/// so far (`open: true`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub name: String,
    pub outcome: Outcome,
    pub bytes_to_target: u64,
    pub bytes_from_target: u64,
    pub open: bool,
}

/// What the relay reports, in one ordered queue: every record, then, after a
/// drain, `Drained`. Nothing is sent after `Drained`, and nothing sent before
/// it can arrive after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    Record(Record),
    Drained,
}

/// An accepted connection not yet reported: held for its dial, or copying.
struct Live {
    name: String,
    dialed: Arc<AtomicBool>,
    to_target: Arc<AtomicU64>,
    from_target: Arc<AtomicU64>,
}

/// A running relay. Dropping it does not stop it; call [`RelayHandle::drain`].
pub struct RelayHandle {
    stop: Arc<AtomicBool>,
    live: Arc<Mutex<BTreeMap<u64, Live>>>,
    acceptors: Vec<JoinHandle<()>>,
    reports: mpsc::Sender<Report>,
    peer_pid: Arc<Mutex<PeerPid>>,
}

impl RelayHandle {
    /// Name the one process that may connect, once, on a relay started with
    /// [`PeerPid::Awaiting`]. Refused otherwise, so a running broker's peer
    /// can never be changed or widened.
    pub fn set_peer_pid(&self, pid: i32) -> Result<(), String> {
        let mut current = self.peer_pid.lock();
        match *current {
            PeerPid::Awaiting if pid > 0 => {
                *current = PeerPid::Exactly(pid);
                Ok(())
            }
            PeerPid::Awaiting => Err(format!("peer pid {pid} is not a process id")),
            other => Err(format!("peer pid is not awaited (it is {other:?})")),
        }
    }

    /// Stop accepting, then report every accepted connection that has no final
    /// record yet (copying ones as dialed, held ones as refused, `open: true`,
    /// bytes so far), then `Drained`. Exactly once per connection: a
    /// connection's final record is sent under the same lock drain takes the
    /// entries under, and every acceptor has stopped before drain starts.
    pub fn drain(self) {
        self.stop.store(true, Ordering::SeqCst);
        for acceptor in self.acceptors {
            let _ = acceptor.join();
        }
        let mut live = self.live.lock();
        for entry in std::mem::take(&mut *live).into_values() {
            let _ = self.reports.send(Report::Record(Record {
                name: entry.name,
                outcome: if entry.dialed.load(Ordering::SeqCst) {
                    Outcome::Dialed
                } else {
                    Outcome::Refused
                },
                bytes_to_target: entry.to_target.load(Ordering::SeqCst),
                bytes_from_target: entry.from_target.load(Ordering::SeqCst),
                open: true,
            }));
        }
        let _ = self.reports.send(Report::Drained);
    }
}

/// Start one acceptor per endpoint. Every report is sent on `reports`.
pub fn start(
    endpoints: Vec<Endpoint>,
    peers: PeerPolicy,
    limits: Limits,
    reports: mpsc::Sender<Report>,
) -> io::Result<RelayHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let live = Arc::new(Mutex::new(BTreeMap::new()));
    let active = Arc::new(AtomicUsize::new(0));
    let ids = Arc::new(AtomicU64::new(0));
    let peer_pid = Arc::new(Mutex::new(peers.pid));
    let mut acceptors = Vec::with_capacity(endpoints.len());
    for endpoint in endpoints {
        endpoint.listener.set_nonblocking(true)?;
        let context = Context {
            stop: Arc::clone(&stop),
            live: Arc::clone(&live),
            active: Arc::clone(&active),
            ids: Arc::clone(&ids),
            reports: reports.clone(),
            uid: peers.uid,
            peer_pid: Arc::clone(&peer_pid),
            limits,
        };
        acceptors.push(
            thread::Builder::new()
                .name(format!("broker-accept-{}", endpoint.name))
                .spawn(move || accept_loop(endpoint, context))?,
        );
    }
    Ok(RelayHandle {
        stop,
        live,
        acceptors,
        reports,
        peer_pid,
    })
}

#[derive(Clone)]
struct Context {
    stop: Arc<AtomicBool>,
    live: Arc<Mutex<BTreeMap<u64, Live>>>,
    active: Arc<AtomicUsize>,
    ids: Arc<AtomicU64>,
    reports: mpsc::Sender<Report>,
    uid: u32,
    peer_pid: Arc<Mutex<PeerPid>>,
    limits: Limits,
}

impl Context {
    /// A record for a connection that never entered `live`.
    fn record(&self, name: &str, outcome: Outcome) {
        let _ = self.reports.send(Report::Record(Record {
            name: name.to_owned(),
            outcome,
            bytes_to_target: 0,
            bytes_from_target: 0,
            open: false,
        }));
    }

    /// A tracked connection's final record, sent under the `live` lock and
    /// only if drain has not already reported it.
    fn finish(&self, id: u64, name: &str, outcome: Outcome, to: u64, from: u64) {
        let mut live = self.live.lock();
        if live.remove(&id).is_some() {
            let _ = self.reports.send(Report::Record(Record {
                name: name.to_owned(),
                outcome,
                bytes_to_target: to,
                bytes_from_target: from,
                open: false,
            }));
        }
    }

    fn peer_permitted(&self, stream: &UnixStream) -> bool {
        let expected = *self.peer_pid.lock();
        match peer_credentials(stream) {
            Ok((uid, pid)) => {
                uid == self.uid
                    && match expected {
                        PeerPid::Any => true,
                        PeerPid::Awaiting => false,
                        PeerPid::Exactly(expected) => pid == Some(expected),
                    }
            }
            Err(_) => false,
        }
    }
}

/// How long an acceptor waits after an accept error other than "nothing
/// pending" (`EMFILE`, `ENFILE`, `ECONNABORTED`), so a persistent error is a
/// slow retry, never a spin.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

fn accept_loop(endpoint: Endpoint, context: Context) {
    let Endpoint {
        name,
        listener,
        target,
    } = endpoint;
    while !context.stop.load(Ordering::SeqCst) {
        if !wait_readable(listener.as_raw_fd(), Duration::from_millis(50)) {
            continue;
        }
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == ErrorKind::WouldBlock => continue,
            Err(_) => {
                thread::sleep(ACCEPT_ERROR_BACKOFF);
                continue;
            }
        };
        if !context.peer_permitted(&stream) {
            drop(stream);
            context.record(&name, Outcome::ForeignPeer);
            continue;
        }
        // The cap counts connections in flight, held or copying.
        if context.active.fetch_add(1, Ordering::SeqCst) >= context.limits.max_connections {
            context.active.fetch_sub(1, Ordering::SeqCst);
            drop(stream);
            context.record(&name, Outcome::Refused);
            continue;
        }
        let _ = stream.set_nonblocking(false);
        // Tracked before its thread exists, so a drain never misses it.
        let id = context.ids.fetch_add(1, Ordering::SeqCst);
        let tracked = Tracked {
            dialed: Arc::new(AtomicBool::new(false)),
            to_target: Arc::new(AtomicU64::new(0)),
            from_target: Arc::new(AtomicU64::new(0)),
        };
        context.live.lock().insert(
            id,
            Live {
                name: name.clone(),
                dialed: Arc::clone(&tracked.dialed),
                to_target: Arc::clone(&tracked.to_target),
                from_target: Arc::clone(&tracked.from_target),
            },
        );
        let spawned = {
            let context = context.clone();
            let name = name.clone();
            thread::Builder::new().spawn(move || {
                serve(id, &name, stream, target, tracked, &context);
                context.active.fetch_sub(1, Ordering::SeqCst);
            })
        };
        if spawned.is_err() {
            // The stream went down with the closure; nothing was dialed.
            context.active.fetch_sub(1, Ordering::SeqCst);
            context.finish(id, &name, Outcome::Refused, 0, 0);
        }
    }
}

/// A tracked connection's shared state.
struct Tracked {
    dialed: Arc<AtomicBool>,
    to_target: Arc<AtomicU64>,
    from_target: Arc<AtomicU64>,
}

fn serve(
    id: u64,
    name: &str,
    client: UnixStream,
    target: SocketAddr,
    tracked: Tracked,
    context: &Context,
) {
    let Some(upstream) = dial(&client, target, context.limits.hold) else {
        context.finish(id, name, Outcome::Refused, 0, 0);
        return;
    };
    tracked.dialed.store(true, Ordering::SeqCst);
    let Tracked {
        to_target,
        from_target,
        ..
    } = tracked;
    match (client.try_clone(), upstream.try_clone()) {
        (Ok(client_writer), Ok(upstream_writer)) => {
            let inbound = {
                let to_target = Arc::clone(&to_target);
                let client = DiscardRights(client);
                thread::Builder::new().spawn(move || copy(client, upstream_writer, &to_target))
            };
            match inbound {
                Ok(inbound) => {
                    copy(upstream, client_writer, &from_target);
                    let _ = inbound.join();
                }
                Err(_) => {
                    client_writer.shutdown_both();
                    upstream.shutdown_both();
                }
            }
        }
        _ => {
            client.shutdown_both();
            upstream.shutdown_both();
        }
    }
    context.finish(
        id,
        name,
        Outcome::Dialed,
        to_target.load(Ordering::SeqCst),
        from_target.load(Ordering::SeqCst),
    );
}

/// Dial the target once per attempt. `ECONNREFUSED` is held with backoff for at
/// most `hold`, ending early when the client hangs up; any other error ends at
/// once. No retry is unbounded.
fn dial(client: &UnixStream, target: SocketAddr, hold: Duration) -> Option<TcpStream> {
    let started = Instant::now();
    let mut backoff = BACKOFF_START;
    loop {
        // Every attempt is bounded by what is left of the hold (and at least
        // a millisecond), so a full upstream backlog cannot outlast it.
        let left = hold
            .saturating_sub(started.elapsed())
            .max(Duration::from_millis(1));
        match TcpStream::connect_timeout(&target, left) {
            Ok(stream) => return Some(stream),
            Err(error) if error.kind() == ErrorKind::ConnectionRefused => {
                let elapsed = started.elapsed();
                if elapsed >= hold {
                    return None;
                }
                let wait = backoff.min(hold - elapsed);
                if client_hung_up(client, wait) {
                    return None;
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
            Err(_) => return None,
        }
    }
}

/// Wait up to `wait` for the client to hang up; true when it has. Bytes the
/// client already wrote stay unread for the upstream.
fn client_hung_up(client: &UnixStream, wait: Duration) -> bool {
    let started = Instant::now();
    let fd = client.as_raw_fd();
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = i32::try_from(wait.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: one valid pollfd for the duration of the call.
    let ready = unsafe { libc::poll(&mut pollfd, 1, millis) };
    if ready <= 0 {
        return false;
    }
    if pollfd.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
        return true;
    }
    // Readable with nothing queued is end-of-file. FIONREAD reads no message,
    // so a descriptor the client passed during the hold is never installed
    // here (a peek would install it on macOS); the copy loop closes it.
    let mut queued: libc::c_int = 0;
    // SAFETY: FIONREAD writes one int through a valid pointer.
    if unsafe { libc::ioctl(fd, libc::FIONREAD, &mut queued) } != 0 || queued == 0 {
        return true;
    }
    // Data is waiting for the upstream: keep holding, without spinning.
    if let Some(rest) = wait.checked_sub(started.elapsed()) {
        thread::sleep(rest);
    }
    false
}

/// Copy until end-of-file, then half-close the destination's write side. On
/// an error in either direction both sockets are shut, so the other direction
/// ends too and the client sees end-of-file.
fn copy<R, W>(mut from: R, mut to: W, counter: &AtomicU64)
where
    R: Read + ShutdownBoth,
    W: Write + ShutdownBoth,
{
    let mut buffer = vec![0u8; BUFFER_BYTES];
    loop {
        match from.read(&mut buffer) {
            Ok(0) => {
                to.shutdown_write();
                return;
            }
            Ok(n) => {
                if to.write_all(&buffer[..n]).is_err() {
                    break;
                }
                counter.fetch_add(n as u64, Ordering::SeqCst);
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    from.shutdown_both();
    to.shutdown_both();
}

/// The client side of a connection, read so that a passed descriptor never
/// stays with the broker.
///
/// `read(2)` is not enough: macOS installs an `SCM_RIGHTS` descriptor in the
/// receiving process even when it reads with `read(2)` or `recv(2)` and no
/// control buffer (measured; Linux discards it). So the client direction is
/// read with `recvmsg(2)` and a control buffer, and every received right is
/// closed at once; a truncated control message (rights the buffer could not
/// hold, which macOS may leave installed) ends the connection.
pub struct DiscardRights(pub UnixStream);

/// Room for more rights than one message carries on either platform (Linux
/// caps one message at 253).
const CONTROL_BYTES: usize = 4096;

impl Read for DiscardRights {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut iov = libc::iovec {
            iov_base: buffer.as_mut_ptr().cast(),
            iov_len: buffer.len(),
        };
        // u64 words keep the control buffer aligned for cmsghdr.
        let mut control = [0u64; CONTROL_BYTES / 8];
        // SAFETY: a zeroed msghdr filled with valid pointers below.
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = CONTROL_BYTES as _;
        #[cfg(target_os = "linux")]
        let flags = libc::MSG_CMSG_CLOEXEC;
        #[cfg(not(target_os = "linux"))]
        let flags = 0;
        // SAFETY: valid socket, iovec and control buffer for the call.
        let received = unsafe { libc::recvmsg(self.0.as_raw_fd(), &mut message, flags) };
        if received < 0 {
            return Err(io::Error::last_os_error());
        }
        let closed = close_rights(&message);
        if message.msg_flags & libc::MSG_CTRUNC != 0 {
            return Err(io::Error::other(format!(
                "client passed more descriptors than the broker reads ({closed} closed); ending the connection"
            )));
        }
        Ok(received as usize)
    }
}

impl ShutdownBoth for DiscardRights {
    fn shutdown_write(&self) {
        self.0.shutdown_write();
    }
    fn shutdown_both(&self) {
        self.0.shutdown_both();
    }
}

/// Close every descriptor an `SCM_RIGHTS` message in `message` carried;
/// returns how many.
fn close_rights(message: &libc::msghdr) -> usize {
    let mut closed = 0;
    // SAFETY: walking the control buffer recvmsg filled, within msg_controllen.
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let payload = (*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let data = libc::CMSG_DATA(header).cast::<i32>();
                for index in 0..payload / std::mem::size_of::<i32>() {
                    libc::close(std::ptr::read_unaligned(data.add(index)));
                    closed += 1;
                }
            }
            header = libc::CMSG_NXTHDR(message, header);
        }
    }
    closed
}

/// The two socket kinds the relay joins.
pub trait ShutdownBoth {
    fn shutdown_write(&self);
    fn shutdown_both(&self);
}

impl ShutdownBoth for UnixStream {
    fn shutdown_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
    fn shutdown_both(&self) {
        let _ = self.shutdown(Shutdown::Both);
    }
}

impl ShutdownBoth for TcpStream {
    fn shutdown_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
    fn shutdown_both(&self) {
        let _ = self.shutdown(Shutdown::Both);
    }
}

fn wait_readable(fd: i32, wait: Duration) -> bool {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = i32::try_from(wait.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: one valid pollfd for the duration of the call.
    unsafe { libc::poll(&mut pollfd, 1, millis) > 0 }
}

/// The connecting peer's effective uid, and its pid where the platform says.
pub fn peer_credentials(stream: &UnixStream) -> io::Result<(u32, Option<i32>)> {
    let fd = stream.as_raw_fd();
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: valid out-pointers on a connected AF_UNIX socket.
        if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: valid out-pointer and length for LOCAL_PEERPID.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut len,
            )
        };
        Ok((uid, (rc == 0).then_some(pid)))
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    {
        let mut credentials = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: valid out-pointer and length for SO_PEERCRED.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((credentials.uid, Some(credentials.pid)))
    }
}
