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
    /// The host the tier check asks about: this one, outside a test.
    host_os: &'static str,
}

impl BrokerConfig {
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            ready_timeout: READY_TIMEOUT,
            drain_timeout: DRAIN_TIMEOUT,
            unverified_delivery: false,
            host_os: std::env::consts::OS,
        }
    }

    /// Run the tier check as on `os`, so the refusal a Linux host gives is
    /// exercised on every host. `test-support` only.
    #[cfg(feature = "test-support")]
    pub fn with_host_os(mut self, os: &'static str) -> Self {
        self.host_os = os;
        self
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
/// - no §2 `fs.allow` grant with write access covers an endpoint: its path,
///   its directory or any ancestor of it.
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
        // A write grant on the endpoint path itself, on its directory or on
        // any ancestor of it: each lets the workload replace the socket.
        let resolved = super::registry::resolved(Path::new(endpoint));
        for grant in manifest.fs_grants() {
            let FsGrant::ReadWrite { path } = grant else {
                continue;
            };
            let granted = super::registry::resolved(Path::new(path));
            if Path::new(&resolved).starts_with(Path::new(&granted)) {
                return Err(refuse(format!(
                    "the confinement manifest's §2 read-write grant {path} covers the \
                     endpoint, so the workload could replace the broker's socket"
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
    /// A run with connections, from registration until it is published or
    /// its start has ended. `broker` is the broker's pid while that process
    /// is spawned and certainly unreaped, so a cancel may signal it; it is
    /// cleared, under the lock, before anything that can reap the broker.
    Starting {
        broker: Option<u32>,
        cancelled: bool,
    },
    /// A start that failed, a finish or a broker failure is tearing down.
    Ending,
    Live(Live),
    /// A run with no broker whose §6 grants or endpoints are registered: a
    /// run without connections (its grants are peer channels), or a run
    /// whose broker is gone and whose workload could not be cancelled.
    /// `started` is false while the wrapped backend is still starting it.
    Registered {
        started: bool,
    },
    /// The broker is gone. `failure` is the run's failure when its broker
    /// ended under it, until a poll reports it; `outcome` waits to be taken;
    /// `registered` is true while the workload could not be cancelled, so
    /// its endpoints stay reserved.
    Ended {
        outcome: Option<ConnectionsOutcome>,
        failure: Option<ExecutionError>,
        registered: bool,
    },
}

/// What a teardown knows about the workload, which decides whether the run's
/// registration goes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Workload {
    /// It ended, or a cancel stopped it.
    Gone,
    /// The wrapped backend has no such run: it ended, unless it has not
    /// started yet.
    Unknown,
    /// A cancel failed: it may be alive.
    MayLive,
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

/// Why a start was refused. `workload_may_live` is the failed-cancel path
/// (S7): the workload started and could not be cancelled, so its endpoints
/// stay reserved and the caller keeps the run addressable for a later cancel.
pub(crate) struct StartRefusal {
    pub(crate) error: ExecutionError,
    pub(crate) workload_may_live: bool,
}

impl From<ExecutionError> for StartRefusal {
    fn from(error: ExecutionError) -> Self {
        Self {
            error,
            workload_may_live: false,
        }
    }
}

/// Tears down a start with connections that did not publish its run, on an
/// error and on a panic alike, so no `Starting` entry outlives its start.
struct Abandon<'a, B: Backend> {
    brokered: &'a Brokered<B>,
    run_id: &'a str,
    connections: &'a VerifiedConnections,
    broker: Option<Arc<Mutex<BrokerProcess>>>,
    /// Whether the workload may be alive. True while the wrapped backend's
    /// `start` is running, so a panic in it reserves the endpoints too.
    workload_may_live: bool,
    armed: bool,
}

impl<B: Backend> Drop for Abandon<'_, B> {
    fn drop(&mut self) {
        if self.armed {
            self.brokered.abandon(
                self.run_id,
                self.connections,
                self.broker.take(),
                self.workload_may_live,
            );
        }
    }
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
    ///
    /// A run started through this reference is outside this layer: it has no
    /// broker, and its §6 grants are not in the endpoint registry, so nothing
    /// keeps it from another run's endpoint. Start runs through `Brokered`.
    /// A run's end must also be observed through `Brokered` (`poll`,
    /// `poll_outcome`, `cancel`): that is where its broker is drained and its
    /// registration released.
    pub fn inner(&self) -> &B {
        &self.inner
    }

    /// Take what a finished run's connections carried. `None` for a run
    /// without connections, one still live, or one already taken.
    ///
    /// The outcome waits here until it is taken. A new run started under the
    /// same run id replaces an outcome nobody took.
    pub fn take_connections(&self, run_id: &str) -> Option<ConnectionsOutcome> {
        let mut runs = self.shared.runs.lock();
        match runs.get_mut(run_id) {
            // Kept while it still holds the run's registration.
            Some(Entry::Ended {
                failure: None,
                registered: true,
                outcome,
            }) => outcome.take(),
            Some(Entry::Ended { failure: None, .. }) => match runs.remove(run_id) {
                Some(Entry::Ended { outcome, .. }) => outcome,
                _ => None,
            },
            _ => None,
        }
    }

    /// Whether a cancel of `run_id` has been seen: by this layer, for a run
    /// it has registered, or by the caller, through `external`.
    fn cancel_seen(&self, run_id: &str, external: &AtomicBool) -> bool {
        external.load(std::sync::atomic::Ordering::SeqCst)
            || matches!(
                self.shared.runs.lock().get(run_id),
                Some(Entry::Starting {
                    cancelled: true,
                    ..
                })
            )
    }

    /// [`Backend::start`], with a cancel the caller may raise before this
    /// layer has registered the run (`ExecutionService` marks a starting run
    /// this way), and with the failed-cancel path told apart.
    pub(crate) fn start_cancellable(
        &self,
        request: &ExecutionRequest,
        workspaces: ServedWorkspaces,
        connections: RunConnections,
        cancel: &AtomicBool,
    ) -> Result<BackendRun, StartRefusal> {
        let cancelled = || cancel.load(std::sync::atomic::Ordering::SeqCst);
        let connections = match connections.into_state() {
            State::None => None,
            State::Verified(connections) => Some(connections),
            State::Served(served) => {
                return Err(ExecutionError::invalid(format!(
                    "run {:?}: its connections are already served; a run has one broker",
                    served.run_id
                ))
                .into());
            }
        };
        let Some(config) = &self.shared.config else {
            if let Some(connections) = &connections {
                return Err(ExecutionError::unsupported(format!(
                    "connection `{}`: the run declares connections \
                     (cloister/connections/v1), and this runtime has no connection broker",
                    first_name(connections.document())
                ))
                .into());
            }
            if cancelled() {
                return Err(cancelled_during_start().into());
            }
            return Ok(self
                .inner
                .start(request, workspaces, RunConnections::none())?);
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
            if !grants.is_empty() {
                let mut runs = self.shared.runs.lock();
                self.shared.registry.register(run_id, &[], &grants)?;
                // Replaces an untaken outcome of an earlier run of this id.
                runs.insert(run_id.to_owned(), Entry::Registered { started: false });
            }
            let started = if cancelled() {
                Err(cancelled_during_start())
            } else {
                self.inner
                    .start(request, workspaces, RunConnections::none())
            };
            if !grants.is_empty() {
                let mut runs = self.shared.runs.lock();
                match &started {
                    Ok(_) => {
                        if let Some(Entry::Registered { started }) = runs.get_mut(run_id) {
                            *started = true;
                        }
                    }
                    Err(_) => {
                        if matches!(runs.get(run_id), Some(Entry::Registered { .. })) {
                            runs.remove(run_id);
                        }
                        self.shared.registry.remove(run_id);
                    }
                }
            }
            return Ok(started?);
        };

        // Step 2: every check, before anything is spawned.
        let document = connections.document();
        let class = self.inner.capabilities().backend_class;
        if !config.unverified_delivery
            && let Err(reason) = tier_delivers(class, config.host_os)
        {
            return Err(ExecutionError::unsupported(format!(
                "connection `{}`: {reason}",
                first_name(document)
            ))
            .into());
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
                ))
                .into());
            }
            None => {
                return Err(ExecutionError::invalid_grant(format!(
                    "connection `{}`: the run carries no confinement manifest, so its \
                     endpoint has no §6 connect grant",
                    first_name(document)
                ))
                .into());
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
                Some(
                    Entry::Starting { .. }
                        | Entry::Ending
                        | Entry::Live(_)
                        | Entry::Registered { .. }
                        | Entry::Ended {
                            registered: true,
                            ..
                        }
                )
            ) {
                return Err(ExecutionError::resource_conflict(format!(
                    "run {run_id} already holds endpoints or a connection broker"
                ))
                .into());
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

        // Steps 3 to 5, torn down by `teardown` unless the run is published.
        let mut teardown = Abandon {
            brokered: self,
            run_id,
            connections: &connections,
            broker: None,
            workload_may_live: false,
            armed: true,
        };
        match self.start_brokered(config, request, workspaces, class, cancel, &mut teardown) {
            Ok(run) => {
                teardown.armed = false;
                Ok(run)
            }
            Err(failure) => {
                // A cancel that killed the broker mid-wait surfaces as the
                // wait's own error; the run's answer is that it was cancelled.
                let was_cancelled = self.cancel_seen(run_id, cancel);
                teardown.workload_may_live = failure.workload_may_live;
                drop(teardown);
                let error = if was_cancelled && failure.error.code != ErrorCode::Cancelled {
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
                };
                Err(StartRefusal {
                    error,
                    workload_may_live: failure.workload_may_live,
                })
            }
        }
    }

    /// Steps 3 to 5. `teardown` holds the broker from its spawn, so any
    /// return but `Ok`, a panic included, tears it down.
    fn start_brokered(
        &self,
        config: &BrokerConfig,
        request: &ExecutionRequest,
        workspaces: ServedWorkspaces,
        class: BackendClass,
        cancel: &AtomicBool,
        teardown: &mut Abandon<'_, B>,
    ) -> Result<BackendRun, StartRefusal> {
        let run_id = request.run_id.as_str();
        let connections = teardown.connections;
        // A cancel already seen: nothing is spawned.
        if self.cancel_seen(run_id, cancel) {
            return Err(cancelled_during_start().into());
        }
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
        teardown.broker = Some(Arc::clone(&broker));
        // Step 3: recorded as starting with its broker before the ready wait.
        match self.shared.runs.lock().get_mut(run_id) {
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
        // A cancel seen during the ready wait: the workload is never started.
        if self.cancel_seen(run_id, cancel) {
            return Err(cancelled_during_start().into());
        }
        // Step 5.
        let named = Arc::new(AtomicBool::new(false));
        let served = ServedConnections {
            run_id: run_id.to_owned(),
            digest: connections.digest().to_owned(),
            broker: Arc::clone(&broker),
            awaits_worker,
            named: Arc::clone(&named),
        };
        // From here a workload may exist: a panic in the wrapped start keeps
        // the run's endpoints reserved until a cancel succeeds.
        teardown.workload_may_live = true;
        let run = self
            .inner
            .start(request, workspaces, RunConnections::served(served));
        teardown.workload_may_live = false;
        let run = run?;
        // A started workload from here on: a refusal cancels it.
        let refusal = if awaits_worker && !named.load(std::sync::atomic::Ordering::SeqCst) {
            Some(ExecutionError::backend(format!(
                "backend {} started the run without naming its worker to the connection \
                 broker, which would refuse every connection",
                run.backend_id
            )))
        } else {
            let mut runs = self.shared.runs.lock();
            let marked = matches!(
                runs.get(run_id),
                Some(Entry::Starting {
                    cancelled: true,
                    ..
                })
            );
            // The exit check below reaps a broker that has ended. From here
            // the entry names no pid, so no cancel can signal a reaped one.
            if let Some(Entry::Starting {
                broker: spawned, ..
            }) = runs.get_mut(run_id)
            {
                *spawned = None;
            }
            if marked || cancel.load(std::sync::atomic::Ordering::SeqCst) {
                Some(cancelled_during_start())
            } else if broker.lock().exited() {
                // A broker that ended while the workload started serves
                // nothing. Checked under the lock the end hook takes, so a
                // broker that ends after this is the hook's to fail.
                Some(ExecutionError::backend(
                    "the run's connection broker exited while its workload was starting",
                ))
            } else {
                runs.insert(
                    run_id.to_owned(),
                    Entry::Live(Live {
                        broker: Arc::clone(&broker),
                        connections: connections.clone(),
                    }),
                );
                None
            }
        };
        match refusal {
            None => Ok(run),
            Some(error) => Err(match self.inner.cancel(run_id) {
                Ok(_) => error.into(),
                // The workload may still be running: its endpoints stay
                // reserved until a cancel succeeds (S7).
                Err(cancel) => StartRefusal {
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

    /// Tear down a start that failed after registration. With
    /// `workload_may_live` the registration stays, under an entry a later
    /// cancel releases.
    fn abandon(
        &self,
        run_id: &str,
        connections: &VerifiedConnections,
        broker: Option<Arc<Mutex<BrokerProcess>>>,
        workload_may_live: bool,
    ) {
        // No cancel signals the broker from here: it is about to be reaped.
        self.shared
            .runs
            .lock()
            .insert(run_id.to_owned(), Entry::Ending);
        if let Some(broker) = broker {
            let mut broker = broker.lock();
            let reported = broker.reported();
            broker.shutdown();
            if !reported {
                unlink_unreported(connections.document());
            }
        }
        let mut runs = self.shared.runs.lock();
        if workload_may_live {
            runs.insert(run_id.to_owned(), Entry::Registered { started: true });
        } else {
            self.shared.registry.remove(run_id);
            runs.remove(run_id);
        }
        drop(runs);
        self.shared.changed.notify_all();
    }

    /// End what this layer holds for a run whose workload is `workload`:
    /// drain and stop a live broker and keep its outcome, and release the
    /// run's registration unless the workload may still be alive. Returns the
    /// run's failure when its broker had ended under it.
    fn finish(&self, run_id: &str, workload: Workload) -> Option<ExecutionError> {
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
                    Some(Entry::Ended {
                        failure,
                        registered,
                        ..
                    }) => {
                        let failure = failure.take();
                        if *registered && workload != Workload::MayLive {
                            *registered = false;
                            self.shared.registry.remove(run_id);
                        }
                        return failure;
                    }
                    Some(Entry::Registered { started }) => {
                        // A run the wrapped backend does not know and has
                        // not finished starting keeps its registration: it
                        // may yet go live.
                        let release = match workload {
                            Workload::Gone => true,
                            Workload::Unknown => *started,
                            Workload::MayLive => false,
                        };
                        if release {
                            runs.remove(run_id);
                            self.shared.registry.remove(run_id);
                        }
                        return None;
                    }
                    // A start owns its own registration until it publishes,
                    // and a run this layer holds nothing for has none.
                    Some(Entry::Starting { .. }) | None => return None,
                }
            }
        };
        let drain = self
            .shared
            .config
            .as_ref()
            .map_or(DRAIN_TIMEOUT, |config| config.drain_timeout);
        let outcome = end_broker(&live, Some(drain));
        let registered = workload == Workload::MayLive;
        if !registered {
            self.shared.registry.remove(run_id);
        }
        self.shared.runs.lock().insert(
            run_id.to_owned(),
            Entry::Ended {
                outcome: Some(outcome),
                failure: None,
                registered,
            },
        );
        self.shared.changed.notify_all();
        None
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

/// For a broker that was stopped having reported NOTHING: no ready report
/// (which names each socket it bound, by device and inode) and no error
/// report (before which it unlinks what it bound itself). That is a broker
/// killed by a cancel or by the ready bound while it was binding or
/// confining itself. It may have bound some or all of its endpoints, and the
/// supervisor was never told which.
///
/// Each declared endpoint path that now holds a socket node is unlinked.
/// What that can remove: this broker's socket, which is the usual case (the
/// preflight saw the path absent, and the registry holds the path for this
/// run alone); or a socket another process running as the daemon's uid bound
/// at that path after the preflight, in the run's private 0700 directory,
/// which is indistinguishable here and is the same-uid residual of ADR-0046
/// S10. What it cannot remove: anything that is not a socket, anything at a
/// path the document does not declare, and anything at all once the broker
/// has reported (a ready broker's sockets go by device and inode, and after
/// an error report the supervisor unlinks nothing).
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
    let registered = match inner.cancel(run_id) {
        Ok(_) => {
            shared.registry.remove(run_id);
            false
        }
        // The workload may be alive: its endpoints stay reserved until a
        // cancel succeeds.
        Err(cancel) => {
            failure.detail = format!(
                "{}; cancelling the workload failed: {}",
                failure.detail, cancel.detail
            );
            true
        }
    };
    shared.runs.lock().insert(
        run_id.to_owned(),
        Entry::Ended {
            outcome: Some(outcome),
            failure: Some(failure),
            registered,
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
        self.start_cancellable(request, workspaces, connections, &AtomicBool::new(false))
            .map_err(|refusal| refusal.error)
    }

    fn workspace_capacity(&self) -> usize {
        self.inner.workspace_capacity()
    }

    fn poll(&self, run_id: &str) -> Result<Option<BackendRunStatus>, ExecutionError> {
        if let Some(failure) = self.pending_failure(run_id) {
            // The workload was cancelled when its broker ended; a completion
            // it published first is not this run's result.
            let _ = self.inner.poll(run_id);
            return Ok(Some(BackendRunStatus::Failed(failure)));
        }
        let Some(status) = self.inner.poll(run_id)? else {
            return Ok(None);
        };
        Ok(Some(match self.finish(run_id, Workload::Gone) {
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
        if let Some(failure) = self.finish(run_id, Workload::Gone) {
            outcome.status = BackendRunStatus::Failed(failure);
        }
        outcome.connections = self.take_connections(run_id);
        Ok(Some(outcome))
    }

    /// Cancel the run, a starting one included: its broker is killed where it
    /// stands and the call returns once the start has torn down.
    ///
    /// A cancel for a run this layer has not registered yet reaches the
    /// wrapped backend and nothing else; `ExecutionService` covers that
    /// window with its own mark.
    fn cancel(&self, run_id: &str) -> Result<bool, ExecutionError> {
        let starting = match self.shared.runs.lock().get_mut(run_id) {
            Some(Entry::Starting { broker, cancelled }) => {
                *cancelled = true;
                if let Some(pid) = broker.and_then(crate::backends::spawn::child_pid) {
                    // SAFETY: the broker is this process's own child, and
                    // the entry names its pid only while it is unreaped,
                    // which the lock held here guarantees it still is.
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
        self.finish(
            run_id,
            match &cancelled {
                Ok(true) => Workload::Gone,
                Ok(false) => Workload::Unknown,
                Err(_) => Workload::MayLive,
            },
        );
        cancelled
    }
}
