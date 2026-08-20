//! Shared worker process-group lifecycle for native and libkrun backends.
//!
//! Both backends supervise a worker the same way: establish a process group
//! before exec, wait for exit and cancellation as events, enforce one wall
//! clock deadline, then terminate the group and release the parent-owned
//! ephemeral tree. Keeping that single implementation here is what stops the
//! two copies from drifting — a termination path fixed in one and missed in
//! the other is exactly the defect this module was extracted to prevent.

use std::fs;
use std::io::{self, BufRead};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command};
use std::sync::{Arc, mpsc};
use std::time::Instant;

use parking_lot::Mutex;
use tempfile::TempDir;

use crate::ExecutionError;

/// Operator-facing names for one backend's worker and ephemeral tree. Only
/// diagnostics differ between the native and libkrun supervisors; the
/// lifecycle itself is identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SupervisionLabels {
    /// For example `native worker` or `libkrun worker`.
    pub(super) worker: &'static str,
    /// For example `native ephemeral rootfs` or `ephemeral rootfs volume`.
    pub(super) ephemeral: &'static str,
}

/// Place the worker in its own process group, so every terminal path can reap
/// the WHOLE group rather than just the leader.
///
/// Uses `Command::process_group` and NOT `pre_exec(setpgid)`, and the
/// difference is a hang, not a style preference.
///
/// `pre_exec` installs a closure, and std refuses `posix_spawn` when any
/// closure is present (`!self.get_closures().is_empty() => return Ok(None)` in
/// `sys::process::unix`). That forces the fork+exec path, which allocates a
/// CLOEXEC error pipe for the child to report exec failure on: the parent
/// blocks reading it until the child either writes an errno or execs, closing
/// its end via CLOEXEC.
///
/// On a platform without `pipe2` — macOS is one, and std says so in
/// `sys::pipe::unix`: "The only known way right now to create atomically set
/// the CLOEXEC flag is to use the `pipe2` syscall" — that pipe is created by
/// `libc::pipe()` and then marked CLOEXEC in a SECOND step. Between the two, a
/// fork on any other thread inherits the write end, and the inherited copy has
/// no CLOEXEC to close it. It outlives that child's exec, the original
/// parent's read never reaches EOF, and `Command::spawn` blocks forever.
///
/// That window is real, and closing it is why this uses `process_group`.
///
/// It is NOT known to be `ley-line-open-cdd5d0`. An interleaved A/B — 60
/// iterations per arm, arms alternating inside each iteration, 6 CPU hogs at
/// `--test-threads=16` — measured 2 failures on `pre_exec` against 3 on
/// `process_group`: indistinguishable. The flake's symptom is also a
/// readiness TIMEOUT, meaning `spawn` returned, which is not the hang this
/// mechanism produces. Do not read this comment as a fix for that bead.
///
/// `process_group` carries the same request through
/// `POSIX_SPAWN_SETPGROUP`/`posix_spawnattr_setpgroup` on the `posix_spawn`
/// fast path, so no such pipe is ever created and the window does not exist.
/// A failure to set the group still fails the spawn — libc reports it — which
/// is the property `a_worker_lands_in_its_own_process_group` pins directly.
pub(super) fn configure_process_group(command: &mut Command) {
    // 0 means "the child's own pid", matching `setpgid(0, 0)`.
    command.process_group(0);
}

