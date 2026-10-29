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
    /// True only when the backend can start a run on this host: for the
    /// microVM tier, that it can enter the hypervisor (bead
    /// ley-line-open-831bf1), not merely that its files exist.
    pub available: bool,
    /// Why `available` is false, in words an operator can act on; `None`
    /// when the backend is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
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
    /// A cgroup v2 directory the run's whole process tree is placed in before
    /// it executes (the native tier on Linux, with a delegated root).
    Cgroup,
}

impl CeilingMechanism {
    pub const fn is_enforced(self) -> bool {
        !matches!(self, Self::Unenforced)
    }
}

/// The per-ceiling enforcement declaration for one backend: one entry per
/// [`ResourceLimits`] field (ADR-0035, "OCI resources on execution/v1").
///
/// This exists because the tiers genuinely differ: libkrun applies vCPU and
/// memory ceilings through `krun_set_vm_config` before the guest starts; the
/// native tier on Linux applies memory, CPU bandwidth and pids through a
/// delegated cgroup v2 directory when the operator names one; and the native
/// tier on macOS has no mechanism with the right semantics for any of them.
/// A ceiling accepted where nothing applies it turns the grant into a
/// suggestion and makes the receipt's attestation false.
///
/// Never derive `Default`: every construction site is a full struct literal,
/// so a new field is a compile error at each tier rather than a silent
/// `Unenforced` (see `ConfinementManifest::dimensions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnforcedCeilings {
    pub wall_time: CeilingMechanism,
    pub output_bytes: CeilingMechanism,
    pub memory: CeilingMechanism,
    pub cpu: CeilingMechanism,
    pub pids: CeilingMechanism,
}

/// cgroup v2's default `cpu.max` period, which a zero `cpu.period` means.
pub const DEFAULT_CPU_PERIOD_US: u64 = 100_000;

impl EnforcedCeilings {
    /// A tier whose only ceiling is the supervisor's wall clock — the native
    /// tier on macOS, and on Linux without a delegated cgroup root.
    pub const fn wall_clock_only() -> Self {
        Self {
            wall_time: CeilingMechanism::Supervisor,
            output_bytes: CeilingMechanism::Unenforced,
            memory: CeilingMechanism::Unenforced,
            cpu: CeilingMechanism::Unenforced,
            pids: CeilingMechanism::Unenforced,
        }
    }

    /// A tier that additionally applies vCPU and memory ceilings at VM
    /// configuration time — the libkrun profile. Nothing writes the guest's
    /// cgroups, so `pids` is not enforced.
    pub const fn hypervisor_backed() -> Self {
        Self {
            wall_time: CeilingMechanism::Supervisor,
            output_bytes: CeilingMechanism::Unenforced,
            memory: CeilingMechanism::Hypervisor,
            cpu: CeilingMechanism::Hypervisor,
            pids: CeilingMechanism::Unenforced,
        }
    }

    /// Reject any ceiling `limits` requests that this backend cannot apply,
    /// naming the field (ADR-0035 §4).
    ///
    /// Zero means "no ceiling" for every field, so only a non-zero request can
    /// be unenforceable. A mechanism that enforces a dimension may still be
    /// unable to express a particular value — a hypervisor sizes whole vCPUs
    /// and whole MiB, a cgroup has a period range — and that is refused here
    /// too, rather than rounded: rounding up widens the grant, and rounding
    /// down applies a ceiling nobody authorized.
    pub fn check(&self, limits: &ResourceLimits) -> Result<(), ExecutionError> {
        limits.validate()?;
        // Destructured with no `..`: a new ResourceLimits field is a compile
        // error here until this check decides what enforces it.
        let ResourceLimits {
            wall_time_ms,
            output_bytes,
            memory: MemoryLimits {
                limit: memory_limit,
            },
            cpu: CpuLimits { quota, period: _ },
            pids: PidsLimits { limit: pids_limit },
        } = *limits;
        for (name, requested, mechanism) in [
            ("wallTimeMs", wall_time_ms, self.wall_time),
            ("outputBytes", output_bytes, self.output_bytes),
            ("memory.limit", memory_limit, self.memory),
            ("cpu.quota", quota, self.cpu),
            ("pids.limit", pids_limit, self.pids),
        ] {
            if requested != 0 && !mechanism.is_enforced() {
                return Err(ExecutionError::unsupported(format!(
                    "grant limit {name} cannot be enforced by the selected backend"
                )));
            }
        }
        if self.memory == CeilingMechanism::Hypervisor {
            limits.memory.hypervisor_ram_mib()?;
        }
        match self.cpu {
            CeilingMechanism::Hypervisor => {
                limits.cpu.hypervisor_vcpus()?;
            }
            CeilingMechanism::Cgroup => {
                limits.cpu.cgroup_max()?;
            }
            CeilingMechanism::Unenforced | CeilingMechanism::Supervisor => {}
        }
        Ok(())
    }
}

