//! First-party execution/v1 CLI client.
//!
//! This command speaks the daemon's UDS JSON protocol. It never invokes
//! krunvm, Taskfile, or a repository helper; backend ownership stays in LLO.
//!
//! Each `leyline execution` subcommand is one daemon request
//! ([`request`]), sent by [`respond`] and printed by [`run`]. The split is
//! what lets `tests/execution_transport_parity.rs` drive the exact request a
//! command line produces and compare its response with the Rust, daemon, and
//! MCP surfaces' (bead `ley-line-open-f8ebcf`).

use std::path::Path;

use crate::ExecutionCommands;
use crate::daemon::client::ExecutionClient;
use crate::daemon::execution_contract as contract;
use anyhow::{Context, Result};
use serde_json::Value;

/// The control socket `command` names and the daemon request it sends.
/// Reads `start`'s spec and grant files; touches nothing else.
pub fn request(command: &ExecutionCommands) -> Result<(&Path, Value)> {
    Ok(match command {
        ExecutionCommands::Capabilities { control } => (control, contract::capabilities()),
        ExecutionCommands::Provision {
            control,
            backend_class,
            idempotency_key,
        } => (control, contract::provision(backend_class, idempotency_key)),
        ExecutionCommands::Status { control, run_id } => {
            (control, contract::status(run_id.as_deref()))
        }
        ExecutionCommands::Inspect {
            control,
            run_id,
            after_sequence,
        } => (control, contract::inspect(run_id, *after_sequence)),
        ExecutionCommands::Collect { control, run_id } => (control, contract::collect(run_id)),
        ExecutionCommands::Cleanup {
            control,
            run_id,
            idempotency_key,
        } => (
            control,
            contract::cleanup(run_id, idempotency_key.as_deref()),
        ),
        ExecutionCommands::Start {
            control,
            spec,
            grant,
        } => (
            control,
            contract::start(read_json(spec, "spec")?, read_json(grant, "grant")?),
        ),
        ExecutionCommands::Cancel {
            control,
            run_id,
            idempotency_key,
        } => (
            control,
            contract::cancel(run_id, idempotency_key.as_deref()),
        ),
    })
}

fn read_json(path: &Path, what: &str) -> Result<Value> {
    serde_json::from_slice(
        &std::fs::read(path).with_context(|| format!("read {what} {}", path.display()))?,
    )
    .with_context(|| format!("parse {what} JSON"))
}

/// Send `command`'s request to its daemon and return the response: exactly
/// what [`run`] prints.
pub async fn respond(command: &ExecutionCommands) -> Result<Value> {
    let (control, request) = request(command)?;
    ExecutionClient::new(control).call(request).await
}

/// Run one `leyline execution` subcommand: print the daemon's response as
/// one line of JSON.
pub async fn run(command: &ExecutionCommands) -> Result<()> {
    let response = respond(command).await?;
    println!("{}", serde_json::to_string(&response)?);
    Ok(())
}
