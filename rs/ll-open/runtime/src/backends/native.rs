//! First-party native process worker.
//!
//! This worker is deliberately separate from the lifecycle backend. The
//! parent owns the ephemeral directory and worker process; this process owns
//! only authenticated rootfs resolution, nono application, and execution of
//! the guest-relative artifact. No caller-provided host path is accepted.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::{ExecutionError, ExecutionRequest};

use super::libkrun::confinement::build_process_capabilities;
use super::libkrun::plan::{DirectoryRootfsResolver, compile_plan};
use super::libkrun::volume::{materialize_ephemeral_rootfs, verify_ephemeral_rootfs};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerOptions {
    pub cas_root: PathBuf,
    pub run_root: PathBuf,
    pub runtime_files: Vec<PathBuf>,
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
        let mut runtime_files = Vec::new();

        while let Some(argument) = arguments.next() {
            let value = arguments.next().ok_or_else(|| {
                ExecutionError::invalid(format!(
                    "native worker option {} requires a path",
                    argument.to_string_lossy()
                ))
            })?;
            match argument.to_str() {
                Some("--cas-root") if cas_root.is_none() => cas_root = Some(value.into()),
                Some("--run-root") if run_root.is_none() => run_root = Some(value.into()),
                Some("--runtime-file") => runtime_files.push(value.into()),
                _ => {
                    return Err(ExecutionError::invalid(format!(
                        "unknown or duplicate native worker option: {}",
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
            runtime_files,
        })
    }
}

pub fn execute_from_reader_with_events(
    options: WorkerOptions,
    reader: impl Read,
    mut writer: impl Write,
) -> Result<(), ExecutionError> {
    let request: ExecutionRequest = serde_json::from_reader(reader).map_err(|error| {
        ExecutionError::invalid(format!("invalid native worker request JSON: {error}"))
    })?;
    execute_with_ready(options, &request, |event| {
        serde_json::to_writer(&mut writer, event).map_err(|error| {
            ExecutionError::backend(format!("write native worker event: {error}"))
        })?;
        writer.write_all(b"\n").map_err(|error| {
            ExecutionError::backend(format!("write native worker event: {error}"))
        })?;
        writer
            .flush()
            .map_err(|error| ExecutionError::backend(format!("flush native worker event: {error}")))
    })
}

pub fn execute_with_ready(
    options: WorkerOptions,
    request: &ExecutionRequest,
    on_ready: impl FnOnce(&WorkerEvent) -> Result<(), ExecutionError>,
) -> Result<(), ExecutionError> {
    request.validate()?;
    let resolver = DirectoryRootfsResolver::new(&options.cas_root);
    let mut config = compile_plan(&resolver, request)?;
    config.rootfs = materialize_ephemeral_rootfs(&config.rootfs, &options.run_root)?;
    verify_ephemeral_rootfs(&config.rootfs)?;

    // Resolve every host path before nono becomes irreversible. The child
    // process receives no ambient path authority after this call.
    let capabilities =
        build_process_capabilities(&config.rootfs.canonical_path, &options.runtime_files, &[])?;
    nono::Sandbox::apply_auto(&capabilities).map_err(|error| {
        ExecutionError::backend(format!("apply native nono confinement: {error}"))
    })?;

    on_ready(&WorkerEvent::Ready {
        run_id: config.run_id.clone(),
    })?;

    let executable = config
        .rootfs
        .canonical_path
        .join(OsStr::from_bytes(config.executable.as_bytes()));
    let mut command = Command::new(&executable);
    command
        .args(
            config
                .arguments
                .iter()
                .map(|argument| OsString::from(OsStr::from_bytes(argument.as_bytes()))),
        )
        .current_dir(&config.rootfs.canonical_path)
        .env_clear()
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for entry in &config.environment {
        let bytes = entry.as_bytes();
        let separator = bytes.iter().position(|byte| *byte == b'=').ok_or_else(|| {
            ExecutionError::invalid("native environment entry has no key/value separator")
        })?;
        let (key, value) = (&bytes[..separator], &bytes[separator + 1..]);
        command.env(OsStr::from_bytes(key), OsStr::from_bytes(value));
    }
    let status = command.status().map_err(|error| {
        ExecutionError::backend(format!("start native guest executable: {error}"))
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(ExecutionError::backend(format!(
            "native guest executable exited with {status}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::WorkerOptions;
    use std::ffi::OsString;

    #[test]
    fn worker_options_require_trusted_roots() {
        let error = WorkerOptions::parse([OsString::from("--run-root")])
            .expect_err("missing option value must fail closed");
        assert!(error.detail.contains("requires a path"));

        let error = WorkerOptions::parse([
            OsString::from("--cas-root"),
            OsString::from("cas"),
            OsString::from("--run-root"),
            OsString::from("run"),
            OsString::from("--host-path"),
            OsString::from("/tmp/ambient"),
        ])
        .expect_err("unknown host-path option must fail closed");
        assert!(error.detail.contains("unknown or duplicate"));
    }
}