/// OCI `linux.resources.memory`: the subset LLO enforces.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryLimits {
    /// OCI `memory.limit`, in bytes. Zero means no ceiling.
    pub limit: u64,
}

impl MemoryLimits {
    /// The hypervisor's RAM size for this ceiling, in whole MiB.
    ///
    /// `None` when no ceiling is requested; refused when the ceiling is not a
    /// whole MiB or does not fit `krun_set_vm_config`'s `u32`.
    pub fn hypervisor_ram_mib(&self) -> Result<Option<u32>, ExecutionError> {
        const MIB: u64 = 1024 * 1024;
        if self.limit == 0 {
            return Ok(None);
        }
        if !self.limit.is_multiple_of(MIB) {
            return Err(ExecutionError::unsupported(
                "grant limit memory.limit is not a whole MiB, which is the unit the hypervisor sizes the guest in",
            ));
        }
        u32::try_from(self.limit / MIB).map(Some).map_err(|_| {
            ExecutionError::unsupported(
                "grant limit memory.limit exceeds what the hypervisor can size",
            )
        })
    }
}

/// OCI `linux.resources.cpu` bandwidth: at most `quota` microseconds of CPU in
/// each `period` microseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CpuLimits {
    /// OCI `cpu.quota`, in microseconds per period. Zero means no ceiling.
    pub quota: u64,
    /// OCI `cpu.period`, in microseconds. Zero means
    /// [`DEFAULT_CPU_PERIOD_US`].
    pub period: u64,
}

impl CpuLimits {
    /// The period this ceiling is measured over.
    pub const fn effective_period(&self) -> u64 {
        if self.period == 0 {
            DEFAULT_CPU_PERIOD_US
        } else {
            self.period
        }
    }

    /// The hypervisor's vCPU count for this ceiling.
    ///
    /// `None` when no ceiling is requested; refused unless the quota is a
    /// whole multiple of the period between 1 and 255 vCPUs (`krun_set_vm_config`
    /// takes a `u8`).
    pub fn hypervisor_vcpus(&self) -> Result<Option<u8>, ExecutionError> {
        if self.quota == 0 {
            return Ok(None);
        }
        let period = self.effective_period();
        if !self.quota.is_multiple_of(period) {
            return Err(ExecutionError::unsupported(
                "grant limit cpu.quota is not a whole multiple of cpu.period, and the hypervisor sizes whole vCPUs",
            ));
        }
        // A non-zero quota that is a whole multiple of the period is at least
        // one period, so the quotient is never zero here.
        u8::try_from(self.quota / period).map(Some).map_err(|_| {
            ExecutionError::unsupported(
                "grant limit cpu.quota exceeds what the hypervisor can size",
            )
        })
    }

