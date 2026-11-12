//! The typed, resolved configuration of each kind, and its canonical value.
//!
//! Every path here is resolved: absolute, `~/` expanded, and clean (D5).
//! Set lists are sorted in byte order and hold no duplicate. A struct is
//! built only by resolution, so its invariants hold for every value a caller
//! sees.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};

use super::{API_VERSION, Kind, Value};

/// A resolved document of one kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Document {
    /// `DaemonConfig`.
    Daemon(DaemonConfig),
    /// `ExecutionDaemonConfig`.
    ExecutionDaemon(ExecutionDaemonConfig),
    /// `SignHelperConfig`.
    SignHelper(SignHelperConfig),
}

impl Document {
    /// The document's kind.
    pub fn kind(&self) -> Kind {
        match self {
            Document::Daemon(_) => Kind::Daemon,
            Document::ExecutionDaemon(_) => Kind::ExecutionDaemon,
            Document::SignHelper(_) => Kind::SignHelper,
        }
    }

    /// The resolved document as a value: every key present, defaults filled.
    pub fn to_value(&self) -> Value {
        let (kind, mut members) = match self {
            Document::Daemon(d) => (Kind::Daemon, d.members()),
            Document::ExecutionDaemon(d) => (Kind::ExecutionDaemon, d.members()),
            Document::SignHelper(d) => (Kind::SignHelper, d.members()),
        };
        members.insert("apiVersion".into(), Value::str(API_VERSION));
        members.insert("kind".into(), Value::str(kind.name()));
        Value::Object(members)
    }

    /// The `parser`, `head` and `diagnostics` sections of a daemon kind, which
    /// `parse`, `verify-head` and `rebuild` read (ADR-0049 D1). `None` for the
    /// sign helper.
    pub fn shared_sections(&self) -> Option<(&ParserSettings, &HeadSettings, &Diagnostics)> {
        match self {
            Document::Daemon(d) => Some((&d.parser, &d.head, &d.diagnostics)),
            Document::ExecutionDaemon(d) => Some((&d.parser, &d.head, &d.diagnostics)),
            Document::SignHelper(_) => None,
        }
    }
}

/// A reference to a secret (D4). v1 delivers one variant, a file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SecretRef {
    /// The secret file's resolved path. Read at start through
    /// [`super::files::read_secret_file`]; never copied into the document.
    pub file: String,
}

impl SecretRef {
    fn value(&self) -> Value {
        Value::object([("file", Value::str(&self.file))])
    }
}

/// `source`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSettings {
    /// The source tree.
    pub path: String,
    /// The language filter; `None` detects per file.
    pub language: Option<String>,
}

impl SourceSettings {
    fn value(&self) -> Value {
        Value::object([
            ("path", Value::str(&self.path)),
            ("language", Value::opt(self.language.as_ref(), Value::str)),
        ])
    }
}

/// `parser`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParserSettings {
    /// Sorted, distinct BLAKE3 hex digests.
    pub trusted_query_hashes: Vec<String>,
    /// 1-65536.
    pub query_match_limit: u32,
    /// 1 or more.
    pub query_progress_budget: u64,
}

impl ParserSettings {
    fn value(&self) -> Value {
        Value::object([
            (
                "trustedQueryHashes",
                Value::strs(self.trusted_query_hashes.iter().cloned()),
            ),
            ("queryMatchLimit", Value::Int(self.query_match_limit.into())),
            ("queryProgressBudget", Value::Int(self.query_progress_budget)),
        ])
    }
}

/// `head`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadSettings {
    /// The Ed25519 seed file; `None` writes unsigned heads.
    pub signing_key: Option<SecretRef>,
    /// Sorted, distinct public key files.
    pub trusted_keys: Vec<String>,
    /// Refuse an unsigned head.
    pub require_signature: bool,
}

impl HeadSettings {
    fn value(&self) -> Value {
        Value::object([
            ("signingKey", Value::opt(self.signing_key.as_ref(), SecretRef::value)),
            ("trustedKeys", Value::strs(self.trusted_keys.iter().cloned())),
            ("requireSignature", Value::Bool(self.require_signature)),
        ])
    }
}

/// `diagnostics`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostics {
    /// Log per-phase timings.
    pub profile: bool,
    /// Append sheaf ablation records here.
    pub sheaf_ablation_log: Option<String>,
}

impl Diagnostics {
    fn value(&self) -> Value {
        Value::object([
            ("profile", Value::Bool(self.profile)),
            (
                "sheafAblationLog",
                Value::opt(self.sheaf_ablation_log.as_ref(), Value::str),
            ),
        ])
    }
}

