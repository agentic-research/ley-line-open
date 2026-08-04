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
fn capabilities_from_manifest(
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
