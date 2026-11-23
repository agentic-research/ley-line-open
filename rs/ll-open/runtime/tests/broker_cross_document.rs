//! Every refusal before a broker is spawned, by name (ADR-0046 Slice 1, S2,
//! S4, S6; bead `ley-line-open-5308b7`): the tier check, the delivery check,
//! the cross-document rule against the run's confinement manifest, the
//! endpoint checks and the endpoint registry.
//!
//! Driven through `Backend::start` on a `Brokered` backend, the path an
//! embedder with no service takes, and through `ExecutionService`, which is
//! built on the same layer. Each refusal is asserted to have spawned nothing:
//! no broker process and no backend start.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use leyline_runtime::broker::supervisor::{cross_document, tier_delivers};
use leyline_runtime::confinement::{ConfinementManifest, FsGrant, UnixSocketGrant};
use leyline_runtime::{
    Backend, BackendCapabilities, BackendClass, BackendRun, BackendRunStatus, Brokered,
    ConnectionsCapability, EnforcedCeilings, ErrorCode, ExecutionError, ExecutionRequest,
    ExecutionService, GuestOutput, RunConnections, RunState, ServedConnections, ServedWorkspaces,
    VerifiedConnections,
};

mod broker_fixture;
mod common;

use broker_fixture::{
    Host, channel_dir, connections, echo, endpoint, manifest, native_grant, request,
};

/// What a [`Probe`] does inside `start`, with the served connections it was
/// handed. Without one it names this process as the worker.
type OnStart = Arc<
    dyn Fn(&ExecutionRequest, Option<&ServedConnections>) -> Result<(), ExecutionError>
        + Send
        + Sync,
>;

/// A backend of either class that starts nothing: it counts the starts that
/// reach it, names its worker (or runs `on_start`), finishes a run on
/// request, and reports each cancel.
#[derive(Clone)]
struct Probe {
    class: BackendClass,
    on_start: Option<OnStart>,
    /// Take the served connections for another run's request.
    misroute: bool,
    starts: Arc<AtomicUsize>,
    finished: Arc<Mutex<Vec<String>>>,
    cancel_fails: Arc<AtomicBool>,
    /// One entry per cancel that reached this backend.
    cancels: Arc<Mutex<Vec<String>>>,
    cancelled: Option<mpsc::Sender<()>>,
}

impl Probe {
    fn new(class: BackendClass) -> Self {
        Self {
            class,
            on_start: None,
            misroute: false,
            starts: Arc::new(AtomicUsize::new(0)),
            finished: Arc::default(),
            cancel_fails: Arc::default(),
            cancels: Arc::default(),
            cancelled: None,
        }
    }

    fn on_start(
        mut self,
        hook: impl Fn(&ExecutionRequest, Option<&ServedConnections>) -> Result<(), ExecutionError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.on_start = Some(Arc::new(hook));
        self
    }

    fn finish(&self, run_id: &str) {
        self.finished.lock().unwrap().push(run_id.to_owned());
    }
}

impl Backend for Probe {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            backend_id: "probe/1".into(),
            backend_class: self.class,
            available: true,
            unavailable_reason: None,
            enforced: EnforcedCeilings::wall_clock_only(),
        }
    }

    fn start(
        &self,
        request: &ExecutionRequest,
        _workspaces: ServedWorkspaces,
        connections: RunConnections,
    ) -> Result<BackendRun, ExecutionError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        let served = if self.misroute {
            connections.served_for(&broker_fixture::request("another-run", None), "probe/1")?
        } else {
            connections.served_for(request, "probe/1")?
        };
        match &self.on_start {
            Some(hook) => hook(request, served.as_ref())?,
            // This test process connects in the worker's place.
            None => {
                if let Some(served) = &served {
                    served.name_worker(std::process::id())?;
                }
            }
        }
        Ok(BackendRun {
            backend_id: "probe/1".into(),
            attached: Vec::new(),
        })
    }

    fn poll(&self, run_id: &str) -> Result<Option<BackendRunStatus>, ExecutionError> {
        let mut finished = self.finished.lock().unwrap();
        Ok(finished.iter().position(|id| id == run_id).map(|index| {
            finished.remove(index);
            BackendRunStatus::Succeeded
        }))
    }

    fn output(&self, run_id: &str) -> Option<GuestOutput> {
        Some(GuestOutput {
            stdout_tail: format!("output of {run_id}"),
            ..GuestOutput::default()
        })
    }

    fn cancel(&self, run_id: &str) -> Result<bool, ExecutionError> {
        self.cancels.lock().unwrap().push(run_id.to_owned());
        if let Some(cancelled) = &self.cancelled {
            let _ = cancelled.send(());
        }
        if self.cancel_fails.load(Ordering::SeqCst) {
            return Err(ExecutionError::backend("the probe's cancel failed"));
        }
        Ok(true)
    }
}

