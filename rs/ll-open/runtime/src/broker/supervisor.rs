//! The broker's place in a run's lifecycle (ADR-0046 Slice 1, S2, S6, S7, S8;
//! bead `ley-line-open-5308b7`).
//!
//! [`Brokered`] wraps a [`Backend`] and is itself one. It is the single
//! implementation of the start order, the checks before anything is spawned,
//! teardown and receipt assembly. [`crate::ExecutionService`] is built on it,
//! so no service start routes around it, and an embedder that calls
//! [`Backend::start`] with no service wraps its backend in it the same way.
//!
//! Start order, each step refusing the run before the next (S7):
//!
//! 1. (the caller) authorize the grant;
//! 2. the tier check (S6), the delivery check (S2, `127.0.0.1` only), the
//!    cross-document rule against the run's confinement manifest (S2), the
//!    endpoint checks (S4), and registration in the endpoint registry;
//! 3. spawn the broker, and record the run as starting with it before the
//!    ready wait, so a cancel reaches it;
//! 4. send the document and wait a bound for a ready report whose digest is
//!    the document's;
//! 5. start the workload through the wrapped backend, which names its worker
//!    to the broker on the microVM tier.
//!
//! Any failure after step 3 kills the broker, unlinks its endpoints and
//! removes the run from the registry before `start` returns. One exception:
//! when the workload could not be cancelled it may still be alive, so the
//! registry entry stays until a later cancel succeeds.
//!
//! At every terminal state the broker is drained before it is killed, so a
//! connection open for the run's whole life is in the receipt. A broker that
//! exits under a live run fails the run (D7 rule 5): the workload is
//! cancelled and the outcome carries `complete: false`.

use std::collections::HashMap;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Weak};
use std::time::Duration;

use leyline_schema_spec::connections::ConnectionsDocument;
use parking_lot::{Condvar, Mutex};

use super::control::{BrokerProcess, summarize};
use super::registry::{EndpointRegistry, SocketGrant};
use super::relay::PeerPid;
use crate::confinement::{ConfinementManifest, FsGrant};
use crate::connections::{ServedConnections, State, first_name};
use crate::{
    Backend, BackendCapabilities, BackendClass, BackendRun, BackendRunStatus,
    ConnectionsCapability, ConnectionsOutcome, ErrorCode, ExecutionError, ExecutionRequest,
    RunConnections, RunOutcome, ServedWorkspaces, VerifiedConnections,
};

/// How long the broker has to report ready after it is sent the document
/// (S7 step 4).
pub const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the broker has to report every open connection at teardown (S7).
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

/// Where the broker binary is and how long the supervisor waits on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerConfig {
    /// `leyline-conn-broker`.
    pub binary: PathBuf,
    pub ready_timeout: Duration,
    pub drain_timeout: Duration,
    /// Skip the tier check (`test-support` only).
    unverified_delivery: bool,
}

impl BrokerConfig {
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            ready_timeout: READY_TIMEOUT,
            drain_timeout: DRAIN_TIMEOUT,
            unverified_delivery: false,
        }
    }

    /// Treat this host as a delivering tier, for a test that drives the
    /// lifecycle with the unconfined test broker where the shipped one
    /// refuses (Linux). `test-support` only: no shipping build can skip the
    /// tier check (tools/feature-ledger.txt).
    #[cfg(feature = "test-support")]
    pub fn with_unverified_delivery(mut self) -> Self {
        self.unverified_delivery = true;
        self
    }
}

/// Whether slice 1 delivers connections on `class` under `os` (S6): the
/// microVM tier and the native tier on macOS. Elsewhere the run is refused by
/// name before anything is spawned.
pub fn tier_delivers(class: BackendClass, os: &str) -> Result<(), String> {
    let tier = match class {
        BackendClass::MicroVm => "microVM",
        BackendClass::Native => "native",
    };
    if os == "macos" {
        return Ok(());
    }
    Err(match class {
        BackendClass::MicroVm => format!(
            "the {tier} tier on {os} does not deliver connections: a §6 guestPort is \
             presented on macOS only"
        ),
        BackendClass::Native => format!(
            "the {tier} tier on {os} does not deliver connections: it cannot mediate a \
             §6 UNIX socket grant"
        ),
    })
}

