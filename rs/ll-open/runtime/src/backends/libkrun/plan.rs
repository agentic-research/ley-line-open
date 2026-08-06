use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use leyline_core::ContentAddressed;
use serde::Deserialize;

use crate::{DigestRef, ExecutionError, ExecutionRequest};

const ROOTFS_MANIFEST: &str = "rootfs.manifest.json";

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

        verify_manifest(&rootfs, digest)?;
        Ok(ResolvedRootfs {
            digest: digest.clone(),
            canonical_path: rootfs,
        })
    }
}

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
    let executable_path = rootfs.canonical_path.join(&request.executable);
    let executable_meta = executable_path
        .symlink_metadata()
        .map_err(|error| invalid_io("resolve rootfs executable", error))?;
    if executable_meta.file_type().is_symlink() {
        return Err(ExecutionError::invalid(
            "rootfs executable must not be a symbolic link",
        ));
    }
    if !executable_meta.is_file() {
        return Err(ExecutionError::invalid(
            "rootfs executable must be a manifest-verified regular file",
        ));
    }

    Ok(KrunConfig {
        run_id: request.run_id.clone(),
        rootfs,
        executable: c_string(&request.executable, "executable")?,
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
        vcpus: request.limits.vcpus,
        ram_mib: request.limits.memory_mib,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RootfsManifest {
    version: u32,
    files: Vec<ManifestFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    path: String,
    mode: u32,
    blake3: String,
}

pub(crate) fn verify_manifest(rootfs: &Path, digest: &DigestRef) -> Result<(), ExecutionError> {
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
    let manifest: RootfsManifest = serde_json::from_slice(&bytes)
        .map_err(|error| ExecutionError::invalid(format!("invalid rootfs manifest: {error}")))?;
    if manifest.version != 1 {
        return Err(ExecutionError::invalid(format!(
            "unsupported rootfs manifest version: {}",
            manifest.version
        )));
    }

    let mut declared = BTreeSet::new();
    for entry in manifest.files {
        validate_relative_path(&entry.path)?;
        if !declared.insert(PathBuf::from(&entry.path)) {
            return Err(ExecutionError::invalid(format!(
                "duplicate rootfs manifest path: {}",
                entry.path
            )));
        }
        verify_manifest_file(rootfs, &entry)?;
    }
    if collect_rootfs_files(rootfs)? != declared {
        return Err(ExecutionError::invalid(
            "rootfs files differ from the content-addressed manifest",
        ));
    }

    Ok(())
}

fn verify_manifest_file(rootfs: &Path, entry: &ManifestFile) -> Result<(), ExecutionError> {
    let path = rootfs.join(&entry.path);
    let metadata = path
        .symlink_metadata()
        .map_err(|error| invalid_io("read rootfs file metadata", error))?;
    if metadata.file_type().is_symlink() {
        return Err(ExecutionError::invalid(format!(
            "rootfs manifest entry must not be a symbolic link: {}",
            entry.path
        )));
    }
    if !metadata.is_file() {
        return Err(ExecutionError::invalid(format!(
            "rootfs manifest entry is not a regular file: {}",
            entry.path
        )));
    }
    let canonical = path
        .canonicalize()
        .map_err(|error| invalid_io("canonicalize rootfs file", error))?;
    if !canonical.starts_with(rootfs) {
        return Err(ExecutionError::invalid(format!(
            "rootfs manifest path escapes root: {}",
            entry.path
        )));
    }
    if metadata.permissions().mode() & 0o7777 != entry.mode {
        return Err(ExecutionError::invalid(format!(
            "rootfs mode differs from manifest: {}",
            entry.path
        )));
    }
    let bytes = fs::read(&canonical).map_err(|error| invalid_io("read rootfs file", error))?;
    if bytes.as_slice().hash().to_string() != entry.blake3 {
        return Err(ExecutionError::invalid(format!(
            "rootfs content digest differs from manifest: {}",
            entry.path
        )));
    }
    Ok(())
}

fn collect_rootfs_files(rootfs: &Path) -> Result<BTreeSet<PathBuf>, ExecutionError> {
    fn walk(
        rootfs: &Path,
        directory: &Path,
        files: &mut BTreeSet<PathBuf>,
    ) -> Result<(), ExecutionError> {
        for entry in
            fs::read_dir(directory).map_err(|error| invalid_io("enumerate rootfs", error))?
        {
            let entry = entry.map_err(|error| invalid_io("enumerate rootfs entry", error))?;
            let path = entry.path();
            let metadata = path
                .symlink_metadata()
                .map_err(|error| invalid_io("read rootfs entry metadata", error))?;
            if metadata.file_type().is_symlink() {
                return Err(ExecutionError::invalid(
                    "rootfs symbolic links are not supported",
                ));
            }
            if metadata.is_dir() {
                walk(rootfs, &path, files)?;
            } else if metadata.is_file() {
                let relative = path
                    .strip_prefix(rootfs)
                    .map_err(|_| ExecutionError::invalid("rootfs entry escapes root"))?;
                if relative != Path::new(ROOTFS_MANIFEST) {
                    files.insert(relative.to_path_buf());
                }
            } else {
                return Err(ExecutionError::invalid(
                    "rootfs contains a non-regular filesystem entry",
                ));
            }
        }
        Ok(())
    }

    let mut files = BTreeSet::new();
    walk(rootfs, rootfs, &mut files)?;
    Ok(files)
}

fn validate_relative_path(value: &str) -> Result<(), ExecutionError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(ExecutionError::invalid(format!(
            "rootfs manifest path is not guest-relative: {value}"
        )));
    }
    Ok(())
}

fn c_string(value: &str, field: &str) -> Result<CString, ExecutionError> {
    CString::new(value)
        .map_err(|_| ExecutionError::invalid(format!("{field} contains an interior NUL byte")))
}

fn invalid_io(action: &str, error: std::io::Error) -> ExecutionError {
    ExecutionError::invalid(format!("{action}: {error}"))
}
