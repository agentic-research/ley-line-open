//! Lifecycle backend supervising the first-party native nono worker.

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use tempfile::Builder as TempDirBuilder;

use crate::{
    Backend, BackendCapabilities, BackendClass, BackendRun, BackendRunStatus, ExecutionError,
    ExecutionRequest,
};

use super::cgroup::{CgroupRoot, RunCgroup};
#[cfg(feature = "test-support")]
use super::libkrun::backend::StartPoint;
use super::native::WorkerEvent;
use super::process::{
    Supervised, SupervisionLabels, WorkerProcess, abort_failed_start, cleanup_tempdir,
    configure_process_group, finish_failed_start, published_status, read_readiness_line,
    supervise_worker, terminate_process_group,
};

const LABELS: SupervisionLabels = SupervisionLabels {
    worker: "native worker",
    ephemeral: "native ephemeral rootfs",
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeWorkerConfig {
    pub worker: PathBuf,
    pub cas_root: PathBuf,
    pub ephemeral_root: PathBuf,
    pub runtime_files: Vec<PathBuf>,
    pub ready_timeout: Duration,
}

pub struct NativeWorkerBackend {
    config: NativeWorkerConfig,
    /// The operator-delegated cgroup v2 root, when there is one. Without it
    /// this tier enforces the wall clock only, and refuses memory, CPU and
    /// pids ceilings by name (ADR-0035, "OCI resources on execution/v1").
    cgroup: Option<CgroupRoot>,
    start_lock: Mutex<()>,
    /// Runs whose worker is spawned and has not reported readiness, each
    /// with the channel its `start` waits on, so `cancel` can stop a run in
    /// that window instead of answering "nothing to cancel" while its worker
    /// runs (bead `ley-line-open-62e3b1`). Taken before `children` wherever
    /// both are held, and a run leaves this map for `children` under it, so
    /// a cancel always finds a spawned run in one of the two.
    starting: Mutex<HashMap<String, mpsc::Sender<StartEvent>>>,
    children: Arc<Mutex<HashMap<String, WorkerControl>>>,
    cleanup_errors: Arc<Mutex<Vec<ExecutionError>>>,
    completed: Arc<Mutex<HashMap<String, BackendRunStatus>>>,
    completion_cv: Arc<Condvar>,
    /// Observes points in `start` and `cancel` a test must order events
    /// around (`test-support` only).
    #[cfg(feature = "test-support")]
    hook: Option<Arc<dyn Fn(StartPoint) + Send + Sync>>,
}

struct WorkerControl {
    cancel: mpsc::Sender<()>,
    finished: mpsc::Receiver<Result<(), ExecutionError>>,
}

/// What a starting run's `start` waits on: the worker's readiness line, or a
/// cancel, answered on its sender once the worker is reaped, with whether its
/// run root was removed.
enum StartEvent {
    Readiness(Result<Option<String>, String>),
    Cancel(mpsc::SyncSender<Result<bool, ExecutionError>>),
}

/// A starting run's entry in `starting`, removed when the start ends without
/// publishing the run. Publication removes it itself, under the same lock as
/// the insert into `children`, and disarms this.
///
/// A start that ends without publishing answers every cancel still queued
/// for it with what is true once it has cleaned up: `Ok(false)` when no
/// worker was ever spawned, an error naming the run root when that root
/// remains, and `Ok(true)` otherwise.
struct Starting<'a> {
    starting: &'a Mutex<HashMap<String, mpsc::Sender<StartEvent>>>,
    run_id: Option<String>,
    /// What `start` waits on: readiness, or a cancel.
    events: mpsc::Receiver<StartEvent>,
    /// Whether the worker was spawned.
    spawned: bool,
    /// The run root the start removes when it fails.
    run_root: std::path::PathBuf,
    #[cfg(feature = "test-support")]
    hook: Option<Arc<dyn Fn(StartPoint) + Send + Sync>>,
}

