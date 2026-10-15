//! The worker's half of a share: libkrun's `DynFileSystem` over an
//! [`FsService`], transcribing each FUSE request into a [`Request`] and each
//! [`Reply`] into libkrun's types.

use std::ffi::CStr;
use std::io;
use std::time::Duration;

use super::{Attr, DirEntry, Entry, FsService, LINUX_EIO, Reply, Request};
use crate::backends::libkrun::api::DynFileSystem;
use crate::backends::libkrun::api::virtio_fs::filesystem::{
    Context, DirEntry as KrunDirEntry, Entry as KrunEntry, Extensions, FsOptions, OpenOptions,
    SetattrValid,
};

/// The `stat` libkrun's server reads attributes from.
#[cfg(target_os = "macos")]
pub(super) type Stat = libc::stat;
#[cfg(target_os = "linux")]
pub(super) type Stat = libc::stat64;

/// A virtio-fs device whose every request `service` answers.
pub struct ShareFs<F> {
    service: F,
}

impl<F: FsService> ShareFs<F> {
    pub fn new(service: F) -> Self {
        Self { service }
    }

    pub fn service(&self) -> &F {
        &self.service
    }

    fn call(&self, request: Request) -> io::Result<Reply> {
        match self.service.call(request) {
            Reply::Error { errno } => Err(io::Error::from_raw_os_error(errno)),
            reply => Ok(reply),
        }
    }

    fn entry(&self, request: Request) -> io::Result<KrunEntry> {
        match self.call(request)? {
            Reply::Entry { entry } => Ok(krun_entry(&entry)),
            _ => Err(unexpected()),
        }
    }

    fn attr(&self, request: Request) -> io::Result<(Stat, Duration)> {
        match self.call(request)? {
            Reply::Attr { attr, ttl } => Ok((stat(&attr), ttl)),
            _ => Err(unexpected()),
        }
    }

    fn opened(&self, request: Request) -> io::Result<(Option<u64>, OpenOptions)> {
        match self.call(request)? {
            Reply::Opened { handle, flags } => {
                Ok((Some(handle), OpenOptions::from_bits_truncate(flags)))
            }
            _ => Err(unexpected()),
        }
    }

    fn done(&self, request: Request) -> io::Result<()> {
        match self.call(request)? {
            Reply::Done => Ok(()),
            _ => Err(unexpected()),
        }
    }
}

/// A reply that does not answer the request: the service and the device
/// disagree about the protocol, which the guest sees as `EIO`.
fn unexpected() -> io::Error {
    io::Error::from_raw_os_error(LINUX_EIO)
}

fn stat(attr: &Attr) -> Stat {
    // SAFETY: stat is plain old data; all-zero is a valid value.
    let mut st: Stat = unsafe { std::mem::zeroed() };
    st.st_ino = attr.inode as _;
    st.st_mode = attr.mode as _;
    st.st_nlink = attr.nlink as _;
    st.st_size = attr.size as _;
    st.st_blocks = attr.blocks as _;
    st.st_blksize = attr.blksize as _;
    st.st_atime = attr.atime.as_secs() as _;
    st.st_atime_nsec = attr.atime.subsec_nanos() as _;
    st.st_mtime = attr.mtime.as_secs() as _;
    st.st_mtime_nsec = attr.mtime.subsec_nanos() as _;
    st.st_ctime = attr.ctime.as_secs() as _;
    st.st_ctime_nsec = attr.ctime.subsec_nanos() as _;
    st
}

fn krun_entry(entry: &Entry) -> KrunEntry {
    KrunEntry {
        inode: entry.inode,
        generation: 0,
        attr: stat(&entry.attr),
        attr_flags: 0,
        attr_timeout: entry.attr_ttl,
        entry_timeout: entry.entry_ttl,
    }
}

fn krun_dirent(entry: &DirEntry) -> KrunDirEntry<'_> {
    KrunDirEntry {
        ino: entry.inode,
        offset: entry.offset,
        type_: entry.kind,
        name: &entry.name,
    }
}

fn bytes(name: &CStr) -> Vec<u8> {
    name.to_bytes().to_vec()
}

/// FORGET, RELEASEDIR, ACCESS and STATFS answer here: the service keeps no
/// per-lookup or per-directory-handle state, and permission is its decision
/// at each request. DAX stays `ENOSYS` (ADR-0044 V3: a writable mapping
/// would bypass WRITE).
#[allow(clippy::too_many_arguments)]
impl<F: FsService> DynFileSystem for ShareFs<F> {
    fn init(&self, _capable: FsOptions) -> io::Result<FsOptions> {
        match self.call(Request::Init)? {
            Reply::Init { want } => Ok(FsOptions::from_bits_truncate(want)),
            _ => Err(unexpected()),
        }
    }

    fn lookup(&self, _ctx: Context, parent: u64, name: &CStr) -> io::Result<KrunEntry> {
        self.entry(Request::Lookup {
            parent,
            name: bytes(name),
        })
    }

    fn getattr(
        &self,
        _ctx: Context,
        inode: u64,
        handle: Option<u64>,
    ) -> io::Result<(Stat, Duration)> {
        self.attr(Request::Getattr { inode, handle })
    }

