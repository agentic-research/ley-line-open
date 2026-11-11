use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

use leyline_runtime::{
    Backend, BackendCapabilities, BackendClass, BackendRun, BackendRunStatus, DigestRef, ErrorCode,
    ExecutionError, ExecutionRequest, ExecutionService, ResourceLimits, RunState,
};

#[derive(Clone, Default)]
struct RecordingBackend {
    calls: Arc<Mutex<Vec<&'static str>>>,
}

#[derive(Clone)]
struct CompletingBackend;

#[derive(Clone)]
struct FailingBackend;

#[derive(Clone)]
struct BlockingBackend {
    gate: Arc<StartGate>,
    starts: Arc<AtomicUsize>,
}

impl BlockingBackend {
    fn new(gate: Arc<StartGate>) -> Self {
        Self {
            gate,
            starts: Arc::new(AtomicUsize::new(0)),
        }
    }
}

struct StartGate {
    entered: Mutex<bool>,
    entered_cv: Condvar,
    release: Mutex<bool>,
    release_cv: Condvar,
}

impl StartGate {
    fn new() -> Self {
        Self {
            entered: Mutex::new(false),
            entered_cv: Condvar::new(),
            release: Mutex::new(false),
            release_cv: Condvar::new(),
        }
    }

    fn wait_until_entered(&self) {
        let mut entered = self.entered.lock().expect("entered lock");
        while !*entered {
            entered = self.entered_cv.wait(entered).expect("entered wait");
        }
    }

    fn release(&self) {
        *self.release.lock().expect("release lock") = true;
        self.release_cv.notify_all();
    }
}

impl Backend for CompletingBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            backend_id: "completing/1".into(),
            backend_class: BackendClass::MicroVm,
            available: true,
            unavailable_reason: None,
            enforced: leyline_runtime::EnforcedCeilings::hypervisor_backed(),
        }
    }

    fn start(
        &self,
        _request: &ExecutionRequest,
        _workspaces: leyline_runtime::ServedWorkspaces,
    ) -> Result<BackendRun, ExecutionError> {
        Ok(BackendRun {
            backend_id: "completing/1".into(),
            attached: Vec::new(),
        })
    }

    fn poll(&self, _run_id: &str) -> Result<Option<BackendRunStatus>, ExecutionError> {
        Ok(Some(BackendRunStatus::Succeeded))
    }

    fn cancel(&self, _run_id: &str) -> Result<bool, ExecutionError> {
        Ok(true)
    }
}

impl Backend for FailingBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            backend_id: "failing/1".into(),
            backend_class: BackendClass::MicroVm,
            available: true,
            unavailable_reason: None,
            enforced: leyline_runtime::EnforcedCeilings::hypervisor_backed(),
        }
    }

    fn start(
        &self,
        _request: &ExecutionRequest,
        _workspaces: leyline_runtime::ServedWorkspaces,
    ) -> Result<BackendRun, ExecutionError> {
        Ok(BackendRun {
            backend_id: "failing/1".into(),
            attached: Vec::new(),
        })
    }

    fn poll(&self, _run_id: &str) -> Result<Option<BackendRunStatus>, ExecutionError> {
        Ok(Some(BackendRunStatus::Failed(ExecutionError {
            code: ErrorCode::BackendFailed,
            retryable: false,
            detail: "guest exited with status 127".into(),
        })))
    }

    fn cancel(&self, _run_id: &str) -> Result<bool, ExecutionError> {
        Ok(true)
    }
}