fn brokered(host: &Host, class: BackendClass) -> (Brokered<Probe>, Probe) {
    let probe = Probe::new(class);
    (Brokered::new(probe.clone(), host.broker(None)), probe)
}

fn start(
    backend: &Brokered<Probe>,
    run_id: &str,
    manifest: Option<&ConfinementManifest>,
    connections: &VerifiedConnections,
) -> Result<BackendRun, ExecutionError> {
    backend.start(
        &request(run_id, manifest),
        ServedWorkspaces::none(),
        RunConnections::verified(connections.clone()),
    )
}

/// Asserts a refusal that spawned nothing and names what it should.
fn refused(
    host: &Host,
    probe: &Probe,
    result: Result<BackendRun, ExecutionError>,
    code: ErrorCode,
    names: &[&str],
) {
    let error = result.expect_err("refused");
    assert_eq!(error.code, code, "{error}");
    for name in names {
        assert!(
            error.detail.contains(name),
            "{name:?} in {:?}",
            error.detail
        );
    }
    assert_eq!(probe.starts.load(Ordering::SeqCst), 0, "no backend start");
    assert_eq!(host.broker_pid(), None, "no broker was spawned");
}

#[test]
fn slice_one_delivers_on_macos_and_refuses_every_linux_tier_by_name() {
    assert_eq!(tier_delivers(BackendClass::MicroVm, "macos"), Ok(()));
    assert_eq!(tier_delivers(BackendClass::Native, "macos"), Ok(()));
    let microvm = tier_delivers(BackendClass::MicroVm, "linux").expect_err("refused");
    assert!(microvm.contains("microVM tier on linux"), "{microvm}");
    assert!(microvm.contains("guestPort"), "{microvm}");
    let native = tier_delivers(BackendClass::Native, "linux").expect_err("refused");
    assert!(native.contains("native tier on linux"), "{native}");
    assert!(native.contains("§6"), "{native}");
}

/// The shipped configuration performs the tier check: off macOS a run with
/// connections is refused `unsupportedBackend`, naming tier and connection,
/// before anything is spawned; on macOS the same run starts.
#[test]
fn the_tier_check_runs_before_anything_is_spawned() {
    let _serial = common::serial();
    let host = Host::new();
    let (_keep, dir) = channel_dir();
    let declared = connections(&dir, &["api"], "127.0.0.1", echo());
    let manifest = manifest(vec![native_grant(&dir, "api")]);
    let probe = Probe::new(BackendClass::Native);
    // `BrokerConfig::new`, as a daemon builds it: no test override.
    let config = leyline_runtime::BrokerConfig::new(host.broker(None).binary);
    // As on Linux, on every host: refused by name, nothing spawned.
    for (class, tier) in [
        (BackendClass::Native, "native tier on linux"),
        (BackendClass::MicroVm, "microVM tier on linux"),
    ] {
        let elsewhere = Probe::new(class);
        let backend = Brokered::new(elsewhere.clone(), config.clone().with_host_os("linux"));
        refused(
            &host,
            &elsewhere,
            start(&backend, "run-linux", Some(&manifest), &declared),
            ErrorCode::UnsupportedBackend,
            &["connection `api`", tier],
        );
    }
    let backend = Brokered::new(probe.clone(), config);
    let result = start(&backend, "run-tier", Some(&manifest), &declared);
    if cfg!(target_os = "macos") {
        result.expect("the native tier on macOS delivers");
        assert!(backend.cancel("run-tier").expect("cancel"));
    } else {
        refused(
            &host,
            &probe,
            result,
            ErrorCode::UnsupportedBackend,
            &["connection `api`", "native tier on linux"],
        );
    }
}

#[test]
fn an_undelivered_loopback_target_is_refused_by_name() {
    let _serial = common::serial();
    let host = Host::new();
    let (_keep, dir) = channel_dir();
    let (backend, probe) = brokered(&host, BackendClass::Native);
    let manifest = manifest(vec![native_grant(&dir, "api")]);
    for target in ["127.0.0.2", "::1"] {
        refused(
            &host,
            &probe,
            start(
                &backend,
                "run-target",
                Some(&manifest),
                &connections(&dir, &["api"], target, 4000),
            ),
            ErrorCode::UnsupportedBackend,
            &[
                "connection `api`",
                "loopback address not delivered by this runtime",
            ],
        );
    }
}

