//! The broker's control pipe and the supervisor's handle (ADR-0046 S5, S7, S8).
//!
//! The supervisor writes the authorized canonical document to the broker's
//! stdin (never argv, never a file) as one JSON line, `{"op":"start",...}`.
//! The broker answers on stdout, one JSON event per line:
//!
//! - `{"event":"ready","connectionsDigest":"blake3-256:<hex>","bound":[...]}`
//!   once every endpoint is bound and the profile is applied; the supervisor
//!   compares the digest with the grant's `connectionsDigest` and refuses a
//!   mismatch, and keeps `bound` (each socket's path, device and inode) so it
//!   unlinks only sockets this broker created;
//! - `{"event":"connection",...}` per connection when it ends;
//! - after `{"op":"drain"}`, a record for every open connection
//!   (`"open":true`) and then `{"event":"drained"}`; no record follows it;
//! - `{"event":"error","detail":...}` before exiting on a failure.
//!
//! `{"op":"peer","pid":N}` names the one process that may connect, once, when
//! the broker was started with `--await-peer` (the microVM tier: the worker
//! exists only after the broker, ADR-0046 S7); until then every peer is
//! refused.
//!
//! The broker also holds the read end of a lifeline pipe whose only write end
//! the supervisor holds (`--lifeline-fd`). End-of-file on it means the
//! supervisor is gone, and the broker exits at once.

use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use leyline_schema_spec::connections::ConnectionsDocument;

use super::relay::{Limits, Outcome, PeerPid, Record, Report};
use crate::ExecutionError;

/// The fd number the lifeline read end has in the broker.
pub const LIFELINE_FD: i32 = 3;

/// Descriptors the broker may need: a dialed connection holds two (the client
/// and upstream sockets, borrowed by both copy directions), and a passed right
/// is open briefly before it is closed; four per connection at the
/// 256-connection cap leaves that headroom, plus listeners and standard streams.
pub const FILE_LIMIT: u64 = 4 * super::MAX_CONNECTIONS as u64 + 64;

/// Supervisor to broker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", deny_unknown_fields)]
pub enum Request {
    Start { document: String },
    Peer { pid: i32 },
    Drain,
}

/// One socket the broker bound, as it found it before confining itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Bound {
    pub path: String,
    pub device: u64,
    pub inode: u64,
}

/// Broker to supervisor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "camelCase", deny_unknown_fields)]
pub enum Event {
    #[serde(rename_all = "camelCase")]
    Ready {
        connections_digest: String,
        bound: Vec<Bound>,
    },
    #[serde(rename_all = "camelCase")]
    Connection {
        name: String,
        outcome: WireOutcome,
        bytes_to_target: u64,
        bytes_from_target: u64,
        open: bool,
    },
    Drained,
    Error {
        detail: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WireOutcome {
    Dialed,
    Refused,
    ForeignPeer,
}

impl From<&Record> for Event {
    fn from(record: &Record) -> Self {
        Event::Connection {
            name: record.name.clone(),
            outcome: match record.outcome {
                Outcome::Dialed => WireOutcome::Dialed,
                Outcome::Refused => WireOutcome::Refused,
                Outcome::ForeignPeer => WireOutcome::ForeignPeer,
            },
            bytes_to_target: record.bytes_to_target,
            bytes_from_target: record.bytes_from_target,
            open: record.open,
        }
    }
}

/// One connection's receipt outcome (RunReceipt `ConnectionOutcome`, S8).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectionSummary {
    pub name: String,
    pub target: String,
    pub accepted: u64,
    pub dialed: u64,
    pub refused: u64,
    pub foreign_peers: u64,
    pub complete: bool,
    pub bytes_to_target: u64,
    pub bytes_from_target: u64,
}

/// Fold the broker's events into one summary per connection, in document
/// order. `complete` is the caller's: true when the broker drained (every
/// accepted connection then has a record), false after a crash, when every
/// count is a lower bound. A record for an unknown name is ignored.
pub fn summarize(
    document: &ConnectionsDocument,
    events: &[Event],
    complete: bool,
) -> Vec<ConnectionSummary> {
    let mut summaries: Vec<ConnectionSummary> = document
        .connections()
        .iter()
        .map(|c| ConnectionSummary {
            name: c.name.clone(),
            target: c.target_text(),
            complete,
            ..ConnectionSummary::default()
        })
        .collect();
    for event in events {
        let Event::Connection {
            name,
            outcome,
            bytes_to_target,
            bytes_from_target,
            ..
        } = event
        else {
            continue;
        };
        let Some(summary) = summaries.iter_mut().find(|s| &s.name == name) else {
            continue;
        };
        match outcome {
            WireOutcome::Dialed => {
                summary.accepted += 1;
                summary.dialed += 1;
            }
            WireOutcome::Refused => {
                summary.accepted += 1;
                summary.refused += 1;
            }
            WireOutcome::ForeignPeer => summary.foreign_peers += 1,
        }
        summary.bytes_to_target += bytes_to_target;
        summary.bytes_from_target += bytes_from_target;
    }
    summaries
}

/// Encode one message as a JSON line.
pub fn line<T: Serialize>(message: &T) -> String {
    let mut text = serde_json::to_string(message).expect("control messages serialize");
    text.push('\n');
    text
}

/// The broker process's arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub lifeline_fd: i32,
    pub peer: PeerPid,
}

