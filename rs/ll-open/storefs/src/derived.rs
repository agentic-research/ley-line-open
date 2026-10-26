//! Derived paths (ADR-0045 T9, bead `ley-line-open-39998b`): build output
//! and tool state that storefs never proposes. They live in a scratch
//! directory one [`crate::StoreFs`] owns, outside the store, its tree root
//! and the checkout, and are removed with it.
//!
//! Every scratch access resolves its path component by component with
//! `openat` and `O_NOFOLLOW` beneath the scratch root: a symlink the guest
//! made in scratch is served to it as a symlink and never followed here.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;

use crate::{Errno, Kind};

/// The default scratch limit: 16 GiB.
pub const DEFAULT_LIMIT: u64 = 16 << 30;

/// Where a [`crate::StoreFs`] keeps its derived paths: a fresh directory
/// under `parent`, made on the first derived write, holding at most
/// `limit` bytes of file data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Derived {
    pub parent: PathBuf,
    pub limit: u64,
}

impl Derived {
    /// Scratch under the system temporary directory, [`DEFAULT_LIMIT`].
    pub fn in_temp_dir() -> Self {
        Derived {
            parent: std::env::temp_dir(),
            limit: DEFAULT_LIMIT,
        }
    }
}

/// Directory names that are always generated: the parse walk's
/// `is_bloat_dir` names, without `vendor` and without dot-names in general
/// (vendored and dot-prefixed files are often authored), with `.git`.
const GENERATED_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "__pycache__",
    "__pypackages__",
    "venv",
    ".venv",
    ".git",
];

/// Whether the built-in rules make `path` derived: a directory component
/// in [`GENERATED_DIRS`] (the last component counts when `is_dir`), or a
/// macOS metadata file (`._*`, `.DS_Store`).
pub(crate) fn builtin_derived(path: &str, is_dir: bool) -> bool {
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        let last = parts.peek().is_none();
        if (!last || is_dir) && GENERATED_DIRS.contains(&part) {
            return true;
        }
        if last && (part.starts_with("._") || part == ".DS_Store") {
            return true;
        }
    }
    false
}

/// What scratch holds at a path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ScratchStat {
    pub kind: Kind,
    pub size: u64,
    /// Permission bits.
    pub perm: u32,
    pub mtime: Duration,
}

/// One open of a scratch file.
pub(crate) struct ScratchHandle {
    pub file: File,
    pub writable: bool,
}

/// The scratch directory: removed when dropped.
pub(crate) struct Scratch {
    dir: PathBuf,
    root: OwnedFd,
    limit: u64,
    /// Bytes of regular-file data scratch holds.
    used: Mutex<u64>,
}

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A directory opened for reading: `O_RDONLY` is the zero access mode, so
/// the flags are `O_DIRECTORY` alone.
const READ_DIR: i32 = libc::O_DIRECTORY;

