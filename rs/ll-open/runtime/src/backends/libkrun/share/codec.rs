//! The frame header in `leyline-public-schema`'s `capnp/share.capnp`: one
//! unpacked Cap'n Proto message per header, the payload beside it.
//!
//! Decoding refuses (`InvalidData`) anything but exactly one message that
//! fills the header, a kind this side knows, a time whose nanosecond count is
//! under one second, and a payload only on the kinds that carry one (a
//! WRITE request, a DATA reply).

use std::io;
use std::time::Duration;

use capnp::message::{Builder, ReaderOptions};
use leyline_public_schema::share_capnp as wire;

use super::{Attr, DirEntry, Entry, Reply, Request};

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("share frame header: {error}"),
    )
}

fn header(message: &Builder<capnp::message::HeapAllocator>) -> Vec<u8> {
    let mut out =
        Vec::with_capacity(capnp::serialize::compute_serialized_size_in_words(message) * 8);
    capnp::serialize::write_message(&mut out, message).expect("writing to a Vec cannot fail");
    out
}

fn read<T>(
    bytes: &[u8],
    decode: impl FnOnce(
        &capnp::message::Reader<capnp::serialize::BufferSegments<&[u8]>>,
    ) -> capnp::Result<T>,
) -> io::Result<T> {
    let mut rest = bytes;
    let message = capnp::serialize::read_message_from_flat_slice(&mut rest, ReaderOptions::new())
        .map_err(invalid)?;
    if !rest.is_empty() {
        return Err(invalid(format!(
            "{} bytes follow the header's message",
            rest.len()
        )));
    }
    decode(&message).map_err(invalid)
}

fn no_payload(payload: &[u8], kind: &str) -> io::Result<()> {
    if payload.is_empty() {
        Ok(())
    } else {
        Err(invalid(format!(
            "a {kind} carries no payload; the frame has {} bytes",
            payload.len()
        )))
    }
}

// --- times, optionals, attributes --------------------------------------

fn set_time(mut out: wire::time::Builder<'_>, time: Duration) {
    out.set_secs(time.as_secs());
    out.set_nanos(time.subsec_nanos());
}

fn time(input: wire::time::Reader<'_>) -> capnp::Result<Duration> {
    let nanos = input.get_nanos();
    if nanos >= 1_000_000_000 {
        return Err(capnp::Error::failed(format!(
            "a time's nanosecond count is {nanos}, not under one second"
        )));
    }
    Ok(Duration::new(input.get_secs(), nanos))
}

fn set_u64(mut out: wire::optional_u64::Builder<'_>, value: u64) {
    out.set_set(true);
    out.set_value(value);
}

fn opt_u64(input: wire::optional_u64::Reader<'_>) -> Option<u64> {
    input.get_set().then(|| input.get_value())
}

fn set_u32(mut out: wire::optional_u32::Builder<'_>, value: u32) {
    out.set_set(true);
    out.set_value(value);
}

fn opt_u32(input: wire::optional_u32::Reader<'_>) -> Option<u32> {
    input.get_set().then(|| input.get_value())
}

fn set_attr(mut out: wire::attr::Builder<'_>, attr: &Attr) {
    out.set_inode(attr.inode);
    out.set_mode(attr.mode);
    out.set_nlink(attr.nlink);
    out.set_size(attr.size);
    out.set_blocks(attr.blocks);
    out.set_blksize(attr.blksize);
    set_time(out.reborrow().init_atime(), attr.atime);
    set_time(out.reborrow().init_mtime(), attr.mtime);
    set_time(out.init_ctime(), attr.ctime);
}

fn attr(input: wire::attr::Reader<'_>) -> capnp::Result<Attr> {
    Ok(Attr {
        inode: input.get_inode(),
        mode: input.get_mode(),
        nlink: input.get_nlink(),
        size: input.get_size(),
        blocks: input.get_blocks(),
        blksize: input.get_blksize(),
        atime: time(input.get_atime()?)?,
        mtime: time(input.get_mtime()?)?,
        ctime: time(input.get_ctime()?)?,
    })
}

