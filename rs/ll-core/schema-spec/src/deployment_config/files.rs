//! The D4 file rules: reading a secret file or a config document so that what
//! was checked is what is read (ADR-0049 D4).
//!
//! The parent directory is resolved with `realpath`, then walked from `/` one
//! component at a time with `openat(O_DIRECTORY | O_NOFOLLOW)`, and each
//! directory's rules are checked with `fstat` on the descriptor just opened:
//! owned by root or this process's user, and no group or other write bit
//! unless the sticky bit is set (as on `/private/tmp`) and the file is owned
//! by this process's user. The file is then opened relative to the last
//! directory descriptor with `O_NOFOLLOW` (a symlink is refused), `fstat`ed
//! and read through the same descriptor. A component swapped for a symlink
//! after `realpath` is refused by `O_NOFOLLOW`, never followed.
//!
//! A secret file must be a regular file owned by this process's user with no
//! group or other permission bit (`0600`, `0400`). A config document may also
//! be readable by group and others (`0644`, `0444`); any write bit beyond the
//! owner's is refused.

use std::path::{Path, PathBuf};

use super::{ConfigRefusal, Reason};

// libc's file-mode constants are `u16` on macOS and `u32` on Linux. Widening
// them once here keeps every comparison below portable; the cast is a no-op on
// Linux, which is the only reason the lint is allowed.
#[allow(clippy::unnecessary_cast)]
const S_ISVTX: u32 = libc::S_ISVTX as u32;
#[allow(clippy::unnecessary_cast)]
const S_IFMT: u32 = libc::S_IFMT as u32;
#[allow(clippy::unnecessary_cast)]
const S_IFREG: u32 = libc::S_IFREG as u32;

/// The largest file read: a secret or a document is small.
pub const MAX_FILE_BYTES: u64 = 1 << 20;

/// Which D4 rule a file is held to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileRule {
    /// A secret: owner-only (`0600`, `0400`).
    Secret,
    /// The `--config` document: owner-writable only, may be world-readable.
    Document,
}

impl FileRule {
    fn reason(self) -> Reason {
        match self {
            FileRule::Secret => Reason::SecretFilePermissions,
            FileRule::Document => Reason::ConfigFilePermissions,
        }
    }

    /// Mode bits that must be clear.
    fn forbidden_bits(self) -> u32 {
        match self {
            FileRule::Secret => 0o077,
            FileRule::Document => 0o022,
        }
    }

    /// The refusal for a missing file: a missing document is unreadable
    /// (exit 66); a missing secret fails a host check (exit 78).
    fn missing(self) -> Reason {
        match self {
            FileRule::Secret => Reason::HostCheckFailed,
            FileRule::Document => Reason::Unreadable,
        }
    }
}

/// Read a secret file under the D4 rules. `field` names the document field
/// in a refusal. A missing or empty file is refused.
pub fn read_secret_file(field: &str, path: &str) -> Result<Vec<u8>, ConfigRefusal> {
    match read_checked(field, Path::new(path), FileRule::Secret)? {
        Some(bytes) if bytes.is_empty() => Err(ConfigRefusal::new(
            field,
            Reason::SecretFileInvalid,
            format!("{path} is empty"),
        )),
        Some(bytes) => Ok(bytes),
        None => Err(ConfigRefusal::new(
            field,
            Reason::HostCheckFailed,
            format!("{path} does not exist"),
        )),
    }
}

/// Read a secret file that may be absent (the MCP token, generated at start
/// when absent): `None` when the file does not exist. An existing empty file
/// is refused rather than regenerated.
pub fn read_secret_file_if_present(
    field: &str,
    path: &str,
) -> Result<Option<Vec<u8>>, ConfigRefusal> {
    match read_checked(field, Path::new(path), FileRule::Secret)? {
        Some(bytes) if bytes.is_empty() => Err(ConfigRefusal::new(
            field,
            Reason::SecretFileInvalid,
            format!("{path} exists and is empty; it is refused, never regenerated"),
        )),
        other => Ok(other),
    }
}

