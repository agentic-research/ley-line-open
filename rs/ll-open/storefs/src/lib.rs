//! The store-backed filesystem core behind LLO's virtio-fs device (ADR-0044
//! V2, bead `ley-line-open-f1ea99`).
//!
//! [`StoreFs`] answers the FUSE requests a guest's file syscalls become —
//! LOOKUP, GETATTR, READDIR, OPEN, READ, WRITE, FLUSH, FSYNC, RELEASE,
//! SETATTR, and the refused CREATE, MKDIR, UNLINK, RMDIR, RENAME — from the
//! store, and lands writes as ADR-0042 proposals:
//!
//! - **Reads** resolve a path through the store's tree (`_source`, the one
//!   checkout ADR-0043 names today) to its blob in `source_blobs`. An open
//!   reads the blob once; every READ on that handle serves those bytes.
//! - **OPEN for writing** records the blob it read as the handle's base.
//!   WRITE and a SETATTR of the size stage bytes on the handle.
//! - **FLUSH and FSYNC** propose the staged bytes against the handle's base
//!   through [`leyline_cli_lib::source_write::propose`], on the store's one
//!   writer. An accepted proposal advances the handle's base to the blob it
//!   wrote and, when a checkout is attached, is materialized into it. A
//!   refusal is the request's [`Errno`]: stale → `ESTALE`, outside the
//!   grant → `EACCES`, a syntax refusal and every other failure → `EIO`.
//!   The staged bytes stay, so the next FLUSH answers the same.
//! - **RELEASE** drops the handle. The Linux FUSE client does not return a
//!   RELEASE error to `close`, so FLUSH is the commit point.
//!
//! Errors are Linux errno values whatever the host is: the guest is Linux.
//!
//! The crate adds no persisted state; it reads the tables
//! `source_write` and `checkout` own and writes only through them.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use leyline_cli_lib::daemon::db_pool::LiveDb;
use leyline_cli_lib::source_write::{Base, Grant, Proposed, Refusal};
use leyline_core::Hash;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension};

#[cfg(test)]
mod tests;

/// A Linux errno value: what a refused request answers the guest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Errno(pub i32);

impl Errno {
    pub const EPERM: Errno = Errno(1);
    pub const ENOENT: Errno = Errno(2);
    pub const EIO: Errno = Errno(5);
    pub const EBADF: Errno = Errno(9);
    pub const EACCES: Errno = Errno(13);
    pub const ENOTDIR: Errno = Errno(20);
    pub const EISDIR: Errno = Errno(21);
    pub const ENOSYS: Errno = Errno(38);
    pub const ESTALE: Errno = Errno(116);
}

/// Linux `open(2)` flag bits OPEN carries (`<asm-generic/fcntl.h>`).
pub mod open_flags {
    pub const O_ACCMODE: u32 = 0o3;
    pub const O_RDONLY: u32 = 0o0;
    pub const O_WRONLY: u32 = 0o1;
    pub const O_RDWR: u32 = 0o2;
    pub const O_TRUNC: u32 = 0o1000;
}

/// Linux `st_mode` file-type bits (`<linux/stat.h>`).
pub const S_IFREG: u32 = 0o100000;
pub const S_IFDIR: u32 = 0o040000;

/// The inode of the tree's root directory.
pub const ROOT_INODE: u64 = 1;

/// What a guest may do with the store: the paths it sees and the paths it
/// may propose — a run's `WorkspaceGrant` (`execution/v1`) resolved to
/// paths, its `read`/`list` operations to `see` and its `mutate`/`commit`
/// operations to `propose`. A path outside `see` does not exist for the
/// guest (`ENOENT`); a write outside `propose` is `EACCES`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Scope {
    pub see: Grant,
    pub propose: Grant,
}

/// A checkout an accepted proposal is materialized into (ADR-0043 phase 0,
/// [`leyline_cli_lib::checkout`]): the daemon's source directory and its
/// `--language` filter.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Checkout {
    pub dir: PathBuf,
    pub lang_filter: Option<String>,
}

/// How a [`StoreFs`] answers.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Options {
    /// The entry and attribute timeout every LOOKUP, GETATTR and SETATTR
    /// reply carries. Zero makes the guest ask again on every access; a
    /// tree no other writer moves may hold it longer.
    pub ttl: Duration,
    /// Where accepted proposals are materialized; `None` lands them in the
    /// store only.
    pub checkout: Option<Checkout>,
}