    /// The cgroup v2 `cpu.max` line for this ceiling, `"$quota $period"`.
    ///
    /// `None` when no ceiling is requested; refused outside the kernel's
    /// ranges (period 1000..=1000000 µs, quota at least 1000 µs), which the
    /// kernel would otherwise reject at write time with a bare `EINVAL`.
    pub fn cgroup_max(&self) -> Result<Option<String>, ExecutionError> {
        if self.quota == 0 {
            return Ok(None);
        }
        let period = self.effective_period();
        if !(1_000..=1_000_000).contains(&period) {
            return Err(ExecutionError::unsupported(
                "grant limit cpu.period is outside cgroup v2's 1000..=1000000 microseconds",
            ));
        }
        if self.quota < 1_000 {
            return Err(ExecutionError::unsupported(
                "grant limit cpu.quota is below cgroup v2's 1000-microsecond minimum",
            ));
        }
        Ok(Some(format!("{} {period}", self.quota)))
    }
}

/// OCI `linux.resources.pids`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PidsLimits {
    /// OCI `pids.limit`: maximum tasks in the run's process tree. Zero means
    /// no ceiling.
    pub limit: u64,
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

/// execution/v1 `ResourceLimits`: OCI `linux.resources` memory, cpu and pids,
/// plus the LLO-only wall clock and output size (ADR-0035, "OCI resources on
/// execution/v1"). Zero means "no ceiling" for every field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceLimits {
    /// Maximum wall-clock runtime in milliseconds.
    pub wall_time_ms: u64,
    /// Maximum collected output bytes. No tier enforces it yet, so a non-zero
    /// value is refused everywhere.
    pub output_bytes: u64,
    pub memory: MemoryLimits,
    pub cpu: CpuLimits,
    pub pids: PidsLimits,
}

impl ResourceLimits {
    /// Refuse a combination that names no ceiling while looking like one.
    ///
    /// A `cpu.period` without a `cpu.quota` is the one: it bounds nothing, and
    /// ignoring it would be silently accepting a field.
    pub fn validate(&self) -> Result<(), ExecutionError> {
        if self.cpu.quota == 0 && self.cpu.period != 0 {
            return Err(ExecutionError::invalid(
                "cpu.period without cpu.quota names no ceiling",
            ));
        }
        Ok(())
    }

    /// Whether `self` is no wider than `requested` on every dimension the
    /// request bounds. A request of zero bounds nothing; a non-zero request
    /// requires a non-zero ceiling no larger than it. CPU compares bandwidth,
    /// `quota / period`, so a grant may restate a ceiling over another period.
    pub fn narrows(&self, requested: &Self) -> Result<(), ExecutionError> {
        let ResourceLimits {
            wall_time_ms,
            output_bytes,
            memory,
            cpu,
            pids,
        } = *self;
        for (name, requested, resolved) in [
            ("wallTimeMs", requested.wall_time_ms, wall_time_ms),
            ("outputBytes", requested.output_bytes, output_bytes),
            ("memory.limit", requested.memory.limit, memory.limit),
            ("pids.limit", requested.pids.limit, pids.limit),
        ] {
            if requested != 0 && (resolved == 0 || resolved > requested) {
                return Err(ExecutionError::invalid(format!(
                    "grant limit {name} widens the requested limit"
                )));
            }
        }
        if requested.cpu.quota != 0 {
            // resolved.quota / resolved.period <= requested.quota / requested.period,
            // cross-multiplied in u128 so no product overflows.
            let widens = cpu.quota == 0
                || u128::from(cpu.quota) * u128::from(requested.cpu.effective_period())
                    > u128::from(requested.cpu.quota) * u128::from(cpu.effective_period());
            if widens {
                return Err(ExecutionError::invalid(
                    "grant limit cpu.quota widens the requested limit",
                ));
            }
        }
        Ok(())
    }
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
        // The wall clock is the one ceiling every tier enforces, and the
        // supervisor needs a deadline. Memory, CPU and pids may be absent: a
        // tier that requires one (the microVM sizes its guest from memory and
        // CPU) refuses its absence itself.
        if self.limits.wall_time_ms == 0 {
            return Err(ExecutionError::invalid("wall_time_ms must be non-zero"));
        }
        self.limits.validate()?;
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
        use leyline_core::ContentAddressed;
        format!("blake3-256:{}", self.canonical_bytes().hash())
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