#[test]
fn every_cross_document_mismatch_is_refused_naming_the_connection() {
    let _serial = common::serial();
    let host = Host::new();
    let (_keep, dir) = channel_dir();
    let declared = connections(&dir, &["api", "db"], "127.0.0.1", 4000);
    let api = endpoint(&dir, "api");
    let db = endpoint(&dir, "db");
    let directory = format!("{}/", dir.display());
    let with_port = |path: &str, port| {
        UnixSocketGrant::connect(path)
            .with_guest_port(port)
            .expect("guestPort")
    };

    let (native, native_probe) = brokered(&host, BackendClass::Native);
    let (microvm, microvm_probe) = brokered(&host, BackendClass::MicroVm);
    let native_ok = || {
        vec![
            UnixSocketGrant::connect(&api),
            UnixSocketGrant::connect(&db),
        ]
    };
    let read_write = |path: &str| {
        manifest(native_ok())
            .with_fs_grant(FsGrant::read_write(path))
            .expect("§2 grant")
    };

    type Case<'a> = (
        &'a str,
        &'a Brokered<Probe>,
        &'a Probe,
        Option<ConfinementManifest>,
        Vec<&'a str>,
    );
    let cases: Vec<Case<'_>> = vec![
        (
            "no manifest at all",
            &native,
            &native_probe,
            None,
            vec!["connection `api`", "no confinement manifest"],
        ),
        (
            "an endpoint with no grant",
            &native,
            &native_probe,
            Some(manifest(vec![UnixSocketGrant::connect(&api)])),
            vec!["connection `db`", &db, "no §6 connect grant"],
        ),
        (
            "an endpoint granted twice",
            &native,
            &native_probe,
            Some(manifest(vec![
                UnixSocketGrant::connect(&api),
                UnixSocketGrant::connect(&api),
                UnixSocketGrant::connect(&db),
            ])),
            vec!["connection `api`", "2 times"],
        ),
        (
            "the microVM spelling on the native tier",
            &native,
            &native_probe,
            Some(manifest(vec![
                with_port(&api, 13306),
                UnixSocketGrant::connect(&db),
            ])),
            vec!["connection `api`", "guestPort 13306", "native tier"],
        ),
        (
            "the native spelling on the microVM tier",
            &microvm,
            &microvm_probe,
            Some(manifest(vec![
                with_port(&api, 13306),
                UnixSocketGrant::connect(&db),
            ])),
            vec!["connection `db`", "no guestPort", "microVM tier"],
        ),
        (
            "a bind grant on the endpoint",
            &native,
            &native_probe,
            Some(manifest(vec![
                UnixSocketGrant::connect(&api),
                UnixSocketGrant::connect(&db),
                UnixSocketGrant::bind(&db),
            ])),
            vec![
                "connection `db`",
                "§6 bind grant",
                "bind the broker's socket",
            ],
        ),
        (
            "a connect-bind grant in place of the connect grant",
            &native,
            &native_probe,
            Some(manifest(vec![
                UnixSocketGrant::connect_bind(&api),
                UnixSocketGrant::connect(&db),
            ])),
            vec!["connection `api`", "§6 connect-bind grant"],
        ),
        (
            "a connect-bind directory grant over the channel directory",
            &native,
            &native_probe,
            Some(manifest(vec![
                UnixSocketGrant::connect(&api),
                UnixSocketGrant::connect(&db),
                UnixSocketGrant::connect_bind(&directory),
            ])),
            vec!["connection `api`", "§6 connect-bind grant", &directory],
        ),
        (
            "a §2 write grant on the endpoint's directory",
            &native,
            &native_probe,
            Some(read_write(dir.to_str().unwrap())),
            vec!["connection `api`", "§2 read-write grant"],
        ),
        (
            "a §2 write grant on an ancestor of it",
            &native,
            &native_probe,
            Some(read_write(dir.parent().unwrap().to_str().unwrap())),
            vec!["connection `api`", "§2 read-write grant"],
        ),
    ];
    for (case, backend, probe, manifest, names) in cases {
        eprintln!("case: {case}");
        refused(
            &host,
            probe,
            start(backend, "run-cross", manifest.as_ref(), &declared),
            ErrorCode::InvalidGrant,
            &names,
        );
    }

    // What the rule admits: a read-only §2 grant on the directory, a §2
    // write grant elsewhere, a peer channel the document does not declare,
    // and a connect directory grant beside the exact one.
    let admitted = manifest(native_ok())
        .with_unix_socket(UnixSocketGrant::connect("/tmp/peer-channel.sock"))
        .and_then(|m| m.with_unix_socket(UnixSocketGrant::connect(&directory)))
        .and_then(|m| m.with_fs_grant(FsGrant::read_only(dir.to_str().unwrap())))
        .and_then(|m| m.with_fs_grant(FsGrant::read_write("/tmp/elsewhere-rw")))
        .expect("manifest");
    assert_eq!(
        cross_document(BackendClass::Native, declared.document(), &admitted),
        Ok(())
    );
    let microvm_ok = manifest(vec![with_port(&api, 13306), with_port(&db, 13307)]);
    assert_eq!(
        cross_document(BackendClass::MicroVm, declared.document(), &microvm_ok),
        Ok(())
    );
}