/// The store's connections, serialized the way the daemon serializes them:
/// any number of readers, and one writer every proposal goes through.
pub trait Store: Send + Sync {
    fn with_read<T>(&self, f: impl FnOnce(&Connection) -> anyhow::Result<T>) -> anyhow::Result<T>;
    fn with_write<T>(&self, f: impl FnOnce(&Connection) -> anyhow::Result<T>) -> anyhow::Result<T>;
}

/// The daemon's living database: its reader pool and its single writer.
impl Store for LiveDb {
    fn with_read<T>(&self, f: impl FnOnce(&Connection) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let conn = self
            .reader_pool
            .get()
            .map_err(|e| anyhow::anyhow!("reader pool checkout failed: {e}"))?;
        f(&conn)
    }

    fn with_write<T>(&self, f: impl FnOnce(&Connection) -> anyhow::Result<T>) -> anyhow::Result<T> {
        f(&self.writer.lock())
    }
}

impl<S: Store> Store for Arc<S> {
    fn with_read<T>(&self, f: impl FnOnce(&Connection) -> anyhow::Result<T>) -> anyhow::Result<T> {
        S::with_read(self, f)
    }

    fn with_write<T>(&self, f: impl FnOnce(&Connection) -> anyhow::Result<T>) -> anyhow::Result<T> {
        S::with_write(self, f)
    }
}

/// A file or a directory. The store holds files; a directory is a path
/// prefix some visible file is under.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    File,
    Directory,
}

/// A node's attributes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Attr {
    pub inode: u64,
    pub kind: Kind,
    /// Bytes; zero for a directory.
    pub size: u64,
    /// Permission bits: `0o644` for a file the scope may propose, `0o444`
    /// for one it may only see, `0o555` for a directory (no request adds
    /// or removes an entry).
    pub perm: u32,
}

impl Attr {
    /// The Linux `st_mode`: file-type bits and permission bits. The two
    /// fields occupy disjoint bits, so their sum is their union.
    pub fn mode(&self) -> u32 {
        match self.kind {
            Kind::File => S_IFREG + self.perm,
            Kind::Directory => S_IFDIR + self.perm,
        }
    }

    /// The link count: 1 for a file, 2 for a directory.
    pub fn nlink(&self) -> u32 {
        match self.kind {
            Kind::File => 1,
            Kind::Directory => 2,
        }
    }
}

/// A LOOKUP reply: the node and how long the guest may cache it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    pub attr: Attr,
    pub ttl: Duration,
}

/// One READDIR entry. `offset` is the offset that resumes after it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirEntry {
    pub inode: u64,
    pub offset: u64,
    pub kind: Kind,
    pub name: String,
}

/// The attributes a SETATTR changes. The store holds bytes only: a size is
/// staged or proposed, a mode or owner is `EPERM`, and times are not
/// carried.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SetAttr {
    pub size: Option<u64>,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

/// One open: the blob it read (its base), the bytes it serves and stages,
/// and whether those bytes differ from the base.
struct Handle {
    path: String,
    base: Hash,
    bytes: Vec<u8>,
    dirty: bool,
    writable: bool,
}

/// Inode numbers, stable per path for the life of the [`StoreFs`].
struct Inodes {
    by_path: HashMap<String, u64>,
    by_inode: HashMap<u64, String>,
    next: u64,
}

/// The filesystem: see the crate documentation.
pub struct StoreFs<S> {
    store: S,
    scope: Scope,
    options: Options,
    inodes: Mutex<Inodes>,
    handles: Mutex<HashMap<u64, Arc<Mutex<Handle>>>>,
    next_handle: AtomicU64,
}

impl<S: Store> StoreFs<S> {
    pub fn new(store: S, scope: Scope, options: Options) -> Self {
        StoreFs {
            store,
            scope,
            options,
            inodes: Mutex::new(Inodes {
                by_path: HashMap::from([(String::new(), ROOT_INODE)]),
                by_inode: HashMap::from([(ROOT_INODE, String::new())]),
                next: ROOT_INODE + 1,
            }),
            handles: Mutex::new(HashMap::new()),
            next_handle: AtomicU64::new(1),
        }
    }

    /// The store this filesystem serves.
    pub fn store(&self) -> &S {
        &self.store
    }

    /// LOOKUP: the entry `name` names in the directory `parent`.
    pub fn lookup(&self, parent: u64, name: &[u8]) -> Result<Entry, Errno> {
        let dir = self.directory(parent)?;
        let name = std::str::from_utf8(name).map_err(|_| Errno::ENOENT)?;
        if name.is_empty() || name.contains('/') {
            return Err(Errno::ENOENT);
        }
        Ok(Entry {
            attr: self.attr(&join(&dir, name))?,
            ttl: self.options.ttl,
        })
    }

