use std::ffi::OsString;
use std::io::Read;
use std::os::fd::{BorrowedFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use rustix::fs::FileType;

use crate::{ExecutionError, ExecutionRequest, WorkerChannel};
use serde::{Deserialize, Serialize};

use super::confinement::{
    LOOPBACK_CHANNELS_ENV, apply_manifest, check_channel_host_ends, committed_confinement,
    guest_loopback_channels, guest_port_map, loopback_channels_env, vsock_unix_mappings,
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

/// Refuse a workload environment that names the guest init's mount list or
/// its loopback channels: which shares the guest mounts, and which declared
/// connections it presents where, are the worker's decision, taken from the
/// attested document.
///
/// libkrun hands the environment and the executable to the guest on the
/// kernel command line, and the guest kernel parses every `key=value` there
/// as a boot parameter before init sees what is left. So the request's part
/// of that line is held to what the kernel's quote-aware parser cannot read
/// as anything else (bead `ley-line-open-eb5e4e`). Readers that split the
/// line on whitespace and ignore quotes (init's `tsi_enabled`, arm64's
/// id-register overrides) can still see a word inside a quoted value; none
/// of them widens reach, and the gap is bead `ley-line-open-f7f862`:
///
/// - a key is `[A-Z_][A-Z0-9_]*`. Kernel boot parameters are lowercase
///   (`init=`, `rdinit=`, `console=`) or dotted module parameters, so an
///   uppercase key is never one: `{"init": "/bin/x"}` would otherwise replace
///   the guest init, and with it the datagram filter and the relay;
/// - no key starts with `KRUN_`: those are the guest init's own settings
///   (its mounts, its channels, `KRUN_INIT`, `KRUN_CONFIG`, ...), set by the
///   worker or by no one;
/// - no value contains a double quote, which would end the entry and start
///   another;
/// - the executable has no whitespace, quote or control character, because
///   libkrun writes it unquoted (`KRUN_INIT=<executable>`).
fn refuse_reserved_environment(request: &ExecutionRequest) -> Result<(), ExecutionError> {
    for reserved in [VIRTIOFS_MOUNTS_ENV, LOOPBACK_CHANNELS_ENV] {
        if request.public_environment.contains_key(reserved) {
            return Err(ExecutionError::invalid(format!(
                "publicEnvironment may not set {reserved}: the worker sets it from the \
                 attested confinement document"
            )));
        }
    }
    for (key, value) in &request.public_environment {
        let well_formed = key
            .bytes()
            .next()
            .is_some_and(|first| first.is_ascii_uppercase() || first == b'_')
            && key
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_');
        if !well_formed {
            return Err(ExecutionError::invalid(format!(
                "publicEnvironment key {key:?} is not [A-Z_][A-Z0-9_]*: the microVM tier \
                 passes the environment on the guest kernel command line, where any other \
                 key could be read as a kernel parameter (init=, console=, a module's)"
            )));
        }
        if key.starts_with("KRUN_") {
            return Err(ExecutionError::invalid(format!(
                "publicEnvironment may not set {key}: KRUN_ variables configure the guest \
                 init, and the worker sets the ones a run has"
            )));
        }
        if value.contains('"') {
            return Err(ExecutionError::invalid(format!(
                "publicEnvironment entry {key:?} contains a double quote, which would end its \
                 entry on the guest kernel command line and start another; the microVM tier \
                 refuses it"
            )));
        }
    }
    if request
        .executable
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '"')
    {
        return Err(ExecutionError::invalid(format!(
            "executable {:?} contains whitespace, a quote or a control character; libkrun \
             writes it unquoted on the guest kernel command line, where it would split \
             into further parameters",
            request.executable
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
        (grants, _) => Err(ExecutionError::workspace_unserved(format!(
            "the microVM tier attaches one workspace as {WORKSPACE_DIR}; the run is granted {}",
            grants.len()
        ))),
    }
}

/// Have the guest init mount the workspace share at [`WORKSPACE_DIR`] and
/// start the workload there. The executable stays the rootfs's: the plan
/// already anchors it at `/` (`guest_executable`), so it does not resolve
/// against the share.
fn mount_workspace(config: &mut KrunConfig) {
    config.environment.push(
        std::ffi::CString::new(format!(
            "{VIRTIOFS_MOUNTS_ENV}={WORKSPACE_TAG}:{WORKSPACE_DIR}"
        ))
        .expect("static mount entry has no NUL"),
    );
    config.workdir = std::ffi::CString::new(WORKSPACE_DIR).expect("static workdir has no NUL");
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
            use rustix::io::{fcntl_getfd, fcntl_setfd};
            for &fd in held.iter().filter(|fd| !keep.contains(fd)) {
                let fd = std::os::fd::BorrowedFd::borrow_raw(fd);
                // Closed since the listing: nothing to inherit.
                if let Ok(flags) = fcntl_getfd(fd) {
                    fcntl_setfd(fd, child_descriptor_flags(flags, false))?;
                }
            }
            for &fd in &keep {
                let fd = std::os::fd::BorrowedFd::borrow_raw(fd);
                fcntl_setfd(fd, child_descriptor_flags(fcntl_getfd(fd)?, true))?;
            }
            Ok(())
        });
    }
    Ok(())
}

/// The descriptor flags [`inherit_only`]'s child sets on a descriptor whose
/// flags are `flags`: close-on-exec cleared when the worker keeps it, set
/// otherwise, and every other flag unchanged.
///
/// A pure function so the decision is testable without forking: a descriptor
/// Rust opened is already close-on-exec, and toggling the flag instead of
/// setting it would pass exactly those descriptors to the worker.
fn child_descriptor_flags(flags: rustix::io::FdFlags, keep: bool) -> rustix::io::FdFlags {
    use rustix::io::FdFlags;
    if keep {
        flags.difference(FdFlags::CLOEXEC)
    } else {
        flags | FdFlags::CLOEXEC
    }
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
    // ONE manifest, applied and attested, and compared with the grant's
    // commitment here: before the rootfs is read, before the sandbox, and
    // long before the VM. Every projection below (the port map, the vsock
    // mappings, the loopback channels, the VMM's own nono profile) is taken
    // from `committed`, whose policy is parsed from the bytes its digest is
    // computed over (bead ley-line-open-7a3109).
    let committed = committed_confinement(request, &options.runtime_files, &options.devices)?;
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
    let manifest = committed.manifest();

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
    // So every grant in `dimensions().ports` here is an issuer-committed
    // listener, never caller intent, and never a partial document.
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
    // RUN ON A LIVE GUEST (macOS/HVF, bead ley-line-open-7d4d45):
    // `krun-worker/tests/libkrun_guest_listener.rs` declares two listeners,
    // reaches both from the host, and fails to reach a third the guest binds.
    // Running it found that libkrun's port map limited nothing — an unmapped
    // guest listen was bound at the guest's own address, and on Seatbelt a §4
    // grant gives the VMM an unqualified `network-bind` — so every guest
    // listener, on every host interface, was published. LLO libkrun patch
    // 0008 makes the map the complete list (unmapped: EACCES to the guest)
    // and binds mapped ports on loopback. The tests skip without a hypervisor
    // (`LEYLINE_REQUIRE_HYPERVISOR` makes that a failure); a Linux/KVM run is
    // still owed.
    //
    // Since ley-line-open-e2c33f, §4 is a list: every declared listener maps
    // `N:N`, through the pure `guest_port_map` so the refusal and the mapping
    // are testable without a VMM.
    config.port_map = guest_port_map(manifest, config.tsi_features)?;

    // §6 on this tier: the folded socket grants compile to vsock↔socket
    // mappings, consumed by `prepare_vm`. Derived from the same manifest the
    // digest below attests, through the pure function documented at
    // `vsock_unix_mappings` — so the receipt already covers every mapping and
    // the issuer can compute every port from the document they signed.
    config.vsock_unix_map = vsock_unix_mappings(manifest)?;

    // ADR-0046 Q7: the grants carrying a `guestPort` are presented to the
    // workload as TCP on the guest's loopback, by the guest init (LLO libkrun
    // patch 0010). The list is a pure function of the same attested document
    // (`guest_loopback_channels`), handed to init in a variable the request
    // cannot set (`refuse_reserved_environment`), so the guest-visible set is
    // exactly what the digest below commits to.
    let channels = guest_loopback_channels(manifest, config.tsi_features, std::env::consts::OS)?;
    if let Some(variable) = loopback_channels_env(&channels) {
        config.environment.push(variable);
    }

    // Every declared channel's host end can exist, or the run is refused
    // naming it, before the process confines itself and long before the VMM
    // (invariant I4; bead ley-line-open-bb2143).
    check_channel_host_ends(&config.vsock_unix_map)?;

    apply_manifest(&committed, &config.rootfs.canonical_path)?;

    Ok(PreparedRun {
        confinement_digest: committed.digest().to_owned(),
        config,
        share,
        inherited,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::ffi::{CString, OsString};
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, IntoRawFd};
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;

    use super::super::plan::{KrunConfig, ResolvedRootfs};
    use super::*;
    use crate::{
        DigestRef, ErrorCode, ResourceLimits, WorkerChannel, WorkspaceGrant, WorkspaceOperation,
    };

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
            // Anchored at `/`, as `compile_plan` passes the catalog's path.
            executable: CString::new("/bin/agent").unwrap(),
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
        // The executable is the rootfs's, wherever the workload starts.
        assert_eq!(config.executable, CString::new("/bin/agent").unwrap());
    }

    // The worker's own checks, asserted in-process. Each runs before libkrun
    // is reached, and none needs a hypervisor, so they run on every platform
    // the runtime's mutation slice does — the hypervisor tests that also
    // cross these paths skip without one.

    fn request() -> ExecutionRequest {
        ExecutionRequest {
            run_id: "run".into(),
            replay_key: "replay".into(),
            rootfs: DigestRef {
                algorithm: "blake3-256".into(),
                value: "0".repeat(64),
            },
            executable: "bin/agent".into(),
            arguments: Vec::new(),
            public_environment: BTreeMap::new(),
            allowed_egress: Vec::new(),
            confinement_digest: String::new(),
            confinement_manifest: None,
            workspaces: Vec::new(),
            limits: ResourceLimits::default(),
        }
    }

    fn grant(name: &str) -> WorkspaceGrant {
        WorkspaceGrant {
            name: name.into(),
            graph_root: format!("blake3-256:{}", "1".repeat(64)),
            expected_generation: 0,
            operations: vec![WorkspaceOperation::Read],
        }
    }

    fn arguments(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    /// A descriptor number no test process holds: far above any open file
    /// and any reuse by a parallel test.
    const NEVER_OPEN: i32 = 1 << 20;

    #[test]
    fn a_workload_cannot_set_the_guest_inits_mount_list() {
        let mut request = request();
        request
            .public_environment
            .insert("MODE".into(), "audit".into());
        assert!(refuse_reserved_environment(&request).is_ok());

        request
            .public_environment
            .insert(VIRTIOFS_MOUNTS_ENV.into(), "work:/".into());
        let error = refuse_reserved_environment(&request).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidSpec);
        assert!(error.detail.contains(VIRTIOFS_MOUNTS_ENV), "{error}");
    }

    /// Invariant I3 (bead ley-line-open-be3711): the request cannot name a
    /// loopback channel, directly or by smuggling one onto the guest kernel
    /// command line through a quote.
    #[test]
    fn a_workload_cannot_set_the_guest_inits_loopback_channels() {
        let mut named = request();
        named
            .public_environment
            .insert(LOOPBACK_CHANNELS_ENV.into(), "dial:80:65536".into());
        let error = refuse_reserved_environment(&named).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidSpec);
        assert!(error.detail.contains(LOOPBACK_CHANNELS_ENV), "{error}");

        let mut smuggled = request();
        smuggled.public_environment.insert(
            "DB_ADDR".into(),
            "x\" KRUN_LOOPBACK_CHANNELS=\"dial:80:65536".into(),
        );
        let error = refuse_reserved_environment(&smuggled).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidSpec);
        assert!(error.detail.contains("double quote"), "{error}");

        // A key the guest kernel would read as its own parameter (bead
        // ley-line-open-eb5e4e): `init=` replaces the guest init, and with it
        // the datagram filter and the relay. Any key outside [A-Z_][A-Z0-9_]*
        // is refused, and so is every KRUN_ variable.
        for key in [
            "init",
            "rdinit",
            "console",
            "virtio_vsock.x",
            "A\" tsi_hijack \"B",
            "A B",
            "A=B",
            "1ADDR",
            "",
            "http_proxy",
            "KRUN_INIT",
            "KRUN_CONFIG",
            "KRUN_INIT_PID1",
        ] {
            let mut injected = request();
            injected.public_environment.insert(key.into(), "1".into());
            let error = refuse_reserved_environment(&injected).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidSpec, "{key:?}");
            let named = format!("{key:?}");
            assert!(
                error.detail.contains(&named) || error.detail.contains(key),
                "{key:?}: {error}"
            );
        }
        for key in ["_A", "A1", "DB_ADDR", "Z"] {
            let mut fine = request();
            fine.public_environment.insert(key.into(), "1".into());
            assert!(refuse_reserved_environment(&fine).is_ok(), "{key:?}");
        }

        // libkrun writes the executable unquoted.
        for executable in [
            "bin/x init=/bin/y",
            "bin/x\tinit=/bin/y",
            "bin/\"x",
            "bin/x\n",
        ] {
            let mut hostile = request();
            hostile.executable = executable.into();
            let error = refuse_reserved_environment(&hostile).unwrap_err();
            assert!(
                error.detail.contains("executable"),
                "{executable:?}: {error}"
            );
        }

        // An ordinary address is the workload's own configuration, not
        // authority: it passes, and reaches only what a grant presents.
        let mut address = request();
        address
            .public_environment
            .insert("DB_ADDR".into(), "127.0.0.1:13306".into());
        assert!(refuse_reserved_environment(&address).is_ok());
    }

    #[test]
    fn the_share_is_the_runs_one_granted_workspace_or_nothing() {
        // No grant, no share: nothing to attach.
        assert!(workspace_share(&request(), None).unwrap().is_none());

        // A share the run is not granted.
        let (ours, _peer) = UnixStream::pair().unwrap();
        let error = workspace_share(&request(), Some(ours.as_raw_fd())).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidSpec);
        assert!(error.detail.contains("granted no workspace"), "{error}");

        let mut granted = request();
        granted.workspaces.push(grant("src"));

        // A grant without a share.
        let error = workspace_share(&granted, None).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidSpec);
        assert!(error.detail.contains("\"src\""), "{error}");

        // A descriptor that is not open, and one that is not a socket.
        let error = workspace_share(&granted, Some(NEVER_OPEN)).unwrap_err();
        assert!(error.detail.contains("is not open"), "{error}");
        let file = std::fs::File::open("/dev/null").unwrap();
        let error = workspace_share(&granted, Some(file.as_raw_fd())).unwrap_err();
        assert!(error.detail.contains("is not a socket"), "{error}");

        // The granted share: the socket comes back, owned, still connected.
        let (share, mut peer) = UnixStream::pair().unwrap();
        let mut share = workspace_share(&granted, Some(share.into_raw_fd()))
            .unwrap()
            .expect("the granted share");
        share.write_all(b"x").unwrap();
        let mut byte = [0u8; 1];
        peer.read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"x");

        // Two workspaces: the tier attaches one.
        granted.workspaces.push(grant("docs"));
        let error = workspace_share(&granted, None).unwrap_err();
        assert_eq!(error.code, ErrorCode::WorkspaceUnserved);
    }

    #[test]
    fn undeclared_is_what_is_neither_a_standard_stream_nor_declared() {
        let declared = [InheritedDescriptor {
            descriptor: 5,
            channel: WorkerChannel::WorkspaceShare {
                workspace: "src".into(),
                graph_root: String::new(),
            },
        }];
        let held: BTreeSet<i32> = [0, 1, 2, 5, 7, 9].into();
        assert_eq!(undeclared_descriptors(&held, &declared), vec![7, 9]);
        // Nothing beyond what is declared: nothing undeclared.
        let held: BTreeSet<i32> = [0, 1, 2, 5].into();
        assert_eq!(undeclared_descriptors(&held, &declared), Vec::<i32>::new());
        // Without the declaration, the share itself is undeclared.
        assert_eq!(undeclared_descriptors(&held, &[]), vec![5]);
    }

    #[test]
    fn a_held_descriptor_nothing_declares_is_refused() {
        let file = std::fs::File::open("/dev/null").unwrap();
        let error = refuse_undeclared_descriptors(&[]).unwrap_err();
        assert_eq!(error.code, ErrorCode::IdentityPolicyMismatch);
        assert!(
            error.detail.contains(&file.as_raw_fd().to_string()),
            "{error}"
        );
    }

    #[test]
    fn the_spawned_worker_keeps_exactly_the_descriptors_it_is_handed() {
        use rustix::io::FdFlags;
        // A descriptor Rust opened is already close-on-exec; it must stay so.
        assert_eq!(
            child_descriptor_flags(FdFlags::CLOEXEC, false),
            FdFlags::CLOEXEC
        );
        assert_eq!(
            child_descriptor_flags(FdFlags::empty(), false),
            FdFlags::CLOEXEC
        );
        assert_eq!(
            child_descriptor_flags(FdFlags::CLOEXEC, true),
            FdFlags::empty()
        );
        assert_eq!(
            child_descriptor_flags(FdFlags::empty(), true),
            FdFlags::empty()
        );
    }

    #[test]
    fn worker_options_parse_every_option_once() {
        let options = WorkerOptions::parse(arguments(&[
            "--cas-root",
            "/cas",
            "--run-root",
            "/runs",
            "--runtime-file",
            "/lib/a",
            "--runtime-file",
            "/lib/b",
            "--device",
            "/dev/kvm",
            "--tsi-hijack-inet",
            WORKSPACE_SHARE_FD_ARG,
            "3",
        ]))
        .unwrap();
        assert_eq!(
            options,
            WorkerOptions {
                cas_root: PathBuf::from("/cas"),
                run_root: PathBuf::from("/runs"),
                runtime_files: vec![PathBuf::from("/lib/a"), PathBuf::from("/lib/b")],
                devices: vec![PathBuf::from("/dev/kvm")],
                tsi_hijack_inet: true,
                workspace_share_fd: Some(3),
            }
        );

        // Descriptor 0 is a number like any other.
        let options = WorkerOptions::parse(arguments(&[
            "--cas-root",
            "/cas",
            "--run-root",
            "/runs",
            WORKSPACE_SHARE_FD_ARG,
            "0",
        ]))
        .unwrap();
        assert_eq!(options.workspace_share_fd, Some(0));
        assert!(!options.tsi_hijack_inet);
    }

    /// The worker compares the confinement it will apply with the grant's
    /// commitment BEFORE the VM exists (bead `ley-line-open-7a3109`).
    ///
    /// The supervisor's comparison reads the readiness event, which the worker
    /// writes immediately before `krun_start_enter`, so on its own it refused
    /// a drifting run only after the guest was already running. Here the
    /// refusal comes first: the CAS root does not exist, so a worker that
    /// reached the rootfs, the VMM or the sandbox would fail on that instead,
    /// and the drift error proves none of them was reached.
    #[test]
    fn a_worker_refuses_a_drifting_commitment_before_it_reads_the_rootfs() {
        let options = WorkerOptions {
            cas_root: PathBuf::from("/nonexistent/leyline-cas"),
            run_root: PathBuf::from("/nonexistent/leyline-runs"),
            runtime_files: Vec::new(),
            devices: Vec::new(),
            tsi_hijack_inet: false,
            workspace_share_fd: None,
        };
        let mut drifting = request();
        drifting.limits.wall_time_ms = 1_000;
        drifting.confinement_digest = format!("blake3-256:{}", "f".repeat(64));
        let error = prepare(options, &drifting).unwrap_err();
        assert_eq!(error.code, ErrorCode::IdentityPolicyMismatch, "{error}");
        assert!(error.detail.contains("confinement drift"), "{error}");
        assert!(
            error.detail.contains(&drifting.confinement_digest),
            "{error}"
        );
    }

    /// "No commitment" is stated, never defaulted: a worker request that
    /// omits `confinementDigest` is refused, so a producer that forgot the
    /// field cannot reach the attest-with-nothing-to-compare mode by accident.
    /// An empty string is the explicit spelling of that mode.
    #[test]
    fn a_worker_request_without_a_confinement_digest_key_is_refused() {
        let mut value = serde_json::to_value(request()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("confinementDigest")
            .expect("the request serializes the key");
        let error = read_request(value.to_string().as_bytes()).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidSpec);
        assert!(error.detail.contains("confinementDigest"), "{error}");
        // Present and empty: accepted.
        read_request(serde_json::to_string(&request()).unwrap().as_bytes()).unwrap();
    }

    #[test]
    fn worker_options_refuse_duplicates_and_malformed_values() {
        let base = ["--cas-root", "/cas", "--run-root", "/runs"];
        for extra in [
            &["--cas-root", "/other"][..],
            &["--run-root", "/other"][..],
            &[WORKSPACE_SHARE_FD_ARG, "3", WORKSPACE_SHARE_FD_ARG, "4"][..],
            &[WORKSPACE_SHARE_FD_ARG, "-1"][..],
            &[WORKSPACE_SHARE_FD_ARG, "three"][..],
            &["--unknown", "x"][..],
            &["--device"][..],
        ] {
            let mut values = base.to_vec();
            values.extend_from_slice(extra);
            let error = WorkerOptions::parse(arguments(&values)).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidSpec, "{values:?}: {error}");
        }
        for missing in [&["--run-root", "/runs"][..], &["--cas-root", "/cas"][..]] {
            assert!(
                WorkerOptions::parse(arguments(missing)).is_err(),
                "{missing:?}"
            );
        }
    }
}
