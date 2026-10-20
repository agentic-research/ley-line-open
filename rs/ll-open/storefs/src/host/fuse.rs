//! [`StoreFs`] behind the host's FUSE client (ADR-0045 T8): the Linux host
//! mount, and the macOS one where macFUSE or fuse-t is installed.
//!
//! Each request is the [`StoreFs`] call of the same name, as the libkrun
//! share answers a guest's (`crate::share`); this adapter translates the
//! host's `open(2)` and `rename(2)` flags into the Linux values [`StoreFs`]
//! reads and the Linux errno values it answers into the host's.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use fuser::consts::{FOPEN_DIRECT_IO, FOPEN_KEEP_CACHE};
use fuser::{
    FileAttr, FileType, Filesystem, KernelConfig, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
};

use crate::share::fopen_bits;
use crate::{Attr, Entry, Errno, Kind, SetAttr, Store, StoreFs, open_flags, rename_flags};

/// `st_blksize`: the I/O size the host is told to prefer.
const BLKSIZE: u32 = 4096;

/// [`StoreFs`] as a [`fuser::Filesystem`]. A FUSE inode is the [`StoreFs`]
/// inode; the root is inode 1 in both.
pub struct HostFuse<S> {
    fs: Arc<StoreFs<S>>,
}

impl<S: Store> HostFuse<S> {
    pub fn new(fs: Arc<StoreFs<S>>) -> Self {
        Self { fs }
    }
}

/// The host's errno for a Linux one. `ENOSYS`, `ENOTEMPTY` and `ESTALE` are
/// the values [`StoreFs`] answers that differ between Linux and macOS.
pub fn host_errno(errno: Errno) -> i32 {
    match errno {
        Errno::ENOSYS => libc::ENOSYS,
        Errno::ENOTEMPTY => libc::ENOTEMPTY,
        Errno::ESTALE => libc::ESTALE,
        Errno(other) => other,
    }
}

/// The Linux `open(2)` flags [`StoreFs::open`] reads, from the host's: the
/// access mode (the same bits on Linux and macOS) and `O_TRUNC` (which is
/// not).
pub fn linux_open_flags(host: i32) -> u32 {
    let access = (host & libc::O_ACCMODE) as u32;
    if host & libc::O_TRUNC != 0 {
        access | open_flags::O_TRUNC
    } else {
        access
    }
}

/// The Linux `renameat2(2)` flags [`StoreFs::rename`] reads, from the
/// host's: macOS's `renamex_np(2)` spells `RENAME_NOREPLACE` as
/// `RENAME_EXCL` and `RENAME_EXCHANGE` as `RENAME_SWAP`.
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
        // Any other bit is one StoreFs does not know: pass it on, so it is
        // refused as unknown rather than dropped.
        linux | (host & !(RENAME_SWAP | RENAME_EXCL))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = rename_flags::RENAME_NOREPLACE;
        host
    }
}

/// A node's FUSE attributes, owned by this process's user.
pub fn file_attr(attr: &Attr) -> FileAttr {
    let mtime = UNIX_EPOCH + attr.mtime;
    // SAFETY: `getuid(2)` and `getgid(2)` cannot fail and touch no memory.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    FileAttr {
        ino: attr.inode,
        size: attr.size,
        blocks: attr.size.div_ceil(512),
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: mtime,
        kind: match attr.kind {
            Kind::File => FileType::RegularFile,
            Kind::Directory => FileType::Directory,
            Kind::Symlink => FileType::Symlink,
        },
        perm: attr.perm as u16,
        nlink: attr.nlink(),
        uid,
        gid,
        rdev: 0,
        blksize: BLKSIZE,
        flags: 0,
    }
}

/// The owner a SETATTR names, unless it is the one every node is presented
/// with: setting that one changes nothing.
fn owner_change(uid: Option<u32>, gid: Option<u32>) -> (Option<u32>, Option<u32>) {
    // SAFETY: as in `file_attr`.
    let (own_uid, own_gid) = unsafe { (libc::getuid(), libc::getgid()) };
    (uid.filter(|&u| u != own_uid), gid.filter(|&g| g != own_gid))
}

fn reply_entry(reply: ReplyEntry, entry: Result<Entry, Errno>) {
    match entry {
        Ok(entry) => reply.entry(&entry.ttl, &file_attr(&entry.attr), 0),
        Err(errno) => reply.error(host_errno(errno)),
    }
}

fn reply_empty(reply: ReplyEmpty, done: Result<(), Errno>) {
    match done {
        Ok(()) => reply.ok(),
        Err(errno) => reply.error(host_errno(errno)),
    }
}

fn reply_attr(reply: ReplyAttr, attr: Result<(Attr, Duration), Errno>) {
    match attr {
        Ok((attr, ttl)) => reply.attr(&ttl, &file_attr(&attr)),
        Err(errno) => reply.error(host_errno(errno)),
    }
}

