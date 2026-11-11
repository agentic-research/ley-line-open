use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use leyline_core::ContentAddressed;

pub(crate) use crate::artifacts::rootfs_manifest::ROOTFS_MANIFEST;
use crate::artifacts::rootfs_manifest::{FileSpec, RootfsSpec, cas_directory_mode};
use crate::{DigestRef, ExecutionError, ExecutionRequest};

pub trait RootfsResolver: Send + Sync {
    fn resolve(&self, digest: &DigestRef) -> Result<ResolvedRootfs, ExecutionError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRootfs {
    pub digest: DigestRef,
    pub canonical_path: PathBuf,
}

/// Resolver for a materialized rootfs stored beneath a trusted CAS directory.
pub struct DirectoryRootfsResolver {
    cas_root: PathBuf,
}

impl DirectoryRootfsResolver {
    pub fn new(cas_root: impl Into<PathBuf>) -> Self {
        Self {
            cas_root: cas_root.into(),
        }
    }
}

impl RootfsResolver for DirectoryRootfsResolver {
    fn resolve(&self, digest: &DigestRef) -> Result<ResolvedRootfs, ExecutionError> {
        digest.validate_blake3()?;
        let cas_root = self
            .cas_root
            .canonicalize()
            .map_err(|error| invalid_io("canonicalize rootfs CAS", error))?;
        let rootfs = cas_root
            .join(&digest.value)
            .canonicalize()
            .map_err(|error| invalid_io("resolve rootfs digest", error))?;
        if !rootfs.starts_with(&cas_root) || !rootfs.is_dir() {
            return Err(ExecutionError::invalid(
                "resolved rootfs must be a directory beneath the configured CAS",
            ));
        }

        verify_manifest(&rootfs, digest, Placement::Cas)?;
        Ok(ResolvedRootfs {
            digest: digest.clone(),
            canonical_path: rootfs,
        })
    }
}

/// `KRUN_TSI_HIJACK_INET` from `libkrun.h` — carry the guest's `AF_INET`
/// sockets over vsock instead of leaving them unreachable.
///
/// Named here rather than written as a bare `1` at the call site so the one
/// place that weakens the boundary says what it is doing.
pub const KRUN_TSI_HIJACK_INET: u32 = 1 << 0;

#[derive(Debug)]
pub struct KrunConfig {
    pub run_id: String,
    pub rootfs: ResolvedRootfs,
    pub executable: CString,
    pub arguments: Vec<CString>,
    pub environment: Vec<CString>,
    pub workdir: CString,
    pub vcpus: u8,
    pub ram_mib: u32,
    pub wall_time_ms: u64,
    /// TSI feature mask for the guest's vsock device. `0` — no Transparent
    /// Socket Impersonation — is the default and the safe one: the guest
    /// reaches the host only over ports it was explicitly handed.
    ///
    /// Non-zero enables socket hijacking, which lets an unmodified guest keep
    /// binding `AF_INET` and have it carried out over vsock. That is strictly
    /// weaker — the boundary stops being "channels the guest was given" — so it
    /// is an operator decision, expressed here rather than inferred, and
    /// recorded so a receipt can say which boundary a run actually had.
    pub tsi_features: u32,
    /// Guest TCP ports exposed to the host, as libkrun's `"host:guest"` strings.
    ///
    /// EMPTY IS MEANINGFUL AND IS THE DEFAULT. libkrun treats "never called"
    /// as expose-every-listening-port and "called with an empty array" as
    /// expose-none, so this must always be passed — see `prepare_vm`.
    pub port_map: Vec<CString>,
    /// Guest-vsock-port ↔ host-UNIX-socket pairings delivering §6 on this
    /// tier, handed verbatim to `krun_add_vsock_port2` by `prepare_vm`.
    ///
    /// EMPTY IS THE CLOSED DEFAULT, same discipline as `port_map` above: a
    /// mapping only ever originates from the manifest the grant authorized
    /// (`vsock_unix_mappings` in `worker.rs`), never from the request, so a
    /// plan compiled straight from an `ExecutionRequest` grants none.
    pub vsock_unix_map: Vec<super::confinement::VsockUnixMapping>,
}

pub fn compile_plan(
    resolver: &dyn RootfsResolver,
    request: &ExecutionRequest,
) -> Result<KrunConfig, ExecutionError> {
    request.validate()?;
    let rootfs = resolver.resolve(&request.rootfs)?;
    executable_in_manifest(&rootfs, &request.executable)?;

    Ok(KrunConfig {
        run_id: request.run_id.clone(),
        rootfs,
        executable: guest_executable(&request.executable)?,
        arguments: request
            .arguments
            .iter()
            .map(|value| c_string(value, "argument"))
            .collect::<Result<_, _>>()?,
        environment: request
            .public_environment
            .iter()
            .map(|(key, value)| c_string(&format!("{key}={value}"), "environment"))
            .collect::<Result<_, _>>()?,
        workdir: CString::new("/").expect("static workdir has no NUL"),
        // Hypervisor sizing, zero meaning "unsized". The native tier compiles
        // this plan too and never reads these, and may legitimately carry a
        // ceiling no hypervisor can size (1.5 CPUs under a cgroup), so a value
        // that is not whole vCPUs or MiB is not an error HERE. On the microVM
        // tier it never arrives: `KrunWorkerBackend::start` refuses, by name,
        // both an unsized request and one the hypervisor cannot size exactly,
        // before a worker exists, and the worker refuses an unsized plan
        // before `krun_set_vm_config`.
        vcpus: request
            .limits
            .cpu
            .hypervisor_vcpus()
            .ok()
            .flatten()
            .unwrap_or(0),
        ram_mib: request
            .limits
            .memory
            .hypervisor_ram_mib()
            .ok()
            .flatten()
            .unwrap_or(0),
        wall_time_ms: request.limits.wall_time_ms,
        // Both default closed, and neither is derived from the request: a
        // workload does not get to widen its own boundary. Socket hijacking is
        // an operator decision made at backend configuration, and an exposed
        // port has to come from a manifest the grant authorized — so a plan
        // compiled straight from an ExecutionRequest grants neither.
        tsi_features: 0,
        port_map: Vec::new(),
        vsock_unix_map: Vec::new(),
    })
}

/// The executable must be a regular file the manifest lists, reached as the
/// guest's kernel reaches it: through the manifest's own links (an
/// absolute target is the guest's `/`), never through the host's. Returns
/// its real path, rootfs-relative. Nothing a link names is looked up on the
/// host; only the final regular file is, beneath directories the resolver
/// verified are real.
pub(crate) fn executable_in_manifest(
    rootfs: &ResolvedRootfs,
    executable: &str,
) -> Result<String, ExecutionError> {
    let spec = read_manifest(&rootfs.canonical_path, &rootfs.digest)?;
    let real = spec.resolve_file(executable).map_err(|reason| {
        ExecutionError::invalid(format!(
            "rootfs executable must be a manifest-verified regular file: {reason}"
        ))
    })?;
    let metadata = rootfs
        .canonical_path
        .join(&real)
        .symlink_metadata()
        .map_err(|error| invalid_io("resolve rootfs executable", error))?;
    if !metadata.is_file() {
        return Err(ExecutionError::invalid(
            "rootfs executable must be a manifest-verified regular file",
        ));
    }
    Ok(real)
}

/// What a verified rootfs directory is: the tree its manifest names, and
/// its directories as they are on disk, rootfs-relative (the root is the
/// empty path), each with its permission bits. For manifest v2 the two
/// sets of directories are equal; v1 names no directory.
#[derive(Debug)]
pub(crate) struct VerifiedTree {
    pub spec: RootfsSpec,
    pub directories: BTreeMap<String, u32>,
}

/// The tree the manifest `digest` names, read from `rootfs`: its bytes are
/// checked against the digest first, then parsed as the version they name
/// (v2 only in its canonical form; an unknown version is refused).
pub(crate) fn read_manifest(
    rootfs: &Path,
    digest: &DigestRef,
) -> Result<RootfsSpec, ExecutionError> {
    let manifest_path = rootfs.join(ROOTFS_MANIFEST);
    let metadata = manifest_path
        .symlink_metadata()
        .map_err(|error| invalid_io("read rootfs manifest metadata", error))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ExecutionError::invalid(
            "rootfs manifest must be a regular file",
        ));
    }
    let bytes =
        fs::read(&manifest_path).map_err(|error| invalid_io("read rootfs manifest", error))?;
    if bytes.as_slice().hash().to_string() != digest.value {
        return Err(ExecutionError::invalid(
            "rootfs manifest digest does not match requested content identity",
        ));
    }
    RootfsSpec::parse(&bytes).map_err(ExecutionError::invalid)
}

