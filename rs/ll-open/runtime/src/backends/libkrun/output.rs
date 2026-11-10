//! What a libkrun run printed, kept bounded (bead `ley-line-open-a26ad0`).
//!
//! A libkrun worker writes two streams. Its standard output carries the
//! guest's standard output (libkrun's `krun-stdout` console port). Its
//! standard error carries the worker's JSON events (readiness, which the
//! backend reads in `start`, and a terminal `failed` event when the worker
//! fails), libkrun's diagnostics, and, once the guest runs, the guest's
//! standard error (the `krun-stderr` port). Before this module both were
//! discarded once the run was ready, so a failed guest left only "libkrun
//! worker exited with exit status: 1", and a serving guest left nothing at
//! all.
//!
//! [`OutputCapture`] keeps the last [`GUEST_OUTPUT_TAIL_BYTES`] of the
//! standard output and the last [`WORKER_STDERR_TAIL_BYTES`] of the
//! post-readiness standard error, plus the last line there shaped like a
//! `failed` event. Older bytes are dropped and counted, so a run that prints
//! without bound costs a fixed amount of memory.
//!
//! Because the guest writes the same standard error, a `failed` line read
//! after readiness may be the guest's forgery. It is reported as unverified
//! text and never becomes the failure's code or `retryable`, which stay the
//! supervisor's.
//!
//! Where the bytes go is deny by default. They reach exactly two places, both
//! the in-process embedder that started the run:
//!
//! - the run's [`crate::BackendRunStatus::Failed`] detail, when the run fails.
//!   `ExecutionService` records that detail only as the BLAKE3 digest in the
//!   terminal event's `detailDigest`, so the event log and receipt commit to
//!   it without carrying it;
//! - [`crate::Backend::output`] and `KrunWorkerBackend::wait_for_output`,
//!   while the run is active.
//!
//! Nothing is written to disk, logged, or sent over the daemon's wire. LLO
//! adds nothing to the bytes. They are the workload's own, untrusted, and
//! may contain anything the workload printed. Control characters other than
//! newline and tab are replaced, so a terminal that shows a detail cannot be
//! driven by escape sequences the guest wrote.

use std::collections::VecDeque;
use std::io::Read;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use crate::{ExecutionError, GuestOutput};

use super::worker::WorkerEvent;

/// How much of the guest's console a run keeps: its last 16 KiB. Enough for
/// a panic, a stack trace or a server's start-up banner; small enough that
/// a run that prints forever costs a fixed amount.
pub const GUEST_OUTPUT_TAIL_BYTES: usize = 16 * 1024;

/// How much of the worker's post-readiness standard error a run keeps: its
/// last 8 KiB. The worker writes at most one event there, and libkrun's own
/// diagnostics.
pub const WORKER_STDERR_TAIL_BYTES: usize = 8 * 1024;

/// The longest worker standard-error line parsed as an event. A longer line
/// is kept in the tail but not parsed.
const EVENT_LINE_MAX_BYTES: usize = 64 * 1024;

/// How long a finished run's capture waits for its streams to end once the
/// worker has exited. The worker's descendants are killed with its process
/// group before the exit is reported, so both pipes are already at
/// end-of-file; the bound only keeps a stray holder of a pipe end from
/// delaying the run's completion.
const DRAIN_BOUND: Duration = Duration::from_secs(2);

/// The last `cap` bytes written to one stream.
#[derive(Debug)]
struct Tail {
    bytes: VecDeque<u8>,
    cap: usize,
    dropped: u64,
}

impl Tail {
    fn new(cap: usize) -> Self {
        Self {
            bytes: VecDeque::with_capacity(cap.min(4096)),
            cap,
            dropped: 0,
        }
    }

    fn push(&mut self, data: &[u8]) {
        // Of a write longer than the tail, only its last `cap` bytes count.
        let skipped = data.len().saturating_sub(self.cap);
        self.dropped += skipped as u64;
        let data = &data[skipped..];
        let overflow = (self.bytes.len() + data.len()).saturating_sub(self.cap);
        self.bytes.drain(..overflow);
        self.dropped += overflow as u64;
        self.bytes.extend(data);
    }

