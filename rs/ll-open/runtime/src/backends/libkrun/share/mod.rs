//! A virtio-fs share whose FUSE requests are answered in another process
//! (ADR-0044 phase C, bead `ley-line-open-f210c2`).
//!
//! The VMM runs in the `leyline-krun-worker` process; the store a run's
//! workspace is served from lives in the daemon that started it, behind the
//! daemon's one writer. The worker's virtio-fs device ([`ShareFs`], `vmm`
//! feature) forwards each FUSE request over a connected UNIX socket the
//! daemon handed it at spawn; the daemon answers with [`serve`] over an
//! [`FsService`] (`leyline-storefs`'s store-backed one).
//!
//! The wire carries FUSE's own vocabulary: inode numbers, `st_mode` bits,
//! Linux errno values, `FOPEN_*` and `FUSE_*` flag bits, `DT_*` entry types
//! and timeouts. The service decides every reply; the device transcribes it
//! into libkrun's types and adds nothing.
//!
//! Framing: two little-endian `u32` lengths (header, payload), a JSON header
//! ([`Request`] or [`Reply`]), then the payload — the bytes a WRITE carries or
//! a READ answers, which stay out of the JSON.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

#[cfg(feature = "vmm")]
mod device;
#[cfg(feature = "vmm")]
pub use device::ShareFs;

/// `FUSE_DO_READDIRPLUS` (`<linux/fuse.h>`): the guest asks for entries with
/// their attributes.
pub const FUSE_DO_READDIRPLUS: u64 = 1 << 13;
/// `FUSE_WRITEBACK_CACHE`: the guest caches writes. A share never asks for it:
/// every WRITE must reach the service.
pub const FUSE_WRITEBACK_CACHE: u64 = 1 << 16;
/// `FOPEN_DIRECT_IO`: reads and writes bypass the guest's page cache.
pub const FOPEN_DIRECT_IO: u32 = 1;
/// `FOPEN_KEEP_CACHE`: the guest keeps the pages it holds for the file.
pub const FOPEN_KEEP_CACHE: u32 = 1 << 1;
/// `FOPEN_CACHE_DIR`: the guest may cache the directory's listing.
pub const FOPEN_CACHE_DIR: u32 = 1 << 3;
/// `DT_DIR` (`<dirent.h>`).
pub const DT_DIR: u32 = 4;
/// `DT_REG` (`<dirent.h>`).
pub const DT_REG: u32 = 8;
/// `DT_LNK` (`<dirent.h>`).
pub const DT_LNK: u32 = 10;
/// Linux `EIO`: what the device answers when the service cannot be reached.
pub const LINUX_EIO: i32 = 5;

/// The largest header or payload a frame may carry: far above any FUSE
/// request (libkrun's buffers are 1 MiB) and a directory listing, and a
/// bound on what a corrupt length can make the reader allocate.
pub const MAX_FRAME: u32 = 64 << 20;

/// A node's attributes, as `stat` reports them to the guest.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Attr {
    pub inode: u64,
    /// Linux `st_mode`: file-type and permission bits.
    pub mode: u32,
    pub nlink: u32,
    pub size: u64,
    /// 512-byte blocks.
    pub blocks: u64,
    pub blksize: u32,
    /// `st_atime`, `st_mtime` and `st_ctime`, since the Unix epoch. git
    /// decides from them (with the size and inode) whether a file needs
    /// rehashing.
    pub atime: Duration,
    pub mtime: Duration,
    pub ctime: Duration,
}

/// A LOOKUP reply. `inode` 0 is a negative entry: the name does not exist,
/// and the guest may cache that for `entry_ttl`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub inode: u64,
    pub attr: Attr,
    pub entry_ttl: Duration,
    pub attr_ttl: Duration,
}

/// One directory entry; `offset` resumes after it.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct DirEntry {
    pub inode: u64,
    pub offset: u64,
    /// `DT_REG`, `DT_DIR` or `DT_LNK`.
    pub kind: u32,
    pub name: Vec<u8>,
}