/// Where a verified tree sits, which decides the directory modes a v2
/// manifest requires of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Placement {
    /// A shared CAS entry (or the importer's staging of one): each
    /// directory at [`cas_directory_mode`] of its declared mode.
    Cas,
    /// One run's copy: each directory at its declared mode.
    RunCopy,
}

/// Verify `rootfs` against the manifest `digest` names: its regular files,
/// each with its content and mode, exactly the manifest's; its symbolic
/// links, each with its target bytes, exactly the manifest's (none, for
/// v1); and for v2 its directories exactly the manifest's, each at the mode
/// `placement` requires. v1 names no directory, so its directories are
/// returned, not judged: the CAS resolver accepts any, and a per-run view
/// is held to the implied set by
/// [`verify_ephemeral_rootfs`](super::volume::verify_ephemeral_rootfs).
/// The tree is walked without following a link before any file is read.
pub(crate) fn verify_manifest(
    rootfs: &Path,
    digest: &DigestRef,
    placement: Placement,
) -> Result<VerifiedTree, ExecutionError> {
    let spec = read_manifest(rootfs, digest)?;
    let on_disk = collect_rootfs_tree(rootfs)?;
    if spec.version == 1 && !on_disk.symlinks.is_empty() {
        return Err(ExecutionError::invalid(
            "rootfs symbolic links are not supported by rootfs manifest v1",
        ));
    }
    if on_disk.symlinks != spec.symlinks {
        return Err(ExecutionError::invalid(
            "rootfs symbolic links differ from the content-addressed manifest",
        ));
    }
    if !on_disk.files.keys().eq(spec.files.keys()) {
        return Err(ExecutionError::invalid(
            "rootfs files differ from the content-addressed manifest",
        ));
    }
    let required = |declared: u32| match placement {
        Placement::Cas => cas_directory_mode(declared),
        Placement::RunCopy => declared,
    };
    if spec.version >= 2
        && !on_disk
            .directories
            .iter()
            .map(|(path, mode)| (path, *mode))
            .eq(spec
                .directories
                .iter()
                .map(|(path, mode)| (path, required(*mode))))
    {
        return Err(ExecutionError::invalid(
            "rootfs directories or their modes differ from the content-addressed manifest",
        ));
    }
    for (path, file) in &spec.files {
        verify_manifest_file(rootfs, path, file)?;
    }
    Ok(VerifiedTree {
        spec,
        directories: on_disk.directories,
    })
}