impl Drop for Starting<'_> {
    fn drop(&mut self) {
        let Some(run_id) = self.run_id.take() else {
            return;
        };
        #[cfg(feature = "test-support")]
        if let Some(hook) = &self.hook {
            hook(StartPoint::Ending);
        }
        // Under the lock a cancel holds while it queues, so every cancel
        // queued for this run is in `events` once the entry is gone.
        let mut starting = self.starting.lock();
        starting.remove(&run_id);
        let answer = if !self.spawned {
            Ok(false)
        } else if self.run_root.exists() {
            Err(ExecutionError::backend(format!(
                "the run was cancelled while its start failed, and its run root {} could not be removed",
                self.run_root.display()
            )))
        } else {
            Ok(true)
        };
        while let Ok(event) = self.events.try_recv() {
            if let StartEvent::Cancel(acknowledge) = event {
                let _ = acknowledge.send(answer.clone());
            }
        }
        drop(starting);
    }
}

/// End a starting run on a cancel: kill and reap its worker, remove its run
/// root, answer the cancel with whether the removal worked, and return the
/// error the start ends with.
fn cancel_starting(
    child: &mut std::process::Child,
    run_root: tempfile::TempDir,
    acknowledge: mpsc::SyncSender<Result<bool, ExecutionError>>,
) -> ExecutionError {
    terminate_process_group(child.id());
    let _ = child.wait();
    let cleanup = cleanup_tempdir(run_root, LABELS);
    let _ = acknowledge.send(cleanup.clone().map(|()| true));
    let mut error = ExecutionError {
        code: crate::ErrorCode::Cancelled,
        retryable: false,
        detail: "the run was cancelled before its native worker reported readiness".into(),
    };
    if let Err(cleanup) = cleanup {
        error.detail = format!("{}; cleanup also failed: {}", error.detail, cleanup.detail);
    }
    error
}

impl NativeWorkerBackend {
    pub fn new(config: NativeWorkerConfig) -> Self {
        Self {
            config,
            cgroup: None,
            start_lock: Mutex::new(()),
            starting: Mutex::new(HashMap::new()),
            children: Arc::new(Mutex::new(HashMap::new())),
            cleanup_errors: Arc::new(Mutex::new(Vec::new())),
            completed: Arc::new(Mutex::new(HashMap::new())),
            completion_cv: Arc::new(Condvar::new()),
            #[cfg(feature = "test-support")]
            hook: None,
        }
    }

    /// Call `hook` at each [`StartPoint`], on the thread that reaches it,
    /// which waits for it to return. `test-support` only.
    #[cfg(feature = "test-support")]
    pub fn with_start_hook(mut self, hook: impl Fn(StartPoint) + Send + Sync + 'static) -> Self {
        self.hook = Some(Arc::new(hook));
        self
    }

    /// Enforce memory, CPU bandwidth and pids ceilings through `root`, for
    /// each controller it delegates. Every run then gets its own cgroup under
    /// `root`, which also makes teardown `cgroup.kill` of the whole tree.
    pub fn with_cgroup_root(mut self, root: CgroupRoot) -> Self {
        self.cgroup = Some(root);
        self
    }

    fn configured(&self) -> bool {
        self.config.worker.is_file()
            && self.config.cas_root.is_dir()
            && self.config.ephemeral_root.is_dir()
            && self.config.runtime_files.iter().all(|path| path.is_file())
    }

    pub fn take_cleanup_errors(&self) -> Vec<ExecutionError> {
        std::mem::take(&mut *self.cleanup_errors.lock())
    }

    /// Wait for terminal completion published after cleanup and diagnostics.
    /// This is the synchronization contract; callers must not infer lifecycle
    /// completion from directory state or timing.
    pub fn wait_for_completion(&self, run_id: &str) -> Result<BackendRunStatus, ExecutionError> {
        let mut completed = self.completed.lock();
        loop {
            if let Some(status) = completed.remove(run_id) {
                return Ok(status);
            }
            if !self.children.lock().contains_key(run_id) {
                return Err(ExecutionError::invalid(format!(
                    "run_id is not active or pending completion: {run_id}"
                )));
            }
            self.completion_cv.wait(&mut completed);
        }
    }