/// Read the `--config` document under the D4 rules. A relative path is taken
/// against the working directory.
pub fn read_config_file(path: &Path) -> Result<Vec<u8>, ConfigRefusal> {
    let absolute = std::path::absolute(path).map_err(|e| {
        ConfigRefusal::new("", Reason::Unreadable, format!("{}: {e}", path.display()))
    })?;
    read_checked("", &absolute, FileRule::Document)?.ok_or_else(|| {
        ConfigRefusal::new(
            "",
            Reason::Unreadable,
            format!("{} does not exist", path.display()),
        )
    })
}

/// Create a secret file exclusively (`O_CREAT | O_EXCL | O_NOFOLLOW`, mode
/// `0600`) and write `bytes`; a file or symlink already at `path` is never
/// written through. Missing parent directories are created with mode `0700`.
/// Returns `false` when the file already exists (another process won the
/// race): read it instead.
pub fn create_secret_file(field: &str, path: &str, bytes: &[u8]) -> Result<bool, ConfigRefusal> {
    imp::create(field, Path::new(path), bytes)
}

/// Read `path` held to `rule`: `None` when the file does not exist (its
/// parent directory must). `field` names the document field in a refusal.
pub fn read_checked(
    field: &str,
    path: &Path,
    rule: FileRule,
) -> Result<Option<Vec<u8>>, ConfigRefusal> {
    imp::read(field, path, rule)
}

fn split(
    field: &str,
    path: &Path,
    rule: FileRule,
) -> Result<(PathBuf, std::ffi::OsString), ConfigRefusal> {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if path.is_absolute() => {
            Ok((parent.to_path_buf(), name.to_owned()))
        }
        _ => Err(ConfigRefusal::new(
            field,
            rule.reason(),
            format!(
                "{} does not name a file in an absolute directory",
                path.display()
            ),
        )),
    }
}