/// The cross-document rule (S2): the connections document against the run's
/// confinement manifest. Each refusal names the connection and the grant.
///
/// - every endpoint has exactly one §6 connect grant on that exact path, in
///   the tier's spelling: the object form carrying `guestPort` on the microVM
///   tier, the bare string on the native tier;
/// - no §6 grant that can bind (`bind`, `connect-bind`) names an endpoint or
///   is a directory grant containing one;
/// - no §2 `fs.allow` grant with write access covers an endpoint's parent.
///
/// A §6 connect grant that names no endpoint is legal: it is a peer channel
/// the workload's own peer binds. Whether it reaches another run's endpoint
/// is the registry's to refuse.
pub fn cross_document(
    class: BackendClass,
    document: &ConnectionsDocument,
    manifest: &ConfinementManifest,
) -> Result<(), ExecutionError> {
    for connection in document.connections() {
        let endpoint = connection.endpoint_unix.as_str();
        let refuse = |reason: String| {
            ExecutionError::invalid_grant(format!(
                "connection `{}`: endpoint {endpoint}: {reason}",
                connection.name
            ))
        };
        let connects: Vec<_> = manifest
            .unix_sockets()
            .iter()
            .filter(|grant| grant.path() == endpoint && !grant.permits_bind())
            .collect();
        for grant in manifest.unix_sockets() {
            if grant.permits_bind()
                && SocketGrant(grant.path().to_owned()).covers(Path::new(endpoint))
            {
                return Err(refuse(format!(
                    "the confinement manifest's §6 {} grant {} lets the workload bind the \
                     broker's socket",
                    grant.mode(),
                    grant.path()
                )));
            }
        }
        match connects.as_slice() {
            [] => {
                return Err(refuse(
                    "the confinement manifest has no §6 connect grant on that path".into(),
                ));
            }
            [grant] => match (class, grant.guest_port()) {
                (BackendClass::MicroVm, None) => {
                    return Err(refuse(
                        "its §6 connect grant carries no guestPort, and the microVM tier \
                         presents a connection on the guest's loopback"
                            .into(),
                    ));
                }
                (BackendClass::Native, Some(port)) => {
                    return Err(refuse(format!(
                        "its §6 connect grant carries guestPort {port}, and the native tier \
                         has no guest loopback; the grant is the bare path there"
                    )));
                }
                _ => {}
            },
            more => {
                return Err(refuse(format!(
                    "the confinement manifest grants §6 connect on that path {} times; \
                     one connection has one grant",
                    more.len()
                )));
            }
        }
        let parent = Path::new(endpoint).parent().unwrap_or(Path::new("/"));
        let parent = super::registry::resolved(parent);
        for grant in manifest.fs_grants() {
            let FsGrant::ReadWrite { path } = grant else {
                continue;
            };
            let granted = super::registry::resolved(Path::new(path));
            if Path::new(&parent).starts_with(Path::new(&granted)) {
                return Err(refuse(format!(
                    "the confinement manifest's §2 read-write grant {path} covers the \
                     endpoint's directory, so the workload could replace the broker's socket"
                )));
            }
        }
    }
    Ok(())
}

/// Every §6 path a manifest names, as the registry compares them.
fn socket_grants(manifest: &ConfinementManifest) -> Vec<SocketGrant> {
    manifest
        .unix_sockets()
        .iter()
        .map(|grant| SocketGrant(grant.path().to_owned()))
        .collect()
}

fn cancelled_during_start() -> ExecutionError {
    ExecutionError {
        code: ErrorCode::Cancelled,
        retryable: false,
        detail: "the run was cancelled while it was starting; its connection broker is gone \
                 and its endpoints are unlinked"
            .into(),
    }
}

/// One run's broker, live.
struct Live {
    broker: Arc<Mutex<BrokerProcess>>,
    connections: VerifiedConnections,
}

enum Entry {
    /// From registration until the run is published or its start has ended.
    /// `broker` is the broker's pid once it is spawned: it stays unreaped
    /// while the entry is in this state, so a cancel may signal it.
    Starting {
        broker: Option<u32>,
        cancelled: bool,
    },
    /// A start that failed, a finish or a broker failure is tearing down.
    Ending,
    Live(Live),
    /// The broker is gone. `failure` is the run's failure when its broker
    /// ended under it, until a poll reports it; `outcome` waits to be taken.
    Ended {
        outcome: Option<ConnectionsOutcome>,
        failure: Option<ExecutionError>,
    },
}

