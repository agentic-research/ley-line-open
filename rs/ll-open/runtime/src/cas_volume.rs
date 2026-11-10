//! The CAS root's filesystem: how it compares names, and a case-sensitive
//! volume for it on hosts whose default filesystem is not (bead
//! `ley-line-open-ba86d3`).
//!
//! A rootfs is written as ordinary files beneath the CAS root, so two image
//! paths the host filesystem compares as equal (`Pod` and `pod` on default
//! APFS, or the NFC and NFD spellings of `é` on any APFS volume) would be
//! one file there. The OCI importer never merges them: it probes the CAS
//! root once ([`probe_sensitivity`]) and refuses an image whose tree holds
//! such a pair, by name, before writing any of it.
//!
//! Images that need distinct case (Debian's perl ships `Pod/` and `pod/`)
//! import into a CAS root on a case-sensitive volume, which
//! [`CasVolume::provision`] supplies:
//!
//! - **macOS.** A grow-on-demand, case-sensitive APFS sparsebundle, created
//!   and attached with `/usr/bin/hdiutil` as the calling user (no `sudo`:
//!   `hdiutil` creates and attaches a user-owned image unprivileged). The
//!   paths are fixed beneath the state directory (`<state>`, see
//!   [`default_state_dir`]):
//!
//!   ```text
//!   <state>/cas-volume/                 owner-only (0700)
//!   <state>/cas-volume/cas.sparsebundle the image; grows on demand up to its size cap
//!   <state>/cas-volume/mnt/             where it is attached (nobrowse, owners on)
//!   <state>/cas-volume/mnt/cas/         the CAS root returned to the caller
//!   ```
//!
//!   Provisioning is idempotent: an existing image is attached, an attached
//!   one is reused, and neither is ever recreated or reformatted. The size
//!   cap ([`DEFAULT_SIZE_CAP_BYTES`], 64 GiB) is the image's logical
//!   maximum, fixed when it is created; the image file holds only what is
//!   written. Nothing detaches the volume implicitly: it stays attached
//!   until [`CasVolume::detach`], `hdiutil detach <state>/cas-volume/mnt`,
//!   logout or reboot, and [`CasVolume::attach_existing`] (which never
//!   creates one) re-attaches it when a provider or daemon starts.
//!   Removing it is the operator's: detach, then delete the
//!   `cas.sparsebundle` directory; every rootfs in it is re-importable.
//! - **Linux.** No volume is made. The CAS root is `<state>/cas`; the call
//!   probes it and succeeds when its filesystem is case-sensitive (ext4,
//!   xfs, btrfs) and refuses by name otherwise (a casefolded directory,
//!   vfat, a case-insensitive mount).
//! - **Elsewhere.** Refused by name.
//!
//! No call here uses the network, a secret or elevated privilege.

use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use parking_lot::Mutex;
use serde::Serialize;

/// The default logical maximum of a provisioned volume: 64 GiB. The image
/// grows on demand, so this bounds it rather than reserving it.
pub const DEFAULT_SIZE_CAP_BYTES: u64 = 64 << 30;

/// The smallest size cap accepted: 1 GiB.
pub const MIN_SIZE_CAP_BYTES: u64 = 1 << 30;

/// The APFS volume name of a provisioned image.
pub const VOLUME_NAME: &str = "LeylineCAS";

const HDIUTIL: &str = "/usr/bin/hdiutil";

/// How a filesystem compares the names in one directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemSensitivity {
    /// `Pod` and `pod` are two names.
    pub case_sensitive: bool,
    /// The NFC and NFD spellings of one name (`é` as U+00E9, and as `e`
    /// followed by U+0301) are two names.
    pub normalization_sensitive: bool,
}

impl FilesystemSensitivity {
    /// A filesystem that compares names byte for byte.
    pub const EXACT: Self = Self {
        case_sensitive: true,
        normalization_sensitive: true,
    };
}

/// Probe how the filesystem holding `dir` compares names: in a private
/// directory created (and removed) inside `dir`, write one name and look
/// up a spelling that differs only by case, then only by Unicode
/// normalization. Cheap: two empty files. `dir` must exist and be writable.
pub fn probe_sensitivity(dir: &Path) -> io::Result<FilesystemSensitivity> {
    let probe = tempfile::Builder::new()
        .prefix(".fs-probe-")
        .tempdir_in(dir)?;
    let differs = |written: &str, looked_up: &str| -> io::Result<bool> {
        fs::File::create(probe.path().join(written))?;
        absent(fs::symlink_metadata(probe.path().join(looked_up)))
    };
    let sensitivity = FilesystemSensitivity {
        case_sensitive: differs("case-probe", "CASE-PROBE")?,
        normalization_sensitive: differs("norm-probe-\u{e9}", "norm-probe-e\u{301}")?,
    };
    probe.close()?;
    Ok(sensitivity)
}

