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
            enforced: leyline_runtime::EnforcedCeilings::hypervisor_backed(),
        }
    }

    fn start(&self, _request: &ExecutionRequest) -> Result<BackendRun, ExecutionError> {
        Ok(BackendRun {
            backend_id: "completing/1".into(),
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
            enforced: leyline_runtime::EnforcedCeilings::hypervisor_backed(),
        }
    }

    fn start(&self, _request: &ExecutionRequest) -> Result<BackendRun, ExecutionError> {
        Ok(BackendRun {
            backend_id: "failing/1".into(),
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
            enforced: leyline_runtime::EnforcedCeilings::hypervisor_backed(),
        }
    }

    fn start(&self, _request: &ExecutionRequest) -> Result<BackendRun, ExecutionError> {
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
            enforced: leyline_runtime::EnforcedCeilings::hypervisor_backed(),
        }
    }

    fn start(&self, _request: &ExecutionRequest) -> Result<BackendRun, ExecutionError> {
        self.calls
            .lock()
            .expect("recording backend lock")
            .push("start");
        Ok(BackendRun {
            backend_id: "recording/1".into(),
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
        limits: ResourceLimits {
            vcpus: 2,
            memory_mib: 2048,
            wall_time_ms: 30_000,
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
            enforced: leyline_runtime::EnforcedCeilings::hypervisor_backed(),
        }
    }

    fn start(&self, _request: &ExecutionRequest) -> Result<BackendRun, ExecutionError> {
        Ok(BackendRun {
            backend_id: "just-finished/1".into(),
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