struct Shared {
    config: Option<BrokerConfig>,
    registry: EndpointRegistry,
    runs: Mutex<HashMap<String, Entry>>,
    changed: Condvar,
}

/// A backend with its runs' connections served around it. See the module
/// documentation.
pub struct Brokered<B> {
    inner: Arc<B>,
    shared: Arc<Shared>,
}

impl<B: Backend> Brokered<B> {
    /// Serve connections for `backend`'s runs with the broker `config` names.
    pub fn new(backend: B, config: BrokerConfig) -> Self {
        Self::build(Arc::new(backend), Some(config))
    }

    /// `backend` with no broker: a run that declares connections is refused
    /// by name, and no capability is reported.
    pub fn without_broker(backend: B) -> Self {
        Self::build(Arc::new(backend), None)
    }

    /// The same backend, serving connections with the broker `config` names.
    pub fn with_broker(self, config: BrokerConfig) -> Self {
        Self::build(self.inner, Some(config))
    }

    fn build(inner: Arc<B>, config: Option<BrokerConfig>) -> Self {
        Self {
            inner,
            shared: Arc::new(Shared {
                config,
                registry: EndpointRegistry::new(),
                runs: Mutex::new(HashMap::new()),
                changed: Condvar::new(),
            }),
        }
    }

    /// The wrapped backend, for what the [`Backend`] trait does not carry.
    /// A run's end must still be observed through this value's `poll`,
    /// `poll_outcome` or `cancel`: that is where its broker is drained.
    pub fn inner(&self) -> &B {
        &self.inner
    }

    /// Take what a finished run's connections carried. `None` for a run
    /// without connections, one still live, or one already taken.
    pub fn take_connections(&self, run_id: &str) -> Option<ConnectionsOutcome> {
        let mut runs = self.shared.runs.lock();
        match runs.get(run_id) {
            Some(Entry::Ended { failure: None, .. }) => match runs.remove(run_id) {
                Some(Entry::Ended { outcome, .. }) => outcome,
                _ => None,
            },
            _ => None,
        }
    }

    /// The pid of a live run's broker (`test-support` only).
    #[cfg(feature = "test-support")]
    pub fn broker_pid(&self, run_id: &str) -> Option<u32> {
        match self.shared.runs.lock().get(run_id) {
            Some(Entry::Live(live)) => Some(live.broker.lock().pid()),
            _ => None,
        }
    }

