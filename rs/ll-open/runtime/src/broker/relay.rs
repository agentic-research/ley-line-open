//! The relay (ADR-0046 S5): one listener per connection, one dial per accepted
//! connection to the committed target, bytes copied and every passed
//! descriptor closed on arrival ([`DiscardRights`]).
//!
//! The broker reads no address from the stream and parses nothing, so the
//! workload cannot steer it. Each listener's target is a `SocketAddr` built
//! from the authorized literal when the broker starts.

use std::collections::BTreeMap;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
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
    /// The wake pipe: a byte on it ends every acceptor's wait.
    wake_read: OwnedFd,
    wake_write: OwnedFd,
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
        // SAFETY: one byte from a valid buffer to the wake pipe this handle owns.
        unsafe { libc::write(self.wake_write.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
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
        drop(self.wake_read);
    }
}

/// Start one acceptor per endpoint. Every report is sent on `reports`.
pub fn start(
    endpoints: Vec<Endpoint>,
    peers: PeerPolicy,
    limits: Limits,
    reports: mpsc::Sender<Report>,
) -> io::Result<RelayHandle> {
    let mut pipe = [0; 2];
    // SAFETY: a valid two-element out array.
    if unsafe { libc::pipe(pipe.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe(2) just returned these descriptors, owned here.
    let (wake_read, wake_write) =
        unsafe { (OwnedFd::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1])) };
    let live = Arc::new(Mutex::new(BTreeMap::new()));
    let active = Arc::new(AtomicUsize::new(0));
    let ids = Arc::new(AtomicU64::new(0));
    let peer_pid = Arc::new(Mutex::new(peers.pid));
    let mut acceptors = Vec::with_capacity(endpoints.len());
    for endpoint in endpoints {
        // Non-blocking: a connection that vanishes between the wake-up and
        // the accept must not block the acceptor past a drain.
        endpoint.listener.set_nonblocking(true)?;
        let context = Context {
            live: Arc::clone(&live),
            active: Arc::clone(&active),
            ids: Arc::clone(&ids),
            reports: reports.clone(),
            uid: peers.uid,
            peer_pid: Arc::clone(&peer_pid),
            limits,
        };
        let wake = wake_read.as_raw_fd();
        acceptors.push(
            thread::Builder::new()
                .name(format!("broker-accept-{}", endpoint.name))
                .spawn(move || accept_loop(endpoint, context, wake))?,
        );
    }
    Ok(RelayHandle {
        wake_read,
        wake_write,
        live,
        acceptors,
        reports,
        peer_pid,
    })
}

