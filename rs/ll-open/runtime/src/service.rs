use std::collections::HashMap;

use parking_lot::RwLock;

use crate::{
    BackendCapabilities, BackendRun, ExecutionError, ExecutionRequest, RunRecord, RunState,
};

/// Backend boundary shared by native and microVM implementations.
pub trait Backend: Send + Sync + 'static {
    /// A read-only availability/capability snapshot.
    fn capabilities(&self) -> BackendCapabilities;

    /// Start one already validated execution.
    fn start(&self, request: &ExecutionRequest) -> Result<BackendRun, ExecutionError>;

    /// Cancel one active execution and release backend-owned resources.
    ///
    /// The boolean is false when the backend has no active run with this ID;
    /// transport adapters can project that as an idempotent no-op or a typed
    /// not-found result without reaching into backend internals.
    fn cancel(&self, run_id: &str) -> Result<bool, ExecutionError>;
}

#[derive(Default)]
struct ServiceState {
    runs: HashMap<String, RunRecord>,
    replay: HashMap<String, String>,
}

/// One lifecycle implementation used by every transport adapter.
pub struct ExecutionService<B> {
    backend: B,
    state: RwLock<ServiceState>,
}

impl<B: Backend> ExecutionService<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            state: RwLock::new(ServiceState::default()),
        }
    }

    pub fn capabilities(&self) -> BackendCapabilities {
        self.backend.capabilities()
    }

    pub fn status(&self, run_id: &str) -> Result<Option<RunRecord>, ExecutionError> {
        Ok(self.state.read().runs.get(run_id).cloned())
    }

    pub fn start(&self, request: ExecutionRequest) -> Result<RunRecord, ExecutionError> {
        request.validate()?;

        let mut state = self.state.write();
        if let Some(run_id) = state.replay.get(&request.replay_key) {
            return state
                .runs
                .get(run_id)
                .cloned()
                .ok_or_else(|| ExecutionError {
                    code: crate::ErrorCode::Internal,
                    retryable: false,
                    detail: "replay index references a missing run".into(),
                });
        }

        if state.runs.contains_key(&request.run_id) {
            return Err(ExecutionError {
                code: crate::ErrorCode::ResourceConflict,
                retryable: false,
                detail: format!("run_id already exists: {}", request.run_id),
            });
        }

        let started = self.backend.start(&request)?;
        let record = RunRecord {
            run_id: request.run_id.clone(),
            replay_key: request.replay_key.clone(),
            state: RunState::Running,
            backend_id: started.backend_id,
        };
        state
            .replay
            .insert(request.replay_key, request.run_id.clone());
        state.runs.insert(request.run_id, record.clone());
        Ok(record)
    }

    /// Cancel one active run and return its terminal record.
    pub fn cancel(&self, run_id: &str) -> Result<RunRecord, ExecutionError> {
        let existing = self
            .state
            .read()
            .runs
            .get(run_id)
            .cloned()
            .ok_or_else(|| ExecutionError::invalid("run_id not found"))?;

        if existing.state == RunState::Cancelled {
            return Ok(existing);
        }
        if !self.backend.cancel(run_id)? {
            return Err(ExecutionError::backend(
                "backend no longer owns the active run",
            ));
        }

        let mut state = self.state.write();
        let record = state
            .runs
            .get_mut(run_id)
            .ok_or_else(|| ExecutionError::internal("run disappeared during cancellation"))?;
        record.state = RunState::Cancelled;
        Ok(record.clone())
    }
}
