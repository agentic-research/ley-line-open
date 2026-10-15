//! The adapter (ADR-0044 phase C, bead `ley-line-open-f210c2`): [`StoreFs`]
//! answering a virtio-fs share's FUSE requests.
//!
//! The guest's FUSE requests reach the VMM's virtio-fs device, which runs in
//! the `leyline-krun-worker` process; the device forwards each one to the
//! daemon that started the run (`leyline_runtime::backends::libkrun::share`),
//! where this adapter answers it from the daemon's store, on the daemon's one
//! writer. Every FUSE-level decision is made here, per the README's
//! "Adapter" mapping; the device only transcribes the reply into libkrun's
//! types.

use std::time::Duration;

use leyline_runtime::backends::libkrun::share::{
    Attr as WireAttr, DT_DIR, DT_REG, DirEntry as WireDirEntry, Entry as WireEntry,
    FOPEN_CACHE_DIR, FOPEN_DIRECT_IO, FOPEN_KEEP_CACHE, FUSE_DO_READDIRPLUS, FsService, Reply,
    Request,
};

use crate::{Attr, DirEntry, Entry, Errno, Kind, SetAttr, Store, StoreFs};

/// `st_blksize`: the I/O size the guest is told to prefer.
const BLKSIZE: u32 = 4096;

/// A [`StoreFs`] as the [`FsService`] behind a guest's virtio-fs share.
pub struct StoreShare<S> {
    fs: StoreFs<S>,
}

impl<S: Store> StoreShare<S> {
    pub fn new(fs: StoreFs<S>) -> Self {
        Self { fs }
    }

    /// The filesystem this share serves.
    pub fn fs(&self) -> &StoreFs<S> {
        &self.fs
    }

    /// LOOKUP. A name the guest cannot see is a negative entry (inode 0)
    /// the guest caches for [`StoreFs::negative_ttl`]; with a zero negative
    /// timeout it stays `ENOENT`, which the guest does not cache.
    fn lookup(&self, parent: u64, name: &[u8]) -> Result<Reply, Errno> {
        match self.fs.lookup(parent, name) {
            Ok(entry) => Ok(entry_reply(&entry)),
            Err(Errno::ENOENT) => {
                let ttl = self.fs.negative_ttl(parent, name);
                if ttl.is_zero() {
                    return Err(Errno::ENOENT);
                }
                Ok(Reply::Entry {
                    entry: WireEntry {
                        inode: 0,
                        attr: WireAttr::default(),
                        entry_ttl: ttl,
                        attr_ttl: Duration::ZERO,
                    },
                })
            }
            Err(errno) => Err(errno),
        }
    }

