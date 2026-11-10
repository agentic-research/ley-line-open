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

use crate::{ExecutionError, RunOutput};

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
        let data = if data.len() > self.cap {
            self.dropped += (data.len() - self.cap) as u64;
            &data[data.len() - self.cap..]
        } else {
            data
        };
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
}

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
            }),
            changed: Condvar::new(),
        }
    }

    /// Read the guest's console until it ends.
    pub fn drain_guest(&self, mut stream: impl Read) {
        let mut buffer = [0u8; 8192];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    self.state.lock().guest.push(&buffer[..read]);
                    self.changed.notify_all();
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        self.state.lock().guest_open = false;
        self.changed.notify_all();
    }

    /// Read the worker's standard error after readiness until it ends,
    /// keeping its tail and its terminal `failed` event.
    pub fn drain_worker(&self, mut stream: impl Read) {
        let mut buffer = [0u8; 8192];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    self.push_worker(&buffer[..read]);
                    self.changed.notify_all();
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
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
    pub fn snapshot(&self) -> RunOutput {
        snapshot(&self.state.lock())
    }

    /// Wait, at most until `deadline`, for `until` to hold of the output or
    /// for both streams to end. Returns the output it saw last.
    pub fn wait(&self, until: &dyn Fn(&RunOutput) -> bool, deadline: Instant) -> RunOutput {
        let mut state = self.state.lock();
        loop {
            let output = snapshot(&state);
            if until(&output) || (!state.guest_open && !state.worker_open) {
                return output;
            }
            if self.changed.wait_until(&mut state, deadline).timed_out() {
                return snapshot(&state);
            }
        }
    }

    /// The failure a finished run reports: the supervisor's `observed`
    /// failure, whose code and `retryable` it keeps, followed by any
    /// reported `failed` event (as unverified text) and what the run
    /// printed.
    pub fn fold_into(&self, observed: ExecutionError) -> ExecutionError {
        let deadline = Instant::now() + DRAIN_BOUND;
        let output = self.wait(&|_| false, deadline);
        failure_with_output(observed, &output)
    }
}

fn snapshot(state: &State) -> RunOutput {
    RunOutput {
        guest: state.guest.text(),
        guest_dropped_bytes: state.guest.dropped,
        worker_stderr: state.worker.text(),
        worker_stderr_dropped_bytes: state.worker.dropped,
        reported_failure: state.failure.clone(),
    }
}

fn parse_event(line: &[u8], failure: &mut Option<ExecutionError>) {
    let line = line.trim_ascii();
    if !line.starts_with(b"{") {
        return;
    }
    if let Ok(WorkerEvent::Failed { error }) = serde_json::from_slice::<WorkerEvent>(line) {
        *failure = Some(error);
    }
}

/// `observed`, replaced by the worker's own failure when it reported one,
/// with the run's printed output appended to the detail.
pub fn failure_with_output(observed: ExecutionError, output: &RunOutput) -> ExecutionError {
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
        ("guest console", &output.guest, output.guest_dropped_bytes),
        (
            "guest stderr and worker diagnostics",
            &output.worker_stderr,
            output.worker_stderr_dropped_bytes,
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
        assert!(output.worker_stderr.starts_with("libkrun noise\n"));

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
        let quiet = failure_with_output(observed.clone(), &RunOutput::default());
        assert_eq!(quiet, observed);

        let output = RunOutput {
            guest: "boom\n".into(),
            guest_dropped_bytes: 7,
            worker_stderr: String::new(),
            worker_stderr_dropped_bytes: 0,
            reported_failure: None,
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
        let reported = RunOutput {
            worker_stderr: "noise".into(),
            reported_failure: Some(ExecutionError {
                code: ErrorCode::Unauthorized,
                retryable: true,
                detail: "cause".into(),
            }),
            ..RunOutput::default()
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
    fn a_wait_returns_when_its_condition_holds_or_the_streams_end() {
        let capture = Arc::new(OutputCapture::new());
        let writer = Arc::clone(&capture);
        let deadline = Instant::now() + Duration::from_secs(30);
        let thread = std::thread::spawn(move || writer.drain_guest(&b"ready\n"[..]));
        let seen = capture.wait(&|output| output.guest.contains("ready"), deadline);
        assert_eq!(seen.guest, "ready\n");
        thread.join().unwrap();
        // Guest ended; the worker stream ending too ends a wait that can
        // never be satisfied, well before its deadline.
        capture.drain_worker(&b""[..]);
        let seen = capture.wait(&|_| false, deadline);
        assert_eq!(seen.guest, "ready\n");
        assert!(Instant::now() < deadline);
        // A wait whose deadline passes returns what it saw.
        let open = OutputCapture::new();
        let seen = open.wait(&|_| false, Instant::now());
        assert_eq!(seen, RunOutput::default());
    }
}