impl Backend for BlockingBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            backend_id: "blocking/1".into(),
            backend_class: BackendClass::MicroVm,
            available: true,
            unavailable_reason: None,
            enforced: leyline_runtime::EnforcedCeilings::hypervisor_backed(),
        }
    }

    fn start(
        &self,
        _request: &ExecutionRequest,
        _workspaces: leyline_runtime::ServedWorkspaces,
    ) -> Result<BackendRun, ExecutionError> {
        if self.starts.fetch_add(1, Ordering::SeqCst) != 0 {
            return Err(ExecutionError {
                code: ErrorCode::BackendFailed,
                retryable: false,
                detail: "blocking backend was entered more than once".into(),
            });
        }
        *self.gate.entered.lock().expect("entered lock") = true;
        self.gate.entered_cv.notify_all();
        let mut release = self.gate.release.lock().expect("release lock");
        while !*release {
            release = self.gate.release_cv.wait(release).expect("release wait");
        }
        Ok(BackendRun {
            backend_id: "blocking/1".into(),
            attached: Vec::new(),
        })
    }

    fn cancel(&self, _run_id: &str) -> Result<bool, ExecutionError> {
        Ok(true)
    }
}

impl RecordingBackend {
    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().expect("recording backend lock").clone()
    }
}

impl Backend for RecordingBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            backend_id: "recording/1".into(),
            backend_class: BackendClass::MicroVm,
            available: true,
            unavailable_reason: None,
            enforced: leyline_runtime::EnforcedCeilings::hypervisor_backed(),
        }
    }

    fn start(
        &self,
        _request: &ExecutionRequest,
        _workspaces: leyline_runtime::ServedWorkspaces,
    ) -> Result<BackendRun, ExecutionError> {
        self.calls
            .lock()
            .expect("recording backend lock")
            .push("start");
        Ok(BackendRun {
            backend_id: "recording/1".into(),
            attached: Vec::new(),
        })
    }

    fn cancel(&self, _run_id: &str) -> Result<bool, ExecutionError> {
        self.calls
            .lock()
            .expect("recording backend lock")
            .push("cancel");
        Ok(true)
    }
}

fn request(replay_key: &str) -> ExecutionRequest {
    ExecutionRequest {
        run_id: "run-01".into(),
        replay_key: replay_key.into(),
        rootfs: DigestRef {
            algorithm: "blake3-256".into(),
            value: "a".repeat(64),
        },
        executable: "usr/bin/true".into(),
        arguments: vec!["true".into()],
        public_environment: BTreeMap::from([("CI".into(), "true".into())]),
        allowed_egress: Vec::new(),
        confinement_digest: String::new(),
        confinement_manifest: None,
        workspaces: Vec::new(),
        limits: ResourceLimits {
            wall_time_ms: 30_000,
            ..ResourceLimits::default()
        },
    }
}

#[test]
fn status_before_start_is_read_only() {
    // Catches a status implementation that probes, provisions, or starts the
    // backend merely to answer that no run exists.
    let backend = RecordingBackend::default();
    let service = ExecutionService::new(backend.clone());

    assert_eq!(service.status("missing").expect("status"), None);
    assert!(backend.calls().is_empty());
}

/// A backend that keeps no output reports none for any run: the trait's
/// default is absence, never an empty capture that would read as "the run
/// printed nothing" (bead ley-line-open-a26ad0).
#[test]
fn a_backend_without_output_reports_none() {
    assert_eq!(Backend::output(&CompletingBackend, "any-run"), None);
}

/// The default `poll_outcome` is `poll` with no output: the status passes
/// through unchanged.
#[test]
fn the_default_outcome_is_the_polled_status_without_output() {
    let backend = CompletingBackend;
    let outcome = Backend::poll_outcome(&backend, "run-01").expect("poll");
    let polled = Backend::poll(&backend, "run-01").expect("poll");
    assert_eq!(
        outcome.as_ref().map(|outcome| &outcome.status),
        polled.as_ref()
    );
    assert!(outcome.is_some_and(|outcome| outcome.output.is_none()));
}

/// A backend that is available only once its CAS root exists, as the
/// native and libkrun backends are.
struct NeedsCasRoot(std::path::PathBuf);

