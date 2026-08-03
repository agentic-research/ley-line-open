//! First-party execution/v1 CLI client.
//!
//! This command speaks the daemon's UDS JSON protocol. It never invokes
//! krunvm, Taskfile, or a repository helper; backend ownership stays in LLO.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

async fn call(control: &Path, request: Value) -> Result<()> {
    let stream = UnixStream::connect(control)
        .await
        .with_context(|| format!("connect execution daemon socket {}", control.display()))?;
    let (reader, mut writer) = stream.into_split();
    writer.write_all(request.to_string().as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.shutdown().await?;
    let mut lines = BufReader::new(reader).lines();
    let Some(response) = lines.next_line().await? else {
        bail!("execution daemon closed the connection without a response");
    };
    println!("{response}");
    Ok(())
}

pub async fn capabilities(control: &Path) -> Result<()> {
    call(control, json!({"op": "llo_execution_capabilities"})).await
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