#[test]
fn every_endpoint_check_is_refused_by_name() {
    use std::os::unix::fs::PermissionsExt;
    let _serial = common::serial();
    let host = Host::new();
    let (backend, probe) = brokered(&host, BackendClass::Native);
    let run = |dir: &std::path::Path| {
        start(
            &backend,
            "run-endpoint",
            Some(&manifest(vec![native_grant(dir, "api")])),
            &connections(dir, &["api"], "127.0.0.1", 4000),
        )
    };

    let (_keep, dir) = channel_dir();
    std::fs::write(endpoint(&dir, "api"), b"").expect("squatter");
    refused(
        &host,
        &probe,
        run(&dir),
        ErrorCode::ResourceConflict,
        &["connection `api`", "endpoint exists"],
    );

    let (_keep, dir) = channel_dir();
    refused(
        &host,
        &probe,
        run(&dir.join("missing")),
        ErrorCode::InvalidGrant,
        &["connection `api`", "endpoint parent missing"],
    );

    let (_keep, dir) = channel_dir();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("mode");
    refused(
        &host,
        &probe,
        run(&dir),
        ErrorCode::InvalidGrant,
        &["connection `api`", "endpoint parent not private"],
    );

    let (_keep, dir) = channel_dir();
    let link = dir.join("link");
    let (_target_keep, target) = channel_dir();
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    refused(
        &host,
        &probe,
        run(&link),
        ErrorCode::InvalidGrant,
        &["connection `api`", "reached through a symlink"],
    );
}

/// Run B whose §6 grant names run A's endpoint is refused in both start
/// orders, and a run's endpoints are free again once it has ended.
#[test]
fn the_registry_keeps_one_run_from_another_runs_broker() {
    let _serial = common::serial();
    let host = Host::new();
    let (backend, probe) = brokered(&host, BackendClass::Native);
    let (_keep, dir) = channel_dir();
    let port = echo();
    let declared = connections(&dir, &["api"], "127.0.0.1", port);
    let own = manifest(vec![native_grant(&dir, "api")]);
    // B's manifest: a plain §6 connect grant on A's endpoint, no connections.
    let reach = request("run-b", Some(&own));
    let start_b = || backend.start(&reach, ServedWorkspaces::none(), RunConnections::none());

    // A first: B is refused, and A's broker saw nothing from B.
    start(&backend, "run-a", Some(&own), &declared).expect("run A");
    let error = start_b().expect_err("B reaches A's endpoint");
    assert_eq!(error.code, ErrorCode::InvalidGrant, "{error}");
    assert!(error.detail.contains("another run (run-a)"), "{error}");
    assert_eq!(probe.starts.load(Ordering::SeqCst), 1, "B never started");
    // The same directory, a second run: refused by name.
    let error = start(&backend, "run-a2", Some(&own), &declared).expect_err("endpoint taken");
    assert_eq!(error.code, ErrorCode::ResourceConflict, "{error}");
    assert!(error.detail.contains("connection `api`"), "{error}");
    assert!(error.detail.contains("endpoint exists"), "{error}");

    probe.finish("run-a");
    let outcome = backend
        .poll_outcome("run-a")
        .expect("poll")
        .expect("run A finished");
    let recorded = outcome.connections.expect("A's connections");
    assert_eq!(recorded.digest, declared.digest());
    assert_eq!(recorded.connections.len(), 1);
    assert_eq!(recorded.connections[0].accepted, 0, "nothing from B");
    assert_eq!(recorded.connections[0].foreign_peers, 0);
    assert!(recorded.connections[0].complete);
    assert!(
        !std::path::Path::new(&endpoint(&dir, "api")).exists(),
        "A's endpoint is unlinked at its end"
    );

    // B first: A's endpoint would land under B's grant, so A is refused.
    start_b().expect("B alone holds a peer channel");
    let error = start(&backend, "run-a3", Some(&own), &declared).expect_err("held by B");
    assert_eq!(error.code, ErrorCode::ResourceConflict, "{error}");
    assert!(
        error.detail.contains("held by another run (run-b)"),
        "{error}"
    );
    // B ends: the endpoint is free.
    assert!(backend.cancel("run-b").expect("cancel B"));
    start(&backend, "run-a4", Some(&own), &declared).expect("A after B ended");

    // A live run whose workload cannot be cancelled: its broker is stopped
    // and its socket unlinked, and its endpoint stays reserved (the workload
    // may still be alive) until a cancel succeeds.
    probe.cancel_fails.store(true, Ordering::SeqCst);
    backend
        .cancel("run-a4")
        .expect_err("the workload's cancel failed");
    assert!(!std::path::Path::new(&endpoint(&dir, "api")).exists());
    let error = start_b().expect_err("A's endpoint is still reserved");
    assert!(error.detail.contains("another run (run-a4)"), "{error}");
    probe.cancel_fails.store(false, Ordering::SeqCst);
    assert!(backend.cancel("run-a4").expect("cancel A"));
    assert!(backend.take_connections("run-a4").is_some());
    start_b().expect("released by the cancel that succeeded");
    assert!(backend.cancel("run-b").expect("cancel B"));
}