fn verify_manifest_file(
    rootfs: &Path,
    relative: &str,
    entry: &FileSpec,
) -> Result<(), ExecutionError> {
    let path = rootfs.join(relative);
    let metadata = path
        .symlink_metadata()
        .map_err(|error| invalid_io("read rootfs file metadata", error))?;
    if metadata.file_type().is_symlink() {
        return Err(ExecutionError::invalid(format!(
            "rootfs manifest entry must not be a symbolic link: {relative}"
        )));
    }
    if !metadata.is_file() {
        return Err(ExecutionError::invalid(format!(
            "rootfs manifest entry is not a regular file: {relative}"
        )));
    }
    let canonical = path
        .canonicalize()
        .map_err(|error| invalid_io("canonicalize rootfs file", error))?;
    if !canonical.starts_with(rootfs) {
        return Err(ExecutionError::invalid(format!(
            "rootfs manifest path escapes root: {relative}"
        )));
    }
    if metadata.permissions().mode() & 0o7777 != entry.mode {
        return Err(ExecutionError::invalid(format!(
            "rootfs mode differs from manifest: {relative}"
        )));
    }
    let bytes = fs::read(&canonical).map_err(|error| invalid_io("read rootfs file", error))?;
    if bytes.as_slice().hash().to_string() != entry.blake3 {
        return Err(ExecutionError::invalid(format!(
            "rootfs content digest differs from manifest: {relative}"
        )));
    }
    Ok(())
}