    fn text(&self) -> String {
        let (front, back) = self.bytes.as_slices();
        let mut raw = Vec::with_capacity(self.bytes.len());
        raw.extend_from_slice(front);
        raw.extend_from_slice(back);
        sanitize(&raw)
    }
}

/// Bytes a workload wrote, as text a terminal can show without being
/// driven by them: invalid UTF-8 and every control character except newline
/// and tab become U+FFFD.
pub fn sanitize(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw)
        .chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}

#[derive(Debug)]
struct State {
    guest: Tail,
    worker: Tail,
    failure: Option<ExecutionError>,
    /// The worker's standard-error line being assembled for event parsing.
    line: Vec<u8>,
    /// The line being assembled outgrew `EVENT_LINE_MAX_BYTES`; it is
    /// skipped up to its newline.
    line_overflow: bool,
    guest_open: bool,
    worker_open: bool,
    /// How many times a wait has parked on `changed`: what a test
    /// synchronizes on to act only once a wait is really waiting.
    #[cfg(test)]
    parks: usize,
}

#[cfg(test)]
fn note_parked(state: &mut State) {
    state.parks += 1;
}

#[cfg(not(test))]
fn note_parked(_state: &mut State) {}

/// One run's bounded output, filled by two drain threads and read by the
/// backend and its embedder.
#[derive(Debug)]
pub struct OutputCapture {
    state: Mutex<State>,
    changed: Condvar,
}

impl Default for OutputCapture {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputCapture {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                guest: Tail::new(GUEST_OUTPUT_TAIL_BYTES),
                worker: Tail::new(WORKER_STDERR_TAIL_BYTES),
                failure: None,
                line: Vec::new(),
                line_overflow: false,
                guest_open: true,
                worker_open: true,
                #[cfg(test)]
                parks: 0,
            }),
            changed: Condvar::new(),
        }
    }

    /// Read the guest's console until it ends.
    pub fn drain_guest(&self, stream: impl Read) {
        read_until_end(stream, |data| {
            self.state.lock().guest.push(data);
            self.changed.notify_all();
        });
        self.state.lock().guest_open = false;
        self.changed.notify_all();
    }

    /// Read the worker's standard error after readiness until it ends,
    /// keeping its tail and its terminal `failed` event.
    pub fn drain_worker(&self, stream: impl Read) {
        read_until_end(stream, |data| {
            self.push_worker(data);
            self.changed.notify_all();
        });
        let mut state = self.state.lock();
        // A last event without its newline.
        let line = std::mem::take(&mut state.line);
        if !state.line_overflow {
            parse_event(&line, &mut state.failure);
        }
        state.worker_open = false;
        drop(state);
        self.changed.notify_all();
    }

    fn push_worker(&self, data: &[u8]) {
        let mut state = self.state.lock();
        state.worker.push(data);
        for &byte in data {
            if byte == b'\n' {
                let line = std::mem::take(&mut state.line);
                if !state.line_overflow {
                    parse_event(&line, &mut state.failure);
                }
                state.line_overflow = false;
            } else if !state.line_overflow {
                if state.line.len() == EVENT_LINE_MAX_BYTES {
                    state.line.clear();
                    state.line_overflow = true;
                } else {
                    state.line.push(byte);
                }
            }
        }
    }

    /// What the run has printed so far.
    pub fn snapshot(&self) -> GuestOutput {
        snapshot(&self.state.lock())
    }

    /// Wait, at most until `deadline`, for `until` to hold of the output or
    /// for both streams to end. Returns the output it saw last. `until` runs
    /// under the capture's lock: it must not read this run's output again
    /// (`Backend::output`), or it deadlocks and stalls the drains.
    pub fn wait(&self, until: &dyn Fn(&GuestOutput) -> bool, deadline: Instant) -> GuestOutput {
        let mut state = self.state.lock();
        loop {
            let output = snapshot(&state);
            if until(&output) || (!state.guest_open && !state.worker_open) {
                return output;
            }
            note_parked(&mut state);
            if self.changed.wait_until(&mut state, deadline).timed_out() {
                return snapshot(&state);
            }
        }
    }

    /// A finished run's final output: what it printed once both streams
    /// have ended, waiting at most [`DRAIN_BOUND`] for them.
    pub fn settle(&self) -> GuestOutput {
        let deadline = Instant::now() + DRAIN_BOUND;
        self.wait(&|_| false, deadline)
    }
}