/// A FUSE request the guest sent.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Init,
    Lookup {
        parent: u64,
        name: Vec<u8>,
    },
    Getattr {
        inode: u64,
        handle: Option<u64>,
    },
    /// The attributes SETATTR names as valid; the rest are `None`.
    Setattr {
        inode: u64,
        handle: Option<u64>,
        size: Option<u64>,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
    },
    Mkdir {
        parent: u64,
        name: Vec<u8>,
    },
    Unlink {
        parent: u64,
        name: Vec<u8>,
    },
    Rmdir {
        parent: u64,
        name: Vec<u8>,
    },
    /// `flags` are `renameat2(2)`'s; 0 for RENAME.
    Rename {
        olddir: u64,
        oldname: Vec<u8>,
        newdir: u64,
        newname: Vec<u8>,
        flags: u32,
    },
    /// `flags` are the guest's `open(2)` flags.
    Open {
        inode: u64,
        flags: u32,
    },
    Create {
        parent: u64,
        name: Vec<u8>,
        flags: u32,
    },
    Read {
        handle: u64,
        offset: u64,
        size: u32,
    },
    Write {
        handle: u64,
        offset: u64,
        /// Carried as the frame's payload.
        #[serde(skip)]
        data: Vec<u8>,
    },
    Flush {
        handle: u64,
    },
    Fsync {
        handle: u64,
    },
    Release {
        handle: u64,
    },
    Opendir {
        inode: u64,
    },
    Readdir {
        inode: u64,
        offset: u64,
    },
    Readdirplus {
        inode: u64,
        offset: u64,
    },
    /// SYMLINK: a link `name` in `parent` whose target is `target`.
    Symlink {
        parent: u64,
        name: Vec<u8>,
        target: Vec<u8>,
    },
    /// READLINK: answered with [`Reply::Data`], the link's target.
    Readlink {
        inode: u64,
    },
}

/// The answer to a [`Request`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    /// A Linux errno value.
    Error {
        errno: i32,
    },
    /// INIT: the `FUSE_*` capability bits the service wants.
    Init {
        want: u64,
    },
    Entry {
        entry: Entry,
    },
    Attr {
        attr: Attr,
        ttl: Duration,
    },
    /// OPEN and OPENDIR: the handle and its `FOPEN_*` bits.
    Opened {
        handle: u64,
        flags: u32,
    },
    Created {
        entry: Entry,
        handle: u64,
        flags: u32,
    },
    Data {
        /// Carried as the frame's payload.
        #[serde(skip)]
        data: Vec<u8>,
    },
    Written {
        size: u32,
    },
    Done,
    Dir {
        entries: Vec<DirEntry>,
    },
    DirPlus {
        entries: Vec<(DirEntry, Entry)>,
    },
}

/// What answers a share's requests.
pub trait FsService: Send + Sync {
    fn call(&self, request: Request) -> Reply;
}

/// Where a run's workspace is served from: the embedder that holds the store
/// (the daemon). The libkrun backend asks it before it spawns the worker;
/// an error refuses the run.
pub trait WorkspaceShares: Send + Sync {
    /// The service answering the guest's share of `grant`'s workspace for
    /// run `run_id`, scoped by the grant's operations.
    fn open(
        &self,
        run_id: &str,
        grant: &crate::WorkspaceGrant,
    ) -> Result<std::sync::Arc<dyn FsService>, crate::ExecutionError>;
}

fn write_frame(writer: &mut impl Write, header: &[u8], payload: &[u8]) -> io::Result<()> {
    let header_len = frame_len(header.len())?;
    let payload_len = frame_len(payload.len())?;
    let mut frame = Vec::with_capacity(8 + header.len() + payload.len());
    frame.extend_from_slice(&header_len.to_le_bytes());
    frame.extend_from_slice(&payload_len.to_le_bytes());
    frame.extend_from_slice(header);
    frame.extend_from_slice(payload);
    writer.write_all(&frame)?;
    writer.flush()
}

