#![cfg(unix)]

use std::sync::Arc;

use leyline_cli_lib::daemon::client::ExecutionClient;
use serde_json::json;
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