    /// Stop one run and remove its run root.
    ///
    /// `Ok(true)` once the run's worker is reaped and its run root removed,
    /// whether the run was running or still starting; `Ok(false)` only when
    /// there is no such run and no worker for it.
    pub fn cancel(&self, run_id: &str) -> Result<bool, ExecutionError> {
        let starting = self.starting.lock();
        let control = self.children.lock().remove(run_id);
        let Some(control) = control else {
            // Spawned, and not yet ready: its `start` owns the worker. Ask it
            // to stop, and wait until it has. A start already ending for
            // another reason drops its receiver only after reaping the worker
            // and removing the run root, so either answer means the same.
            let Some(start) = starting.get(run_id) else {
                return Ok(false);
            };
            let (acknowledge, acknowledged) = mpsc::sync_channel(1);
            let sent = start.send(StartEvent::Cancel(acknowledge)).is_ok();
            drop(starting);
            #[cfg(feature = "test-support")]
            if let Some(hook) = &self.hook {
                hook(StartPoint::CancelSignalled);
            }
            // The start's answer is whether the run root went too. No answer
            // means the start ended on its own failure, which reaps and
            // cleans up before its receiver is dropped.
            return match sent.then(|| acknowledged.recv()) {
                Some(Ok(answer)) => answer,
                // Unreachable: a start answers every queued cancel before
                // it drops its receiver (`Starting`).
                _ => Ok(true),
            };
        };
        drop(starting);
        let _ = control.cancel.send(());
        control.finished.recv().map_err(|_| {
            ExecutionError::backend("native worker supervisor stopped before cleanup completed")
        })??;
        // Cancellation is represented by the shared lifecycle, not as a
        // successful backend completion. Drop the supervisor's completion
        // marker so a later poll cannot retain a stale terminal result.
        self.completed.lock().remove(run_id);
        Ok(true)
    }
}