/// Parse `--lifeline-fd <fd> [--peer-pid <pid> | --await-peer]`, in any order.
pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut lifeline_fd: Option<i32> = None;
    let mut peer: Option<PeerPid> = None;
    let mut iter = args.into_iter();
    while let Some(flag) = iter.next() {
        let set_peer = |peer: &mut Option<PeerPid>, value: PeerPid| {
            if peer.replace(value).is_some() {
                Err("--peer-pid and --await-peer name the peer once".to_owned())
            } else {
                Ok(())
            }
        };
        match flag.as_str() {
            "--await-peer" => set_peer(&mut peer, PeerPid::Awaiting)?,
            "--lifeline-fd" | "--peer-pid" => {
                let value = iter.next().ok_or(format!("{flag} needs a value"))?;
                let number: i32 = value
                    .parse()
                    .map_err(|_| format!("{flag} {value:?} is not a number"))?;
                if flag == "--peer-pid" {
                    if number <= 0 {
                        return Err(format!("--peer-pid {number} is not a process id"));
                    }
                    set_peer(&mut peer, PeerPid::Exactly(number))?;
                } else if lifeline_fd.replace(number).is_some() {
                    return Err(format!("{flag} given twice"));
                }
            }
            _ => return Err(format!("unknown argument {flag}")),
        }
    }
    Ok(Args {
        lifeline_fd: lifeline_fd.ok_or("--lifeline-fd is required")?,
        peer: peer.unwrap_or(PeerPid::Any),
    })
}

/// Raise this process's soft descriptor limit to at least `wanted`, within the
/// hard limit. Returns the soft limit in force afterwards; the caller refuses
/// to start below what it needs rather than spin on `EMFILE`.
pub fn raise_fd_limit(wanted: u64) -> u64 {
    // SAFETY: a zeroed rlimit filled by getrlimit.
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: valid out-pointer.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return 0;
    }
    #[allow(clippy::unnecessary_cast)]
    let (soft, hard) = (limit.rlim_cur as u64, limit.rlim_max as u64);
    if soft >= wanted {
        return soft;
    }
    let target = wanted.min(hard);
    limit.rlim_cur = target as libc::rlim_t;
    // SAFETY: valid in-pointer.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
        return soft;
    }
    target
}

/// End this process the moment the lifeline's write end is gone: a read
/// returns end-of-file (or fails) once no process holds it.
pub fn watch_lifeline(fd: i32) {
    thread::spawn(move || {
        // SAFETY: the broker owns the lifeline descriptor for its whole life;
        // ManuallyDrop keeps this handle from closing it.
        let mut lifeline = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(fd) });
        let mut byte = [0u8; 1];
        loop {
            if lifeline_ended(&std::io::Read::read(&mut *lifeline, &mut byte)) {
                std::process::exit(0);
            }
        }
    });
}

