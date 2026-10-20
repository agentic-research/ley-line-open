//! What the host FUSE adapter ([`super::fuse`]) translates between the
//! host's kernel and [`StoreFs`](crate::StoreFs): errno values, `open(2)`
//! and `rename(2)` flags, attributes, and the owner and mode a request
//! names. Every decision the adapter makes lives here, as a pure function;
//! `fuse.rs` only hands each request to [`StoreFs`](crate::StoreFs) and its
//! answer to the kernel's reply.

use std::time::UNIX_EPOCH;

use fuser::{FileAttr, FileType};

use crate::{Attr, Errno, Kind, open_flags, rename_flags};

/// `st_blksize`: the I/O size the host is told to prefer.
pub const BLKSIZE: u32 = 4096;

/// The host's errno for a Linux one. `ENOSYS`, `ENOTEMPTY` and `ESTALE` are
/// the values [`StoreFs`](crate::StoreFs) answers that differ between Linux
/// and macOS.
pub fn host_errno(errno: Errno) -> i32 {
    match errno {
        Errno::ENOSYS => libc::ENOSYS,
        Errno::ENOTEMPTY => libc::ENOTEMPTY,
        Errno::ESTALE => libc::ESTALE,
        Errno(other) => other,
    }
}

/// The Linux `open(2)` flags [`StoreFs::open`](crate::StoreFs::open) reads,
/// from the host's: the access mode (the same bits on Linux and macOS) and
/// `O_TRUNC` (which is not).
pub fn linux_open_flags(host: i32) -> u32 {
    let access = (host & libc::O_ACCMODE) as u32;
    if host & libc::O_TRUNC != 0 {
        access | open_flags::O_TRUNC
    } else {
        access
    }
}

/// A bit `renameat2(2)` does not define: [`linux_rename_flags`] sets it for a
/// host flag with no Linux spelling, so
/// [`StoreFs::rename`](crate::StoreFs::rename) refuses the rename as asking
/// for something unknown rather than dropping or misreading the flag.
pub const UNKNOWN_RENAME_FLAG: u32 = 1 << 31;

/// The Linux `renameat2(2)` flags [`StoreFs::rename`](crate::StoreFs::rename)
/// reads, from the host's: macOS's `renamex_np(2)` spells `RENAME_NOREPLACE`
/// as `RENAME_EXCL` and `RENAME_EXCHANGE` as `RENAME_SWAP`. Its other flags
/// (`RENAME_SECLUDE` is `0x1`, Linux's `RENAME_NOREPLACE` bit) have no Linux
/// spelling and become [`UNKNOWN_RENAME_FLAG`].
pub fn linux_rename_flags(host: u32) -> u32 {
    #[cfg(target_os = "macos")]
    {
        const RENAME_SWAP: u32 = 0x2;
        const RENAME_EXCL: u32 = 0x4;
        let mut linux = 0;
        if host & RENAME_EXCL != 0 {
            linux |= rename_flags::RENAME_NOREPLACE;
        }
        if host & RENAME_SWAP != 0 {
            linux |= rename_flags::RENAME_EXCHANGE;
        }
        if host & !(RENAME_SWAP | RENAME_EXCL) != 0 {
            linux |= UNKNOWN_RENAME_FLAG;
        }
        linux
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = rename_flags::RENAME_NOREPLACE;
        host
    }
}

/// A node's FUSE attributes, owned by `owner` (this process's user).
pub fn file_attr(attr: &Attr, owner: (u32, u32)) -> FileAttr {
    let mtime = UNIX_EPOCH + attr.mtime;
    FileAttr {
        ino: attr.inode,
        size: attr.size,
        blocks: attr.size.div_ceil(512),
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: mtime,
        kind: file_type(attr.kind),
        perm: attr.perm as u16,
        nlink: attr.nlink(),
        uid: owner.0,
        gid: owner.1,
        rdev: 0,
        blksize: BLKSIZE,
        flags: 0,
    }
}

/// A node kind's FUSE file type.
pub fn file_type(kind: Kind) -> FileType {
    match kind {
        Kind::File => FileType::RegularFile,
        Kind::Directory => FileType::Directory,
        Kind::Symlink => FileType::Symlink,
    }
}

/// The owner a SETATTR names, unless it is `owner`, the one every node is
/// presented with: setting that one changes nothing.
pub fn owner_change(
    uid: Option<u32>,
    gid: Option<u32>,
    owner: (u32, u32),
) -> (Option<u32>, Option<u32>) {
    (uid.filter(|&u| u != owner.0), gid.filter(|&g| g != owner.1))
}

/// The mode a CREATE stages on its new file: the requested mode when it
/// grants execute, so the file lands executable; `None` lands it regular.
pub fn create_mode(mode: u32) -> Option<u32> {
    (mode & 0o111 != 0).then_some(mode)
}

/// This process's user and group: the owner every node is presented with.
pub fn process_owner() -> (u32, u32) {
    // SAFETY: `getuid(2)` and `getgid(2)` cannot fail and touch no memory.
    unsafe { (libc::getuid(), libc::getgid()) }
}
