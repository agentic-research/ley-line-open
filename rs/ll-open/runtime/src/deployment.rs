//! The execution deployment library entry (ADR-0049 D13): one semantics for
//! the execution daemon and for embedders that link this crate in-process.
//!
//! [`resolve_execution_deployment`] reads an `ExecutionDaemonConfig`
//! document (`apiVersion: config.leyline/v1`), resolves it against injected
//! [`HostFacts`], and returns exactly the structs `leyline execution-daemon`
//! builds from it: the backend's [`NativeWorkerConfig`] or
//! [`KrunWorkerConfig`], the CAS storage choice, the evidence verifier
//! choice, and the rest of the document, with the canonical resolved bytes
//! and their `configDigest`. It is pure: it reads no environment and no
//! file. [`check_execution_host`] runs the read-only host checks
//! `--check-config` runs; the writing steps (attaching a volume, creating
//! the run root) stay where start performs them.
//!
//! Embedders call [`check_removed_environment`] with
//! `std::env::vars_os()` at their own start: it refuses, naming every
//! offender with its replacement key, exactly as the binaries do.
//!
//! `configDigest` names one resolved configuration. It is recorded as
//! evidence and grants nothing; it is never resolved against a CAS.

use std::path::{Path, PathBuf};
use std::time::Duration;

use leyline_schema_spec::deployment_config::{
    self as spec, BackendSettings, CasSettings, Document, EvidenceSettings, ExecutionDaemonConfig,
    Kind,
};
pub use leyline_schema_spec::deployment_config::{
    ConfigRefusal, Diagnostics as DiagnosticsSettings, Enrichment as EnrichmentSettings,
    HeadSettings, HostFacts, Os, ParserSettings, REMOVED_VARIABLES_JSON, Reason, RemovedVariable,
    RemovedVariables, Syntax, check_removed_environment, removed_variables, test_seams_set,
};

use crate::backends::libkrun::backend::KrunWorkerConfig;
use crate::backends::native_backend::NativeWorkerConfig;
use crate::model::DigestRef;

/// The execution daemon's configuration, resolved from one document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionDeployment {
    /// The backend and the worker configuration it is built from.
    pub backend: BackendDeployment,
    /// Where the CAS root lives.
    pub cas: CasDeployment,
    /// The trusted catalog document.
    pub catalog: PathBuf,
    /// The evidence verifier to install.
    pub evidence: EvidenceDeployment,
    /// The arena file (64 MiB).
    pub arena: PathBuf,
    /// The controller file.
    pub control: PathBuf,
    /// LLO's state directory.
    pub state_dir: PathBuf,
    /// The source tree, when the daemon serves a store.
    pub source: Option<SourceDeployment>,
    /// Activate or resume the private CDC index.
    pub cdc: bool,
    /// `parser`.
    pub parser: ParserSettings,
    /// `head`.
    pub head: HeadSettings,
    /// `diagnostics`.
    pub diagnostics: DiagnosticsSettings,
    /// `enrichment`.
    pub enrichment: EnrichmentSettings,
    /// The canonical resolved bytes (ADR-0049 D6): what `--print-config`
    /// writes.
    pub resolved: Vec<u8>,
    /// BLAKE3-256 over [`ExecutionDeployment::resolved`], algorithm
    /// `blake3-256`.
    pub config_digest: DigestRef,
}

impl ExecutionDeployment {
    /// The arena size the execution daemon uses, in MiB.
    pub const ARENA_SIZE_MIB: u64 = spec::EXECUTION_ARENA_SIZE_MIB;

    /// `configDigest` as text: `blake3-256:<hex>`.
    pub fn config_digest_text(&self) -> String {
        format!(
            "{}:{}",
            self.config_digest.algorithm, self.config_digest.value
        )
    }

    /// The CAS root: the directory's, or the volume layout's (not attached).
    pub fn cas_root(&self) -> &Path {
        match &self.backend {
            BackendDeployment::Native { config, .. } => &config.cas_root,
            BackendDeployment::MicroVm { config, .. } => &config.cas_root,
        }
    }