/// Hand every chunk `stream` yields to `sink` until it ends: end-of-file, or
/// any read error but an interrupted read, which is retried.
fn read_until_end(mut stream: impl Read, mut sink: impl FnMut(&[u8])) {
    let mut buffer = [0u8; 8192];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return,
            Ok(read) => sink(&buffer[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

fn snapshot(state: &State) -> GuestOutput {
    GuestOutput {
        stdout_tail: state.guest.text(),
        stdout_dropped_bytes: state.guest.dropped,
        stderr_tail: state.worker.text(),
        stderr_dropped_bytes: state.worker.dropped,
        untrusted: true,
        reported_failure: state.failure.clone(),
    }
}

fn parse_event(line: &[u8], failure: &mut Option<ExecutionError>) {
    let line = line.trim_ascii();
    if !line.starts_with(b"{") {
        return;
    }
    if let Ok(WorkerEvent::Failed { mut error }) = serde_json::from_slice::<WorkerEvent>(line) {
        // JSON decoding turns an escaped `\u001b` back into a raw control
        // byte, so the decoded detail is sanitized like every guest byte.
        error.detail = sanitize(error.detail.as_bytes());
        *failure = Some(error);
    }
}

/// `observed`, its code and `retryable` unchanged, with any reported
/// `failed` event appended as unverified text and then the run's printed
/// output. The section headings are framing, not authority: a guest's own
/// bytes keep their newlines and can imitate them.
pub fn failure_with_output(observed: ExecutionError, output: &GuestOutput) -> ExecutionError {
    // The classification is always the supervisor's. A `failed` event read
    // after readiness arrived on a stream the guest also writes, so it is
    // reported as text, never adopted as the code or `retryable`.
    let mut error = observed;
    if let Some(reported) = &output.reported_failure {
        error.detail.push_str(&format!(
            "\nreported on the worker's standard error, which the guest's standard error \
             shares (unverified): {:?}, retryable {}: {}",
            reported.code, reported.retryable, reported.detail
        ));
    }
    for (name, text, dropped) in [
        (
            "guest console",
            &output.stdout_tail,
            output.stdout_dropped_bytes,
        ),
        (
            "guest stderr and worker diagnostics",
            &output.stderr_tail,
            output.stderr_dropped_bytes,
        ),
    ] {
        if text.is_empty() {
            continue;
        }
        let kept = if dropped > 0 {
            format!("tail; {dropped} earlier bytes dropped")
        } else {
            "all of it".to_owned()
        };
        error
            .detail
            .push_str(&format!("\n--- {name} ({kept}) ---\n{text}"));
    }
    error
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::ErrorCode;

    fn tail_of(cap: usize, writes: &[&[u8]]) -> (String, u64) {
        let mut tail = Tail::new(cap);
        for write in writes {
            tail.push(write);
        }
        (tail.text(), tail.dropped)
    }

    #[test]
    fn a_tail_keeps_the_last_bytes_and_counts_the_rest() {
        assert_eq!(tail_of(4, &[b"ab"]), ("ab".into(), 0));
        assert_eq!(tail_of(4, &[b"abcd"]), ("abcd".into(), 0));
        assert_eq!(tail_of(4, &[b"ab", b"cde"]), ("bcde".into(), 1));
        assert_eq!(tail_of(4, &[b"abcdefg"]), ("defg".into(), 3));
        assert_eq!(tail_of(4, &[b"xy", b"abcdefg"]), ("defg".into(), 5));
        assert_eq!(tail_of(4, &[b"", b"a"]), ("a".into(), 0));
    }

    #[test]
    fn control_characters_and_invalid_utf8_are_replaced() {
        assert_eq!(sanitize(b"a\tb\nc"), "a\tb\nc");
        assert_eq!(
            sanitize(b"\x1b[31mred\r\x00"),
            "\u{FFFD}[31mred\u{FFFD}\u{FFFD}"
        );
        assert_eq!(sanitize(b"\xff!"), "\u{FFFD}!");
        assert_eq!(sanitize("\u{9b}".as_bytes()), "\u{FFFD}");
    }

    const FAILED: &[u8] = br#"{"type":"failed","error":{"code":"backend-failed","retryable":false,"detail":"cause"}}"#;

    #[test]
    fn the_terminal_failed_event_is_kept_whatever_the_chunking() {
        let capture = OutputCapture::new();
        capture.push_worker(b"libkrun noise\n  ");
        capture.push_worker(&FAILED[..10]);
        capture.push_worker(&FAILED[10..]);
        capture.push_worker(b"\n");
        let output = capture.snapshot();
        assert_eq!(
            output.reported_failure.map(|error| error.detail),
            Some("cause".into())
        );
        assert!(output.stderr_tail.starts_with("libkrun noise\n"));

        // An event without its final newline is read at end of stream.
        let capture = OutputCapture::new();
        capture.drain_worker(FAILED);
        assert!(capture.snapshot().reported_failure.is_some());
        // Only `failed` events count; readiness or junk is ignored.
        let capture = OutputCapture::new();
        capture.drain_worker(&b"{\"type\":\"ready\",\"run_id\":\"r\"}\nnot json\n{broken\n"[..]);
        assert_eq!(capture.snapshot().reported_failure, None);
    }

    #[test]
    fn an_overlong_line_is_never_parsed_and_parsing_resumes_after_it() {
        let capture = OutputCapture::new();
        let mut long = FAILED.to_vec();
        long.extend(std::iter::repeat_n(b' ', EVENT_LINE_MAX_BYTES));
        capture.push_worker(&long);
        capture.push_worker(b"\n");
        assert_eq!(capture.snapshot().reported_failure, None);
        capture.push_worker(FAILED);
        capture.push_worker(b"\n");
        assert!(capture.snapshot().reported_failure.is_some());
        // A line of exactly the limit is still parsed.
        let capture = OutputCapture::new();
        let mut exact = FAILED.to_vec();
        exact.resize(EVENT_LINE_MAX_BYTES, b' ');
        capture.push_worker(&exact);
        capture.push_worker(b"\n");
        assert!(capture.snapshot().reported_failure.is_some());
    }

    #[test]
    fn a_failure_keeps_the_supervisors_classification_and_gains_the_output() {
        let observed = ExecutionError::backend("libkrun worker exited with exit status: 1");
        let quiet = failure_with_output(observed.clone(), &GuestOutput::default());
        assert_eq!(quiet, observed);

        let output = GuestOutput {
            stdout_tail: "boom\n".into(),
            stdout_dropped_bytes: 7,
            ..GuestOutput::default()
        };
        let loud = failure_with_output(observed.clone(), &output);
        assert_eq!(
            (loud.code, loud.retryable),
            (observed.code, observed.retryable)
        );
        assert_eq!(
            loud.detail,
            "libkrun worker exited with exit status: 1\n--- guest console (tail; 7 earlier \
             bytes dropped) ---\nboom\n"
        );

        // A reported event, which the guest can forge, is text only: the
        // code and `retryable` stay the supervisor's.
        let reported = GuestOutput {
            stderr_tail: "noise".into(),
            reported_failure: Some(ExecutionError {
                code: ErrorCode::Unauthorized,
                retryable: true,
                detail: "cause".into(),
            }),
            ..GuestOutput::default()
        };
        let named = failure_with_output(observed.clone(), &reported);
        assert_eq!(
            (named.code, named.retryable),
            (observed.code, observed.retryable)
        );
        assert_eq!(
            named.detail,
            "libkrun worker exited with exit status: 1\nreported on the worker's standard \
             error, which the guest's standard error shares (unverified): Unauthorized, \
             retryable true: cause\n--- guest stderr and worker diagnostics (all of it) \
             ---\nnoise"
        );
    }

    #[test]
    fn the_bounds_are_the_documented_sizes() {
        assert_eq!(GUEST_OUTPUT_TAIL_BYTES, 16_384);
        assert_eq!(WORKER_STDERR_TAIL_BYTES, 8_192);
        assert_eq!(EVENT_LINE_MAX_BYTES, 65_536);
    }

    /// A reader that yields a scripted sequence of results, then end of file.
    struct Script(std::collections::VecDeque<std::io::Result<&'static [u8]>>);

    impl Read for Script {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            match self.0.pop_front() {
                None => Ok(0),
                Some(Ok(bytes)) => {
                    buffer[..bytes.len()].copy_from_slice(bytes);
                    Ok(bytes.len())
                }
                Some(Err(error)) => Err(error),
            }
        }
    }

    #[test]
    fn a_read_is_retried_only_when_interrupted() {
        use std::io::{Error, ErrorKind};
        let mut seen = Vec::new();
        read_until_end(
            Script(
                [
                    Err(Error::from(ErrorKind::Interrupted)),
                    Ok(&b"a"[..]),
                    Err(Error::from(ErrorKind::Other)),
                    Ok(&b"b"[..]),
                ]
                .into(),
            ),
            |data| seen.extend_from_slice(data),
        );
        assert_eq!(
            seen, b"a",
            "an interrupted read retries; any other error ends"
        );
    }

    /// A reader fed from a channel: each message is one read, and the
    /// sender's drop is end of file.
    struct Channel(std::sync::mpsc::Receiver<Vec<u8>>);

    impl Read for Channel {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            match self.0.recv() {
                Ok(bytes) => {
                    buffer[..bytes.len()].copy_from_slice(&bytes);
                    Ok(bytes.len())
                }
                Err(_) => Ok(0),
            }
        }
    }

    /// Spin until a wait has parked or `waiter` has finished without one.
    fn until_parked<T>(capture: &OutputCapture, waiter: &std::thread::JoinHandle<T>) {
        while capture.state.lock().parks == 0 && !waiter.is_finished() {
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_settle_waits_for_output_still_arriving() {
        let capture = Arc::new(OutputCapture::new());
        capture.drain_worker(&b""[..]);
        let (send, receive) = std::sync::mpsc::channel();
        let drainer = {
            let capture = Arc::clone(&capture);
            std::thread::spawn(move || capture.drain_guest(Channel(receive)))
        };
        let settler = {
            let capture = Arc::clone(&capture);
            std::thread::spawn(move || capture.settle())
        };
        until_parked(&capture, &settler);
        send.send(b"late".to_vec()).unwrap();
        drop(send);
        let output = settler.join().unwrap();
        drainer.join().unwrap();
        assert_eq!(
            output.stdout_tail, "late",
            "settle returned before the guest's stream ended"
        );
        assert!(output.untrusted);
    }

    #[test]
    fn a_wait_lasts_until_both_streams_end() {
        let capture = Arc::new(OutputCapture::new());
        capture.drain_guest(&b""[..]);
        let waiter = {
            let capture = Arc::clone(&capture);
            std::thread::spawn(move || {
                capture.wait(
                    &|output| output.stderr_tail.contains('x'),
                    Instant::now() + Duration::from_secs(60),
                )
            })
        };
        until_parked(&capture, &waiter);
        capture.drain_worker(&b"x"[..]);
        let seen = waiter.join().unwrap();
        assert_eq!(seen.stderr_tail, "x", "one ended stream ended the wait");
    }

    #[test]
    fn a_wait_returns_when_its_condition_holds_or_the_streams_end() {
        let capture = Arc::new(OutputCapture::new());
        let writer = Arc::clone(&capture);
        let deadline = Instant::now() + Duration::from_secs(30);
        let thread = std::thread::spawn(move || writer.drain_guest(&b"ready\n"[..]));
        let seen = capture.wait(&|output| output.stdout_tail.contains("ready"), deadline);
        assert_eq!(seen.stdout_tail, "ready\n");
        thread.join().unwrap();
        // Guest ended; the worker stream ending too ends a wait that can
        // never be satisfied, well before its deadline.
        capture.drain_worker(&b""[..]);
        let seen = capture.wait(&|_| false, deadline);
        assert_eq!(seen.stdout_tail, "ready\n");
        assert!(Instant::now() < deadline);
        // A wait whose deadline passes returns what it saw.
        let open = OutputCapture::new();
        let seen = open.wait(&|_| false, Instant::now());
        assert_eq!(seen, GuestOutput::default());
    }
}