mod imp {
    use super::*;
    use std::ffi::{CString, OsStr};
    use std::fs::File;
    use std::io::{self, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    fn cstr(s: &OsStr) -> io::Result<CString> {
        CString::new(s.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
    }

    fn openat(
        dir: Option<&OwnedFd>,
        name: &OsStr,
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> io::Result<OwnedFd> {
        let name = cstr(name)?;
        let dirfd = dir.map_or(libc::AT_FDCWD, |d| d.as_raw_fd());
        // SAFETY: `name` is a NUL-terminated C string that outlives the call;
        // `dirfd` is AT_FDCWD or a descriptor `dir` owns.
        let fd = unsafe {
            libc::openat(
                dirfd,
                name.as_ptr(),
                flags | libc::O_CLOEXEC,
                mode as libc::c_uint,
            )
        };
        // openat returns exactly -1 on failure.
        if fd == -1 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: `fd` was just returned by openat and is owned by no one else.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    }

    /// Open directory `name` beneath `dir` without following a symlink. The
    /// one place the directory open flags are spelled, for the walk and for
    /// creation alike.
    fn open_dir(dir: Option<&OwnedFd>, name: &OsStr) -> io::Result<OwnedFd> {
        openat(
            dir,
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )
    }

    fn fstat(fd: &OwnedFd) -> io::Result<libc::stat> {
        let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `fd` is open; `st` is a valid out-pointer for one stat.
        if unsafe { libc::fstat(fd.as_raw_fd(), st.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fstat returned 0, so it initialized `st`.
        Ok(unsafe { st.assume_init() })
    }

    pub(super) fn euid() -> u32 {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }

    /// Refuse a symlink on `path` owned by neither root nor `me`. `realpath`
    /// discards the symlinks it crosses, so without this check another user
    /// could plant `/tmp/x -> <somewhere>` in a sticky directory and redirect
    /// where a secret is read or created (the rule Linux's
    /// `protected_symlinks` applies, which macOS lacks). Each prefix of
    /// `path` is `lstat`ed, so every symlink named on the path is checked.
    pub(super) fn check_symlink_owners(
        field: &str,
        path: &Path,
        rule: FileRule,
        me: u32,
    ) -> Result<(), ConfigRefusal> {
        use std::os::unix::fs::MetadataExt;
        let mut prefix = PathBuf::new();
        for component in path.components() {
            prefix.push(component);
            match std::fs::symlink_metadata(&prefix) {
                Ok(meta)
                    if meta.file_type().is_symlink() && meta.uid() != 0 && meta.uid() != me =>
                {
                    return Err(ConfigRefusal::new(
                        field,
                        rule.reason(),
                        format!(
                            "{} is a symlink owned by uid {}, neither root nor this user",
                            prefix.display(),
                            meta.uid()
                        ),
                    ));
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(e) => {
                    return Err(ConfigRefusal::new(
                        field,
                        rule.reason(),
                        format!("{}: {e}", prefix.display()),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Check a directory descriptor: owned by root or this user, and no
    /// group or other write bit unless the sticky bit is set.
    fn check_dir(
        fd: &OwnedFd,
        shown: &Path,
        refuse: &dyn Fn(String) -> ConfigRefusal,
    ) -> Result<(), ConfigRefusal> {
        let st = fstat(fd).map_err(|e| refuse(format!("{}: {e}", shown.display())))?;
        let mode = st.st_mode as u32;
        if st.st_uid != 0 && st.st_uid != euid() {
            return Err(refuse(format!(
                "directory {} is owned by uid {}, neither root nor this user",
                shown.display(),
                st.st_uid
            )));
        }
        if mode & 0o022 != 0 && mode & S_ISVTX == 0 {
            return Err(refuse(format!(
                "directory {} is writable by group or others",
                shown.display()
            )));
        }
        Ok(())
    }

    /// Open the already-resolved directory `resolved` one component at a
    /// time from `/` with no-follow, checking each descriptor. A component
    /// that is (or became) a symlink is refused, never followed.
    pub(super) fn open_resolved(
        resolved: &Path,
        refuse: &dyn Fn(String) -> ConfigRefusal,
    ) -> Result<OwnedFd, ConfigRefusal> {
        let mut current: Option<OwnedFd> = None;
        let mut shown = PathBuf::new();
        for component in resolved.components() {
            shown.push(component);
            let fd = open_dir(current.as_ref(), component.as_os_str()).map_err(|e| {
                refuse(format!(
                    "directory {} cannot be opened without following a symlink: {e}",
                    shown.display()
                ))
            })?;
            check_dir(&fd, &shown, refuse)?;
            current = Some(fd);
        }
        current.ok_or_else(|| refuse(format!("{} names no directory", resolved.display())))
    }

    /// Refuse foreign symlinks on `parent`, `realpath` it, then open it with
    /// [`open_resolved`].
    fn walk(field: &str, parent: &Path, rule: FileRule) -> Result<OwnedFd, ConfigRefusal> {
        let refuse = |why: String| ConfigRefusal::new(field, rule.reason(), why);
        check_symlink_owners(field, parent, rule, euid())?;
        let resolved = std::fs::canonicalize(parent).map_err(|e| {
            let reason = if e.kind() == io::ErrorKind::NotFound {
                rule.missing()
            } else {
                rule.reason()
            };
            ConfigRefusal::new(
                field,
                reason,
                format!("directory {}: {e}", parent.display()),
            )
        })?;
        open_resolved(&resolved, &refuse)
    }

    pub(super) fn read(
        field: &str,
        path: &Path,
        rule: FileRule,
    ) -> Result<Option<Vec<u8>>, ConfigRefusal> {
        let (parent, name) = split(field, path, rule)?;
        let refuse = |why: String| ConfigRefusal::new(field, rule.reason(), why);
        let dir = walk(field, &parent, rule)?;
        // O_NONBLOCK: a FIFO planted at the path opens at once and is then
        // refused as not a regular file, instead of blocking the check.
        let fd = match openat(
            Some(&dir),
            &name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY,
            0,
        ) {
            Ok(fd) => fd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
                return Err(refuse(format!("{} is a symlink", path.display())));
            }
            Err(e) => return Err(refuse(format!("{}: {e}", path.display()))),
        };
        let st = fstat(&fd).map_err(|e| refuse(format!("{}: {e}", path.display())))?;
        let mode = st.st_mode as u32;
        if mode & S_IFMT != S_IFREG {
            return Err(refuse(format!("{} is not a regular file", path.display())));
        }
        if st.st_uid != euid() {
            return Err(refuse(format!(
                "{} is owned by uid {}, not this user",
                path.display(),
                st.st_uid
            )));
        }
        // A second hard link to a secret (macOS has no protected_hardlinks)
        // is a name for it outside the checked directory: refused.
        if rule == FileRule::Secret && st.st_nlink != 1 {
            return Err(refuse(format!(
                "{} has {} hard links; a secret file has exactly one",
                path.display(),
                st.st_nlink
            )));
        }
        if mode & rule.forbidden_bits() != 0 {
            return Err(refuse(format!(
                "{} has mode {:04o}; {} is required",
                path.display(),
                mode & 0o7777,
                match rule {
                    FileRule::Secret => "0600 or 0400",
                    FileRule::Document => "no group or other write bit",
                }
            )));
        }
        // One bound: read at most one byte past the limit through the
        // checked descriptor, and refuse when that byte arrived.
        let mut bytes = Vec::new();
        File::from(fd)
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| refuse(format!("{}: {e}", path.display())))?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(ConfigRefusal::new(
                field,
                Reason::HostCheckFailed,
                format!("{} is larger than {MAX_FILE_BYTES} bytes", path.display()),
            ));
        }
        Ok(Some(bytes))
    }

    pub(super) fn create(field: &str, path: &Path, bytes: &[u8]) -> Result<bool, ConfigRefusal> {
        let rule = FileRule::Secret;
        let (parent, name) = split(field, path, rule)?;
        let refuse = |why: String| ConfigRefusal::new(field, rule.reason(), why);
        // Check the deepest existing ancestor, then make each missing
        // directory with mkdirat relative to the checked descriptor: nothing
        // is created before a check, and nothing through a symlink. A
        // directory another process creates in the meantime fails the
        // mkdirat, and the create is refused rather than adopting it.
        let mut existing = parent.clone();
        let mut missing: Vec<std::ffi::OsString> = Vec::new();
        while std::fs::symlink_metadata(&existing).is_err() {
            let (Some(up), Some(name)) = (existing.parent(), existing.file_name()) else {
                return Err(refuse(format!(
                    "{} has no existing ancestor",
                    parent.display()
                )));
            };
            missing.push(name.to_owned());
            existing = up.to_path_buf();
        }
        let mut dir = walk(field, &existing, rule)?;
        let mut shown = existing.clone();
        for name in missing.iter().rev() {
            shown.push(name);
            let c = cstr(name).map_err(|e| refuse(format!("{}: {e}", shown.display())))?;
            // SAFETY: `c` is NUL-terminated and outlives the call; `dir` is open.
            if unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), 0o700) } != 0 {
                let e = io::Error::last_os_error();
                return Err(refuse(format!("create directory {}: {e}", shown.display())));
            }
            let next = open_dir(Some(&dir), name).map_err(|e| {
                refuse(format!(
                    "directory {} cannot be opened without following a symlink: {e}",
                    shown.display()
                ))
            })?;
            check_dir(&next, &shown, &refuse)?;
            dir = next;
        }
        let fd = match openat(
            Some(&dir),
            &name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            0o600,
        ) {
            Ok(fd) => fd,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
            Err(e) => return Err(refuse(format!("create {}: {e}", path.display()))),
        };
        let mut file = File::from(fd);
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| refuse(format!("write {}: {e}", path.display())))?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn dir() -> tempfile::TempDir {
        let d = tempfile::TempDir::new().unwrap();
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        d
    }

    fn write(path: &Path, bytes: &[u8], mode: u32) {
        std::fs::write(path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn s(p: &Path) -> &str {
        p.to_str().unwrap()
    }

    fn refuse(why: String) -> ConfigRefusal {
        ConfigRefusal::new("f", Reason::SecretFilePermissions, why)
    }

    #[test]
    fn a_present_secret_is_returned_whole() {
        let d = dir();
        let f = d.path().join("token");
        write(&f, b"abc", 0o600);
        assert_eq!(
            read_secret_file_if_present("f", s(&f)).unwrap(),
            Some(b"abc".to_vec())
        );
    }

    /// A symlink owned by someone other than root and the reader is refused,
    /// wherever it sits on the path; one the reader owns is not. The owner is
    /// injected: a test cannot create a symlink another user owns.
    #[test]
    fn a_foreign_symlink_on_the_path_is_refused() {
        let d = dir();
        let real = d.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = d.path().join("link");
        symlink(&real, &link).unwrap();
        let mine = imp::euid();
        assert!(imp::check_symlink_owners("f", &link.join("k"), FileRule::Secret, mine).is_ok());
        let e = imp::check_symlink_owners("f", &link.join("k"), FileRule::Secret, mine + 4242)
            .unwrap_err();
        assert!(e.message.contains("is a symlink owned by uid"), "{e}");
        // A prefix that cannot be looked up for a reason other than absence
        // (a file used as a directory) is refused, not taken as absent.
        let file = d.path().join("file");
        write(&file, b"x", 0o600);
        assert!(imp::check_symlink_owners("f", &file.join("k"), FileRule::Secret, mine).is_err());
        // An absent prefix ends the check.
        assert!(
            imp::check_symlink_owners("f", &d.path().join("none/k"), FileRule::Secret, mine)
                .is_ok()
        );
    }

    /// The resolved walk opens each component with no-follow, refuses a
    /// symlink and a non-directory, and names the directory it refused.
    #[test]
    fn the_resolved_walk_never_follows_and_names_the_directory() {
        let d = dir();
        let resolved = std::fs::canonicalize(d.path()).unwrap();
        let real = resolved.join("real");
        std::fs::create_dir(&real).unwrap();
        assert!(imp::open_resolved(&real, &refuse).is_ok());
        symlink(&real, resolved.join("link")).unwrap();
        let e = imp::open_resolved(&resolved.join("link"), &refuse).unwrap_err();
        assert!(
            e.message.contains(&format!(
                "directory {} cannot be opened",
                resolved.join("link").display()
            )),
            "{e}"
        );
        write(&resolved.join("file"), b"x", 0o600);
        assert!(imp::open_resolved(&resolved.join("file"), &refuse).is_err());
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o770)).unwrap();
        let e = imp::open_resolved(&real, &refuse).unwrap_err();
        assert!(
            e.message.contains(&format!(
                "directory {} is writable by group",
                real.display()
            )),
            "{e}"
        );
    }

    /// The size bound: exactly `MAX_FILE_BYTES` is read whole, one byte more
    /// is refused.
    #[test]
    fn the_size_bound_is_exact() {
        let d = dir();
        let f = d.path().join("doc.json");
        write(&f, &vec![b'a'; MAX_FILE_BYTES as usize], 0o600);
        assert_eq!(read_config_file(&f).unwrap().len() as u64, MAX_FILE_BYTES);
        write(&f, &vec![b'a'; MAX_FILE_BYTES as usize + 1], 0o600);
        let e = read_config_file(&f).unwrap_err();
        assert_eq!(e.reason, Reason::HostCheckFailed);
    }

    /// A FIFO at the path is refused at once rather than blocking the read.
    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let d = dir();
        let fifo = d.path().join("fifo");
        let c = std::ffi::CString::new(s(&fifo)).unwrap();
        // SAFETY: `c` is a NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let e = read_secret_file("f", s(&fifo)).unwrap_err();
        assert!(e.message.contains("not a regular file"), "{e}");
    }

    /// An open failure that is not a symlink is not reported as one, and a
    /// symlink is.
    #[test]
    fn open_failures_are_named_for_what_they_are() {
        let d = dir();
        let f = d.path().join("locked");
        write(&f, b"x", 0o000);
        let e = read_secret_file("f", s(&f)).unwrap_err();
        assert!(!e.message.contains("is a symlink"), "{e}");
        symlink(&f, d.path().join("link")).unwrap();
        let e = read_secret_file("f", s(&d.path().join("link"))).unwrap_err();
        assert!(e.message.contains("is a symlink"), "{e}");
    }

    /// Creation into a directory this user cannot write is refused, whether
    /// the directory or the file cannot be made.
    #[test]
    fn creation_without_write_permission_is_refused() {
        let d = dir();
        let ro = d.path().join("ro");
        std::fs::create_dir(&ro).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o500)).unwrap();
        let e = create_secret_file("f", s(&ro.join("t")), b"t").unwrap_err();
        assert!(e.message.contains("create"), "{e}");
        let e = create_secret_file("f", s(&ro.join("sub/t")), b"t").unwrap_err();
        assert!(e.message.contains("create directory"), "{e}");
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn a_secret_must_be_owner_only() {
        let d = dir();
        let f = d.path().join("key");
        for mode in [0o600, 0o400] {
            write(&f, b"seed", mode);
            assert_eq!(read_secret_file("head.signingKey", s(&f)).unwrap(), b"seed");
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        for mode in [0o640, 0o604, 0o644, 0o700 | 0o010] {
            write(&f, b"seed", mode);
            let e = read_secret_file("head.signingKey", s(&f)).unwrap_err();
            assert_eq!(
                (e.field.as_str(), e.reason),
                ("head.signingKey", Reason::SecretFilePermissions),
                "{mode:o}"
            );
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn a_document_may_be_world_readable_but_not_writable() {
        let d = dir();
        let f = d.path().join("c.json");
        for mode in [0o644, 0o444, 0o600] {
            write(&f, b"{}", mode);
            assert_eq!(read_config_file(&f).unwrap(), b"{}");
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        for mode in [0o664, 0o646] {
            write(&f, b"{}", mode);
            assert_eq!(
                read_config_file(&f).unwrap_err().reason,
                Reason::ConfigFilePermissions
            );
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let e = read_config_file(&d.path().join("missing.json")).unwrap_err();
        assert_eq!(e.reason, Reason::Unreadable);
        assert_eq!(e.reason.exit_code(), 66);
        let e = read_config_file(&d.path().join("nodir/c.json")).unwrap_err();
        assert_eq!(e.reason, Reason::Unreadable);
    }

    #[test]
    fn a_symlink_file_is_refused_and_never_followed() {
        let d = dir();
        let target = d.path().join("real");
        write(&target, b"seed", 0o600);
        let link = d.path().join("link");
        symlink(&target, &link).unwrap();
        assert_eq!(
            read_secret_file("f", s(&link)).unwrap_err().reason,
            Reason::SecretFilePermissions
        );
        assert_eq!(
            read_config_file(&link).unwrap_err().reason,
            Reason::ConfigFilePermissions
        );
    }

    #[test]
    fn a_symlinked_directory_is_checked_where_it_really_lives() {
        let d = dir();
        let real = d.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        write(&real.join("key"), b"seed", 0o600);
        let alias = d.path().join("alias");
        symlink(&real, &alias).unwrap();
        // The parent is realpath'd: a symlinked directory (macOS /tmp) is
        // accepted when the directories it resolves to pass.
        assert_eq!(
            read_secret_file("f", s(&alias.join("key"))).unwrap(),
            b"seed"
        );
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert_eq!(
            read_secret_file("f", s(&alias.join("key")))
                .unwrap_err()
                .reason,
            Reason::SecretFilePermissions,
            "the resolved directory's group write bit is checked"
        );
    }

    #[test]
    fn a_sticky_shared_directory_is_accepted_for_the_users_own_file() {
        let d = dir();
        let shared = d.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
        write(&shared.join("key"), b"seed", 0o600);
        assert_eq!(
            read_secret_file("f", s(&shared.join("key"))).unwrap(),
            b"seed"
        );
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(
            read_secret_file("f", s(&shared.join("key")))
                .unwrap_err()
                .reason,
            Reason::SecretFilePermissions
        );
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn missing_empty_and_non_regular_secrets() {
        let d = dir();
        let missing = d.path().join("missing");
        assert_eq!(
            read_secret_file("f", s(&missing)).unwrap_err().reason,
            Reason::HostCheckFailed
        );
        assert_eq!(read_secret_file_if_present("f", s(&missing)).unwrap(), None);
        let empty = d.path().join("empty");
        write(&empty, b"", 0o600);
        assert_eq!(
            read_secret_file("f", s(&empty)).unwrap_err().reason,
            Reason::SecretFileInvalid
        );
        assert_eq!(
            read_secret_file_if_present("f", s(&empty))
                .unwrap_err()
                .reason,
            Reason::SecretFileInvalid
        );
        let sub = d.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        assert_eq!(
            read_secret_file("f", s(&sub)).unwrap_err().reason,
            Reason::SecretFilePermissions
        );
        assert_eq!(
            read_secret_file("f", "relative/key").unwrap_err().reason,
            Reason::SecretFilePermissions
        );
        assert_eq!(
            read_secret_file("f", "/").unwrap_err().reason,
            Reason::SecretFilePermissions
        );
    }

    /// A second hard link is a name for the secret outside the checked
    /// directory: refused. A document may have one.
    #[test]
    fn a_hard_linked_secret_is_refused() {
        let d = dir();
        let f = d.path().join("key");
        write(&f, b"seed", 0o600);
        std::fs::hard_link(&f, d.path().join("alias")).unwrap();
        let e = read_secret_file("f", s(&f)).unwrap_err();
        assert!(e.message.contains("hard links"), "{e}");
        assert_eq!(e.reason, Reason::SecretFilePermissions);
    }

    /// Missing directories are made one at a time, each relative to the
    /// checked descriptor of its parent, owner-only; a symlinked parent this
    /// user owns is followed through realpath and checked where it lives.
    #[test]
    fn creation_makes_missing_directories_through_checked_descriptors() {
        let d = dir();
        let real = d.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        let alias = d.path().join("alias");
        symlink(&real, &alias).unwrap();
        let token = alias.join("a/b/daemon.token");
        assert!(create_secret_file("mcp.tokenFile", s(&token), b"t").unwrap());
        for sub in ["a", "a/b"] {
            let mode = std::fs::metadata(real.join(sub))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "{sub}");
        }
        assert_eq!(std::fs::read(real.join("a/b/daemon.token")).unwrap(), b"t");
        // A group-writable existing ancestor is refused before anything is made.
        let open = d.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(create_secret_file("f", s(&open.join("x/t")), b"t").is_err());
        assert!(
            !open.join("x").exists(),
            "nothing is created before the check"
        );
    }

    #[test]
    fn creation_is_exclusive_and_never_follows_a_planted_symlink() {
        let d = dir();
        let token = d.path().join("leyline/daemon.token");
        assert!(create_secret_file("mcp.tokenFile", s(&token), b"t0k").unwrap());
        let meta = std::fs::metadata(&token).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(token.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert!(
            !create_secret_file("mcp.tokenFile", s(&token), b"other").unwrap(),
            "exists: not rewritten"
        );
        assert_eq!(read_secret_file("f", s(&token)).unwrap(), b"t0k");
        let victim = d.path().join("victim");
        write(&victim, b"keep", 0o600);
        let planted = d.path().join("leyline/planted");
        symlink(&victim, &planted).unwrap();
        assert!(!create_secret_file("mcp.tokenFile", s(&planted), b"x").unwrap());
        assert_eq!(
            std::fs::read(&victim).unwrap(),
            b"keep",
            "never written through"
        );
        let dangling = d.path().join("leyline/dangling");
        symlink(d.path().join("nowhere"), &dangling).unwrap();
        assert!(!create_secret_file("mcp.tokenFile", s(&dangling), b"x").unwrap());
        assert!(
            !d.path().join("nowhere").exists(),
            "a dangling symlink is not created through"
        );
    }
}
