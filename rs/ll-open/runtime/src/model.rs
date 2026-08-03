use std::collections::BTreeMap;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

use crate::ExecutionError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BackendClass {
    Native,
    MicroVm,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendCapabilities {
    pub backend_id: String,
    pub backend_class: BackendClass,
    pub available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DigestRef {
    pub algorithm: String,
    pub value: String,
}

impl DigestRef {
    pub(crate) fn validate_blake3(&self) -> Result<(), ExecutionError> {
        if self.algorithm != "blake3-256" {
            return Err(ExecutionError::invalid(
                "rootfs digest algorithm must be blake3-256",
            ));
        }
        if self.value.len() != 64
            || !self
                .value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(ExecutionError::invalid(
                "rootfs digest must be 64 lowercase hexadecimal characters",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceLimits {
    pub vcpus: u8,
    pub memory_mib: u32,
    pub wall_time_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionRequest {
    pub run_id: String,
    pub replay_key: String,
    pub rootfs: DigestRef,
    pub executable: String,
    pub arguments: Vec<String>,
    pub public_environment: BTreeMap<String, String>,
    pub allowed_egress: Vec<String>,
    pub limits: ResourceLimits,
}

impl ExecutionRequest {
    pub(crate) fn validate(&self) -> Result<(), ExecutionError> {
        if self.run_id.is_empty() {
            return Err(ExecutionError::invalid("run_id must not be empty"));
        }
        if self.replay_key.is_empty() {
            return Err(ExecutionError::invalid("replay_key must not be empty"));
        }
        self.rootfs.validate_blake3()?;
        validate_guest_path(&self.executable)?;
        if self.limits.vcpus == 0 || self.limits.memory_mib == 0 || self.limits.wall_time_ms == 0 {
            return Err(ExecutionError::invalid(
                "vcpus, memory_mib, and wall_time_ms must be non-zero",
            ));
        }
        if !self.allowed_egress.is_empty() {
            return Err(ExecutionError::unsupported(
                "the embedded libkrun backend does not yet support egress grants",
            ));
        }
        Ok(())
    }
}

fn validate_guest_path(value: &str) -> Result<(), ExecutionError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(ExecutionError::invalid(
            "executable must be a non-empty guest-relative path without traversal",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendRun {
    pub backend_id: String,
}

/// Terminal result observed by a backend supervisor.  The service projects
/// this into the shared execution/v1 lifecycle and receipt stream instead of
/// leaving a run permanently `Running` after its worker exits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendRunStatus {
    Succeeded,
    Failed(ExecutionError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RunState {
    Accepted,
    Provisioning,
    Ready,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Cleaning,
    Cleaned,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunEventRecord {
    pub sequence: u64,
    pub state: RunState,
    pub timestamp_ms: u64,
    /// Content-addressed diagnostic detail for terminal failures.
    /// The detail itself is kept out of the lifecycle stream; consumers can
    /// resolve the digest through the attested evidence store.
    pub detail_digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunInspection {
    pub run_id: String,
    pub state: RunState,
    pub events: Vec<RunEventRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiptContext {
    pub run_spec_digest: String,
    pub run_grant_digest: String,
    pub confinement_digest: String,
    pub backend_class: BackendClass,
    pub input_roots: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunReceiptData {
    pub run_id: String,
    pub terminal_state: RunState,
    pub event_log_root: String,
    pub context: ReceiptContext,
    pub backend_id: String,
    pub started_at_unix_ms: u64,
    pub completed_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRecord {
    pub run_id: String,
    pub replay_key: String,
    pub state: RunState,
    pub backend_id: String,
}
