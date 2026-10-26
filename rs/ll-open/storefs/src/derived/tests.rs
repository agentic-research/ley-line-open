//! The scratch directory derived paths live in (ADR-0045 T9), below
//! `StoreFs`: confinement, accounting, error mapping.

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;

use super::*;

fn scratch(limit: u64) -> (tempfile::TempDir, Scratch) {
    let parent = tempfile::tempdir().unwrap();
    let s = Scratch::create(&Derived {
        parent: parent.path().to_path_buf(),
        limit,
    })
    .unwrap();
    (parent, s)
}

fn write_file(s: &Scratch, path: &str, bytes: &[u8]) -> io::Result<()> {
    let file = s.open(path, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o644)?;
    s.write(&file, 0, bytes)
}

fn errno<T>(result: io::Result<T>) -> Option<i32> {
    result.err().and_then(|e| e.raw_os_error())
}

fn cloexec(fd: std::os::fd::RawFd) -> bool {
    // SAFETY: `fd` is an open descriptor this test holds.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    flags != -1 && flags & libc::FD_CLOEXEC != 0
}

#[test]
fn the_default_limit_is_sixteen_gib() {
    assert_eq!(DEFAULT_LIMIT, 17_179_869_184);
    assert_eq!(Derived::in_temp_dir().limit, 17_179_869_184);
}

#[test]
fn every_host_errno_maps_to_its_linux_value() {
    for (host, linux) in [
        (libc::ENOENT, Errno::ENOENT),
        (libc::EEXIST, Errno::EEXIST),
        (libc::ENOTEMPTY, Errno::ENOTEMPTY),
        (libc::ENOTDIR, Errno::ENOTDIR),
        (libc::EISDIR, Errno::EISDIR),
        (libc::EACCES, Errno::EACCES),
        (libc::EPERM, Errno::EPERM),
        (libc::EINVAL, Errno::EINVAL),
        (libc::EXDEV, Errno::EXDEV),
        (libc::ELOOP, Errno::ELOOP),
        (libc::ENAMETOOLONG, Errno::ENAMETOOLONG),
        (libc::ENOSPC, Errno::ENOSPC),
        (libc::EDQUOT, Errno::ENOSPC),
        (libc::EBUSY, Errno::EIO),
    ] {
        assert_eq!(
            errno_of(&io::Error::from_raw_os_error(host)),
            linux,
            "host {host}"
        );
    }
    assert_eq!(errno_of(&io::Error::other("no errno")), Errno::EIO);
}

#[test]
fn a_path_component_is_one_plain_name() {
    for bad in ["", ".", "..", "a/b"] {
        assert_eq!(errno(cname(bad)), Some(libc::EINVAL), "{bad:?}");
    }
    assert!(cname("a.b").is_ok());
    assert!(cname("..x").is_ok());
}

#[test]
fn a_scratch_directory_skips_a_name_that_exists_and_fails_under_a_missing_parent() {
    let parent = tempfile::tempdir().unwrap();
    // Every name the next scratch could take, already taken but one far on:
    // the collision must move on, not fail.
    let next = NEXT.load(Ordering::Relaxed);
    let taken: Vec<_> = (next..next + 64)
        .map(|n| {
            parent
                .path()
                .join(format!("leyline-scratch-{}-{n}", std::process::id()))
        })
        .collect();
    for dir in &taken {
        std::fs::create_dir(dir).unwrap();
    }
    let s = Scratch::create(&Derived {
        parent: parent.path().to_path_buf(),
        limit: 1,
    })
    .unwrap();
    assert!(!taken.contains(&s.dir().to_path_buf()));
    assert!(s.dir().is_dir());

    let missing = parent.path().join("no/such/parent");
    let made = Scratch::create(&Derived {
        parent: missing,
        limit: 1,
    });
    assert_eq!(errno(made), Some(libc::ENOENT));
}

#[test]
fn the_root_and_every_opened_file_are_close_on_exec() {
    let (_parent, s) = scratch(1 << 20);
    assert!(cloexec(s.root.as_raw_fd()));
    write_file(&s, "f", b"x").unwrap();
    let file = s.open("f", libc::O_RDONLY, 0).unwrap();
    assert!(cloexec(file.as_raw_fd()));
}

