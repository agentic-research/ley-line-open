use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;

use crate::confinement::ConfinementManifest;
use crate::{ExecutionError, ExecutionRequest};
use serde::{Deserialize, Serialize};

use super::api::{DynamicKrunApi, KRUN_TSI_HIJACK_INET, PreparedVm, prepare_vm};
use super::confinement::{
    VmmHostResources, apply_manifest, confinement_manifest, vsock_unix_mappings,
};
use super::plan::{DirectoryRootfsResolver, compile_plan};
use super::volume::{materialize_ephemeral_rootfs, verify_ephemeral_rootfs};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerOptions {
    pub cas_root: PathBuf,
    pub run_root: PathBuf,
    pub libkrun: PathBuf,
    pub runtime_files: Vec<PathBuf>,
    pub devices: Vec<PathBuf>,
    /// Carry the guest's `AF_INET` sockets over vsock (`KRUN_TSI_HIJACK_INET`).
    ///
    /// Off unless an operator asks for it, and asked for HERE — on the worker's
    /// command line — rather than anywhere a workload can reach. It exists
    /// because an unmodified guest that binds TCP (mache on 7532) cannot
    /// otherwise be reached across a vsock-only boundary.
    ///
    /// It is strictly weaker than the default. Without it the guest talks to
    /// the host only over vsock ports it was explicitly handed; with it, the
    /// guest's ordinary sockets are silently rerouted, and what the host can
    /// reach is decided by the port map instead of by what was granted.
    pub tsi_hijack_inet: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerEvent {
    Ready {
        run_id: String,
        /// The digest of the confinement manifest this worker compiled and
        /// applied. Same contract as the native worker's — see
        /// `backends::native::WorkerEvent`.
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
        let mut libkrun = None;
        let mut runtime_files = Vec::new();
        let mut devices = Vec::new();

        let mut tsi_hijack_inet = false;

        while let Some(argument) = arguments.next() {
            // Checked before a value is consumed: this is the one valueless
            // flag, and the loop below assumes every option takes a path.
            if argument.to_str() == Some("--tsi-hijack-inet") {
                tsi_hijack_inet = true;
                continue;
            }
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
            tsi_hijack_inet,
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
    // `compile_plan` hardcodes 0 — a workload cannot widen its own boundary
    // through the request. Hijacking is applied here, from the operator's
    // command line, and nowhere else.
    if options.tsi_hijack_inet {
        config.tsi_features = KRUN_TSI_HIJACK_INET;
    }
    config.rootfs = materialize_ephemeral_rootfs(&config.rootfs, &options.run_root)?;

    // Loading occurs before nono is applied because the platform dynamic
    // loader may need to resolve libkrun's transitive libraries. All actual VM
    // configuration and execution happen after ambient authority is dropped.
    let api = DynamicKrunApi::load(&options.libkrun)?;
    verify_ephemeral_rootfs(&config.rootfs)?;
    let mut runtime_files = options.runtime_files;
    runtime_files.push(options.libkrun);
    let resources = VmmHostResources {
        runtime_files,
        devices: options.devices,
    };
    // ONE manifest, applied and attested. The previous version built a manifest
    // here and then called `apply(&config, &resources)`, which built a SECOND
    // one from the same inputs — and its comment claimed the digest described
    // "the policy this process is actually confined by", which held only because
    // the arguments happened to match. Change which path `apply` derives from
    // and the worker would attest a digest for a policy it did not apply, with
    // nothing to catch it: the digest's only consumer is a comparison against
    // the grant, not against the applied set.
    //
    // `native.rs` already did it this way, and its comment — "Deriving both from
    // `manifest` is what makes the attestation true by construction rather than
    // by discipline" — was accurate there and inaccurate one directory over.
    // Re-parsed rather than carried as a value: the worker is a separate
    // process, so the document crosses as JSON either way, and parsing it here
    // means the worker applies the same builder invariants `authorization.rs`
    // did instead of trusting a shape someone else validated.
    let authorized = match &request.confinement_manifest {
        Some(document) => Some(ConfinementManifest::parse(document).map_err(|error| {
            ExecutionError::invalid(format!(
                "RunGrant.confinementManifest did not survive the worker boundary: {error}"
            ))
        })?),
        None => None,
    };
    let manifest = confinement_manifest(
        &resources.runtime_files,
        &resources.devices,
        authorized.as_ref(),
    )?;

    // §4 on the microVM tier. `apply_manifest` below confines the VMM HOST
    // process; the guest's own listener is governed by the port map, which is a
    // different mechanism reached through a different call. Until now nothing
    // connected them: `KrunConfig.port_map` was `Vec::new()` unconditionally and
    // fed by no manifest, so a declared listener was compiled for the host
    // process and silently ignored for the guest — the same silent-drop class
    // this branch fixed one tier down.
    //
    // What the tier can actually deliver depends on the boundary:
    //
    //   no hijacking — a guest AF_INET bind reaches nothing. Granting it would
    //     attest a listener that cannot receive a connection, which is a
    //     declaration with no effect. Refused.
    //   hijacking on — guest sockets ARE carried over vsock, so the declared
    //     port maps to itself and the empty-by-default map stops being empty.
    //     Note libkrun's constraint: an exposed port is reachable in the guest
    //     by its HOST port number, so `N:N` is the only mapping that leaves the
    //     guest's own view of §4 intact.
    // REACHABLE since #329, through exactly one route. `confinement_manifest`
    // above folds §4 from the grant's carried document — parsed and
    // digest-verified in `authorization.rs`, forwarded on
    // `ExecutionRequest.confinement_manifest`, re-parsed at this boundary —
    // and from nowhere else: LLO's own policy never sets a port, and the
    // equality contract inside the fold refuses any carried document that
    // differs from the compiled one, differing dimensions named by section.
    // So a `Some` here is always an issuer-committed listener, never caller
    // intent, and never a partial document.
    //
    // Two prior versions of this comment were each wrong in turn, in the
    // direction that misdirects hardest, and this file seems to attract the
    // failure — so, plainly: the first said the manifest could not be parsed
    // (it could; the ingest route existed), the second said `dimensions().port`
    // was always `None` here and pointed at an open design question that #329
    // closed with one field and no new machinery. If the code below and this
    // comment ever disagree again, trust the code and fix the comment in the
    // same commit that changed it.
    //
    // VERIFIED BY READING, NOT YET BY RUNNING: cloister executed the fold up
    // to `dimensions().port == Some((bind, None))`, but no live microVM has
    // exercised port_map delivery or the tsi refusal below — macOS has no
    // libkrun to run and CI has no KVM. `libkrun_guest_listener.rs` is the
    // test that does it — it self-skips via `hypervisor_or_skip()` when no
    // hypervisor is present, so it runs green-by-vacuity everywhere but real
    // Linux hardware. Run it there before calling the guest-listener path
    // proven.
    if let Some((bind, _address)) = manifest.dimensions().port {
        if config.tsi_features == 0 {
            return Err(ExecutionError::invalid(format!(
                "confinement/v1 §4 port.bind {bind} is not deliverable on the \
                 microVM tier without socket hijacking: the guest's AF_INET \
                 bind is not carried anywhere, so the listener could never be \
                 reached. Start the worker with --tsi-hijack-inet, or omit the \
                 dimension."
            )));
        }
        config.port_map = vec![
            std::ffi::CString::new(format!("{bind}:{bind}"))
                .map_err(|_| ExecutionError::invalid("port map entry contains a NUL byte"))?,
        ];
    }

    // §6 on this tier: the folded socket grants compile to vsock↔socket
    // mappings, consumed by `prepare_vm`. Derived from the same manifest the
    // digest below attests, through the pure function documented at
    // `vsock_unix_mappings` — so the receipt already covers every mapping and
    // the issuer can compute every port from the document they signed.
    config.vsock_unix_map = vsock_unix_mappings(&manifest)?;

    apply_manifest(&manifest, &config.rootfs.canonical_path)?;

    let vm: PreparedVm<'_> = prepare_vm(&api, &config)?;
    on_ready(&WorkerEvent::Ready {
        run_id: config.run_id.clone(),
        confinement_digest: manifest.confinement_digest()?,
    })?;
    vm.enter()
}
