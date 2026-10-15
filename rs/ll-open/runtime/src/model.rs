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
    /// The RunGrant's `workspaces`, carried from authorization: the Graph
    /// authority the run holds. The microVM tier attaches the one it is
    /// granted as the guest's `/work` (ADR-0044 V5); a resolver carries
    /// them and never chooses them, and the service refuses one that does.
    #[serde(default)]
    pub workspaces: Vec<WorkspaceGrant>,
}

/// One RunGrant `WorkspaceGrant` (`execution/v1`): resolved authority over one
/// logical Graph workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceGrant {
    /// The `RunSpec.workspaceInputs` name this grant resolves.
    pub name: String,
    /// The authorized input Graph root, `blake3-256:<hex>`.
    pub graph_root: String,
    /// Carried and not checked. ADR-0041 S1 defines it as the commit `seq`
    /// within the grant's epoch, which no store implements yet; a share is
    /// pinned by `graph_root` instead (it opens only when the store's
    /// `treeRoot` is the grant's `graphRoot`, ADR-0044 phase C).
    pub expected_generation: u64,
    /// Exactly the operations permitted.
    pub operations: Vec<WorkspaceOperation>,
}

/// An operation a [`WorkspaceGrant`] permits (`execution/v1`
/// `WorkspaceOperation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkspaceOperation {
    Read,
    List,
    Query,
    Mutate,
    Commit,
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
    /// Every channel the run's worker holds beyond its standard streams,
    /// verified against what the worker declared at readiness. The receipt's
    /// backend evidence names them ([`BackendEvidenceDocument`]).
    pub channels: Vec<WorkerChannel>,
}

/// A channel a backend's worker holds outside its confinement manifest.
///
/// `confinement/v1` describes what the confined process may open: paths,
/// hosts, a listener, UNIX socket paths. A descriptor the process inherits
/// already open is none of those, and no sandbox profile governs it, so it
/// is declared here instead: the worker declares each one at readiness, the
/// backend checks the declaration against what it handed the worker, and the
/// receipt's backend evidence names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum WorkerChannel {
    /// The run's workspace share (ADR-0044 phase C): one end of a connected
    /// UNIX socket pair whose peer, in the daemon, answers the guest's FUSE
    /// requests for `/work` from the store, scoped by the grant.
    WorkspaceShare {
        /// The granted workspace's name.
        workspace: String,
        /// The granted workspace's `graphRoot`, `blake3-256:<hex>`.
        #[serde(rename = "graphRoot")]
        graph_root: String,
    },
}

/// The document a receipt's `backend.evidence` digests: which backend ran
/// the workload and the channels its worker held beyond the confinement
/// manifest (execution/v1 README, "Backend evidence").
///
/// Every field is a value an issuer knows from its own grant, so a verifier
/// recomputes the document and compares digests; it names no descriptor
/// number or host path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendEvidenceDocument {
    pub backend_id: String,
    pub channels: Vec<WorkerChannel>,
}

impl BackendEvidenceDocument {
    /// The media type of the receipt's `backend.evidence`.
    pub const MEDIA_TYPE: &'static str = "application/vnd.leyline.execution.backend-evidence+json";
    /// The contract identifier the document carries.
    pub const SCHEMA_VERSION: &'static str = "leyline/execution-backend-evidence/v1";

    /// The canonical bytes: compact JSON, object keys in ASCII byte order at
    /// every level, no trailing newline, with `schemaVersion` added.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut value = serde_json::to_value(self).expect("the document serializes");
        value["schemaVersion"] = Self::SCHEMA_VERSION.into();
        serde_json::to_vec(&sorted(value)).expect("a JSON value serializes")
    }

    /// `blake3-256:<hex>` over [`Self::canonical_bytes`].
    pub fn digest(&self) -> String {
        format!(
            "blake3-256:{}",
            blake3::hash(&self.canonical_bytes()).to_hex()
        )
    }
}

/// `value` with every object's keys in ASCII byte order. `serde_json` keeps
/// insertion order in this workspace (`preserve_order`), so the order is
/// rebuilt from a `BTreeMap` at each level.
fn sorted(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, sorted(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(sorted).collect())
        }
        other => other,
    }
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
    /// The document `backend.evidence` digests: the backend and every
    /// channel its worker held beyond the confinement manifest.
    pub backend_evidence: BackendEvidenceDocument,
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

#[cfg(test)]
mod tests {
    use super::{BackendEvidenceDocument, WorkerChannel};

    /// The canonical bytes the execution/v1 README's "Backend evidence"
    /// section specifies, pinned: keys in ASCII order at every level
    /// (`role`, the serde tag, sorts between `graphRoot` and `workspace`),
    /// compact, no trailing newline.
    #[test]
    fn backend_evidence_canonical_bytes_are_pinned() {
        let document = BackendEvidenceDocument {
            backend_id: "libkrun/1".into(),
            channels: vec![WorkerChannel::WorkspaceShare {
                workspace: "repo".into(),
                graph_root: format!("blake3-256:{}", "e".repeat(64)),
            }],
        };
        let expected = format!(
            "{{\"backendId\":\"libkrun/1\",\"channels\":[{{\"graphRoot\":\"blake3-256:{}\",\
             \"role\":\"workspaceShare\",\"workspace\":\"repo\"}}],\
             \"schemaVersion\":\"leyline/execution-backend-evidence/v1\"}}",
            "e".repeat(64)
        );
        assert_eq!(
            String::from_utf8(document.canonical_bytes()).unwrap(),
            expected
        );
        assert_eq!(
            document.digest(),
            format!("blake3-256:{}", blake3::hash(expected.as_bytes()).to_hex())
        );

        let none = BackendEvidenceDocument {
            backend_id: "libkrun/1".into(),
            channels: Vec::new(),
        };
        assert_eq!(
            String::from_utf8(none.canonical_bytes()).unwrap(),
            "{\"backendId\":\"libkrun/1\",\"channels\":[],\
             \"schemaVersion\":\"leyline/execution-backend-evidence/v1\"}"
        );
    }
}