impl<S: Store + 'static> Filesystem for HostFuse<S> {
    /// Ask for `FUSE_ATOMIC_O_TRUNC`, so `open(O_TRUNC)` arrives as one OPEN
    /// carrying `O_TRUNC` and its truncate is staged on that open's handle.
    /// The writeback cache stays off: every WRITE reaches the handle before
    /// FLUSH.
    fn init(&mut self, _req: &Request<'_>, config: &mut KernelConfig) -> Result<(), libc::c_int> {
        if let Err(missing) = config.add_capabilities(fuser::consts::FUSE_ATOMIC_O_TRUNC) {
            log::info!("kernel lacks FUSE capabilities {missing:#x}: O_TRUNC arrives as SETATTR");
        }
        Ok(())
    }

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        reply_entry(reply, self.fs.lookup(parent, name.as_bytes()));
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, fh: Option<u64>, reply: ReplyAttr) {
        reply_attr(reply, self.fs.getattr(ino, fh));
    }

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        fh: Option<u64>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let (uid, gid) = owner_change(uid, gid);
        let set = SetAttr {
            size,
            mode,
            uid,
            gid,
        };
        reply_attr(reply, self.fs.setattr(ino, fh, set));
    }

    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        match self.fs.readlink(ino) {
            Ok(target) => reply.data(&target),
            Err(errno) => reply.error(host_errno(errno)),
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        reply_entry(reply, self.fs.mkdir(parent, name.as_bytes()));
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        reply_empty(reply, self.fs.unlink(parent, name.as_bytes()));
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        reply_empty(reply, self.fs.rmdir(parent, name.as_bytes()));
    }

    fn symlink(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let target = target.as_os_str().as_bytes();
        reply_entry(reply, self.fs.symlink(parent, link_name.as_bytes(), target));
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        let renamed = self.fs.rename(
            parent,
            name.as_bytes(),
            newparent,
            newname.as_bytes(),
            linux_rename_flags(flags),
        );
        reply_empty(reply, renamed);
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        match self.fs.open(ino, linux_open_flags(flags)) {
            Ok(opened) => reply.opened(
                opened.handle,
                fopen_bits(&[
                    (opened.keep_cache, FOPEN_KEEP_CACHE),
                    (opened.direct_io, FOPEN_DIRECT_IO),
                ]),
            ),
            Err(errno) => reply.error(host_errno(errno)),
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        match self.fs.read(fh, offset.max(0) as u64, size) {
            Ok(data) => reply.data(&data),
            Err(errno) => reply.error(host_errno(errno)),
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        match self.fs.write(fh, offset.max(0) as u64, data) {
            Ok(written) => reply.written(written as u32),
            Err(errno) => reply.error(host_errno(errno)),
        }
    }

    /// FLUSH (each `close`) lands the handle's staged bytes; the kernel
    /// returns its refusal to `close`.
    fn flush(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, _lock: u64, reply: ReplyEmpty) {
        reply_empty(reply, self.fs.flush(fh));
    }

    fn fsync(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, _data: bool, reply: ReplyEmpty) {
        reply_empty(reply, self.fs.fsync(fh));
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.fs.release(fh);
        reply.ok();
    }

    fn opendir(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        match self.fs.opendir(ino) {
            Ok(opened) => reply.opened(0, fopen_bits(&[(opened.keep_cache, FOPEN_KEEP_CACHE)])),
            Err(errno) => reply.error(host_errno(errno)),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        match self.fs.readdir(ino, offset.max(0) as u64) {
            Ok(entries) => {
                for dirent in entries {
                    let kind = match dirent.kind {
                        Kind::File => FileType::RegularFile,
                        Kind::Directory => FileType::Directory,
                        Kind::Symlink => FileType::Symlink,
                    };
                    if reply.add(dirent.inode, dirent.offset as i64, kind, &dirent.name) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(errno) => reply.error(host_errno(errno)),
        }
    }

    /// CREATE: the file exists at once and lands at its handle's first
    /// FLUSH, which `close` sends.
    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let created = self
            .fs
            .create(parent, name.as_bytes(), linux_open_flags(flags))
            .and_then(|(entry, handle)| {
                if mode & 0o111 != 0 {
                    let exec = SetAttr {
                        mode: Some(mode),
                        ..SetAttr::default()
                    };
                    if let Err(errno) = self.fs.setattr(entry.attr.inode, Some(handle), exec) {
                        self.fs.release(handle);
                        return Err(errno);
                    }
                }
                Ok((entry, handle))
            });
        match created {
            Ok((entry, handle)) => reply.created(
                &entry.ttl,
                &file_attr(&entry.attr),
                0,
                handle,
                FOPEN_DIRECT_IO,
            ),
            Err(errno) => reply.error(host_errno(errno)),
        }
    }
}