impl Backend for NativeWorkerBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            backend_id: "native-nono/1".into(),
            backend_class: BackendClass::Native,
            available: self.configured(),
            unavailable_reason: (!self.configured()).then(|| {
                "native nono worker backend is not configured with existing first-party resources"
                    .into()
            }),
            // nono mediates capabilities, not resources. Its manifest does
            // carry a `resources` block, but its own schema scopes that to
            // "enforced by the supervisor. Requires exec_strategy:
            // \"supervised\"" — nono's CLI runner, not the library this
            // links. So the wall clock (which `supervise_worker` enforces
            // here) is this tier's only ceiling unless the operator delegated
            // a cgroup v2 root, which only Linux has. macOS has no mechanism
            // with the right semantics for memory, CPU bandwidth or pids:
            // RLIMIT_AS is RLIMIT_RSS's alias there and lowering it fails,
            // RLIMIT_CPU is CPU-seconds, RLIMIT_NPROC is per uid.
            enforced: self.cgroup.as_ref().map_or_else(
                crate::EnforcedCeilings::wall_clock_only,
                CgroupRoot::enforced,
            ),
        }
    }

    fn start(
        &self,
        request: &ExecutionRequest,
        workspaces: crate::ServedWorkspaces,
    ) -> Result<BackendRun, ExecutionError> {
        request.validate()?;
        // ADR-0035 §4 again, at the backend: `ExecutionService::start` checks
        // too, but this is a public type an embedder can call directly, and a
        // direct call once accepted `memory_mib: 64` and applied nothing.
        self.capabilities().enforced.check(&request.limits)?;
        if !self.configured() {
            return Err(ExecutionError::backend(
                "native nono worker backend is not configured with existing first-party resources",
            ));
        }
        // A workspace grant is Graph authority, and this tier has no share to
        // deliver it through. Starting anyway ran the workload without the
        // workspace while the receipt named its graph root as an input; the
        // microVM tier refuses the same grant when it has no share provider
        // (bead ley-line-open-f8ebcf). The code is `workspaceUnserved`, not
        // the generic `unsupportedBackend`, so a caller can route the run to
        // a tier that serves workspaces (bead ley-line-open-204a94).
        //
        // Refused whether the workspace is named by the request or handed
        // over as an open share (bead ley-line-open-1022a8): the tier
        // attaches none, so it takes none.
        if let Some(grant) = request.workspaces.first().or(workspaces.grants().next()) {
            return Err(ExecutionError::workspace_unserved(format!(
                "the run is granted workspace {:?}, and the native tier serves no \
                 workspace shares; run it on the microVM tier",
                grant.name
            )));
        }
        let _start_guard = self.start_lock.lock();
        if self.children.lock().contains_key(&request.run_id) {
            return Err(ExecutionError {
                code: crate::ErrorCode::ResourceConflict,
                retryable: false,
                detail: format!("run_id already active: {}", request.run_id),
            });
        }

        // Created before the worker exists, so a ceiling the kernel refuses
        // (an out-of-range value, a root that stopped being writable) fails
        // the start with nothing to tear down but the directory. Dropped on
        // every early return below, which removes it.
        let run_cgroup: Option<RunCgroup> = self
            .cgroup
            .as_ref()
            .map(|root| root.create_run(&request.run_id, &request.limits))
            .transpose()?;
        let run_root = TempDirBuilder::new()
            .prefix("leyline-native-run-")
            .tempdir_in(&self.config.ephemeral_root)
            .map_err(|error| ExecutionError::backend(format!("create native run root: {error}")))?;
        let mut command = Command::new(&self.config.worker);
        command
            .arg("--cas-root")
            .arg(&self.config.cas_root)
            .arg("--run-root")
            .arg(run_root.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for path in &self.config.runtime_files {
            command.arg("--runtime-file").arg(path);
        }
        // The worker's standard input is its lifeline (bead
        // ley-line-open-62e3b1): after the request, nothing more is written,
        // and this process keeps the write end until the worker is reaped.
        // When this process ends, for any reason, the worker reads
        // end-of-file and ends, its workload with it. Standard input rather
        // than a descriptor of its own, because handing over any other
        // number takes a `pre_exec` hook, which gives up the `posix_spawn`
        // path `configure_process_group` depends on.
        command.arg(super::native::LIFELINE_FD_ARG).arg("0");
        configure_process_group(&mut command);

        // Registered before the worker exists, so from its spawn until the
        // run is published a cancel reaches it through `event_tx`: there is
        // no moment with a worker and nothing to cancel (bead
        // ley-line-open-62e3b1). A failed spawn drops `registered`, which
        // removes the entry.
        let (event_tx, event_rx) = mpsc::channel();
        self.starting
            .lock()
            .insert(request.run_id.clone(), event_tx.clone());
        let mut registered = Starting {
            starting: &self.starting,
            run_id: Some(request.run_id.clone()),
            events: event_rx,
            spawned: false,
            run_root: run_root.path().to_path_buf(),
            #[cfg(feature = "test-support")]
            hook: self.hook.clone(),
        };
        #[cfg(feature = "test-support")]
        if let Some(hook) = &self.hook {
            hook(StartPoint::Registered);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return Err(finish_failed_start(
                    run_root,
                    LABELS,
                    ExecutionError::backend(format!("start first-party native worker: {error}")),
                ));
            }
        };
        registered.spawned = true;
        #[cfg(feature = "test-support")]
        if let Some(hook) = &self.hook {
            hook(StartPoint::Spawned);
        }
        // Place the worker before it has its request. It reads the request
        // before it materializes the rootfs, applies nono, or spawns the
        // workload, so nothing the run executes starts outside the cgroup,
        // and every process it spawns inherits it. A write to cgroup.procs
        // here rather than a `pre_exec` hook keeps the `posix_spawn` path
        // `configure_process_group` depends on.
        if let Some(run_cgroup) = &run_cgroup
            && let Err(error) = run_cgroup.place(child.id())
        {
            return Err(abort_failed_start(&mut child, run_root, LABELS, error));
        }
        let Some(mut stdin) = child.stdin.take() else {
            return Err(abort_failed_start(
                &mut child,
                run_root,
                LABELS,
                ExecutionError::backend("native worker stdin was not piped"),
            ));
        };
        if let Err(error) = serde_json::to_writer(&mut stdin, request) {
            return Err(abort_failed_start(
                &mut child,
                run_root,
                LABELS,
                ExecutionError::backend(format!("send native worker request: {error}")),
            ));
        }
        // Terminate the request with a newline. Nothing downstream may depend
        // on stdin reaching EOF to know the request is complete: EOF arrives
        // only when EVERY copy of the pipe's write end is closed, and a
        // process spawned concurrently can inherit one (see `read_request`).
        if let Err(error) = stdin.write_all(b"\n") {
            return Err(abort_failed_start(
                &mut child,
                run_root,
                LABELS,
                ExecutionError::backend(format!("send native worker request: {error}")),
            ));
        }
        if let Err(error) = stdin.flush() {
            return Err(abort_failed_start(
                &mut child,
                run_root,
                LABELS,
                ExecutionError::backend(format!("flush native worker request: {error}")),
            ));
        }
        // Kept: from here on `stdin` is the lifeline's write end.
        let lifeline = stdin;

        let Some(stderr) = child.stderr.take() else {
            return Err(abort_failed_start(
                &mut child,
                run_root,
                LABELS,
                ExecutionError::backend("native worker stderr was not piped"),
            ));
        };
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let result = read_readiness_line(&mut reader);
            let _ = event_tx.send(StartEvent::Readiness(result));
            let _ = std::io::copy(&mut reader, &mut std::io::sink());
        });
        let line = match registered.events.recv_timeout(self.config.ready_timeout) {
            Ok(StartEvent::Cancel(acknowledge)) => {
                return Err(cancel_starting(&mut child, run_root, acknowledge));
            }
            Ok(StartEvent::Readiness(Ok(Some(line)))) => line,
            Ok(StartEvent::Readiness(Ok(None))) => {
                return Err(abort_failed_start(
                    &mut child,
                    run_root,
                    LABELS,
                    ExecutionError::backend("native worker closed stderr before readiness"),
                ));
            }
            Ok(StartEvent::Readiness(Err(error))) => {
                return Err(abort_failed_start(
                    &mut child,
                    run_root,
                    LABELS,
                    ExecutionError::backend(format!("read native worker readiness: {error}")),
                ));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(abort_failed_start(
                    &mut child,
                    run_root,
                    LABELS,
                    ExecutionError::backend("timed out waiting for native worker readiness"),
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(abort_failed_start(
                    &mut child,
                    run_root,
                    LABELS,
                    ExecutionError::backend("native worker readiness channel disconnected"),
                ));
            }
        };
        match serde_json::from_str::<WorkerEvent>(line.trim()) {
            Ok(WorkerEvent::Ready {
                run_id,
                confinement_digest,
            }) if run_id == request.run_id => {
                // ADR-0035: the worker attests the policy it compiled, and a
                // run whose attestation disagrees with the grant never
                // reaches Running. The worker writes readiness immediately
                // before it spawns the workload, so this second check cannot
                // stop a drifting run from starting: the worker's own
                // comparison (`committed_confinement`, before the rootfs is
                // read) is the one that does (bead ley-line-open-7a3109).
                // This one refuses a worker that attests something else.
                if !request.confinement_digest.is_empty()
                    && confinement_digest != request.confinement_digest
                {
                    return Err(abort_failed_start(
                        &mut child,
                        run_root,
                        LABELS,
                        ExecutionError::identity_mismatch(format!(
                            "confinement drift: the worker applied a policy \
                             digesting to {confinement_digest}, but the grant \
                             authorized {}",
                            request.confinement_digest
                        )),
                    ));
                }
            }
            Ok(WorkerEvent::Ready { run_id, .. }) => {
                return Err(abort_failed_start(
                    &mut child,
                    run_root,
                    LABELS,
                    ExecutionError::backend(format!(
                        "native worker readiness named unexpected run {run_id}"
                    )),
                ));
            }
            Ok(WorkerEvent::Failed { error }) => {
                let _ = child.wait();
                return Err(finish_failed_start(run_root, LABELS, error));
            }
            Err(error) => {
                return Err(abort_failed_start(
                    &mut child,
                    run_root,
                    LABELS,
                    ExecutionError::backend(format!(
                        "invalid native worker readiness event: {error}"
                    )),
                ));
            }
        }

        // Every fallible step must happen BEFORE the run is published in
        // `children`. Returning `Err` after the insert but before the
        // supervisor is spawned would strand a started, confined worker with
        // nothing to reap it and wedge the run id permanently, because
        // `wait_for_completion` would block on a condvar nobody can notify.
        let deadline =
            match Instant::now().checked_add(Duration::from_millis(request.limits.wall_time_ms)) {
                Some(deadline) => deadline,
                None => {
                    return Err(abort_failed_start(
                        &mut child,
                        run_root,
                        LABELS,
                        ExecutionError::invalid("wall-clock limit is too large"),
                    ));
                }
            };

        let (cancel_tx, cancel_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let run_id = request.run_id.clone();
        // The run moves from `starting` to `children` under the `starting`
        // lock, which a cancel holds while it looks in both, so no cancel
        // finds it in neither. A cancel that arrived after readiness, before
        // this lock, is waiting in `event_rx`, and is honored here, still
        // under the lock.
        let mut starting = self.starting.lock();
        starting.remove(&run_id);
        registered.run_id = None;
        while let Ok(event) = registered.events.try_recv() {
            if let StartEvent::Cancel(acknowledge) = event {
                return Err(cancel_starting(&mut child, run_root, acknowledge));
            }
        }
        self.children.lock().insert(
            run_id.clone(),
            WorkerControl {
                cancel: cancel_tx,
                finished: finished_rx,
            },
        );
        drop(starting);
        let children = Arc::clone(&self.children);
        let cleanup_errors = Arc::clone(&self.cleanup_errors);
        let completed = Arc::clone(&self.completed);
        let completion_cv = Arc::clone(&self.completion_cv);
        std::thread::spawn(move || {
            // Held until the worker is reaped; this process ending drops it.
            let _lifeline = lifeline;
            let supervised = supervise_worker(
                WorkerProcess::new(child),
                run_root,
                cancel_rx,
                deadline,
                LABELS,
            );
            // After the worker is reaped: `cgroup.kill` takes whatever the
            // group kill missed (a process that called setsid), and the
            // directory goes. A teardown failure is reported, never dropped.
            let removed = run_cgroup.map_or(Ok(()), RunCgroup::remove);
            let result = with_cgroup_cleanup(supervised, removed);
            if let Err(error) = &result {
                cleanup_errors.lock().push(error.clone());
            }
            // Retire the run and publish its completion as one step, under
            // the lock `wait_for_completion` holds while it reads `children`.
            // Retiring it last let a cleanup that followed an observed
            // completion find the run still active, collect this result, and
            // report the worker's own failure as the cleanup's (bead
            // ley-line-open-f8ebcf).
            // A cancelled run publishes nothing: the shared lifecycle records
            // it `Cancelled` (bead ley-line-open-9c6a8f).
            {
                let mut completed = completed.lock();
                children.lock().remove(&run_id);
                if let Some(status) = published_status(&result) {
                    completed.insert(run_id, status);
                }
            }
            completion_cv.notify_all();
            let _ = finished_tx.send(result.map(|_| ()));
        });
        Ok(BackendRun {
            backend_id: "native-nono/1".into(),
            // The native worker inherits its standard streams and nothing
            // else.
            attached: Vec::new(),
        })
    }

    fn poll(&self, run_id: &str) -> Result<Option<BackendRunStatus>, ExecutionError> {
        Ok(self.completed.lock().remove(run_id))
    }

    fn cancel(&self, run_id: &str) -> Result<bool, ExecutionError> {
        NativeWorkerBackend::cancel(self, run_id)
    }
}

/// A run's result once its cgroup is torn down: the supervision outcome, with
/// a cgroup teardown failure reported rather than dropped. When both failed,
/// the supervision error keeps its code and the teardown failure is appended.
fn with_cgroup_cleanup(
    supervised: Result<Supervised, ExecutionError>,
    removed: Result<(), ExecutionError>,
) -> Result<Supervised, ExecutionError> {
    match (supervised, removed) {
        (Ok(ended), removed) => removed.map(|()| ended),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup)) => Err(ExecutionError {
            detail: format!(
                "{}; cgroup cleanup also failed: {}",
                error.detail, cleanup.detail
            ),
            ..error
        }),
    }
}