pub(super) fn terminate_process_group(pid: u32) {
    // SAFETY: `pid` came directly from a live Child. A negative pid addresses
    // the process group established before exec; the direct-pid fallback
    // still reaps a worker if group setup failed at the OS boundary.
    unsafe {
        let process_group = -(pid as libc::pid_t);
        if libc::kill(process_group, libc::SIGKILL) == -1 {
            let _ = libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
    }
}

/// Wait for `pid` to exit **without reaping it**.
///
/// This is the load-bearing primitive for the whole module. Reaping frees the
/// pid for reuse, and a process-group id *is* a pid — so `kill(-pid)` issued
/// after the leader has been reaped can land on an unrelated process group.
/// macOS wraps its pid space at ~100k, so that is not a theoretical concern.
///
/// `WNOWAIT` leaves the child in a waitable state: the zombie keeps the pid,
/// and therefore the process-group id, allocated until someone actually reaps.
/// That is what makes the subsequent group kill safe.
fn await_exit_without_reaping(pid: u32) -> io::Result<()> {
    retry_while_interrupted(|| observe_exit(pid))
}

/// One `waitid` observation of `pid`.
fn observe_exit(pid: u32) -> io::Result<()> {
    // SAFETY: `waitid` with `WNOWAIT` only observes; it does not consume the
    // child. `info` is written by the kernel and never read here.
    let rc = unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOWAIT,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Retry `observe` while the kernel reports `EINTR`, propagating every other
/// error.
///
/// Split out from the syscall so the retry rule is testable: a signal
/// arriving mid-`waitid` is not something a test can schedule, so as one
/// inlined loop this branch was unobservable — invert the `EINTR` check and
/// nothing failed. The two ways to get it wrong have opposite failure modes
/// and both are covered below: treating `EINTR` as fatal aborts a healthy
/// wait, and treating a real error as `EINTR` spins forever.
fn retry_while_interrupted(mut observe: impl FnMut() -> io::Result<()>) -> io::Result<()> {
    loop {
        match observe() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

/// Kill the group, then reap. **This ordering is the module's one rule** and
/// every terminal path goes through here so it is stated exactly once.
fn kill_group_then_reap(child: &mut Child) -> io::Result<std::process::ExitStatus> {
    terminate_process_group(child.id());
    child.wait()
}

enum ProcessState {
    Live(Child),
    Reaped,
}

/// A worker process and its process group, with the kill-before-reap rule
/// encoded rather than left to call-site ordering.
///
/// The raw pid deliberately has no accessor: `pid` is reachable only from the
/// reaper, which holds the lock. Removing the getter is what makes "signal a
/// pid you no longer own" unrepresentable rather than merely discouraged.
///
/// The previous shape handed out a raw `Child` *and* a raw pid, so the reap
/// (on the waiting thread) and the group kill (on the supervising thread)
/// were ordered only by accident. Here the `Child` never escapes: the reaper
/// and every signaller take the same lock, so "signal a pid that was already
/// reaped" cannot be expressed.
pub(super) struct WorkerProcess {
    state: Arc<Mutex<ProcessState>>,
    pid: u32,
}

/// A handle that can request termination but can never reap.
#[derive(Clone)]
pub(super) struct TerminationSignal {
    state: Arc<Mutex<ProcessState>>,
}

impl TerminationSignal {
    /// Kill the worker's process group, unless it has already been reaped.
    /// Holding the lock across the kill is what makes the pid safe to use:
    /// the reaper cannot reap underneath it.
    pub(super) fn terminate(&self) {
        if let ProcessState::Live(child) = &*self.state.lock() {
            terminate_process_group(child.id());
        }
    }
}

impl WorkerProcess {
    pub(super) fn new(child: Child) -> Self {
        let pid = child.id();
        Self {
            state: Arc::new(Mutex::new(ProcessState::Live(child))),
            pid,
        }
    }

    pub(super) fn signal(&self) -> TerminationSignal {
        TerminationSignal {
            state: Arc::clone(&self.state),
        }
    }

    /// Wait for a natural exit, then terminate the group and reap.
    ///
    /// The group kill happens *after* exit is observed but *before* the reap,
    /// so descendants the worker left behind are collected while the pid is
    /// still provably ours. A worker exiting says nothing about its group: a
    /// guest that double-forked, or a worker killed while its guest kept
    /// running, both leave live processes whose rootfs cleanup is next.
    pub(super) fn await_exit_and_reap(self) -> io::Result<std::process::ExitStatus> {
        // Deliberately outside the lock: this blocks for the run's lifetime,
        // and signallers must stay able to terminate while it waits.
        let awaited = await_exit_without_reaping(self.pid);
        let mut state = self.state.lock();
        match std::mem::replace(&mut *state, ProcessState::Reaped) {
            ProcessState::Live(mut child) => {
                let status = kill_group_then_reap(&mut child);
                awaited.and(status)
            }
            ProcessState::Reaped => Err(io::Error::other("worker was already reaped")),
        }
    }

    /// Terminate and reap without waiting for a natural exit.
    pub(super) fn terminate_and_reap(&self) -> io::Result<std::process::ExitStatus> {
        let mut state = self.state.lock();
        match std::mem::replace(&mut *state, ProcessState::Reaped) {
            ProcessState::Live(mut child) => kill_group_then_reap(&mut child),
            ProcessState::Reaped => Err(io::Error::other("worker was already reaped")),
        }
    }
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        if matches!(&*self.state.lock(), ProcessState::Live(_)) {
            let _ = self.terminate_and_reap();
        }
    }
}

pub(super) fn read_readiness_line(reader: &mut impl BufRead) -> Result<Option<String>, String> {
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map(|bytes| if bytes == 0 { None } else { Some(line) })
        .map_err(|error| error.to_string())
}

/// Supervise one started worker until a terminal event, then terminate its
/// process group and release the parent-owned ephemeral tree.
///
/// The process group is killed on **every** terminal path, including a clean
/// exit. A worker's own exit says nothing about descendants it left behind in
/// the group: a guest that double-forked, or a worker that was itself killed
/// while its guest kept running, both leave live processes whose rootfs is
/// about to be deleted. `SIGKILL` to an already-empty group is a no-op, so
/// the unconditional call costs nothing on the ordinary path.
pub(super) fn supervise_worker(
    process: WorkerProcess,
    run_root: TempDir,
    cancel: mpsc::Receiver<()>,
    deadline: Instant,
    labels: SupervisionLabels,
) -> Result<(), ExecutionError> {
    // `Child::try_wait` plus a short timeout is scheduler polling. Instead,
    // dedicate one thread to the blocking wait and merge its exit event with
    // cancellation events. The only timed wait below is the actual deadline;
    // no child-state polling or sleep is involved.
    enum Event {
        Exited(io::Result<std::process::ExitStatus>),
        Cancelled,
    }
    let signal = process.signal();
    let (event_tx, event_rx) = mpsc::sync_channel(2);
    let exit_tx = event_tx.clone();
    std::thread::spawn(move || {
        // The reaper. It is the only thing that reaps, and it kills the group
        // first — see `WorkerProcess::await_exit_and_reap`.
        let _ = exit_tx.send(Event::Exited(process.await_exit_and_reap()));
    });
    std::thread::spawn(move || {
        if cancel.recv().is_ok() {
            let _ = event_tx.send(Event::Cancelled);
        }
    });

    let event = match deadline.checked_duration_since(Instant::now()) {
        Some(remaining) => match event_rx.recv_timeout(remaining) {
            Ok(event) => event,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                signal.terminate();
                let _ = event_rx.recv();
                return cleanup_with_error(run_root, labels, wall_clock_exceeded());
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                signal.terminate();
                let _ = event_rx.recv();
                return cleanup_with_error(
                    run_root,
                    labels,
                    ExecutionError::backend(format!(
                        "{} supervision channel disconnected",
                        labels.worker
                    )),
                );
            }
        },
        None => {
            signal.terminate();
            let _ = event_rx.recv();
            return cleanup_with_error(run_root, labels, wall_clock_exceeded());
        }
    };

    // No group kill on this side of an `Exited` event. The reaper already
    // killed the group before reaping; killing here would be signalling a pid
    // that has been freed and possibly recycled.
    match event {
        Event::Cancelled => {
            signal.terminate();
            let _ = event_rx.recv();
            cleanup_tempdir(run_root, labels)
        }
        Event::Exited(Ok(status)) if status.success() => cleanup_tempdir(run_root, labels),
        Event::Exited(Ok(status)) => cleanup_with_error(
            run_root,
            labels,
            ExecutionError::backend(format!("{} exited with {status}", labels.worker)),
        ),
        Event::Exited(Err(error)) => cleanup_with_error(
            run_root,
            labels,
            ExecutionError::backend(format!("wait for {}: {error}", labels.worker)),
        ),
    }
}

fn wall_clock_exceeded() -> ExecutionError {
    ExecutionError {
        code: crate::ErrorCode::ResourceExhausted,
        retryable: false,
        detail: "execution exceeded wall-clock limit".into(),
    }
}

/// Release the ephemeral tree while preserving an already-observed failure.
/// A cleanup failure is appended rather than replacing the original cause.
pub(super) fn cleanup_with_error(
    run_root: TempDir,
    labels: SupervisionLabels,
    error: ExecutionError,
) -> Result<(), ExecutionError> {
    Err(finish_failed_start(run_root, labels, error))
}

pub(super) fn cleanup_tempdir(
    run_root: TempDir,
    labels: SupervisionLabels,
) -> Result<(), ExecutionError> {
    make_tree_removable(run_root.path(), labels)?;
    run_root
        .close()
        .map_err(|error| ExecutionError::backend(format!("remove {}: {error}", labels.ephemeral)))
}

/// Terminate a worker that never reached readiness, then clean up.
pub(super) fn abort_failed_start(
    child: &mut Child,
    run_root: TempDir,
    labels: SupervisionLabels,
    error: ExecutionError,
) -> ExecutionError {
    terminate_process_group(child.id());
    let _ = child.wait();
    finish_failed_start(run_root, labels, error)
}

pub(super) fn finish_failed_start(
    run_root: TempDir,
    labels: SupervisionLabels,
    error: ExecutionError,
) -> ExecutionError {
    match cleanup_tempdir(run_root, labels) {
        Ok(()) => error,
        Err(cleanup_error) => ExecutionError {
            detail: format!(
                "{}; cleanup also failed: {}",
                error.detail, cleanup_error.detail
            ),
            ..error
        },
    }
}

/// Restore owner traversal on a tree a guest may have chmod'd shut, so the
/// parent can remove it. Regular files are leaves; symlinks are never
/// followed.
pub(super) fn make_tree_removable(
    path: &Path,
    labels: SupervisionLabels,
) -> Result<(), ExecutionError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        ExecutionError::backend(format!(
            "inspect {} cleanup path: {error}",
            labels.ephemeral
        ))
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(());
    }
    let mut permissions = metadata.permissions();
    permissions.set_mode(permissions.mode() | 0o700);
    fs::set_permissions(path, permissions).map_err(|error| {
        ExecutionError::backend(format!(
            "restore {} cleanup permissions: {error}",
            labels.ephemeral
        ))
    })?;
    for entry in fs::read_dir(path).map_err(|error| {
        ExecutionError::backend(format!(
            "enumerate {} for cleanup: {error}",
            labels.ephemeral
        ))
    })? {
        make_tree_removable(
            &entry
                .map_err(|error| {
                    ExecutionError::backend(format!(
                        "enumerate {} cleanup entry: {error}",
                        labels.ephemeral
                    ))
                })?
                .path(),
            labels,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        SupervisionLabels, WorkerProcess, await_exit_without_reaping, cleanup_tempdir,
        configure_process_group, finish_failed_start, make_tree_removable, read_readiness_line,
        retry_while_interrupted, supervise_worker, terminate_process_group,
    };
    use crate::ExecutionError;
    use std::fs;
    use std::io;
    use std::io::{BufRead, BufReader, Cursor, Read};
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use wait_timeout::ChildExt;

    const LABELS: SupervisionLabels = SupervisionLabels {
        worker: "test worker",
        ephemeral: "test ephemeral rootfs",
    };

    /// The pid-safety invariant, proved directly.
    ///
    /// A reaped pid is free for reuse, and a process-group id *is* a pid, so
    /// `kill(-pid)` after a reap can land on an unrelated group. The reaper
    /// therefore observes exit with `WNOWAIT` and kills the group *before*
    /// reaping. This asserts the observation genuinely does not consume the
    /// child: a second observation must also succeed. If the first had reaped,
    /// this fails with ECHILD — which is exactly the freed pid we must avoid.
    #[test]
    fn awaiting_exit_leaves_the_child_reapable_so_its_group_stays_allocated() {
        let mut command = Command::new("sh");
        command.args(["-c", "exit 3"]);
        configure_process_group(&mut command);
        let mut child = command.spawn().expect("spawn exiting fixture");
        let pid = child.id();

        await_exit_without_reaping(pid).expect("observe exit without reaping");
        await_exit_without_reaping(pid)
            .expect("child must still be reapable — observation must not free the pid");

        assert_eq!(
            child.wait().expect("reap").code(),
            Some(3),
            "the status must survive to the real reap"
        );
    }

    /// The worker must land in its OWN process group, not share its parent's.
    ///
    /// A worker in the parent's group is the dangerous shape: the supervisor
    /// believes it owns a group it does not, and the guarantee that every
    /// terminal path reaps the *whole* group quietly degrades to reaping one
    /// process.
    ///
    /// This asserts the guarantee DIRECTLY — spawn, then ask the OS which
    /// group the child is in. The previous version injected a failing
    /// `setpgid` instead, because `pre_exec` was the mechanism and a real
    /// `setpgid(0, 0)` has no portable way to fail here. `process_group` moved
    /// the request into `posix_spawn` (see `configure_process_group` for why
    /// that matters — it is what stops `Command::spawn` wedging), where there
    /// is no callback to inject into. The positive property is both observable
    /// and the thing actually promised, so it is the better assertion
    /// regardless: it fails if the call is dropped, and it fails if the group
    /// is established as something other than the child's own.
    #[test]
    fn a_worker_lands_in_its_own_process_group() {
        let mut command = Command::new("sleep");
        command.arg("30");
        configure_process_group(&mut command);

        let mut child = command.spawn().expect("sleep must spawn");
        let pid = child.id() as libc::pid_t;
        // SAFETY: both calls only read process-group state. `getpgid(0)` is
        // this process; `getpgid(pid)` is the live child, which has not been
        // reaped yet because `child` is still held.
        let (child_pgid, parent_pgid) = unsafe { (libc::getpgid(pid), libc::getpgid(0)) };

        terminate_process_group(child.id());
        let _ = child.wait();

        assert_eq!(
            child_pgid, pid,
            "the worker must LEAD its own process group, so that signalling \
             -pgid reaches the worker and everything it spawned"
        );
        assert_ne!(
            child_pgid, parent_pgid,
            "a worker sharing the supervisor's group means every group-wide \
             kill either misses the worker's children or hits the supervisor"
        );
    }

    /// `EINTR` is a retry, not a failure: a signal arriving mid-`waitid` must
    /// not abort a healthy wait.
    #[test]
    fn an_interrupted_observation_is_retried_rather_than_reported() {
        let mut attempts = 0;
        let result = retry_while_interrupted(|| {
            attempts += 1;
            if attempts < 3 {
                Err(io::Error::from(io::ErrorKind::Interrupted))
            } else {
                Ok(())
            }
        });

        result.expect("an interrupted wait must resume, not fail");
        assert_eq!(
            attempts, 3,
            "each interruption must be retried exactly once"
        );
    }

    /// The other direction: a real error must propagate. Treating it as an
    /// interruption would spin forever on a wait that can never succeed.
    #[test]
    fn a_non_interrupt_error_propagates_instead_of_spinning() {
        let mut attempts = 0;
        let error = retry_while_interrupted(|| {
            attempts += 1;
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        })
        .expect_err("a non-interrupt error must be reported");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(attempts, 1, "a fatal error must not be retried");
    }

    /// The direct-pid fallback, proved by denying it the group it falls back
    /// from.
    ///
    /// This child is spawned **without** `configure_process_group`, so it
    /// stays in the test runner's group and no process group carries its pid
    /// as an id. `kill(-pid)` therefore fails with ESRCH and only the
    /// fallback can reach it. That makes the group kill's error check the
    /// entire subject of this test: invert it, or make it never match, and
    /// this child outlives the call.
    ///
    /// The comment on `terminate_process_group` claims the fallback "still
    /// reaps a worker if group setup failed at the OS boundary". Nothing
    /// proved that until here — every other test in this module configures
    /// the group first, so they all exercise the success path.
    #[test]
    fn terminating_a_worker_with_no_group_of_its_own_falls_back_to_the_pid() {
        let mut child = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn long-lived fixture");
        let pid = child.id();

        terminate_process_group(pid);

        let status = child
            .wait_timeout(Duration::from_secs(5))
            .expect("wait on the fixture");
        assert!(
            status.is_some(),
            "the direct-pid fallback must reap a worker that has no process \
             group of its own; without it this child sleeps out the timeout"
        );
    }

    /// A signaller that races the reaper must never signal a freed pid. Once
    /// reaped, `terminate` takes the same lock, sees `Reaped`, and does
    /// nothing — the kill is unreachable rather than merely unlikely.
    #[test]
    fn terminating_after_the_reap_cannot_signal_a_recycled_pid() {
        let mut command = Command::new("sh");
        command.args(["-c", "exit 0"]);
        configure_process_group(&mut command);
        let child = command.spawn().expect("spawn exiting fixture");

        let process = WorkerProcess::new(child);
        let signal = process.signal();
        process.terminate_and_reap().expect("reap");

        signal.terminate();
        assert!(
            process.terminate_and_reap().is_err(),
            "a second reap must report the worker is already gone, not wait again"
        );
    }

    #[test]
    fn readiness_line_distinguishes_data_from_eof() {
        let mut event = Cursor::new(b"ready\n");
        assert_eq!(
            read_readiness_line(&mut event).expect("read readiness"),
            Some("ready\n".into())
        );

        let mut eof = Cursor::new(Vec::<u8>::new());
        assert_eq!(read_readiness_line(&mut eof).expect("read EOF"), None);
    }

    #[test]
    fn termination_closes_descendant_inherited_handles() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "tail -f /dev/null & echo $!; wait"])
            .stdout(Stdio::piped());
        configure_process_group(&mut command);
        let mut child = command.spawn().expect("spawn process group fixture");
        let stdout = child.stdout.take().expect("fixture stdout");
        let mut reader = BufReader::new(stdout);
        let mut child_pid = String::new();
        reader.read_line(&mut child_pid).expect("descendant pid");
        let descendant: libc::pid_t = child_pid.trim().parse().expect("numeric descendant pid");

        let (closed_tx, closed_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut remainder = Vec::new();
            let result = reader.read_to_end(&mut remainder);
            let _ = closed_tx.send(result);
        });

        drop(WorkerProcess::new(child));
        assert_descendant_released(closed_rx, descendant, "group termination");
    }

    /// A worker that exits on its own does not take its process group with
    /// it. The guest may have double-forked, or the worker may itself have
    /// been killed while the guest kept running — either way the supervisor
    /// is about to delete the rootfs those descendants are executing from.
    #[test]
    fn supervision_releases_the_group_after_a_nonzero_worker_exit() {
        assert_supervision_releases_descendants("tail -f /dev/null & echo $!; exit 7", true);
    }

    #[test]
    fn supervision_releases_the_group_after_a_successful_worker_exit() {
        assert_supervision_releases_descendants("tail -f /dev/null & echo $!; exit 0", false);
    }

    fn assert_supervision_releases_descendants(script: &str, expect_failure: bool) {
        let run_root = tempfile::tempdir().expect("run root");
        let mut command = Command::new("sh");
        command.args(["-c", script]).stdout(Stdio::piped());
        configure_process_group(&mut command);
        let mut child = command.spawn().expect("spawn supervised fixture");
        let stdout = child.stdout.take().expect("fixture stdout");
        let mut reader = BufReader::new(stdout);
        let mut child_pid = String::new();
        reader.read_line(&mut child_pid).expect("descendant pid");
        let descendant: libc::pid_t = child_pid.trim().parse().expect("numeric descendant pid");

        let (closed_tx, closed_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut remainder = Vec::new();
            let result = reader.read_to_end(&mut remainder);
            let _ = closed_tx.send(result);
        });

        let (_cancel_tx, cancel_rx) = mpsc::channel();
        let deadline = Instant::now() + Duration::from_secs(30);
        let outcome = supervise_worker(
            WorkerProcess::new(child),
            run_root,
            cancel_rx,
            deadline,
            LABELS,
        );
        assert_eq!(
            outcome.is_err(),
            expect_failure,
            "supervision outcome must follow the worker's exit status: {outcome:?}"
        );

        assert_descendant_released(closed_rx, descendant, "worker exit");
    }

    /// EOF on the inherited handle is the observable: it arrives exactly when
    /// the last descendant holding it dies. No sleep, no liveness poll.
    fn assert_descendant_released(
        closed_rx: mpsc::Receiver<std::io::Result<usize>>,
        descendant: libc::pid_t,
        after: &str,
    ) {
        let closed = closed_rx.recv_timeout(Duration::from_secs(5));
        if closed.is_err() {
            // SAFETY: the pid was emitted by the live fixture immediately
            // before the assertion; this is failure-path cleanup only.
            unsafe {
                let _ = libc::kill(descendant, libc::SIGKILL);
            }
        }
        closed
            .unwrap_or_else(|_| panic!("descendant kept an inherited handle after {after}"))
            .expect("read fixture stdout");
    }

    #[test]
    fn cleanup_failure_preserves_original_error_context() {
        let root = tempfile::tempdir().expect("tempdir");
        fs::remove_dir(root.path()).expect("remove fixture path");
        let error = finish_failed_start(root, LABELS, ExecutionError::backend("worker failed"));
        assert!(error.detail.contains("worker failed"));
        assert!(error.detail.contains("cleanup also failed"));
    }

    #[test]
    fn removable_tree_walks_directories_but_leaves_files_as_leaves() {
        let root = tempfile::tempdir().expect("tempdir");
        let locked = root.path().join("locked");
        fs::create_dir(&locked).expect("locked directory");
        let file = locked.join("file");
        fs::write(&file, b"content").expect("file");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))
            .expect("restrict directory");

        make_tree_removable(root.path(), LABELS).expect("restore tree permissions");
        assert_eq!(
            fs::metadata(&locked)
                .expect("locked metadata")
                .permissions()
                .mode()
                & 0o700,
            0o700
        );
        make_tree_removable(&file, LABELS).expect("regular file is a leaf");
        cleanup_tempdir(root, LABELS).expect("cleanup restored tree");
    }
}