/// `enrichment`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrichment {
    /// The session corpus; `None` and the pass does nothing.
    pub session_corpus: Option<String>,
}

impl Enrichment {
    fn value(&self) -> Value {
        Value::object([(
            "sessionCorpus",
            Value::opt(self.session_corpus.as_ref(), Value::str),
        )])
    }
}

/// `DaemonConfig`: `leyline daemon`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonConfig {
    /// `arena.path`.
    pub arena_path: String,
    /// `arena.sizeMib`.
    pub arena_size_mib: u64,
    /// `control`.
    pub control: String,
    /// `mount`; `None` runs headless.
    pub mount: Option<MountSettings>,
    /// `source`.
    pub source: Option<SourceSettings>,
    /// `shutdownAfterSeconds`; `None` never shuts down on its own.
    pub shutdown_after_seconds: Option<u64>,
    /// `mcp`.
    pub mcp: McpSettings,
    /// `cdc.enabled` (`cdc.target` is always `source-blobs`).
    pub cdc_enabled: bool,
    /// `parser`.
    pub parser: ParserSettings,
    /// `head`.
    pub head: HeadSettings,
    /// `diagnostics`.
    pub diagnostics: Diagnostics,
    /// `enrichment`.
    pub enrichment: Enrichment,
}

/// The one CDC target v1 has.
pub const CDC_TARGET_SOURCE_BLOBS: &str = "source-blobs";

impl DaemonConfig {
    fn members(&self) -> BTreeMap<String, Value> {
        let Value::Object(members) = Value::object([
            (
                "arena",
                Value::object([
                    ("path", Value::str(&self.arena_path)),
                    ("sizeMib", Value::Int(self.arena_size_mib)),
                ]),
            ),
            ("control", Value::str(&self.control)),
            ("mount", Value::opt(self.mount.as_ref(), MountSettings::value)),
            ("source", Value::opt(self.source.as_ref(), SourceSettings::value)),
            (
                "shutdownAfterSeconds",
                Value::opt(self.shutdown_after_seconds, Value::Int),
            ),
            ("mcp", self.mcp.value()),
            (
                "cdc",
                Value::object([
                    ("enabled", Value::Bool(self.cdc_enabled)),
                    ("target", Value::str(CDC_TARGET_SOURCE_BLOBS)),
                ]),
            ),
            ("parser", self.parser.value()),
            ("head", self.head.value()),
            ("diagnostics", self.diagnostics.value()),
            ("enrichment", self.enrichment.value()),
        ]) else {
            unreachable!("Value::object builds an object")
        };
        members
    }

    /// Each field set to its loosening value, as `(field, notice)` (D5,
    /// "Loosening is visible").
    pub fn loosenings(&self) -> Vec<(&'static str, &'static str)> {
        let mut out = Vec::new();
        if self.mcp.no_auth {
            out.push(("mcp.noAuth", "the ADR-0022 token gate is off on every MCP listener"));
        }
        if self.mcp.http.as_ref().is_some_and(|h| h.allow_public) {
            out.push((
                "mcp.http.allowPublic",
                "the MCP HTTP listener may bind a non-loopback address",
            ));
        }
        out
    }
}

/// The filesystem a mount is served by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountBackend {
    /// The host's NFS client.
    Nfs,
    /// FUSE (a `fuse` build).
    Fuse,
}

impl MountBackend {
    /// `nfs` or `fuse`.
    pub fn name(self) -> &'static str {
        match self {
            MountBackend::Nfs => "nfs",
            MountBackend::Fuse => "fuse",
        }
    }
}

/// `mount`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountSettings {
    /// The mount point.
    pub path: String,
    /// The backend.
    pub backend: MountBackend,
    /// The NFS port; 0 lets the kernel pick.
    pub nfs_port: u16,
}

impl MountSettings {
    fn value(&self) -> Value {
        Value::object([
            ("path", Value::str(&self.path)),
            ("backend", Value::str(self.backend.name())),
            ("nfsPort", Value::Int(self.nfs_port.into())),
        ])
    }
}

/// `mcp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSettings {
    /// `mcp.http`; `None`: no HTTP listener.
    pub http: Option<McpHttp>,
    /// `mcp.noAuth`.
    pub no_auth: bool,
    /// `mcp.uds`; `None`: no UDS listener.
    pub uds: Option<String>,
    /// `mcp.groups`, sorted and distinct; `None` grants every group.
    pub groups: Option<Vec<String>>,
    /// `mcp.tokenFile`.
    pub token_file: SecretRef,
}

