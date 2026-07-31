use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use parking_lot::Mutex;

use crate::{
    Backend, BackendCapabilities, BackendClass, BackendRun, ExecutionError, ExecutionRequest,
};

use super::worker::WorkerEvent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KrunWorkerConfig {
    pub worker: PathBuf,
    pub cas_root: PathBuf,
    pub libkrun: PathBuf,
    pub runtime_files: Vec<PathBuf>,
    pub devices: Vec<PathBuf>,
    pub ready_timeout: Duration,
}

pub struct KrunWorkerBackend {
    config: KrunWorkerConfig,
    children: Mutex<HashMap<String, Child>>,
}

impl KrunWorkerBackend {
    pub fn new(config: KrunWorkerConfig) -> Self {
        Self {
            config,
            children: Mutex::new(HashMap::new()),
        }
    }

    fn configured(&self) -> bool {
        self.config.worker.is_file()
            && self.config.cas_root.is_dir()
            && self.config.libkrun.is_file()
            && self.config.runtime_files.iter().all(|path| path.is_file())
            && self.config.devices.iter().all(|path| path.exists())
    }

    fn reap_finished(&self) {
        self.children
            .lock()
            .retain(|_, child| matches!(child.try_wait(), Ok(None)));
    }

    fn terminate(child: &mut Child) {
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Backend for KrunWorkerBackend {
    fn capabilities(&self) -> BackendCapabilities {
        self.reap_finished();
        BackendCapabilities {
            backend_id: "libkrun/1".into(),
            backend_class: BackendClass::MicroVm,
            available: self.configured(),
        }
    }

    fn start(&self, request: &ExecutionRequest) -> Result<BackendRun, ExecutionError> {
        self.reap_finished();
        if !self.configured() {
            return Err(ExecutionError::backend(
                "libkrun worker backend is not configured with existing first-party resources",
            ));
        }

        let mut command = Command::new(&self.config.worker);
        command
            .arg("--cas-root")
            .arg(&self.config.cas_root)
            .arg("--libkrun")
            .arg(&self.config.libkrun)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for path in &self.config.runtime_files {
            command.arg("--runtime-file").arg(path);
        }
        for path in &self.config.devices {
            command.arg("--device").arg(path);
        }

        let mut child = command.spawn().map_err(|error| {
            ExecutionError::backend(format!("start first-party libkrun worker: {error}"))
        })?;
        let mut stdin = child.stdin.take().ok_or_else(|| {
            ExecutionError::backend("first-party libkrun worker stdin was not piped")
        })?;
        if let Err(error) = serde_json::to_writer(&mut stdin, request) {
            Self::terminate(&mut child);
            return Err(ExecutionError::backend(format!(
                "send execution request to libkrun worker: {error}"
            )));
        }
        if let Err(error) = stdin.flush() {
            Self::terminate(&mut child);
            return Err(ExecutionError::backend(format!(
                "flush execution request to libkrun worker: {error}"
            )));
        }
        drop(stdin);

        let stderr = child.stderr.take().ok_or_else(|| {
            ExecutionError::backend("first-party libkrun worker stderr was not piped")
        })?;
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
                return Err(ExecutionError::backend(format!(
                    "libkrun worker exited before readiness: {status:?}"
                )));
            }
            Ok(Err(error)) => {
                Self::terminate(&mut child);
                return Err(ExecutionError::backend(format!(
                    "read libkrun worker readiness: {error}"
                )));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Self::terminate(&mut child);
                return Err(ExecutionError::backend(
                    "timed out waiting for libkrun worker readiness",
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Self::terminate(&mut child);
                return Err(ExecutionError::backend(
                    "libkrun worker readiness channel disconnected",
                ));
            }
        };
        let event: WorkerEvent = serde_json::from_str(line.trim()).map_err(|error| {
            Self::terminate(&mut child);
            ExecutionError::backend(format!("invalid libkrun worker readiness event: {error}"))
        })?;
        match event {
            WorkerEvent::Ready { run_id } if run_id == request.run_id => {}
            WorkerEvent::Ready { run_id } => {
                Self::terminate(&mut child);
                return Err(ExecutionError::backend(format!(
                    "libkrun worker readiness named unexpected run {run_id}"
                )));
            }
            WorkerEvent::Failed { error } => {
                let _ = child.wait();
                return Err(error);
            }
        }

        self.children.lock().insert(request.run_id.clone(), child);
        Ok(BackendRun {
            backend_id: "libkrun/1".into(),
        })
    }
}

impl Drop for KrunWorkerBackend {
    fn drop(&mut self) {
        for child in self.children.get_mut().values_mut() {
            Self::terminate(child);
        }
    }
}
