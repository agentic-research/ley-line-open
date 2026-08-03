use std::collections::HashMap;

use parking_lot::RwLock;

use crate::{
    BackendCapabilities, BackendRun, ExecutionError, ExecutionRequest, RunRecord, RunState,
    authorization::{AuthorizationPolicy, AuthorizedExecution, authorize},
};

/// Trusted boundary that resolves schema-level logical identities into the
/// private guest-relative request understood by a backend.
///
/// Implementations own CAS/Graph lookup and may introduce host paths only
/// internally. Callers must not supply a host path through this trait; the
/// resolver receives the already-authorized, owned intent extracted from the
/// generated execution/v1 schema.
pub trait ExecutionResolver: Send + Sync + 'static {
    fn resolve(&self, authorized: &AuthorizedExecution)
    -> Result<ExecutionRequest, ExecutionError>;
}

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

    /// Authorize a generated execution/v1 spec and grant, resolve their
    /// logical identities, then enter the same lifecycle as every other
    /// transport. This is the only schema-to-backend entry point; UDS, CLI,
    /// and MCP adapters should all call it instead of reimplementing policy.
    pub fn start_authorized<R: ExecutionResolver>(
        &self,
        spec_bytes: &[u8],
        grant_bytes: &[u8],
        policy: &AuthorizationPolicy,
        resolver: &R,
    ) -> Result<RunRecord, ExecutionError> {
        let authorized = authorize(spec_bytes, grant_bytes, policy)?;
        let request = resolver.resolve(&authorized)?;
        if request.run_id != authorized.run_id {
            return Err(ExecutionError::invalid(
                "resolver returned a run_id different from the authorized identity",
            ));
        }
        if request.replay_key != authorized.replay_key {
            return Err(ExecutionError::invalid(
                "resolver returned a replay key different from the authorized grant",
            ));
        }
        if request.allowed_egress != authorized.allowed_egress {
            return Err(ExecutionError::invalid(
                "resolver changed the grant's allowed egress",
            ));
        }
        self.start(request)
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
