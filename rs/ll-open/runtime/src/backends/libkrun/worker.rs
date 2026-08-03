use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;

use crate::{ExecutionError, ExecutionRequest};
use serde::{Deserialize, Serialize};

use super::api::{DynamicKrunApi, PreparedVm, prepare_vm};
use super::confinement::{VmmHostResources, apply};
use super::plan::{DirectoryRootfsResolver, compile_plan};
use super::volume::{materialize_ephemeral_rootfs, verify_ephemeral_rootfs};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerOptions {
    pub cas_root: PathBuf,
    pub run_root: PathBuf,
    pub libkrun: PathBuf,
    pub runtime_files: Vec<PathBuf>,
    pub devices: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerEvent {
    Ready { run_id: String },
    Failed { error: ExecutionError },
}

impl WorkerOptions {
    pub fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Self, ExecutionError> {
        let mut arguments = arguments.into_iter();
        let mut cas_root = None;
        let mut run_root = None;
        let mut libkrun = None;
        let mut runtime_files = Vec::new();
        let mut devices = Vec::new();

        while let Some(argument) = arguments.next() {
            let value = arguments.next().ok_or_else(|| {
                ExecutionError::invalid(format!(
                    "worker option {} requires a path",
                    argument.to_string_lossy()
                ))
            })?;
            match argument.to_str() {
                Some("--cas-root") if cas_root.is_none() => cas_root = Some(value.into()),
                Some("--libkrun") if libkrun.is_none() => libkrun = Some(value.into()),
                Some("--run-root") if run_root.is_none() => run_root = Some(value.into()),
                Some("--runtime-file") => runtime_files.push(value.into()),
                Some("--device") => devices.push(value.into()),
                _ => {
                    return Err(ExecutionError::invalid(format!(
                        "unknown or duplicate worker option: {}",
                        argument.to_string_lossy()
                    )));
                }
            }
        }

        Ok(Self {
            cas_root: cas_root
                .ok_or_else(|| ExecutionError::invalid("missing --cas-root option"))?,
            run_root: run_root
                .ok_or_else(|| ExecutionError::invalid("missing --run-root option"))?,
            libkrun: libkrun.ok_or_else(|| ExecutionError::invalid("missing --libkrun option"))?,
            runtime_files,
            devices,
        })
    }
}

pub fn execute_from_reader(
    options: WorkerOptions,
    reader: impl Read,
) -> Result<(), ExecutionError> {
    let request: ExecutionRequest = serde_json::from_reader(reader).map_err(|error| {
        ExecutionError::invalid(format!("invalid worker request JSON: {error}"))
    })?;
    execute(options, &request)
}

pub fn execute_from_reader_with_events(
    options: WorkerOptions,
    reader: impl Read,
    mut writer: impl Write,
) -> Result<(), ExecutionError> {
    let request: ExecutionRequest = serde_json::from_reader(reader).map_err(|error| {
        ExecutionError::invalid(format!("invalid worker request JSON: {error}"))
    })?;
    execute_with_ready(options, &request, |event| {
        serde_json::to_writer(&mut writer, event)
            .map_err(|error| ExecutionError::backend(format!("write worker event: {error}")))?;
        writer
            .write_all(b"\n")
            .map_err(|error| ExecutionError::backend(format!("write worker event: {error}")))?;
        writer
            .flush()
            .map_err(|error| ExecutionError::backend(format!("flush worker event: {error}")))
    })
}

pub fn execute(options: WorkerOptions, request: &ExecutionRequest) -> Result<(), ExecutionError> {
    execute_with_ready(options, request, |_| Ok(()))
}

pub fn execute_with_ready(
    options: WorkerOptions,
    request: &ExecutionRequest,
    on_ready: impl FnOnce(&WorkerEvent) -> Result<(), ExecutionError>,
) -> Result<(), ExecutionError> {
    let resolver = DirectoryRootfsResolver::new(&options.cas_root);
    let mut config = compile_plan(&resolver, request)?;
    config.rootfs = materialize_ephemeral_rootfs(&config.rootfs, &options.run_root)?;

    // Loading occurs before nono is applied because the platform dynamic
    // loader may need to resolve libkrun's transitive libraries. All actual VM
    // configuration and execution happen after ambient authority is dropped.
    let api = DynamicKrunApi::load(&options.libkrun)?;
    verify_ephemeral_rootfs(&config.rootfs)?;
    let mut runtime_files = options.runtime_files;
    runtime_files.push(options.libkrun);
    apply(
        &config,
        &VmmHostResources {
            runtime_files,
            devices: options.devices,
        },
    )?;

    let vm: PreparedVm<'_> = prepare_vm(&api, &config)?;
    on_ready(&WorkerEvent::Ready {
        run_id: config.run_id.clone(),
    })?;
    vm.enter()
}
