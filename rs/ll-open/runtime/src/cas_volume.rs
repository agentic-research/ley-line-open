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
//!
//!   Each `hdiutil` run is killed after [`DEFAULT_TOOL_TIMEOUT`] and fails
//!   by name ([`CasVolumeError::Timeout`]). Provisioning takes a file lock
//!   (`<state>/cas-volume/provision.lock`) across processes, removes the
//!   `partial-<pid>.sparsebundle` a crashed provider left (only when that
//!   process is gone), and uses a volume at `mnt/` only when `hdiutil info -plist`
//!   names `cas.sparsebundle` as its image ([`CasVolumeError::ForeignVolume`]
//!   otherwise) (bead `ley-line-open-167b29`).
//!
//!   macOS 27 marks `hdiutil create/attach/detach` deprecated in favour of
//!   `diskutil image` (bead `ley-line-open-168a65`). LLO stays on `hdiutil`:
//!   `diskutil image create blank` offers no case-sensitive APFS, and
//!   reaching one through `diskutil` takes four tool runs and a device node
//!   parsed from output. `tests/cas_volume.rs` pins the exact warnings and
//!   fails when they change or `diskutil image` gains a case-sensitive
//!   filesystem; `task runtime:test:cas-volume` runs it, and the recorded
//!   release suite (`task release:hypervisor:record`) includes it.
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
use std::time::{Duration, Instant};

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
    /// The CAS root is case-sensitive but the run root, where each run's
    /// rootfs is copied, is not: a tree the CAS holds would merge there
    /// (bead `ley-line-open-741388`).
    #[error(
        "the run root {} is case-insensitive, but the CAS root {} is case-sensitive: a rootfs holding names that differ only by case (Pod/ and pod/) cannot be copied there for a run; omit --run-root to use the volume's own, or name one on a case-sensitive filesystem",
        .run_root.display(),
        .cas_root.display()
    )]
    RunRootCaseInsensitive {
        run_root: PathBuf,
        cas_root: PathBuf,
    },
    #[error("invalid CAS volume configuration: {0}")]
    Config(String),
    #[error("{command} failed: {detail}")]
    Tool { command: String, detail: String },
    /// A disk-image tool run outlived its bound and was killed.
    #[error("{command} did not finish within {after:?} and was killed")]
    Timeout { command: String, after: Duration },
    /// What is mounted at the volume's mountpoint is not its image.
    #[error(
        "{} is not the CAS volume: it is mounted from {}, not {}; detach it and provision again",
        .mountpoint.display(),
        .found.as_deref().map_or_else(|| "no attached disk image".to_string(), |path| path.display().to_string()),
        .expected.display()
    )]
    ForeignVolume {
        mountpoint: PathBuf,
        expected: PathBuf,
        found: Option<PathBuf>,
    },
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
    /// The disk image whose volume is mounted at `mountpoint`, `None` when
    /// what is mounted there is no attached image (bead
    /// `ley-line-open-167b29`). A tool that cannot say refuses, so a
    /// volume is never trusted on the strength of a mount alone.
    fn attached_image(&self, mountpoint: &Path) -> Result<Option<PathBuf>, CasVolumeError> {
        Err(CasVolumeError::Config(format!(
            "this disk-image tool cannot say which image is attached at {}",
            mountpoint.display()
        )))
    }
    /// `statfs(2)`'s mount flags for the filesystem at `mountpoint`.
    fn mount_flags(&self, mountpoint: &Path) -> Result<u64, CasVolumeError> {
        mount_flags(mountpoint)
    }
}

/// `statfs(2)`'s `f_flags` for the filesystem holding `path`: `MNT_*` bits
/// on macOS, `ST_*` bits on Linux. Read through rustix, whose `StatFs`
/// carries `f_flags` on both, so one code path serves every host.
pub fn mount_flags(path: &Path) -> Result<u64, CasVolumeError> {
    let stat = rustix::fs::statfs(path).map_err(|errno| {
        io_error("inspect the mount flags of", path)(io::Error::from_raw_os_error(
            errno.raw_os_error(),
        ))
    })?;
    #[allow(clippy::unnecessary_cast, clippy::cast_sign_loss)]
    Ok(stat.f_flags as u64)
}

/// The `nodev` and `nosuid` bits of [`mount_flags`].
#[cfg(target_os = "macos")]
const MOUNT_NODEV: u64 = libc::MNT_NODEV as u64;
#[cfg(target_os = "macos")]
const MOUNT_NOSUID: u64 = libc::MNT_NOSUID as u64;
// `ST_*` is already a `u64` on 64-bit Linux, not on every target.
#[cfg(not(target_os = "macos"))]
#[allow(clippy::unnecessary_cast)]
const MOUNT_NODEV: u64 = libc::ST_NODEV as u64;
#[cfg(not(target_os = "macos"))]
#[allow(clippy::unnecessary_cast)]
const MOUNT_NOSUID: u64 = libc::ST_NOSUID as u64;

/// Whether mount flags include both `nodev` and `nosuid`.
fn nodev_and_nosuid(flags: u64) -> bool {
    flags & MOUNT_NODEV != 0 && flags & MOUNT_NOSUID != 0
}

/// How long one `hdiutil` run may take before it is killed and provisioning
/// fails by name: creating a 64 GiB sparsebundle takes seconds, and an
/// attach or detach less.
pub const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(120);

/// `/usr/bin/hdiutil`, by absolute path (never from `PATH`), as the calling
/// user, with stdin closed, each run bounded by a timeout.
#[derive(Debug, Clone)]
pub struct Hdiutil {
    program: PathBuf,
    timeout: Duration,
}

impl Default for Hdiutil {
    fn default() -> Self {
        Self {
            program: PathBuf::from(HDIUTIL),
            timeout: DEFAULT_TOOL_TIMEOUT,
        }
    }
}