    /// GETATTR. With a handle, the size is the bytes that handle serves.
    pub fn getattr(&self, inode: u64, handle: Option<u64>) -> Result<(Attr, Duration), Errno> {
        let mut attr = self.attr(&self.path_of(inode)?)?;
        if let Some(open) = handle.and_then(|h| self.handle(h).ok()) {
            attr.size = open.lock().bytes.len() as u64;
        }
        Ok((attr, self.options.ttl))
    }

    /// READDIR: the directory's entries after `offset`, in name order. `.`
    /// and `..` are not listed.
    pub fn readdir(&self, inode: u64, offset: u64) -> Result<Vec<DirEntry>, Errno> {
        let dir = self.directory(inode)?;
        let children = self.query(|conn| self.children(conn, &dir))?;
        Ok(children
            .into_iter()
            .enumerate()
            .skip(offset as usize)
            .map(|(i, (name, kind))| DirEntry {
                inode: self.inode_of(&join(&dir, &name)),
                offset: i as u64 + 1,
                kind,
                name,
            })
            .collect())
    }

    /// OPEN: reads the file's blob, which the handle serves and, opened for
    /// writing, proposes against. `O_TRUNC` on a write open stages an empty
    /// file.
    pub fn open(&self, inode: u64, flags: u32) -> Result<u64, Errno> {
        let path = self.path_of(inode)?;
        if self.attr(&path)?.kind == Kind::Directory {
            return Err(Errno::EISDIR);
        }
        let writable = flags & open_flags::O_ACCMODE != open_flags::O_RDONLY;
        if writable && !self.scope.propose.covers(&path) {
            return Err(Errno::EACCES);
        }
        let (base, mut bytes) = self
            .query(|conn| blob_of(conn, &path))?
            .ok_or(Errno::ENOENT)?;
        let truncate = writable && flags & open_flags::O_TRUNC != 0;
        if truncate {
            bytes.clear();
        }
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.handles.lock().insert(
            handle,
            Arc::new(Mutex::new(Handle {
                path,
                base,
                bytes,
                dirty: truncate,
                writable,
            })),
        );
        Ok(handle)
    }

    /// READ: up to `size` bytes from `offset` of what the handle serves.
    pub fn read(&self, handle: u64, offset: u64, size: u32) -> Result<Vec<u8>, Errno> {
        let open = self.handle(handle)?;
        let open = open.lock();
        let len = open.bytes.len();
        let start = usize::try_from(offset).unwrap_or(usize::MAX).min(len);
        let end = start.saturating_add(size as usize).min(len);
        Ok(open.bytes[start..end].to_vec())
    }

    /// WRITE: stages `data` at `offset` on the handle; a gap past the end
    /// reads as zeros. Nothing reaches the store until FLUSH or FSYNC.
    pub fn write(&self, handle: u64, offset: u64, data: &[u8]) -> Result<usize, Errno> {
        let open = self.handle(handle)?;
        let mut open = open.lock();
        if !open.writable {
            return Err(Errno::EBADF);
        }
        let start = offset as usize;
        let end = start + data.len();
        let len = open.bytes.len().max(end);
        open.bytes.resize(len, 0);
        open.bytes[start..end].copy_from_slice(data);
        open.dirty = true;
        Ok(data.len())
    }

    /// FLUSH (each `close`): proposes the staged bytes. See [`errno_for`].
    pub fn flush(&self, handle: u64) -> Result<(), Errno> {
        self.commit(handle)
    }

    /// FSYNC: proposes the staged bytes, as FLUSH does.
    pub fn fsync(&self, handle: u64) -> Result<(), Errno> {
        self.commit(handle)
    }

    /// RELEASE: drops the handle and any bytes no FLUSH landed.
    pub fn release(&self, handle: u64) {
        self.handles.lock().remove(&handle);
    }