    /// Steps 3 to 5. `slot` holds the broker from its spawn, so the caller
    /// tears it down on any error.
    fn start_brokered(
        &self,
        config: &BrokerConfig,
        request: &ExecutionRequest,
        workspaces: ServedWorkspaces,
        connections: &VerifiedConnections,
        class: BackendClass,
        slot: &mut Option<Arc<Mutex<BrokerProcess>>>,
    ) -> Result<BackendRun, StartFailure> {
        let run_id = request.run_id.as_str();
        // On the microVM tier the one peer is the worker, which does not
        // exist yet: the broker refuses every peer until it is named.
        let awaits_worker = class == BackendClass::MicroVm;
        let peer = if awaits_worker {
            PeerPid::Awaiting
        } else {
            PeerPid::Any
        };
        let on_end = {
            let shared = Arc::downgrade(&self.shared);
            let inner = Arc::downgrade(&self.inner);
            let run_id = run_id.to_owned();
            move || broker_ended(&shared, &inner, &run_id)
        };
        let broker = BrokerProcess::spawn_watched(&config.binary, peer, on_end)?;
        let pid = broker.pid();
        let broker = Arc::new(Mutex::new(broker));
        *slot = Some(Arc::clone(&broker));
        // Step 3: recorded as starting with its broker before the ready wait.
        match self.shared.runs.lock().get_mut(run_id) {
            Some(Entry::Starting {
                cancelled: true, ..
            }) => return Err(cancelled_during_start().into()),
            Some(Entry::Starting {
                broker: spawned, ..
            }) => *spawned = Some(pid),
            _ => {
                return Err(
                    ExecutionError::internal("a starting run lost its broker entry").into(),
                );
            }
        }
        // Step 4.
        broker.lock().start(
            connections.document(),
            connections.digest(),
            config.ready_timeout,
        )?;
        // Step 5.
        let named = Arc::new(AtomicBool::new(false));
        let served = ServedConnections {
            run_id: run_id.to_owned(),
            digest: connections.digest().to_owned(),
            broker: Arc::clone(&broker),
            awaits_worker,
            named: Arc::clone(&named),
        };
        let run = self
            .inner
            .start(request, workspaces, RunConnections::served(served))?;
        // A started workload from here on: a refusal cancels it.
        let refusal = if awaits_worker && !named.load(std::sync::atomic::Ordering::SeqCst) {
            Some(ExecutionError::backend(format!(
                "backend {} started the run without naming its worker to the connection \
                 broker, which would refuse every connection",
                run.backend_id
            )))
        } else {
            let mut runs = self.shared.runs.lock();
            match runs.get(run_id) {
                Some(Entry::Starting {
                    cancelled: true, ..
                }) => Some(cancelled_during_start()),
                // A broker that ended while the workload started serves
                // nothing. Checked under the lock the end hook takes, so a
                // broker that ends after this is the hook's to fail.
                _ if broker.lock().exited() => Some(ExecutionError::backend(
                    "the run's connection broker exited while its workload was starting",
                )),
                _ => {
                    runs.insert(
                        run_id.to_owned(),
                        Entry::Live(Live {
                            broker: Arc::clone(&broker),
                            connections: connections.clone(),
                        }),
                    );
                    None
                }
            }
        };
        match refusal {
            None => Ok(run),
            Some(error) => Err(match self.inner.cancel(run_id) {
                Ok(_) => error.into(),
                // The workload may still be running: its endpoints stay
                // reserved until a cancel succeeds (S7).
                Err(cancel) => StartFailure {
                    error: ExecutionError {
                        detail: format!(
                            "{}; cancelling the workload also failed, so its endpoints stay \
                             reserved until a cancel succeeds: {}",
                            error.detail, cancel.detail
                        ),
                        ..error
                    },
                    workload_may_live: true,
                },
            }),
        }
    }

    /// Tear down a start that failed after registration.
    fn abandon(
        &self,
        run_id: &str,
        connections: &VerifiedConnections,
        broker: Option<Arc<Mutex<BrokerProcess>>>,
        keep_registered: bool,
    ) {
        // No cancel signals the broker from here: it is about to be reaped.
        self.shared
            .runs
            .lock()
            .insert(run_id.to_owned(), Entry::Ending);
        if let Some(broker) = broker {
            let mut broker = broker.lock();
            let ready = broker.reported_ready();
            broker.shutdown();
            if !ready {
                unlink_unreported(connections.document());
            }
        }
        if !keep_registered {
            self.shared.registry.remove(run_id);
        }
        self.shared.runs.lock().remove(run_id);
        self.shared.changed.notify_all();
    }

    /// Drain and stop a live run's broker and keep its outcome. Returns the
    /// run's failure when its broker had ended under it. `keep_registered`
    /// leaves the registry entry for a run whose workload may still be alive.
    fn finish(&self, run_id: &str, keep_registered: bool) -> Option<ExecutionError> {
        let live = {
            let mut runs = self.shared.runs.lock();
            loop {
                match runs.get_mut(run_id) {
                    Some(Entry::Ending) => self.shared.changed.wait(&mut runs),
                    Some(Entry::Live(_)) => {
                        let Some(Entry::Live(live)) = runs.insert(run_id.to_owned(), Entry::Ending)
                        else {
                            unreachable!("matched a live entry under the lock");
                        };
                        break live;
                    }
                    Some(Entry::Ended { failure, .. }) => {
                        let failure = failure.take();
                        if !keep_registered {
                            self.shared.registry.remove(run_id);
                        }
                        return failure;
                    }
                    // A start owns its own registration until it publishes.
                    Some(Entry::Starting { .. }) => return None,
                    // A run without connections: only its §6 grants are
                    // registered. Removed under the lock a start registers
                    // under, so a start of the same run id that begins now
                    // keeps the registration it makes.
                    None => {
                        if !keep_registered {
                            self.shared.registry.remove(run_id);
                        }
                        return None;
                    }
                }
            }
        };
        let drain = self
            .shared
            .config
            .as_ref()
            .map_or(DRAIN_TIMEOUT, |config| config.drain_timeout);
        let outcome = end_broker(&live, Some(drain));
        if !keep_registered {
            self.shared.registry.remove(run_id);
        }
        self.shared.runs.lock().insert(
            run_id.to_owned(),
            Entry::Ended {
                outcome: Some(outcome),
                failure: None,
            },
        );
        self.shared.changed.notify_all();
        None
    }

