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

/// Refuse a workload environment entry that cannot be one, or that names the
/// guest init's own settings, and an executable the kernel command line
/// cannot carry.
///
/// The workload's argv and environment reach the guest through the exec
/// config file ([`EXEC_CONFIG_PATH`], LLO libkrun patch 0013), never through
/// the kernel command line, which the kernel and init re-parse (beads
/// `ley-line-open-7410ec`, `d4b07f`, `f7f862`). So a key or value is the
/// workload's own bytes and is held only to what an environment can hold:
///
/// - a key is non-empty, and holds neither `=` nor NUL; a value holds no NUL;
/// - no key starts with `KRUN_`. Init reads some of its settings from its
///   environment after it applies the file's (`KRUN_RLIMITS`,
///   `KRUN_WORKDIR`, `KRUN_INIT`, `KRUN_INIT_PID1`), so such a key would
///   configure init: its settings are set by the worker or by no one.
///
/// The executable still rides the command line, as libkrun's `KRUN_INIT`
/// (required by `krun_set_exec`, ignored by init under `KRUN_CONFIG_ARGV`):
/// it is printable ASCII with no space or quote, which libkrun writes
/// unquoted and whose command-line builder accepts.
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
        if key.is_empty() || key.contains('=') || key.contains('\0') {
            return Err(ExecutionError::invalid(format!(
                "publicEnvironment key {key:?} cannot name an environment variable: it is \
                 empty or holds `=` or NUL"
            )));
        }
        if key.starts_with("KRUN_") {
            return Err(ExecutionError::invalid(format!(
                "publicEnvironment may not set {key}: KRUN_ variables configure the guest \
                 init, and the worker sets the ones a run has"
            )));
        }
        if value.contains('\0') {
            return Err(ExecutionError::invalid(format!(
                "publicEnvironment value of {key:?} holds NUL, which no environment can carry"
            )));
        }
    }
    if !request
        .executable
        .bytes()
        .all(|byte| byte.is_ascii_graphic() && byte != b'"')
    {
        return Err(ExecutionError::invalid(format!(
            "executable {:?} contains whitespace, a quote, a control or a non-ASCII \
             character; libkrun writes it unquoted on the guest kernel command line, where \
             it would split into further parameters",
            request.executable
        )));
    }
    Ok(())
}

/// Where the worker writes the workload's argv and environment in the run's
/// own root, and where the guest init reads them (`KRUN_CONFIG`; LLO libkrun
/// patch 0013, beads `ley-line-open-7410ec`, `d4b07f`). Init removes it once
/// read. A rootfs that holds this path is refused: the write is exclusive.
// Same as rootfs_manifest::RUN_EXEC_SPEC (D3). Whichever branch lands second
// replaces this literal with that shared const.
pub const EXEC_CONFIG_PATH: &str = "/.leyline-exec.json";

/// The most argv and environment strings one run carries: init's JSON
/// reader holds 16384 tokens, five of them the file's own structure.
pub const MAX_EXEC_STRINGS: usize = 16_000;

/// The longest one argv or environment string may be, in bytes: the
/// kernel's per-string execve limit (`MAX_ARG_STRLEN`, 32 pages) holds the
/// terminating NUL too.
pub const MAX_EXEC_STRING_BYTES: usize = 128 * 1024;

/// The most bytes argv and environment may take together, NULs included:
/// well inside the kernel's execve limit (a quarter of the guest's stack
/// limit, 2 MiB by default).
pub const MAX_EXEC_TOTAL_BYTES: usize = 1024 * 1024;

/// The workload's argv (the executable, then the request's arguments) and
/// environment (`KEY=VALUE`), as the guest init execs them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecConfig {
    pub cmd: Vec<String>,
    pub env: Vec<String>,
}

impl ExecConfig {
    /// The file's bytes: `{"Cmd": [...], "Env": [...]}`. serde_json writes
    /// `"` and `\` escaped, `\b \t \n \f \r` as two-character escapes, every
    /// other C0 control as `\u00XX`, and every other character as its own
    /// UTF-8: exactly the set the guest init decodes.
    pub fn to_json(&self) -> String {
        serde_json::json!({ "Cmd": self.cmd, "Env": self.env }).to_string()
    }
}