    /// SETATTR. A size with a handle (`ftruncate`) stages on it; a size
    /// without one (`truncate`) is a proposal of its own against the
    /// file's current blob. A mode or owner is `EPERM`; times are not
    /// carried.
    pub fn setattr(
        &self,
        inode: u64,
        handle: Option<u64>,
        set: SetAttr,
    ) -> Result<(Attr, Duration), Errno> {
        let path = self.path_of(inode)?;
        if set.mode.is_some() || set.uid.is_some() || set.gid.is_some() {
            return Err(Errno::EPERM);
        }
        if let Some(size) = set.size {
            if self.attr(&path)?.kind == Kind::Directory {
                return Err(Errno::EISDIR);
            }
            match handle {
                Some(handle) => {
                    let open = self.handle(handle)?;
                    let mut open = open.lock();
                    if !open.writable {
                        return Err(Errno::EBADF);
                    }
                    open.bytes.resize(size as usize, 0);
                    open.dirty = true;
                }
                None => {
                    if !self.scope.propose.covers(&path) {
                        return Err(Errno::EACCES);
                    }
                    let (base, mut bytes) = self
                        .query(|conn| blob_of(conn, &path))?
                        .ok_or(Errno::ENOENT)?;
                    bytes.resize(size as usize, 0);
                    self.propose(&path, base, bytes)?;
                }
            }
        }
        self.getattr(inode, handle)
    }

    /// CREATE: refused. A proposal writes a file the store already holds;
    /// the store has no create.
    pub fn create(&self, parent: u64, _name: &[u8], _flags: u32) -> Result<(Entry, u64), Errno> {
        self.directory(parent)?;
        Err(Errno::EPERM)
    }

    /// MKDIR: refused; a directory is a prefix of the files the store holds.
    pub fn mkdir(&self, parent: u64, _name: &[u8]) -> Result<Entry, Errno> {
        self.directory(parent)?;
        Err(Errno::EPERM)
    }

    /// UNLINK: refused; the store has no delete.
    pub fn unlink(&self, parent: u64, name: &[u8]) -> Result<(), Errno> {
        self.lookup(parent, name)?;
        Err(Errno::EPERM)
    }

    /// RMDIR: refused; the store has no delete.
    pub fn rmdir(&self, parent: u64, name: &[u8]) -> Result<(), Errno> {
        self.lookup(parent, name)?;
        Err(Errno::EPERM)
    }

    /// RENAME: refused; the store has no move.
    pub fn rename(
        &self,
        olddir: u64,
        oldname: &[u8],
        newdir: u64,
        _newname: &[u8],
    ) -> Result<(), Errno> {
        self.lookup(olddir, oldname)?;
        self.directory(newdir)?;
        Err(Errno::EPERM)
    }

    /// FLUSH and FSYNC: propose the handle's staged bytes against its base.
    fn commit(&self, handle: u64) -> Result<(), Errno> {
        let open = self.handle(handle)?;
        let mut open = open.lock();
        if !open.dirty {
            return Ok(());
        }
        open.base = self.propose(&open.path, open.base, open.bytes.clone())?;
        open.dirty = false;
        Ok(())
    }

    /// One file proposed through the store's writer (ADR-0042), then
    /// materialized into the checkout under the same writer, as the
    /// daemon's `propose` op does. Answers the blob the file now holds.
    fn propose(&self, path: &str, base: Hash, bytes: Vec<u8>) -> Result<Hash, Errno> {
        let accepted = self
            .store
            .with_write(|conn| {
                let accepted = leyline_cli_lib::source_write::propose(
                    conn,
                    &self.scope.propose,
                    vec![Proposed {
                        path: path.to_string(),
                        base: Base::Blob(base),
                        bytes,
                    }],
                )?;
                if let Some(checkout) = &self.options.checkout {
                    let paths = [path.to_string()];
                    match leyline_cli_lib::checkout::reconcile(
                        conn,
                        &checkout.dir,
                        checkout.lang_filter.as_deref(),
                        Some(&paths),
                    ) {
                        Ok(reconciled) => {
                            if let Some(problem) = reconciled.problems() {
                                log::warn!("proposal to '{path}' landed; {problem}");
                            }
                        }
                        Err(e) => log::warn!(
                            "proposal to '{path}' landed; the checkout was not brought up \
                             to it: {e:#}"
                        ),
                    }
                }
                Ok(accepted)
            })
            .map_err(|e| {
                log::info!("proposal to '{path}' not applied: {e:#}");
                errno_for(&e)
            })?;
        if let Some(problem) = &accepted.post_commit_error {
            log::warn!("proposal to '{path}' landed; after it: {problem}");
        }
        accepted.blobs.get(path).copied().ok_or(Errno::EIO)
    }

    fn handle(&self, handle: u64) -> Result<Arc<Mutex<Handle>>, Errno> {
        self.handles
            .lock()
            .get(&handle)
            .cloned()
            .ok_or(Errno::EBADF)
    }

    fn path_of(&self, inode: u64) -> Result<String, Errno> {
        self.inodes
            .lock()
            .by_inode
            .get(&inode)
            .cloned()
            .ok_or(Errno::ENOENT)
    }