    /// A live run whose broker has exited is failed here too, so a poll never
    /// reports such a run as running or succeeded.
    fn fail_if_broker_ended(&self, run_id: &str) {
        let ended = match self.shared.runs.lock().get(run_id) {
            Some(Entry::Live(live)) => live.broker.lock().exited(),
            _ => false,
        };
        if ended {
            broker_ended(
                &Arc::downgrade(&self.shared),
                &Arc::downgrade(&self.inner),
                run_id,
            );
        }
    }

    /// The failure a poll reports for a run whose broker ended under it.
    fn pending_failure(&self, run_id: &str) -> Option<ExecutionError> {
        let mut runs = self.shared.runs.lock();
        loop {
            match runs.get_mut(run_id) {
                Some(Entry::Ending) => self.shared.changed.wait(&mut runs),
                Some(Entry::Ended { failure, .. }) => return failure.take(),
                _ => return None,
            }
        }
    }
}

/// Why steps 3 to 5 failed. `workload_may_live` is the failed-cancel path
/// (S7): the workload started and could not be cancelled, so the caller keeps
/// the run's registry entry.
struct StartFailure {
    error: ExecutionError,
    workload_may_live: bool,
}

impl From<ExecutionError> for StartFailure {
    fn from(error: ExecutionError) -> Self {
        Self {
            error,
            workload_may_live: false,
        }
    }
}

/// Stop one broker and summarize what it reported. `drain` asks it first for
/// a final record of every open connection; without it (the broker is
/// already gone) every count is a lower bound and `complete` is false.
fn end_broker(live: &Live, drain: Option<Duration>) -> ConnectionsOutcome {
    let mut broker = live.broker.lock();
    let complete = drain.is_some_and(|bound| broker.drain(bound));
    let connections = summarize(live.connections.document(), broker.events(), complete);
    broker.shutdown();
    ConnectionsOutcome {
        digest: live.connections.digest().to_owned(),
        connections,
    }
}

