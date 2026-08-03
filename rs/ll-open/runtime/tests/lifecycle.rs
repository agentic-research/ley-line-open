use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

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

impl Backend for CompletingBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            backend_id: "completing/1".into(),
            backend_class: BackendClass::MicroVm,
            available: true,
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