fn microvm_grant(dir: &Path) -> ConfinementManifest {
    manifest(vec![
        UnixSocketGrant::connect(endpoint(dir, "api"))
            .with_guest_port(13306)
            .unwrap(),
    ])
}

/// Dial the endpoint and round-trip one line with the echo; `None` when the
/// broker closed the connection without a byte.
fn dial(dir: &Path) -> Option<Vec<u8>> {
    let mut stream = UnixStream::connect(endpoint(dir, "api")).expect("dial the endpoint");
    stream
        .set_read_timeout(Some(broker_fixture::BOUND))
        .expect("bound");
    stream.write_all(b"ping").expect("write");
    let mut reply = [0u8; 4];
    match stream.read_exact(&mut reply) {
        Ok(()) => Some(reply.to_vec()),
        Err(_) => None,
    }
}

/// The microVM tier's broker accepts only the worker, so a backend that
/// starts a run without naming it has a broker that refuses every
/// connection: the run is refused and its broker torn down. When the
/// workload then cannot be cancelled, the run's endpoints stay reserved
/// until a cancel succeeds.
#[test]
fn a_microvm_backend_that_names_no_worker_is_refused_and_torn_down() {
    let _serial = common::serial();
    let host = Host::new();
    let (_keep, dir) = channel_dir();
    let declared = connections(&dir, &["api"], "127.0.0.1", echo());
    let granted = microvm_grant(&dir);
    let probe = Probe::new(BackendClass::MicroVm).on_start(|_, _| Ok(()));
    let backend = Brokered::new(probe.clone(), host.broker(None));
    let error = start(&backend, "run-unnamed", Some(&granted), &declared).expect_err("refused");
    assert_eq!(error.code, ErrorCode::BackendFailed, "{error}");
    assert!(
        error.detail.contains("without naming its worker"),
        "{error}"
    );
    assert_eq!(
        *probe.cancels.lock().unwrap(),
        ["run-unnamed"],
        "the started workload is cancelled"
    );
    let broker = host.broker_pid().expect("a broker was spawned");
    common::await_exit(broker, broker_fixture::BOUND).expect("the broker is gone");
    assert!(!Path::new(&endpoint(&dir, "api")).exists());

    // The same, with a workload that cannot be cancelled: the refusal names
    // both failures, the broker and its socket still go, and the endpoint
    // stays reserved against every other run.
    probe.cancel_fails.store(true, Ordering::SeqCst);
    let error = start(&backend, "run-stuck", Some(&granted), &declared).expect_err("refused");
    for part in [
        "without naming its worker",
        "cancelling the workload also failed",
        "the probe's cancel failed",
    ] {
        assert!(error.detail.contains(part), "{part:?} in {error}");
    }
    assert!(!Path::new(&endpoint(&dir, "api")).exists());
    let error = start(&backend, "run-next", Some(&granted), &declared).expect_err("reserved");
    assert_eq!(error.code, ErrorCode::InvalidGrant, "{error}");
    assert!(
        error
            .detail
            .contains("the broker endpoint of another run (run-stuck)"),
        "{error}"
    );
    // A cancel that succeeds releases it.
    probe.cancel_fails.store(false, Ordering::SeqCst);
    assert!(backend.cancel("run-stuck").expect("cancel"));

    // Named, the same run starts; the broker attested the document's digest
    // and relays for the named process, this one.
    let attested = Arc::new(Mutex::new(String::new()));
    let seen = Arc::clone(&attested);
    let naming = Probe::new(BackendClass::MicroVm).on_start(move |_, served| {
        let served = served.expect("served connections");
        *seen.lock().unwrap() = served.digest().to_owned();
        served.name_worker(std::process::id())?;
        let again = served
            .name_worker(std::process::id())
            .expect_err("the peer is named once");
        assert!(again.detail.contains("already named"), "{again}");
        Ok(())
    });
    let backend = Brokered::new(naming, host.broker(None));
    start(&backend, "run-named", Some(&granted), &declared).expect("started");
    assert_eq!(*attested.lock().unwrap(), declared.digest());
    assert_eq!(dial(&dir).as_deref(), Some(&b"ping"[..]));
    assert_eq!(
        backend.output("run-named").map(|output| output.stdout_tail),
        Some("output of run-named".to_owned()),
        "the wrapped backend's output is the run's"
    );
    assert!(backend.cancel("run-named").expect("cancel"));
    let outcome = backend.take_connections("run-named").expect("outcome");
    assert_eq!(
        (
            outcome.connections[0].accepted,
            outcome.connections[0].dialed
        ),
        (1, 1)
    );

    // Pid 0 names no process: refused, never sent to the broker.
    let zero = Probe::new(BackendClass::MicroVm)
        .on_start(|_, served| served.expect("served").name_worker(0));
    let backend = Brokered::new(zero, host.broker(None));
    let error = start(&backend, "run-zero", Some(&granted), &declared).expect_err("pid 0");
    assert!(error.detail.contains("not a worker process id"), "{error}");

    // On the native tier there is no worker to name: the broker checks the
    // peer's uid, and naming is a no-op that leaves it relaying.
    let (_native_keep, native_dir) = channel_dir();
    let native = connections(&native_dir, &["api"], "127.0.0.1", echo());
    let backend = Brokered::new(Probe::new(BackendClass::Native), host.broker(None));
    start(
        &backend,
        "run-native",
        Some(&manifest(vec![native_grant(&native_dir, "api")])),
        &native,
    )
    .expect("started");
    assert_eq!(dial(&native_dir).as_deref(), Some(&b"ping"[..]));
    assert!(backend.cancel("run-native").expect("cancel"));
}

