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

use crate::confinement::ConfinementManifest;
use crate::{ExecutionError, ExecutionRequest};

use super::libkrun::confinement::{Tier, capabilities_from_manifest, confinement_manifest};
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
    Ready {
        run_id: String,
        /// The digest of the confinement manifest this worker actually
        /// compiled and applied (ADR-0035). The worker is the only party
        /// that can report this — the policy is built here, after fork, from
        /// a rootfs path resolved against a materialized tree. A daemon-side
        /// recomputation would be a second implementation of that
        /// derivation, which is the drift the single manifest prevents.
        ///
        /// `#[serde(default)]` so a worker predating the field is reported
        /// as attesting nothing, and refused, rather than failing to parse.
        #[serde(default)]
        confinement_digest: String,
    },
    Failed {
        error: ExecutionError,
    },
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
    // One manifest, two uses: the capabilities actually applied, and the
    // digest attested below. Deriving both from `manifest` is what makes the
    // attestation true by construction rather than by discipline.
    // Same source as the microVM tier, and for the same reason: §4 may only
    // originate from the document the grant authorized. This tier is where
    // §4 is strongest — Landlock filters `bind(2)` per port, so the grant
    // means here exactly what it says — which is why it would be wrong to
    // wire the listener on the tier that needs a port map and leave it
    // unreachable on the tier whose kernel enforces it directly.
    let authorized = match &request.confinement_manifest {
        Some(document) => Some(ConfinementManifest::parse(document).map_err(|error| {
            ExecutionError::invalid(format!(
                "RunGrant.confinementManifest did not survive the worker boundary: {error}"
            ))
        })?),
        None => None,
    };
    let manifest = confinement_manifest(&options.runtime_files, &[], authorized.as_ref())?;
    let capabilities =
        capabilities_from_manifest(&manifest, &config.rootfs.canonical_path, Tier::Native)?;
    nono::Sandbox::apply_auto(&capabilities).map_err(|error| {
        ExecutionError::backend(format!("apply native nono confinement: {error}"))
    })?;

    // Reported *after* the policy is irreversibly applied, so the digest
    // describes what this process is now confined by rather than what it
    // intended to be confined by.
    on_ready(&WorkerEvent::Ready {
        run_id: config.run_id.clone(),
        confinement_digest: manifest.confinement_digest()?,
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
        let (key, value) = split_environment_entry(entry.as_bytes())?;
        command.env(OsStr::from_bytes(key), OsStr::from_bytes(value));
    }
    let status = command.status().map_err(|error| {
        ExecutionError::backend(format!("start native guest executable: {error}"))
    })?;
    guest_exit_result(status)
}

fn split_environment_entry(bytes: &[u8]) -> Result<(&[u8], &[u8]), ExecutionError> {
    let separator = bytes.iter().position(|byte| *byte == b'=').ok_or_else(|| {
        ExecutionError::invalid("native environment entry has no key/value separator")
    })?;
    Ok((&bytes[..separator], &bytes[separator + 1..]))
}

fn guest_exit_result(status: std::process::ExitStatus) -> Result<(), ExecutionError> {
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
    use super::{
        WorkerOptions, execute_from_reader_with_events, execute_with_ready, guest_exit_result,
        split_environment_entry,
    };
    use crate::{DigestRef, ExecutionRequest, ResourceLimits};
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::process::Command;

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

    #[test]
    fn worker_options_preserve_runtime_files_and_reject_duplicate_roots() {
        let options = WorkerOptions::parse([
            OsString::from("--cas-root"),
            OsString::from("cas"),
            OsString::from("--run-root"),
            OsString::from("run"),
            OsString::from("--runtime-file"),
            OsString::from("lib-a"),
            OsString::from("--runtime-file"),
            OsString::from("lib-b"),
        ])
        .expect("valid worker options");
        assert_eq!(options.cas_root, PathBuf::from("cas"));
        assert_eq!(options.run_root, PathBuf::from("run"));
        assert_eq!(
            options.runtime_files,
            vec![PathBuf::from("lib-a"), PathBuf::from("lib-b")]
        );

        for duplicate in ["--cas-root", "--run-root"] {
            let mut arguments = vec![
                OsString::from("--cas-root"),
                OsString::from("cas"),
                OsString::from("--run-root"),
                OsString::from("run"),
            ];
            arguments.push(OsString::from(duplicate));
            arguments.push(OsString::from("other"));
            let error = WorkerOptions::parse(arguments)
                .expect_err("duplicate trusted root must fail closed");
            assert!(error.detail.contains("unknown or duplicate"));
        }
    }

    fn invalid_request() -> ExecutionRequest {
        ExecutionRequest {
            run_id: "run-invalid".into(),
            replay_key: "replay-invalid".into(),
            rootfs: DigestRef {
                algorithm: "sha256".into(),
                value: "a".repeat(64),
            },
            executable: "bin/agent".into(),
            arguments: Vec::new(),
            public_environment: BTreeMap::new(),
            allowed_egress: Vec::new(),
            confinement_digest: String::new(),
            confinement_manifest: None,
            limits: ResourceLimits {
                vcpus: 1,
                memory_mib: 64,
                wall_time_ms: 1_000,
            },
        }
    }

    #[test]
    fn native_entrypoints_reject_invalid_input_before_confinement() {
        let options = WorkerOptions {
            cas_root: PathBuf::from("missing-cas"),
            run_root: PathBuf::from("missing-run"),
            runtime_files: Vec::new(),
        };
        let error =
            execute_from_reader_with_events(options.clone(), b"not-json".as_slice(), Vec::new())
                .expect_err("invalid worker JSON must fail");
        assert!(error.detail.contains("invalid native worker request JSON"));

        let mut ready_called = false;
        execute_with_ready(options, &invalid_request(), |_| {
            ready_called = true;
            Ok(())
        })
        .expect_err("invalid request must fail before rootfs resolution");
        assert!(!ready_called);
    }

    #[test]
    fn environment_split_preserves_equals_in_the_value() {
        let (key, value) = split_environment_entry(b"TOKEN=a=b").expect("environment entry");
        assert_eq!(key, b"TOKEN");
        assert_eq!(value, b"a=b");
        split_environment_entry(b"TOKEN").expect_err("missing separator must fail");
    }

    #[test]
    fn guest_exit_status_controls_native_success() {
        let success = Command::new("sh")
            .args(["-c", "exit 0"])
            .status()
            .expect("success status");
        guest_exit_result(success).expect("successful guest");

        let failure = Command::new("sh")
            .args(["-c", "exit 7"])
            .status()
            .expect("failure status");
        let error = guest_exit_result(failure).expect_err("failed guest must fail");
        assert!(error.detail.contains("native guest executable exited"));
    }
}