    fn inode_of(&self, path: &str) -> u64 {
        let mut inodes = self.inodes.lock();
        if let Some(&inode) = inodes.by_path.get(path) {
            return inode;
        }
        let inode = inodes.next;
        inodes.next += 1;
        inodes.by_path.insert(path.to_string(), inode);
        inodes.by_inode.insert(inode, path.to_string());
        inode
    }

    /// The path of `inode` when it names a directory.
    fn directory(&self, inode: u64) -> Result<String, Errno> {
        let path = self.path_of(inode)?;
        match self.attr(&path)?.kind {
            Kind::Directory => Ok(path),
            Kind::File => Err(Errno::ENOTDIR),
        }
    }

    fn attr(&self, path: &str) -> Result<Attr, Errno> {
        let (kind, size) = self
            .query(|conn| self.stat_path(conn, path))?
            .ok_or(Errno::ENOENT)?;
        let perm = match kind {
            Kind::Directory => 0o555,
            Kind::File if self.scope.propose.covers(path) => 0o644,
            Kind::File => 0o444,
        };
        Ok(Attr {
            inode: self.inode_of(path),
            kind,
            size,
            perm,
        })
    }

    /// What `path` is for the guest: a file the store holds a blob for and
    /// the scope sees, a directory some such file is under, or nothing. The
    /// root is always a directory.
    fn stat_path(&self, conn: &Connection, path: &str) -> anyhow::Result<Option<(Kind, u64)>> {
        if self.scope.see.covers(path) {
            let size: Option<i64> = conn
                .query_row(
                    "SELECT length(b.blob_bytes) FROM _source s \
                       JOIN source_blobs b ON b.blob_hash = s.content_hash WHERE s.id = ?1",
                    [path],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(size) = size {
                return Ok(Some((Kind::File, size as u64)));
            }
        }
        let is_dir = path.is_empty() || !self.children(conn, path)?.is_empty();
        Ok(is_dir.then_some((Kind::Directory, 0)))
    }

    /// The names directly under the directory `dir` that the scope sees.
    fn children(&self, conn: &Connection, dir: &str) -> anyhow::Result<BTreeMap<String, Kind>> {
        let prefix = join(dir, "");
        let mut stmt = conn.prepare_cached(
            "SELECT s.id FROM _source s JOIN source_blobs b ON b.blob_hash = s.content_hash \
              WHERE substr(s.id, 1, length(?1)) = ?1",
        )?;
        let mut out = BTreeMap::new();
        for id in stmt.query_map([&prefix], |r| r.get::<_, String>(0))? {
            let id = id?;
            if !self.scope.see.covers(&id) {
                continue;
            }
            let rest = &id[prefix.len()..];
            match rest.split_once('/') {
                Some((name, _)) => out.insert(name.to_string(), Kind::Directory),
                None => out.insert(rest.to_string(), Kind::File),
            };
        }
        Ok(out)
    }

    fn query<T>(&self, f: impl FnOnce(&Connection) -> anyhow::Result<T>) -> Result<T, Errno> {
        self.store.with_read(f).map_err(|e| {
            log::warn!("store read failed: {e:#}");
            Errno::EIO
        })
    }
}

/// `name` under the directory `dir` (the root is `""`).
fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// The blob `path` holds and its bytes, or `None` when the store holds none.
fn blob_of(conn: &Connection, path: &str) -> anyhow::Result<Option<(Hash, Vec<u8>)>> {
    let row: Option<(Vec<u8>, Vec<u8>)> = conn
        .query_row(
            "SELECT s.content_hash, b.blob_bytes FROM _source s \
               JOIN source_blobs b ON b.blob_hash = s.content_hash WHERE s.id = ?1",
            [path],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(hash, bytes)| {
        let hash = <[u8; 32]>::try_from(hash)
            .map_err(|h| anyhow::anyhow!("'{path}': a {}-byte blob hash", h.len()))?;
        Ok((Hash::from_bytes(hash), bytes))
    })
    .transpose()
}

/// The errno a proposal that was not applied answers: a stale base is
/// `ESTALE`, a path outside the grant `EACCES`, and a syntax refusal, every
/// other refusal and every internal failure `EIO`.
pub fn errno_for(err: &anyhow::Error) -> Errno {
    match err.downcast_ref::<Refusal>() {
        Some(Refusal::Stale(_)) => Errno::ESTALE,
        Some(Refusal::OutsideGrant { .. }) => Errno::EACCES,
        _ => Errno::EIO,
    }
}