/// Served connections are one run's: a backend that takes them for another
/// run's request is refused.
#[test]
fn served_connections_are_refused_for_another_runs_request() {
    let _serial = common::serial();
    let host = Host::new();
    let (_keep, dir) = channel_dir();
    let declared = connections(&dir, &["api"], "127.0.0.1", echo());
    let mut probe = Probe::new(BackendClass::Native);
    probe.misroute = true;
    let backend = Brokered::new(probe, host.broker(None));
    let error = start(
        &backend,
        "run-own",
        Some(&manifest(vec![native_grant(&dir, "api")])),
        &declared,
    )
    .expect_err("another run's request");
    assert_eq!(error.code, ErrorCode::IdentityPolicyMismatch, "{error}");
    assert!(error.detail.contains("\"run-own\""), "{error}");
    assert!(error.detail.contains("\"another-run\""), "{error}");
    assert!(!Path::new(&endpoint(&dir, "api")).exists(), "torn down");
}

/// Failures after the broker is ready (S7 step 5): a broker that dies while
/// the workload starts, and a squatter that replaces the broker's socket.
#[test]
fn a_failure_after_ready_tears_down_only_what_the_broker_bound() {
    let _serial = common::serial();
    let host = Host::new();
    let (_keep, dir) = channel_dir();
    let declared = connections(&dir, &["api"], "127.0.0.1", echo());
    let granted = manifest(vec![native_grant(&dir, "api")]);
    let socket = endpoint(&dir, "api");

    // The broker is killed while the backend starts the workload: the start
    // is refused, although the backend answered Ok.
    let pid_file = host.path("broker.pid");
    let dying = Probe::new(BackendClass::Native).on_start(move |_, _| {
        let broker: libc::pid_t = std::fs::read_to_string(&pid_file)
            .expect("broker pid")
            .trim()
            .parse()
            .expect("pid");
        // SAFETY: a signal to the broker this test's backend spawned.
        unsafe { libc::kill(broker, libc::SIGKILL) };
        common::await_exit(broker, broker_fixture::BOUND).expect("the broker exits");
        Ok(())
    });
    let backend = Brokered::new(dying.clone(), host.broker(None));
    let error = start(&backend, "run-dying", Some(&granted), &declared).expect_err("refused");
    assert_eq!(error.code, ErrorCode::BackendFailed, "{error}");
    assert!(
        error
            .detail
            .contains("exited while its workload was starting"),
        "{error}"
    );
    assert_eq!(*dying.cancels.lock().unwrap(), ["run-dying"]);
    assert!(
        !Path::new(&socket).exists(),
        "the dead broker's socket is unlinked"
    );

    // The registry entry went with the failed start: on the same backend,
    // the same endpoint is free for another run. This one's backend replaces
    // the broker's socket with its own and then fails: teardown removes only
    // the socket the broker reported, so the squatter's stays.
    let squatter: Arc<Mutex<Option<UnixListener>>> = Arc::default();
    let (keep, path) = (Arc::clone(&squatter), socket.clone());
    let squatting = Probe::new(BackendClass::Native).on_start(move |_, _| {
        std::fs::remove_file(&path).expect("remove the broker's socket");
        *keep.lock().unwrap() = Some(UnixListener::bind(&path).expect("bind in its place"));
        Err(ExecutionError::backend("the workload did not start"))
    });
    let backend = Brokered::new(squatting, host.broker(None));
    let error = start(&backend, "run-squatted", Some(&granted), &declared).expect_err("failed");
    assert!(error.detail.contains("did not start"), "{error}");
    assert!(
        Path::new(&socket).exists(),
        "a socket the broker did not bind is never removed"
    );
}

