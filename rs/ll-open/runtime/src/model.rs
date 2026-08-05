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
    /// Which resource ceilings this backend actually applies, and by what
    /// mechanism. See [`EnforcedCeilings`].
    pub enforced: EnforcedCeilings,
}

/// How a backend applies one resource ceiling — or that it does not.
///
/// ADR-0035 §3: `memory: 512MiB` means three different things under a
/// hypervisor, a cgroup, and `RLIMIT_AS`, so a receipt that records only the
/// number attests less than it appears to. Naming the mechanism is what makes
/// the ceiling's meaning recoverable from the receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CeilingMechanism {
    /// This tier does not apply the ceiling at all. A grant that requests one
    /// is rejected rather than silently accepted (ADR-0035 §4).
    Unenforced,
    /// The supervising process applies it — a wall-clock deadline it observes
    /// and acts on.
    Supervisor,
    /// The hypervisor applies it at VM configuration time, before the guest
    /// runs.
    Hypervisor,
}

impl CeilingMechanism {
    pub const fn is_enforced(self) -> bool {
        !matches!(self, Self::Unenforced)
    }
}

/// The per-ceiling enforcement declaration for one backend.
///
/// This exists because the two tiers genuinely differ: libkrun applies vCPU
/// and memory ceilings through `krun_set_vm_config` before the guest starts,
/// while the native tier's confinement is nono, which mediates capabilities
/// rather than resources — its `resources` block is enforced by nono's own
/// supervised CLI runner, not by the library LLO links. A ceiling accepted
/// where nothing applies it turns the grant into a suggestion and makes the
/// receipt's attestation false.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnforcedCeilings {
    pub wall_time: CeilingMechanism,
    pub vcpus: CeilingMechanism,
    pub memory: CeilingMechanism,
}

impl EnforcedCeilings {
    /// A tier whose only ceiling is the supervisor's wall clock — the native
    /// / nono profile.
    pub const fn wall_clock_only() -> Self {
        Self {
            wall_time: CeilingMechanism::Supervisor,
            vcpus: CeilingMechanism::Unenforced,
            memory: CeilingMechanism::Unenforced,
        }
    }

    /// A tier that additionally applies vCPU and memory ceilings at VM
    /// configuration time — the libkrun profile.
    pub const fn hypervisor_backed() -> Self {
        Self {
            wall_time: CeilingMechanism::Supervisor,
            vcpus: CeilingMechanism::Hypervisor,
            memory: CeilingMechanism::Hypervisor,
        }
    }

    /// Reject any ceiling `limits` requests that this backend cannot apply.
    ///
    /// Zero means "policy default" throughout execution/v1, so only a
    /// non-zero request can be unenforceable.
    pub(crate) fn check(&self, limits: &ResourceLimits) -> Result<(), ExecutionError> {
        for (name, requested, mechanism) in [
            ("wallTimeMs", limits.wall_time_ms, self.wall_time),
            ("vcpus", u64::from(limits.vcpus), self.vcpus),
            ("memoryBytes", u64::from(limits.memory_mib), self.memory),
        ] {
            if requested != 0 && !mechanism.is_enforced() {
                return Err(ExecutionError::unsupported(format!(
                    "grant limit {name} cannot be enforced by the selected backend"
                )));
            }
        }
        Ok(())
    }
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
    /// The `confinementDigest` the grant authorized, carried through so the
    /// supervisor can check the worker's attestation against it.
    ///
    /// Empty means the resolver declared none, and the worker's attestation
    /// is then not checked — a state the shipped resolver never produces, and
    /// which exists only so an embedder-supplied resolver fails loudly at its
    /// own boundary rather than silently here.
    #[serde(default)]
    pub confinement_digest: String,
    /// The `confinement/v1` document `confinement_digest` was taken over, when
    /// the grant carried one.
    ///
    /// Carried for the same reason the digest is — the worker needs it, and the
    /// resolver must not substitute its own. `authorization.rs` has already
    /// parsed it and refused any grant whose document does not digest to
    /// `confinement_digest`, so a value here is authorized, not caller intent.
    /// That distinction is what makes it a legitimate source for a dimension
    /// the plan refuses to take from the request: `plan.rs` declines to derive
    /// a listener from an `ExecutionRequest` because "a workload does not get
    /// to widen its own boundary", and names the manifest the grant authorized
    /// as where one must come from. This is that manifest.
    ///
    /// `None` means the grant carried no document. The worker then compiles its
    /// own policy exactly as before, so a run that worked without this field
    /// still does.
    #[serde(default)]
    pub confinement_manifest: Option<String>,
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
                "the configured execution backend does not yet support egress grants",
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
    /// What the backend actually applied, per ceiling (ADR-0035 §3). The
    /// receipt records the mechanism because the number alone does not carry
    /// its own meaning across tiers.
    pub enforced: EnforcedCeilings,
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