impl Backend for NeedsCasRoot {
    fn capabilities(&self) -> BackendCapabilities {
        let available = self.0.is_dir();
        BackendCapabilities {
            backend_id: "needs-cas/1".into(),
            backend_class: BackendClass::MicroVm,
            available,
            unavailable_reason: (!available).then(|| "no CAS root".into()),
            enforced: leyline_runtime::EnforcedCeilings::hypervisor_backed(),
        }
    }

    fn start(
        &self,
        _request: &ExecutionRequest,
        _workspaces: leyline_runtime::ServedWorkspaces,
    ) -> Result<BackendRun, ExecutionError> {
        unreachable!("provision tests start nothing")
    }

    fn cancel(&self, _run_id: &str) -> Result<bool, ExecutionError> {
        Ok(false)
    }
}

#[test]
fn provision_makes_the_cas_root_exist_before_the_backend_checks_it_and_reports_it() {
    use leyline_runtime::cas_volume::{CasStorage, CasVolume, DiskImages, FilesystemSensitivity};
    // Linux semantics on any host: the CAS root is `<state>/cas`, made to
    // exist and probed (here, by a probe that reports case-sensitive).
    struct NoImages;
    impl DiskImages for NoImages {
        fn create(
            &self,
            _: &std::path::Path,
            _: u64,
        ) -> Result<(), leyline_runtime::cas_volume::CasVolumeError> {
            unreachable!()
        }
        fn attach(
            &self,
            _: &std::path::Path,
            _: &std::path::Path,
        ) -> Result<(), leyline_runtime::cas_volume::CasVolumeError> {
            unreachable!()
        }
        fn detach(
            &self,
            _: &std::path::Path,
        ) -> Result<(), leyline_runtime::cas_volume::CasVolumeError> {
            unreachable!()
        }
        fn is_mounted(
            &self,
            _: &std::path::Path,
        ) -> Result<bool, leyline_runtime::cas_volume::CasVolumeError> {
            unreachable!()
        }
    }
    let state = tempfile::TempDir::new().unwrap();
    let volume = CasVolume::with_parts(
        state.path(),
        leyline_runtime::cas_volume::DEFAULT_SIZE_CAP_BYTES,
        leyline_runtime::cas_volume::HostOs::Linux,
        Box::new(NoImages),
        |_| Ok(FilesystemSensitivity::EXACT),
    )
    .unwrap();
    let cas_root = volume.cas_root();
    let service = ExecutionService::new(NeedsCasRoot(cas_root.clone()))
        .with_cas_storage(CasStorage::Volume(volume));
    assert_eq!(service.provisioned_cas_root(), None);
    service
        .provision(BackendClass::MicroVm, "provision-storage")
        .expect("storage first, then the backend finds its CAS root");
    assert!(cas_root.is_dir());
    assert_eq!(service.provisioned_cas_root(), Some(cas_root.clone()));

    let output: serde_json::Value = serde_json::from_str(
        &leyline_runtime::transport::provision_json(
            &service,
            r#"{"backendClass":"microVm","idempotencyKey":"provision-storage"}"#,
        )
        .expect("provision JSON"),
    )
    .unwrap();
    assert_eq!(output["casRoot"], cas_root.to_str().unwrap());
    assert_eq!(output["provisioned"], true);
}

