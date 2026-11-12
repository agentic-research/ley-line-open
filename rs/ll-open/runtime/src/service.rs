use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use leyline_core::ContentAddressed;
use parking_lot::RwLock;

use crate::backends::libkrun::share::WorkspaceShares;
use crate::{
    BackendCapabilities, BackendRun, BackendRunStatus, ExecutionError, ExecutionRequest,
    ReceiptContext, RunEventRecord, RunInspection, RunReceiptData, RunRecord, RunState,
    ServedWorkspaces,
    authorization::{AuthorizationPolicy, AuthorizedExecution},
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
                match capabilities.unavailable_reason {
                    Some(reason) => format!("requested execution backend is unavailable: {reason}"),
                    None => "requested execution backend is unavailable".into(),
                },
            ))
        }
    }

    /// Start one already validated execution, with the workspace shares the
    /// service materialized for it (bead `ley-line-open-1022a8`).
    ///
    /// `workspaces` is the run's whole workspace authority: one open share
    /// per granted workspace, or [`ServedWorkspaces::none`]. A backend takes
    /// each share it attaches ([`crate::ServedWorkspace::attach`]) and
    /// returns the proofs in [`BackendRun::attached`]; one that cannot use a
    /// share refuses with `workspaceUnserved` before it spawns anything. It
    /// must also refuse a `request` whose `workspaces` differ from the shares
    /// it is handed ([`ServedWorkspaces::serve_exactly`]), since the request
    /// is what its worker is told.
    fn start(
        &self,
        request: &ExecutionRequest,
        workspaces: ServedWorkspaces,
    ) -> Result<BackendRun, ExecutionError>;

    /// How many workspaces this backend serves one run. The service asks
    /// before it opens any share: a run granted more is refused
    /// `workspaceUnserved` with nothing opened and nothing started. The
    /// default is none, so a backend serves workspaces only by saying so.
    fn workspace_capacity(&self) -> usize {
        0
    }

    /// Observe a worker that may have completed without an explicit cancel.
    /// Backends that cannot report completion leave the result absent; the
    /// service still preserves the explicit-cancel lifecycle for them.
    fn poll(&self, _run_id: &str) -> Result<Option<BackendRunStatus>, ExecutionError> {
        Ok(None)
    }

    /// What an active run's guest has printed so far, bounded, for the
    /// embedder that started it. `None` when the run is not active here, or
    /// when the backend keeps no output (the default). A finished run's
    /// output comes with its status from [`Backend::poll_outcome`].
    fn output(&self, _run_id: &str) -> Option<crate::GuestOutput> {
        None
    }

    /// [`Backend::poll`] with the finished run's guest output alongside its
    /// status, as one typed [`crate::RunOutcome`]. Like `poll`, it consumes
    /// the completion. The default carries no output.
    fn poll_outcome(&self, run_id: &str) -> Result<Option<crate::RunOutcome>, ExecutionError> {
        Ok(self.poll(run_id)?.map(|status| crate::RunOutcome {
            status,
            output: None,
        }))
    }

    /// Cancel one active execution and release backend-owned resources.
    ///
    /// The boolean is false when the backend has no active run with this ID;
    /// transport adapters can project that as an idempotent no-op or a typed
    /// not-found result without reaching into backend internals.
    fn cancel(&self, run_id: &str) -> Result<bool, ExecutionError>;
}

/// A storage refusal by name is the host's (unsupported, not retryable);
/// a failed tool or filesystem call is the backend's.
fn storage_error(error: crate::cas_volume::CasVolumeError) -> ExecutionError {
    use crate::cas_volume::CasVolumeError;
    let detail = format!("provision the CAS root: {error}");
    match error {
        CasVolumeError::UnsupportedHost { .. }
        | CasVolumeError::CaseInsensitive { .. }
        | CasVolumeError::RunRootCaseInsensitive { .. } => ExecutionError::unsupported(detail),
        CasVolumeError::Config(_)
        | CasVolumeError::Tool { .. }
        | CasVolumeError::Timeout { .. }
        | CasVolumeError::ForeignVolume { .. }
        | CasVolumeError::Io { .. } => ExecutionError::backend(detail),
    }
}

