use std::ffi::OsString;
use std::io::Read;
use std::os::fd::{BorrowedFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use rustix::fs::FileType;

use crate::confinement::ConfinementManifest;
use crate::{ExecutionError, ExecutionRequest, WorkerChannel};
use serde::{Deserialize, Serialize};

use super::confinement::{
    VmmHostResources, apply_manifest, confinement_manifest, vsock_unix_mappings,
};
use super::plan::{DirectoryRootfsResolver, KRUN_TSI_HIJACK_INET, KrunConfig, compile_plan};
use super::volume::{materialize_ephemeral_rootfs, verify_ephemeral_rootfs};

/// The worker option naming the inherited descriptor of the run's workspace
/// share: one end of a connected UNIX socket whose peer, in the daemon,
/// answers the share's FUSE requests (`super::share`).
pub const WORKSPACE_SHARE_FD_ARG: &str = "--workspace-share-fd";

/// The virtio-fs tag of the workspace share.
pub const WORKSPACE_TAG: &str = "work";

/// Where the guest sees its workspace, and its working directory.
pub const WORKSPACE_DIR: &str = "/work";

/// The guest init's mount list (LLO libkrun patch 0005): comma-separated
/// `tag:directory` pairs init mounts as virtio-fs before the workload runs.
pub const VIRTIOFS_MOUNTS_ENV: &str = "KRUN_VIRTIOFS_MOUNTS";

/// Refuse a workload environment that names the guest init's mount list:
/// which shares the guest mounts, and where, is the worker's decision.
fn refuse_reserved_environment(request: &ExecutionRequest) -> Result<(), ExecutionError> {
    if request.public_environment.contains_key(VIRTIOFS_MOUNTS_ENV) {
        return Err(ExecutionError::invalid(format!(
            "publicEnvironment may not set {VIRTIOFS_MOUNTS_ENV}: the worker sets the \
             guest's virtio-fs mounts"
        )));
    }
    Ok(())
}

/// The run's workspace share: the inherited socket named by
/// [`WORKSPACE_SHARE_FD_ARG`], when the run is granted a workspace. A share
/// without a grant, a grant without a share, and a descriptor that is not a
/// socket are refused; the tier attaches one workspace.
fn workspace_share(
    request: &ExecutionRequest,
    fd: Option<i32>,
) -> Result<Option<UnixStream>, ExecutionError> {
    match (request.workspaces.as_slice(), fd) {
        ([], None) => Ok(None),
        ([], Some(_)) => Err(ExecutionError::invalid(
            "the worker was handed a workspace share, and the run is granted no workspace",
        )),
        ([grant], None) => Err(ExecutionError::invalid(format!(
            "the run is granted workspace {:?}, and the worker was handed no share",
            grant.name
        ))),
        ([_], Some(fd)) => {
            // SAFETY: borrowed only for the fstat below; ownership is taken
            // once the descriptor is known to be an open socket.
            let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
            let stat = rustix::fs::fstat(borrowed).map_err(|error| {
                ExecutionError::invalid(format!(
                    "workspace share descriptor {fd} is not open: {error}"
                ))
            })?;
            if FileType::from_raw_mode(stat.st_mode as _) != FileType::Socket {
                return Err(ExecutionError::invalid(format!(
                    "workspace share descriptor {fd} is not a socket"
                )));
            }
            // SAFETY: the descriptor is open, a socket, and inherited for
            // this purpose alone; nothing else in the worker names it.
            Ok(Some(unsafe { UnixStream::from_raw_fd(fd) }))
        }
        (grants, _) => Err(ExecutionError::unsupported(format!(
            "the microVM tier attaches one workspace as {WORKSPACE_DIR}; the run is granted {}",
            grants.len()
        ))),
    }
}

/// Have the guest init mount the workspace share at [`WORKSPACE_DIR`] and
/// start the workload there. The executable stays the rootfs's: the plan's
/// guest-relative path is anchored at `/`, since init would otherwise
/// resolve it against the share.
fn mount_workspace(config: &mut KrunConfig) {
    config.environment.push(
        std::ffi::CString::new(format!(
            "{VIRTIOFS_MOUNTS_ENV}={WORKSPACE_TAG}:{WORKSPACE_DIR}"
        ))
        .expect("static mount entry has no NUL"),
    );
    config.workdir = std::ffi::CString::new(WORKSPACE_DIR).expect("static workdir has no NUL");
    if !config.executable.as_bytes().starts_with(b"/") {
        let mut absolute = b"/".to_vec();
        absolute.extend_from_slice(config.executable.as_bytes());
        config.executable =
            std::ffi::CString::new(absolute).expect("a CString's bytes have no NUL");
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerOptions {
    pub cas_root: PathBuf,
    pub run_root: PathBuf,
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
    /// The inherited descriptor of the run's workspace share
    /// ([`WORKSPACE_SHARE_FD_ARG`]), set by the backend that spawned this
    /// worker for a run granted a workspace.
    pub workspace_share_fd: Option<i32>,
}

/// One descriptor a worker holds beyond its standard streams: its number in
/// the worker, and the channel it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InheritedDescriptor {
    pub descriptor: i32,
    pub channel: WorkerChannel,
}

/// The descriptors a worker for `request`, handed the share descriptor
/// `share_fd`, holds beyond its standard streams: the share, for a run
/// granted one workspace, and nothing else.
///
/// The one declaration both processes derive: the backend from what it
/// hands the worker, the worker from what it was handed. The worker refuses
/// to start holding any descriptor this does not name
/// ([`refuse_undeclared_descriptors`]), and the backend refuses a readiness
/// that declares anything else.
pub fn declared_descriptors(
    request: &ExecutionRequest,
    share_fd: Option<i32>,
) -> Vec<InheritedDescriptor> {
    match (request.workspaces.as_slice(), share_fd) {
        ([grant], Some(descriptor)) => vec![InheritedDescriptor {
            descriptor,
            channel: WorkerChannel::WorkspaceShare {
                workspace: grant.name.clone(),
                graph_root: grant.graph_root.clone(),
            },
        }],
        // Every other pairing is refused by `workspace_share` before the
        // declaration is used.
        _ => Vec::new(),
    }
}

/// Standard input (the request), output, and error (readiness and failure
/// events): the descriptors every worker holds without declaring them.
const STANDARD_STREAMS: [i32; 3] = [0, 1, 2];

/// The descriptors in `held` that are neither a standard stream nor
/// declared.
fn undeclared_descriptors(
    held: &std::collections::BTreeSet<i32>,
    declared: &[InheritedDescriptor],
) -> Vec<i32> {
    held.iter()
        .copied()
        .filter(|fd| {
            !STANDARD_STREAMS.contains(fd) && !declared.iter().any(|d| d.descriptor == *fd)
        })
        .collect()
}

/// Every descriptor open in this process, listed from `/dev/fd` (on Linux a
/// link to `/proc/self/fd`). The listing's own directory descriptor appears
/// in it and is closed once the listing is dropped, so only the descriptors
/// still open afterwards are answered.
fn held_descriptors() -> Result<std::collections::BTreeSet<i32>, ExecutionError> {
    let listed: Vec<i32> = std::fs::read_dir("/dev/fd")
        .and_then(|entries| {
            entries
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|error| {
            ExecutionError::backend(format!(
                "list this worker's open descriptors in /dev/fd: {error}"
            ))
        })?
        .iter()
        .filter_map(|name| name.to_str()?.parse().ok())
        .collect();
    Ok(listed
        .into_iter()
        .filter(|&fd| {
            // SAFETY: borrowed for one fcntl that only reads the flags; a
            // closed number answers EBADF.
            rustix::io::fcntl_getfd(unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }).is_ok()
        })
        .collect())
}

/// Refuse to run holding a descriptor `declared` does not name.
///
/// An inherited descriptor is authority no sandbox profile governs: nono
/// confines what this process may open, not what it already holds. So the
/// worker checks, before it reads the rootfs or applies its confinement,
/// that what it holds is its standard streams and its declared channels and
/// nothing else, and refuses otherwise rather than carry an undescribed
/// channel into the run.
pub fn refuse_undeclared_descriptors(
    declared: &[InheritedDescriptor],
) -> Result<(), ExecutionError> {
    let undeclared = undeclared_descriptors(&held_descriptors()?, declared);
    if undeclared.is_empty() {
        return Ok(());
    }
    let declared: Vec<i32> = declared.iter().map(|d| d.descriptor).collect();
    Err(ExecutionError::identity_mismatch(format!(
        "the worker holds undeclared inherited descriptor(s) {undeclared:?}: it declares \
         its standard streams and {declared:?}, and starts with nothing else"
    )))
}

/// Have `command`'s child inherit its standard streams and `keep`, and no
/// other descriptor of this process.
///
/// Every descriptor Rust's standard library opens is close-on-exec, but a
/// process can inherit others from its own parent (a build tool's jobserver
/// pipes, a shell's redirections) or open them through C code, and each
/// would pass on to the worker, which refuses to start holding them
/// ([`refuse_undeclared_descriptors`]). So the spawner lists its open
/// descriptors before the fork and the child marks each one above the
/// standard streams close-on-exec, except `keep`, whose flag it clears. A
/// descriptor opened without close-on-exec between the listing and the fork
/// still reaches the worker, and the worker's refusal is what catches it.
pub fn inherit_only(command: &mut Command, keep: &[i32]) -> Result<(), ExecutionError> {
    let held: Vec<i32> = held_descriptors()?
        .into_iter()
        .filter(|fd| !STANDARD_STREAMS.contains(fd))
        .collect();
    let keep = keep.to_vec();
    // SAFETY: the closure runs in the forked child before exec, allocates
    // nothing, and makes only fcntl calls, which are async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};
            for &fd in held.iter().filter(|fd| !keep.contains(fd)) {
                let fd = std::os::fd::BorrowedFd::borrow_raw(fd);
                // Closed since the listing: nothing to inherit.
                if let Ok(flags) = fcntl_getfd(fd) {
                    fcntl_setfd(fd, flags | FdFlags::CLOEXEC)?;
                }
            }
            for &fd in &keep {
                let fd = std::os::fd::BorrowedFd::borrow_raw(fd);
                fcntl_setfd(fd, fcntl_getfd(fd)?.difference(FdFlags::CLOEXEC))?;
            }
            Ok(())
        });
    }
    Ok(())
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
        /// Every descriptor the worker holds beyond its standard streams
        /// ([`declared_descriptors`]), verified open before readiness.
        #[serde(default)]
        inherited: Vec<InheritedDescriptor>,
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
        let mut devices = Vec::new();

        let mut tsi_hijack_inet = false;
        let mut workspace_share_fd = None;

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
                Some("--run-root") if run_root.is_none() => run_root = Some(value.into()),
                Some("--runtime-file") => runtime_files.push(value.into()),
                Some("--device") => devices.push(value.into()),
                Some(WORKSPACE_SHARE_FD_ARG) if workspace_share_fd.is_none() => {
                    let fd = value
                        .to_str()
                        .and_then(|value| value.parse::<i32>().ok())
                        .filter(|fd| *fd >= 0)
                        .ok_or_else(|| {
                            ExecutionError::invalid(format!(
                                "{WORKSPACE_SHARE_FD_ARG} {} is not a file descriptor number",
                                value.to_string_lossy()
                            ))
                        })?;
                    workspace_share_fd = Some(fd);
                }
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
            runtime_files,
            devices,
            tsi_hijack_inet,
            workspace_share_fd,
        })
    }
}