/// A cancel that arrives while the wrapped backend is starting the workload,
/// and that the backend's own cancel does not stop: the start finishes, sees
/// the cancel, cancels the run and answers `cancelled`; the canceller
/// returns with the broker gone.
#[test]
fn a_cancel_during_step_five_is_honored_before_the_run_is_published() {
    let _serial = common::serial();
    let host = Host::new();
    let (_keep, dir) = channel_dir();
    let declared = connections(&dir, &["api"], "127.0.0.1", echo());
    let granted = manifest(vec![native_grant(&dir, "api")]);
    let (entered, in_start) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let released = Mutex::new(released);
    let (cancelled, cancel_seen) = mpsc::channel();
    let mut probe = Probe::new(BackendClass::Native).on_start(move |_, _| {
        let _ = entered.send(());
        let _ = released.lock().unwrap().recv();
        Ok(())
    });
    probe.cancelled = Some(cancelled);
    let backend = Brokered::new(probe.clone(), host.broker(None));
    std::thread::scope(|scope| {
        let starting = scope.spawn(|| start(&backend, "run-late", Some(&granted), &declared));
        // EVENT: the broker is ready and the backend is starting the workload.
        in_start
            .recv_timeout(broker_fixture::BOUND)
            .expect("in start");
        let cancelling = scope.spawn(|| backend.cancel("run-late"));
        // EVENT: the cancel is marked and has reached the wrapped backend.
        cancel_seen
            .recv_timeout(broker_fixture::BOUND)
            .expect("the cancel reached the backend");
        release.send(()).expect("release the start");
        let error = starting.join().unwrap().expect_err("cancelled");
        assert_eq!(error.code, ErrorCode::Cancelled, "{error}");
        assert!(
            error.detail.contains("cancelled while it was starting"),
            "{error}"
        );
        assert!(cancelling.join().unwrap().expect("cancel"));
    });
    assert_eq!(
        *probe.cancels.lock().unwrap(),
        ["run-late", "run-late"],
        "once from the cancel, once from the start that saw it"
    );
    let broker = host.broker_pid().expect("broker pid");
    common::await_exit(broker, broker_fixture::BOUND).expect("the broker is gone");
    assert!(!Path::new(&endpoint(&dir, "api")).exists());
    assert!(backend.take_connections("run-late").is_none());
}