    /// Each run's ephemeral rootfs parent.
    pub fn run_root(&self) -> &Path {
        match &self.backend {
            BackendDeployment::Native { config, .. } => &config.ephemeral_root,
            BackendDeployment::MicroVm { config, .. } => &config.ephemeral_root,
        }
    }
}

/// The backend this daemon supervises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendDeployment {
    /// The native nono backend.
    Native {
        /// Exactly what `NativeWorkerBackend::new` is given.
        config: NativeWorkerConfig,
        /// A delegated cgroup v2 root (Linux), opened at start with
        /// `CgroupRoot::open`.
        cgroup_root: Option<PathBuf>,
    },
    /// The libkrun microVM backend.
    MicroVm {
        /// Exactly what `KrunWorkerBackend::new` is given.
        config: KrunWorkerConfig,
        /// ADR-0044 read freshness of a workspace share.
        workspace: WorkspaceFreshness,
    },
}

/// ADR-0044 read freshness: how long a guest may cache what a workspace
/// share tells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceFreshness {
    /// Timeout of a path with no recent change.
    pub settled: Duration,
    /// Timeout of a recently changed path.
    pub changed: Duration,
    /// How long a change keeps a path's timeouts short.
    pub window: Duration,
}

/// Where the CAS root lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CasDeployment {
    /// An operator directory that must exist.
    Directory {
        /// The CAS root.
        root: PathBuf,
    },
    /// A case-sensitive volume beneath the state directory.
    Volume {
        /// The state directory (`CasVolume::new`'s first argument).
        state_dir: PathBuf,
        /// The volume's logical maximum, in bytes.
        size_cap_bytes: u64,
        /// The run root (`CasVolume::with_run_root`); the volume's own by
        /// default.
        run_root: PathBuf,
    },
}

/// The evidence verifier to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceDeployment {
    /// Metadata-only evidence and unsigned grants, for fixtures.
    Unverified,
    /// `CasDsseEvidenceVerifier` over `store`, trusting `trust_keys`.
    Verified {
        /// Ed25519 public key files, sorted.
        trust_keys: Vec<PathBuf>,
        /// The evidence CAS directory.
        store: PathBuf,
    },
}

/// The source tree a daemon serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDeployment {
    /// The tree.
    pub path: PathBuf,
    /// The language filter; `None` detects per file.
    pub language: Option<String>,
}

/// Read and resolve an `ExecutionDaemonConfig` document. Any other kind is
/// `wrong-kind`; every refusal names its field and stable reason.
pub fn resolve_execution_deployment(
    text: &[u8],
    syntax: Syntax,
    host: &HostFacts,
) -> Result<ExecutionDeployment, ConfigRefusal> {
    let resolved = spec::resolve(text, syntax, &[Kind::ExecutionDaemon], host)?;
    let config_digest = DigestRef {
        algorithm: spec::DIGEST_ALGORITHM.to_owned(),
        value: resolved.digest_hex(),
    };
    let Document::ExecutionDaemon(doc) = resolved.document else {
        unreachable!("resolve accepted only ExecutionDaemonConfig")
    };
    Ok(deployment(doc, host.os, resolved.canonical, config_digest))
}