fn frame_len(len: usize) -> io::Result<u32> {
    u32::try_from(len)
        .ok()
        .filter(|&len| len <= MAX_FRAME)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("share frame part of {len} bytes exceeds {MAX_FRAME}"),
            )
        })
}

/// One frame's header and payload; `None` at a clean end of stream (no byte
/// of a new frame read).
fn read_frame(reader: &mut impl Read) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
    let mut lengths = [0u8; 8];
    let mut filled = 0;
    while filled < lengths.len() {
        match reader.read(&mut lengths[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => filled += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    let [h0, h1, h2, h3, p0, p1, p2, p3] = lengths;
    let header_len = frame_len(u32::from_le_bytes([h0, h1, h2, h3]) as usize)?;
    let payload_len = frame_len(u32::from_le_bytes([p0, p1, p2, p3]) as usize)?;
    let mut header = vec![0; header_len as usize];
    reader.read_exact(&mut header)?;
    let mut payload = vec![0; payload_len as usize];
    reader.read_exact(&mut payload)?;
    Ok(Some((header, payload)))
}

fn invalid(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

pub fn write_request(writer: &mut impl Write, request: &Request) -> io::Result<()> {
    let header = serde_json::to_vec(request).map_err(invalid)?;
    let payload = match request {
        Request::Write { data, .. } => data.as_slice(),
        _ => &[],
    };
    write_frame(writer, &header, payload)
}

/// The next request; `None` when the peer closed the stream between frames.
pub fn read_request(reader: &mut impl Read) -> io::Result<Option<Request>> {
    let Some((header, payload)) = read_frame(reader)? else {
        return Ok(None);
    };
    let mut request: Request = serde_json::from_slice(&header).map_err(invalid)?;
    if let Request::Write { data, .. } = &mut request {
        *data = payload;
    }
    Ok(Some(request))
}

pub fn write_reply(writer: &mut impl Write, reply: &Reply) -> io::Result<()> {
    let header = serde_json::to_vec(reply).map_err(invalid)?;
    let payload = match reply {
        Reply::Data { data } => data.as_slice(),
        _ => &[],
    };
    write_frame(writer, &header, payload)
}

pub fn read_reply(reader: &mut impl Read) -> io::Result<Reply> {
    let (header, payload) = read_frame(reader)?.ok_or(io::ErrorKind::UnexpectedEof)?;
    let mut reply: Reply = serde_json::from_slice(&header).map_err(invalid)?;
    if let Reply::Data { data } = &mut reply {
        *data = payload;
    }
    Ok(reply)
}

/// Answer requests from `reader` with `service`, replies to `writer`, until
/// the peer closes the stream. Requests are answered one at a time, in
/// order.
pub fn serve(
    reader: &mut impl Read,
    writer: &mut impl Write,
    service: &dyn FsService,
) -> io::Result<()> {
    while let Some(request) = read_request(reader)? {
        write_reply(writer, &service.call(request))?;
    }
    Ok(())
}

/// An [`FsService`] in another process, reached over a connected stream that
/// process [`serve`]s. Calls are serialized: one request is in flight at a
/// time. A stream that fails stays failed: every later call is `EIO`,
/// because a reply read after a broken exchange could answer the wrong
/// request.
pub struct RemoteFs {
    stream: Mutex<Option<UnixStream>>,
}

impl RemoteFs {
    pub fn new(stream: UnixStream) -> Self {
        Self {
            stream: Mutex::new(Some(stream)),
        }
    }
}

impl FsService for RemoteFs {
    fn call(&self, request: Request) -> Reply {
        let mut stream = self.stream.lock();
        let Some(connected) = stream.as_mut() else {
            return Reply::Error { errno: LINUX_EIO };
        };
        let exchanged = write_request(connected, &request).and_then(|()| read_reply(connected));
        match exchanged {
            Ok(reply) => reply,
            Err(_) => {
                *stream = None;
                Reply::Error { errno: LINUX_EIO }
            }
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod device_tests;