/// Whether a lookup found nothing: `NotFound` is absence, any other error is
/// the probe's failure.
fn absent(lookup: io::Result<fs::Metadata>) -> io::Result<bool> {
    match lookup {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

/// LLO's state directory: `$LEYLINE_STATE_DIR`, else
/// `$XDG_STATE_HOME/leyline`, else `$HOME/.local/state/leyline`. Each must
/// be an absolute path.
pub fn default_state_dir() -> Result<PathBuf, CasVolumeError> {
    state_dir_from(|name| std::env::var_os(name))
}

fn state_dir_from(
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<PathBuf, CasVolumeError> {
    let absolute = |name: &str, path: PathBuf| {
        if path.is_absolute() {
            Ok(path)
        } else {
            Err(CasVolumeError::Config(format!(
                "{name} must be an absolute path, not {}",
                path.display()
            )))
        }
    };
    if let Some(dir) = var("LEYLINE_STATE_DIR").filter(|value| !value.is_empty()) {
        return absolute("LEYLINE_STATE_DIR", PathBuf::from(dir));
    }
    if let Some(dir) = var("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        return Ok(absolute("XDG_STATE_HOME", PathBuf::from(dir))?.join("leyline"));
    }
    match var("HOME").filter(|value| !value.is_empty()) {
        Some(home) => Ok(absolute("HOME", PathBuf::from(home))?.join(".local/state/leyline")),
        None => Err(CasVolumeError::Config(
            "no state directory: set LEYLINE_STATE_DIR (or XDG_STATE_HOME or HOME)".into(),
        )),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CasVolumeError {
    /// This host has no case-sensitive CAS volume mechanism.
    #[error(
        "a case-sensitive CAS volume is not supported on {os}: LLO provisions one on macOS and verifies one on Linux"
    )]
    UnsupportedHost { os: String },
    /// The CAS root's filesystem is not case-sensitive, and this host makes
    /// no volume to put it on (Linux), or the attached volume is not the
    /// case-sensitive one this module creates.
    #[error("the CAS root {} is case-insensitive: {remedy}", .path.display())]
    CaseInsensitive { path: PathBuf, remedy: &'static str },
    #[error("invalid CAS volume configuration: {0}")]
    Config(String),
    #[error("{command} failed: {detail}")]
    Tool { command: String, detail: String },
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> CasVolumeError {
    let path = path.to_path_buf();
    move |source| CasVolumeError::Io {
        action,
        path,
        source,
    }
}

/// A provisioned (or verified) CAS root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProvisionedCas {
    /// The CAS root: the directory `leyline rootfs import-oci --cas-root`
    /// writes to and `execution-daemon` serves. What Cloister sets
    /// `CLOISTER_CAS_ROOT` to.
    pub cas_root: PathBuf,
    /// Where the volume is attached; absent when no volume was made (Linux).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mountpoint: Option<PathBuf>,
    /// The sparsebundle; absent when no volume was made (Linux).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<PathBuf>,
    /// Whether this call created the image.
    pub created: bool,
    /// Whether this call attached it (false when it was already attached).
    pub attached: bool,
    /// The cap a created image was given; a cap requested for an existing
    /// image is not applied (resize it with `hdiutil resize` while detached).
    pub size_cap_bytes: u64,
    /// The CAS root's filesystem, probed after provisioning.
    pub filesystem: FilesystemSensitivity,
}

/// The host operating systems provisioning distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    MacOs,
    Linux,
    Other(&'static str),
}

impl HostOs {
    pub fn current() -> Self {
        match std::env::consts::OS {
            "macos" => Self::MacOs,
            "linux" => Self::Linux,
            other => Self::Other(other),
        }
    }
}

impl fmt::Display for HostOs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MacOs => f.write_str("macos"),
            Self::Linux => f.write_str("linux"),
            Self::Other(os) => f.write_str(os),
        }
    }
}

/// The disk-image operations provisioning needs. [`Hdiutil`] is the real
/// one; tests substitute a fake to cover the logic on any host.
pub trait DiskImages: Send + Sync {
    /// Create a grow-on-demand, case-sensitive APFS sparsebundle at
    /// `bundle` with logical maximum `size_cap_bytes`. Never attaches it.
    fn create(&self, bundle: &Path, size_cap_bytes: u64) -> Result<(), CasVolumeError>;
    /// Attach `bundle` at `mountpoint` (an existing directory).
    fn attach(&self, bundle: &Path, mountpoint: &Path) -> Result<(), CasVolumeError>;
    /// Detach whatever is attached at `mountpoint`.
    fn detach(&self, mountpoint: &Path) -> Result<(), CasVolumeError>;
    /// Whether a filesystem is mounted at `mountpoint`.
    fn is_mounted(&self, mountpoint: &Path) -> Result<bool, CasVolumeError>;
}

/// `/usr/bin/hdiutil`, by absolute path (never from `PATH`), as the calling
/// user, with stdin closed.
#[derive(Debug, Clone)]
pub struct Hdiutil {
    program: PathBuf,
}

impl Default for Hdiutil {
    fn default() -> Self {
        Self {
            program: PathBuf::from(HDIUTIL),
        }
    }
}

impl Hdiutil {
    fn run(&self, args: &[&std::ffi::OsStr]) -> Result<(), CasVolumeError> {
        let rendered = || {
            std::iter::once(self.program.display().to_string())
                .chain(args.iter().map(|arg| arg.to_string_lossy().into_owned()))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let output = Command::new(&self.program)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| CasVolumeError::Tool {
                command: rendered(),
                detail: error.to_string(),
            })?;
        if output.status.success() {
            return Ok(());
        }
        Err(CasVolumeError::Tool {
            command: rendered(),
            detail: format!(
                "{}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        })
    }
}

/// `hdiutil -size` in mebibytes, rounding up.
fn size_argument(size_cap_bytes: u64) -> String {
    format!("{}m", size_cap_bytes.div_ceil(1 << 20))
}

impl DiskImages for Hdiutil {
    fn create(&self, bundle: &Path, size_cap_bytes: u64) -> Result<(), CasVolumeError> {
        let size = size_argument(size_cap_bytes);
        self.run(&[
            "create".as_ref(),
            "-size".as_ref(),
            size.as_ref(),
            "-type".as_ref(),
            "SPARSEBUNDLE".as_ref(),
            "-fs".as_ref(),
            "Case-sensitive APFS".as_ref(),
            "-volname".as_ref(),
            VOLUME_NAME.as_ref(),
            "-nospotlight".as_ref(),
            bundle.as_os_str(),
        ])
    }

    fn attach(&self, bundle: &Path, mountpoint: &Path) -> Result<(), CasVolumeError> {
        self.run(&[
            "attach".as_ref(),
            "-nobrowse".as_ref(),
            "-noautoopen".as_ref(),
            "-owners".as_ref(),
            "on".as_ref(),
            "-mountpoint".as_ref(),
            mountpoint.as_os_str(),
            bundle.as_os_str(),
        ])
    }

    fn detach(&self, mountpoint: &Path) -> Result<(), CasVolumeError> {
        self.run(&["detach".as_ref(), mountpoint.as_os_str()])
    }

    fn is_mounted(&self, mountpoint: &Path) -> Result<bool, CasVolumeError> {
        is_mount_point(mountpoint)
    }
}

/// Whether `path` is the root of a mounted filesystem: it is a directory
/// on a different device from its parent's. A missing path is not.
pub fn is_mount_point(path: &Path) -> Result<bool, CasVolumeError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(io_error("inspect", path)(error)),
    };
    if !metadata.is_dir() {
        return Ok(false);
    }
    let parent = path.parent().unwrap_or(path);
    let parent = fs::metadata(parent).map_err(io_error("inspect", parent))?;
    Ok(metadata.dev() != parent.dev())
}

/// Where a provisioned volume lives beneath a state directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeLayout {
    /// `<state>/cas-volume`, owner-only.
    pub dir: PathBuf,
    /// `<state>/cas-volume/cas.sparsebundle`.
    pub image: PathBuf,
    /// `<state>/cas-volume/mnt`.
    pub mountpoint: PathBuf,
    /// `<state>/cas-volume/mnt/cas` on macOS; `<state>/cas` on Linux.
    pub cas_root: PathBuf,
}

/// A case-sensitive CAS root beneath one state directory.
pub struct CasVolume {
    state_dir: PathBuf,
    size_cap_bytes: u64,
    host: HostOs,
    images: Box<dyn DiskImages>,
    probe: fn(&Path) -> io::Result<FilesystemSensitivity>,
    /// Serializes this process's provisioning: two `provision` calls must
    /// not both create the image.
    lock: Mutex<()>,
}

impl fmt::Debug for CasVolume {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CasVolume")
            .field("state_dir", &self.state_dir)
            .field("size_cap_bytes", &self.size_cap_bytes)
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

impl CasVolume {
    /// The volume beneath `state_dir` (an absolute path) for this host, with
    /// `hdiutil` and the real probe.
    pub fn new(state_dir: impl Into<PathBuf>, size_cap_bytes: u64) -> Result<Self, CasVolumeError> {
        Self::with_parts(
            state_dir,
            size_cap_bytes,
            HostOs::current(),
            Box::new(Hdiutil::default()),
            probe_sensitivity,
        )
    }

    /// The same, with the host, the disk-image tool and the probe supplied:
    /// how tests cover every host's logic on one.
    pub fn with_parts(
        state_dir: impl Into<PathBuf>,
        size_cap_bytes: u64,
        host: HostOs,
        images: Box<dyn DiskImages>,
        probe: fn(&Path) -> io::Result<FilesystemSensitivity>,
    ) -> Result<Self, CasVolumeError> {
        let state_dir = state_dir.into();
        if !state_dir.is_absolute() {
            return Err(CasVolumeError::Config(format!(
                "the state directory must be an absolute path, not {}",
                state_dir.display()
            )));
        }
        if size_cap_bytes < MIN_SIZE_CAP_BYTES {
            return Err(CasVolumeError::Config(format!(
                "the size cap must be at least {MIN_SIZE_CAP_BYTES} bytes (1 GiB), not {size_cap_bytes}"
            )));
        }
        Ok(Self {
            state_dir,
            size_cap_bytes,
            host,
            images,
            probe,
            lock: Mutex::new(()),
        })
    }

    pub fn layout(&self) -> VolumeLayout {
        let dir = self.state_dir.join("cas-volume");
        let mountpoint = dir.join("mnt");
        let cas_root = match self.host {
            HostOs::MacOs => mountpoint.join("cas"),
            _ => self.state_dir.join("cas"),
        };
        VolumeLayout {
            image: dir.join("cas.sparsebundle"),
            mountpoint,
            cas_root,
            dir,
        }
    }

    /// The CAS root path this volume provides, whether or not it exists yet.
    pub fn cas_root(&self) -> PathBuf {
        self.layout().cas_root
    }

    /// Make the CAS root exist on a case-sensitive filesystem and return it:
    /// on macOS create the image if there is none, attach it if it is not
    /// attached, and create `cas/` in it; on Linux verify `<state>/cas`.
    /// Idempotent.
    pub fn provision(&self) -> Result<ProvisionedCas, CasVolumeError> {
        let _serial = self.lock.lock();
        match self.host {
            HostOs::MacOs => self.provision_macos(true).map(|provisioned| {
                provisioned.expect("provisioning with creation always yields a volume")
            }),
            HostOs::Linux => self.verify_linux(true).map(|provisioned| {
                provisioned.expect("provisioning with creation always yields a CAS root")
            }),
            HostOs::Other(os) => Err(CasVolumeError::UnsupportedHost { os: os.into() }),
        }
    }

    /// What a provider or daemon calls when it starts: re-attach a volume an
    /// earlier [`provision`](Self::provision) created, never creating one.
    /// `None` when none was provisioned.
    pub fn attach_existing(&self) -> Result<Option<ProvisionedCas>, CasVolumeError> {
        let _serial = self.lock.lock();
        match self.host {
            HostOs::MacOs => self.provision_macos(false),
            HostOs::Linux => self.verify_linux(false),
            HostOs::Other(os) => Err(CasVolumeError::UnsupportedHost { os: os.into() }),
        }
    }

    /// Detach an attached volume. `false` when nothing was attached (and
    /// always on Linux, which attaches nothing). Fails, and leaves it
    /// attached, while a file in it is open.
    pub fn detach(&self) -> Result<bool, CasVolumeError> {
        let _serial = self.lock.lock();
        match self.host {
            HostOs::MacOs => {
                let layout = self.layout();
                if !self.images.is_mounted(&layout.mountpoint)? {
                    return Ok(false);
                }
                self.images.detach(&layout.mountpoint)?;
                Ok(true)
            }
            HostOs::Linux => Ok(false),
            HostOs::Other(os) => Err(CasVolumeError::UnsupportedHost { os: os.into() }),
        }
    }

    fn provision_macos(&self, create: bool) -> Result<Option<ProvisionedCas>, CasVolumeError> {
        let layout = self.layout();
        let image_exists = fs::symlink_metadata(&layout.image).is_ok();
        if !image_exists && !create {
            return Ok(None);
        }
        private_directory(&layout.dir)?;
        let mut created = false;
        if !image_exists {
            created = self.create_image(&layout)?;
        }
        let mut attached = false;
        if !self.images.is_mounted(&layout.mountpoint)? {
            if fs::symlink_metadata(&layout.mountpoint).is_err() {
                fs::create_dir(&layout.mountpoint)
                    .map_err(io_error("create", &layout.mountpoint))?;
            }
            self.images.attach(&layout.image, &layout.mountpoint)?;
            if !self.images.is_mounted(&layout.mountpoint)? {
                return Err(CasVolumeError::Tool {
                    command: format!("{HDIUTIL} attach"),
                    detail: format!(
                        "it reported success but nothing is mounted at {}",
                        layout.mountpoint.display()
                    ),
                });
            }
            attached = true;
        }
        if fs::symlink_metadata(&layout.cas_root).is_err() {
            fs::create_dir(&layout.cas_root).map_err(io_error("create", &layout.cas_root))?;
            fs::set_permissions(&layout.cas_root, fs::Permissions::from_mode(0o755))
                .map_err(io_error("set the mode of", &layout.cas_root))?;
        }
        // The CAS root is a directory on the attached volume itself, never a
        // link to (or a mount of) somewhere else.
        let cas = fs::symlink_metadata(&layout.cas_root)
            .map_err(io_error("inspect", &layout.cas_root))?;
        let mount =
            fs::metadata(&layout.mountpoint).map_err(io_error("inspect", &layout.mountpoint))?;
        if !cas.is_dir() || cas.dev() != mount.dev() {
            return Err(CasVolumeError::Config(format!(
                "{} must be a directory on the volume attached at {}, not a link or another mount",
                layout.cas_root.display(),
                layout.mountpoint.display()
            )));
        }
        let filesystem = self.probe(&layout.cas_root)?;
        if !filesystem.case_sensitive {
            return Err(CasVolumeError::CaseInsensitive {
                path: layout.cas_root,
                remedy: "what is mounted there is not the case-sensitive volume LLO creates; detach it and provision again",
            });
        }
        Ok(Some(ProvisionedCas {
            cas_root: layout.cas_root,
            mountpoint: Some(layout.mountpoint),
            image: Some(layout.image),
            created,
            attached,
            size_cap_bytes: self.size_cap_bytes,
            filesystem,
        }))
    }

    /// Create the image under a private name and rename it into place, so a
    /// crash or a concurrent provider never leaves a half-made image at the
    /// stable path. Returns whether this call's image is the one in place.
    fn create_image(&self, layout: &VolumeLayout) -> Result<bool, CasVolumeError> {
        let partial = layout
            .dir
            .join(format!("partial-{}.sparsebundle", std::process::id()));
        if fs::symlink_metadata(&partial).is_ok() {
            fs::remove_dir_all(&partial).map_err(io_error("remove", &partial))?;
        }
        self.images.create(&partial, self.size_cap_bytes)?;
        match fs::rename(&partial, &layout.image) {
            Ok(()) => Ok(true),
            // Another process created it first: use theirs.
            Err(_) if fs::symlink_metadata(&layout.image).is_ok() => {
                fs::remove_dir_all(&partial).map_err(io_error("remove", &partial))?;
                Ok(false)
            }
            Err(error) => Err(io_error("rename into place", &partial)(error)),
        }
    }

    fn verify_linux(&self, create: bool) -> Result<Option<ProvisionedCas>, CasVolumeError> {
        let cas_root = self.layout().cas_root;
        if !cas_root.is_dir() {
            if !create {
                return Ok(None);
            }
            fs::create_dir_all(&cas_root).map_err(io_error("create", &cas_root))?;
        }
        let filesystem = self.probe(&cas_root)?;
        if !filesystem.case_sensitive {
            return Err(CasVolumeError::CaseInsensitive {
                path: cas_root,
                remedy: "on Linux LLO makes no volume; put the state directory (LEYLINE_STATE_DIR) on a case-sensitive filesystem, without casefolding",
            });
        }
        Ok(Some(ProvisionedCas {
            cas_root,
            mountpoint: None,
            image: None,
            created: false,
            attached: false,
            size_cap_bytes: self.size_cap_bytes,
            filesystem,
        }))
    }

    fn probe(&self, dir: &Path) -> Result<FilesystemSensitivity, CasVolumeError> {
        (self.probe)(dir).map_err(io_error("probe the name sensitivity of", dir))
    }
}

/// Where an execution provider's CAS root comes from, which its `provision`
/// operation makes exist and reports (`ProvisionOutput.casRoot`).
#[derive(Debug)]
pub enum CasStorage {
    /// An operator-supplied directory (`execution-daemon --cas-root`):
    /// provisioning only checks that it exists.
    Directory(PathBuf),
    /// A case-sensitive volume LLO provisions (`execution-daemon
    /// --cas-volume`).
    Volume(CasVolume),
}

impl CasStorage {
    /// The CAS root path, whether or not it exists yet.
    pub fn cas_root(&self) -> PathBuf {
        match self {
            Self::Directory(path) => path.clone(),
            Self::Volume(volume) => volume.cas_root(),
        }
    }

    /// Make the CAS root exist and return it.
    pub fn provision(&self) -> Result<PathBuf, CasVolumeError> {
        match self {
            Self::Directory(path) if path.is_dir() => Ok(path.clone()),
            Self::Directory(path) => Err(CasVolumeError::Config(format!(
                "the CAS root {} is not an existing directory",
                path.display()
            ))),
            Self::Volume(volume) => volume.provision().map(|provisioned| provisioned.cas_root),
        }
    }
}

/// Create `dir` (and its parents) and make it owner-only. An existing path
/// must be a directory this user owns.
fn private_directory(dir: &Path) -> Result<(), CasVolumeError> {
    fs::create_dir_all(dir).map_err(io_error("create", dir))?;
    let metadata = fs::symlink_metadata(dir).map_err(io_error("inspect", dir))?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid {
        return Err(CasVolumeError::Config(format!(
            "{} must be a directory owned by uid {uid}",
            dir.display()
        )));
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .map_err(io_error("set the mode of", dir))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use super::*;

    /// A disk-image tool that records calls; "attaching" marks the
    /// mountpoint mounted without mounting anything.
    #[derive(Clone, Default)]
    struct Fake {
        calls: Arc<Mutex<Vec<String>>>,
        mounted: Arc<Mutex<BTreeSet<PathBuf>>>,
        attach_mounts_nothing: bool,
        /// Another provider places the final image while this one creates.
        create_races: bool,
        /// The tool reports success and writes nothing.
        create_writes_nothing: bool,
    }

    impl Fake {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().clone()
        }
    }

    impl DiskImages for Fake {
        fn create(&self, bundle: &Path, size_cap_bytes: u64) -> Result<(), CasVolumeError> {
            if self.create_races {
                let theirs = bundle.with_file_name("cas.sparsebundle");
                fs::create_dir(&theirs).unwrap();
                fs::write(theirs.join("Info.plist"), b"theirs").unwrap();
            }
            if !self.create_writes_nothing {
                fs::create_dir(bundle).map_err(io_error("create", bundle))?;
                fs::write(bundle.join("Info.plist"), b"ours").unwrap();
            }
            self.calls.lock().push(format!(
                "create {} {}",
                bundle.file_name().unwrap().to_string_lossy(),
                size_argument(size_cap_bytes)
            ));
            Ok(())
        }
        fn attach(&self, bundle: &Path, mountpoint: &Path) -> Result<(), CasVolumeError> {
            assert!(bundle.is_dir(), "attach an image that exists");
            assert!(mountpoint.is_dir(), "attach at a directory that exists");
            self.calls.lock().push(format!(
                "attach {}",
                bundle.file_name().unwrap().to_string_lossy()
            ));
            if !self.attach_mounts_nothing {
                self.mounted.lock().insert(mountpoint.to_path_buf());
            }
            Ok(())
        }
        fn detach(&self, mountpoint: &Path) -> Result<(), CasVolumeError> {
            self.calls.lock().push("detach".into());
            self.mounted.lock().remove(mountpoint);
            Ok(())
        }
        fn is_mounted(&self, mountpoint: &Path) -> Result<bool, CasVolumeError> {
            Ok(self.mounted.lock().contains(mountpoint))
        }
    }

    fn sensitive(_: &Path) -> io::Result<FilesystemSensitivity> {
        Ok(FilesystemSensitivity {
            case_sensitive: true,
            normalization_sensitive: false,
        })
    }

    fn insensitive(_: &Path) -> io::Result<FilesystemSensitivity> {
        Ok(FilesystemSensitivity {
            case_sensitive: false,
            normalization_sensitive: false,
        })
    }

    fn volume(
        state: &Path,
        host: HostOs,
        fake: &Fake,
        probe: fn(&Path) -> io::Result<FilesystemSensitivity>,
    ) -> CasVolume {
        CasVolume::with_parts(state, 4 << 30, host, Box::new(fake.clone()), probe).unwrap()
    }

    #[test]
    fn macos_provision_creates_attaches_and_returns_the_stable_cas_root() {
        let state = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        let volume = volume(state.path(), HostOs::MacOs, &fake, sensitive);
        let provisioned = volume.provision().unwrap();
        let base = state.path().join("cas-volume");
        assert_eq!(provisioned.cas_root, base.join("mnt/cas"));
        assert_eq!(provisioned.mountpoint.as_deref(), Some(&*base.join("mnt")));
        assert_eq!(
            provisioned.image.as_deref(),
            Some(&*base.join("cas.sparsebundle"))
        );
        assert!((provisioned.created, provisioned.attached) == (true, true));
        assert!(provisioned.cas_root.is_dir());
        assert!(base.join("cas.sparsebundle").is_dir());
        assert_eq!(
            fs::metadata(&base).unwrap().permissions().mode() & 0o777,
            0o700,
            "the volume's directory is owner-only"
        );
        assert_eq!(
            fake.calls(),
            [
                format!("create partial-{}.sparsebundle 4096m", std::process::id()),
                "attach cas.sparsebundle".to_string(),
            ]
        );
        assert!(
            !base
                .join(format!("partial-{}.sparsebundle", std::process::id()))
                .exists(),
            "the partial image was renamed into place"
        );
    }

    #[test]
    fn macos_provision_is_idempotent_and_never_recreates_the_image() {
        let state = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        let volume = volume(state.path(), HostOs::MacOs, &fake, sensitive);
        let first = volume.provision().unwrap();
        let again = volume.provision().unwrap();
        assert_eq!(again.cas_root, first.cas_root);
        assert!((again.created, again.attached) == (false, false));
        assert!(volume.detach().unwrap());
        assert!(!volume.detach().unwrap(), "nothing left to detach");
        let reattached = volume.provision().unwrap();
        assert!((reattached.created, reattached.attached) == (false, true));
        assert_eq!(
            fake.calls()
                .iter()
                .filter(|call| call.starts_with("create"))
                .count(),
            1
        );
    }

    #[test]
    fn attach_existing_reattaches_and_never_creates() {
        let state = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        let volume = volume(state.path(), HostOs::MacOs, &fake, sensitive);
        assert_eq!(volume.attach_existing().unwrap(), None);
        assert!(fake.calls().is_empty(), "no image, so nothing is run");
        assert!(
            !state.path().join("cas-volume").exists(),
            "and nothing is created"
        );
        volume.provision().unwrap();
        volume.detach().unwrap();
        let started = volume.attach_existing().unwrap().expect("re-attached");
        assert!((started.created, started.attached) == (false, true));
        let again = volume.attach_existing().unwrap().expect("still attached");
        assert!((again.created, again.attached) == (false, false));
    }

    #[test]
    fn an_attach_that_mounts_nothing_is_an_error() {
        let state = tempfile::tempdir().unwrap();
        let fake = Fake {
            attach_mounts_nothing: true,
            ..Fake::default()
        };
        let error = volume(state.path(), HostOs::MacOs, &fake, sensitive)
            .provision()
            .unwrap_err();
        assert!(
            error.to_string().contains("nothing is mounted at"),
            "{error}"
        );
    }

    #[test]
    fn a_concurrent_provider_s_image_is_used_and_ours_removed() {
        let state = tempfile::tempdir().unwrap();
        let fake = Fake {
            create_races: true,
            ..Fake::default()
        };
        let provisioned = volume(state.path(), HostOs::MacOs, &fake, sensitive)
            .provision()
            .unwrap();
        assert!(!provisioned.created, "theirs is in place");
        let base = state.path().join("cas-volume");
        assert_eq!(
            fs::read(base.join("cas.sparsebundle/Info.plist")).unwrap(),
            b"theirs"
        );
        let names: Vec<_> = fs::read_dir(&base)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(
            !names
                .iter()
                .any(|name| name.to_string_lossy().starts_with("partial-")),
            "{names:?}"
        );
    }

    #[test]
    fn an_image_that_cannot_be_renamed_into_place_is_an_error() {
        let state = tempfile::tempdir().unwrap();
        let fake = Fake {
            create_writes_nothing: true,
            ..Fake::default()
        };
        let error = volume(state.path(), HostOs::MacOs, &fake, sensitive)
            .provision()
            .unwrap_err();
        assert!(
            error.to_string().starts_with("rename into place"),
            "{error}"
        );
    }

    #[test]
    fn a_state_path_that_is_not_a_directory_is_refused() {
        let state = tempfile::tempdir().unwrap();
        fs::write(state.path().join("cas-volume"), b"").unwrap();
        let error = volume(state.path(), HostOs::MacOs, &Fake::default(), sensitive)
            .provision()
            .unwrap_err();
        assert!(error.to_string().contains("create"), "{error}");
        let dir = state.path().join("dir");
        fs::create_dir(&dir).unwrap();
        let file = state.path().join("file");
        fs::write(&file, b"").unwrap();
        assert!(private_directory(&dir).is_ok());
        assert!(matches!(
            private_directory(&file),
            Err(CasVolumeError::Io { .. } | CasVolumeError::Config(_))
        ));
        let link = state.path().join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(
            matches!(private_directory(&link), Err(CasVolumeError::Config(_))),
            "a link to a directory is not the directory"
        );
    }

    #[test]
    fn the_real_tool_runs_by_its_path_with_these_arguments() {
        let scratch = tempfile::tempdir().unwrap();
        let log = scratch.path().join("args");
        let tool = |exit: u8| {
            let program = scratch.path().join(format!("hdiutil-{exit}"));
            fs::write(
                &program,
                format!(
                    "#!/bin/sh\nprintf '%s|' \"$@\" >> '{}'\necho refused >&2\nexit {exit}\n",
                    log.display()
                ),
            )
            .unwrap();
            fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
            Hdiutil { program }
        };
        let ok = tool(0);
        ok.create(Path::new("/b.sparsebundle"), 2 << 30).unwrap();
        ok.attach(Path::new("/b.sparsebundle"), Path::new("/m"))
            .unwrap();
        ok.detach(Path::new("/m")).unwrap();
        assert_eq!(
            fs::read_to_string(&log).unwrap(),
            "create|-size|2048m|-type|SPARSEBUNDLE|-fs|Case-sensitive APFS|-volname|LeylineCAS|-nospotlight|/b.sparsebundle|\
             attach|-nobrowse|-noautoopen|-owners|on|-mountpoint|/m|/b.sparsebundle|\
             detach|/m|"
        );
        let failing = tool(1);
        for error in [
            failing.create(Path::new("/b"), 1 << 30).unwrap_err(),
            failing
                .attach(Path::new("/b"), Path::new("/m"))
                .unwrap_err(),
            failing.detach(Path::new("/m")).unwrap_err(),
        ] {
            assert!(
                matches!(&error, CasVolumeError::Tool { detail, .. } if detail.contains("refused")),
                "{error}"
            );
        }
        let missing = Hdiutil {
            program: scratch.path().join("absent"),
        };
        assert!(matches!(
            missing.detach(Path::new("/m")),
            Err(CasVolumeError::Tool { .. })
        ));
        assert_eq!(
            Hdiutil::default().program,
            PathBuf::from("/usr/bin/hdiutil")
        );
        assert!(Hdiutil::default().is_mounted(Path::new("/dev")).unwrap());
        assert!(!Hdiutil::default().is_mounted(scratch.path()).unwrap());
    }

    #[test]
    fn absence_is_only_not_found() {
        assert!(!absent(fs::symlink_metadata("/")).unwrap());
        assert!(absent(Err(io::Error::from(io::ErrorKind::NotFound))).unwrap());
        assert!(absent(Err(io::Error::from(io::ErrorKind::PermissionDenied))).is_err());
    }

    #[test]
    fn storage_reports_and_provisions_its_cas_root() {
        let dir = tempfile::tempdir().unwrap();
        let storage = CasStorage::Directory(dir.path().to_path_buf());
        assert_eq!(storage.cas_root(), dir.path());
        assert_eq!(storage.provision().unwrap(), dir.path());
        let state = tempfile::tempdir().unwrap();
        let storage = CasStorage::Volume(volume(
            state.path(),
            HostOs::Linux,
            &Fake::default(),
            sensitive,
        ));
        assert_eq!(storage.cas_root(), state.path().join("cas"));
        assert_eq!(storage.provision().unwrap(), state.path().join("cas"));
    }

    #[test]
    fn hosts_and_volumes_render_by_name() {
        assert_eq!(HostOs::MacOs.to_string(), "macos");
        assert_eq!(HostOs::Linux.to_string(), "linux");
        assert_eq!(HostOs::Other("plan9").to_string(), "plan9");
        let state = tempfile::tempdir().unwrap();
        let rendered = format!(
            "{:?}",
            volume(state.path(), HostOs::MacOs, &Fake::default(), sensitive)
        );
        assert!(
            rendered.starts_with("CasVolume") && rendered.contains("MacOs"),
            "{rendered}"
        );
        assert!(default_state_dir().unwrap().is_absolute());
    }

    #[test]
    fn a_cas_root_that_is_a_link_off_the_volume_is_refused() {
        let state = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        let volume = volume(state.path(), HostOs::MacOs, &fake, sensitive);
        volume.provision().unwrap();
        let cas_root = volume.cas_root();
        fs::remove_dir(&cas_root).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), &cas_root).unwrap();
        let error = volume.provision().unwrap_err();
        assert!(error.to_string().contains("not a link"), "{error}");
        assert!(volume.attach_existing().is_err());
    }

    #[test]
    fn a_case_insensitive_volume_at_the_mountpoint_is_refused() {
        let state = tempfile::tempdir().unwrap();
        let error = volume(state.path(), HostOs::MacOs, &Fake::default(), insensitive)
            .provision()
            .unwrap_err();
        assert!(
            matches!(error, CasVolumeError::CaseInsensitive { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("is case-insensitive"), "{error}");
    }

    #[test]
    fn linux_verifies_the_state_directory_and_makes_no_volume() {
        let state = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        let volume = volume(state.path(), HostOs::Linux, &fake, sensitive);
        assert_eq!(volume.attach_existing().unwrap(), None);
        let provisioned = volume.provision().unwrap();
        assert_eq!(provisioned.cas_root, state.path().join("cas"));
        assert!(provisioned.cas_root.is_dir());
        assert_eq!((provisioned.mountpoint, provisioned.image), (None, None));
        assert!((provisioned.created, provisioned.attached) == (false, false));
        assert!(volume.attach_existing().unwrap().is_some());
        assert!(!volume.detach().unwrap());
        assert!(fake.calls().is_empty(), "Linux runs no disk-image tool");
    }

    #[test]
    fn linux_refuses_a_case_insensitive_state_directory_by_name() {
        let state = tempfile::tempdir().unwrap();
        let error = volume(state.path(), HostOs::Linux, &Fake::default(), insensitive)
            .provision()
            .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains(&format!(
                "the CAS root {} is case-insensitive",
                state.path().join("cas").display()
            )),
            "{message}"
        );
        assert!(message.contains("LEYLINE_STATE_DIR"), "{message}");
    }

    #[test]
    fn other_hosts_are_refused_by_name() {
        let state = tempfile::tempdir().unwrap();
        let volume = volume(
            state.path(),
            HostOs::Other("freebsd"),
            &Fake::default(),
            sensitive,
        );
        for error in [
            volume.provision().unwrap_err(),
            volume.attach_existing().unwrap_err(),
            volume.detach().unwrap_err(),
        ] {
            assert!(
                matches!(&error, CasVolumeError::UnsupportedHost { os } if os == "freebsd"),
                "{error}"
            );
            assert!(error.to_string().contains("not supported on freebsd"));
        }
        assert!(!state.path().join("cas-volume").exists());
    }

    #[test]
    fn configuration_is_validated() {
        let fake = Fake::default();
        let relative = CasVolume::with_parts(
            "relative",
            DEFAULT_SIZE_CAP_BYTES,
            HostOs::MacOs,
            Box::new(fake.clone()),
            sensitive,
        );
        assert!(matches!(relative, Err(CasVolumeError::Config(_))));
        let tiny = CasVolume::with_parts(
            "/state",
            MIN_SIZE_CAP_BYTES - 1,
            HostOs::MacOs,
            Box::new(fake.clone()),
            sensitive,
        );
        assert!(matches!(tiny, Err(CasVolumeError::Config(_))));
        assert!(
            CasVolume::with_parts(
                "/state",
                MIN_SIZE_CAP_BYTES,
                HostOs::MacOs,
                Box::new(fake),
                sensitive
            )
            .is_ok()
        );
    }

    #[test]
    fn the_size_argument_rounds_up_to_whole_mebibytes() {
        assert_eq!(size_argument(64 << 30), "65536m");
        assert_eq!(size_argument((1 << 30) + 1), "1025m");
        assert_eq!(size_argument(1 << 20), "1m");
    }

    #[test]
    fn the_state_directory_is_leyline_state_dir_then_xdg_then_home() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| std::ffi::OsString::from(value))
            }
        };
        assert_eq!(
            state_dir_from(env(&[
                ("LEYLINE_STATE_DIR", "/s"),
                ("XDG_STATE_HOME", "/x"),
                ("HOME", "/h")
            ]))
            .unwrap(),
            PathBuf::from("/s")
        );
        assert_eq!(
            state_dir_from(env(&[("XDG_STATE_HOME", "/x"), ("HOME", "/h")])).unwrap(),
            PathBuf::from("/x/leyline")
        );
        assert_eq!(
            state_dir_from(env(&[("LEYLINE_STATE_DIR", ""), ("HOME", "/h")])).unwrap(),
            PathBuf::from("/h/.local/state/leyline")
        );
        assert!(state_dir_from(env(&[("LEYLINE_STATE_DIR", "rel")])).is_err());
        assert!(state_dir_from(env(&[])).is_err());
    }

    #[test]
    fn a_directory_is_a_mount_point_only_on_its_own_device() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_mount_point(dir.path()).unwrap());
        assert!(!is_mount_point(&dir.path().join("missing")).unwrap());
        let file = dir.path().join("file");
        fs::write(&file, b"").unwrap();
        assert!(!is_mount_point(&file).unwrap(), "a file is never one");
        assert!(
            is_mount_point(&file.join("below")).is_err(),
            "a lookup that fails other than NotFound is an error, not absence"
        );
        assert!(is_mount_point(Path::new("/dev")).unwrap(), "devfs/devtmpfs");
    }

    #[test]
    fn the_probe_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        probe_sensitivity(dir.path()).unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