#[test]
fn a_symlink_is_never_followed_at_the_last_component_or_on_the_way() {
    let (_parent, s) = scratch(1 << 20);
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "outside").unwrap();
    let target = outside.path().join("secret");
    s.symlink(target.as_os_str().as_encoded_bytes(), "link")
        .unwrap();
    s.symlink(outside.path().as_os_str().as_encoded_bytes(), "dirlink")
        .unwrap();
    assert!(s.open("link", libc::O_RDONLY, 0).is_err());
    assert!(s.open("dirlink/secret", libc::O_RDONLY, 0).is_err());
    assert!(
        s.open("dirlink/new", libc::O_RDWR | libc::O_CREAT, 0o644)
            .is_err()
    );
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 1);
    assert_eq!(s.stat("link").unwrap().unwrap().kind, Kind::Symlink);
    assert_eq!(
        s.readlink("link").unwrap(),
        target.as_os_str().as_encoded_bytes()
    );
}

#[test]
fn readlink_of_a_file_is_an_error() {
    let (_parent, s) = scratch(1 << 20);
    write_file(&s, "f", b"x").unwrap();
    assert_eq!(errno(s.readlink("f")), Some(libc::EINVAL));
}

#[test]
fn a_read_makes_no_directory_and_only_a_create_makes_missing_parents() {
    let (_parent, s) = scratch(1 << 20);
    assert_eq!(s.stat("a/b/c").unwrap(), None);
    assert!(!s.dir().join("a").exists());
    assert_eq!(errno(s.open("p/q", libc::O_RDONLY, 0)), Some(libc::ENOENT));
    assert!(!s.dir().join("p").exists());
    assert_eq!(s.list("a/b").unwrap(), Vec::new());
    assert!(!s.dir().join("a").exists());
    write_file(&s, "p/q/r", b"r").unwrap();
    assert!(s.dir().join("p/q/r").is_file());
}

#[test]
fn a_missing_path_is_nothing_and_an_inaccessible_one_is_an_error() {
    let (_parent, s) = scratch(1 << 20);
    assert_eq!(s.list("nope").unwrap(), Vec::new());
    assert_eq!(s.list("nope/x").unwrap(), Vec::new());
    s.mkdir("locked/sub").unwrap();
    write_file(&s, "locked/f", b"f").unwrap();
    // Readable, not searchable: names list, nothing under them resolves.
    let locked = s.dir().join("locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o600)).unwrap();
    let stat_child = s.stat("locked/f");
    let stat_deeper = s.stat("locked/sub/x");
    let list_sub = s.list("locked/sub");
    let list_deeper = s.list("locked/sub/x");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(errno(stat_child), Some(libc::EACCES));
    assert_eq!(errno(stat_deeper), Some(libc::EACCES));
    assert_eq!(errno(list_sub), Some(libc::EACCES));
    assert_eq!(errno(list_deeper), Some(libc::EACCES));
}

#[test]
fn permissions_and_kinds_are_the_files_own() {
    let (_parent, s) = scratch(1 << 20);
    write_file(&s, "f", b"abc").unwrap();
    s.chmod("f", 0o640).unwrap();
    let stat = s.stat("f").unwrap().unwrap();
    assert_eq!((stat.kind, stat.perm, stat.size), (Kind::File, 0o640, 3));
    s.mkdir("d").unwrap();
    s.chmod("d", 0o750).unwrap();
    let stat = s.stat("d").unwrap().unwrap();
    assert_eq!(
        (stat.kind, stat.perm, stat.size),
        (Kind::Directory, 0o750, 0)
    );
    let mut listed = s.list("").unwrap();
    listed.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        listed,
        vec![
            ("d".to_string(), Kind::Directory),
            ("f".to_string(), Kind::File)
        ]
    );
}

#[test]
fn a_rename_over_a_file_frees_the_replaced_bytes() {
    let (_parent, s) = scratch(8);
    write_file(&s, "x", b"12345").unwrap();
    write_file(&s, "y", b"678").unwrap();
    // The refused write leaves `z` created and empty.
    assert_eq!(errno(write_file(&s, "z", b"9")), Some(libc::ENOSPC));
    s.rename("y", "x").unwrap();
    write_file(&s, "w", b"abcde").unwrap();
}

#[test]
fn unlink_and_truncate_free_bytes_and_growth_is_bounded() {
    let (_parent, s) = scratch(8);
    let file = s
        .open("a", libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o644)
        .unwrap();
    s.write(&file, 0, b"12345678").unwrap();
    assert_eq!(errno(s.set_len(&file, 9)), Some(libc::ENOSPC));
    s.set_len(&file, 2).unwrap();
    s.write(&file, 2, b"345678").unwrap();
    drop(file);
    s.unlink("a").unwrap();
    write_file(&s, "b", b"12345678").unwrap();
}