fn deployment(
    doc: ExecutionDaemonConfig,
    os: Os,
    resolved: Vec<u8>,
    config_digest: DigestRef,
) -> ExecutionDeployment {
    let cas_root = PathBuf::from(doc.cas_root(os));
    let run_root = PathBuf::from(doc.run_root());
    let worker = PathBuf::from(&doc.worker.path);
    let runtime_files: Vec<PathBuf> = doc.worker.runtime_files.iter().map(PathBuf::from).collect();
    let ready_timeout = Duration::from_millis(doc.worker.ready_timeout_ms);
    let backend = match &doc.backend {
        BackendSettings::Native { cgroup_root } => BackendDeployment::Native {
            config: NativeWorkerConfig {
                worker,
                cas_root,
                ephemeral_root: run_root.clone(),
                runtime_files,
                ready_timeout,
            },
            cgroup_root: cgroup_root.as_ref().map(PathBuf::from),
        },
        BackendSettings::MicroVm {
            devices,
            tsi_hijack_inet,
            workspace,
        } => BackendDeployment::MicroVm {
            config: KrunWorkerConfig {
                worker,
                cas_root,
                ephemeral_root: run_root.clone(),
                runtime_files,
                devices: devices.iter().map(PathBuf::from).collect(),
                ready_timeout,
                tsi_hijack_inet: *tsi_hijack_inet,
            },
            workspace: WorkspaceFreshness {
                settled: Duration::from_millis(workspace.ttl_ms),
                changed: Duration::from_millis(workspace.changed_ttl_ms),
                window: Duration::from_millis(workspace.change_window_ms),
            },
        },
    };
    let cas = match &doc.cas {
        CasSettings::Directory { root, .. } => CasDeployment::Directory {
            root: PathBuf::from(root),
        },
        CasSettings::Volume { size_cap_gib, .. } => CasDeployment::Volume {
            state_dir: PathBuf::from(&doc.state_dir),
            // sizeCapGib is at most 2^24, so this cannot overflow.
            size_cap_bytes: size_cap_gib << 30,
            run_root,
        },
    };
    let evidence = match &doc.evidence {
        EvidenceSettings::Unverified => EvidenceDeployment::Unverified,
        EvidenceSettings::Verified { trust_keys, store } => EvidenceDeployment::Verified {
            trust_keys: trust_keys.iter().map(PathBuf::from).collect(),
            store: PathBuf::from(store),
        },
    };
    ExecutionDeployment {
        backend,
        cas,
        catalog: PathBuf::from(&doc.catalog),
        evidence,
        arena: PathBuf::from(&doc.arena_path),
        control: PathBuf::from(&doc.control),
        state_dir: PathBuf::from(&doc.state_dir),
        source: doc.source.map(|s| SourceDeployment {
            path: PathBuf::from(s.path),
            language: s.language,
        }),
        cdc: doc.cdc_enabled,
        parser: doc.parser,
        head: doc.head,
        diagnostics: doc.diagnostics,
        enrichment: doc.enrichment,
        resolved,
        config_digest,
    }
}

/// The outcome of the read-only host checks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostCheck {
    /// Every failed check, sorted by field (exit 78, or 65 for a secret
    /// whose contents are refused).
    pub refusals: Vec<ConfigRefusal>,
    /// Checks that need a write, deferred to start: listed on stderr, never
    /// failing the check.
    pub deferred: Vec<String>,
}

fn host_refusal(field: &str, message: impl Into<String>) -> ConfigRefusal {
    ConfigRefusal::new(field, Reason::HostCheckFailed, message)
}