#[derive(Default)]
struct ServiceState {
    runs: HashMap<String, RunRecord>,
    replay: HashMap<String, String>,
    /// Run identities reserved while the backend performs startup.  Backend
    /// startup may materialize a rootfs or wait for worker readiness, so it
    /// must not happen while the lifecycle state lock is held.  Reservations
    /// preserve run-id/replay-key uniqueness during that lock-free interval.
    starting: HashMap<String, String>,
    events: HashMap<String, Vec<RunEventRecord>>,
    receipts: HashMap<String, ReceiptContext>,
    /// The channels each run's worker held beyond its confinement manifest,
    /// as its backend verified them at start; the receipt's backend
    /// evidence names them.
    channels: HashMap<String, Vec<crate::WorkerChannel>>,
    provisioned: bool,
    provisioned_backend: Option<String>,
    /// The CAS root the last successful provision made exist.
    provisioned_cas_root: Option<std::path::PathBuf>,
}

/// One lifecycle implementation used by every transport adapter.
pub struct ExecutionService<B> {
    backend: B,
    /// Where the backend's CAS root comes from. `provision` makes it exist
    /// before the backend checks its readiness (bead `ley-line-open-ba86d3`).
    storage: Option<crate::cas_volume::CasStorage>,
    /// Where a granted workspace is served from. `None`: a run granted a
    /// workspace is refused before the backend is called.
    shares: Option<std::sync::Arc<dyn WorkspaceShares>>,
    state: RwLock<ServiceState>,
}