/// The broker process, after its program name: parse `args`, watch the
/// lifeline, secure the descriptor limit, then [`serve`] stdin/stdout with
/// `confine`. Returns the exit status. The shipped `leyline-conn-broker`
/// passes [`super::profile::apply`].
pub fn run_broker<C>(args: impl IntoIterator<Item = String>, confine: C) -> u8
where
    C: FnOnce(&ConnectionsDocument) -> Result<(), ExecutionError>,
{
    let refuse = |detail: String| {
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(line(&Event::Error { detail }).as_bytes());
        let _ = stdout.flush();
        2
    };
    let args = match parse_args(args) {
        Ok(args) => args,
        Err(detail) => return refuse(detail),
    };
    // The lifeline first: from here on, a vanished supervisor ends us.
    watch_lifeline(args.lifeline_fd);
    // Enough descriptors for the connection cap, or refuse to start rather
    // than refuse connections by EMFILE later.
    let limit = raise_fd_limit(FILE_LIMIT);
    if limit < FILE_LIMIT {
        return refuse(format!(
            "descriptor limit {limit} is below the {FILE_LIMIT} the connection cap needs"
        ));
    }
    match serve(
        std::io::stdin().lock(),
        std::io::stdout(),
        args.peer,
        Limits::default(),
        confine,
    ) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

/// Whether one read of the lifeline means the supervisor is gone: end-of-file
/// or any error but an interrupted call. A byte (which no supervisor writes)
/// changes nothing.
pub fn lifeline_ended(read: &std::io::Result<usize>) -> bool {
    match read {
        Ok(0) => true,
        Ok(_) => false,
        Err(error) => error.kind() != std::io::ErrorKind::Interrupted,
    }
}

/// Clear close-on-exec on `fd`, so it crosses the next exec.
pub fn clear_cloexec(fd: i32) -> std::io::Result<()> {
    // SAFETY: fcntl on a descriptor number; failure is reported.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: F_SETFD on the descriptor F_GETFD just read; it cannot fail on
    // an open descriptor.
    unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) };
    Ok(())
}

/// Everything the broker does after its arguments are read: take the start
/// request, verify the document, bind, `confine`, relay, report, drain.
///
/// `confine` is [`super::profile::apply`] in the binary. It runs after every
/// endpoint is bound and before ready is reported (bind, then confine). A
/// caller that passes anything else runs an unconfined broker; only tests do.
/// Returns when the supervisor drains or closes the control pipe; an error is
/// also reported on `output` as an `error` event.
pub fn serve<R, W, C>(
    input: R,
    output: W,
    peer: PeerPid,
    limits: Limits,
    confine: C,
) -> Result<(), String>
where
    R: BufRead,
    W: Write + Send + 'static,
    C: FnOnce(&ConnectionsDocument) -> Result<(), ExecutionError>,
{
    let output = std::sync::Arc::new(parking_lot::Mutex::new(output));
    let emit = {
        let output = std::sync::Arc::clone(&output);
        move |event: &Event| {
            let mut output = output.lock();
            let _ = output.write_all(line(event).as_bytes());
            let _ = output.flush();
        }
    };
    let fail = |detail: String| {
        emit(&Event::Error {
            detail: detail.clone(),
        });
        Err(detail)
    };
    let mut requests = input.lines();
    let document = match requests.next() {
        Some(Ok(text)) => match serde_json::from_str::<Request>(&text) {
            Ok(Request::Start { document }) => document,
            Ok(other) => return fail(format!("expected start, got {other:?}")),
            Err(error) => return fail(format!("unreadable start request: {error}")),
        },
        _ => return fail("control pipe closed before start".into()),
    };
    let parsed = match leyline_schema_spec::connections::parse(document.as_bytes()) {
        Ok(parsed) => parsed,
        Err(error) => return fail(error.to_string()),
    };
    if parsed.canonical_bytes() != document.as_bytes() {
        return fail("connections document is not canonical".into());
    }
    if let Err(error) = super::check_delivered(&parsed) {
        return fail(error.detail);
    }
    let endpoints = match super::relay::bind(&parsed) {
        Ok(endpoints) => endpoints,
        Err(error) => return fail(format!("bind endpoint: {error}")),
    };
    // What was bound, read before the profile denies file metadata.
    let mut bound = Vec::with_capacity(parsed.connections().len());
    for connection in parsed.connections() {
        match std::fs::symlink_metadata(&connection.endpoint_unix) {
            Ok(metadata) => bound.push(Bound {
                path: connection.endpoint_unix.clone(),
                device: metadata.dev(),
                inode: metadata.ino(),
            }),
            Err(error) => return fail(format!("inspect bound endpoint: {error}")),
        }
    }
    if let Err(error) = confine(&parsed) {
        // Still unconfined, and no ready will tell the supervisor what was
        // bound: remove our own sockets before refusing.
        for socket in &bound {
            let _ = std::fs::remove_file(&socket.path);
        }
        return fail(error.detail);
    }
    // SAFETY: geteuid cannot fail.
    let uid = unsafe { libc::geteuid() };
    let (sender, reports) = mpsc::channel();
    let handle = match super::relay::start(
        endpoints,
        super::relay::PeerPolicy { uid, pid: peer },
        limits,
        sender,
    ) {
        Ok(handle) => handle,
        Err(error) => return fail(format!("start relay: {error}")),
    };
    emit(&Event::Ready {
        connections_digest: format!("blake3-256:{}", parsed.digest_hex()),
        bound,
    });
    // One forwarder, one ordered queue: `Drained` is the last thing written.
    let forwarder = {
        let emit = emit.clone();
        thread::spawn(move || {
            for report in reports {
                match report {
                    Report::Record(record) => emit(&Event::from(&record)),
                    Report::Drained => {
                        emit(&Event::Drained);
                        return;
                    }
                }
            }
        })
    };
    loop {
        match requests.next() {
            Some(Ok(text)) => match serde_json::from_str::<Request>(&text) {
                Ok(Request::Drain) => break,
                Ok(Request::Peer { pid }) => {
                    if let Err(detail) = handle.set_peer_pid(pid) {
                        emit(&Event::Error { detail });
                    }
                }
                _ => {}
            },
            // End-of-file on the control pipe ends the run without a drain.
            _ => return Ok(()),
        }
    }
    handle.drain();
    let _ = forwarder.join();
    Ok(())
}

