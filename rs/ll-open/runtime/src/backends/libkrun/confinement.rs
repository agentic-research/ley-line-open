use std::path::PathBuf;

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
    let mut capabilities = CapabilitySet::new()
        .allow_path(&config.rootfs.canonical_path, AccessMode::ReadWrite)
        .map_err(nono_error)?;
    for path in &resources.runtime_files {
        capabilities = capabilities
            .allow_file(path, AccessMode::Read)
            .map_err(nono_error)?;
    }
    for path in &resources.devices {
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