impl Hdiutil {
    /// The real tool with another bound on each run.
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            timeout,
            ..Self::default()
        }
    }

    fn run(&self, args: &[&std::ffi::OsStr]) -> Result<(), CasVolumeError> {
        self.output(args).map(drop)
    }

    /// Run the tool and return its standard output. A run that outlives
    /// [`Hdiutil::timeout`] is killed (provisioning holds the volume's lock
    /// while it waits) and fails as [`CasVolumeError::Timeout`].
    fn output(&self, args: &[&std::ffi::OsStr]) -> Result<Vec<u8>, CasVolumeError> {
        let rendered = || {
            std::iter::once(self.program.display().to_string())
                .chain(args.iter().map(|arg| arg.to_string_lossy().into_owned()))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let tool_error = |detail: String| CasVolumeError::Tool {
            command: rendered(),
            detail,
        };
        use std::os::unix::process::CommandExt;
        // In its own process group, so a timeout kills whatever it started
        // too, not only the tool itself.
        let mut child = Command::new(&self.program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|error| tool_error(error.to_string()))?;
        let group = child.id() as libc::pid_t;
        let kill_group = || {
            // SAFETY: signals the process group this call created.
            unsafe { libc::kill(-group, libc::SIGKILL) };
        };
        let timed_out = || CasVolumeError::Timeout {
            command: rendered(),
            after: self.timeout,
        };
        // Drained on their own threads, so a chatty tool never blocks on a
        // full pipe while this one waits for it to exit; collected by the
        // same deadline, since a descendant can hold a pipe open after the
        // tool exits.
        let drain = |pipe: Option<Box<dyn io::Read + Send>>| {
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                if let Some(mut pipe) = pipe {
                    let _ = pipe.read_to_end(&mut bytes);
                }
                let _ = sender.send(bytes);
            });
            receiver
        };
        let stdout = drain(child.stdout.take().map(|pipe| Box::new(pipe) as _));
        let stderr = drain(child.stderr.take().map(|pipe| Box::new(pipe) as _));
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    kill_group();
                    let _ = child.wait();
                    return Err(timed_out());
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(error) => return Err(tool_error(error.to_string())),
            }
        };
        let collect = |receiver: std::sync::mpsc::Receiver<Vec<u8>>| {
            receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        };
        let (Ok(stdout), Ok(stderr)) = (collect(stdout), collect(stderr)) else {
            kill_group();
            return Err(timed_out());
        };
        if status.success() {
            return Ok(stdout);
        }
        Err(tool_error(format!(
            "{status}: {}",
            String::from_utf8_lossy(&stderr).trim()
        )))
    }
}

/// A property-list value, as far as `hdiutil info -plist` needs reading:
/// dictionaries, arrays and strings. Anything else (numbers, booleans,
/// dates, data) is [`Plist::Other`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum Plist {
    Dict(Vec<(String, Plist)>),
    Array(Vec<Plist>),
    String(String),
    Other,
}

impl Plist {
    /// The value of `key` in a dictionary.
    fn get(&self, key: &str) -> Option<&Plist> {
        match self {
            Self::Dict(pairs) => pairs
                .iter()
                .find_map(|(name, value)| (name == key).then_some(value)),
            _ => None,
        }
    }

