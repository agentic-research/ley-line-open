use std::path::{Path, PathBuf};

use nono::{AccessMode, CapabilitySet, Sandbox};

use crate::ExecutionError;
use crate::confinement::{ConfinementManifest, FsGrant};

use super::plan::KrunConfig;

/// Host resources required after the worker irreversibly drops ambient
/// authority. Runtime files are read-only; device nodes require read/write
/// access for the virtualization API.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VmmHostResources {
    pub runtime_files: Vec<PathBuf>,
    pub devices: Vec<PathBuf>,
}

pub fn build_capabilities(
    config: &KrunConfig,
    resources: &VmmHostResources,
) -> Result<CapabilitySet, ExecutionError> {
    build_process_capabilities(
        &config.rootfs.canonical_path,
        &resources.runtime_files,
        &resources.devices,
    )
}

/// The `confinement/v1` manifest this backend compiles for one worker.
///
/// The common fail-closed policy for a native or VMM worker: the rootfs is
/// the only read/write tree, runtime libraries are read-only, device paths
/// are explicitly read/write, and no network capability is granted. Keeping
/// it independent of libkrun stops a native nono backend from silently
/// widening authority as that backend is built out.
///
/// ADR-0035 §1: the applied `CapabilitySet` and the declared
/// `confinementDigest` must be projections of one object. This is that
/// object. `build_process_capabilities` derives the CapabilitySet *from* it
/// rather than beside it, so a policy change that skipped the manifest would
/// not compile.
///
/// The trailing slash on the rootfs is load-bearing: `confinement/v1` §2
/// distinguishes a directory subtree from a single file by it, and that
/// distinction is exactly nono's `allow_path` vs `allow_file`. Encoding it in
/// the path rather than in a separate flag keeps the manifest self-describing
/// — a reader of the JSON can tell which grant a path is.
pub fn confinement_manifest(
    rootfs: &Path,
    runtime_files: &[PathBuf],
    devices: &[PathBuf],
) -> ConfinementManifest {
    let mut manifest = ConfinementManifest::new()
        .with_fs_grant(FsGrant::read_write(format!("{}/", rootfs.display())));
    for path in runtime_files {
        manifest = manifest.with_fs_grant(FsGrant::read_only(path.display().to_string()));
    }
    for path in devices {
        manifest = manifest.with_fs_grant(FsGrant::read_write(path.display().to_string()));
    }
    // No `network` block at all. §3: an omitted block means no egress, which
    // is what `block_network()` enforces — so declaring an empty allow-list
    // would say the same thing twice, in two places that could disagree.
    manifest
}

pub fn build_process_capabilities(
    rootfs: &Path,
    runtime_files: &[PathBuf],
    devices: &[PathBuf],
) -> Result<CapabilitySet, ExecutionError> {
    capabilities_from_manifest(&confinement_manifest(rootfs, runtime_files, devices))
}

