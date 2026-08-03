use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use leyline_core::ContentAddressed;
use parking_lot::RwLock;

use crate::{
    BackendCapabilities, BackendRun, BackendRunStatus, ExecutionError, ExecutionRequest,
    ReceiptContext, RunEventRecord, RunInspection, RunReceiptData, RunRecord, RunState,
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

    /// Explicitly provision backend-owned resources. The default is a
    /// read-only readiness check for backends whose resources are supplied by
    /// the host/container and do not need a separate mount operation.
    fn provision(&self) -> Result<BackendCapabilities, ExecutionError> {
        let capabilities = self.capabilities();
        if capabilities.available {
            Ok(capabilities)
        } else {
            Err(ExecutionError::unsupported(
                "requested execution backend is unavailable",
            ))
        }
    }

    /// Start one already validated execution.
    fn start(&self, request: &ExecutionRequest) -> Result<BackendRun, ExecutionError>;

    /// Observe a worker that may have completed without an explicit cancel.
    /// Backends that cannot report completion leave the result absent; the
    /// service still preserves the explicit-cancel lifecycle for them.
    fn poll(&self, _run_id: &str) -> Result<Option<BackendRunStatus>, ExecutionError> {
        Ok(None)
    }

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
    events: HashMap<String, Vec<RunEventRecord>>,
    receipts: HashMap<String, ReceiptContext>,
    provisioned: bool,
    provisioned_backend: Option<String>,
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

    pub fn provision(
        &self,
        backend_class: crate::BackendClass,
        idempotency_key: &str,
    ) -> Result<BackendCapabilities, ExecutionError> {
        if idempotency_key.is_empty() {
            return Err(ExecutionError::invalid(
                "provision idempotency key must not be empty",
            ));
        }
        let capabilities = self.backend.provision()?;
        if capabilities.backend_class != backend_class {
            return Err(ExecutionError::unsupported(
                "requested backend class is unavailable",
            ));
        }
        let mut state = self.state.write();
        state.provisioned = true;
        state.provisioned_backend = Some(capabilities.backend_id.clone());
        Ok(capabilities)
    }

    pub fn is_provisioned(&self) -> bool {
        self.state.read().provisioned
    }

    pub fn status(&self, run_id: &str) -> Result<Option<RunRecord>, ExecutionError> {
        self.refresh_run(run_id)?;
        Ok(self.state.read().runs.get(run_id).cloned())
    }

    /// Read the current run state and ordered events after a cursor.
    pub fn inspect(
        &self,
        run_id: &str,
        after_sequence: u64,
    ) -> Result<RunInspection, ExecutionError> {
        self.refresh_run(run_id)?;
        let state = self.state.read();
        let record = state
            .runs
            .get(run_id)
            .ok_or_else(|| ExecutionError::invalid("run_id not found"))?;
        let events = state
            .events
            .get(run_id)
            .into_iter()
            .flatten()
            .filter(|event| event.sequence > after_sequence)
            .cloned()
            .collect();
        Ok(RunInspection {
            run_id: record.run_id.clone(),
            state: record.state,
            events,
        })
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
        let run_id = record.run_id.clone();
        state.events.insert(
            run_id,
            vec![
                event(1, RunState::Accepted),
                event(2, RunState::Provisioning),
                event(3, RunState::Ready),
                event(4, RunState::Running),
            ],
        );
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
        if !self.is_provisioned() {
            return Err(ExecutionError::unsupported(
                "execution backend must be explicitly provisioned before start",
            ));
        }
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
        let record = self.start(request)?;
        self.state.write().receipts.insert(
            record.run_id.clone(),
            ReceiptContext {
                run_spec_digest: authorized.spec_digest,
                run_grant_digest: authorized.grant_digest,
                confinement_digest: authorized.confinement_digest,
                backend_class: authorized.backend,
                input_roots: authorized
                    .intent
                    .workspace_inputs
                    .into_iter()
                    .map(|workspace| workspace.graph_root)
                    .collect(),
            },
        );
        Ok(record)
    }

    /// Collect terminal evidence for one run. Collection is read-only; the
    /// caller must explicitly request cleanup afterwards.
    pub fn collect(&self, run_id: &str) -> Result<RunReceiptData, ExecutionError> {
        self.refresh_run(run_id)?;
        let state = self.state.read();
        let record = state
            .runs
            .get(run_id)
            .ok_or_else(|| ExecutionError::invalid("run_id not found"))?;
        if !matches!(
            record.state,
            RunState::Cancelled | RunState::Succeeded | RunState::Failed
        ) {
            return Err(ExecutionError::invalid(
                "run is not terminal; cancel or await completion before collect",
            ));
        }
        let events = state.events.get(run_id).cloned().unwrap_or_default();
        let event_bytes = serde_json::to_vec(&events)
            .map_err(|error| ExecutionError::internal(format!("encode event log: {error}")))?;
        let context = state
            .receipts
            .get(run_id)
            .cloned()
            .ok_or_else(|| ExecutionError::invalid("run has no schema receipt context"))?;
        let started_at_unix_ms = events.first().map_or(0, |event| event.timestamp_ms);
        let completed_at_unix_ms = events
            .last()
            .map_or(started_at_unix_ms, |event| event.timestamp_ms);
        Ok(RunReceiptData {
            run_id: record.run_id.clone(),
            terminal_state: record.state,
            event_log_root: format!("blake3-256:{}", event_bytes.hash()),
            backend_id: record.backend_id.clone(),
            context,
            started_at_unix_ms,
            completed_at_unix_ms,
        })
    }

    /// Idempotently release backend-owned resources and mark the lifecycle
    /// cleaned. A worker that already exited is a successful cleanup result.
    pub fn cleanup(&self, run_id: &str) -> Result<RunRecord, ExecutionError> {
        self.refresh_run(run_id)?;
        let existing = self
            .state
            .read()
            .runs
            .get(run_id)
            .cloned()
            .ok_or_else(|| ExecutionError::invalid("run_id not found"))?;
        if existing.state == RunState::Cleaned {
            return Ok(existing);
        }
        let _ = self.backend.cancel(run_id)?;
        let mut state = self.state.write();
        let record = {
            let record = state
                .runs
                .get_mut(run_id)
                .ok_or_else(|| ExecutionError::internal("run disappeared during cleanup"))?;
            record.state = RunState::Cleaned;
            record.clone()
        };
        let events = state.events.entry(run_id.to_owned()).or_default();
        let next = events.len() as u64 + 1;
        events.push(event(next, RunState::Cleaning));
        events.push(event(next + 1, RunState::Cleaned));
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
        let record = {
            let record = state
                .runs
                .get_mut(run_id)
                .ok_or_else(|| ExecutionError::internal("run disappeared during cancellation"))?;
            record.state = RunState::Cancelled;
            record.clone()
        };
        let sequence = state
            .events
            .get(run_id)
            .map_or(1, |events| events.len() as u64 + 1);
        state
            .events
            .entry(run_id.to_owned())
            .or_default()
            .push(event(sequence, RunState::Cancelled));
        Ok(record)
    }
}

