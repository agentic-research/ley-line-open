//! First-party execution/v1 CLI client.
//!
//! This command speaks the daemon's UDS JSON protocol. It never invokes
//! krunvm, Taskfile, or a repository helper; backend ownership stays in LLO.

use std::path::Path;

use crate::daemon::client::ExecutionClient;
use anyhow::{Context, Result};
use serde_json::{Value, json};

async fn call(control: &Path, request: Value) -> Result<()> {
    let response = ExecutionClient::new(control).call(request).await?;
    println!("{}", serde_json::to_string(&response)?);
    Ok(())
}

pub async fn capabilities(control: &Path) -> Result<()> {
    call(control, json!({"op": "llo_execution_capabilities"})).await
}

pub async fn provision(control: &Path, backend_class: &str, idempotency_key: &str) -> Result<()> {
    call(
        control,
        json!({
            "op": "llo_execution_provision",
            "backendClass": backend_class,
            "idempotencyKey": idempotency_key
        }),
    )
    .await
}

pub async fn status(control: &Path, run_id: Option<&str>) -> Result<()> {
    call(
        control,
        json!({"op": "llo_execution_status", "runId": run_id.unwrap_or("")}),
    )
    .await
}

pub async fn start(control: &Path, spec: &Path, grant: &Path) -> Result<()> {
    let spec: Value = serde_json::from_slice(
        &std::fs::read(spec).with_context(|| format!("read spec {}", spec.display()))?,
    )
    .context("parse spec JSON")?;
    let grant: Value = serde_json::from_slice(
        &std::fs::read(grant).with_context(|| format!("read grant {}", grant.display()))?,
    )
    .context("parse grant JSON")?;
    call(
        control,
        json!({"op": "llo_execution_start", "spec": spec, "grant": grant}),
    )
    .await
}

pub async fn inspect(control: &Path, run_id: &str, after_sequence: u64) -> Result<()> {
    call(
        control,
        json!({
            "op": "llo_execution_inspect",
            "runId": run_id,
            "afterSequence": after_sequence
        }),
    )
    .await
}

pub async fn collect(control: &Path, run_id: &str) -> Result<()> {
    call(
        control,
        json!({"op": "llo_execution_collect", "runId": run_id}),
    )
    .await
}

pub async fn cleanup(control: &Path, run_id: &str, idempotency_key: Option<&str>) -> Result<()> {
    call(
        control,
        json!({
            "op": "llo_execution_cleanup",
            "runId": run_id,
            "idempotencyKey": idempotency_key.unwrap_or("")
        }),
    )
    .await
}

pub async fn cancel(control: &Path, run_id: &str, idempotency_key: Option<&str>) -> Result<()> {
    call(
        control,
        json!({
            "op": "llo_execution_cancel",
            "runId": run_id,
            "idempotencyKey": idempotency_key.unwrap_or("")
        }),
    )
    .await
}