/// `ExecutionService::cancel` of a run whose backend is still starting it,
/// with no connections: the cancel returns only once the start has ended,
/// and the run is never published.
#[test]
fn a_service_cancel_waits_for_the_starting_run_to_end() {
    let _serial = common::serial();
    let (entered, in_start) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let released = Mutex::new(released);
    let (cancelled, cancel_seen) = mpsc::channel();
    let mut probe = Probe::new(BackendClass::Native).on_start(move |_, _| {
        let _ = entered.send(());
        let _ = released.lock().unwrap().recv();
        Ok(())
    });
    probe.cancelled = Some(cancelled);
    let service = ExecutionService::new(probe.clone());
    let order = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        let starting = scope.spawn(|| {
            let result = service.start(request("run-gated", None));
            order.lock().unwrap().push("start ended");
            result
        });
        in_start
            .recv_timeout(broker_fixture::BOUND)
            .expect("in start");
        let cancelling = scope.spawn(|| {
            let record = service.cancel("run-gated");
            // The backend's cancels so far: the canceller's own, and the one
            // the start made when it saw the mark. Both precede this return.
            order
                .lock()
                .unwrap()
                .push(match probe.cancels.lock().unwrap().len() {
                    2 => "cancel returned after the start cancelled the run",
                    _ => "cancel returned before the start ended",
                });
            record
        });
        cancel_seen
            .recv_timeout(broker_fixture::BOUND)
            .expect("the cancel reached the backend");
        release.send(()).expect("release the start");
        let error = starting.join().unwrap().expect_err("cancelled");
        assert_eq!(error.code, ErrorCode::Cancelled, "{error}");
        let record = cancelling.join().unwrap().expect("cancel");
        assert_eq!(record.state, RunState::Cancelled);
        assert_eq!(record.run_id, "run-gated");
        assert_eq!(record.replay_key, "replay-run-gated");
        assert_eq!(record.backend_id, "probe/1");
    });
    assert!(
        order
            .lock()
            .unwrap()
            .contains(&"cancel returned after the start cancelled the run"),
        "{order:?}"
    );
    assert_eq!(service.status("run-gated").expect("status"), None);
    // The reservation is gone: the same run starts again.
    drop(release);
    service
        .start(request("run-gated", None))
        .expect("the run id is free");
}

/// The capability is the broker: reported with one, absent without, and a
/// backend handed connections nothing serves refuses them by name.
#[test]
fn connections_nothing_serves_are_refused_by_name_on_every_path() {
    let _serial = common::serial();
    let host = Host::new();
    let (_keep, dir) = channel_dir();
    let declared = connections(&dir, &["api"], "127.0.0.1", 4000);
    let granted = manifest(vec![native_grant(&dir, "api")]);

    let probe = Probe::new(BackendClass::Native);
    assert_eq!(probe.connections(), None, "a bare backend serves none");
    let unbrokered = Brokered::without_broker(probe.clone());
    assert_eq!(unbrokered.connections(), None);
    refused(
        &host,
        &probe,
        start(&unbrokered, "run-none", Some(&granted), &declared),
        ErrorCode::UnsupportedBackend,
        &["connection `api`", "no connection broker"],
    );
    let (with_broker, _) = brokered(&host, BackendClass::Native);
    assert_eq!(
        with_broker.connections(),
        Some(ConnectionsCapability {
            version: "1".into()
        })
    );

    // The first-party backend, called with no broker layer around it.
    let native = host.backend("exec cat > /dev/null");
    let error = native
        .start(
            &request("run-direct", Some(&granted)),
            ServedWorkspaces::none(),
            RunConnections::verified(declared.clone()),
        )
        .expect_err("unserved");
    assert_eq!(error.code, ErrorCode::UnsupportedBackend, "{error}");
    assert!(error.detail.contains("connection `api`"), "{error}");
    assert!(error.detail.contains("no broker serving them"), "{error}");
    assert_eq!(host.worker_pid(), None, "no worker was spawned");

    // The service's direct-request path, with and without a broker.
    let service = ExecutionService::new(probe.clone());
    assert_eq!(service.connections(), None);
    let error = service
        .start_with_connections(request("run-service", Some(&granted)), declared.clone())
        .expect_err("no broker");
    assert_eq!(error.code, ErrorCode::UnsupportedBackend, "{error}");
    assert!(error.detail.contains("no connection broker"), "{error}");
    assert_eq!(probe.starts.load(Ordering::SeqCst), 0);

    // A document that is not its own canonical form is never a value.
    let canonical = String::from_utf8(declared.document().canonical_bytes()).unwrap();
    assert_eq!(
        VerifiedConnections::from_canonical(canonical.as_bytes()).expect("canonical"),
        declared
    );
    let respaced = canonical.replace("\n  ", "\n    ");
    let error = VerifiedConnections::from_canonical(respaced.as_bytes()).expect_err("respaced");
    assert_eq!(error.code, ErrorCode::InvalidGrant);
    assert!(error.detail.contains("not in canonical form"), "{error}");
    let error = VerifiedConnections::from_canonical(b"{}").expect_err("not a document");
    assert!(
        error.detail.contains("not a valid cloister/connections/v1"),
        "{error}"
    );
}