/// A rootfs directory as it is on disk: rootfs-relative `/`-separated
/// paths, the root manifest left out.
#[derive(Debug, Default)]
struct OnDisk {
    /// Regular files, each with its permission bits.
    files: BTreeMap<String, u32>,
    /// Directories, the root as `""`, each with its permission bits.
    directories: BTreeMap<String, u32>,
    /// Symbolic links, each with its target, never followed.
    symlinks: BTreeMap<String, String>,
}

fn collect_rootfs_tree(rootfs: &Path) -> Result<OnDisk, ExecutionError> {
    fn walk(directory: &Path, relative: &str, tree: &mut OnDisk) -> Result<(), ExecutionError> {
        let metadata = directory
            .symlink_metadata()
            .map_err(|error| invalid_io("read rootfs directory metadata", error))?;
        tree.directories
            .insert(relative.to_string(), metadata.permissions().mode() & 0o7777);
        for entry in
            fs::read_dir(directory).map_err(|error| invalid_io("enumerate rootfs", error))?
        {
            let entry = entry.map_err(|error| invalid_io("enumerate rootfs entry", error))?;
            let name = entry.file_name().into_string().map_err(|name| {
                ExecutionError::invalid(format!(
                    "rootfs entry name is not UTF-8: {}",
                    name.to_string_lossy()
                ))
            })?;
            let child = if relative.is_empty() {
                name
            } else {
                format!("{relative}/{name}")
            };
            let path = entry.path();
            let metadata = path
                .symlink_metadata()
                .map_err(|error| invalid_io("read rootfs entry metadata", error))?;
            if metadata.file_type().is_symlink() {
                let target = fs::read_link(&path)
                    .map_err(|error| invalid_io("read rootfs link", error))?
                    .into_os_string()
                    .into_string()
                    .map_err(|_| {
                        ExecutionError::invalid(format!("rootfs link target is not UTF-8: {child}"))
                    })?;
                tree.symlinks.insert(child, target);
            } else if metadata.is_dir() {
                walk(&path, &child, tree)?;
            } else if metadata.is_file() {
                if child != ROOTFS_MANIFEST {
                    tree.files
                        .insert(child, metadata.permissions().mode() & 0o7777);
                }
            } else {
                return Err(ExecutionError::invalid(
                    "rootfs contains a non-regular filesystem entry",
                ));
            }
        }
        Ok(())
    }

    let mut tree = OnDisk::default();
    walk(rootfs, "", &mut tree)?;
    Ok(tree)
}

/// The executable as the guest names it: the rootfs-relative path the request
/// carries, anchored at the guest's `/` (bead `ley-line-open-fe130c`).
///
/// Absolute in every plan, not only when a workspace moves the working
/// directory: libkrun's init `execvp`s what it is given, and a bare name
/// (`hello`, as a scratch or distroless image's entrypoint is) has no slash,
/// so `execvp` searched `PATH` and the run exited 127 while the same bytes at
/// `bin/hello` ran. A run never depends on `PATH` to find its own executable.
/// Consumers on the host (the native tier) join it to the rootfs without the
/// leading slash.
pub fn guest_executable(relative: &str) -> Result<CString, ExecutionError> {
    let relative = relative.trim_start_matches('/');
    c_string(&format!("/{relative}"), "executable")
}

fn c_string(value: &str, field: &str) -> Result<CString, ExecutionError> {
    CString::new(value)
        .map_err(|_| ExecutionError::invalid(format!("{field} contains an interior NUL byte")))
}

fn invalid_io(action: &str, error: std::io::Error) -> ExecutionError {
    ExecutionError::invalid(format!("{action}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::guest_executable;

    /// Bead ley-line-open-fe130c: the exec path is absolute whatever the
    /// catalog's guest-relative path looks like, a top-level name included.
    #[test]
    fn the_guest_executable_is_always_absolute() {
        for (relative, guest) in [
            ("hello", "/hello"),
            ("bin/hello", "/bin/hello"),
            ("usr/local/bin/x", "/usr/local/bin/x"),
            ("/already", "/already"),
        ] {
            let path = guest_executable(relative).unwrap();
            assert_eq!(path.to_str().unwrap(), guest, "{relative}");
        }
        assert!(guest_executable("a\0b").is_err());
    }
}