/// Run the read-only host checks `--check-config` runs (ADR-0049 D5). It
/// writes nothing: no directory is created, no volume attached, no file
/// generated, no probe written. Every failed check is reported.
pub fn check_execution_host(deployment: &ExecutionDeployment) -> HostCheck {
    let mut check = HostCheck::default();
    let mut refuse = |r: ConfigRefusal| check.refusals.push(r);

    let worker = match &deployment.backend {
        BackendDeployment::Native { config, .. } => &config.worker,
        BackendDeployment::MicroVm { config, .. } => &config.worker,
    };
    if !worker.is_file() {
        refuse(host_refusal(
            "worker.path",
            format!("{} is not an existing regular file", worker.display()),
        ));
    }
    let runtime_files = match &deployment.backend {
        BackendDeployment::Native { config, .. } => &config.runtime_files,
        BackendDeployment::MicroVm { config, .. } => &config.runtime_files,
    };
    for file in runtime_files {
        if !file.exists() {
            refuse(host_refusal(
                "worker.runtimeFiles",
                format!("{} does not exist", file.display()),
            ));
        }
    }
    match std::fs::read(&deployment.catalog) {
        Ok(bytes) => {
            if let Err(error) = crate::CatalogResolver::from_json(&bytes) {
                refuse(host_refusal(
                    "catalog",
                    format!("{} does not parse: {error}", deployment.catalog.display()),
                ));
            }
        }
        Err(error) => refuse(host_refusal(
            "catalog",
            format!("{}: {error}", deployment.catalog.display()),
        )),
    }
    match &deployment.backend {
        BackendDeployment::Native {
            cgroup_root: Some(root),
            ..
        } => {
            if let Err(error) = crate::backends::cgroup::CgroupRoot::open(root) {
                refuse(host_refusal("backend.native.cgroupRoot", error.detail));
            }
        }
        BackendDeployment::Native { .. } => {}
        BackendDeployment::MicroVm { config, .. } => {
            for device in &config.devices {
                if !device.exists() {
                    refuse(host_refusal(
                        "backend.microVm.devices",
                        format!("{} does not exist", device.display()),
                    ));
                }
            }
        }
    }
    let run_root = deployment.run_root();
    match &deployment.cas {
        CasDeployment::Directory { root } => {
            if !root.is_dir() {
                refuse(host_refusal(
                    "cas.directory.root",
                    format!("{} is not an existing directory", root.display()),
                ));
            }
            if run_root.exists() && !run_root.is_dir() {
                refuse(host_refusal(
                    "cas.directory.runRoot",
                    format!("{} exists and is not a directory", run_root.display()),
                ));
            }
            if !run_root.exists() {
                check
                    .deferred
                    .push(format!("create the run root {}", run_root.display()));
            }
        }
        CasDeployment::Volume { state_dir, .. } => {
            if let crate::cas_volume::HostOs::Other(_) = crate::cas_volume::HostOs::current() {
                refuse(host_refusal(
                    "cas.volume",
                    "a case-sensitive CAS volume is supported on macOS and Linux only",
                ));
            }
            check.deferred.push(format!(
                "attach the CAS volume beneath {} (start re-attaches one provisioning created)",
                state_dir.display()
            ));
            check.deferred.push(format!(
                "check that the run root {} is case-sensitive",
                run_root.display()
            ));
        }
    }
    // The resolver's overlap check is on spellings. On the host, two
    // spellings can name one directory: the /var and /tmp symlinks, and on
    // macOS case and Unicode normalization. Re-check the resolved paths.
    if let Some(field) = resolved_overlap(deployment) {
        refuse(ConfigRefusal::new(
            field,
            Reason::RunRootOverlapsCasRoot,
            format!(
                "the run root {} and the CAS root {} name overlapping directories on this host",
                run_root.display(),
                deployment.cas_root().display()
            ),
        ));
    }
    if let EvidenceDeployment::Verified { trust_keys, store } = &deployment.evidence {
        // The two halves of the gate start installs
        // (`cas_evidence::operator_verifier`), each reported by field.
        if let Err(error) = crate::cas_evidence::load_trust_keys(trust_keys) {
            refuse(host_refusal(
                "evidence.verified.trustKeys",
                error.to_string(),
            ));
        }
        if let Err(error) = crate::cas_evidence::CasEvidenceStore::open(store) {
            refuse(host_refusal("evidence.verified.store", error.to_string()));
        }
    }
    if let Some(source) = &deployment.source
        && !source.path.is_dir()
    {
        refuse(host_refusal(
            "source.path",
            format!("{} is not an existing directory", source.path.display()),
        ));
    }
    if let Some(key) = &deployment.head.signing_key {
        match spec::files::read_secret_file("head.signingKey", &key.file) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                let seed = text.trim();
                if seed.len() != 64 || !seed.bytes().all(|b| b.is_ascii_hexdigit()) {
                    refuse(ConfigRefusal::new(
                        "head.signingKey",
                        Reason::SecretFileInvalid,
                        format!("{} does not hold a 64-hex-character Ed25519 seed", key.file),
                    ));
                }
            }
            Err(r) => refuse(r),
        }
    }
    for key in &deployment.head.trusted_keys {
        if let Err(error) = crate::cas_evidence::load_trust_key(Path::new(key)) {
            refuse(host_refusal("head.trustedKeys", error.to_string()));
        }
    }
    check.refusals.sort_by(|a, b| {
        a.field
            .cmp(&b.field)
            .then_with(|| a.message.cmp(&b.message))
    });
    check
}