impl<B: Backend> ExecutionService<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            storage: None,
            shares: None,
            state: RwLock::new(ServiceState::default()),
        }
    }

    /// Serve each authorized run's granted workspaces from `shares`
    /// (ADR-0044 V5). The service opens them before it calls the backend and
    /// hands them over as [`ServedWorkspaces`] (bead `ley-line-open-1022a8`).
    pub fn with_workspace_shares(mut self, shares: std::sync::Arc<dyn WorkspaceShares>) -> Self {
        self.shares = Some(shares);
        self
    }

    /// The CAS root `provision` makes exist and reports. It must be the
    /// directory the backend was configured to read rootfs trees from.
    pub fn with_cas_storage(mut self, storage: crate::cas_volume::CasStorage) -> Self {
        self.storage = Some(storage);
        self
    }

    /// The CAS root the last successful `provision` made exist; `None`
    /// before one, or without [`with_cas_storage`](Self::with_cas_storage).
    pub fn provisioned_cas_root(&self) -> Option<std::path::PathBuf> {
        self.state.read().provisioned_cas_root.clone()
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
        // A request refused for its class changes nothing: checked before
        // storage, which may create and attach a volume.
        if self.backend.capabilities().backend_class != backend_class {
            return Err(ExecutionError::unsupported(
                "requested backend class is unavailable",
            ));
        }
        // Storage next, outside the state lock (creating and attaching a
        // volume takes seconds): a backend checks its CAS root exists.
        let cas_root = match &self.storage {
            Some(storage) => Some(storage.provision().map_err(storage_error)?),
            None => None,
        };
        let capabilities = self.backend.provision()?;
        if capabilities.backend_class != backend_class {
            return Err(ExecutionError::unsupported(
                "requested backend class is unavailable",
            ));
        }
        let mut state = self.state.write();
        state.provisioned = true;
        state.provisioned_backend = Some(capabilities.backend_id.clone());
        state.provisioned_cas_root = cas_root;
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

    /// Start a request no grant was verified for. Such a run holds no
    /// workspace: workspace shares are materialized only for a verified grant
    /// ([`Self::start_authorized_with_verifier`]), so a request naming
    /// workspaces here is refused before anything is reserved or started.
    pub fn start(&self, request: ExecutionRequest) -> Result<RunRecord, ExecutionError> {
        if let Some(grant) = request.workspaces.first() {
            return Err(ExecutionError::workspace_unserved(format!(
                "the request names workspace {:?} without a verified grant; a workspace \
                 share is opened only for a run started through `start_authorized`",
                grant.name
            )));
        }
        self.start_run(request, false)
    }

    /// Refuse, before any share is opened, a run granted more workspaces
    /// than the backend serves ([`Backend::workspace_capacity`]).
    fn workspaces_within_capacity(&self, request: &ExecutionRequest) -> Result<(), ExecutionError> {
        let capacity = self.backend.workspace_capacity();
        let Some(grant) = request.workspaces.get(capacity) else {
            return Ok(());
        };
        Err(ExecutionError::workspace_unserved(format!(
            "the run is granted {} workspace(s), and backend {} serves {capacity} per \
             run; workspace {:?} cannot be served, so no share is opened and nothing \
             is started",
            request.workspaces.len(),
            self.backend.capabilities().backend_id,
            grant.name
        )))
    }

    /// `verified`: the request came from a verified grant, so its workspaces
    /// are materialized. Only `start_authorized_with_verifier` passes true.
    fn start_run(
        &self,
        request: ExecutionRequest,
        verified: bool,
    ) -> Result<RunRecord, ExecutionError> {
        request.validate()?;
        // ADR-0035 §4: a ceiling the selected tier cannot enforce is a
        // rejection, not a no-op. This sits on `start` rather than on
        // `start_authorized` so the direct-request path cannot route around
        // it — otherwise the check would guard the schema entry point and
        // leave the one an embedder calls wide open.
        self.backend
            .capabilities()
            .enforced
            .check(&request.limits)?;

        {
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

            if state
                .starting
                .values()
                .any(|replay| replay == &request.replay_key)
            {
                return Err(ExecutionError {
                    code: crate::ErrorCode::ResourceConflict,
                    retryable: true,
                    detail: format!("replay key is already starting: {}", request.replay_key),
                });
            }
            if state.runs.contains_key(&request.run_id) {
                return Err(ExecutionError {
                    code: crate::ErrorCode::ResourceConflict,
                    retryable: false,
                    detail: format!("run_id already exists: {}", request.run_id),
                });
            }
            if state.starting.contains_key(&request.run_id) {
                return Err(ExecutionError {
                    code: crate::ErrorCode::ResourceConflict,
                    retryable: true,
                    detail: format!("run_id is already starting: {}", request.run_id),
                });
            }
            state
                .starting
                .insert(request.run_id.clone(), request.replay_key.clone());
        }

        // Rootfs materialization and worker readiness are deliberately outside
        // the service lock.  The reservation above keeps identity uniqueness
        // intact while allowing status/inspect/cancel for other runs to make
        // progress.
        //
        // The run's workspace shares are opened here, by the service, before
        // the backend is asked to start anything (bead ley-line-open-1022a8):
        // a grant nothing serves, or a store that does not hold the granted
        // graph root, refuses the start with no worker in existence. The
        // backend is handed the open shares and can start the run only with
        // them.
        //
        // The backend is asked first whether it serves that many workspaces
        // at all, so a tier that serves none is refused with no share opened
        // for it.
        let workspaces = if verified {
            self.workspaces_within_capacity(&request).and_then(|()| {
                ServedWorkspaces::materialize(
                    self.shares.as_deref(),
                    &request.run_id,
                    &request.workspaces,
                )
            })
        } else {
            // `start` refused a request naming workspaces already.
            ServedWorkspaces::materialize(None, &request.run_id, &[])
        };
        let started = workspaces.and_then(|(workspaces, opened)| {
            Ok((self.backend.start(&request, workspaces)?, opened))
        });
        let (started, opened) = match started {
            Ok(started) => started,
            Err(error) => {
                self.state.write().starting.remove(&request.run_id);
                return Err(error);
            }
        };
        let channels: Vec<crate::WorkerChannel> = started
            .attached
            .iter()
            .map(crate::AttachedWorkspace::channel)
            .collect();
        // An assertion, not a comparison of two reports. A backend cannot
        // forge an attached share; `Opened::held` holds what it returned to
        // the shares the service opened for THIS run (each proof names the
        // run and the whole grant, so a share kept from another run, or from
        // a wider grant of the same workspace, is refused) and to each share
        // still being held. What it catches is a backend that answered `Ok`
        // having dropped or swapped a share, which started the workload
        // without its workspace (bead ley-line-open-204a94). No first-party
        // tier can: each attaches what it is handed or refuses before spawn.
        // For one that does, the run is cancelled and never recorded, so it
        // cannot complete, be collected, or claim the workspace's graph root
        // as an input.
        if let Err(error) = opened.held(&started.attached) {
            // The refusal is the answer either way; a cancel that also fails
            // is named in it rather than masking it.
            let cancel = self.backend.cancel(&request.run_id);
            let mut state = self.state.write();
            state.starting.remove(&request.run_id);
            let Err(cancel) = cancel else {
                return Err(error);
            };
            // The workload may still be running. Forgetting it would leave a
            // live run nothing can address and free its run_id for a second
            // start. It is recorded `Failed` instead: terminal, so it is never
            // polled or receipted (it has no receipt context), its run_id
            // refuses another start, and `cleanup` retries the cancel.
            let error = ExecutionError {
                detail: format!(
                    "{}; cancelling the run also failed, so it is recorded failed \
                     and `cleanup` retries the cancel: {cancel}",
                    error.detail
                ),
                ..error
            };
            let detail_digest = serde_json::to_vec(&error)
                .ok()
                .map(|bytes| format!("blake3-256:{}", bytes.hash()));
            state.runs.insert(
                request.run_id.clone(),
                RunRecord {
                    run_id: request.run_id.clone(),
                    replay_key: request.replay_key.clone(),
                    state: RunState::Failed,
                    backend_id: started.backend_id,
                },
            );
            state.events.insert(
                request.run_id,
                vec![
                    event(1, RunState::Accepted),
                    event(2, RunState::Provisioning),
                    event(3, RunState::Ready),
                    event(4, RunState::Running),
                    event_with_detail(5, RunState::Failed, detail_digest),
                ],
            );
            return Err(error);
        }
        let record = RunRecord {
            run_id: request.run_id.clone(),
            replay_key: request.replay_key.clone(),
            state: RunState::Running,
            backend_id: started.backend_id,
        };
        let mut state = self.state.write();
        state.starting.remove(&request.run_id);
        state
            .replay
            .insert(request.replay_key, request.run_id.clone());
        state.channels.insert(request.run_id.clone(), channels);
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
        self.start_authorized_with_verifier(
            spec_bytes,
            grant_bytes,
            policy,
            resolver,
            &crate::authorization::MetadataOnlyEvidenceVerifier,
        )
    }

    /// Production entry point: the embedding authority supplies the verifier
    /// for Signet/NotMe/Interlace evidence. The compatibility method above is
    /// retained for existing local fixtures and must not be used as a trust
    /// boundary.
    pub fn start_authorized_with_verifier<R: ExecutionResolver>(
        &self,
        spec_bytes: &[u8],
        grant_bytes: &[u8],
        policy: &AuthorizationPolicy,
        resolver: &R,
        verifier: &dyn crate::authorization::EvidenceVerifier,
    ) -> Result<RunRecord, ExecutionError> {
        if !self.is_provisioned() {
            return Err(ExecutionError::not_provisioned(
                "execution backend must be explicitly provisioned before start",
            ));
        }
        let authorized = crate::authorization::authorize_with_verifier(
            spec_bytes,
            grant_bytes,
            policy,
            verifier,
        )?;
        // ADR-0046 S3: a grant may now carry verified connections, but this
        // daemon has no connection broker yet (bead ley-line-open-52b5b3), so
        // a declared connection would never be served. Refuse by name before
        // the resolver or the backend runs, rather than start a run whose
        // connections silently do not exist. The daemon does not advertise
        // `cloister/execution/v1/connections` until it can serve them.
        if authorized.connections.is_some() {
            return Err(ExecutionError::unsupported(
                "RunGrant carries connections (cloister/connections/v1), but this daemon has no connection broker",
            ));
        }
        let request = resolver.resolve(&authorized)?;
        if request.run_id != authorized.run_id {
            return Err(ExecutionError::identity_mismatch(
                "resolver returned a run_id different from the authorized identity",
            ));
        }
        if request.replay_key != authorized.replay_key {
            return Err(ExecutionError::identity_mismatch(
                "resolver returned a replay key different from the authorized grant",
            ));
        }
        if request.allowed_egress != authorized.allowed_egress {
            return Err(ExecutionError::identity_mismatch(
                "resolver changed the grant's allowed egress",
            ));
        }
        // The fourth check, and the one whose absence failed OPEN rather than
        // closed. Both backends gate the ADR-0035 drift comparison behind
        // `if !request.confinement_digest.is_empty()`, so a resolver that
        // merely DROPPED this field silently disabled the attestation for that
        // run — no error, no receipt anomaly, nothing to notice.
        //
        // `catalog.rs` already says the digest is "carried, never chosen", and
        // the shipped resolver obeys. But `ExecutionResolver` is a public
        // trait: the rule was prose on one implementation rather than a check
        // on the seam. This repo's own `TestResolver` writes `String::new()`
        // here, which is why the entrypoint test passed vacuously.
        if request.confinement_digest != authorized.confinement_digest {
            return Err(ExecutionError::identity_mismatch(
                "resolver changed the grant's confinement digest",
            ));
        }
        // The document the digest was taken over is held to the same rule
        // (bead ley-line-open-7a3109): a resolver that dropped or swapped it
        // would have the worker fold a policy the issuer never signed, and
        // only a digest difference would catch that, after a worker started.
        if request.confinement_manifest != authorized.confinement_manifest {
            return Err(ExecutionError::identity_mismatch(
                "resolver changed the grant's carried confinement document",
            ));
        }
        // The fifth: a resolver that dropped a workspace grant would run the
        // workload without the share it was granted, and one that widened
        // its operations would let it propose where it may only read.
        if request.workspaces != authorized.workspaces {
            return Err(ExecutionError::identity_mismatch(
                "resolver changed the grant's workspaces",
            ));
        }
        // `inputRoots` is "Content roots materialized for the run": the
        // rootfs the resolver chose, then each granted workspace. The rootfs
        // is read from the request the backend is handed (both first-party
        // backends verify the materialized tree against this digest before
        // the workload runs), not from anything the caller asked for.
        let rootfs = format!("{}:{}", request.rootfs.algorithm, request.rootfs.value);
        let record = self.start_run(request, true)?;
        // A replayed start answers an existing run, possibly under other
        // authority (another spec, grant or input roots sharing its replay
        // key). That run's receipt stays the one it was started under: it is
        // recorded once, never rewritten (bead ley-line-open-799fbc).
        self.state
            .write()
            .receipts
            .entry(record.run_id.clone())
            .or_insert(ReceiptContext {
                run_spec_digest: authorized.spec_digest,
                run_grant_digest: authorized.grant_digest,
                confinement_digest: authorized.confinement_digest,
                backend_class: authorized.backend,
                input_roots: std::iter::once(rootfs)
                    .chain(
                        authorized
                            .intent
                            .workspace_inputs
                            .into_iter()
                            .map(|workspace| workspace.graph_root),
                    )
                    .collect(),
            });
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
            backend_evidence: crate::BackendEvidenceDocument {
                backend_id: record.backend_id.clone(),
                channels: state.channels.get(run_id).cloned().ok_or_else(|| {
                    ExecutionError::internal("run has no recorded worker channels")
                })?,
            },
            enforced: self.backend.capabilities().enforced,
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
        // Project any backend completion first, exactly as every other read
        // path does. Without this, a worker that finished between the
        // caller's decision and its request surfaces as a hard backend error
        // ("no longer owns the active run") while `cleanup` on the same run
        // succeeds — a benign race reported as a failure.
        self.refresh_run(run_id)?;
        let existing = self
            .state
            .read()
            .runs
            .get(run_id)
            .cloned()
            .ok_or_else(|| ExecutionError::invalid("run_id not found"))?;

        // Cancellation asks for "this run is not executing". A run that
        // already reached a terminal state satisfies that, so cancelling it
        // is an idempotent no-op rather than an error.
        if matches!(
            existing.state,
            RunState::Cancelled | RunState::Succeeded | RunState::Failed
        ) {
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
        let (terminal, detail_digest) = match outcome {
            BackendRunStatus::Succeeded => (RunState::Succeeded, None),
            BackendRunStatus::Failed(error) => (
                RunState::Failed,
                Some(format!(
                    "blake3-256:{}",
                    serde_json::to_vec(&error)
                        .map_err(|encode| {
                            ExecutionError::internal(format!(
                                "encode backend failure detail: {encode}"
                            ))
                        })?
                        .hash()
                )),
            ),
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
            .push(event_with_detail(sequence, terminal, detail_digest));
        Ok(())
    }
}

fn event(sequence: u64, state: RunState) -> RunEventRecord {
    event_with_detail(sequence, state, None)
}

fn event_with_detail(
    sequence: u64,
    state: RunState,
    detail_digest: Option<String>,
) -> RunEventRecord {
    RunEventRecord {
        sequence,
        state,
        timestamp_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        detail_digest,
    }
}