    fn items(&self) -> &[Plist] {
        match self {
            Self::Array(items) => items,
            _ => &[],
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }
}

/// One piece of an XML property list.
#[derive(Debug, PartialEq, Eq)]
enum XmlToken<'a> {
    Open(&'a str),
    Close(&'a str),
    /// `<name/>`.
    Empty(&'a str),
    Text(String),
}

/// The five predefined entities and numeric character references, decoded.
/// An unknown or malformed reference is no text at all.
fn xml_unescape(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let end = rest[start..].find(';')? + start;
        let entity = &rest[start + 1..end];
        out.push(match entity {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let code = match entity.strip_prefix("#x") {
                    Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                    None => entity.strip_prefix('#')?.parse().ok()?,
                };
                char::from_u32(code)?
            }
        });
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// The tags and text of `xml`, declarations and comments left out. `None`
/// when a tag is not closed or a reference is malformed.
fn xml_tokens(xml: &str) -> Option<Vec<XmlToken<'_>>> {
    let mut tokens = Vec::new();
    let mut rest = xml;
    while let Some(open) = rest.find('<') {
        let (before, _) = rest.split_at(open);
        if !before.is_empty() {
            tokens.push(XmlToken::Text(xml_unescape(before)?));
        }
        let close = rest[open..].find('>')? + open;
        let tag = &rest[open + 1..close];
        rest = &rest[close + 1..];
        if tag.starts_with('?') || tag.starts_with('!') {
            continue;
        }
        tokens.push(if let Some(tag) = tag.strip_prefix('/') {
            XmlToken::Close(tag_name(tag))
        } else if let Some(tag) = tag.strip_suffix('/') {
            XmlToken::Empty(tag_name(tag))
        } else {
            XmlToken::Open(tag_name(tag))
        });
    }
    Some(tokens)
}

/// A tag's name, without its attributes.
fn tag_name(tag: &str) -> &str {
    tag.split_whitespace().next().unwrap_or("")
}

/// How deeply a property list may nest before it is refused.
const MAX_PLIST_DEPTH: usize = 32;

/// The root value of an XML property list. Structure comes from the tags
/// alone, so no string's content (a path holding a newline, a `=` rule or
/// the text of another entry) can pass for another entry.
fn parse_plist(xml: &str) -> Option<Plist> {
    fn text<'t, 'a>(tokens: &mut std::slice::Iter<'t, XmlToken<'a>>, tag: &str) -> Option<String> {
        match tokens.next()? {
            XmlToken::Close(name) if *name == tag => Some(String::new()),
            XmlToken::Text(value) => match tokens.next()? {
                XmlToken::Close(name) if *name == tag => Some(value.clone()),
                _ => None,
            },
            _ => None,
        }
    }
    /// The value an already-read opening token starts.
    fn value<'t, 'a>(
        first: &'t XmlToken<'a>,
        tokens: &mut std::slice::Iter<'t, XmlToken<'a>>,
        depth: usize,
    ) -> Option<Plist> {
        if depth > MAX_PLIST_DEPTH {
            return None;
        }
        match first {
            XmlToken::Open("dict") => {
                let mut pairs = Vec::new();
                loop {
                    match tokens.next()? {
                        XmlToken::Close("dict") => return Some(Plist::Dict(pairs)),
                        XmlToken::Open("key") => {
                            let key = text(tokens, "key")?;
                            let next = tokens.next()?;
                            pairs.push((key, value(next, tokens, depth + 1)?));
                        }
                        XmlToken::Text(space) if space.trim().is_empty() => {}
                        _ => return None,
                    }
                }
            }
            XmlToken::Open("array") => {
                let mut items = Vec::new();
                loop {
                    match tokens.next()? {
                        XmlToken::Close("array") => return Some(Plist::Array(items)),
                        XmlToken::Text(space) if space.trim().is_empty() => {}
                        next => items.push(value(next, tokens, depth + 1)?),
                    }
                }
            }
            XmlToken::Open("string") => text(tokens, "string").map(Plist::String),
            XmlToken::Empty("string") => Some(Plist::String(String::new())),
            XmlToken::Empty("dict") => Some(Plist::Dict(Vec::new())),
            XmlToken::Empty("array") => Some(Plist::Array(Vec::new())),
            // A scalar this reader does not need: its text, then its close.
            XmlToken::Open(tag) => text(tokens, tag).map(|_| Plist::Other),
            XmlToken::Empty(_) => Some(Plist::Other),
            XmlToken::Text(space) if space.trim().is_empty() => {
                value(tokens.next()?, tokens, depth)
            }
            XmlToken::Close(_) | XmlToken::Text(_) => None,
        }
    }
    let tokens = xml_tokens(xml)?;
    let mut tokens = tokens.iter();
    loop {
        match tokens.next()? {
            XmlToken::Open("plist") => return value(tokens.next()?, &mut tokens, 0),
            XmlToken::Text(space) if space.trim().is_empty() => {}
            _ => return None,
        }
    }
}

/// The image `hdiutil info -plist` lists as mounted at `mountpoint`: the
/// `image-path` of the `images` entry one of whose `system-entities` has
/// that `mount-point`. Paths are compared canonicalized. `Err` when the
/// output is not a property list of that shape.
fn attached_image_in_plist(xml: &str, mountpoint: &Path) -> Result<Option<PathBuf>, &'static str> {
    let root = parse_plist(xml).ok_or("its output is not an XML property list")?;
    let Some(Plist::Array(images)) = root.get("images") else {
        return Err("its output has no `images` array");
    };
    let Ok(wanted) = mountpoint.canonicalize() else {
        return Ok(None);
    };
    Ok(images.iter().find_map(|image| {
        let mounted_here = image
            .get("system-entities")
            .map_or(&[][..], Plist::items)
            .iter()
            .filter_map(|entity| entity.get("mount-point")?.as_str())
            .any(|mount| {
                Path::new(mount)
                    .canonicalize()
                    .is_ok_and(|mount| mount == wanted)
            });
        let path = image.get("image-path")?.as_str()?;
        mounted_here.then(|| PathBuf::from(path))
    }))
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

    fn attached_image(&self, mountpoint: &Path) -> Result<Option<PathBuf>, CasVolumeError> {
        let info = self.output(&["info".as_ref(), "-plist".as_ref()])?;
        let unusable = |detail: &str| CasVolumeError::Tool {
            command: format!("{} info -plist", self.program.display()),
            detail: detail.to_string(),
        };
        let xml = std::str::from_utf8(&info).map_err(|_| unusable("its output is not UTF-8"))?;
        attached_image_in_plist(xml, mountpoint).map_err(unusable)
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
    /// The default parent of each run's ephemeral rootfs: on the same
    /// case-sensitive filesystem as the CAS root, so a tree the CAS holds
    /// (`Pod/` beside `pod/`) can be copied for a run (bead
    /// `ley-line-open-741388`). `<state>/cas-volume/mnt/runs` on macOS;
    /// `<state>/runs` on Linux.
    pub run_root: PathBuf,
}

/// A case-sensitive CAS root beneath one state directory.
pub struct CasVolume {
    state_dir: PathBuf,
    size_cap_bytes: u64,
    host: HostOs,
    images: Box<dyn DiskImages>,
    probe: fn(&Path) -> io::Result<FilesystemSensitivity>,
    /// The run root an operator named instead of [`VolumeLayout::run_root`].
    run_root: Option<PathBuf>,
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
            .field("run_root", &self.run_root())
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
            run_root: None,
            lock: Mutex::new(()),
        })
    }

    /// The same volume with each run's ephemeral rootfs beneath `run_root`
    /// (an absolute path) instead of [`VolumeLayout::run_root`]. It must be
    /// case-sensitive too: [`CasVolume::check_run_root`] and every provision
    /// refuse it otherwise.
    pub fn with_run_root(mut self, run_root: PathBuf) -> Result<Self, CasVolumeError> {
        if !run_root.is_absolute() {
            return Err(CasVolumeError::Config(format!(
                "the run root must be an absolute path, not {}",
                run_root.display()
            )));
        }
        self.run_root = Some(run_root);
        Ok(self)
    }

    pub fn layout(&self) -> VolumeLayout {
        let dir = self.state_dir.join("cas-volume");
        let mountpoint = dir.join("mnt");
        let (cas_root, run_root) = match self.host {
            HostOs::MacOs => (mountpoint.join("cas"), mountpoint.join("runs")),
            _ => (self.state_dir.join("cas"), self.state_dir.join("runs")),
        };
        VolumeLayout {
            image: dir.join("cas.sparsebundle"),
            mountpoint,
            cas_root,
            run_root,
            dir,
        }
    }

    /// Where each run's ephemeral rootfs is materialized: the run root an
    /// operator named, else [`VolumeLayout::run_root`].
    pub fn run_root(&self) -> PathBuf {
        self.run_root
            .clone()
            .unwrap_or_else(|| self.layout().run_root)
    }

    /// What a daemon calls when it starts: refuse now a run root that is
    /// not case-sensitive, unless it lies on the volume's mountpoint (macOS),
    /// where it exists only once the volume is attached and every provision
    /// checks it. Creates the run root when it is checked.
    pub fn check_run_root(&self) -> Result<(), CasVolumeError> {
        let run_root = self.run_root();
        if self.host == HostOs::MacOs && run_root.starts_with(self.layout().mountpoint) {
            return Ok(());
        }
        self.verify_run_root()
    }

    /// Make the run root exist and refuse it, naming it and the CAS root,
    /// unless its filesystem is case-sensitive.
    fn verify_run_root(&self) -> Result<(), CasVolumeError> {
        let run_root = self.run_root();
        fs::create_dir_all(&run_root).map_err(io_error("create", &run_root))?;
        if !self.probe(&run_root)?.case_sensitive {
            return Err(CasVolumeError::RunRootCaseInsensitive {
                run_root,
                cas_root: self.cas_root(),
            });
        }
        Ok(())
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
                if !layout.dir.is_dir() {
                    return Ok(false);
                }
                // Under the provisioning lock, so a detach never lands between
                // another process's image check and its use of the volume.
                let _provisioning = provisioning_lock(&layout.dir)?;
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
        if fs::symlink_metadata(&layout.image).is_err() && !create {
            return Ok(None);
        }
        private_directory(&layout.dir)?;
        // Across processes too: two providers (or a daemon and the CLI)
        // provisioning or detaching at once take turns. Released when
        // `_provisioning` drops.
        let _provisioning = provisioning_lock(&layout.dir)?;
        // Read under the lock: another process may have created it.
        let image_exists = fs::symlink_metadata(&layout.image).is_ok();
        if !image_exists && !create {
            return Ok(None);
        }
        if create {
            remove_stale_partials(&layout.dir)?;
        }
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
        // A mount at `mnt` is the volume only if it comes from
        // `cas.sparsebundle`: someone else's image (or any other filesystem)
        // mounted there first is refused, not written to.
        let expected = layout
            .image
            .canonicalize()
            .map_err(io_error("resolve", &layout.image))?;
        let found = self.images.attached_image(&layout.mountpoint)?;
        if found
            .as_deref()
            .and_then(|path| path.canonicalize().ok())
            .is_none_or(|found| found != expected)
        {
            return Err(CasVolumeError::ForeignVolume {
                mountpoint: layout.mountpoint,
                expected: layout.image,
                found,
            });
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
        // And the volume is still mounted there after `cas/` was made: a
        // detach in between would leave a `cas/` on the host's own disk,
        // whose device then equals its parent's.
        if !cas.is_dir()
            || cas.dev() != mount.dev()
            || !self.images.is_mounted(&layout.mountpoint)?
        {
            return Err(CasVolumeError::Config(format!(
                "{} must be a directory on the volume attached at {}, not a link or another mount",
                layout.cas_root.display(),
                layout.mountpoint.display()
            )));
        }
        // Nothing in the CAS is a device or gains privilege on the host: an
        // unprivileged attach mounts nodev and nosuid, and a volume mounted
        // without them (by root, say) is refused.
        if !nodev_and_nosuid(self.images.mount_flags(&layout.mountpoint)?) {
            return Err(CasVolumeError::Config(format!(
                "the volume at {} is not mounted nodev and nosuid; detach it and provision again as an unprivileged user",
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
        self.verify_run_root()?;
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
        self.verify_run_root()?;
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

/// Take the cross-process provisioning lock (`<dir>/provision.lock`): held
/// while provisioning or detaching, released when the file drops. Each
/// holder's tool runs are bounded, so a wait for it is too.
fn provisioning_lock(dir: &Path) -> Result<fs::File, CasVolumeError> {
    let lock_path = dir.join("provision.lock");
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(io_error("open", &lock_path))?;
    lock.lock().map_err(io_error("lock", &lock_path))?;
    Ok(lock)
}

/// Remove each `partial-<pid>.sparsebundle` in `dir` whose process no longer
/// exists: what a provider that crashed between `hdiutil create` and the
/// rename left (bead `ley-line-open-167b29`). A live process's is its own
/// to finish or remove; a name that is not of that form is left alone.
fn remove_stale_partials(dir: &Path) -> Result<(), CasVolumeError> {
    for entry in fs::read_dir(dir).map_err(io_error("list", dir))? {
        let entry = entry.map_err(io_error("list", dir))?;
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|name| name.strip_prefix("partial-"))
            .and_then(|rest| rest.strip_suffix(".sparsebundle"))
            .and_then(|pid| pid.parse::<std::num::NonZeroU32>().ok())
            .and_then(|pid| libc::pid_t::try_from(pid.get()).ok())
        else {
            continue;
        };
        if process_exists(pid) {
            continue;
        }
        let path = entry.path();
        fs::remove_dir_all(&path).map_err(io_error("remove", &path))?;
    }
    Ok(())
}

/// Whether process `pid` exists: `kill(pid, 0)` succeeds, or fails only
/// because it belongs to another user.
fn process_exists(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 performs only the existence and permission checks.
    let signalled = unsafe { libc::kill(pid, 0) } == 0;
    signalled || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
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
        /// The image each mountpoint was attached from.
        images: Arc<Mutex<std::collections::BTreeMap<PathBuf, PathBuf>>>,
        /// Report this image (or, with `Some(None)`, none) as mounted at
        /// every mountpoint instead of the one attached.
        reports_image: Option<Option<PathBuf>>,
        /// Another process detaches the volume right after its image is
        /// checked.
        detached_after_check: bool,
        /// The mount lacks nodev or nosuid.
        unrestricted: bool,
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
                self.images
                    .lock()
                    .insert(mountpoint.to_path_buf(), bundle.to_path_buf());
            }
            Ok(())
        }
        fn detach(&self, mountpoint: &Path) -> Result<(), CasVolumeError> {
            self.calls.lock().push("detach".into());
            self.mounted.lock().remove(mountpoint);
            self.images.lock().remove(mountpoint);
            Ok(())
        }
        fn is_mounted(&self, mountpoint: &Path) -> Result<bool, CasVolumeError> {
            Ok(self.mounted.lock().contains(mountpoint))
        }
        fn attached_image(&self, mountpoint: &Path) -> Result<Option<PathBuf>, CasVolumeError> {
            if let Some(reported) = &self.reports_image {
                return Ok(reported.clone());
            }
            let image = self.images.lock().get(mountpoint).cloned();
            if self.detached_after_check {
                self.mounted.lock().remove(mountpoint);
            }
            Ok(image)
        }
        fn mount_flags(&self, _: &Path) -> Result<u64, CasVolumeError> {
            Ok(if self.unrestricted {
                0
            } else {
                MOUNT_NODEV | MOUNT_NOSUID
            })
        }
    }

    /// Review of 167b29 (C1): a detach between the image check and the
    /// creation of `cas/` would leave `cas/` on the host's own disk; it is
    /// refused, not reported as the volume.
    #[test]
    fn a_volume_detached_mid_provision_is_refused() {
        let state = tempfile::tempdir().unwrap();
        let fake = Fake {
            detached_after_check: true,
            ..Fake::default()
        };
        let error = volume(state.path(), HostOs::MacOs, &fake, sensitive)
            .provision()
            .unwrap_err();
        assert!(
            error.to_string().contains("not a link or another mount"),
            "{error}"
        );
    }

    /// Review of 167b29 (N4): a volume mounted without nodev or nosuid is
    /// refused; the real mount's flags are read with statfs.
    #[test]
    fn a_volume_without_nodev_and_nosuid_is_refused() {
        let state = tempfile::tempdir().unwrap();
        let fake = Fake {
            unrestricted: true,
            ..Fake::default()
        };
        let error = volume(state.path(), HostOs::MacOs, &fake, sensitive)
            .provision()
            .unwrap_err();
        assert!(error.to_string().contains("nodev and nosuid"), "{error}");
        // Both bits, not either.
        assert!(nodev_and_nosuid(MOUNT_NODEV | MOUNT_NOSUID | 1));
        assert!(!nodev_and_nosuid(MOUNT_NODEV));
        assert!(!nodev_and_nosuid(MOUNT_NOSUID));
        assert!(!nodev_and_nosuid(0));
        // The real flags: the root filesystem is the root (macOS
        // MNT_ROOTFS), a temporary directory is neither nodev nor nosuid,
        // and a missing path is an error, for the tool's default too.
        #[cfg(target_os = "macos")]
        assert_ne!(
            Hdiutil::default().mount_flags(Path::new("/")).unwrap() & libc::MNT_ROOTFS as u64,
            0
        );
        assert!(!nodev_and_nosuid(mount_flags(state.path()).unwrap()));
        assert!(mount_flags(&state.path().join("missing")).is_err());
        assert!(mount_flags(Path::new("a\0b")).is_err());
    }

    /// `hdiutil info -plist` as macOS 27.2 prints it for the images given as
    /// `(image path, mount point)`, each with an unmounted whole-disk entity
    /// before its mounted volume.
    fn info_plist(images: &[(&str, &str)]) -> String {
        let escape = |text: &str| {
            text.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
        };
        let mut xml = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n<dict>\n\t<key>framework</key>\n\t<string>710</string>\n\t<key>images</key>\n\t<array>\n",
        );
        for (image, mount) in images {
            xml.push_str(&format!(
                "\t\t<dict>\n\t\t\t<key>autodiskmount</key>\n\t\t\t<true/>\n\
                 \t\t\t<key>blockcount</key>\n\t\t\t<integer>2097152</integer>\n\
                 \t\t\t<key>image-path</key>\n\t\t\t<string>{}</string>\n\
                 \t\t\t<key>system-entities</key>\n\t\t\t<array>\n\
                 \t\t\t\t<dict>\n\t\t\t\t\t<key>content-hint</key>\n\t\t\t\t\t<string>GUID_partition_scheme</string>\n\
                 \t\t\t\t\t<key>dev-entry</key>\n\t\t\t\t\t<string>/dev/disk4</string>\n\t\t\t\t</dict>\n\
                 \t\t\t\t<dict>\n\t\t\t\t\t<key>dev-entry</key>\n\t\t\t\t\t<string>/dev/disk5s1</string>\n\
                 \t\t\t\t\t<key>mount-point</key>\n\t\t\t\t\t<string>{}</string>\n\t\t\t\t</dict>\n\
                 \t\t\t</array>\n\t\t\t<key>writeable</key>\n\t\t\t<true/>\n\t\t</dict>\n",
                escape(image),
                escape(mount)
            ));
        }
        xml.push_str(
            "\t</array>\n\t<key>revision</key>\n\t<string>710</string>\n</dict>\n</plist>\n",
        );
        xml
    }

    /// The real tool's `attached_image` reads `hdiutil info -plist`.
    #[test]
    fn the_real_tools_attached_image_comes_from_hdiutil_info() {
        let scratch = tempfile::tempdir().unwrap();
        let mnt = scratch.path().join("mnt");
        fs::create_dir(&mnt).unwrap();
        let plist = scratch.path().join("info.plist");
        fs::write(
            &plist,
            info_plist(&[("/a/cas.sparsebundle", &mnt.to_string_lossy())]),
        )
        .unwrap();
        let tool = |body: String| Hdiutil {
            program: script(scratch.path(), "hdiutil-info", &body),
            timeout: Duration::from_secs(20),
        };
        let real = tool(format!(
            "[ \"$1 $2\" = 'info -plist' ] || exit 9\ncat '{}'",
            plist.display()
        ));
        assert_eq!(
            real.attached_image(&mnt).unwrap(),
            Some(PathBuf::from("/a/cas.sparsebundle"))
        );
        assert_eq!(real.attached_image(scratch.path()).unwrap(), None);
        // Output that is no property list refuses; it never reads as
        // "nothing is attached".
        for (body, reason) in [
            (
                "echo 'image-path : /a/cas.sparsebundle'",
                "not an XML property list",
            ),
            ("printf '<plist><dict></dict></plist>'", "no `images` array"),
            ("printf '\\377'", "not UTF-8"),
        ] {
            let error = tool(body.to_string()).attached_image(&mnt).unwrap_err();
            assert!(
                matches!(&error, CasVolumeError::Tool { command, detail }
                    if command.ends_with("info -plist") && detail.contains(reason)),
                "{body}: {error}"
            );
        }
    }

    /// Review of 167b29 (C2): a tool that exits while something it started
    /// still holds its output is bounded by the same timeout, and that
    /// descendant is killed with it.
    #[test]
    fn a_tool_whose_descendant_holds_its_output_is_bounded_too() {
        let scratch = tempfile::tempdir().unwrap();
        let pidfile = scratch.path().join("descendant");
        let tool = Hdiutil {
            program: script(
                scratch.path(),
                "hdiutil-forks",
                &format!("sleep 30 &\necho $! > '{}'\nexit 0", pidfile.display()),
            ),
            timeout: Duration::from_millis(300),
        };
        let started = Instant::now();
        let error = tool.detach(Path::new("/m")).unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(matches!(error, CasVolumeError::Timeout { .. }), "{error}");
        // The whole process group was killed: the descendant is gone (once
        // its new parent reaps it).
        let descendant: libc::pid_t = fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while process_exists(descendant) {
            assert!(
                Instant::now() < deadline,
                "descendant {descendant} survived"
            );
            std::thread::yield_now();
        }
    }

    /// A shell script standing in for hdiutil.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let program = dir.join(name);
        fs::write(&program, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        program
    }

    /// Bead ley-line-open-167b29 (1): a hung hdiutil fails provisioning by
    /// name within its timeout instead of holding the volume's lock forever.
    #[test]
    fn a_tool_run_that_outlives_its_timeout_is_killed_and_named() {
        let scratch = tempfile::tempdir().unwrap();
        let hung = Hdiutil {
            program: script(scratch.path(), "hdiutil-hangs", "exec sleep 30"),
            timeout: Duration::from_millis(300),
        };
        let started = Instant::now();
        let error = hung.create(Path::new("/b"), 1 << 30).unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(
            matches!(&error, CasVolumeError::Timeout { command, after }
                if command.contains("create") && *after == Duration::from_millis(300)),
            "{error}"
        );
        assert!(
            error.to_string().contains("did not finish within"),
            "{error}"
        );

        let state = tempfile::tempdir().unwrap();
        let volume = CasVolume::with_parts(
            state.path(),
            4 << 30,
            HostOs::MacOs,
            Box::new(hung),
            sensitive,
        )
        .unwrap();
        for attempt in 0..2 {
            let error = volume.provision().unwrap_err();
            assert!(
                matches!(error, CasVolumeError::Timeout { .. }),
                "attempt {attempt}: {error}"
            );
        }
        assert_eq!(Hdiutil::default().timeout, DEFAULT_TOOL_TIMEOUT);
        assert_eq!(
            Hdiutil::with_timeout(Duration::from_secs(7)).timeout,
            Duration::from_secs(7)
        );
    }

    /// A tool that writes more than a pipe holds before exiting is not
    /// deadlocked by its own output, and its output is returned.
    #[test]
    fn a_chatty_tool_is_drained_while_it_runs() {
        let scratch = tempfile::tempdir().unwrap();
        let chatty = Hdiutil {
            program: script(
                scratch.path(),
                "hdiutil-chatty",
                "head -c 200000 /dev/zero | tr '\\0' a; head -c 200000 /dev/zero | tr '\\0' b >&2",
            ),
            timeout: Duration::from_secs(20),
        };
        let out = chatty.output(&["info".as_ref()]).unwrap();
        assert_eq!(out.len(), 200_000);
    }

    /// Bead ley-line-open-167b29 (2): a crash between `hdiutil create` and
    /// the rename leaves `partial-<pid>.sparsebundle`. Provisioning removes
    /// every one whose process is gone, and leaves a live provider's alone.
    #[test]
    fn stale_partial_bundles_are_removed_and_live_ones_kept() {
        let state = tempfile::tempdir().unwrap();
        let base = state.path().join("cas-volume");
        fs::create_dir_all(&base).unwrap();
        let mut exited = Command::new("/usr/bin/true").spawn().unwrap();
        let dead = exited.id();
        exited.wait().unwrap();
        let live = std::os::unix::process::parent_id();
        for pid in [dead, live] {
            let partial = base.join(format!("partial-{pid}.sparsebundle"));
            fs::create_dir(&partial).unwrap();
            fs::write(partial.join("Info.plist"), b"half made").unwrap();
        }
        fs::create_dir(base.join("partial-notapid.sparsebundle")).unwrap();
        volume(state.path(), HostOs::MacOs, &Fake::default(), sensitive)
            .provision()
            .unwrap();
        assert!(
            !base.join(format!("partial-{dead}.sparsebundle")).exists(),
            "a dead process's partial bundle is removed"
        );
        assert!(
            base.join(format!("partial-{live}.sparsebundle")).exists(),
            "a live process's partial bundle is left to it"
        );
        assert!(base.join("partial-notapid.sparsebundle").exists());
    }

    /// Bead ley-line-open-167b29 (3): a filesystem mounted at `mnt` that is
    /// not `cas.sparsebundle` (another image, or no image at all) is refused,
    /// whether it was already there or the attach produced it.
    #[test]
    fn a_mount_not_backed_by_the_cas_image_is_refused() {
        for reported in [Some(PathBuf::from("/elsewhere/other.sparsebundle")), None] {
            let state = tempfile::tempdir().unwrap();
            let fake = Fake {
                reports_image: Some(reported.clone()),
                ..Fake::default()
            };
            let volume = volume(state.path(), HostOs::MacOs, &fake, sensitive);
            let error = volume.provision().unwrap_err();
            assert!(
                matches!(&error, CasVolumeError::ForeignVolume { found, expected, .. }
                    if *found == reported && expected.ends_with("cas-volume/cas.sparsebundle")),
                "{error}"
            );
            assert!(
                error.to_string().contains("is not the CAS volume"),
                "{error}"
            );
            // Already mounted (by someone else): refused before use.
            fake.mounted.lock().insert(volume.layout().mountpoint);
            assert!(matches!(
                volume.attach_existing(),
                Err(CasVolumeError::ForeignVolume { .. })
            ));
        }
        // The image at the stable path is accepted, through a symlinked
        // spelling of the state directory too.
        let state = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        volume(state.path(), HostOs::MacOs, &fake, sensitive)
            .provision()
            .unwrap();
    }

    #[test]
    fn hdiutil_info_names_the_image_mounted_at_a_mountpoint() {
        let scratch = tempfile::tempdir().unwrap();
        let mnt = scratch.path().join("mnt");
        let other = scratch.path().join("other");
        fs::create_dir(&mnt).unwrap();
        fs::create_dir(&other).unwrap();
        let (mnt_text, other_text) = (mnt.to_string_lossy(), other.to_string_lossy());
        let info = info_plist(&[
            ("/a/other & <more>.dmg", &other_text),
            ("/a/cas.sparsebundle", &mnt_text),
        ]);
        let image = |mountpoint: &Path| attached_image_in_plist(&info, mountpoint).unwrap();
        assert_eq!(image(&mnt), Some(PathBuf::from("/a/cas.sparsebundle")));
        assert_eq!(image(&other), Some(PathBuf::from("/a/other & <more>.dmg")));
        assert_eq!(image(scratch.path()), None);
        assert_eq!(image(&scratch.path().join("gone")), None);
        assert_eq!(attached_image_in_plist(&info_plist(&[]), &mnt), Ok(None));
        for (broken, reason) in [
            ("", "not an XML property list"),
            ("framework : 710\n", "not an XML property list"),
            ("<plist><dict><key>images</key>", "not an XML property list"),
            (
                "<plist><dict><key>images</key><string>x</string></dict></plist>",
                "no `images` array",
            ),
            ("<plist><array/></plist>", "no `images` array"),
        ] {
            let error = attached_image_in_plist(broken, &mnt).expect_err(broken);
            assert!(error.contains(reason), "{broken}: {error}");
        }
    }

    /// Review of 167b29 (N3): the text form of `hdiutil info` let an
    /// attached image whose path holds a newline and the text of another
    /// entry pass for the CAS image mounted at the CAS mountpoint. In the
    /// property list that text is one string's content, never structure.
    #[test]
    fn a_crafted_image_path_cannot_pass_for_another_images_entry() {
        let scratch = tempfile::tempdir().unwrap();
        let mnt = scratch.path().join("mnt");
        let theirs = scratch.path().join("theirs");
        fs::create_dir(&mnt).unwrap();
        fs::create_dir(&theirs).unwrap();
        let forged = format!(
            "/tmp/x\n================================================\n\
             image-path      : /state/cas-volume/cas.sparsebundle\n\
             /dev/disk9s1\t41504653\t{mnt}\n\
             </string><key>mount-point</key><string>{mnt}",
            mnt = mnt.display()
        );
        let info = info_plist(&[(&forged, &theirs.to_string_lossy())]);
        assert_eq!(attached_image_in_plist(&info, &mnt), Ok(None));
        assert_eq!(
            attached_image_in_plist(&info, &theirs),
            Ok(Some(PathBuf::from(&forged)))
        );
    }

    #[test]
    fn property_lists_are_read_by_structure() {
        let parsed = parse_plist(
            "<?xml version=\"1.0\"?><!-- c --><plist version=\"1.0\"> <dict>\
             <key>s</key><string>a &amp; b &lt;&gt;&quot;&apos;&#65;&#x42;</string>\
             <key>empty</key><string></string><key>none</key><string/>\
             <key>n</key><integer>7</integer><key>t</key><true/>\
             <key>a</key><array><string>x</string><dict/><array/><real>1.5</real></array>\
             </dict> </plist>",
        )
        .unwrap();
        assert_eq!(
            parsed.get("s").and_then(Plist::as_str),
            Some("a & b <>\"'AB")
        );
        assert_eq!(parsed.get("empty"), Some(&Plist::String(String::new())));
        assert_eq!(parsed.get("none"), Some(&Plist::String(String::new())));
        assert_eq!(parsed.get("n"), Some(&Plist::Other));
        assert_eq!(parsed.get("t"), Some(&Plist::Other));
        assert_eq!(
            parsed.get("a").map(Plist::items),
            Some(
                &[
                    Plist::String("x".into()),
                    Plist::Dict(Vec::new()),
                    Plist::Array(Vec::new()),
                    Plist::Other
                ][..]
            )
        );
        assert_eq!(parsed.get("absent"), None);
        // Accessors on the wrong kind of value.
        assert_eq!(Plist::Other.get("s"), None);
        assert!(Plist::String("x".into()).items().is_empty());
        assert_eq!(Plist::Other.as_str(), None);
        for malformed in [
            "<plist><dict><key>k</key></dict></plist>",
            "<plist><dict><string>v</string></dict></plist>",
            "<plist><dict><key>k</key><string>v</dict></plist>",
            "<plist><dict><key>k<string>v</string></key></dict></plist>",
            // A value closed by another tag's name, with text and without.
            "<plist><dict><key>k</key><string>v</key></dict></plist>",
            "<plist><dict><key>k</key><string></key></dict></plist>",
            "<plist><dict><key>k</string><string>v</string></dict></plist>",
            // Text that is not whitespace, where only structure belongs: in
            // a dictionary, in an array, and between a key and its value.
            "<plist><dict>junk<key>k</key><string>v</string></dict></plist>",
            "<plist><array>junk<string>x</string></array></plist>",
            "<plist><dict><key>k</key>junk<string>v</string></dict></plist>",
            "<plist><string>a &bogus; b</string></plist>",
            "<plist><string>a &#xD800; b</string></plist>",
            "<plist><string>a & b</string></plist>",
            "<plist><string>unclosed",
            "<plist></plist>",
            "<dict></dict>",
            "text<plist><dict/></plist>",
            "<plist></dict></plist>",
        ] {
            assert_eq!(parse_plist(malformed), None, "{malformed}");
        }
        // Nesting is bounded.
        let nested = |depth: usize| {
            format!(
                "<plist>{}{}</plist>",
                "<array>".repeat(depth),
                "</array>".repeat(depth)
            )
        };
        assert!(parse_plist(&nested(MAX_PLIST_DEPTH + 1)).is_some());
        assert_eq!(parse_plist(&nested(MAX_PLIST_DEPTH + 2)), None);
        // Through dictionaries too.
        let nested_dicts = |depth: usize| {
            format!(
                "<plist>{}<string>x</string>{}</plist>",
                "<dict><key>k</key>".repeat(depth),
                "</dict>".repeat(depth)
            )
        };
        assert!(parse_plist(&nested_dicts(MAX_PLIST_DEPTH)).is_some());
        assert_eq!(parse_plist(&nested_dicts(MAX_PLIST_DEPTH + 1)), None);
        // Text before the first tag is kept as text, not dropped.
        assert_eq!(
            xml_tokens("a<b>c</b>"),
            Some(vec![
                XmlToken::Text("a".into()),
                XmlToken::Open("b"),
                XmlToken::Text("c".into()),
                XmlToken::Close("b"),
            ])
        );
        assert_eq!(
            xml_tokens("<a x=\"1\"><b/></a>"),
            Some(vec![
                XmlToken::Open("a"),
                XmlToken::Empty("b"),
                XmlToken::Close("a")
            ])
        );
    }

    #[test]
    fn a_tool_that_cannot_name_its_image_refuses() {
        struct Mute;
        impl DiskImages for Mute {
            fn create(&self, _: &Path, _: u64) -> Result<(), CasVolumeError> {
                Ok(())
            }
            fn attach(&self, _: &Path, _: &Path) -> Result<(), CasVolumeError> {
                Ok(())
            }
            fn detach(&self, _: &Path) -> Result<(), CasVolumeError> {
                Ok(())
            }
            fn is_mounted(&self, _: &Path) -> Result<bool, CasVolumeError> {
                Ok(true)
            }
        }
        assert!(matches!(
            Mute.attached_image(Path::new("/m")),
            Err(CasVolumeError::Config(detail)) if detail.contains("cannot say")
        ));
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

    /// Case-insensitive beneath any directory named `host-disk`, sensitive
    /// elsewhere: a volume beside the host's default filesystem.
    fn host_disk_is_insensitive(path: &Path) -> io::Result<FilesystemSensitivity> {
        Ok(FilesystemSensitivity {
            case_sensitive: !path
                .components()
                .any(|part| part.as_os_str() == "host-disk"),
            normalization_sensitive: false,
        })
    }

    /// Bead ley-line-open-741388: with no run root named, each run's
    /// rootfs goes on the volume, beside the CAS root, and provisioning
    /// makes it exist.
    #[test]
    fn the_default_run_root_is_on_the_volume_and_made_by_provisioning() {
        for (host, expected) in [
            (HostOs::MacOs, "cas-volume/mnt/runs"),
            (HostOs::Linux, "runs"),
        ] {
            let state = tempfile::tempdir().unwrap();
            let volume = volume(
                state.path(),
                host,
                &Fake::default(),
                host_disk_is_insensitive,
            );
            assert_eq!(volume.run_root(), state.path().join(expected));
            assert_eq!(volume.layout().run_root, state.path().join(expected));
            volume.check_run_root().unwrap();
            volume.provision().unwrap();
            assert!(state.path().join(expected).is_dir(), "{host:?}");
        }
    }

    /// An explicit run root on a case-insensitive filesystem is refused,
    /// naming both paths: at start when it is off the volume, and by every
    /// provision whatever it is.
    #[test]
    fn a_case_insensitive_run_root_is_refused_naming_both_paths() {
        for host in [HostOs::MacOs, HostOs::Linux] {
            let state = tempfile::tempdir().unwrap();
            let run_root = state.path().join("host-disk/runs");
            let volume = volume(
                state.path(),
                host,
                &Fake::default(),
                host_disk_is_insensitive,
            )
            .with_run_root(run_root.clone())
            .unwrap();
            for error in [
                volume.check_run_root().unwrap_err(),
                volume.provision().unwrap_err(),
            ] {
                assert!(
                    matches!(&error, CasVolumeError::RunRootCaseInsensitive { run_root: named, cas_root }
                        if *named == run_root && *cas_root == volume.cas_root()),
                    "{host:?}: {error}"
                );
                let message = error.to_string();
                assert!(
                    message.contains(&run_root.display().to_string())
                        && message.contains(&volume.cas_root().display().to_string()),
                    "{message}"
                );
            }
        }
    }

    /// A case-sensitive run root the operator names is accepted: off the
    /// volume, checked at start; on the macOS volume's mountpoint (where
    /// Cloister puts it), left until the volume is attached, then checked
    /// by every provision.
    #[test]
    fn a_case_sensitive_run_root_is_accepted_on_or_off_the_volume() {
        let state = tempfile::tempdir().unwrap();
        let elsewhere = state.path().join("fast-disk/runs");
        let volume = volume(
            state.path(),
            HostOs::Linux,
            &Fake::default(),
            host_disk_is_insensitive,
        )
        .with_run_root(elsewhere.clone())
        .unwrap();
        volume.check_run_root().unwrap();
        assert!(elsewhere.is_dir());

        let state = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        let on_volume = state.path().join("cas-volume/mnt/cloister-runs");
        // Even probed as insensitive, it is not checked (or created) at
        // start: the volume is not attached yet, and a directory made now
        // would be on the host's disk.
        let volume = self::volume(state.path(), HostOs::MacOs, &fake, insensitive)
            .with_run_root(on_volume.clone())
            .unwrap();
        volume.check_run_root().unwrap();
        assert!(!on_volume.exists());
        assert!(matches!(
            volume.provision(),
            Err(CasVolumeError::CaseInsensitive { .. })
        ));
        let volume = self::volume(state.path(), HostOs::MacOs, &fake, sensitive)
            .with_run_root(on_volume.clone())
            .unwrap();
        volume.provision().unwrap();
        assert!(on_volume.is_dir());
        assert!(matches!(
            volume.with_run_root(PathBuf::from("relative/runs")),
            Err(CasVolumeError::Config(detail)) if detail.contains("absolute")
        ));
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
            Hdiutil {
                program,
                timeout: DEFAULT_TOOL_TIMEOUT,
            }
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
            timeout: DEFAULT_TOOL_TIMEOUT,
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
