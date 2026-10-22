//! [`StoreFs`] behind the host's FUSE client (ADR-0045 T8): the Linux host
//! mount, and the macOS one where macFUSE or fuse-t is installed.
//!
//! Each request is the [`StoreFs`] call of the same name, as the libkrun
//! share answers a guest's (`crate::share`), with its flags, attributes and
//! errno translated by [`super::fuse_abi`]. This file is only that glue:
//! fuser's requests and replies come from a `/dev/fuse` session and cannot be
//! built in a unit test, so it is exercised through the kernel
//! (`tests/host_mount.rs`, `tools/cli_smoke.sh --mount`) and every decision it
//! makes lives in `fuse_abi.rs`, which is.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use fuser::consts::{FOPEN_DIRECT_IO, FOPEN_KEEP_CACHE};
use fuser::{
    Filesystem, KernelConfig, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
};

use super::fuse_abi::{
    create_mode, file_attr, file_type, host_errno, linux_open_flags, linux_rename_flags,
    owner_change, process_owner,
};
use crate::share::fopen_bits;
use crate::{Attr, Entry, Errno, SetAttr, Store, StoreFs};

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

fn reply_entry(reply: ReplyEntry, entry: Result<Entry, Errno>) {
    match entry {
        Ok(entry) => reply.entry(&entry.ttl, &file_attr(&entry.attr, process_owner()), 0),
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
        Ok((attr, ttl)) => reply.attr(&ttl, &file_attr(&attr, process_owner())),
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
        let (uid, gid) = owner_change(uid, gid, process_owner());
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
                    let kind = file_type(dirent.kind);
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
    /// FLUSH, which `close` sends; a mode that grants execute is staged with
    /// it.
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
                if let Some(mode) = create_mode(mode) {
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
                &file_attr(&entry.attr, process_owner()),
                0,
                handle,
                FOPEN_DIRECT_IO,
            ),
            Err(errno) => reply.error(host_errno(errno)),
        }
    }
}