    fn answer(&self, request: Request) -> Result<Reply, Errno> {
        Ok(match request {
            // The writeback cache stays off: every WRITE reaches the handle
            // before FLUSH, so the base check sees it.
            Request::Init => Reply::Init {
                want: FUSE_DO_READDIRPLUS,
            },
            Request::Lookup { parent, name } => return self.lookup(parent, &name),
            Request::Getattr { inode, handle } => {
                let (attr, ttl) = self.fs.getattr(inode, handle)?;
                Reply::Attr {
                    attr: wire_attr(&attr),
                    ttl,
                }
            }
            Request::Setattr {
                inode,
                handle,
                size,
                mode,
                uid,
                gid,
            } => {
                let (attr, ttl) = self.fs.setattr(
                    inode,
                    handle,
                    SetAttr {
                        size,
                        mode,
                        uid,
                        gid,
                    },
                )?;
                Reply::Attr {
                    attr: wire_attr(&attr),
                    ttl,
                }
            }
            Request::Mkdir { parent, name } => entry_reply(&self.fs.mkdir(parent, &name)?),
            Request::Unlink { parent, name } => {
                self.fs.unlink(parent, &name)?;
                Reply::Done
            }
            Request::Rmdir { parent, name } => {
                self.fs.rmdir(parent, &name)?;
                Reply::Done
            }
            Request::Rename {
                olddir,
                oldname,
                newdir,
                newname,
                flags,
            } => {
                self.fs.rename(olddir, &oldname, newdir, &newname, flags)?;
                Reply::Done
            }
            Request::Open { inode, flags } => {
                let opened = self.fs.open(inode, flags)?;
                Reply::Opened {
                    handle: opened.handle,
                    flags: fopen_bits(&[
                        (opened.keep_cache, FOPEN_KEEP_CACHE),
                        (opened.direct_io, FOPEN_DIRECT_IO),
                    ]),
                }
            }
            // A created file's handle is a write handle: direct I/O, never
            // the page cache.
            Request::Create {
                parent,
                name,
                flags,
            } => {
                let (entry, handle) = self.fs.create(parent, &name, flags)?;
                Reply::Created {
                    entry: wire_entry(&entry),
                    handle,
                    flags: FOPEN_DIRECT_IO,
                }
            }
            Request::Read {
                handle,
                offset,
                size,
            } => Reply::Data {
                data: self.fs.read(handle, offset, size)?,
            },
            Request::Write {
                handle,
                offset,
                data,
            } => Reply::Written {
                size: self.fs.write(handle, offset, &data)? as u32,
            },
            Request::Flush { handle } => {
                self.fs.flush(handle)?;
                Reply::Done
            }
            Request::Fsync { handle } => {
                self.fs.fsync(handle)?;
                Reply::Done
            }
            Request::Release { handle } => {
                self.fs.release(handle);
                Reply::Done
            }
            Request::Opendir { inode } => {
                let opened = self.fs.opendir(inode)?;
                Reply::Opened {
                    handle: 0,
                    flags: fopen_bits(&[
                        (true, FOPEN_CACHE_DIR),
                        (opened.keep_cache, FOPEN_KEEP_CACHE),
                    ]),
                }
            }
            Request::Readdir { inode, offset } => Reply::Dir {
                entries: self
                    .fs
                    .readdir(inode, offset)?
                    .iter()
                    .map(wire_dirent)
                    .collect(),
            },
            Request::Readdirplus { inode, offset } => Reply::DirPlus {
                entries: self
                    .fs
                    .readdirplus(inode, offset)?
                    .iter()
                    .map(|(dirent, entry)| (wire_dirent(dirent), wire_entry(entry)))
                    .collect(),
            },
        })
    }
}

impl<S: Store> FsService for StoreShare<S> {
    /// Answers `request`; an [`Errno`] is the reply's errno, untranslated:
    /// the values are already Linux.
    fn call(&self, request: Request) -> Reply {
        self.answer(request)
            .unwrap_or_else(|Errno(errno)| Reply::Error { errno })
    }
}

/// The `FOPEN_*` bits whose condition holds; each bit is distinct.
pub(crate) fn fopen_bits(bits: &[(bool, u32)]) -> u32 {
    bits.iter()
        .filter(|(set, _)| *set)
        .map(|(_, bit)| bit)
        .sum()
}

fn wire_attr(attr: &Attr) -> WireAttr {
    WireAttr {
        inode: attr.inode,
        mode: attr.mode(),
        nlink: attr.nlink(),
        size: attr.size,
        blocks: attr.size.div_ceil(512),
        blksize: BLKSIZE,
        atime: attr.mtime,
        mtime: attr.mtime,
        ctime: attr.mtime,
    }
}

/// The entry and attribute timeouts are both the entry's [`Entry::ttl`].
fn wire_entry(entry: &Entry) -> WireEntry {
    WireEntry {
        inode: entry.attr.inode,
        attr: wire_attr(&entry.attr),
        entry_ttl: entry.ttl,
        attr_ttl: entry.ttl,
    }
}

fn entry_reply(entry: &Entry) -> Reply {
    Reply::Entry {
        entry: wire_entry(entry),
    }
}

fn wire_dirent(dirent: &DirEntry) -> WireDirEntry {
    WireDirEntry {
        inode: dirent.inode,
        offset: dirent.offset,
        kind: match dirent.kind {
            Kind::File => DT_REG,
            Kind::Directory => DT_DIR,
        },
        name: dirent.name.as_bytes().to_vec(),
    }
}