/// Compile a manifest into the `CapabilitySet` nono applies.
///
/// A trailing slash selects `allow_path` (directory subtree); anything else
/// is `allow_file`. nono rejects a directory passed to `allow_file` and a
/// non-directory passed to `allow_path`, so a manifest that mislabels a path
/// fails here rather than granting the wrong shape.
///
/// This compiles confinement/v1 **directly**, deliberately bypassing nono's own
/// `CapabilityManifest` and its `TryFrom<&CapabilityManifest> for CapabilitySet`.
/// The two manifest shapes diverge structurally — `credentials` alone is an array
/// of objects against confinement/v1's scalar `credentialSource`, so no
/// field-for-field mapping exists — and routing through nono's would put a second
/// shape between the object whose digest is attested and the policy actually
/// applied, which is the drift the single manifest exists to prevent. The full
/// field-by-field mapping and the rationale are ADR-0035's second open question
/// (bead `ley-line-open-c17486`).
pub fn capabilities_from_manifest(
    manifest: &ConfinementManifest,
) -> Result<CapabilitySet, ExecutionError> {
    let mut capabilities = CapabilitySet::new();
    for grant in manifest.fs_grants() {
        let (path, mode) = match grant {
            FsGrant::ReadOnly(path) => (path.as_str(), AccessMode::Read),
            FsGrant::ReadWrite { path } => (path.as_str(), AccessMode::ReadWrite),
        };
        capabilities = if let Some(directory) = path.strip_suffix('/') {
            capabilities
                .allow_path(Path::new(directory), mode)
                .map_err(nono_error)?
        } else {
            capabilities
                .allow_file(Path::new(path), mode)
                .map_err(nono_error)?
        };
    }

    // §3 egress. nono's CapabilitySet filters by PORT, never by hostname —
    // host-based egress is nono's *manifest* `network.allow_domains`, which
    // rides its proxy path, not the `apply_auto` path this compiles to. So a
    // manifest declaring allowHosts describes something this tier cannot
    // enforce, and the answer is to say so rather than to block everything and
    // let the digest attest a policy that never took effect.
    //
    // The failure direction is closed either way — dropping the grant leaves
    // the sandbox stricter than the manifest. That is exactly why it needs an
    // error: a silent over-restriction still means the attested confinement
    // digest commits to a document whose §3 had no effect, which is the drift
    // ADR-0035 exists to prevent, one dimension over.
    if !manifest.allowed_hosts().is_empty() {
        return Err(ExecutionError::invalid(format!(
            "confinement/v1 §3 network.allowHosts is not enforceable on this tier: \
             nono filters by port, not by hostname, so the {} declared host(s) \
             would be silently dropped. Route host-scoped egress through the \
             proxy, or omit the dimension.",
            manifest.allowed_hosts().len()
        )));
    }

    // §4 listener. Exactly the shape cloister-harness runs today — deny
    // everything, then open one loopback port to the vault-proxy shim.
    //
    // `allow_localhost_port` is bidirectional (connect + bind) and applies
    // regardless of network mode. On Linux, Landlock filters by port and NOT by
    // destination IP, so "loopback only" is a property of the `block_network`
    // below rather than of this call; on macOS, Seatbelt scopes it to localhost
    // directly. Both reach §4's default of 127.0.0.1, by different routes.
    if let Some((bind, address)) = manifest.port_bind() {
        // §4 defaults to 127.0.0.1 and requires anything wider to be declared
        // explicitly. That declaration is precisely what cannot be honoured
        // here: with Landlock filtering on port alone, a grant for 0.0.0.0 and
        // a grant for loopback compile to the identical rule, so accepting the
        // wider one would attest an exposure decision nothing enforces.
        let loopback = matches!(
            address,
            None | Some("127.0.0.1") | Some("localhost") | Some("::1")
        );
        if !loopback {
            return Err(ExecutionError::invalid(format!(
                "confinement/v1 §4 port.address {:?} is not enforceable on this tier: \
                 nono filters by port, not by bind address, so a non-loopback \
                 listener compiles to the same rule as a loopback one. §4 requires \
                 exposure beyond loopback to be an explicit declaration, and this \
                 tier cannot honour that declaration.",
                address.unwrap_or_default()
            )));
        }
        capabilities = capabilities.allow_localhost_port(bind);
    }

    Ok(capabilities.block_network())
}

/// Apply nono to the current worker process. This is irreversible and must be
/// called only after the worker has loaded libkrun and resolved its rootfs.
pub fn apply(config: &KrunConfig, resources: &VmmHostResources) -> Result<(), ExecutionError> {
    let support = Sandbox::support_info();
    if !support.is_supported {
        return Err(ExecutionError::backend(format!(
            "nono sandbox is unavailable on {}: {}",
            support.platform, support.details
        )));
    }
    let capabilities = build_capabilities(config, resources)?;
    Sandbox::apply_auto(&capabilities)
        .map(|_| ())
        .map_err(nono_error)
}

fn nono_error(error: nono::NonoError) -> ExecutionError {
    ExecutionError::backend(format!("apply nono VMM confinement: {error}"))
}