    // `st_mode` is u16 on macOS and u32 on Linux: the cast widens on one and
    // is the identity on the other, which `u32::from` would make a
    // useless-conversion lint on Linux instead.
    #[allow(clippy::unnecessary_cast)]
    fn setattr(
        &self,
        _ctx: Context,
        inode: u64,
        attr: Stat,
        handle: Option<u64>,
        valid: SetattrValid,
    ) -> io::Result<(Stat, Duration)> {
        self.attr(Request::Setattr {
            inode,
            handle,
            size: valid
                .contains(SetattrValid::SIZE)
                .then_some(attr.st_size as u64),
            mode: valid
                .contains(SetattrValid::MODE)
                .then_some(attr.st_mode as u32),
            uid: valid.contains(SetattrValid::UID).then_some(attr.st_uid),
            gid: valid.contains(SetattrValid::GID).then_some(attr.st_gid),
        })
    }

    fn mkdir(
        &self,
        _ctx: Context,
        parent: u64,
        name: &CStr,
        _mode: u32,
        _umask: u32,
        _extensions: Extensions,
    ) -> io::Result<KrunEntry> {
        self.entry(Request::Mkdir {
            parent,
            name: bytes(name),
        })
    }

    fn unlink(&self, _ctx: Context, parent: u64, name: &CStr) -> io::Result<()> {
        self.done(Request::Unlink {
            parent,
            name: bytes(name),
        })
    }

    fn rmdir(&self, _ctx: Context, parent: u64, name: &CStr) -> io::Result<()> {
        self.done(Request::Rmdir {
            parent,
            name: bytes(name),
        })
    }

    fn rename(
        &self,
        _ctx: Context,
        olddir: u64,
        oldname: &CStr,
        newdir: u64,
        newname: &CStr,
        flags: u32,
    ) -> io::Result<()> {
        self.done(Request::Rename {
            olddir,
            oldname: bytes(oldname),
            newdir,
            newname: bytes(newname),
            flags,
        })
    }

    fn open(
        &self,
        _ctx: Context,
        inode: u64,
        _kill_priv: bool,
        flags: u32,
    ) -> io::Result<(Option<u64>, OpenOptions)> {
        self.opened(Request::Open { inode, flags })
    }

    fn create(
        &self,
        _ctx: Context,
        parent: u64,
        name: &CStr,
        _mode: u32,
        _kill_priv: bool,
        flags: u32,
        _umask: u32,
        _extensions: Extensions,
    ) -> io::Result<(KrunEntry, Option<u64>, OpenOptions)> {
        match self.call(Request::Create {
            parent,
            name: bytes(name),
            flags,
        })? {
            Reply::Created {
                entry,
                handle,
                flags,
            } => Ok((
                krun_entry(&entry),
                Some(handle),
                OpenOptions::from_bits_truncate(flags),
            )),
            _ => Err(unexpected()),
        }
    }

    fn read(
        &self,
        _ctx: Context,
        _inode: u64,
        handle: u64,
        w: &mut dyn io::Write,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> io::Result<usize> {
        match self.call(Request::Read {
            handle,
            offset,
            size,
        })? {
            Reply::Data { data } => {
                w.write_all(&data)?;
                Ok(data.len())
            }
            _ => Err(unexpected()),
        }
    }

    fn write(
        &self,
        _ctx: Context,
        _inode: u64,
        handle: u64,
        r: &mut dyn io::Read,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _delayed_write: bool,
        _kill_priv: bool,
        _flags: u32,
    ) -> io::Result<usize> {
        let mut data = vec![0; size as usize];
        r.read_exact(&mut data)?;
        match self.call(Request::Write {
            handle,
            offset,
            data,
        })? {
            Reply::Written { size } => Ok(size as usize),
            _ => Err(unexpected()),
        }
    }

    fn flush(&self, _ctx: Context, _inode: u64, handle: u64, _lock_owner: u64) -> io::Result<()> {
        self.done(Request::Flush { handle })
    }

    fn fsync(&self, _ctx: Context, _inode: u64, _datasync: bool, handle: u64) -> io::Result<()> {
        self.done(Request::Fsync { handle })
    }

    fn release(
        &self,
        _ctx: Context,
        _inode: u64,
        _flags: u32,
        handle: u64,
        _flush: bool,
        _flock_release: bool,
        _lock_owner: Option<u64>,
    ) -> io::Result<()> {
        self.done(Request::Release { handle })
    }

    fn opendir(
        &self,
        _ctx: Context,
        inode: u64,
        _flags: u32,
    ) -> io::Result<(Option<u64>, OpenOptions)> {
        self.opened(Request::Opendir { inode })
    }

    fn readdir(
        &self,
        _ctx: Context,
        inode: u64,
        _handle: u64,
        _size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(KrunDirEntry) -> io::Result<usize>,
    ) -> io::Result<()> {
        let Reply::Dir { entries } = self.call(Request::Readdir { inode, offset })? else {
            return Err(unexpected());
        };
        for entry in &entries {
            if add_entry(krun_dirent(entry))? == 0 {
                break;
            }
        }
        Ok(())
    }

    fn readdirplus(
        &self,
        _ctx: Context,
        inode: u64,
        _handle: u64,
        _size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(KrunDirEntry, KrunEntry) -> io::Result<usize>,
    ) -> io::Result<()> {
        let Reply::DirPlus { entries } = self.call(Request::Readdirplus { inode, offset })? else {
            return Err(unexpected());
        };
        for (entry, lookup) in &entries {
            if add_entry(krun_dirent(entry), krun_entry(lookup))? == 0 {
                break;
            }
        }
        Ok(())
    }

    fn releasedir(&self, _ctx: Context, _inode: u64, _flags: u32, _handle: u64) -> io::Result<()> {
        Ok(())
    }

    fn access(&self, _ctx: Context, _inode: u64, _mask: u32) -> io::Result<()> {
        Ok(())
    }
}
