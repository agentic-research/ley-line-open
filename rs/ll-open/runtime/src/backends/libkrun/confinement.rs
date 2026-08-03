use std::path::{Path, PathBuf};

use nono::{AccessMode, CapabilitySet, Sandbox};

use crate::ExecutionError;

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

/// Build the common fail-closed policy for a native worker or a VMM worker.
///
/// The rootfs is the only read/write tree. Runtime libraries are read-only;
/// optional device paths are explicitly read/write. No network capability is
/// granted here. Keeping this policy independent of libkrun prevents a native
/// nono backend from silently widening authority while the backend is added.
pub fn build_process_capabilities(
    rootfs: &Path,
    runtime_files: &[PathBuf],
    devices: &[PathBuf],
) -> Result<CapabilitySet, ExecutionError> {
    let mut capabilities = CapabilitySet::new()
        .allow_path(rootfs, AccessMode::ReadWrite)
        .map_err(nono_error)?;
    for path in runtime_files {
        capabilities = capabilities
            .allow_file(path, AccessMode::Read)
            .map_err(nono_error)?;
    }
    for path in devices {
        capabilities = capabilities
            .allow_file(path, AccessMode::ReadWrite)
            .map_err(nono_error)?;
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
