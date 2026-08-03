use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use tempfile::{Builder as TempDirBuilder, TempDir};

use crate::{
    Backend, BackendCapabilities, BackendClass, BackendRun, BackendRunStatus, ExecutionError,
    ExecutionRequest,
};

use super::worker::WorkerEvent;

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
        configure_worker_process_group(&mut command);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return Err(finish_failed_start(
                    rootfs,
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
                    ExecutionError::backend("first-party libkrun worker stdin was not piped"),
                ));
            }
        };
        if let Err(error) = serde_json::to_writer(&mut stdin, request) {
            return Err(abort_failed_start(
                &mut child,
                rootfs,
                ExecutionError::backend(format!(
                    "send execution request to libkrun worker: {error}"
                )),
            ));
        }
        if let Err(error) = stdin.flush() {
            return Err(abort_failed_start(
                &mut child,
                rootfs,
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
                    ExecutionError::backend("first-party libkrun worker stderr was not piped"),
                ));
            }
        };
        let (event_tx, event_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            let result = reader
                .read_line(&mut line)
                .map(|bytes| if bytes == 0 { None } else { Some(line) })
                .map_err(|error| error.to_string());
            let _ = event_tx.send(result);
            let _ = std::io::copy(&mut reader, &mut std::io::sink());
        });

        let line = match event_rx.recv_timeout(self.config.ready_timeout) {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => {
                let status = child.wait().ok();
                return Err(finish_failed_start(
                    rootfs,
                    ExecutionError::backend(format!(
                        "libkrun worker exited before readiness: {status:?}"
                    )),
                ));
            }
            Ok(Err(error)) => {
                return Err(abort_failed_start(
                    &mut child,
                    rootfs,
                    ExecutionError::backend(format!("read libkrun worker readiness: {error}")),
                ));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(abort_failed_start(
                    &mut child,
                    rootfs,
                    ExecutionError::backend("timed out waiting for libkrun worker readiness"),
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(abort_failed_start(
                    &mut child,
                    rootfs,
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
                    ExecutionError::backend(format!(
                        "libkrun worker readiness named unexpected run {run_id}"
                    )),
                ));
            }
            WorkerEvent::Failed { error } => {
                let _ = child.wait();
                return Err(finish_failed_start(rootfs, error));
            }
        }

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
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(request.limits.wall_time_ms))
            .ok_or_else(|| ExecutionError::invalid("wall-clock limit is too large"))?;
        std::thread::spawn(move || {
            let result = supervise_worker(child, rootfs, cancel_rx, deadline);
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

fn supervise_worker(
    mut child: Child,
    rootfs: TempDir,
    cancel: mpsc::Receiver<()>,
    deadline: Instant,
) -> Result<(), ExecutionError> {
    // Wait for child exit and cancellation as events. The deadline is the
    // only timed wait; there is no 10ms child-state polling loop.
    enum Event {
        Exited(std::io::Result<std::process::ExitStatus>),
        Cancelled,
    }
    let pid = child.id();
    let (event_tx, event_rx) = mpsc::sync_channel(2);
    let exit_tx = event_tx.clone();
    std::thread::spawn(move || {
        let _ = exit_tx.send(Event::Exited(child.wait()));
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
                terminate_worker(pid);
                let _ = event_rx.recv();
                return cleanup_with_error(
                    rootfs,
                    ExecutionError {
                        code: crate::ErrorCode::ResourceExhausted,
                        retryable: false,
                        detail: "execution exceeded wall-clock limit".into(),
                    },
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                terminate_worker(pid);
                let _ = event_rx.recv();
                return cleanup_with_error(
                    rootfs,
                    ExecutionError::backend("libkrun worker supervision channel disconnected"),
                );
            }
        },
        None => {
            terminate_worker(pid);
            let _ = event_rx.recv();
            return cleanup_with_error(
                rootfs,
                ExecutionError {
                    code: crate::ErrorCode::ResourceExhausted,
                    retryable: false,
                    detail: "execution exceeded wall-clock limit".into(),
                },
            );
        }
    };

    match event {
        Event::Cancelled => {
            terminate_worker(pid);
            let _ = event_rx.recv();
            cleanup_tempdir(rootfs)
        }
        Event::Exited(Ok(status)) if status.success() => cleanup_tempdir(rootfs),
        Event::Exited(Ok(status)) => cleanup_with_error(
            rootfs,
            ExecutionError::backend(format!("libkrun worker exited with {status}")),
        ),
        Event::Exited(Err(error)) => cleanup_with_error(
            rootfs,
            ExecutionError::backend(format!("wait for libkrun worker: {error}")),
        ),
    }
}

fn terminate_worker(pid: u32) {
    // SAFETY: `pid` came directly from the live `Child` immediately before it
    // was moved to the waiter thread; sending SIGKILL does not dereference it
    // or create an alias to process memory.
    #[cfg(unix)]
    unsafe {
        let process_group = -(pid as libc::pid_t);
        if libc::kill(process_group, libc::SIGKILL) == -1 {
            let _ = libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
    }
}

fn configure_worker_process_group(command: &mut Command) {
    // SAFETY: `pre_exec` runs in the child after fork and before exec. The
    // callback performs only the async-signal-safe `setpgid` syscall and
    // allocates no Rust state.
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

fn cleanup_with_error(rootfs: TempDir, error: ExecutionError) -> Result<(), ExecutionError> {
    match cleanup_tempdir(rootfs) {
        Ok(()) => Err(error),
        Err(cleanup_error) => Err(ExecutionError {
            detail: format!(
                "{}; cleanup also failed: {}",
                error.detail, cleanup_error.detail
            ),
            ..error
        }),
    }
}

fn cleanup_tempdir(rootfs: TempDir) -> Result<(), ExecutionError> {
    make_tree_removable(rootfs.path())?;
    rootfs.close().map_err(|error| {
        ExecutionError::backend(format!("remove ephemeral rootfs volume: {error}"))
    })
}

fn abort_failed_start(child: &mut Child, rootfs: TempDir, error: ExecutionError) -> ExecutionError {
    terminate_worker(child.id());
    let _ = child.wait();
    finish_failed_start(rootfs, error)
}

fn finish_failed_start(rootfs: TempDir, error: ExecutionError) -> ExecutionError {
    match cleanup_tempdir(rootfs) {
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

fn make_tree_removable(path: &Path) -> Result<(), ExecutionError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        ExecutionError::backend(format!("inspect ephemeral rootfs cleanup path: {error}"))
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(());
    }

    let mut permissions = metadata.permissions();
    permissions.set_mode(permissions.mode() | 0o700);
    fs::set_permissions(path, permissions).map_err(|error| {
        ExecutionError::backend(format!(
            "restore ephemeral rootfs cleanup permissions: {error}"
        ))
    })?;
    for entry in fs::read_dir(path).map_err(|error| {
        ExecutionError::backend(format!("enumerate ephemeral rootfs for cleanup: {error}"))
    })? {
        let entry = entry.map_err(|error| {
            ExecutionError::backend(format!("enumerate ephemeral rootfs cleanup entry: {error}"))
        })?;
        make_tree_removable(&entry.path())?;
    }
    Ok(())
}