impl<B: Backend> ExecutionService<B> {
    /// Project one backend completion into the shared append-only lifecycle.
    /// This is deliberately called by read paths, so UDS, CLI, and MCP all
    /// observe the same terminal transition without a transport-specific
    /// watcher or arbitrary polling sleeps.
    fn refresh_run(&self, run_id: &str) -> Result<(), ExecutionError> {
        let active = self.state.read().runs.get(run_id).is_some_and(|record| {
            matches!(
                record.state,
                RunState::Accepted | RunState::Provisioning | RunState::Ready | RunState::Running
            )
        });
        if !active {
            return Ok(());
        }
        let Some(outcome) = self.backend.poll(run_id)? else {
            return Ok(());
        };
        let terminal = match outcome {
            BackendRunStatus::Succeeded => RunState::Succeeded,
            BackendRunStatus::Failed(_) => RunState::Failed,
        };
        let mut state = self.state.write();
        let record = state
            .runs
            .get_mut(run_id)
            .ok_or_else(|| ExecutionError::internal("run disappeared during completion refresh"))?;
        if !matches!(
            record.state,
            RunState::Accepted | RunState::Provisioning | RunState::Ready | RunState::Running
        ) {
            return Ok(());
        }
        record.state = terminal;
        let sequence = state
            .events
            .get(run_id)
            .map_or(1, |events| events.len() as u64 + 1);
        state
            .events
            .entry(run_id.to_owned())
            .or_default()
            .push(event(sequence, terminal));
        Ok(())
    }
}

fn event(sequence: u64, state: RunState) -> RunEventRecord {
    RunEventRecord {
        sequence,
        state,
        timestamp_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
    }
}