impl McpSettings {
    fn value(&self) -> Value {
        Value::object([
            ("http", Value::opt(self.http.as_ref(), McpHttp::value)),
            ("noAuth", Value::Bool(self.no_auth)),
            ("uds", Value::opt(self.uds.as_ref(), Value::str)),
            (
                "groups",
                Value::opt(self.groups.as_ref(), |g| Value::strs(g.iter().cloned())),
            ),
            ("tokenFile", self.token_file.value()),
        ])
    }
}

/// `mcp.http`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpHttp {
    /// 1-65535.
    pub port: u16,
    /// Canonical spelling.
    pub bind: IpAddr,
    /// A non-loopback bind is allowed.
    pub allow_public: bool,
}

impl McpHttp {
    fn value(&self) -> Value {
        Value::object([
            ("port", Value::Int(self.port.into())),
            ("bind", Value::str(self.bind.to_string())),
            ("allowPublic", Value::Bool(self.allow_public)),
        ])
    }
}

/// `ExecutionDaemonConfig`: `leyline execution-daemon`, and the input of the
/// `leyline-runtime` library entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionDaemonConfig {
    /// `arena.path` (the arena is 64 MiB).
    pub arena_path: String,
    /// `control`.
    pub control: String,
    /// `stateDir`.
    pub state_dir: String,
    /// `cas`.
    pub cas: CasSettings,
    /// `worker`.
    pub worker: WorkerSettings,
    /// `catalog`.
    pub catalog: String,
    /// `backend`.
    pub backend: BackendSettings,
    /// `cdc.enabled`.
    pub cdc_enabled: bool,
    /// `evidence`.
    pub evidence: EvidenceSettings,
    /// `source`.
    pub source: Option<SourceSettings>,
    /// `parser`.
    pub parser: ParserSettings,
    /// `head`.
    pub head: HeadSettings,
    /// `diagnostics`.
    pub diagnostics: Diagnostics,
    /// `enrichment`.
    pub enrichment: Enrichment,
}

/// The execution daemon's fixed arena size, in MiB.
pub const EXECUTION_ARENA_SIZE_MIB: u64 = 64;

impl ExecutionDaemonConfig {
    fn members(&self) -> BTreeMap<String, Value> {
        let Value::Object(members) = Value::object([
            ("arena", Value::object([("path", Value::str(&self.arena_path))])),
            ("control", Value::str(&self.control)),
            ("stateDir", Value::str(&self.state_dir)),
            ("cas", self.cas.value()),
            ("worker", self.worker.value()),
            ("catalog", Value::str(&self.catalog)),
            ("backend", self.backend.value()),
            (
                "cdc",
                Value::object([("enabled", Value::Bool(self.cdc_enabled))]),
            ),
            ("evidence", self.evidence.value()),
            ("source", Value::opt(self.source.as_ref(), SourceSettings::value)),
            ("parser", self.parser.value()),
            ("head", self.head.value()),
            ("diagnostics", self.diagnostics.value()),
            ("enrichment", self.enrichment.value()),
        ]) else {
            unreachable!("Value::object builds an object")
        };
        members
    }

    /// Each field set to its loosening value, as `(field, notice)`.
    pub fn loosenings(&self) -> Vec<(&'static str, &'static str)> {
        let mut out = Vec::new();
        if self.evidence == EvidenceSettings::Unverified {
            out.push((
                "evidence.unverified",
                "grants are not signature-checked and evidence is checked for metadata only",
            ));
        }
        if let BackendSettings::MicroVm {
            tsi_hijack_inet: true,
            ..
        } = self.backend
        {
            out.push((
                "backend.microVm.tsiHijackInet",
                "guest TCP sockets are carried over vsock: this weakens isolation",
            ));
        }
        out
    }

    /// The CAS root this document names: the directory's, or the volume's
    /// layout beneath `stateDir` (computed, never attached).
    pub fn cas_root(&self, os: super::Os) -> String {
        match &self.cas {
            CasSettings::Directory { root, .. } => root.clone(),
            CasSettings::Volume { .. } => volume_cas_root(&self.state_dir, os),
        }
    }

    /// Each run's ephemeral rootfs parent.
    pub fn run_root(&self) -> &str {
        match &self.cas {
            CasSettings::Directory { run_root, .. } | CasSettings::Volume { run_root, .. } => {
                run_root
            }
        }
    }
}

