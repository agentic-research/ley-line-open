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

use super::super::process::{
    SupervisionLabels, WorkerProcess, abort_failed_start, configure_process_group,
    finish_failed_start, read_readiness_line, supervise_worker,
};
use super::worker::WorkerEvent;

const LABELS: SupervisionLabels = SupervisionLabels {
    worker: "libkrun worker",
    ephemeral: "ephemeral rootfs volume",
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KrunWorkerConfig {
    pub worker: PathBuf,
    pub cas_root: PathBuf,
    pub ephemeral_root: PathBuf,
    pub libkrun: PathBuf,
    pub runtime_files: Vec<PathBuf>,
    pub devices: Vec<PathBuf>,
    pub ready_timeout: Duration,
}

pub struct KrunWorkerBackend {
    config: KrunWorkerConfig,
    start_lock: Mutex<()>,
    children: Arc<Mutex<HashMap<String, WorkerControl>>>,
    cleanup_errors: Arc<Mutex<Vec<ExecutionError>>>,
    completed: Arc<Mutex<HashMap<String, BackendRunStatus>>>,
    completion_cv: Arc<Condvar>,
}

struct WorkerControl {
    cancel: mpsc::Sender<()>,
    finished: mpsc::Receiver<Result<(), ExecutionError>>,
}

impl KrunWorkerBackend {
    pub fn new(config: KrunWorkerConfig) -> Self {
        Self {
            config,
            start_lock: Mutex::new(()),
            children: Arc::new(Mutex::new(HashMap::new())),
            cleanup_errors: Arc::new(Mutex::new(Vec::new())),
            completed: Arc::new(Mutex::new(HashMap::new())),
            completion_cv: Arc::new(Condvar::new()),
        }
    }

    fn configured(&self) -> bool {
        self.config.worker.is_file()
            && self.config.cas_root.is_dir()
            && self.config.ephemeral_root.is_dir()
            && self.config.libkrun.is_file()
            && self.config.runtime_files.iter().all(|path| path.is_file())
            && self.config.devices.iter().all(|path| path.exists())
    }

    /// Stop one backend run and release its parent-owned ephemeral rootfs.
    pub fn cancel(&self, run_id: &str) -> Result<bool, ExecutionError> {
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
}

impl Backend for KrunWorkerBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            backend_id: "libkrun/1".into(),
            backend_class: BackendClass::MicroVm,
            available: self.configured(),
        }
    }

    fn start(&self, request: &ExecutionRequest) -> Result<BackendRun, ExecutionError> {
        request.validate()?;
        if !self.configured() {
            return Err(ExecutionError::backend(
                "libkrun worker backend is not configured with existing first-party resources",
            ));
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

        let rootfs = TempDirBuilder::new()
            .prefix("leyline-run-")
            .tempdir_in(&self.config.ephemeral_root)
            .map_err(|error| {
                ExecutionError::backend(format!("create ephemeral rootfs volume: {error}"))
            })?;

        let mut command = Command::new(&self.config.worker);
        command
            .arg("--cas-root")
            .arg(&self.config.cas_root)
            .arg("--libkrun")
            .arg(&self.config.libkrun)
            .arg("--run-root")
            .arg(rootfs.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for path in &self.config.runtime_files {
            command.arg("--runtime-file").arg(path);
        }
        for path in &self.config.devices {
            command.arg("--device").arg(path);
        }
        configure_process_group(&mut command);

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
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let result = read_readiness_line(&mut reader);
            let _ = event_tx.send(result);
            let _ = std::io::copy(&mut reader, &mut std::io::sink());
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
            WorkerEvent::Ready { run_id } if run_id == request.run_id => {}
            WorkerEvent::Ready { run_id } => {
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
        self.children.lock().insert(
            run_id.clone(),
            WorkerControl {
                cancel: cancel_tx,
                finished: finished_rx,
            },
        );
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
            completed.lock().insert(
                run_id.clone(),
                match result.clone() {
                    Ok(()) => BackendRunStatus::Succeeded,
                    Err(error) => BackendRunStatus::Failed(error),
                },
            );
            completion_cv.notify_all();
            let _ = finished_tx.send(result);
            children.lock().remove(&run_id);
        });
        Ok(BackendRun {
            backend_id: "libkrun/1".into(),
        })
    }

    fn poll(&self, run_id: &str) -> Result<Option<BackendRunStatus>, ExecutionError> {
        Ok(self.completed.lock().remove(run_id))
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
            libkrun: PathBuf::new(),
            runtime_files: Vec::new(),
            devices: Vec::new(),
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