/// The worker's request, read from its standard input: one JSON
/// [`ExecutionRequest`], complete as soon as its closing brace arrives.
pub fn read_request(reader: impl Read) -> Result<ExecutionRequest, ExecutionError> {
    crate::backends::process::read_request(reader)
        .map_err(|error| ExecutionError::invalid(format!("invalid worker request JSON: {error}")))
}

/// A run the worker checked, planned, materialized and confined, ready for
/// the VMM: everything [`prepare`] does before libkrun is called. The
/// `leyline-krun-worker` crate links libkrun, builds the VM from
/// [`PreparedRun::config`], attaches [`PreparedRun::share`], reports
/// [`PreparedRun::ready`] and enters the VM.
#[derive(Debug)]
pub struct PreparedRun {
    /// The VM to build: the compiled plan with the workspace mount, the
    /// operator's socket hijacking, the port map and the vsock mappings.
    pub config: KrunConfig,
    /// The run's workspace share, when the run is granted a workspace: the
    /// socket the guest's `/work` device forwards its FUSE requests over.
    pub share: Option<UnixStream>,
    /// The digest of the confinement manifest this process applied.
    pub confinement_digest: String,
    /// The descriptors this process holds beyond its standard streams.
    pub inherited: Vec<InheritedDescriptor>,
}

