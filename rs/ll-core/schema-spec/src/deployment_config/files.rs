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

#[cfg(unix)]
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
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: `fd` was just returned by openat and is owned by no one else.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
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

    fn euid() -> u32 {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }

    struct Walk {
        dir: OwnedFd,
        /// A group- or other-writable sticky directory was crossed: the file
        /// must then be owned by this process's user (it must be anyway).
        sticky_writable: bool,
    }

    /// `realpath` the parent, then open it one component at a time from `/`
    /// with no-follow, checking each directory's descriptor.
    fn walk(field: &str, parent: &Path, rule: FileRule) -> Result<Walk, ConfigRefusal> {
        let refuse = |why: String| ConfigRefusal::new(field, rule.reason(), why);
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
        let me = euid();
        let mut sticky_writable = false;
        let mut current: Option<OwnedFd> = None;
        let mut shown = PathBuf::from("/");
        let components: Vec<&OsStr> = std::iter::once(OsStr::new("/"))
            .chain(resolved.components().skip(1).map(|c| c.as_os_str()))
            .collect();
        for (i, name) in components.into_iter().enumerate() {
            if i > 0 {
                shown.push(name);
            }
            let fd = openat(
                current.as_ref(),
                name,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                0,
            )
            .map_err(|e| {
                refuse(format!(
                    "directory {} cannot be opened without following a symlink: {e}",
                    shown.display()
                ))
            })?;
            let st = fstat(&fd).map_err(|e| refuse(format!("{}: {e}", shown.display())))?;
            let mode = st.st_mode as u32;
            if st.st_uid != 0 && st.st_uid != me {
                return Err(refuse(format!(
                    "directory {} is owned by uid {}, neither root nor this user",
                    shown.display(),
                    st.st_uid
                )));
            }
            if mode & 0o022 != 0 {
                if mode & (libc::S_ISVTX as u32) == 0 {
                    return Err(refuse(format!(
                        "directory {} is writable by group or others",
                        shown.display()
                    )));
                }
                sticky_writable = true;
            }
            current = Some(fd);
        }
        Ok(Walk {
            // The loop opened at least `/`.
            dir: current.expect("the walk opens /"),
            sticky_writable,
        })
    }

    pub(super) fn read(
        field: &str,
        path: &Path,
        rule: FileRule,
    ) -> Result<Option<Vec<u8>>, ConfigRefusal> {
        let (parent, name) = split(field, path, rule)?;
        let refuse = |why: String| ConfigRefusal::new(field, rule.reason(), why);
        let walk = walk(field, &parent, rule)?;
        let fd = match openat(
            Some(&walk.dir),
            &name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
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
        if mode & (libc::S_IFMT as u32) != libc::S_IFREG as u32 {
            return Err(refuse(format!("{} is not a regular file", path.display())));
        }
        if st.st_uid != euid() {
            return Err(refuse(format!(
                "{} is owned by uid {}, not this user{}",
                path.display(),
                st.st_uid,
                if walk.sticky_writable {
                    " (in a shared sticky directory)"
                } else {
                    ""
                }
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
        if st.st_size as u64 > MAX_FILE_BYTES {
            return Err(ConfigRefusal::new(
                field,
                Reason::HostCheckFailed,
                format!("{} is larger than {MAX_FILE_BYTES} bytes", path.display()),
            ));
        }
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
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&parent)
                .map_err(|e| refuse(format!("create directory {}: {e}", parent.display())))?;
        }
        let walk = walk(field, &parent, rule)?;
        let fd = match openat(
            Some(&walk.dir),
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

#[cfg(not(unix))]
mod imp {
    use super::*;

    fn unsupported(field: &str) -> ConfigRefusal {
        ConfigRefusal::new(
            field,
            Reason::UnsupportedOs,
            "the D4 file rules need a Unix host",
        )
    }

    pub(super) fn read(
        field: &str,
        _: &Path,
        _: FileRule,
    ) -> Result<Option<Vec<u8>>, ConfigRefusal> {
        Err(unsupported(field))
    }

    pub(super) fn create(field: &str, _: &Path, _: &[u8]) -> Result<bool, ConfigRefusal> {
        Err(unsupported(field))
    }
}

#[cfg(all(test, unix))]
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
