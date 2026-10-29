#![cfg(unix)]

use std::sync::Arc;

use leyline_cli_lib::ExecutionCommands;
use leyline_cli_lib::cmd_execution;
use leyline_cli_lib::daemon::client::ExecutionClient;
use leyline_cli_lib::daemon::execution_contract;
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

#[tokio::test]
async fn execution_client_round_trips_one_request_without_process_shelling() {
    let dir = tempdir().expect("temp directory");
    let socket = dir.path().join("execution.sock");
    let listener = UnixListener::bind(&socket).expect("bind test socket");
    let expected = Arc::new(json!({
        "op": "llo_execution_capabilities"
    }));
    let expected_for_server = Arc::clone(&expected);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept client");
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        let line = lines
            .next_line()
            .await
            .expect("read request")
            .expect("request line");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap(),
            *expected_for_server
        );
        writer
            .write_all(
                b"{\"capabilities\":[{\"name\":\"cloister/execution/v1\",\"version\":\"v1\"}]}\n",
            )
            .await
            .expect("write response");
    });

    let response = ExecutionClient::new(&socket)
        .capabilities()
        .await
        .expect("client response");
    assert_eq!(response["capabilities"][0]["name"], "cloister/execution/v1");
    server.await.expect("server task");
}

#[test]
fn execution_contract_builds_every_wire_envelope_exactly() {
    let spec = json!({"schemaVersion": "cloister/execution/v1"});
    let grant = json!({"grantId": "grant-1"});

    assert_eq!(
        execution_contract::capabilities(),
        json!({"op": "llo_execution_capabilities"})
    );
    assert_eq!(
        execution_contract::provision("microVm", "provision-1"),
        json!({
            "op": "llo_execution_provision",
            "backendClass": "microVm",
            "idempotencyKey": "provision-1"
        })
    );
    assert_eq!(
        execution_contract::status(Some("run-1")),
        json!({"op": "llo_execution_status", "runId": "run-1"})
    );
    assert_eq!(
        execution_contract::status(None),
        json!({"op": "llo_execution_status", "runId": ""})
    );
    assert_eq!(
        execution_contract::start(spec.clone(), grant.clone()),
        json!({"op": "llo_execution_start", "spec": spec, "grant": grant})
    );
    assert_eq!(
        execution_contract::inspect("run-1", 17),
        json!({
            "op": "llo_execution_inspect",
            "runId": "run-1",
            "afterSequence": 17
        })
    );
    assert_eq!(
        execution_contract::cancel("run-1", Some("cancel-1")),
        json!({
            "op": "llo_execution_cancel",
            "runId": "run-1",
            "idempotencyKey": "cancel-1"
        })
    );
    assert_eq!(
        execution_contract::cancel("run-1", None),
        json!({
            "op": "llo_execution_cancel",
            "runId": "run-1",
            "idempotencyKey": ""
        })
    );
    assert_eq!(
        execution_contract::collect("run-1"),
        json!({"op": "llo_execution_collect", "runId": "run-1"})
    );
    assert_eq!(
        execution_contract::cleanup("run-1", Some("cleanup-1")),
        json!({
            "op": "llo_execution_cleanup",
            "runId": "run-1",
            "idempotencyKey": "cleanup-1"
        })
    );
    assert_eq!(
        execution_contract::cleanup("run-1", None),
        json!({
            "op": "llo_execution_cleanup",
            "runId": "run-1",
            "idempotencyKey": ""
        })
    );
}

