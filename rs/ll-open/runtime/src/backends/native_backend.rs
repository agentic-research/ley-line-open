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

use super::native::WorkerEvent;
use super::process::{
    SupervisionLabels, WorkerProcess, abort_failed_start, configure_process_group,
    finish_failed_start, read_readiness_line, supervise_worker,
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

impl NativeWorkerBackend {
    pub fn new(config: NativeWorkerConfig) -> Self {
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

    pub fn cancel(&self, run_id: &str) -> Result<bool, ExecutionError> {
        let Some(control) = self.children.lock().remove(run_id) else {
            return Ok(false);
        };
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
        }
    }

    fn start(&self, request: &ExecutionRequest) -> Result<BackendRun, ExecutionError> {
        request.validate()?;
        if !self.configured() {
            return Err(ExecutionError::backend(
                "native nono worker backend is not configured with existing first-party resources",
            ));
        }
        let _start_guard = self.start_lock.lock();
        if self.children.lock().contains_key(&request.run_id) {
            return Err(ExecutionError {
                code: crate::ErrorCode::ResourceConflict,
                retryable: false,
                detail: format!("run_id already active: {}", request.run_id),
            });
        }

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
        configure_process_group(&mut command);

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
        if let Err(error) = stdin.flush() {
            return Err(abort_failed_start(
                &mut child,
                run_root,
                LABELS,
                ExecutionError::backend(format!("flush native worker request: {error}")),
            ));
        }
        drop(stdin);

        let Some(stderr) = child.stderr.take() else {
            return Err(abort_failed_start(
                &mut child,
                run_root,
                LABELS,
                ExecutionError::backend("native worker stderr was not piped"),
            ));
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
                    run_root,
                    LABELS,
                    ExecutionError::backend("native worker closed stderr before readiness"),
                ));
            }
            Ok(Err(error)) => {
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
            Ok(WorkerEvent::Ready { run_id }) if run_id == request.run_id => {}
            Ok(WorkerEvent::Ready { run_id }) => {
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
                run_root,
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
            backend_id: "native-nono/1".into(),
        })
    }

    fn poll(&self, run_id: &str) -> Result<Option<BackendRunStatus>, ExecutionError> {
        Ok(self.completed.lock().remove(run_id))
    }

    fn cancel(&self, run_id: &str) -> Result<bool, ExecutionError> {
        NativeWorkerBackend::cancel(self, run_id)
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