/// A provision refused for its backend class touches no storage: no volume
/// is created or attached for a request that will fail.
#[test]
fn a_provision_for_the_wrong_backend_class_touches_no_storage() {
    use leyline_runtime::cas_volume::{
        CasStorage, CasVolume, CasVolumeError, DEFAULT_SIZE_CAP_BYTES, DiskImages, HostOs,
    };
    use std::path::Path;
    struct Untouchable;
    impl DiskImages for Untouchable {
        fn create(&self, _: &Path, _: u64) -> Result<(), CasVolumeError> {
            panic!("a refused provision created a volume")
        }
        fn attach(&self, _: &Path, _: &Path) -> Result<(), CasVolumeError> {
            panic!("a refused provision attached a volume")
        }
        fn detach(&self, _: &Path) -> Result<(), CasVolumeError> {
            panic!("detach")
        }
        fn is_mounted(&self, _: &Path) -> Result<bool, CasVolumeError> {
            panic!("a refused provision inspected the volume")
        }
    }
    let state = tempfile::TempDir::new().unwrap();
    let volume = CasVolume::with_parts(
        state.path(),
        DEFAULT_SIZE_CAP_BYTES,
        HostOs::MacOs,
        Box::new(Untouchable),
        |_| unreachable!("a refused provision probed the volume"),
    )
    .unwrap();
    // CompletingBackend is a microVM backend.
    let error = ExecutionService::new(CompletingBackend)
        .with_cas_storage(CasStorage::Volume(volume))
        .provision(BackendClass::Native, "provision-wrong-class")
        .expect_err("the wrong class is refused");
    assert_eq!(error.code, ErrorCode::UnsupportedBackend);
    assert_eq!(std::fs::read_dir(state.path()).unwrap().count(), 0);
}