/// The CAS root of a volume beneath `state_dir`: `<state>/cas-volume/mnt/cas`
/// on macOS, `<state>/cas` on Linux (`leyline_runtime::cas_volume`).
pub fn volume_cas_root(state_dir: &str, os: super::Os) -> String {
    match os {
        super::Os::MacOs => format!("{state_dir}/cas-volume/mnt/cas"),
        super::Os::Linux => format!("{state_dir}/cas"),
    }
}

/// A volume's default run root: `<state>/cas-volume/mnt/runs` on macOS,
/// `<state>/runs` on Linux.
pub fn volume_default_run_root(state_dir: &str, os: super::Os) -> String {
    match os {
        super::Os::MacOs => format!("{state_dir}/cas-volume/mnt/runs"),
        super::Os::Linux => format!("{state_dir}/runs"),
    }
}

/// `cas`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CasSettings {
    /// `cas.directory`: an operator directory that must exist.
    Directory {
        /// `cas.directory.root`.
        root: String,
        /// `cas.directory.runRoot`.
        run_root: String,
    },
    /// `cas.volume`: a case-sensitive volume beneath `stateDir`.
    Volume {
        /// `cas.volume.sizeCapGib`.
        size_cap_gib: u64,
        /// `cas.volume.runRoot`, resolved (the volume's own by default).
        run_root: String,
    },
}

impl CasSettings {
    fn value(&self) -> Value {
        match self {
            CasSettings::Directory { root, run_root } => Value::object([(
                "directory",
                Value::object([
                    ("root", Value::str(root)),
                    ("runRoot", Value::str(run_root)),
                ]),
            )]),
            CasSettings::Volume {
                size_cap_gib,
                run_root,
            } => Value::object([(
                "volume",
                Value::object([
                    ("sizeCapGib", Value::Int(*size_cap_gib)),
                    ("runRoot", Value::str(run_root)),
                ]),
            )]),
        }
    }
}

/// `worker`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerSettings {
    /// `worker.path`.
    pub path: String,
    /// `worker.runtimeFiles`, sorted and distinct.
    pub runtime_files: Vec<String>,
    /// `worker.readyTimeoutMs`.
    pub ready_timeout_ms: u64,
}

impl WorkerSettings {
    fn value(&self) -> Value {
        Value::object([
            ("path", Value::str(&self.path)),
            ("runtimeFiles", Value::strs(self.runtime_files.iter().cloned())),
            ("readyTimeoutMs", Value::Int(self.ready_timeout_ms)),
        ])
    }
}

/// `backend`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendSettings {
    /// `backend.native`.
    Native {
        /// `backend.native.cgroupRoot`.
        cgroup_root: Option<String>,
    },
    /// `backend.microVm`.
    MicroVm {
        /// `backend.microVm.devices`, sorted and distinct.
        devices: Vec<String>,
        /// `backend.microVm.tsiHijackInet`.
        tsi_hijack_inet: bool,
        /// `backend.microVm.workspace`.
        workspace: WorkspaceFreshness,
    },
}

impl BackendSettings {
    fn value(&self) -> Value {
        match self {
            BackendSettings::Native { cgroup_root } => Value::object([(
                "native",
                Value::object([("cgroupRoot", Value::opt(cgroup_root.as_ref(), Value::str))]),
            )]),
            BackendSettings::MicroVm {
                devices,
                tsi_hijack_inet,
                workspace,
            } => Value::object([(
                "microVm",
                Value::object([
                    ("devices", Value::strs(devices.iter().cloned())),
                    ("tsiHijackInet", Value::Bool(*tsi_hijack_inet)),
                    (
                        "workspace",
                        Value::object([
                            ("ttlMs", Value::Int(workspace.ttl_ms)),
                            ("changedTtlMs", Value::Int(workspace.changed_ttl_ms)),
                            ("changeWindowMs", Value::Int(workspace.change_window_ms)),
                        ]),
                    ),
                ]),
            )]),
        }
    }
}

/// `backend.microVm.workspace`: ADR-0044 read freshness, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceFreshness {
    /// Timeout of a path with no recent change.
    pub ttl_ms: u64,
    /// Timeout of a recently changed path.
    pub changed_ttl_ms: u64,
    /// How long a change keeps a path's timeouts short.
    pub change_window_ms: u64,
}

/// `evidence`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceSettings {
    /// `evidence.unverified`: metadata-only, for fixtures.
    Unverified,
    /// `evidence.verified`.
    Verified {
        /// Sorted, distinct, at least one.
        trust_keys: Vec<String>,
        /// The evidence CAS directory.
        store: String,
    },
}