fn set_entry(mut out: wire::entry::Builder<'_>, entry: &Entry) {
    out.set_inode(entry.inode);
    set_attr(out.reborrow().init_attr(), &entry.attr);
    set_time(out.reborrow().init_entry_ttl(), entry.entry_ttl);
    set_time(out.init_attr_ttl(), entry.attr_ttl);
}

fn entry(input: wire::entry::Reader<'_>) -> capnp::Result<Entry> {
    Ok(Entry {
        inode: input.get_inode(),
        attr: attr(input.get_attr()?)?,
        entry_ttl: time(input.get_entry_ttl()?)?,
        attr_ttl: time(input.get_attr_ttl()?)?,
    })
}

fn set_dir_entry(mut out: wire::dir_entry::Builder<'_>, entry: &DirEntry) {
    out.set_inode(entry.inode);
    out.set_offset(entry.offset);
    out.set_kind(entry.kind);
    out.set_name(&entry.name);
}

fn dir_entry(input: wire::dir_entry::Reader<'_>) -> capnp::Result<DirEntry> {
    Ok(DirEntry {
        inode: input.get_inode(),
        offset: input.get_offset(),
        kind: input.get_kind(),
        name: input.get_name()?.to_vec(),
    })
}

fn set_name(mut out: wire::name::Builder<'_>, parent: u64, name: &[u8]) {
    out.set_parent(parent);
    out.set_name(name);
}

fn name(input: wire::name::Reader<'_>) -> capnp::Result<(u64, Vec<u8>)> {
    Ok((input.get_parent(), input.get_name()?.to_vec()))
}

// --- requests -----------------------------------------------------------