/// A broker killed before its ready report told the supervisor what it
/// bound. The preflight saw each endpoint path absent and the registry holds
/// it for this run alone, so a socket there now is this broker's: remove it,
/// and nothing that is not a socket.
fn unlink_unreported(document: &ConnectionsDocument) {
    for connection in document.connections() {
        let path = Path::new(&connection.endpoint_unix);
        if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket()) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The broker of `run_id` closed its side of the control pipe. Under a live
/// run that is the run's failure (D7 rule 5): the workload is cancelled, the
/// endpoints are unlinked, and the outcome keeps every record reported before
/// the end with `complete: false`. In any other state it is that state's own
/// teardown, and nothing happens here.
fn broker_ended<B: Backend>(shared: &Weak<Shared>, inner: &Weak<B>, run_id: &str) {
    let (Some(shared), Some(inner)) = (shared.upgrade(), inner.upgrade()) else {
        return;
    };
    let live = {
        let mut runs = shared.runs.lock();
        match runs.get(run_id) {
            Some(Entry::Live(_)) => match runs.insert(run_id.to_owned(), Entry::Ending) {
                Some(Entry::Live(live)) => live,
                _ => unreachable!("matched a live entry under the lock"),
            },
            _ => return,
        }
    };
    let outcome = end_broker(&live, None);
    let mut failure = ExecutionError::backend(
        "the run's connection broker exited while the run was live, so the run is failed \
         and its workload cancelled",
    );
    match inner.cancel(run_id) {
        Ok(_) => {
            shared.registry.remove(run_id);
        }
        // The workload may be alive: its endpoints stay reserved until a
        // cancel succeeds.
        Err(cancel) => {
            failure.detail = format!(
                "{}; cancelling the workload failed: {}",
                failure.detail, cancel.detail
            );
        }
    }
    shared.runs.lock().insert(
        run_id.to_owned(),
        Entry::Ended {
            outcome: Some(outcome),
            failure: Some(failure),
        },
    );
    shared.changed.notify_all();
}

impl<B: Backend> Backend for Brokered<B> {
    fn capabilities(&self) -> BackendCapabilities {
        self.inner.capabilities()
    }

    fn provision(&self) -> Result<BackendCapabilities, ExecutionError> {
        self.inner.provision()
    }

    /// A runtime with a broker serves connections, whatever its tier; a tier
    /// that cannot deliver one refuses the run at start by name (S3, S6).
    fn connections(&self) -> Option<ConnectionsCapability> {
        self.shared
            .config
            .as_ref()
            .map(|_| ConnectionsCapability::current())
    }

    fn start(
        &self,
        request: &ExecutionRequest,
        workspaces: ServedWorkspaces,
        connections: RunConnections,
    ) -> Result<BackendRun, ExecutionError> {
        let connections = match connections.into_state() {
            State::None => None,
            State::Verified(connections) => Some(connections),
            State::Served(served) => {
                return Err(ExecutionError::invalid(format!(
                    "run {:?}: its connections are already served; a run has one broker",
                    served.run_id
                )));
            }
        };
        let Some(config) = &self.shared.config else {
            if let Some(connections) = &connections {
                return Err(ExecutionError::unsupported(format!(
                    "connection `{}`: the run declares connections \
                     (cloister/connections/v1), and this runtime has no connection broker",
                    first_name(connections.document())
                )));
            }
            return self
                .inner
                .start(request, workspaces, RunConnections::none());
        };
        let run_id = request.run_id.as_str();
        let manifest = request
            .confinement_manifest
            .as_deref()
            .map(ConfinementManifest::parse);
        let Some(connections) = connections else {
            // No broker, but the run's §6 grants are registered all the same,
            // so it cannot reach another run's endpoint and no later endpoint
            // lands under one of its grants (S4). A manifest that does not
            // parse is the worker's to refuse.
            let grants = match &manifest {
                Some(Ok(manifest)) => socket_grants(manifest),
                _ => Vec::new(),
            };
            if grants.is_empty() {
                return self
                    .inner
                    .start(request, workspaces, RunConnections::none());
            }
            {
                let _runs = self.shared.runs.lock();
                self.shared.registry.register(run_id, &[], &grants)?;
            }
            let started = self
                .inner
                .start(request, workspaces, RunConnections::none());
            if started.is_err() {
                self.shared.registry.remove(run_id);
            }
            return started;
        };

        // Step 2: every check, before anything is spawned.
        let document = connections.document();
        let class = self.inner.capabilities().backend_class;
        if !config.unverified_delivery
            && let Err(reason) = tier_delivers(class, std::env::consts::OS)
        {
            return Err(ExecutionError::unsupported(format!(
                "connection `{}`: {reason}",
                first_name(document)
            )));
        }
        super::check_delivered(document)?;
        let manifest = match manifest {
            Some(Ok(manifest)) => manifest,
            Some(Err(error)) => {
                return Err(ExecutionError::invalid_grant(format!(
                    "connection `{}`: the run's confinement manifest does not parse, so its \
                     endpoint has no §6 grant: {}",
                    first_name(document),
                    error.detail
                )));
            }
            None => {
                return Err(ExecutionError::invalid_grant(format!(
                    "connection `{}`: the run carries no confinement manifest, so its \
                     endpoint has no §6 connect grant",
                    first_name(document)
                )));
            }
        };
        cross_document(class, document, &manifest)?;
        // SAFETY: geteuid cannot fail.
        let euid = unsafe { libc::geteuid() };
        for connection in document.connections() {
            super::preflight::check_endpoint(
                &connection.name,
                Path::new(&connection.endpoint_unix),
                euid,
            )?;
        }
        let endpoints: Vec<PathBuf> = document
            .connections()
            .iter()
            .map(|connection| PathBuf::from(&connection.endpoint_unix))
            .collect();
        {
            let mut runs = self.shared.runs.lock();
            if matches!(
                runs.get(run_id),
                Some(Entry::Starting { .. } | Entry::Ending | Entry::Live(_))
            ) {
                return Err(ExecutionError::resource_conflict(format!(
                    "run {run_id} already has a connection broker"
                )));
            }
            self.shared
                .registry
                .register(run_id, &endpoints, &socket_grants(&manifest))?;
            runs.insert(
                run_id.to_owned(),
                Entry::Starting {
                    broker: None,
                    cancelled: false,
                },
            );
        }

        // Steps 3 to 5.
        let mut slot = None;
        match self.start_brokered(config, request, workspaces, &connections, class, &mut slot) {
            Ok(run) => Ok(run),
            Err(failure) => {
                // A cancel that killed the broker mid-wait surfaces as the
                // wait's own error; the run's answer is that it was cancelled.
                let cancelled = matches!(
                    self.shared.runs.lock().get(run_id),
                    Some(Entry::Starting {
                        cancelled: true,
                        ..
                    })
                );
                self.abandon(run_id, &connections, slot, failure.workload_may_live);
                Err(if cancelled && failure.error.code != ErrorCode::Cancelled {
                    ExecutionError {
                        detail: format!(
                            "{} ({})",
                            cancelled_during_start().detail,
                            failure.error.detail
                        ),
                        ..cancelled_during_start()
                    }
                } else {
                    failure.error
                })
            }
        }
    }

    fn workspace_capacity(&self) -> usize {
        self.inner.workspace_capacity()
    }

    fn poll(&self, run_id: &str) -> Result<Option<BackendRunStatus>, ExecutionError> {
        self.fail_if_broker_ended(run_id);
        if let Some(failure) = self.pending_failure(run_id) {
            // The workload was cancelled when its broker ended; a completion
            // it published first is not this run's result.
            let _ = self.inner.poll(run_id);
            return Ok(Some(BackendRunStatus::Failed(failure)));
        }
        let Some(status) = self.inner.poll(run_id)? else {
            return Ok(None);
        };
        Ok(Some(match self.finish(run_id, false) {
            Some(failure) => BackendRunStatus::Failed(failure),
            None => status,
        }))
    }

    fn output(&self, run_id: &str) -> Option<crate::GuestOutput> {
        self.inner.output(run_id)
    }

    /// As [`Backend::poll`], and the finished run's connections are taken
    /// into the outcome.
    fn poll_outcome(&self, run_id: &str) -> Result<Option<RunOutcome>, ExecutionError> {
        self.fail_if_broker_ended(run_id);
        if let Some(failure) = self.pending_failure(run_id) {
            let output = self
                .inner
                .poll_outcome(run_id)
                .ok()
                .flatten()
                .and_then(|outcome| outcome.output);
            return Ok(Some(RunOutcome {
                status: BackendRunStatus::Failed(failure),
                output,
                connections: self.take_connections(run_id),
            }));
        }
        let Some(mut outcome) = self.inner.poll_outcome(run_id)? else {
            return Ok(None);
        };
        if let Some(failure) = self.finish(run_id, false) {
            outcome.status = BackendRunStatus::Failed(failure);
        }
        outcome.connections = self.take_connections(run_id);
        Ok(Some(outcome))
    }

    /// Cancel the run, a starting one included: its broker is killed where it
    /// stands and the call returns once the start has torn down.
    fn cancel(&self, run_id: &str) -> Result<bool, ExecutionError> {
        let starting = match self.shared.runs.lock().get_mut(run_id) {
            Some(Entry::Starting { broker, cancelled }) => {
                *cancelled = true;
                if let Some(pid) = *broker
                    && let Ok(pid) = libc::pid_t::try_from(pid)
                    && pid > 1
                {
                    // SAFETY: the broker is this process's own child, and it
                    // is not reaped while its entry is `Starting`, which the
                    // lock held here guarantees it still is.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
                true
            }
            _ => false,
        };
        if starting {
            // Reaches a worker the wrapped backend has spawned and not yet
            // published; the start itself cancels one published after this.
            let interrupted = self.inner.cancel(run_id);
            let mut runs = self.shared.runs.lock();
            while matches!(
                runs.get(run_id),
                Some(Entry::Starting { .. } | Entry::Ending)
            ) {
                self.shared.changed.wait(&mut runs);
            }
            return interrupted.map(|_| true);
        }
        // The workload first, then the drain, so a connection that was open
        // when the workload ended has its final record.
        let cancelled = self.inner.cancel(run_id);
        self.finish(run_id, cancelled.is_err());
        cancelled
    }
}