impl EvidenceSettings {
    fn value(&self) -> Value {
        match self {
            EvidenceSettings::Unverified => {
                Value::object([("unverified", Value::object::<_, String>([]))])
            }
            EvidenceSettings::Verified { trust_keys, store } => Value::object([(
                "verified",
                Value::object([
                    ("trustKeys", Value::strs(trust_keys.iter().cloned())),
                    ("store", Value::str(store)),
                ]),
            )]),
        }
    }
}

/// `SignHelperConfig`: `leyline-sign-helper`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignHelperConfig {
    /// `bind`: loopback, canonical.
    pub bind: SocketAddr,
    /// `rateLimitPerSecond`.
    pub rate_limit_per_second: u32,
    /// `auth.required`.
    pub auth_required: bool,
    /// `auth.callerTokens`.
    pub caller_tokens: Option<SecretRef>,
    /// `signAllow.required`.
    pub sign_allow_required: bool,
    /// `signAllow.callers`: label to sorted, distinct prefixes.
    pub sign_allow_callers: BTreeMap<String, Vec<String>>,
    /// `resolveAllow`, sorted and distinct.
    pub resolve_allow: Vec<String>,
    /// `resolveCache.maxEntries`.
    pub resolve_cache_max_entries: u64,
    /// `resolveCache.ttlMs`; `None` is per scheme.
    pub resolve_cache_ttl_ms: Option<u64>,
    /// `subprocess.opBin`.
    pub op_bin: Option<String>,
    /// `subprocess.securityBin`.
    pub security_bin: Option<String>,
    /// `health.probeUrl`.
    pub health_probe_url: Option<String>,
    /// `keychainAccount`.
    pub keychain_account: String,
}

/// The sign helper's default bind (ADR-0019 requirement 2).
pub const SIGN_HELPER_DEFAULT_BIND: &str = "127.0.0.1:8786";

/// Render a socket address in its one spelling: `127.0.0.1:8786`, `[::1]:8786`.
pub fn socket_text(addr: &SocketAddr) -> String {
    match addr {
        SocketAddr::V4(a) => format!("{}:{}", a.ip(), a.port()),
        SocketAddr::V6(a) => format!("[{}]:{}", a.ip(), a.port()),
    }
}

impl SignHelperConfig {
    fn members(&self) -> BTreeMap<String, Value> {
        let Value::Object(members) = Value::object([
            ("bind", Value::str(socket_text(&self.bind))),
            (
                "rateLimitPerSecond",
                Value::Int(self.rate_limit_per_second.into()),
            ),
            (
                "auth",
                Value::object([
                    ("required", Value::Bool(self.auth_required)),
                    (
                        "callerTokens",
                        Value::opt(self.caller_tokens.as_ref(), SecretRef::value),
                    ),
                ]),
            ),
            (
                "signAllow",
                Value::object([
                    ("required", Value::Bool(self.sign_allow_required)),
                    (
                        "callers",
                        Value::Object(
                            self.sign_allow_callers
                                .iter()
                                .map(|(k, v)| (k.clone(), Value::strs(v.iter().cloned())))
                                .collect(),
                        ),
                    ),
                ]),
            ),
            ("resolveAllow", Value::strs(self.resolve_allow.iter().cloned())),
            (
                "resolveCache",
                Value::object([
                    ("maxEntries", Value::Int(self.resolve_cache_max_entries)),
                    ("ttlMs", Value::opt(self.resolve_cache_ttl_ms, Value::Int)),
                ]),
            ),
            (
                "subprocess",
                Value::object([
                    ("opBin", Value::opt(self.op_bin.as_ref(), Value::str)),
                    (
                        "securityBin",
                        Value::opt(self.security_bin.as_ref(), Value::str),
                    ),
                ]),
            ),
            (
                "health",
                Value::object([(
                    "probeUrl",
                    Value::opt(self.health_probe_url.as_ref(), Value::str),
                )]),
            ),
            ("keychainAccount", Value::str(&self.keychain_account)),
        ]) else {
            unreachable!("Value::object builds an object")
        };
        members
    }

    /// Each field set to its loosening value, as `(field, notice)`.
    pub fn loosenings(&self) -> Vec<(&'static str, &'static str)> {
        let mut out = Vec::new();
        if !self.auth_required {
            out.push((
                "auth.required",
                "the helper starts without caller authentication when no caller tokens are configured",
            ));
        }
        if !self.sign_allow_required {
            out.push((
                "signAllow.required",
                "the helper starts without a /sign URL allow-list when none is configured",
            ));
        }
        out
    }
}