/// `path` as the host names it: the deepest existing ancestor
/// `realpath`ed, the rest appended. On macOS, case-folded and NFC-normalized
/// as APFS compares names by default.
fn host_spelling(path: &Path) -> String {
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    let resolved = loop {
        match std::fs::canonicalize(&existing) {
            Ok(r) => break r,
            Err(_) => match (existing.parent(), existing.file_name()) {
                (Some(up), Some(name)) => {
                    rest.push(name.to_owned());
                    existing = up.to_path_buf();
                }
                _ => break existing,
            },
        }
    };
    let mut full = resolved;
    for name in rest.iter().rev() {
        full.push(name);
    }
    let text = full.to_string_lossy().into_owned();
    if cfg!(target_os = "macos") {
        use unicode_normalization::UnicodeNormalization;
        text.nfc().collect::<String>().to_lowercase()
    } else {
        text
    }
}

/// The field of a run root that overlaps the CAS root once both are named
/// as the host names them, if they do.
fn resolved_overlap(deployment: &ExecutionDeployment) -> Option<&'static str> {
    let run = host_spelling(deployment.run_root());
    let cas = host_spelling(deployment.cas_root());
    let beneath = |x: &str, y: &str| x.strip_prefix(y).is_some_and(|r| r.starts_with('/'));
    (run == cas || beneath(&run, &cas) || beneath(&cas, &run)).then_some(match deployment.cas {
        CasDeployment::Directory { .. } => "cas.directory.runRoot",
        CasDeployment::Volume { .. } => "cas.volume.runRoot",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two spellings of one directory that the resolver's spelling check
    /// accepts are refused once the host names them: a symlinked parent, and
    /// on macOS a case variant.
    #[test]
    fn an_overlap_through_another_spelling_is_refused_on_the_host() {
        let dir = tempfile::TempDir::new().unwrap();
        let cas = dir.path().join("Cas");
        std::fs::create_dir(&cas).unwrap();
        std::os::unix::fs::symlink(&cas, dir.path().join("alias")).unwrap();
        let doc = |root: &Path, run_root: &Path| {
            format!(
                r#"{{"apiVersion": "config.leyline/v1", "kind": "ExecutionDaemonConfig",
                "cas": {{"directory": {{"root": "{}", "runRoot": "{}"}}}},
                "worker": {{"path": "/w"}}, "catalog": "/c", "evidence": {{"unverified": {{}}}}}}"#,
                root.display(),
                run_root.display()
            )
        };
        let through_symlink = doc(&cas, &dir.path().join("alias/runs"));
        let d = resolve_execution_deployment(
            through_symlink.as_bytes(),
            Syntax::Json,
            &host(Os::MacOs),
        )
        .expect("the spellings differ, so the resolver accepts it");
        assert_eq!(resolved_overlap(&d), Some("cas.directory.runRoot"));
        assert!(
            check_execution_host(&d)
                .refusals
                .iter()
                .any(|r| r.reason == Reason::RunRootOverlapsCasRoot)
        );
        let apart = doc(&cas, &dir.path().join("runs"));
        let d =
            resolve_execution_deployment(apart.as_bytes(), Syntax::Json, &host(Os::MacOs)).unwrap();
        assert_eq!(resolved_overlap(&d), None);
        if cfg!(target_os = "macos") {
            let case = doc(&cas, &dir.path().join("cas/runs"));
            let d = resolve_execution_deployment(case.as_bytes(), Syntax::Json, &host(Os::MacOs))
                .unwrap();
            assert_eq!(resolved_overlap(&d), Some("cas.directory.runRoot"));
        }
    }

    fn host(os: Os) -> HostFacts {
        HostFacts {
            home: Some("/Users/fixture".into()),
            xdg_state_home: None,
            data_dir: Some("/Users/fixture/Library/Application Support".into()),
            os,
        }
    }

    const MINIMAL: &str = r#"{"apiVersion": "config.leyline/v1", "kind": "ExecutionDaemonConfig",
        "cas": {"volume": {"sizeCapGib": 2}}, "worker": {"path": "/w"}, "catalog": "/c",
        "evidence": {"verified": {"trustKeys": ["/k"], "store": "/s"}},
        "source": {"path": "/src", "language": "go"}, "cdc": {"enabled": true}}"#;

    #[test]
    fn a_volume_resolves_to_the_layout_cas_volume_computes() {
        for os in [Os::MacOs, Os::Linux] {
            let d =
                resolve_execution_deployment(MINIMAL.as_bytes(), Syntax::Json, &host(os)).unwrap();
            let state = PathBuf::from("/Users/fixture/.local/state/leyline");
            let host_os = match os {
                Os::MacOs => crate::cas_volume::HostOs::MacOs,
                Os::Linux => crate::cas_volume::HostOs::Linux,
            };
            let volume = crate::cas_volume::CasVolume::with_parts(
                &state,
                2 << 30,
                host_os,
                Box::new(crate::cas_volume::Hdiutil::default()),
                crate::cas_volume::probe_sensitivity,
            )
            .unwrap();
            assert_eq!(d.cas_root(), volume.cas_root());
            assert_eq!(d.run_root(), volume.run_root());
            assert_eq!(
                d.cas,
                CasDeployment::Volume {
                    state_dir: state.clone(),
                    size_cap_bytes: 2 << 30,
                    run_root: volume.run_root(),
                }
            );
            assert_eq!(
                d.evidence,
                EvidenceDeployment::Verified {
                    trust_keys: vec!["/k".into()],
                    store: "/s".into()
                }
            );
            assert_eq!(
                d.source,
                Some(SourceDeployment {
                    path: "/src".into(),
                    language: Some("go".into())
                })
            );
            assert!(d.cdc);
            assert_eq!(d.state_dir, state);
            assert_eq!(
                d.arena,
                PathBuf::from("/Users/fixture/.mache/execution.arena")
            );
            assert_eq!(
                d.control,
                PathBuf::from("/Users/fixture/.mache/execution.ctrl")
            );
            assert_eq!(d.catalog, PathBuf::from("/c"));
            assert_eq!(ExecutionDeployment::ARENA_SIZE_MIB, 64);
            assert_eq!(d.config_digest.algorithm, "blake3-256");
            assert_eq!(
                d.config_digest.value,
                blake3::hash(&d.resolved).to_hex().to_string()
            );
            assert_eq!(
                d.config_digest_text(),
                format!("blake3-256:{}", d.config_digest.value)
            );
        }
    }

    #[test]
    fn another_kind_is_refused_by_name() {
        let e = resolve_execution_deployment(
            br#"{"apiVersion": "config.leyline/v1", "kind": "DaemonConfig"}"#,
            Syntax::Json,
            &host(Os::MacOs),
        )
        .unwrap_err();
        assert_eq!((e.field.as_str(), e.reason), ("kind", Reason::WrongKind));
    }

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn host_checks_report_every_failure_and_write_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = |name: &str| dir.path().join(name).to_str().unwrap().to_owned();
        let doc = format!(
            r#"{{"apiVersion": "config.leyline/v1", "kind": "ExecutionDaemonConfig",
            "cas": {{"directory": {{"root": "{cas}", "runRoot": "{runs}"}}}},
            "worker": {{"path": "{worker}", "runtimeFiles": ["{rt}"]}}, "catalog": "{catalog}",
            "evidence": {{"verified": {{"trustKeys": ["{key}"], "store": "{store}"}}}},
            "backend": {{"microVm": {{"devices": ["{dev}"]}}}},
            "source": {{"path": "{src}"}},
            "head": {{"signingKey": {{"file": "{seed}"}}, "trustedKeys": ["{tk}"]}}}}"#,
            cas = p("cas"),
            runs = p("runs"),
            worker = p("worker"),
            rt = p("rt"),
            catalog = p("catalog.json"),
            key = p("key.pub"),
            store = p("store"),
            dev = p("dev"),
            src = p("src"),
            seed = p("seed"),
            tk = p("tk.pub"),
        );
        let d =
            resolve_execution_deployment(doc.as_bytes(), Syntax::Json, &host(Os::MacOs)).unwrap();
        let check = check_execution_host(&d);
        let fields: Vec<&str> = check.refusals.iter().map(|r| r.field.as_str()).collect();
        assert_eq!(
            fields,
            [
                "backend.microVm.devices",
                "cas.directory.root",
                "catalog",
                "evidence.verified.store",
                "evidence.verified.trustKeys",
                "head.signingKey",
                "head.trustedKeys",
                "source.path",
                "worker.path",
                "worker.runtimeFiles",
            ]
        );
        assert_eq!(check.deferred.len(), 1, "{:?}", check.deferred);
        assert!(
            !dir.path().join("runs").exists(),
            "the run root is never created"
        );

        // Make every check pass.
        std::fs::create_dir(dir.path().join("cas")).unwrap();
        std::fs::create_dir(dir.path().join("store")).unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        for f in ["worker", "rt", "dev"] {
            write(&dir.path().join(f), b"x");
        }
        write(&dir.path().join("catalog.json"), br#"{"entries":[]}"#);
        // RFC 8032 test 1's public key; a trust key file is parsed, not
        // only found.
        let key = b"d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
        write(&dir.path().join("key.pub"), key);
        write(&dir.path().join("tk.pub"), b"x");
        let check = check_execution_host(&d);
        assert_eq!(
            check
                .refusals
                .iter()
                .map(|r| r.field.as_str())
                .collect::<Vec<_>>(),
            ["head.signingKey", "head.trustedKeys"],
            "a malformed head trust key is refused; the seed is not yet written"
        );
        write(&dir.path().join("tk.pub"), key);
        let seed = dir.path().join("seed");
        write(&seed, "ab".repeat(32).as_bytes());
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&seed, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let check = check_execution_host(&d);
        assert!(check.refusals.is_empty(), "{:?}", check.refusals);

        write(&seed, b"not a seed");
        let check = check_execution_host(&d);
        assert!(
            check
                .refusals
                .iter()
                .any(|r| r.field == "head.signingKey" && r.reason == Reason::SecretFileInvalid)
        );
    }

    #[test]
    fn a_volume_defers_the_writing_checks() {
        let d = resolve_execution_deployment(MINIMAL.as_bytes(), Syntax::Json, &host(Os::MacOs))
            .unwrap();
        let check = check_execution_host(&d);
        assert_eq!(check.deferred.len(), 2, "{:?}", check.deferred);
    }

    #[test]
    fn removed_variables_are_reexported_for_embedders() {
        let err =
            check_removed_environment([("LEYLINE_STATE_DIR".into(), "/x".into())]).unwrap_err();
        assert_eq!(err.offenders[0].replacement, "stateDir");
        assert!(REMOVED_VARIABLES_JSON.contains("LEYLINE_STATE_DIR"));
        assert_eq!(removed_variables().len(), 19);
        assert!(test_seams_set([("LLO_EXTRACTION_EPOCH".into(), "1".into())]).len() == 1);
    }
}