impl PreparedRun {
    /// The readiness event this run reports once its VM is built.
    pub fn ready(&self) -> WorkerEvent {
        WorkerEvent::Ready {
            run_id: self.config.run_id.clone(),
            confinement_digest: self.confinement_digest.clone(),
            inherited: self.inherited.clone(),
        }
    }
}

/// Check, plan, materialize and confine a run, up to the VMM.
///
/// On return the process is confined by the manifest whose digest
/// [`PreparedRun::confinement_digest`] names; everything after it — building
/// the VM and entering it — happens under that confinement.
pub fn prepare(
    options: WorkerOptions,
    request: &ExecutionRequest,
) -> Result<PreparedRun, ExecutionError> {
    // Checked before anything is read or materialized: the share is the
    // run's granted workspace or nothing, and the guest init's mount list
    // is the worker's alone.
    refuse_reserved_environment(request)?;
    let share = workspace_share(request, options.workspace_share_fd)?;
    // Before the rootfs is read or anything else is opened: what this
    // process holds is its standard streams and the declared share.
    let inherited = declared_descriptors(request, options.workspace_share_fd);
    refuse_undeclared_descriptors(&inherited)?;
    let resolver = DirectoryRootfsResolver::new(&options.cas_root);
    let mut config = compile_plan(&resolver, request)?;
    if share.is_some() {
        mount_workspace(&mut config);
    }
    // `compile_plan` hardcodes 0 — a workload cannot widen its own boundary
    // through the request. Hijacking is applied here, from the operator's
    // command line, and nowhere else.
    if options.tsi_hijack_inet {
        config.tsi_features = KRUN_TSI_HIJACK_INET;
    }
    config.rootfs = materialize_ephemeral_rootfs(&config.rootfs, &options.run_root)?;

    // libkrun is linked into this binary; the only library it loads is
    // libkrunfw, at `krun_create_ctx` inside `prepare_vm` — after nono is
    // applied — so libkrunfw must be among the `--runtime-file` grants. All VM
    // configuration and execution happen after ambient authority is dropped.
    verify_ephemeral_rootfs(&config.rootfs)?;
    let resources = VmmHostResources {
        runtime_files: options.runtime_files,
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

    Ok(PreparedRun {
        confinement_digest: manifest.confinement_digest()?,
        config,
        share,
        inherited,
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;
    use std::path::PathBuf;

    use super::super::plan::{KrunConfig, ResolvedRootfs};
    use super::mount_workspace;
    use crate::DigestRef;

    #[test]
    fn a_mounted_workspace_is_the_guests_working_directory() {
        let mut config = KrunConfig {
            run_id: "run".into(),
            rootfs: ResolvedRootfs {
                digest: DigestRef {
                    algorithm: "blake3-256".into(),
                    value: "0".repeat(64),
                },
                canonical_path: PathBuf::from("/rootfs"),
            },
            // Guest-relative, as `compile_plan` passes the catalog's path.
            executable: CString::new("bin/agent").unwrap(),
            arguments: Vec::new(),
            environment: vec![CString::new("MODE=audit").unwrap()],
            workdir: CString::new("/").unwrap(),
            vcpus: 1,
            ram_mib: 256,
            wall_time_ms: 1_000,
            tsi_features: 0,
            port_map: Vec::new(),
            vsock_unix_map: Vec::new(),
        };
        mount_workspace(&mut config);
        assert_eq!(
            config.environment,
            vec![
                CString::new("MODE=audit").unwrap(),
                CString::new("KRUN_VIRTIOFS_MOUNTS=work:/work").unwrap(),
            ]
        );
        assert_eq!(config.workdir, CString::new("/work").unwrap());
        // The executable is the rootfs's, wherever the workload starts: a
        // relative path would resolve against /work, the share.
        assert_eq!(config.executable, CString::new("/bin/agent").unwrap());

        let mut absolute = config;
        absolute.executable = CString::new("/usr/bin/agent").unwrap();
        mount_workspace(&mut absolute);
        assert_eq!(absolute.executable, CString::new("/usr/bin/agent").unwrap());
    }
}