#[tokio::test]
async fn execution_client_methods_send_every_contract_request() {
    let dir = tempdir().expect("temp directory");
    let socket = dir.path().join("execution.sock");
    let listener = UnixListener::bind(&socket).expect("bind test socket");
    let expected = vec![
        execution_contract::capabilities(),
        execution_contract::provision("microVm", "provision-1"),
        execution_contract::status(Some("run-1")),
        execution_contract::start(json!({"spec": 1}), json!({"grant": 2})),
        execution_contract::inspect("run-1", 7),
        execution_contract::cancel("run-1", Some("cancel-1")),
        execution_contract::collect("run-1"),
        execution_contract::cleanup("run-1", Some("cleanup-1")),
    ];
    let expected_for_server = expected.clone();
    let server = tokio::spawn(async move {
        for (index, expected_request) in expected_for_server.into_iter().enumerate() {
            let (stream, _) = listener.accept().await.expect("accept client");
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let line = lines
                .next_line()
                .await
                .expect("read request")
                .expect("request line");
            assert_eq!(
                serde_json::from_str::<Value>(&line).expect("request JSON"),
                expected_request
            );
            writer
                .write_all(format!("{{\"response\":{index}}}\n").as_bytes())
                .await
                .expect("write response");
        }
    });

    let client = ExecutionClient::new(&socket);
    let responses = vec![
        client.capabilities().await.expect("capabilities"),
        client
            .provision("microVm", "provision-1")
            .await
            .expect("provision"),
        client.status(Some("run-1")).await.expect("status"),
        client
            .start(json!({"spec": 1}), json!({"grant": 2}))
            .await
            .expect("start"),
        client.inspect("run-1", 7).await.expect("inspect"),
        client
            .cancel("run-1", Some("cancel-1"))
            .await
            .expect("cancel"),
        client.collect("run-1").await.expect("collect"),
        client
            .cleanup("run-1", Some("cleanup-1"))
            .await
            .expect("cleanup"),
    ];
    assert_eq!(
        responses,
        (0..expected.len())
            .map(|index| json!({"response": index}))
            .collect::<Vec<_>>()
    );
    server.await.expect("server task");
}

async fn assert_command_fails_without_daemon(
    future: impl std::future::Future<Output = anyhow::Result<()>>,
) {
    let error = future.await.expect_err("missing daemon must fail closed");
    assert!(
        error
            .to_string()
            .contains("connect execution daemon socket"),
        "unexpected error: {error:#}"
    );
}

#[tokio::test]
async fn execution_commands_fail_closed_when_daemon_is_unavailable() {
    let dir = tempdir().expect("temp directory");
    let missing_socket = dir.path().join("missing.sock");
    let spec_path = dir.path().join("spec.json");
    let grant_path = dir.path().join("grant.json");
    std::fs::write(&spec_path, br#"{"spec":true}"#).expect("write spec");
    std::fs::write(&grant_path, br#"{"grant":true}"#).expect("write grant");

    let control = || missing_socket.clone();
    for command in [
        ExecutionCommands::Capabilities { control: control() },
        ExecutionCommands::Provision {
            control: control(),
            backend_class: "microVm".into(),
            idempotency_key: "provision-1".into(),
        },
        ExecutionCommands::Status {
            control: control(),
            run_id: Some("run-1".into()),
        },
        ExecutionCommands::Start {
            control: control(),
            spec: spec_path.clone(),
            grant: grant_path.clone(),
        },
        ExecutionCommands::Inspect {
            control: control(),
            run_id: "run-1".into(),
            after_sequence: 0,
        },
        ExecutionCommands::Cancel {
            control: control(),
            run_id: "run-1".into(),
            idempotency_key: Some("cancel-1".into()),
        },
        ExecutionCommands::Collect {
            control: control(),
            run_id: "run-1".into(),
        },
        ExecutionCommands::Cleanup {
            control: control(),
            run_id: "run-1".into(),
            idempotency_key: Some("cleanup-1".into()),
        },
    ] {
        assert_command_fails_without_daemon(cmd_execution::run(&command)).await;
    }
}

/// `leyline execution` dispatches through `cmd_execution::run`, failure
/// included.
#[tokio::test]
async fn the_cli_dispatch_reaches_the_execution_client() {
    let dir = tempdir().expect("temp directory");
    assert_command_fails_without_daemon(leyline_cli_lib::run(
        leyline_cli_lib::Commands::Execution {
            command: ExecutionCommands::Capabilities {
                control: dir.path().join("missing.sock"),
            },
        },
    ))
    .await;
}

#[test]
fn a_start_command_with_an_unreadable_spec_fails_before_dialing() {
    let dir = tempdir().expect("temp directory");
    let error = cmd_execution::request(&ExecutionCommands::Start {
        control: dir.path().join("never-dialed.sock"),
        spec: dir.path().join("missing-spec.json"),
        grant: dir.path().join("missing-grant.json"),
    })
    .expect_err("a missing spec file");
    assert!(error.to_string().contains("read spec"), "{error:#}");
    let spec = dir.path().join("spec.json");
    std::fs::write(&spec, b"not json").expect("write spec");
    let error = cmd_execution::request(&ExecutionCommands::Start {
        control: dir.path().join("never-dialed.sock"),
        spec,
        grant: dir.path().join("missing-grant.json"),
    })
    .expect_err("a malformed spec file");
    assert!(error.to_string().contains("parse spec JSON"), "{error:#}");
}
