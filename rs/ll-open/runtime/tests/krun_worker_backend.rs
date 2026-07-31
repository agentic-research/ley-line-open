use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use leyline_runtime::backends::libkrun::backend::{KrunWorkerBackend, KrunWorkerConfig};
use leyline_runtime::{Backend, BackendClass, DigestRef, ExecutionRequest, ResourceLimits};
use tempfile::TempDir;

fn request() -> ExecutionRequest {
    ExecutionRequest {
        run_id: "run-backend-01".into(),
        replay_key: "replay-backend-01".into(),
        rootfs: DigestRef {
            algorithm: "blake3-256".into(),
            value: "a".repeat(64),
        },
        executable: "usr/bin/true".into(),
        arguments: vec!["true".into()],
        public_environment: BTreeMap::new(),
        allowed_egress: Vec::new(),
        limits: ResourceLimits {
            vcpus: 1,
            memory_mib: 512,
            wall_time_ms: 10_000,
        },
    }
}

#[test]
fn backend_spawns_the_explicit_first_party_worker_and_waits_for_ready() {
    // Catches reporting a run as started before the confined worker has
    // prepared libkrun, and catches PATH-based backend selection.
    let fixture = TempDir::new().expect("fixture");
    let request_log = fixture.path().join("request.json");
    let worker = fixture.path().join("leyline-krun-worker");
    fs::write(
        &worker,
        format!(
            "#!/bin/sh\n/bin/cat > '{}'\nprintf '%s\\n' '{{\"type\":\"ready\",\"run_id\":\"run-backend-01\"}}' >&2\n/bin/sleep 2\n",
            request_log.display()
        ),
    )
    .expect("fake worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).expect("worker mode");
    let cas = fixture.path().join("cas");
    fs::create_dir(&cas).expect("CAS");
    let libkrun = fixture.path().join("libkrun.dylib");
    fs::write(&libkrun, b"library").expect("library fixture");
    let backend = KrunWorkerBackend::new(KrunWorkerConfig {
        worker,
        cas_root: cas,
        libkrun,
        runtime_files: Vec::new(),
        devices: Vec::new(),
        ready_timeout: Duration::from_secs(1),
    });

    let capabilities = backend.capabilities();
    assert!(capabilities.available);
    assert_eq!(capabilities.backend_class, BackendClass::MicroVm);
    let started = backend.start(&request()).expect("ready worker");

    assert_eq!(started.backend_id, "libkrun/1");
    let observed: ExecutionRequest =
        serde_json::from_slice(&fs::read(request_log).expect("request log")).expect("request JSON");
    assert_eq!(observed, request());
}
