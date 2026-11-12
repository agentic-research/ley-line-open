//! The broker's control pipe and the supervisor's handle (ADR-0046 S5, S7, S8).
//!
//! The supervisor writes the authorized canonical document to the broker's
//! stdin (never argv, never a file) as one JSON line, `{"op":"start",...}`.
//! The broker answers on stdout, one JSON event per line:
//!
//! - `{"event":"ready","connectionsDigest":"blake3-256:<hex>"}` once every
//!   endpoint is bound and the profile is applied; the supervisor compares the
//!   digest with the grant's `connectionsDigest` and refuses a mismatch;
//! - `{"event":"connection",...}` per connection when it ends;
//! - after `{"op":"drain"}`, a record for every open connection
//!   (`"open":true`) and then `{"event":"drained"}`;
//! - `{"event":"error","detail":...}` before exiting on a failure.
//!
//! The broker also holds the read end of a lifeline pipe whose only write end
//! the supervisor holds (`--lifeline-fd`). End-of-file on it means the
//! supervisor is gone, and the broker exits at once.

use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use leyline_schema_spec::connections::ConnectionsDocument;

use super::relay::{Outcome, Record};
use crate::ExecutionError;

/// The fd number the lifeline read end has in the broker.
pub const LIFELINE_FD: i32 = 3;

/// Supervisor to broker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", deny_unknown_fields)]
pub enum Request {
    Start { document: String },
    Drain,
}

/// Broker to supervisor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "camelCase", deny_unknown_fields)]
pub enum Event {
    #[serde(rename_all = "camelCase")]
    Ready {
        connections_digest: String,
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
    pub peer_pid: Option<i32>,
}

/// Parse `--lifeline-fd <fd> [--peer-pid <pid>]`, in any order.
pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut lifeline_fd = None;
    let mut peer_pid = None;
    let mut iter = args.into_iter();
    while let Some(flag) = iter.next() {
        let slot = match flag.as_str() {
            "--lifeline-fd" => &mut lifeline_fd,
            "--peer-pid" => &mut peer_pid,
            _ => return Err(format!("unknown argument {flag}")),
        };
        let value = iter.next().ok_or(format!("{flag} needs a value"))?;
        let number: i32 = value
            .parse()
            .map_err(|_| format!("{flag} {value:?} is not a number"))?;
        if slot.replace(number).is_some() {
            return Err(format!("{flag} given twice"));
        }
    }
    Ok(Args {
        lifeline_fd: lifeline_fd.ok_or("--lifeline-fd is required")?,
        peer_pid,
    })
}

/// End this process the moment the lifeline's write end is gone: a read
/// returns end-of-file (or fails) once no process holds it.
pub fn watch_lifeline(fd: i32) {
    thread::spawn(move || {
        let mut byte = 0u8;
        loop {
            // SAFETY: a one-byte read into a valid buffer.
            let n = unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) };
            if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            if n <= 0 {
                std::process::exit(0);
            }
        }
    });
}

/// Everything the broker does after its arguments are read: take the start
/// request, verify the document, bind, `confine`, relay, report, drain.
///
/// `confine` is [`super::profile::apply`] in the binary. It runs after every
/// endpoint is bound and before ready is reported (bind, then confine).
/// Returns when the supervisor drains or closes the control pipe; an error is
/// also reported on `output` as an `error` event.
pub fn serve<R, W, C>(
    input: R,
    output: W,
    peer_pid: Option<i32>,
    limits: super::relay::Limits,
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
    if let Err(error) = confine(&parsed) {
        return fail(error.detail);
    }
    // SAFETY: geteuid cannot fail.
    let uid = unsafe { libc::geteuid() };
    let (sender, records) = mpsc::channel();
    let handle = match super::relay::start(
        endpoints,
        super::relay::PeerPolicy { uid, pid: peer_pid },
        limits,
        sender,
    ) {
        Ok(handle) => handle,
        Err(error) => return fail(format!("start relay: {error}")),
    };
    emit(&Event::Ready {
        connections_digest: format!("blake3-256:{}", parsed.digest_hex()),
    });
    {
        let emit = emit.clone();
        thread::spawn(move || {
            for record in records {
                emit(&Event::from(&record));
            }
        });
    }
    loop {
        match requests.next() {
            Some(Ok(text)) => {
                if let Ok(Request::Drain) = serde_json::from_str::<Request>(&text) {
                    break;
                }
            }
            // End-of-file on the control pipe ends the run without a drain.
            _ => return Ok(()),
        }
    }
    for record in handle.drain() {
        emit(&Event::from(&record));
    }
    emit(&Event::Drained);
    Ok(())
}

/// The supervisor's handle on one broker process.
pub struct BrokerProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    lifeline: Option<OwnedFd>,
    events: mpsc::Receiver<Event>,
    seen: Vec<Event>,
    endpoints: Vec<PathBuf>,
}

impl BrokerProcess {
    /// Spawn the broker binary. Its stdin and stdout are the control pipe; fd
    /// 3 is the lifeline's read end, and this process keeps the only write end.
    ///
    /// The descriptor hygiene that keeps the write end out of every other
    /// spawn (`POSIX_SPAWN_CLOEXEC_DEFAULT`) is `ley-line-open-528525`.
    pub fn spawn(
        binary: &Path,
        document: &ConnectionsDocument,
        peer_pid: Option<i32>,
    ) -> Result<Self, ExecutionError> {
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
        if let Some(pid) = peer_pid {
            command.arg("--peer-pid").arg(pid.to_string());
        }
        let raw_read = read_end.as_raw_fd();
        // SAFETY: async-signal-safe dup2 in the child before exec; dup2 clears
        // close-on-exec on the new descriptor, so only fd 3 crosses.
        unsafe {
            use std::os::unix::process::CommandExt;
            command.pre_exec(move || {
                if libc::dup2(raw_read, LIFELINE_FD) < 0 {
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
            endpoints: document
                .connections()
                .iter()
                .map(|c| PathBuf::from(&c.endpoint_unix))
                .collect(),
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
                Ok(Event::Ready { connections_digest }) => {
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

    /// Close the lifeline, kill the broker, reap it, and unlink every endpoint.
    /// Idempotent.
    pub fn shutdown(&mut self) {
        self.lifeline.take();
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        for endpoint in &self.endpoints {
            let _ = std::fs::remove_file(endpoint);
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

fn set_cloexec(fd: i32) {
    // SAFETY: fcntl on a descriptor this process owns.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
}
