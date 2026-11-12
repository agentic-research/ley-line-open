use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use tempfile::Builder as TempDirBuilder;

use crate::{
    AttachedWorkspace, Backend, BackendCapabilities, BackendClass, BackendRun, BackendRunStatus,
    ExecutionError, ExecutionRequest, GuestOutput, ServedWorkspace, ServedWorkspaces,
};

use super::output::OutputCapture;

use super::super::process::{
    SupervisionLabels, WorkerProcess, abort_failed_start, configure_process_group,
    finish_failed_start, published_status, read_readiness_line, supervise_worker,
};
use super::share::{FsService, serve};
// The share's `worker_end` is created close-on-exec, as every descriptor
// here is, so a process another thread spawns concurrently does not inherit
// it; only the worker's child clears the flag, after its fork
// (`inherit_only`), and the parent holds `worker_end` until spawn returns.
use super::worker::{WORKSPACE_SHARE_FD_ARG, WorkerEvent, declared_descriptors, inherit_only};

const LABELS: SupervisionLabels = SupervisionLabels {
    worker: "libkrun worker",
    ephemeral: "ephemeral rootfs volume",
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KrunWorkerConfig {
    pub worker: PathBuf,
    pub cas_root: PathBuf,
    pub ephemeral_root: PathBuf,
    pub runtime_files: Vec<PathBuf>,
    pub devices: Vec<PathBuf>,
    pub ready_timeout: Duration,
    /// Carry guest `AF_INET` sockets over vsock (`KRUN_TSI_HIJACK_INET`) for
    /// every run this backend starts.
    ///
    /// This is the operator surface, and it has to live here rather than only
    /// on the worker's command line: operators do not spawn the worker, this
    /// backend does. A flag the backend never forwards is settable by tests and
    /// by nobody else.
    ///
    /// Default off, and deliberately NOT reachable from an `ExecutionRequest` —
    /// a workload must not be able to widen its own boundary. It exists for the
    /// case where an unmodified guest binds TCP (mache on 7532) and could not
    /// otherwise be reached across a vsock-only boundary; what the host may
    /// then reach is still decided by the port map, not by the guest.
    pub tsi_hijack_inet: bool,
}

pub struct KrunWorkerBackend {
    config: KrunWorkerConfig,
    /// Whose hypervisor-entitlement rule applies to the worker: a
    /// [`std::env::consts::OS`] value, or `None` when the caller vouches for
    /// the worker. Only the `test-support` hooks below change it.
    entitlement_platform: Option<&'static str>,
    /// The free space the run root must have before a run's copy is made
    /// ([`crate::backends::run_root::RUN_ROOT_FREE_FLOOR_BYTES`] unless set).
    free_floor: u64,
    start_lock: Mutex<()>,
    children: Arc<Mutex<HashMap<String, WorkerControl>>>,
    cleanup_errors: Arc<Mutex<Vec<ExecutionError>>>,
    completed: Arc<Mutex<HashMap<String, crate::RunOutcome>>>,
    completion_cv: Arc<Condvar>,
    /// What each active run has printed (bead `ley-line-open-a26ad0`):
    /// published with the run in `children`, retired with it.
    outputs: Arc<Mutex<HashMap<String, Arc<OutputCapture>>>>,
    /// The workspace share each run attached, held from `start` until the
    /// run's outcome is collected or it is cancelled. The share's serving
    /// thread ends with the worker, so without this a worker that exits
    /// right after readiness would leave nothing holding the share, and the
    /// service would refuse a run that in fact started and failed (bead
    /// `ley-line-open-1022a8`).
    shares: Mutex<HashMap<String, Arc<dyn FsService>>>,
}

struct WorkerControl {
    cancel: mpsc::Sender<()>,
    finished: mpsc::Receiver<Result<(), ExecutionError>>,
}

impl KrunWorkerBackend {
    pub fn new(config: KrunWorkerConfig) -> Self {
        Self {
            config,
            entitlement_platform: Some(std::env::consts::OS),
            free_floor: super::super::run_root::RUN_ROOT_FREE_FLOOR_BYTES,
            start_lock: Mutex::new(()),
            children: Arc::new(Mutex::new(HashMap::new())),
            cleanup_errors: Arc::new(Mutex::new(Vec::new())),
            completed: Arc::new(Mutex::new(HashMap::new())),
            completion_cv: Arc::new(Condvar::new()),
            outputs: Arc::new(Mutex::new(HashMap::new())),
            shares: Mutex::new(HashMap::new()),
        }
    }

    /// Wait, at most `timeout`, until what the active run `run_id` has
    /// printed satisfies `until`, and return it: an event-driven way for an
    /// embedder to wait for a served guest to announce itself. Returns the
    /// output seen last when the run's streams end or the timeout passes
    /// first, and an error when `run_id` is not active. `until` runs under
    /// the run's capture lock, so it must not call [`Backend::output`] for
    /// the same run.
    pub fn wait_for_output(
        &self,
        run_id: &str,
        until: &dyn Fn(&GuestOutput) -> bool,
        timeout: Duration,
    ) -> Result<GuestOutput, ExecutionError> {
        let capture =
            self.outputs.lock().get(run_id).cloned().ok_or_else(|| {
                ExecutionError::invalid(format!("run_id is not active: {run_id}"))
            })?;
        Ok(capture.wait(until, Instant::now() + timeout))
    }

    /// The share this tier attaches for the run, taken from the shares the
    /// service materialized (bead `ley-line-open-1022a8`): none, or the one
    /// workspace it mounts as the guest's `/work`. Refused before anything is
    /// spawned when the request names workspaces other than the shares
    /// handed over (the request is what the worker is told), or when there
    /// are more shares than the tier attaches.
    fn workspace_share(
        request: &ExecutionRequest,
        workspaces: ServedWorkspaces,
    ) -> Result<Option<ServedWorkspace>, ExecutionError> {
        if !workspaces.serve_exactly(request) {
            return Err(ExecutionError::workspace_unserved(format!(
                "the request names {} workspace(s) and was handed {}; a workspace is \
                 served only from a share the execution service opened for this run \
                 under its verified grant",
                request.workspaces.len(),
                if workspaces.grants().next().is_some() {
                    "shares that are not exactly this run's"
                } else {
                    "no workspace share"
                }
            )));
        }
        let mut shares = workspaces.into_shares();
        match shares.len() {
            0 => Ok(None),
            1 => Ok(shares.pop()),
            granted => Err(ExecutionError::workspace_unserved(format!(
                "the microVM tier attaches one workspace as /work; the run is granted {granted}"
            ))),
        }
    }

    /// Refuse a run, before anything is created or spawned, when the run
    /// root has fewer than `bytes` free (bead `ley-line-open-dac894`).
    pub fn with_run_root_free_floor(mut self, bytes: u64) -> Self {
        self.free_floor = bytes;
        self
    }

    /// Skip the worker's hypervisor-entitlement check, for a fake worker that
    /// stands in for the VMM in a supervision-contract test. A backend built
    /// this way can report `available` for a worker that cannot start a
    /// guest, so it exists only under the non-default `test-support` feature,
    /// which no shipping build enables (tools/feature-ledger.txt).
    #[cfg(feature = "test-support")]
    pub fn with_unverified_hypervisor_access(mut self) -> Self {
        self.entitlement_platform = None;
        self
    }

    /// Apply `os`'s hypervisor-entitlement rule instead of the host's, so a
    /// test can hold the macOS rule to account on any platform. Like the
    /// opt-out above, `test-support` only.
    #[cfg(feature = "test-support")]
    pub fn with_entitlement_platform(mut self, os: &'static str) -> Self {
        self.entitlement_platform = Some(os);
        self
    }

    fn configured(&self) -> bool {
        self.config.worker.is_file()
            && self.config.cas_root.is_dir()
            && self.config.ephemeral_root.is_dir()
            && self.config.runtime_files.iter().all(|path| path.is_file())
            && self.config.devices.iter().all(|path| path.exists())
    }

    /// `Ok` exactly when a run can start: the resources exist AND the worker
    /// can enter the hypervisor (bead ley-line-open-831bf1, I1-I3). The
    /// entitlement is checked here, at capability negotiation, so a worker
    /// that would die inside `krun_start_enter` is never handed a run.
    ///
    /// Not cached: the signature is read on every call (one `codesign`
    /// spawn on macOS), so a worker re-signed or replaced in place is judged
    /// as it is now, and a guest boot dwarfs the cost.
    fn availability(&self) -> Result<(), String> {
        if !self.configured() {
            return Err(
                "libkrun worker backend is not configured with existing first-party resources"
                    .into(),
            );
        }
        match self.entitlement_platform {
            Some(os) => super::entitlement::hypervisor_entitlement(os, &self.config.worker),
            None => Ok(()),
        }
    }

    /// Stop one backend run and release its parent-owned ephemeral rootfs.
    pub fn cancel(&self, run_id: &str) -> Result<bool, ExecutionError> {
        // A cancelled run is retired whatever state it was in: its share is
        // let go.
        self.shares.lock().remove(run_id);
        let Some(control) = self.children.lock().remove(run_id) else {
            return Ok(false);
        };
        let _ = control.cancel.send(());
        control.finished.recv().map_err(|_| {
            ExecutionError::backend("libkrun worker supervisor stopped before cleanup completed")
        })??;
        // Cancellation is represented by the shared lifecycle, not as a
        // successful backend completion. Drop the supervisor's completion
        // marker so a later poll cannot retain a stale terminal result.
        self.completed.lock().remove(run_id);
        Ok(true)
    }

    /// Drain cleanup failures observed by autonomous worker supervisors.
    pub fn take_cleanup_errors(&self) -> Vec<ExecutionError> {
        std::mem::take(&mut *self.cleanup_errors.lock())
    }

    /// Wait for one known run's terminal backend result. This is an explicit
    /// lifecycle synchronization point; callers must not infer completion by
    /// watching the ephemeral filesystem or sleeping for a guessed duration.
    pub fn wait_for_completion(&self, run_id: &str) -> Result<BackendRunStatus, ExecutionError> {
        Ok(self.wait_for_outcome(run_id)?.status)
    }

    /// [`Self::wait_for_completion`] with the run's guest output alongside
    /// its status: the typed [`crate::RunOutcome`], whose `output` holds the
    /// bounded, sanitized, untrusted tails of a finished or failed run.
    pub fn wait_for_outcome(&self, run_id: &str) -> Result<crate::RunOutcome, ExecutionError> {
        let mut completed = self.completed.lock();
        loop {
            if let Some(outcome) = completed.remove(run_id) {
                self.shares.lock().remove(run_id);
                return Ok(outcome);
            }
            if !self.children.lock().contains_key(run_id) {
                return Err(ExecutionError::invalid(format!(
                    "run_id is not active or pending completion: {run_id}"
                )));
            }
            self.completion_cv.wait(&mut completed);
        }
    }
}

impl Backend for KrunWorkerBackend {
    fn capabilities(&self) -> BackendCapabilities {
        let availability = self.availability();
        BackendCapabilities {
            backend_id: "libkrun/1".into(),
            backend_class: BackendClass::MicroVm,
            available: availability.is_ok(),
            unavailable_reason: availability.err(),
            // `krun_set_vm_config(ctx, vcpus, ram_mib)` applies both at VM
            // configuration time, before the guest runs — see plan.rs, which
            // derives them from `memory.limit` and `cpu.quota / cpu.period`.
            // The wall clock is still the supervisor's. Nothing writes the
            // guest's cgroups (layer 3 is not built), so pids is refused.
            enforced: crate::EnforcedCeilings {
                wall_time: crate::CeilingMechanism::Supervisor,
                output_bytes: crate::CeilingMechanism::Unenforced,
                memory: crate::CeilingMechanism::Hypervisor,
                cpu: crate::CeilingMechanism::Hypervisor,
                pids: crate::CeilingMechanism::Unenforced,
            },
        }
    }

    fn start(
        &self,
        request: &ExecutionRequest,
        workspaces: ServedWorkspaces,
    ) -> Result<BackendRun, ExecutionError> {
        request.validate()?;
        // ADR-0035 §4 at the backend as well as the service, so a direct
        // call cannot route around it.
        self.capabilities().enforced.check(&request.limits)?;
        // The hypervisor sizes the guest from these, so the tier has no
        // "no ceiling" for either: the VM needs a RAM size and a vCPU count.
        if request.limits.memory.limit == 0 || request.limits.cpu.quota == 0 {
            return Err(ExecutionError::invalid(
                "the microVM tier sizes the guest from memory.limit and cpu.quota; a run must grant both",
            ));
        }
        if let Err(reason) = self.availability() {
            return Err(ExecutionError::backend(reason));
        }
        // A run ID is not published in `children` until its worker reports
        // readiness. Serialize that reservation window so concurrent callers
        // cannot both spawn a worker for the same identity.
        let _start_guard = self.start_lock.lock();
        if self.children.lock().contains_key(&request.run_id) {
            return Err(ExecutionError {
                code: crate::ErrorCode::ResourceConflict,
                retryable: false,
                detail: format!("run_id already active: {}", request.run_id),
            });
        }

        super::super::run_root::ensure_free_space(&self.config.ephemeral_root, self.free_floor)?;

        // The share was opened by the service before this call; it is taken
        // here, before anything is spawned or created, so a refusal (shares
        // that are not the request's workspaces, more than the tier attaches)
        // leaves no run root and no worker.
        let (share, attached): (Option<Arc<dyn FsService>>, Option<AttachedWorkspace>) =
            Self::workspace_share(request, workspaces)?
                .map(ServedWorkspace::attach)
                .unzip();
        // Held by the backend for the run's life, beside the serving thread.
        let held_share = share.clone();

        let rootfs = TempDirBuilder::new()
            .prefix(&super::super::run_root::run_dir_prefix("leyline-run-"))
            .tempdir_in(&self.config.ephemeral_root)
            .map_err(|error| {
                ExecutionError::backend(format!("create ephemeral rootfs volume: {error}"))
            })?;

        let mut command = Command::new(&self.config.worker);
        command
            .arg("--cas-root")
            .arg(&self.config.cas_root)
            .arg("--run-root")
            .arg(rootfs.path())
            .stdin(Stdio::piped())
            // The guest's console. Read from spawn on, so a guest that
            // prints never blocks on a full pipe, and kept bounded
            // (`output::OutputCapture`).
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for path in &self.config.runtime_files {
            command.arg("--runtime-file").arg(path);
        }
        for path in &self.config.devices {
            command.arg("--device").arg(path);
        }
        // The operator's opt-in, forwarded. Without this line the worker flag
        // is reachable only by a test that spawns the worker itself, which is
        // not an opt-in at all — it is dead configuration.
        if self.config.tsi_hijack_inet {
            command.arg("--tsi-hijack-inet");
        }
        configure_process_group(&mut command);
        // The workspace share: one end of a connected socket pair is
        // inherited by the worker, whose virtio-fs device sends the guest's
        // requests down it; this process answers them from the store.
        let share = match share {
            Some(service) => match UnixStream::pair() {
                Ok((daemon_end, worker_end)) => {
                    command
                        .arg(WORKSPACE_SHARE_FD_ARG)
                        .arg(worker_end.as_raw_fd().to_string());
                    Some((service, daemon_end, worker_end))
                }
                Err(error) => {
                    return Err(finish_failed_start(
                        rootfs,
                        LABELS,
                        ExecutionError::backend(format!("create workspace share socket: {error}")),
                    ));
                }
            },
            None => None,
        };
        // What the worker must declare at readiness: exactly the share it
        // is handed, under the descriptor number it inherits.
        let declared = declared_descriptors(
            request,
            share
                .as_ref()
                .map(|(_, _, worker_end)| worker_end.as_raw_fd()),
        );
        // The worker inherits its standard streams and the declared share,
        // and nothing else this process holds.
        let keep: Vec<i32> = declared.iter().map(|d| d.descriptor).collect();
        if let Err(error) = inherit_only(&mut command, &keep) {
            return Err(finish_failed_start(rootfs, LABELS, error));
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return Err(finish_failed_start(
                    rootfs,
                    LABELS,
                    ExecutionError::backend(format!("start first-party libkrun worker: {error}")),
                ));
            }
        };
        // What the run prints, from its first byte. Both drains end at the
        // worker's exit, when the process group is killed and its pipes
        // close.
        let capture = Arc::new(OutputCapture::new());
        if let Some(stdout) = child.stdout.take() {
            let capture = Arc::clone(&capture);
            std::thread::spawn(move || capture.drain_guest(stdout));
        }
        // The worker holds its end now; dropping ours makes the worker's
        // exit the share's end of stream, which ends the serving thread.
        if let Some((service, daemon_end, worker_end)) = share {
            drop(worker_end);
            std::thread::spawn(move || {
                let (mut reader, mut writer) = (&daemon_end, &daemon_end);
                // An I/O error means the worker is gone: its device answers
                // EIO from then on, and the run's own exit is supervised.
                let _ = serve(&mut reader, &mut writer, service.as_ref());
            });
        }
        let mut stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                return Err(abort_failed_start(
                    &mut child,
                    rootfs,
                    LABELS,
                    ExecutionError::backend("first-party libkrun worker stdin was not piped"),
                ));
            }
        };
        if let Err(error) = serde_json::to_writer(&mut stdin, request) {
            return Err(abort_failed_start(
                &mut child,
                rootfs,
                LABELS,
                ExecutionError::backend(format!(
                    "send execution request to libkrun worker: {error}"
                )),
            ));
        }
        // Terminate the request with a newline. Nothing downstream may depend
        // on stdin reaching EOF to know the request is complete: EOF arrives
        // only when EVERY copy of the pipe's write end is closed, and a
        // process spawned concurrently can inherit one (see `read_request`).
        if let Err(error) = stdin.write_all(b"\n") {
            return Err(abort_failed_start(
                &mut child,
                rootfs,
                LABELS,
                ExecutionError::backend(format!(
                    "send execution request to libkrun worker: {error}"
                )),
            ));
        }
        if let Err(error) = stdin.flush() {
            return Err(abort_failed_start(
                &mut child,
                rootfs,
                LABELS,
                ExecutionError::backend(format!(
                    "flush execution request to libkrun worker: {error}"
                )),
            ));
        }
        drop(stdin);

        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                return Err(abort_failed_start(
                    &mut child,
                    rootfs,
                    LABELS,
                    ExecutionError::backend("first-party libkrun worker stderr was not piped"),
                ));
            }
        };
        let (event_tx, event_rx) = mpsc::sync_channel(1);
        let worker_capture = Arc::clone(&capture);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let result = read_readiness_line(&mut reader);
            let _ = event_tx.send(result);
            // After readiness: the worker's terminal `failed` event, if it
            // fails, and libkrun's diagnostics, kept bounded. The reader's
            // buffer may already hold some of it.
            worker_capture.drain_worker(reader);
        });

        let line = match event_rx.recv_timeout(self.config.ready_timeout) {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => {
                return Err(abort_failed_start(
                    &mut child,
                    rootfs,
                    LABELS,
                    ExecutionError::backend("libkrun worker closed stderr before readiness"),
                ));
            }
            Ok(Err(error)) => {
                return Err(abort_failed_start(
                    &mut child,
                    rootfs,
                    LABELS,
                    ExecutionError::backend(format!("read libkrun worker readiness: {error}")),
                ));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(abort_failed_start(
                    &mut child,
                    rootfs,
                    LABELS,
                    ExecutionError::backend("timed out waiting for libkrun worker readiness"),
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(abort_failed_start(
                    &mut child,
                    rootfs,
                    LABELS,
                    ExecutionError::backend("libkrun worker readiness channel disconnected"),
                ));
            }
        };
        let event: WorkerEvent = match serde_json::from_str(line.trim()) {
            Ok(event) => event,
            Err(error) => {
                return Err(abort_failed_start(
                    &mut child,
                    rootfs,
                    LABELS,
                    ExecutionError::backend(format!(
                        "invalid libkrun worker readiness event: {error}"
                    )),
                ));
            }
        };
        match event {
            WorkerEvent::Ready {
                run_id,
                confinement_digest,
                inherited,
            } if run_id == request.run_id => {
                // The worker refuses to start holding a descriptor it does
                // not declare; this refuses a declaration other than what
                // this process handed it, so the receipt names the channels
                // the worker actually holds.
                if inherited != declared {
                    return Err(abort_failed_start(
                        &mut child,
                        rootfs,
                        LABELS,
                        ExecutionError::identity_mismatch(format!(
                            "descriptor drift: the worker declares inherited \
                             descriptors {inherited:?}, and was handed {declared:?}"
                        )),
                    ));
                }
                // Same check as the native tier: the worker attests the
                // policy it applied, and a disagreement with the grant is
                // refused. Readiness is written immediately before
                // `krun_start_enter`, so this cannot stop a drifting guest
                // from starting; the worker's own comparison, before the
                // rootfs is read, does (bead ley-line-open-7a3109).
                if !request.confinement_digest.is_empty()
                    && confinement_digest != request.confinement_digest
                {
                    return Err(abort_failed_start(
                        &mut child,
                        rootfs,
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
            WorkerEvent::Ready { run_id, .. } => {
                return Err(abort_failed_start(
                    &mut child,
                    rootfs,
                    LABELS,
                    ExecutionError::backend(format!(
                        "libkrun worker readiness named unexpected run {run_id}"
                    )),
                ));
            }
            WorkerEvent::Failed { error } => {
                let _ = child.wait();
                return Err(finish_failed_start(rootfs, LABELS, error));
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
                        rootfs,
                        LABELS,
                        ExecutionError::invalid("wall-clock limit is too large"),
                    ));
                }
            };

        let (cancel_tx, cancel_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let run_id = request.run_id.clone();
        self.outputs
            .lock()
            .insert(run_id.clone(), Arc::clone(&capture));
        if let Some(share) = held_share {
            self.shares.lock().insert(run_id.clone(), share);
        }
        self.children.lock().insert(
            run_id.clone(),
            WorkerControl {
                cancel: cancel_tx,
                finished: finished_rx,
            },
        );
        let outputs = Arc::clone(&self.outputs);
        let children = Arc::clone(&self.children);
        let cleanup_errors = Arc::clone(&self.cleanup_errors);
        let completed = Arc::clone(&self.completed);
        let completion_cv = Arc::clone(&self.completion_cv);
        std::thread::spawn(move || {
            let result = supervise_worker(
                WorkerProcess::new(child),
                rootfs,
                cancel_rx,
                deadline,
                LABELS,
            );
            if let Err(error) = &result {
                cleanup_errors.lock().push(error.clone());
            }
            // What the guest printed, once its streams have ended, travels
            // with the terminal status as a typed `RunOutcome`. A failure
            // keeps the supervisor's classification and also carries the
            // tails in its detail. The live capture is released.
            let output = capture.settle();
            // A cancelled run publishes nothing: the shared lifecycle records
            // it `Cancelled` (bead ley-line-open-9c6a8f).
            let outcome = published_status(&result).map(|status| crate::RunOutcome {
                status: match status {
                    BackendRunStatus::Failed(error) => {
                        BackendRunStatus::Failed(super::output::failure_with_output(error, &output))
                    }
                    other => other,
                },
                output: Some(output),
            });
            outputs.lock().remove(&run_id);
            // Retire the run and publish its completion as one step, under
            // the lock `wait_for_completion` holds while it reads `children`
            // (the native backend explains the race; bead
            // ley-line-open-f8ebcf).
            {
                let mut completed = completed.lock();
                children.lock().remove(&run_id);
                if let Some(outcome) = outcome {
                    completed.insert(run_id, outcome);
                }
            }
            completion_cv.notify_all();
            let _ = finished_tx.send(result.map(|_| ()));
        });
        Ok(BackendRun {
            backend_id: "libkrun/1".into(),
            // The share taken from the service's hand, which the worker's
            // readiness declaration was held to above.
            attached: attached.into_iter().collect(),
        })
    }

    /// One: the workspace the tier mounts as the guest's `/work`.
    fn workspace_capacity(&self) -> usize {
        1
    }

    fn poll(&self, run_id: &str) -> Result<Option<BackendRunStatus>, ExecutionError> {
        Ok(self.poll_outcome(run_id)?.map(|outcome| outcome.status))
    }

    fn poll_outcome(&self, run_id: &str) -> Result<Option<crate::RunOutcome>, ExecutionError> {
        let outcome = self.completed.lock().remove(run_id);
        if outcome.is_some() {
            // Collected: the run is retired, and its share with it.
            self.shares.lock().remove(run_id);
        }
        Ok(outcome)
    }

    fn output(&self, run_id: &str) -> Option<GuestOutput> {
        let capture = self.outputs.lock().get(run_id).cloned()?;
        Some(capture.snapshot())
    }

    fn cancel(&self, run_id: &str) -> Result<bool, ExecutionError> {
        KrunWorkerBackend::cancel(self, run_id)
    }
}

impl Drop for KrunWorkerBackend {
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
    use super::{KrunWorkerBackend, KrunWorkerConfig};
    use crate::{Backend, BackendRunStatus};
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn backend_trait_poll_returns_published_completion() {
        let backend = KrunWorkerBackend::new(KrunWorkerConfig {
            worker: PathBuf::new(),
            cas_root: PathBuf::new(),
            ephemeral_root: PathBuf::new(),
            runtime_files: Vec::new(),
            devices: Vec::new(),
            ready_timeout: Duration::from_secs(1),
            tsi_hijack_inet: false,
        });
        backend.completed.lock().insert(
            "run-1".into(),
            crate::RunOutcome {
                status: BackendRunStatus::Succeeded,
                output: None,
            },
        );
        assert!(matches!(
            Backend::poll(&backend, "run-1").expect("poll"),
            Some(BackendRunStatus::Succeeded)
        ));
    }

    /// The tier serves exactly one workspace per run, its guest's `/work`:
    /// the service opens a share for a run granted one, and refuses a run
    /// granted two before opening anything. Zero would refuse every
    /// workspace run; two would open a second share the tier then refuses.
    #[test]
    fn the_microvm_tier_serves_one_workspace_per_run() {
        let backend = KrunWorkerBackend::new(KrunWorkerConfig {
            worker: PathBuf::new(),
            cas_root: PathBuf::new(),
            ephemeral_root: PathBuf::new(),
            runtime_files: Vec::new(),
            devices: Vec::new(),
            ready_timeout: Duration::from_secs(1),
            tsi_hijack_inet: false,
        });
        assert_eq!(Backend::workspace_capacity(&backend), 1);
    }

    /// A backend whose fake worker declares the share it was handed, reports
    /// ready for run `run-1`, and then runs `after_ready` (shell). Returns the
    /// backend, the fixture that holds it, and the `run-1` request granted one
    /// read-only workspace with the shares the service would hand over.
    fn share_backend(
        after_ready: &str,
    ) -> (
        KrunWorkerBackend,
        tempfile::TempDir,
        crate::ExecutionRequest,
        crate::ServedWorkspaces,
        crate::workspaces::Opened,
    ) {
        use crate::backends::libkrun::share::{FsService, Reply, Request, WorkspaceShares};
        use std::os::unix::fs::PermissionsExt;

        struct Share;
        impl FsService for Share {
            fn call(&self, _request: Request) -> Reply {
                Reply::Done
            }
        }
        struct Provider;
        impl WorkspaceShares for Provider {
            fn open(
                &self,
                _run_id: &str,
                _grant: &crate::WorkspaceGrant,
            ) -> Result<std::sync::Arc<dyn FsService>, crate::ExecutionError> {
                Ok(std::sync::Arc::new(Share))
            }
        }

        let fixture = tempfile::tempdir().expect("fixture");
        let worker = fixture.path().join("worker");
        std::fs::write(
            &worker,
            format!(
                r#"#!/bin/sh
fd=
prev=
for arg in "$@"; do
  if [ "$prev" = "--workspace-share-fd" ]; then fd="$arg"; fi
  prev="$arg"
done
IFS= read -r _
printf '%s\n' "{{\"type\":\"ready\",\"run_id\":\"run-1\",\"inherited\":[{{\"descriptor\":$fd,\"channel\":{{\"role\":\"workspaceShare\",\"workspace\":\"repo\",\"graphRoot\":\"blake3-256:root\"}}}}]}}" >&2
{after_ready}
"#
            ),
        )
        .expect("worker script");
        std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o755)).expect("mode");
        let (cas_root, ephemeral_root) = (fixture.path().join("cas"), fixture.path().join("runs"));
        std::fs::create_dir(&cas_root).expect("cas");
        std::fs::create_dir(&ephemeral_root).expect("runs");
        let mut backend = KrunWorkerBackend::new(KrunWorkerConfig {
            worker,
            cas_root,
            ephemeral_root,
            runtime_files: Vec::new(),
            devices: Vec::new(),
            ready_timeout: Duration::from_secs(30),
            tsi_hijack_inet: false,
        });
        // A shell script stands in for the VMM.
        backend.entitlement_platform = None;

        let request = crate::ExecutionRequest {
            run_id: "run-1".into(),
            replay_key: "replay-1".into(),
            rootfs: crate::DigestRef {
                algorithm: "blake3-256".into(),
                value: "a".repeat(64),
            },
            executable: "bin/guest".into(),
            arguments: Vec::new(),
            public_environment: Default::default(),
            allowed_egress: Vec::new(),
            confinement_digest: String::new(),
            confinement_manifest: None,
            workspaces: vec![crate::WorkspaceGrant {
                name: "repo".into(),
                graph_root: "blake3-256:root".into(),
                expected_generation: 0,
                operations: vec![crate::WorkspaceOperation::Read],
                write_scope: Vec::new(),
            }],
            limits: crate::ResourceLimits {
                wall_time_ms: 30_000,
                memory: crate::MemoryLimits { limit: 256 << 20 },
                cpu: crate::CpuLimits {
                    quota: 100_000,
                    period: 100_000,
                },
                ..Default::default()
            },
        };
        let (shares, opened) = crate::ServedWorkspaces::materialize(
            Some(&Provider),
            &request.run_id,
            &request.workspaces,
        )
        .expect("open the share");
        (backend, fixture, request, shares, opened)
    }

    /// A worker that reports ready and exits at once (a guest that fails to
    /// enter, say) is a run that failed, not a share nobody served. The
    /// share's serving thread ends with the worker, so the backend itself
    /// holds the share until the run's outcome is collected: the service's
    /// start assertion passes, and the run is recorded and ends `Failed`
    /// with what it printed.
    #[test]
    fn a_worker_that_exits_right_after_ready_still_holds_its_share() {
        let (backend, _fixture, request, shares, opened) =
            share_backend("printf 'entering the guest failed\\n' >&2\nexit 1");
        let started = backend
            .start(&request, shares)
            .expect("the worker was ready");

        // The worker has exited and its completion is published: the
        // serving thread's end of the share is closed.
        {
            let mut completed = backend.completed.lock();
            while !completed.contains_key("run-1") {
                backend.completion_cv.wait(&mut completed);
            }
        }
        opened
            .held(&started.attached)
            .expect("the backend holds the share of a run whose outcome is uncollected");

        let outcome = backend
            .poll_outcome("run-1")
            .expect("poll")
            .expect("the run completed");
        let BackendRunStatus::Failed(error) = outcome.status else {
            panic!(
                "a worker that exits 1 is a failed run: {:?}",
                outcome.status
            );
        };
        assert!(
            error.detail.contains("entering the guest failed"),
            "the failed run keeps what it printed: {error}"
        );
        // Collected: the backend lets the share go.
        assert!(!backend.shares.lock().contains_key("run-1"));
    }

    /// The share is retained for exactly the run's life: kept across a poll
    /// that finds the run still active, let go when the run is cancelled.
    #[test]
    fn a_live_runs_share_is_kept_across_polls_and_released_by_cancel() {
        let (backend, _fixture, request, shares, _opened) =
            share_backend("exec /usr/bin/tail -f /dev/null");
        backend
            .start(&request, shares)
            .expect("the worker was ready");
        assert!(
            backend.poll_outcome("run-1").expect("poll").is_none(),
            "the run is active"
        );
        assert!(
            backend.shares.lock().contains_key("run-1"),
            "a poll that collects nothing keeps the share"
        );
        assert!(backend.cancel("run-1").expect("cancel"));
        assert!(
            !backend.shares.lock().contains_key("run-1"),
            "a cancelled run's share is let go"
        );
    }

    /// Waiting for a run's outcome collects it, and lets its share go.
    #[test]
    fn waiting_for_a_runs_outcome_releases_its_share() {
        let (backend, _fixture, request, shares, _opened) = share_backend("exit 0");
        backend
            .start(&request, shares)
            .expect("the worker was ready");
        let outcome = backend
            .wait_for_outcome("run-1")
            .expect("the run completes");
        assert!(matches!(outcome.status, BackendRunStatus::Succeeded));
        assert!(
            !backend.shares.lock().contains_key("run-1"),
            "a collected run's share is let go"
        );
    }
}