/// The workload's argv and environment for `request`, whose executable the
/// guest resolves as `executable`, held to what the guest can exec:
/// bounded in count, per-string length and total size, each refused by name.
pub fn exec_config(
    request: &ExecutionRequest,
    executable: &str,
) -> Result<ExecConfig, ExecutionError> {
    let cmd: Vec<String> = std::iter::once(executable.to_owned())
        .chain(request.arguments.iter().cloned())
        .collect();
    let env: Vec<String> = request
        .public_environment
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    if cmd.len() + env.len() > MAX_EXEC_STRINGS {
        return Err(ExecutionError::invalid(format!(
            "the run's argv and environment hold {} strings; the microVM tier carries at \
             most {MAX_EXEC_STRINGS}",
            cmd.len() + env.len()
        )));
    }
    for argument in &request.arguments {
        if argument.contains('\0') {
            return Err(ExecutionError::invalid(
                "an argument holds NUL, which no argv can carry",
            ));
        }
        if argument.len() >= MAX_EXEC_STRING_BYTES {
            return Err(ExecutionError::invalid(format!(
                "an argument is {} bytes; the guest kernel takes at most {} per string",
                argument.len(),
                MAX_EXEC_STRING_BYTES - 1
            )));
        }
    }
    for (key, value) in &request.public_environment {
        if key.len() + 1 + value.len() >= MAX_EXEC_STRING_BYTES {
            return Err(ExecutionError::invalid(format!(
                "publicEnvironment entry {key:?} is {} bytes; the guest kernel takes at most \
                 {} per string",
                key.len() + 1 + value.len(),
                MAX_EXEC_STRING_BYTES - 1
            )));
        }
    }
    let total: usize = cmd.iter().chain(&env).map(|string| string.len() + 1).sum();
    if total > MAX_EXEC_TOTAL_BYTES {
        return Err(ExecutionError::invalid(format!(
            "the run's argv and environment take {total} bytes; the microVM tier carries at \
             most {MAX_EXEC_TOTAL_BYTES}"
        )));
    }
    Ok(ExecConfig { cmd, env })
}

/// Take the workload's argv and environment off the kernel command line:
/// libkrun writes the plan's arguments after ` -- ` and its environment as
/// `KEY="value"`, both re-parsed by the guest kernel and init. The plan
/// keeps the executable (libkrun requires one) and, as its environment,
/// only the tokens that point init at the exec config file. Everything the
/// worker adds afterwards (the workspace mount, the loopback channels) is
/// its own token.
pub fn move_exec_off_the_kernel_line(config: &mut KrunConfig) {
    config.arguments.clear();
    config.environment = vec![
        std::ffi::CString::new(format!("KRUN_CONFIG={EXEC_CONFIG_PATH}"))
            .expect("static entry has no NUL"),
        std::ffi::CString::new("KRUN_CONFIG_ARGV=1").expect("static entry has no NUL"),
    ];
}