#[derive(Clone)]
struct Context {
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

/// How long an acceptor waits after a failed accept (a connection gone
/// before it was taken, `EMFILE`, `ENFILE`), so a persistent error is a slow
/// retry, never a spin.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// Wait, without a timeout, until the listener has a connection or the wake
/// pipe has a byte. Returns `Some(true)` for a connection, `Some(false)` to
/// stop, `None` for a wake-up with neither (an interrupted poll).
fn wait_for(listener: RawFd, wake: RawFd) -> Option<bool> {
    let mut fds = [
        libc::pollfd {
            fd: listener,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // The longest timeout poll takes (about 24 days); its expiry, like an
    // interrupted call, sets no revents and the caller waits again.
    // SAFETY: two valid pollfds for the duration of the call.
    unsafe { libc::poll(fds.as_mut_ptr(), 2, libc::c_int::MAX) };
    if fds[1].revents != 0 {
        return Some(false);
    }
    if fds[0].revents != 0 {
        return Some(true);
    }
    None
}

fn accept_loop(endpoint: Endpoint, context: Context, wake: RawFd) {
    let Endpoint {
        name,
        listener,
        target,
    } = endpoint;
    loop {
        match wait_for(listener.as_raw_fd(), wake) {
            Some(false) => return,
            None => continue,
            Some(true) => {}
        }
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
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

/// `shutdown(2)` on a raw socket the caller keeps open for the call.
fn shutdown_raw(fd: RawFd, how: libc::c_int) {
    // SAFETY: shutdown on a descriptor number; an error is ignored.
    unsafe { libc::shutdown(fd, how) };
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
    let (client_fd, upstream_fd) = (client.as_raw_fd(), upstream.as_raw_fd());
    // `client` and `upstream` stay owned here until both copies have ended
    // (scoped threads borrow them), so a raw shutdown never reaches a
    // descriptor number another connection has since been given.
    let abort = || {
        shutdown_raw(client_fd, libc::SHUT_RDWR);
        shutdown_raw(upstream_fd, libc::SHUT_RDWR);
    };
    thread::scope(|scope| {
        let inbound = thread::Builder::new().spawn_scoped(scope, || {
            copy(
                DiscardRights(&client),
                &upstream,
                &to_target,
                || shutdown_raw(upstream_fd, libc::SHUT_WR),
                abort,
            )
        });
        match inbound {
            Ok(inbound) => {
                copy(
                    &upstream,
                    &client,
                    &from_target,
                    || shutdown_raw(client_fd, libc::SHUT_WR),
                    abort,
                );
                let _ = inbound.join();
            }
            Err(_) => abort(),
        }
    });
    context.finish(
        id,
        name,
        Outcome::Dialed,
        to_target.load(Ordering::SeqCst),
        from_target.load(Ordering::SeqCst),
    );
}

/// The wait after the given one during the hold: doubled, at most
/// [`BACKOFF_MAX`].
pub fn next_backoff(backoff: Duration) -> Duration {
    (backoff * 2).min(BACKOFF_MAX)
}

/// Dial the target. `ECONNREFUSED` is retried with backoff for at most `hold`,
/// ending early when the client hangs up; any other error ends at once. Every
/// attempt is bounded by what is left of the hold, so a full backlog cannot
/// outlast it. No retry is unbounded.
fn dial(client: &UnixStream, target: SocketAddr, hold: Duration) -> Option<TcpStream> {
    let started = Instant::now();
    let mut backoff = BACKOFF_START;
    loop {
        let left = hold.saturating_sub(started.elapsed());
        let error = match TcpStream::connect_timeout(&target, left.max(Duration::from_millis(1))) {
            Ok(stream) => return Some(stream),
            Err(error) => error,
        };
        if error.kind() != ErrorKind::ConnectionRefused {
            return None;
        }
        let left = hold.saturating_sub(started.elapsed());
        if left.is_zero() || client_hung_up(client, backoff.min(left)) {
            return None;
        }
        backoff = next_backoff(backoff);
    }
}

/// Bytes queued to read on `fd` (`FIONREAD`); 0 when the query fails.
fn queued_bytes(fd: RawFd) -> libc::c_int {
    let mut queued: libc::c_int = 0;
    // SAFETY: FIONREAD writes one int through a valid pointer.
    if unsafe { libc::ioctl(fd, libc::FIONREAD, &mut queued) } == 0 {
        queued
    } else {
        0
    }
}

/// Wait up to `wait` for the client to hang up; true when it has. Readable with
/// nothing queued is end-of-file. `FIONREAD` reads no message, so a descriptor
/// the client passed during the hold is never installed here (a peek would
/// install it on macOS); bytes the client wrote stay unread for the upstream.
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
    if unsafe { libc::poll(&mut pollfd, 1, millis) } > 0 && queued_bytes(fd) == 0 {
        return true;
    }
    // Data is waiting for the upstream: keep holding, without spinning.
    if let Some(rest) = wait.checked_sub(started.elapsed()) {
        thread::sleep(rest);
    }
    false
}

/// Copy until end-of-file, then `at_end` (half-close the destination's write
/// side). On an error in either direction, `on_error` shuts both sockets, so
/// the other direction ends too and the client sees end-of-file.
fn copy<R: Read, W: Write>(
    mut from: R,
    mut to: W,
    counter: &AtomicU64,
    at_end: impl FnOnce(),
    on_error: impl FnOnce(),
) {
    let mut buffer = vec![0u8; BUFFER_BYTES];
    loop {
        match from.read(&mut buffer) {
            Ok(0) => return at_end(),
            Ok(n) => {
                if to.write_all(&buffer[..n]).is_err() {
                    return on_error();
                }
                counter.fetch_add(n as u64, Ordering::SeqCst);
            }
            Err(error) => {
                if error.kind() != ErrorKind::Interrupted {
                    return on_error();
                }
            }
        }
    }
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
pub struct DiscardRights<'a>(pub &'a UnixStream);

/// Control buffer, in 8-byte words for cmsghdr alignment: room for more rights
/// than one message carries on either platform (Linux caps one at 253).
const CONTROL_WORDS: usize = 512;

impl Read for DiscardRights<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut iov = libc::iovec {
            iov_base: buffer.as_mut_ptr().cast(),
            iov_len: buffer.len(),
        };
        let mut control = [0u64; CONTROL_WORDS];
        // SAFETY: a zeroed msghdr filled with valid pointers below.
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = std::mem::size_of_val(&control) as _;
        #[cfg(target_os = "linux")]
        let flags = libc::MSG_CMSG_CLOEXEC;
        #[cfg(not(target_os = "linux"))]
        let flags = 0;
        // SAFETY: valid socket, iovec and control buffer for the call.
        let received = unsafe { libc::recvmsg(self.0.as_raw_fd(), &mut message, flags) };
        if received < 0 {
            return Err(io::Error::last_os_error());
        }
        close_rights(&message);
        if message.msg_flags & libc::MSG_CTRUNC != 0 {
            return Err(io::Error::other(
                "client passed more descriptors than the broker reads; ending the connection",
            ));
        }
        Ok(received as usize)
    }
}

/// Close every descriptor an `SCM_RIGHTS` message in `message` carried.
fn close_rights(message: &libc::msghdr) {
    // SAFETY: walking the control buffer recvmsg filled, within msg_controllen.
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(message);
        while !header.is_null() {
            if let (libc::SOL_SOCKET, libc::SCM_RIGHTS) =
                ((*header).cmsg_level, (*header).cmsg_type)
            {
                let payload = (*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let rights = std::slice::from_raw_parts(
                    libc::CMSG_DATA(header).cast::<[u8; 4]>(),
                    payload / std::mem::size_of::<i32>(),
                );
                for right in rights {
                    libc::close(i32::from_ne_bytes(*right));
                }
            }
            header = libc::CMSG_NXTHDR(message, header);
        }
    }
}

/// The connecting peer's effective uid, and its pid where the platform says.
pub fn peer_credentials(stream: &UnixStream) -> io::Result<(u32, Option<i32>)> {
    super::sys::peer_credentials(stream.as_raw_fd())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_its_ceiling() {
        assert_eq!(
            next_backoff(Duration::from_millis(5)),
            Duration::from_millis(10)
        );
        assert_eq!(
            next_backoff(Duration::from_millis(80)),
            Duration::from_millis(160)
        );
        assert_eq!(
            next_backoff(Duration::from_millis(160)),
            Duration::from_millis(200)
        );
        assert_eq!(next_backoff(BACKOFF_MAX), BACKOFF_MAX);
    }
}