/// The supervisor's handle on one broker process.
pub struct BrokerProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    lifeline: Option<OwnedFd>,
    events: mpsc::Receiver<Event>,
    seen: Vec<Event>,
    bound: Vec<Bound>,
}

impl BrokerProcess {
    /// Spawn the broker binary. Its stdin and stdout are the control pipe; fd
    /// 3 is the lifeline's read end, and this process keeps the only write end.
    ///
    /// The descriptor hygiene that keeps the write end out of every other
    /// spawn (`POSIX_SPAWN_CLOEXEC_DEFAULT`) is `ley-line-open-528525`: until
    /// then a concurrent spawn between `pipe` and `FD_CLOEXEC` can inherit it.
    pub fn spawn(binary: &Path, peer: PeerPid) -> Result<Self, ExecutionError> {
        let mut fds = [0; 2];
        // SAFETY: a valid two-element out array.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(ExecutionError::backend(format!(
                "lifeline pipe: {}",
                std::io::Error::last_os_error()
            )));
        }
        // SAFETY: pipe(2) just returned these two descriptors, owned here.
        let (read_end, write_end) =
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        set_cloexec(write_end.as_raw_fd());
        set_cloexec(read_end.as_raw_fd());

        let mut command = Command::new(binary);
        command
            .arg("--lifeline-fd")
            .arg(LIFELINE_FD.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        match peer {
            PeerPid::Any => {}
            PeerPid::Awaiting => {
                command.arg("--await-peer");
            }
            PeerPid::Exactly(pid) => {
                command.arg("--peer-pid").arg(pid.to_string());
            }
        }
        let raw_read = read_end.as_raw_fd();
        // SAFETY: async-signal-safe dup2/fcntl in the child before exec. dup2
        // clears close-on-exec on the new descriptor; when the read end already
        // is fd 3, dup2 does nothing, so the flag is cleared explicitly.
        unsafe {
            use std::os::unix::process::CommandExt;
            command.pre_exec(move || {
                if raw_read == LIFELINE_FD {
                    clear_cloexec(LIFELINE_FD)?;
                } else if libc::dup2(raw_read, LIFELINE_FD) != LIFELINE_FD {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|error| {
            ExecutionError::backend(format!("spawn connection broker: {error}"))
        })?;
        drop(read_end);
        let stdout = child.stdout.take().expect("piped stdout");
        let (sender, events) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let event = serde_json::from_str(&line).unwrap_or_else(|error| Event::Error {
                    detail: format!("unreadable broker event {line:?}: {error}"),
                });
                if sender.send(event).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            stdin: child.stdin.take(),
            child,
            lifeline: Some(write_end),
            events,
            seen: Vec::new(),
            bound: Vec::new(),
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Send the document and wait up to `timeout` for a ready report whose
    /// digest equals `expected` (`blake3-256:<hex>`, the grant's @16).
    pub fn start(
        &mut self,
        document: &ConnectionsDocument,
        expected: &str,
        timeout: Duration,
    ) -> Result<(), ExecutionError> {
        let text =
            String::from_utf8(document.canonical_bytes()).expect("canonical bytes are ASCII");
        self.send(&Request::Start { document: text })?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.events.recv_timeout(left) {
                Ok(Event::Ready {
                    connections_digest,
                    bound,
                }) => {
                    // Kept before the digest check: a broker that bound
                    // sockets and then disagreed still leaves them to unlink.
                    self.bound = bound;
                    if connections_digest == expected {
                        return Ok(());
                    }
                    return Err(ExecutionError::identity_mismatch(format!(
                        "connection broker attested {connections_digest}, but the grant's connectionsDigest is {expected}"
                    )));
                }
                Ok(Event::Error { detail }) => {
                    return Err(ExecutionError::backend(format!(
                        "connection broker: {detail}"
                    )));
                }
                Ok(other) => self.seen.push(other),
                Err(_) => {
                    return Err(ExecutionError::backend(
                        "connection broker sent no ready report within the bound",
                    ));
                }
            }
        }
    }

    /// Name the process that may connect, for a broker spawned with
    /// [`PeerPid::Awaiting`].
    pub fn set_peer(&mut self, pid: i32) -> Result<(), ExecutionError> {
        self.send(&Request::Peer { pid })
    }

    /// Every event received so far, without waiting.
    pub fn events(&mut self) -> &[Event] {
        while let Ok(event) = self.events.try_recv() {
            self.seen.push(event);
        }
        &self.seen
    }

    /// Ask the broker to stop accepting and report every open connection; wait
    /// up to `timeout`. Returns whether it drained (the receipt's `complete`).
    pub fn drain(&mut self, timeout: Duration) -> bool {
        if self.send(&Request::Drain).is_err() {
            return false;
        }
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.events.recv_timeout(left) {
                Ok(Event::Drained) => return true,
                Ok(event) => self.seen.push(event),
                Err(_) => return false,
            }
        }
    }

    /// Close the lifeline, kill the broker, reap it, and unlink each socket the
    /// broker reported binding, only while that path is still the node it
    /// bound (same device and inode): a squatter's or another run's socket at
    /// the same path is never removed. Idempotent.
    pub fn shutdown(&mut self) {
        self.lifeline.take();
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        for bound in self.bound.drain(..) {
            if let Ok(metadata) = std::fs::symlink_metadata(&bound.path)
                && metadata.dev() == bound.device
                && metadata.ino() == bound.inode
            {
                let _ = std::fs::remove_file(&bound.path);
            }
        }
    }

    /// Drop the lifeline's write end only, as a dying supervisor would.
    pub fn close_lifeline(&mut self) {
        self.lifeline.take();
    }

    /// Whether the broker process has exited.
    pub fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    fn send(&mut self, request: &Request) -> Result<(), ExecutionError> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| ExecutionError::backend("connection broker control pipe is closed"))?;
        stdin
            .write_all(line(request).as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|error| {
                ExecutionError::backend(format!("connection broker control pipe: {error}"))
            })
    }
}