/// Write `config` at [`EXEC_CONFIG_PATH`] under the run's root `rootfs`:
/// created exclusively, never through a link, read-only. The root is the
/// per-run copy, a fresh directory; a rootfs that already holds the path
/// (an image file there, a link to a host file) is refused by name, and
/// nothing is written through it.
pub fn write_exec_config(
    rootfs: &std::path::Path,
    config: &ExecConfig,
) -> Result<(), ExecutionError> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let path = rootfs.join(EXEC_CONFIG_PATH.trim_start_matches('/'));
    // `create_new` is O_CREAT|O_EXCL, which fails on any existing entry and
    // does not follow a symbolic link at the final component (POSIX), so a
    // link the rootfs placed there is refused rather than written through.
    // std opens close-on-exec.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .open(&path)
        .map_err(|error| {
            ExecutionError::invalid(format!(
                "write the workload's exec config at {EXEC_CONFIG_PATH} in the run's root: \
                 {error}; the path is the worker's, and a rootfs may not hold it"
            ))
        })?;
    file.write_all(config.to_json().as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            ExecutionError::backend(format!(
                "write the workload's exec config at {EXEC_CONFIG_PATH}: {error}"
            ))
        })
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
    // The workload's argv and environment, bounded and refused by name here,
    // before anything is materialized, then taken off the kernel command
    // line (beads ley-line-open-7410ec, d4b07f, f7f862). They reach the
    // guest as the exec config file, written below.
    let exec = exec_config(
        request,
        config
            .executable
            .to_str()
            .map_err(|_| ExecutionError::invalid("executable is not UTF-8"))?,
    )?;
    move_exec_off_the_kernel_line(&mut config);
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
    // After the last verification of the per-run root ("the declared tree
    // and nothing else"), before the sandbox and long before the VM: the one
    // file the worker adds there. Nothing verifies the root again, and the
    // guest exists only once the VM is entered, so nothing can race it.
    write_exec_config(&config.rootfs.canonical_path, &exec)?;
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
            write_scope: Vec::new(),
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

        // A value is delivered as the workload's own bytes through the exec
        // config file (beads ley-line-open-7410ec, d4b07f), never through
        // the kernel command line, so a quote cannot end it and start a
        // channel: it is data, and it is admitted.
        let mut quoted = request();
        quoted.public_environment.insert(
            "DB_ADDR".into(),
            "x\" KRUN_LOOPBACK_CHANNELS=\"dial:80:65536".into(),
        );
        assert!(refuse_reserved_environment(&quoted).is_ok());
        assert_eq!(
            exec_config(&quoted, "/bin/agent").unwrap().env,
            vec!["DB_ADDR=x\" KRUN_LOOPBACK_CHANNELS=\"dial:80:65536".to_owned()]
        );

        // What cannot be an environment entry at all, and the guest init's
        // own settings (KRUN_*: its mounts, its channels, its config file,
        // its rlimits), which the worker sets or no one does.
        for key in [
            "A=B",
            "",
            "A\0B",
            "KRUN_INIT",
            "KRUN_CONFIG",
            "KRUN_CONFIG_ARGV",
            "KRUN_INIT_PID1",
            "KRUN_RLIMITS",
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
        // Off the kernel command line, a key is any name an environment can
        // hold: the kernel never sees it, so `init` or `console` is the
        // workload's own variable, not a boot parameter.
        for key in [
            "_A",
            "A1",
            "DB_ADDR",
            "Z",
            "init",
            "rdinit",
            "console",
            "virtio_vsock.x",
            "http_proxy",
            "1ADDR",
            "A B",
            "A\" tsi_hijack \"B",
        ] {
            let mut fine = request();
            fine.public_environment.insert(key.into(), "1".into());
            assert!(refuse_reserved_environment(&fine).is_ok(), "{key:?}");
        }

        // The executable still rides the kernel command line (libkrun's
        // KRUN_INIT, which init ignores under KRUN_CONFIG_ARGV but libkrun
        // requires), unquoted and printable ASCII only.
        for executable in [
            "bin/x init=/bin/y",
            "bin/x\tinit=/bin/y",
            "bin/\"x",
            "bin/x\n",
            "bin/caf\u{e9}",
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

    /// Every byte an argv or environment string can hold (0x01-0x7F, and
    /// multi-byte UTF-8), with the shapes that broke on the kernel command
    /// line: `--`, `--x`, quotes, a backslash, a newline, `a, b`.
    fn hostile_strings() -> Vec<String> {
        let mut every_ascii: String = (1u8..=0x7f).map(char::from).collect();
        every_ascii.push_str(" caf\u{e9} \u{1F600} \u{4e2d}");
        vec![
            "a".into(),
            "--".into(),
            "b".into(),
            "--x".into(),
            "\"".into(),
            "\\".into(),
            "'".into(),
            "line1\nline2".into(),
            "a, b".into(),
            "x=\"ab\"; exit 93".into(),
            "caf\u{e9}".into(),
            "".into(),
            every_ascii,
        ]
    }

    /// Beads ley-line-open-7410ec, d4b07f: the workload's argv and
    /// environment are the exec config's, verbatim, and the JSON the worker
    /// writes uses only escapes the guest init decodes (LLO libkrun patch
    /// 0013): `\"`, `\\`, `\b`, `\f`, `\n`, `\r`, `\t`, and `\u00XX` for the
    /// other C0 controls. Every other character is its own UTF-8.
    #[test]
    fn the_exec_config_carries_argv_and_environment_verbatim() {
        let mut request = request();
        request.arguments = hostile_strings();
        request
            .public_environment
            .insert("MULTI".into(), "l1\nl2\t\"q\"\\".into());
        request
            .public_environment
            .insert("lower.case".into(), hostile_strings().pop().unwrap());
        let config = exec_config(&request, "/bin/agent").unwrap();
        let mut argv = vec!["/bin/agent".to_owned()];
        argv.extend(hostile_strings());
        assert_eq!(config.cmd, argv);
        assert_eq!(
            config.env,
            vec![
                "MULTI=l1\nl2\t\"q\"\\".to_owned(),
                format!("lower.case={}", hostile_strings().pop().unwrap()),
            ]
        );

        let json = config.to_json();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["Cmd"], serde_json::json!(argv));
        // Only the escapes init decodes appear.
        let bytes = json.as_bytes();
        let mut at = 0;
        while at < bytes.len() {
            if bytes[at] == b'\\' {
                let escape = bytes[at + 1];
                assert!(
                    b"\"\\bfnrtu".contains(&escape),
                    "escape \\{} in {json}",
                    escape as char
                );
                if escape == b'u' {
                    let hex = std::str::from_utf8(&bytes[at + 2..at + 6]).unwrap();
                    let code = u32::from_str_radix(hex, 16).unwrap();
                    assert!(code < 0x20, "\\u{hex} is not a C0 control");
                    at += 6;
                } else {
                    at += 2;
                }
            } else {
                assert!(bytes[at] >= 0x20, "a raw control byte in {json}");
                at += 1;
            }
        }
    }

    /// The kernel command line carries only worker-chosen tokens: the plan's
    /// argv moves into the exec config, and the request's environment never
    /// reaches the line.
    #[test]
    fn the_workloads_argv_and_environment_leave_the_kernel_command_line() {
        let mut config = KrunConfig {
            run_id: "run".into(),
            rootfs: ResolvedRootfs {
                digest: DigestRef {
                    algorithm: "blake3-256".into(),
                    value: "0".repeat(64),
                },
                canonical_path: PathBuf::from("/rootfs"),
            },
            executable: CString::new("/bin/agent").unwrap(),
            arguments: vec![CString::new("--").unwrap(), CString::new("b").unwrap()],
            environment: vec![CString::new("A=\"x\"").unwrap()],
            workdir: CString::new("/").unwrap(),
            vcpus: 1,
            ram_mib: 256,
            wall_time_ms: 1_000,
            tsi_features: 0,
            port_map: Vec::new(),
            vsock_unix_map: Vec::new(),
        };
        move_exec_off_the_kernel_line(&mut config);
        assert!(config.arguments.is_empty());
        assert_eq!(
            config.environment,
            vec![
                CString::new(format!("KRUN_CONFIG={EXEC_CONFIG_PATH}")).unwrap(),
                CString::new("KRUN_CONFIG_ARGV=1").unwrap(),
            ]
        );
        // The workspace mount the worker adds afterwards is its own token.
        mount_workspace(&mut config);
        assert_eq!(config.environment.len(), 3);
    }

    /// The exec config is written into the run's own fresh root, exclusively
    /// and without following a link: a rootfs that already holds the path
    /// (a file, a directory, or a link pointing at a host file) is refused
    /// by name, and nothing is written through it.
    #[test]
    fn the_exec_config_is_written_once_and_never_through_a_link() {
        let mut request = request();
        request.arguments = vec!["--".into(), "\"".into()];
        let config = exec_config(&request, "/bin/agent").unwrap();
        let name = EXEC_CONFIG_PATH.trim_start_matches('/');

        let root = tempfile::tempdir().unwrap();
        write_exec_config(root.path(), &config).unwrap();
        let written = std::fs::read_to_string(root.path().join(name)).unwrap();
        assert_eq!(written, config.to_json());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(root.path().join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o400
        );
        // Twice: the path exists now, and is refused.
        let error = write_exec_config(root.path(), &config).unwrap_err();
        assert!(error.detail.contains(EXEC_CONFIG_PATH), "{error}");

        let host = tempfile::tempdir().unwrap();
        let target = host.path().join("host-file");
        std::fs::write(&target, b"host").unwrap();
        let linked = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(&target, linked.path().join(name)).unwrap();
        let error = write_exec_config(linked.path(), &config).unwrap_err();
        assert!(error.detail.contains(EXEC_CONFIG_PATH), "{error}");
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"host",
            "not written through"
        );
    }

    /// What can exist in an exec argv or environment but not in the guest:
    /// more strings than init's JSON reader holds, one string longer than
    /// the kernel's per-string limit, or more bytes in all than execve takes.
    /// Each is refused by name before anything runs.
    #[test]
    fn an_argv_or_environment_past_the_guests_bounds_is_refused_by_name() {
        let mut many = request();
        many.arguments = vec!["a".into(); MAX_EXEC_STRINGS];
        let error = exec_config(&many, "/bin/agent").unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidSpec);
        assert!(error.detail.contains("strings"), "{error}");

        let mut long = request();
        long.arguments = vec!["a".repeat(MAX_EXEC_STRING_BYTES)];
        let error = exec_config(&long, "/bin/agent").unwrap_err();
        assert!(error.detail.contains("argument"), "{error}");
        let mut long_env = request();
        long_env
            .public_environment
            .insert("A".into(), "a".repeat(MAX_EXEC_STRING_BYTES));
        let error = exec_config(&long_env, "/bin/agent").unwrap_err();
        assert!(error.detail.contains("\"A\""), "{error}");

        let mut total = request();
        total.arguments = vec![
            "a".repeat(MAX_EXEC_STRING_BYTES - 1);
            MAX_EXEC_TOTAL_BYTES / MAX_EXEC_STRING_BYTES + 1
        ];
        let error = exec_config(&total, "/bin/agent").unwrap_err();
        assert!(error.detail.contains("bytes"), "{error}");

        // NUL cannot exist in an exec argv or environment: refused by name,
        // in an argument and in a value.
        let mut nul_argument = request();
        nul_argument.arguments = vec!["a\0b".into()];
        let error = exec_config(&nul_argument, "/bin/agent").unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidSpec);
        assert!(error.detail.contains("NUL"), "{error}");
        let mut nul_value = request();
        nul_value
            .public_environment
            .insert("A".into(), "a\0b".into());
        let error = refuse_reserved_environment(&nul_value).unwrap_err();
        assert!(
            error.detail.contains("NUL") && error.detail.contains("\"A\""),
            "{error}"
        );

        // The bounds themselves are admitted.
        let mut edge = request();
        edge.arguments = vec!["a".repeat(MAX_EXEC_STRING_BYTES - 1)];
        exec_config(&edge, "/bin/agent").unwrap();
    }

    /// Each bound at its exact edge: one under it is admitted, the edge is
    /// refused. The per-string bound is the kernel's `MAX_ARG_STRLEN`
    /// (32 pages, the NUL included).
    #[test]
    fn the_exec_bounds_hold_at_their_exact_edges() {
        assert_eq!(MAX_EXEC_STRING_BYTES, 131_072);

        // Strings: argv[0] plus arguments plus environment entries.
        let mut at = request();
        at.arguments = vec!["a".into(); MAX_EXEC_STRINGS - 2];
        at.public_environment.insert("A".into(), "1".into());
        exec_config(&at, "/bin/agent").expect("exactly the most strings");
        at.arguments.push("a".into());
        let error = exec_config(&at, "/bin/agent").unwrap_err();
        assert!(error.detail.contains("strings"), "{error}");

        // One environment entry, `KEY=VALUE` with its NUL under the bound.
        let mut entry = request();
        entry
            .public_environment
            .insert("AB".into(), "v".repeat(MAX_EXEC_STRING_BYTES - 4));
        exec_config(&entry, "/bin/agent").expect("an entry one byte under the bound");
        entry
            .public_environment
            .insert("AB".into(), "v".repeat(MAX_EXEC_STRING_BYTES - 3));
        let error = exec_config(&entry, "/bin/agent").unwrap_err();
        assert!(error.detail.contains("\"AB\""), "{error}");

        // In all, NULs included: "/bin/agent" (11) + 8 x 131,070 + 5 is
        // exactly 1 MiB.
        let mut total = request();
        total.arguments = vec!["a".repeat(131_069); 8];
        total.arguments.push("abcd".into());
        exec_config(&total, "/bin/agent").expect("exactly the most bytes");
        total.arguments.last_mut().unwrap().push('e');
        let error = exec_config(&total, "/bin/agent").unwrap_err();
        assert!(error.detail.contains("bytes"), "{error}");
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
