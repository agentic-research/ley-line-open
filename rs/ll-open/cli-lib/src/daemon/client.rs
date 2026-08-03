//! Rust client for the first-party execution/v1 daemon UDS.
//!
//! This is the embedding seam for Cloister and other trusted Rust callers.
//! It speaks the same newline-delimited JSON operations as the CLI, without
//! spawning a process or taking ownership of backend paths.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Debug, Clone)]
pub struct ExecutionClient {
    control: PathBuf,
}

impl ExecutionClient {
    pub fn new(control: impl Into<PathBuf>) -> Self {
        Self {
            control: control.into(),
        }
    }

    pub fn control_path(&self) -> &Path {
        &self.control
    }

    /// Send one execution/v1 operation and decode its JSON response.
    pub async fn call(&self, request: Value) -> Result<Value> {
        let stream = UnixStream::connect(&self.control).await.with_context(|| {
            format!("connect execution daemon socket {}", self.control.display())
        })?;
        let (reader, mut writer) = stream.into_split();
        writer.write_all(request.to_string().as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.shutdown().await?;
        let mut lines = BufReader::new(reader).lines();
        let Some(response) = lines.next_line().await? else {
            bail!("execution daemon closed the connection without a response");
        };
        serde_json::from_str(&response).context("decode execution daemon JSON response")
    }

    pub async fn capabilities(&self) -> Result<Value> {
        self.call(json!({"op": "llo_execution_capabilities"})).await
    }

    pub async fn provision(&self, backend_class: &str, idempotency_key: &str) -> Result<Value> {
        self.call(json!({
            "op": "llo_execution_provision",
            "backendClass": backend_class,
            "idempotencyKey": idempotency_key,
        }))
        .await
    }

    pub async fn status(&self, run_id: Option<&str>) -> Result<Value> {
        self.call(json!({
            "op": "llo_execution_status",
            "runId": run_id.unwrap_or(""),
        }))
        .await
    }

    pub async fn start(&self, spec: Value, grant: Value) -> Result<Value> {
        self.call(json!({
            "op": "llo_execution_start",
            "spec": spec,
            "grant": grant,
        }))
        .await
    }

    pub async fn inspect(&self, run_id: &str, after_sequence: u64) -> Result<Value> {
        self.call(json!({
            "op": "llo_execution_inspect",
            "runId": run_id,
            "afterSequence": after_sequence,
        }))
        .await
    }

    pub async fn collect(&self, run_id: &str) -> Result<Value> {
        self.call(json!({
            "op": "llo_execution_collect",
            "runId": run_id,
        }))
        .await
    }

    pub async fn cleanup(&self, run_id: &str, idempotency_key: Option<&str>) -> Result<Value> {
        self.call(json!({
            "op": "llo_execution_cleanup",
            "runId": run_id,
            "idempotencyKey": idempotency_key.unwrap_or(""),
        }))
        .await
    }

    pub async fn cancel(&self, run_id: &str, idempotency_key: Option<&str>) -> Result<Value> {
        self.call(json!({
            "op": "llo_execution_cancel",
            "runId": run_id,
            "idempotencyKey": idempotency_key.unwrap_or(""),
        }))
        .await
    }
}
