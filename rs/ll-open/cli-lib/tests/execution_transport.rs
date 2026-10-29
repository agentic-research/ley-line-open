use std::sync::Arc;

use leyline_cli_lib::daemon::DaemonExt;
use leyline_cli_lib::daemon::execution::ExecutionDaemonExt;
use leyline_runtime::ExecutionService;
use serde_json::{Value, json};

mod execution_fixture;

use execution_fixture::{CompletingBackend, grant_bytes, grant_json, spec_bytes, spec_json};

#[test]
fn canonical_fixture_uses_one_runtime_handler_for_lifecycle_operations() {
    let spec = spec_bytes();
    let grant = grant_bytes(&spec);
    let service = Arc::new(ExecutionService::new(CompletingBackend));
    let handler = execution_fixture::handler(&service);

    let extension = ExecutionDaemonExt::new(Arc::new(handler));
    let handler = extension
        .execution_handler()
        .expect("execution extension must expose its handler");

    let capabilities: Value = serde_json::from_str(&handler.capabilities().unwrap()).unwrap();
    assert!(capabilities.to_string().contains("cloister/execution/v1"));
    let provision: Value = serde_json::from_str(
        &handler
            .provision(&json!({"backendClass":"microVm","idempotencyKey":"p-transport"}))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(provision["provisioned"], true);

    let start: Value = serde_json::from_str(
        &handler
            .start(&json!({
                "spec": spec_json(&spec),
                "grant": grant_json(&grant),
            }))
            .unwrap(),
    )
    .unwrap();
    let run_id = start["runId"].as_str().expect("run id").to_owned();
    let status: Value =
        serde_json::from_str(&handler.status(&json!({"runId": run_id})).unwrap()).unwrap();
    assert_eq!(status["state"], "running");

    let inspection: Value = serde_json::from_str(
        &handler
            .inspect(&json!({"runId": run_id, "afterSequence": 0}))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(inspection["runId"], run_id);
    assert!(inspection["events"].as_array().is_some());

    let cancel: Value = serde_json::from_str(
        &handler
            .cancel(&json!({"runId": run_id, "idempotencyKey":"c-transport"}))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(cancel["state"], "cancelled");
    let receipt: Value =
        serde_json::from_str(&handler.collect(&json!({"runId": run_id})).unwrap()).unwrap();
    assert_eq!(receipt["receipt"]["runId"], run_id);
    assert_eq!(receipt["receipt"]["terminalState"], "cancelled");

    let cleanup: Value = serde_json::from_str(
        &handler
            .cleanup(&json!({"runId": run_id, "idempotencyKey":"x-transport"}))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(cleanup["runId"], run_id);
    assert_eq!(cleanup["state"], "cleaned");
}