#[test]
fn provision_refuses_a_missing_operator_cas_root_and_a_refused_host_by_name() {
    use leyline_runtime::cas_volume::{CasStorage, CasVolume, DEFAULT_SIZE_CAP_BYTES};
    let missing = std::env::temp_dir().join("leyline-no-such-cas-root-ba86d3");
    let service = ExecutionService::new(CompletingBackend)
        .with_cas_storage(CasStorage::Directory(missing.clone()));
    let error = service
        .provision(BackendClass::MicroVm, "provision-missing")
        .expect_err("a missing --cas-root is not provisioned");
    assert_eq!(error.code, ErrorCode::BackendFailed);
    assert!(
        error.detail.contains("is not an existing directory"),
        "{error:?}"
    );
    assert!(!service.is_provisioned());

    let state = tempfile::TempDir::new().unwrap();
    let volume = CasVolume::with_parts(
        state.path(),
        DEFAULT_SIZE_CAP_BYTES,
        leyline_runtime::cas_volume::HostOs::Other("plan9"),
        Box::new(leyline_runtime::cas_volume::Hdiutil::default()),
        leyline_runtime::cas_volume::probe_sensitivity,
    )
    .unwrap();
    let error = ExecutionService::new(CompletingBackend)
        .with_cas_storage(CasStorage::Volume(volume))
        .provision(BackendClass::MicroVm, "provision-plan9")
        .expect_err("refused by name");
    assert_eq!(error.code, ErrorCode::UnsupportedBackend);
    assert!(error.detail.contains("not supported on plan9"), "{error:?}");

    // Without storage the output still carries the field, empty.
    let service = ExecutionService::new(CompletingBackend);
    let output: serde_json::Value = serde_json::from_str(
        &leyline_runtime::transport::provision_json(
            &service,
            r#"{"backendClass":"microVm","idempotencyKey":"provision-plain"}"#,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(output["casRoot"], "");
}

#[test]
fn status_projects_backend_completion_without_sleeping() {
    let service = ExecutionService::new(CompletingBackend);
    service.start(request("completion-replay")).expect("start");

    let record = service
        .status("run-01")
        .expect("refresh status")
        .expect("run record");
    assert_eq!(record.state, RunState::Succeeded);

    let inspection = service.inspect("run-01", 0).expect("inspect");
    assert_eq!(
        inspection.events.last().expect("terminal event").state,
        RunState::Succeeded
    );
    assert_eq!(
        inspection
            .events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
}

#[test]
fn inspect_cursor_is_strictly_after_the_reported_sequence() {
    let service = ExecutionService::new(RecordingBackend::default());
    service.start(request("cursor")).expect("start");

    let inspection = service.inspect("run-01", 2).expect("inspect after cursor");
    assert_eq!(
        inspection
            .events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![3, 4]
    );
}

#[test]
fn failed_completion_preserves_content_addressed_detail_on_event() {
    let service = ExecutionService::new(FailingBackend);
    service.start(request("failure-detail")).expect("start");

    let inspection = service.inspect("run-01", 0).expect("inspect");
    let terminal = inspection.events.last().expect("terminal event");
    assert_eq!(terminal.state, RunState::Failed);
    let detail = terminal
        .detail_digest
        .as_deref()
        .expect("failure detail digest");
    assert!(
        detail.starts_with("blake3-256:"),
        "unexpected digest: {detail}"
    );
}

#[test]
fn repeated_start_with_one_replay_key_returns_the_same_run() {
    // Catches loss of replay-key idempotency, which would boot two VMs for a
    // retried transport request.
    let backend = RecordingBackend::default();
    let service = ExecutionService::new(backend.clone());

    let first = service.start(request("replay-1")).expect("first start");
    let second = service.start(request("replay-1")).expect("replayed start");

    assert_eq!(first.run_id, second.run_id);
    assert_eq!(first.state, RunState::Running);
    assert_eq!(second.state, RunState::Running);
    assert_eq!(backend.calls(), vec!["start"]);
}

#[test]
fn status_progresses_while_backend_start_is_waiting() {
    // Backend startup can wait for a worker/rootfs readiness event. The
    // service state lock must not be held across that wait, or an unrelated
    // status request would block behind one slow launch.
    let gate = Arc::new(StartGate::new());
    let service = Arc::new(ExecutionService::new(BlockingBackend::new(Arc::clone(
        &gate,
    ))));
    let start_service = Arc::clone(&service);
    let start_thread = std::thread::spawn(move || start_service.start(request("blocking-start")));

    gate.wait_until_entered();
    let (status_tx, status_rx) = mpsc::channel();
    let status_service = Arc::clone(&service);
    std::thread::spawn(move || {
        status_tx
            .send(status_service.status("unrelated-missing"))
            .expect("status receiver");
    });
    let status = match status_rx.recv_timeout(Duration::from_secs(1)) {
        Ok(status) => status,
        Err(error) => {
            gate.release();
            let _ = start_thread.join();
            panic!("status blocked behind backend startup: {error}");
        }
    };
    assert_eq!(status.expect("status result"), None);

    gate.release();
    start_thread
        .join()
        .expect("start thread")
        .expect("backend start");
}

#[test]
fn concurrent_start_rejects_the_same_replay_key_before_backend_ready() {
    let gate = Arc::new(StartGate::new());
    let service = Arc::new(ExecutionService::new(BlockingBackend::new(Arc::clone(
        &gate,
    ))));
    let start_service = Arc::clone(&service);
    let start_thread = std::thread::spawn(move || start_service.start(request("reserved-replay")));

    gate.wait_until_entered();
    let mut competing = request("reserved-replay");
    competing.run_id = "run-02".into();
    let competing_result = service.start(competing);
    gate.release();
    start_thread
        .join()
        .expect("start thread")
        .expect("backend start");
    let error = competing_result
        .expect_err("an in-flight replay key must be reserved before backend readiness");
    assert_eq!(error.code, ErrorCode::ResourceConflict);
    assert!(error.retryable);
}

#[test]
fn cancel_is_idempotent_and_updates_the_shared_lifecycle_state() {
    let backend = RecordingBackend::default();
    let service = ExecutionService::new(backend.clone());
    service.start(request("cancel-1")).expect("start");

    let cancelled = service.cancel("run-01").expect("cancel");
    assert_eq!(cancelled.state, RunState::Cancelled);
    assert_eq!(
        service
            .status("run-01")
            .expect("status")
            .expect("record")
            .state,
        RunState::Cancelled
    );
    let repeated = service.cancel("run-01").expect("repeat cancel");
    assert_eq!(repeated.state, RunState::Cancelled);
    assert_eq!(backend.calls(), vec!["start", "cancel"]);
    let inspection = service.inspect("run-01", 0).expect("cancel events");
    assert_eq!(
        inspection
            .events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
}

#[test]
fn cleanup_appends_two_consecutive_terminal_events() {
    let service = ExecutionService::new(RecordingBackend::default());
    service.start(request("cleanup")).expect("start");
    let cleaned = service.cleanup("run-01").expect("cleanup");
    assert_eq!(cleaned.state, RunState::Cleaned);

    let inspection = service.inspect("run-01", 0).expect("cleanup events");
    assert_eq!(
        inspection
            .events
            .iter()
            .map(|event| (event.sequence, event.state))
            .collect::<Vec<_>>(),
        vec![
            (1, RunState::Accepted),
            (2, RunState::Provisioning),
            (3, RunState::Ready),
            (4, RunState::Running),
            (5, RunState::Cleaning),
            (6, RunState::Cleaned),
        ]
    );
}

#[test]
fn invalid_content_identity_fails_before_backend_start() {
    // Catches accepting a mutable/ambiguous rootfs identity at the authority
    // boundary or discovering it only after backend materialization.
    let backend = RecordingBackend::default();
    let service = ExecutionService::new(backend.clone());
    let mut request = request("invalid-digest");
    request.rootfs.algorithm = "sha256".into();

    let error = service.start(request).expect_err("digest must be rejected");

    assert_eq!(error.code, ErrorCode::InvalidSpec);
    assert!(backend.calls().is_empty());
}

#[test]
fn guest_path_traversal_fails_before_backend_start() {
    // Catches turning a guest entrypoint into ambient host path authority.
    let backend = RecordingBackend::default();
    let service = ExecutionService::new(backend.clone());
    let mut request = request("path-traversal");
    request.executable = "../../bin/sh".into();

    let error = service.start(request).expect_err("path must be rejected");

    assert_eq!(error.code, ErrorCode::InvalidSpec);
    assert!(backend.calls().is_empty());
}

#[test]
fn egress_grant_fails_closed_until_a_network_broker_exists() {
    // Catches libkrun's implicit TSI path becoming ambient network authority.
    let backend = RecordingBackend::default();
    let service = ExecutionService::new(backend.clone());
    let mut request = request("egress");
    request.allowed_egress = vec!["example.com:443".into()];

    let error = service.start(request).expect_err("egress must fail closed");

    assert_eq!(error.code, ErrorCode::UnsupportedBackend);
    assert!(backend.calls().is_empty());
}

/// A worker that finished a moment ago: its supervisor has already dropped the
/// run from the backend's active table — so `cancel` reports it no longer owns
/// it — while the terminal status is still waiting to be projected.
struct JustFinishedBackend;

impl Backend for JustFinishedBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            backend_id: "just-finished/1".into(),
            backend_class: BackendClass::MicroVm,
            available: true,
            unavailable_reason: None,
            enforced: leyline_runtime::EnforcedCeilings::hypervisor_backed(),
        }
    }

    fn start(
        &self,
        _request: &ExecutionRequest,
        _workspaces: leyline_runtime::ServedWorkspaces,
    ) -> Result<BackendRun, ExecutionError> {
        Ok(BackendRun {
            backend_id: "just-finished/1".into(),
            attached: Vec::new(),
        })
    }

    fn poll(&self, _run_id: &str) -> Result<Option<BackendRunStatus>, ExecutionError> {
        Ok(Some(BackendRunStatus::Succeeded))
    }

    fn cancel(&self, _run_id: &str) -> Result<bool, ExecutionError> {
        Ok(false)
    }
}

#[test]
fn cancelling_a_run_that_just_completed_returns_its_terminal_record() {
    let service = ExecutionService::new(JustFinishedBackend);
    let record = service.start(request("cancel-race")).expect("start");

    // Every other read path projects the backend completion before acting.
    // `cancel` must too, or a benign race — the worker finishing between the
    // caller's decision and its request — surfaces as a hard backend error
    // while `cleanup` on the same run succeeds.
    let cancelled = service
        .cancel(&record.run_id)
        .expect("cancelling a completed run must project its terminal state");
    assert_eq!(cancelled.state, RunState::Succeeded);
}