impl Drop for BrokerProcess {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    #[test]
    fn only_end_of_file_or_a_real_error_ends_the_lifeline() {
        assert!(lifeline_ended(&Ok(0)));
        assert!(!lifeline_ended(&Ok(1)));
        assert!(!lifeline_ended(&Err(Error::from(ErrorKind::Interrupted))));
        assert!(lifeline_ended(&Err(Error::from_raw_os_error(libc::EBADF))));
    }

    #[test]
    fn clear_cloexec_clears_only_that_flag_and_refuses_a_closed_fd() {
        let mut fds = [0; 2];
        // SAFETY: a valid out array.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // Already clear: clearing succeeds (flags 0 is not an error).
        clear_cloexec(fds[0]).unwrap();
        // SAFETY: fcntl on our own descriptor.
        assert_eq!(unsafe { libc::fcntl(fds[0], libc::F_GETFD) }, 0);
        set_cloexec(fds[0]);
        set_cloexec(fds[0]);
        // SAFETY: as above. Setting twice keeps exactly the one flag.
        assert_eq!(
            unsafe { libc::fcntl(fds[0], libc::F_GETFD) },
            libc::FD_CLOEXEC
        );
        clear_cloexec(fds[0]).unwrap();
        // SAFETY: as above.
        assert_eq!(
            unsafe { libc::fcntl(fds[0], libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        // SAFETY: closing our own descriptors.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        assert!(clear_cloexec(fds[0]).is_err(), "a closed descriptor");
    }

    #[test]
    fn the_file_limit_covers_four_descriptors_per_connection() {
        assert_eq!(FILE_LIMIT, 4 * 256 + 64);
        assert_eq!(super::super::BUFFER_BYTES, 65_536);
        assert!(raise_fd_limit(64) >= 64, "already above a small floor");
    }
}

fn set_cloexec(fd: i32) {
    // SAFETY: fcntl on a descriptor this process owns.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
}