impl Drop for NativeWorkerBackend {
    fn drop(&mut self) {
        let controls: Vec<_> = self
            .children
            .lock()
            .drain()
            .map(|(_, control)| control)
            .collect();
        for control in &controls {
            let _ = control.cancel.send(());
        }
        for control in controls {
            let _ = control.finished.recv();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{NativeWorkerBackend, NativeWorkerConfig};
    use crate::{Backend, BackendRunStatus};
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn a_cgroup_teardown_failure_is_reported_alongside_a_run_failure() {
        use crate::{ErrorCode, ExecutionError};
        let run = ExecutionError {
            code: ErrorCode::ResourceExhausted,
            retryable: false,
            detail: "execution exceeded wall-clock limit".into(),
        };
        let teardown = ExecutionError::backend("run cgroup still holds processes");
        let both = super::with_cgroup_cleanup(Err(run.clone()), Err(teardown.clone()))
            .expect_err("both failed");
        assert_eq!(
            both.code,
            ErrorCode::ResourceExhausted,
            "the run's code wins"
        );
        assert_eq!(
            both.detail,
            "execution exceeded wall-clock limit; cgroup cleanup also failed: run cgroup still holds processes"
        );
        assert_eq!(
            super::with_cgroup_cleanup(Ok(super::Supervised::Exited), Err(teardown.clone()))
                .expect_err("teardown"),
            teardown
        );
        assert_eq!(
            super::with_cgroup_cleanup(Err(run.clone()), Ok(())).expect_err("run"),
            run
        );
        // The supervision's own ending survives a clean teardown: a cancel
        // stays a cancel.
        for ended in [super::Supervised::Exited, super::Supervised::Cancelled] {
            assert_eq!(super::with_cgroup_cleanup(Ok(ended), Ok(())), Ok(ended));
        }
    }

    #[test]
    fn backend_trait_poll_returns_published_completion() {
        let backend = NativeWorkerBackend::new(NativeWorkerConfig {
            worker: PathBuf::new(),
            cas_root: PathBuf::new(),
            ephemeral_root: PathBuf::new(),
            runtime_files: Vec::new(),
            ready_timeout: Duration::from_secs(1),
        });
        backend
            .completed
            .lock()
            .insert("run-1".into(), BackendRunStatus::Succeeded);
        assert!(matches!(
            Backend::poll(&backend, "run-1").expect("poll"),
            Some(BackendRunStatus::Succeeded)
        ));
    }
}