impl Scratch {
    /// A fresh scratch directory under `parent`, readable and writable by
    /// this process only.
    pub(crate) fn create(derived: &Derived) -> io::Result<Scratch> {
        loop {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = derived
                .parent
                .join(format!("leyline-scratch-{}-{n}", std::process::id()));
            match std::fs::create_dir(&dir) {
                Ok(()) => {
                    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
                    let root = open_path(&dir, READ_DIR)?;
                    return Ok(Scratch {
                        dir,
                        root,
                        limit: derived.limit,
                        used: Mutex::new(0),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// The directory `path`'s last component is in, opened beneath the
    /// root, and that component. With `make`, missing directories on the
    /// way are made.
    fn parent_of(&self, path: &str, make: bool) -> io::Result<(Dir, CString)> {
        let mut parts: Vec<&str> = path.split('/').collect();
        let last = parts.pop().unwrap_or_default();
        let mut dir = Dir::Borrowed(self.root.as_raw_fd());
        for part in parts {
            let name = cname(part)?;
            let next = match openat(dir.fd(), &name, READ_DIR) {
                Err(e) if make && e.raw_os_error() == Some(libc::ENOENT) => {
                    mkdirat(dir.fd(), &name, 0o755)?;
                    openat(dir.fd(), &name, READ_DIR)?
                }
                other => other?,
            };
            dir = Dir::Owned(next);
        }
        Ok((dir, cname(last)?))
    }

    /// What scratch holds at `path`, without following a symlink.
    pub(crate) fn stat(&self, path: &str) -> io::Result<Option<ScratchStat>> {
        let (dir, name) = match self.parent_of(path, false) {
            Ok(found) => found,
            Err(e) if missing(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        match fstatat(dir.fd(), &name) {
            Ok(st) => Ok(Some(st)),
            Err(e) if missing(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The names directly under the directory `path`, with their kinds.
    pub(crate) fn list(&self, path: &str) -> io::Result<Vec<(String, Kind)>> {
        let dir = if path.is_empty() {
            Dir::Borrowed(self.root.as_raw_fd())
        } else {
            let (parent, name) = match self.parent_of(path, false) {
                Ok(found) => found,
                Err(e) if missing(&e) => return Ok(Vec::new()),
                Err(e) => return Err(e),
            };
            match openat(parent.fd(), &name, READ_DIR) {
                Ok(fd) => Dir::Owned(fd),
                Err(e) if missing(&e) => return Ok(Vec::new()),
                Err(e) => return Err(e),
            }
        };
        let mut out = Vec::new();
        for name in read_names(dir.fd())? {
            let c = cname(&name)?;
            if let Ok(st) = fstatat(dir.fd(), &c) {
                out.push((name, st.kind));
            }
        }
        Ok(out)
    }

    /// Open the file `path` with `flags` (`O_CREAT` makes missing parent
    /// directories too), never following a symlink.
    pub(crate) fn open(&self, path: &str, flags: i32, mode: u32) -> io::Result<File> {
        let make = flags & libc::O_CREAT != 0;
        let (dir, name) = self.parent_of(path, make)?;
        let fd = openat_mode(dir.fd(), &name, flags, mode)?;
        Ok(File::from(fd))
    }

    pub(crate) fn mkdir(&self, path: &str) -> io::Result<()> {
        let (dir, name) = self.parent_of(path, true)?;
        mkdirat(dir.fd(), &name, 0o755)
    }

    pub(crate) fn symlink(&self, target: &[u8], path: &str) -> io::Result<()> {
        let (dir, name) = self.parent_of(path, true)?;
        let target =
            CString::new(target).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        // SAFETY: both pointers are NUL-terminated strings that outlive the
        // call, and `dir` is an open directory descriptor.
        check(unsafe { libc::symlinkat(target.as_ptr(), dir.fd(), name.as_ptr()) })
    }

    pub(crate) fn readlink(&self, path: &str) -> io::Result<Vec<u8>> {
        let (dir, name) = self.parent_of(path, false)?;
        let mut buf = vec![0u8; 4096];
        // SAFETY: `buf` is writable for its length, `name` is NUL-terminated,
        // and `dir` is an open directory descriptor.
        let n = unsafe {
            libc::readlinkat(dir.fd(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len())
        };
        // -1 on failure; otherwise the target's length.
        let n = usize::try_from(n).map_err(|_| io::Error::last_os_error())?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Remove the file or symlink `path`, freeing its bytes.
    pub(crate) fn unlink(&self, path: &str) -> io::Result<()> {
        let (dir, name) = self.parent_of(path, false)?;
        let freed = fstatat(dir.fd(), &name)
            .ok()
            .filter(|s| s.kind == Kind::File)
            .map_or(0, |s| s.size);
        // SAFETY: `name` is NUL-terminated and `dir` is an open directory
        // descriptor.
        check(unsafe { libc::unlinkat(dir.fd(), name.as_ptr(), 0) })?;
        self.release(freed);
        Ok(())
    }

    pub(crate) fn rmdir(&self, path: &str) -> io::Result<()> {
        let (dir, name) = self.parent_of(path, false)?;
        // SAFETY: as in `unlink`.
        check(unsafe { libc::unlinkat(dir.fd(), name.as_ptr(), libc::AT_REMOVEDIR) })
    }

    /// Move `from` to `to` within scratch; a file replaced at `to` frees
    /// its bytes.
    pub(crate) fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let (from_dir, from_name) = self.parent_of(from, false)?;
        let (to_dir, to_name) = self.parent_of(to, true)?;
        let freed = fstatat(to_dir.fd(), &to_name)
            .ok()
            .filter(|s| s.kind == Kind::File)
            .map_or(0, |s| s.size);
        // SAFETY: both names are NUL-terminated and both descriptors are
        // open directories.
        check(unsafe {
            libc::renameat(
                from_dir.fd(),
                from_name.as_ptr(),
                to_dir.fd(),
                to_name.as_ptr(),
            )
        })?;
        self.release(freed);
        Ok(())
    }

    /// Set the permission bits of the file or directory `path`.
    pub(crate) fn chmod(&self, path: &str, perm: u32) -> io::Result<()> {
        let (dir, name) = self.parent_of(path, false)?;
        let fd = openat(dir.fd(), &name, libc::O_RDONLY)?;
        File::from(fd).set_permissions(std::fs::Permissions::from_mode(perm & 0o7777))
    }

    /// WRITE `data` at `offset` of `file`, refused `ENOSPC` when the bytes
    /// it adds would pass the limit.
    pub(crate) fn write(&self, file: &File, offset: u64, data: &[u8]) -> io::Result<()> {
        let size = file.metadata()?.len();
        let end = offset.saturating_add(data.len() as u64);
        self.reserve(end.saturating_sub(size))?;
        if let Err(e) = file.write_all_at(data, offset) {
            self.release(end.saturating_sub(size));
            return Err(e);
        }
        Ok(())
    }

    /// Truncate or extend `file` to `size`, within the limit.
    pub(crate) fn set_len(&self, file: &File, size: u64) -> io::Result<()> {
        let old = file.metadata()?.len();
        self.reserve(size.saturating_sub(old))?;
        file.set_len(size)?;
        self.release(old.saturating_sub(size));
        Ok(())
    }

    fn reserve(&self, bytes: u64) -> io::Result<()> {
        let mut used = self.used.lock();
        if used.saturating_add(bytes) > self.limit {
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        *used += bytes;
        Ok(())
    }

    fn release(&self, bytes: u64) {
        let mut used = self.used.lock();
        *used = used.saturating_sub(bytes);
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // `remove_dir_all` does not follow symlinks.
        if let Err(e) = std::fs::remove_dir_all(&self.dir) {
            log::warn!("scratch {} not removed: {e}", self.dir.display());
        }
    }
}

/// The attributes of an open scratch file.
pub(crate) fn stat_file(file: &File) -> io::Result<ScratchStat> {
    let meta = file.metadata()?;
    Ok(stat_of(
        meta.mode(),
        meta.size(),
        meta.mtime(),
        meta.mtime_nsec(),
    ))
}

/// A directory descriptor: the scratch root, borrowed, or one opened on
/// the way to a path.
enum Dir {
    Borrowed(RawFd),
    Owned(OwnedFd),
}

impl Dir {
    fn fd(&self) -> RawFd {
        match self {
            Dir::Borrowed(fd) => *fd,
            Dir::Owned(fd) => fd.as_raw_fd(),
        }
    }
}

fn cname(name: &str) -> io::Result<CString> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    CString::new(name).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

fn missing(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTDIR))
}

/// A syscall's `int` result: `-1` is failure, with `errno` set.
fn check(rc: libc::c_int) -> io::Result<()> {
    if rc == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// A descriptor a syscall just returned, owned from here; `-1` is failure.
fn owned(fd: libc::c_int) -> io::Result<OwnedFd> {
    check(fd)?;
    // SAFETY: `fd` was just returned by the kernel to this process and is
    // owned by no one else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Open `path` with `flags`, close-on-exec. `flags` never carries
/// `O_CLOEXEC` itself (debug-asserted).
fn open_path(path: &Path, flags: i32) -> io::Result<OwnedFd> {
    debug_assert_eq!(flags & libc::O_CLOEXEC, 0);
    let c = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: `c` is NUL-terminated.
    owned(unsafe { libc::open(c.as_ptr(), flags | libc::O_CLOEXEC) })
}

fn openat(dir: RawFd, name: &CStr, flags: i32) -> io::Result<OwnedFd> {
    openat_mode(dir, name, flags, 0)
}

/// Open `name` in `dir` with `flags`, never following a symlink, and
/// close-on-exec. `flags` never carries `O_NOFOLLOW` or `O_CLOEXEC` itself
/// (debug-asserted).
fn openat_mode(dir: RawFd, name: &CStr, flags: i32, mode: u32) -> io::Result<OwnedFd> {
    debug_assert_eq!(flags & (libc::O_NOFOLLOW | libc::O_CLOEXEC), 0);
    // SAFETY: `name` is NUL-terminated and `dir` is an open directory
    // descriptor.
    owned(unsafe {
        libc::openat(
            dir,
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    })
}

fn mkdirat(dir: RawFd, name: &CStr, mode: libc::mode_t) -> io::Result<()> {
    // SAFETY: `name` is NUL-terminated and `dir` is an open directory
    // descriptor.
    check(unsafe { libc::mkdirat(dir, name.as_ptr(), mode) })
}

fn fstatat(dir: RawFd, name: &CStr) -> io::Result<ScratchStat> {
    // SAFETY: an all-zero `stat` is a valid value for the out-parameter.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `name` is NUL-terminated, `st` is writable, and `dir` is an
    // open directory descriptor.
    check(unsafe { libc::fstatat(dir, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) })?;
    Ok(stat_of(
        st.st_mode as u32,
        st.st_size as u64,
        st.st_mtime,
        st.st_mtime_nsec,
    ))
}

fn stat_of(mode: u32, size: u64, secs: i64, nanos: i64) -> ScratchStat {
    let kind = match mode & libc::S_IFMT as u32 {
        m if m == libc::S_IFDIR as u32 => Kind::Directory,
        m if m == libc::S_IFLNK as u32 => Kind::Symlink,
        _ => Kind::File,
    };
    ScratchStat {
        kind,
        size: if kind == Kind::Directory { 0 } else { size },
        perm: mode & 0o777,
        mtime: Duration::new(secs.max(0) as u64, nanos.clamp(0, 999_999_999) as u32),
    }
}

/// The names in the open directory `dir`, without `.` and `..`.
fn read_names(dir: RawFd) -> io::Result<Vec<String>> {
    // SAFETY: `dir` is an open descriptor; `dup` returns a new one or -1.
    let dup = unsafe { libc::dup(dir) };
    check(dup)?;
    // SAFETY: `dup` is a fresh descriptor; `fdopendir` takes ownership of it
    // on success.
    let stream = unsafe { libc::fdopendir(dup) };
    if stream.is_null() {
        let e = io::Error::last_os_error();
        // SAFETY: `fdopendir` failed, so `dup` is still ours to close.
        unsafe { libc::close(dup) };
        return Err(e);
    }
    // `dup` shares the file offset with `dir`; start from the beginning.
    // SAFETY: `stream` is the open directory stream `fdopendir` returned.
    unsafe { libc::rewinddir(stream) };
    let mut out = Vec::new();
    loop {
        // SAFETY: `stream` is open; `readdir` returns an entry or null.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        // SAFETY: a non-null entry's `d_name` is a NUL-terminated string
        // valid until the next `readdir` on `stream`.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        let name = name.to_string_lossy();
        if name != "." && name != ".." {
            out.push(name.into_owned());
        }
    }
    // SAFETY: `stream` is open and is closed exactly once here, closing `dup`.
    unsafe { libc::closedir(stream) };
    Ok(out)
}

/// The Linux errno a scratch operation's failure answers (the guest is
/// Linux; the host's values differ on macOS).
pub(crate) fn errno_of(e: &io::Error) -> Errno {
    match e.raw_os_error() {
        Some(libc::ENOENT) => Errno::ENOENT,
        Some(libc::EEXIST) => Errno::EEXIST,
        Some(libc::ENOTEMPTY) => Errno::ENOTEMPTY,
        Some(libc::ENOTDIR) => Errno::ENOTDIR,
        Some(libc::EISDIR) => Errno::EISDIR,
        Some(libc::EACCES) => Errno::EACCES,
        Some(libc::EPERM) => Errno::EPERM,
        Some(libc::EINVAL) => Errno::EINVAL,
        Some(libc::EXDEV) => Errno::EXDEV,
        Some(libc::ELOOP) => Errno::ELOOP,
        Some(libc::ENAMETOOLONG) => Errno::ENAMETOOLONG,
        Some(libc::ENOSPC) | Some(libc::EDQUOT) => Errno::ENOSPC,
        _ => Errno::EIO,
    }
}

#[cfg(test)]
#[path = "scratch_tests.rs"]
mod tests;