pub(super) fn request_header(request: &Request) -> Vec<u8> {
    let mut message = Builder::new_default();
    let mut root = message.init_root::<wire::request::Builder<'_>>();
    match request {
        Request::Init => root.set_init(()),
        Request::Lookup { parent, name } => set_name(root.init_lookup(), *parent, name),
        Request::Getattr { inode, handle } => {
            let mut out = root.init_getattr();
            out.set_inode(*inode);
            if let Some(handle) = handle {
                set_u64(out.init_handle(), *handle);
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
            let mut out = root.init_setattr();
            out.set_inode(*inode);
            if let Some(handle) = handle {
                set_u64(out.reborrow().init_handle(), *handle);
            }
            if let Some(size) = size {
                set_u64(out.reborrow().init_size(), *size);
            }
            if let Some(mode) = mode {
                set_u32(out.reborrow().init_mode(), *mode);
            }
            if let Some(uid) = uid {
                set_u32(out.reborrow().init_uid(), *uid);
            }
            if let Some(gid) = gid {
                set_u32(out.init_gid(), *gid);
            }
        }
        Request::Mkdir { parent, name } => set_name(root.init_mkdir(), *parent, name),
        Request::Unlink { parent, name } => set_name(root.init_unlink(), *parent, name),
        Request::Rmdir { parent, name } => set_name(root.init_rmdir(), *parent, name),
        Request::Rename {
            olddir,
            oldname,
            newdir,
            newname,
            flags,
        } => {
            let mut out = root.init_rename();
            out.set_olddir(*olddir);
            out.set_oldname(oldname);
            out.set_newdir(*newdir);
            out.set_newname(newname);
            out.set_flags(*flags);
        }
        Request::Open { inode, flags } => {
            let mut out = root.init_open();
            out.set_inode(*inode);
            out.set_flags(*flags);
        }
        Request::Create {
            parent,
            name,
            flags,
        } => {
            let mut out = root.init_create();
            out.set_parent(*parent);
            out.set_name(name);
            out.set_flags(*flags);
        }
        Request::Read {
            handle,
            offset,
            size,
        } => {
            let mut out = root.init_read();
            out.set_handle(*handle);
            out.set_offset(*offset);
            out.set_size(*size);
        }
        Request::Write { handle, offset, .. } => {
            let mut out = root.init_write();
            out.set_handle(*handle);
            out.set_offset(*offset);
        }
        Request::Flush { handle } => root.init_flush().set_handle(*handle),
        Request::Fsync { handle } => root.init_fsync().set_handle(*handle),
        Request::Release { handle } => root.init_release().set_handle(*handle),
        Request::Opendir { inode } => root.init_opendir().set_inode(*inode),
        Request::Readdir { inode, offset } => {
            let mut out = root.init_readdir();
            out.set_inode(*inode);
            out.set_offset(*offset);
        }
        Request::Readdirplus { inode, offset } => {
            let mut out = root.init_readdirplus();
            out.set_inode(*inode);
            out.set_offset(*offset);
        }
        Request::Symlink {
            parent,
            name,
            target,
        } => {
            let mut out = root.init_symlink();
            out.set_parent(*parent);
            out.set_name(name);
            out.set_target(target);
        }
        Request::Readlink { inode } => root.init_readlink().set_inode(*inode),
    }
    header(&message)
}

pub(super) fn request(bytes: &[u8], payload: Vec<u8>) -> io::Result<Request> {
    use wire::request::Which;
    let request = read(bytes, |message| {
        let root = message.get_root::<wire::request::Reader<'_>>()?;
        Ok(match root.which()? {
            Which::Init(()) => Request::Init,
            Which::Lookup(r) => {
                let (parent, name) = name(r?)?;
                Request::Lookup { parent, name }
            }
            Which::Getattr(r) => {
                let r = r?;
                Request::Getattr {
                    inode: r.get_inode(),
                    handle: opt_u64(r.get_handle()?),
                }
            }
            Which::Setattr(r) => {
                let r = r?;
                Request::Setattr {
                    inode: r.get_inode(),
                    handle: opt_u64(r.get_handle()?),
                    size: opt_u64(r.get_size()?),
                    mode: opt_u32(r.get_mode()?),
                    uid: opt_u32(r.get_uid()?),
                    gid: opt_u32(r.get_gid()?),
                }
            }
            Which::Mkdir(r) => {
                let (parent, name) = name(r?)?;
                Request::Mkdir { parent, name }
            }
            Which::Unlink(r) => {
                let (parent, name) = name(r?)?;
                Request::Unlink { parent, name }
            }
            Which::Rmdir(r) => {
                let (parent, name) = name(r?)?;
                Request::Rmdir { parent, name }
            }
            Which::Rename(r) => {
                let r = r?;
                Request::Rename {
                    olddir: r.get_olddir(),
                    oldname: r.get_oldname()?.to_vec(),
                    newdir: r.get_newdir(),
                    newname: r.get_newname()?.to_vec(),
                    flags: r.get_flags(),
                }
            }
            Which::Open(r) => {
                let r = r?;
                Request::Open {
                    inode: r.get_inode(),
                    flags: r.get_flags(),
                }
            }
            Which::Create(r) => {
                let r = r?;
                Request::Create {
                    parent: r.get_parent(),
                    name: r.get_name()?.to_vec(),
                    flags: r.get_flags(),
                }
            }
            Which::Read(r) => {
                let r = r?;
                Request::Read {
                    handle: r.get_handle(),
                    offset: r.get_offset(),
                    size: r.get_size(),
                }
            }
            Which::Write(r) => {
                let r = r?;
                Request::Write {
                    handle: r.get_handle(),
                    offset: r.get_offset(),
                    data: Vec::new(),
                }
            }
            Which::Flush(r) => Request::Flush {
                handle: r?.get_handle(),
            },
            Which::Fsync(r) => Request::Fsync {
                handle: r?.get_handle(),
            },
            Which::Release(r) => Request::Release {
                handle: r?.get_handle(),
            },
            Which::Opendir(r) => Request::Opendir {
                inode: r?.get_inode(),
            },
            Which::Readdir(r) => {
                let r = r?;
                Request::Readdir {
                    inode: r.get_inode(),
                    offset: r.get_offset(),
                }
            }
            Which::Readdirplus(r) => {
                let r = r?;
                Request::Readdirplus {
                    inode: r.get_inode(),
                    offset: r.get_offset(),
                }
            }
            Which::Symlink(r) => {
                let r = r?;
                Request::Symlink {
                    parent: r.get_parent(),
                    name: r.get_name()?.to_vec(),
                    target: r.get_target()?.to_vec(),
                }
            }
            Which::Readlink(r) => Request::Readlink {
                inode: r?.get_inode(),
            },
        })
    })?;
    match request {
        Request::Write { handle, offset, .. } => Ok(Request::Write {
            handle,
            offset,
            data: payload,
        }),
        other => {
            no_payload(&payload, "request other than WRITE")?;
            Ok(other)
        }
    }
}

// --- replies ------------------------------------------------------------

pub(super) fn reply_header(reply: &Reply) -> Vec<u8> {
    let mut message = Builder::new_default();
    let mut root = message.init_root::<wire::reply::Builder<'_>>();
    match reply {
        Reply::Error { errno } => root.set_error(*errno),
        Reply::Init { want } => root.set_init(*want),
        Reply::Entry { entry } => set_entry(root.init_entry(), entry),
        Reply::Attr { attr, ttl } => {
            let mut out = root.init_attr();
            set_attr(out.reborrow().init_attr(), attr);
            set_time(out.init_ttl(), *ttl);
        }
        Reply::Opened { handle, flags } => {
            let mut out = root.init_opened();
            out.set_handle(*handle);
            out.set_flags(*flags);
        }
        Reply::Created {
            entry,
            handle,
            flags,
        } => {
            let mut out = root.init_created();
            set_entry(out.reborrow().init_entry(), entry);
            out.set_handle(*handle);
            out.set_flags(*flags);
        }
        Reply::Data { .. } => root.set_data(()),
        Reply::Written { size } => root.set_written(*size),
        Reply::Done => root.set_done(()),
        Reply::Dir { entries } => {
            let mut out = root.init_dir(list_len(entries.len()));
            for (i, entry) in entries.iter().enumerate() {
                set_dir_entry(out.reborrow().get(i as u32), entry);
            }
        }
        Reply::DirPlus { entries } => {
            let mut out = root.init_dir_plus(list_len(entries.len()));
            for (i, (dir, entry)) in entries.iter().enumerate() {
                let mut pair = out.reborrow().get(i as u32);
                set_dir_entry(pair.reborrow().init_dir(), dir);
                set_entry(pair.init_entry(), entry);
            }
        }
    }
    header(&message)
}

/// A listing's length as a Cap'n Proto list length. A listing is bounded by
/// the frame limit long before `u32::MAX` entries.
fn list_len(len: usize) -> u32 {
    u32::try_from(len).expect("a directory listing under u32::MAX entries")
}

pub(super) fn reply(bytes: &[u8], payload: Vec<u8>) -> io::Result<Reply> {
    use wire::reply::Which;
    let reply = read(bytes, |message| {
        let root = message.get_root::<wire::reply::Reader<'_>>()?;
        Ok(match root.which()? {
            Which::Error(errno) => Reply::Error { errno },
            Which::Init(want) => Reply::Init { want },
            Which::Entry(r) => Reply::Entry { entry: entry(r?)? },
            Which::Attr(r) => {
                let r = r?;
                Reply::Attr {
                    attr: attr(r.get_attr()?)?,
                    ttl: time(r.get_ttl()?)?,
                }
            }
            Which::Opened(r) => {
                let r = r?;
                Reply::Opened {
                    handle: r.get_handle(),
                    flags: r.get_flags(),
                }
            }
            Which::Created(r) => {
                let r = r?;
                Reply::Created {
                    entry: entry(r.get_entry()?)?,
                    handle: r.get_handle(),
                    flags: r.get_flags(),
                }
            }
            Which::Data(()) => Reply::Data { data: Vec::new() },
            Which::Written(size) => Reply::Written { size },
            Which::Done(()) => Reply::Done,
            Which::Dir(r) => Reply::Dir {
                entries: r?.iter().map(dir_entry).collect::<capnp::Result<_>>()?,
            },
            Which::DirPlus(r) => Reply::DirPlus {
                entries: r?
                    .iter()
                    .map(|pair| Ok((dir_entry(pair.get_dir()?)?, entry(pair.get_entry()?)?)))
                    .collect::<capnp::Result<_>>()?,
            },
        })
    })?;
    match reply {
        Reply::Data { .. } => Ok(Reply::Data { data: payload }),
        other => {
            no_payload(&payload, "reply other than DATA")?;
            Ok(other)
        }
    }
}
