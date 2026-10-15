//! The store-backed filesystem core behind LLO's virtio-fs device (ADR-0044
//! V2, bead `ley-line-open-f1ea99`).
//!
//! [`StoreFs`] answers the FUSE requests a guest's file syscalls become —
//! LOOKUP, GETATTR, READDIR, READDIRPLUS, OPENDIR, OPEN, READ, WRITE,
//! FLUSH, FSYNC, RELEASE, SETATTR, CREATE, UNLINK, RENAME, MKDIR and RMDIR —
//! from the store, and lands changes as ADR-0042 proposals:
//!
//! - **Reads** resolve a path through the store's manifest (`_tree`,
//!   ADR-0045 T1: every file of any language with its git mode, every
//!   symlink, every directory) to its blob in `source_blobs`. An open reads
//!   the blob once; every READ on that handle serves those bytes.
//! - **OPEN for writing** records the blob it read as the handle's base;
//!   **CREATE** opens a handle whose base is "no file". WRITE and a SETATTR
//!   of the size stage bytes on the handle, whatever the store holds now.
//! - **FLUSH and FSYNC** propose the staged bytes against the handle's base
//!   through [`leyline_cli_lib::source_write::propose`], on the store's one
//!   writer: a replace, or a create for a created file. When another writer
//!   moved the file since the base, the store merges the two three ways
//!   (ADR-0045 T5): a clean merge lands, and the handle then serves the
//!   merged bytes; overlapping edits are `ESTALE`. An accepted proposal
//!   advances the handle's base to the blob it wrote and, when a checkout is
//!   attached, is materialized into it. A refusal is the request's [`Errno`]
//!   ([`errno_for`]). The staged bytes stay, so the next FLUSH answers the
//!   same.
//! - **UNLINK** proposes a delete; **RENAME** of a file proposes the delete
//!   of the old path and the create (or replace) of the new one as one
//!   proposal, so a write-temp-then-rename save is one change; RENAME of a
//!   directory is one proposal over every path under it.
//! - **MKDIR**, **RMDIR**, **SYMLINK** and a SETATTR of the mode are
//!   proposals; **READLINK** answers a symlink's stored target.
//! - **RELEASE** drops the handle. The Linux FUSE client does not return a
//!   RELEASE error to `close`, so FLUSH is the commit point.
//!
//! A created file lives in this filesystem's session from CREATE until its
//! first FLUSH lands it (or UNLINK drops it); every name a guest can make is
//! a name the store holds.
//!
//! **Read freshness** (ADR-0044 V3): nothing can tell the guest a path
//! changed, so every OPEN reaches the store and reads the current blob, a
//! read-only open lets the guest keep its cached pages only when the blob is
//! the one the previous open served ([`Opened`]), and entry and attribute
//! timeouts are short only for recently changed paths ([`Freshness`]).
//!
//! **File times** (bead `ley-line-open-b8100d`): a node's [`Attr::mtime`]
//! is the time of the latest change this filesystem recorded to its path,
//! strictly increasing per path, or the [`Clock`]'s origin when it recorded
//! none; git's stat check sees every change, same-size edits included.
//!
//! Errors are Linux errno values whatever the host is: the guest is Linux.
//!
//! The crate adds no persisted state; it reads the tables
//! `source_write` and `checkout` own and writes only through them.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use leyline_cli_lib::daemon::db_pool::LiveDb;
use leyline_cli_lib::source_write::{self, Accepted, Base, Grant, Mode, Proposed, Refusal};
use leyline_cli_lib::tree::{self as store_tree, Entry as StoreEntry};
use leyline_core::Hash;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension};

pub mod daemon;
pub mod share;
pub use daemon::{DaemonShares, scope_for};
pub use share::StoreShare;

#[cfg(test)]
mod daemon_tests;
#[cfg(test)]
mod share_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tree_tests;

/// A Linux errno value: what a refused request answers the guest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Errno(pub i32);

impl Errno {
    pub const EPERM: Errno = Errno(1);
    pub const ENOENT: Errno = Errno(2);
    pub const EIO: Errno = Errno(5);
    pub const EBADF: Errno = Errno(9);
    pub const EACCES: Errno = Errno(13);
    pub const EEXIST: Errno = Errno(17);
    pub const EXDEV: Errno = Errno(18);
    pub const ENOTDIR: Errno = Errno(20);
    pub const EISDIR: Errno = Errno(21);
    pub const EINVAL: Errno = Errno(22);
    pub const ENOSYS: Errno = Errno(38);
    pub const ENOTEMPTY: Errno = Errno(39);
    pub const ESTALE: Errno = Errno(116);
}

/// Linux `renameat2(2)` flag bits RENAME carries (`<linux/fs.h>`).
pub mod rename_flags {
    pub const RENAME_NOREPLACE: u32 = 1;
    pub const RENAME_EXCHANGE: u32 = 2;
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
pub const S_IFLNK: u32 = 0o120000;

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

/// How long the guest may cache what a reply tells it (ADR-0044 V3, "Read
/// freshness"). Nothing can tell the guest that a path changed (libkrun's
/// device has no notification queue, nor does Linux's virtio-fs client), so
/// a cached entry or attribute is only as fresh as its timeout.
///
/// A path is **recently changed** at time `now` when this [`StoreFs`]
/// recorded a change to it at `t0` with `now - t0 < window`. It records a
/// change to a path, and to the path's parent directory, when
///
/// - a proposal it lands writes, creates or deletes the path (FLUSH, FSYNC,
///   SETATTR of a size, UNLINK, RENAME), or
/// - it serves the path (LOOKUP, GETATTR, SETATTR, OPEN, READDIRPLUS, and
///   the directory checks of every request naming a parent) in a state —
///   absent, a directory, or a file with its blob — other than the one it
///   last served: another writer's proposal, observed.
///
/// Entry, attribute and negative-entry timeouts are `changed` for a
/// recently changed path and `settled` for every other.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Freshness {
    /// The timeout of a path no change was recorded for within `window`.
    pub settled: Duration,
    /// The timeout of a recently changed path.
    pub changed: Duration,
    /// How long a recorded change keeps a path's timeouts at `changed`.
    pub window: Duration,
}

impl Default for Freshness {
    /// 60 s for settled paths, 1 s for a path changed in the last 60 s.
    fn default() -> Self {
        Freshness {
            settled: Duration::from_secs(60),
            changed: Duration::from_secs(1),
            window: Duration::from_secs(60),
        }
    }
}

/// How a [`StoreFs`] answers.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Options {
    /// The entry, attribute and negative-entry timeouts replies carry.
    pub freshness: Freshness,
    /// Where accepted proposals are materialized; `None` lands them in the
    /// store only.
    pub checkout: Option<Checkout>,
}

/// Time for [`Freshness`] and file times: the time since a fixed origin,
/// never decreasing, and the wall-clock time of that origin.
pub trait Clock: Send + Sync {
    fn now(&self) -> Duration;
    /// The wall-clock time of the origin [`Clock::now`] counts from, since
    /// the Unix epoch.
    fn origin(&self) -> Duration;
}

/// [`Clock`] over [`Instant`], counting from its creation; its origin is
/// the [`SystemTime`] read at its creation.
#[derive(Clone, Copy, Debug)]
pub struct MonotonicClock {
    origin: Instant,
    wall: Duration,
}

impl Default for MonotonicClock {
    fn default() -> Self {
        MonotonicClock {
            origin: Instant::now(),
            wall: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default(),
        }
    }
}

impl Clock for MonotonicClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    fn origin(&self) -> Duration {
        self.wall
    }
}

/// An OPEN reply: the handle and what the guest's page cache does with the
/// file (FUSE `FOPEN_KEEP_CACHE`, `FOPEN_DIRECT_IO`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Opened {
    pub handle: u64,
    /// The guest may keep the pages it holds for the file: set on a
    /// read-only open of a stored file whose blob is the one the previous
    /// open of the inode served, that open read-only. Unset, the guest
    /// drops the file's pages at this open.
    pub keep_cache: bool,
    /// Every READ and WRITE reaches the backend, bypassing the page cache:
    /// set on every write open, so each WRITE meets the base check.
    pub direct_io: bool,
}

/// An OPENDIR reply: whether the guest may keep the listing it cached for
/// the directory (FUSE `FOPEN_KEEP_CACHE`; the adapter sets
/// `FOPEN_CACHE_DIR` on every OPENDIR).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenedDir {
    /// Set when the directory lists the same names, with the same kinds,
    /// as when the previous OPENDIR of the inode served it.
    pub keep_cache: bool,
}

/// The store's connections, serialized the way the daemon serializes them:
/// any number of readers, and one writer every proposal goes through.
pub trait Store: Send + Sync {
    fn with_read<T>(&self, f: impl FnOnce(&Connection) -> anyhow::Result<T>) -> anyhow::Result<T>;
    fn with_write<T>(&self, f: impl FnOnce(&Connection) -> anyhow::Result<T>) -> anyhow::Result<T>;

    /// Publish the store after a proposal landed, as the daemon's
    /// `propose` op does after its commit: out-of-process readers see the
    /// change. A failure does not undo the proposal; the next publish
    /// serializes the whole store. The default publishes nothing.
    fn publish(&self) -> anyhow::Result<()> {
        Ok(())
    }
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

    fn publish(&self) -> anyhow::Result<()> {
        S::publish(self)
    }
}

/// A file, a directory or a symbolic link: the store's entries (ADR-0045
/// T1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    File,
    Directory,
    Symlink,
}

/// A node's attributes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Attr {
    pub inode: u64,
    pub kind: Kind,
    /// Bytes (a symlink's: its target's); zero for a directory.
    pub size: u64,
    /// Permission bits: `0o644` for a file the scope may propose, `0o444`
    /// for one it may only see, each with the execute bits where the
    /// readable ones are for an executable file (`0o755`, `0o555`); `0o755`
    /// for a directory the scope may propose a path under, `0o555` for one
    /// it may not; `0o777` for a symlink.
    pub perm: u32,
    /// The node's modification time, since the Unix epoch: the time of the
    /// last change this filesystem recorded for the path, or the clock's
    /// origin when it recorded none (see "File times" in the README). The
    /// reply reports it as the change and access time too.
    pub mtime: Duration,
}

impl Attr {
    /// The Linux `st_mode`: file-type bits and permission bits. The two
    /// fields occupy disjoint bits, so their sum is their union.
    pub fn mode(&self) -> u32 {
        match self.kind {
            Kind::File => S_IFREG + self.perm,
            Kind::Directory => S_IFDIR + self.perm,
            Kind::Symlink => S_IFLNK + self.perm,
        }
    }

    /// The link count: 2 for a directory, 1 for anything else.
    pub fn nlink(&self) -> u32 {
        match self.kind {
            Kind::File | Kind::Symlink => 1,
            Kind::Directory => 2,
        }
    }
}

/// A LOOKUP (or READDIRPLUS) reply: the node and how long the guest may
/// cache it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    pub attr: Attr,
    /// The entry and the attribute timeout ([`Freshness`]).
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

/// The attributes a SETATTR changes. A size is staged or proposed; a mode
/// is a proposal of the file's git mode (executable when any execute bit
/// is set, regular otherwise; a directory's or symlink's is accepted and
/// changes nothing); an owner is `EPERM`; times are not carried.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SetAttr {
    pub size: Option<u64>,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

/// One open: what it read (its base: a blob, or [`Base::Absent`] for a file
/// the store does not hold yet), the bytes it serves and stages, and
/// whether those bytes are still to be proposed.
struct Handle {
    path: String,
    base: Base,
    bytes: Vec<u8>,
    /// A mode an `fchmod` staged on this write handle, proposed with its
    /// bytes at FLUSH.
    mode: Option<Mode>,
    dirty: bool,
    writable: bool,
}

/// What this session holds that the store does not yet.
#[derive(Default)]
struct Session {
    /// Files created here and not landed yet, with their bytes (empty from
    /// CREATE). A file leaves when its first FLUSH lands it, when UNLINK
    /// drops it, or when RENAME lands it under its new name.
    pending: BTreeMap<String, Vec<u8>>,
    /// A mode a `chmod` by path gave a pending file, landed with it.
    pending_modes: BTreeMap<String, Mode>,
}

/// Inode numbers, stable per path for the life of the [`StoreFs`]; a
/// RENAME carries the inode to the new path.
struct Inodes {
    by_path: HashMap<String, u64>,
    by_inode: HashMap<u64, String>,
    next: u64,
}

impl Inodes {
    /// [`Inodes::rename`] of `from` and of every path under it.
    fn rename_tree(&mut self, from: &str, to: &str) {
        let prefix = format!("{from}/");
        let under: Vec<String> = self
            .by_path
            .keys()
            .filter(|p| p.starts_with(&prefix))
            .cloned()
            .collect();
        self.rename(from, to);
        for path in under {
            let moved = format!("{to}{}", &path[from.len()..]);
            self.rename(&path, &moved);
        }
    }

    /// The inode of `from` now names `to`; the inode `to` named, if any,
    /// names nothing.
    fn rename(&mut self, from: &str, to: &str) {
        let Some(inode) = self.by_path.remove(from) else {
            return;
        };
        if let Some(replaced) = self.by_path.insert(to.to_string(), inode) {
            self.by_inode.remove(&replaced);
        }
        self.by_inode.insert(inode, to.to_string());
    }
}

/// What a path was when this filesystem last served it: absent, or a node
/// of a kind and, for a stored file, its blob.
type Seen = Option<(Kind, Option<Hash>)>;

/// What this filesystem has told the guest, for [`Freshness`] and the
/// page-cache flags.
#[derive(Default)]
struct Served {
    /// Each path as it was last served.
    seen: HashMap<String, Seen>,
    /// When a change to each path was last recorded ([`Clock::now`]).
    changed: HashMap<String, Duration>,
    /// Per file inode, the blob the previous open served, when that open
    /// was read-only: what the guest's page cache may hold for it.
    pages: HashMap<u64, Hash>,
    /// Per directory inode, the listing the previous OPENDIR served.
    listings: HashMap<u64, BTreeMap<String, Kind>>,
    /// Per file inode, the size the last reply carrying its attributes
    /// (LOOKUP, GETATTR, SETATTR, READDIRPLUS, CREATE) told the guest: what
    /// the guest bounds a cached read by.
    told: HashMap<u64, u64>,
    /// Per path, the time ([`Clock::now`]) of the last change recorded for
    /// its file times: strictly increasing per path, and never zero, so
    /// every change reports a time no earlier reply carried.
    times: HashMap<String, Duration>,
}

impl Served {
    /// A change to `path` at `now`: it and its parent directory are
    /// recently changed until `now + window`, and their times move
    /// ([`Served::touch`]).
    fn record_change(&mut self, path: &str, now: Duration) {
        for changed in with_parent(path) {
            self.changed.insert(changed.to_string(), now);
        }
        self.touch(path, now);
    }

    /// A change to `path` at `now` that moves only its and its parent
    /// directory's times: each takes `now`, or the nanosecond after its
    /// previous time when that is later.
    fn touch(&mut self, path: &str, now: Duration) {
        for changed in with_parent(path) {
            let previous = self.times.get(changed).copied().unwrap_or_default();
            let after = previous + Duration::from_nanos(1);
            self.times.insert(changed.to_string(), now.max(after));
        }
    }
}

/// `path`, then its parent directory unless `path` is the root.
fn with_parent(path: &str) -> impl Iterator<Item = &str> {
    let parent = (!path.is_empty()).then(|| path.rsplit_once('/').map_or("", |(dir, _)| dir));
    std::iter::once(path).chain(parent)
}

/// A node as the store and the session hold it.
#[derive(Clone, Copy, Debug)]
struct Stat {
    kind: Kind,
    size: u64,
    /// The blob of a stored file or symlink; `None` for a directory and for
    /// a file only the session holds.
    blob: Option<Hash>,
    /// A file's git mode; `None` for a directory.
    mode: Option<Mode>,
}

/// The filesystem: see the crate documentation.
pub struct StoreFs<S> {
    store: S,
    scope: Scope,
    options: Options,
    clock: Arc<dyn Clock>,
    inodes: Mutex<Inodes>,
    handles: Mutex<HashMap<u64, Arc<Mutex<Handle>>>>,
    next_handle: AtomicU64,
    /// Never held across a store call.
    session: Mutex<Session>,
    /// Never held across a store call.
    served: Mutex<Served>,
}

impl<S: Store> StoreFs<S> {
    pub fn new(store: S, scope: Scope, options: Options) -> Self {
        Self::with_clock(store, scope, options, Arc::new(MonotonicClock::default()))
    }

    /// A filesystem whose [`Freshness`] reads `clock`.
    pub fn with_clock(store: S, scope: Scope, options: Options, clock: Arc<dyn Clock>) -> Self {
        StoreFs {
            store,
            scope,
            options,
            clock,
            served: Mutex::new(Served::default()),
            inodes: Mutex::new(Inodes {
                by_path: HashMap::from([(String::new(), ROOT_INODE)]),
                by_inode: HashMap::from([(ROOT_INODE, String::new())]),
                next: ROOT_INODE + 1,
            }),
            handles: Mutex::new(HashMap::new()),
            next_handle: AtomicU64::new(1),
            session: Mutex::new(Session::default()),
        }
    }

    /// The store this filesystem serves.
    pub fn store(&self) -> &S {
        &self.store
    }

    /// LOOKUP: the entry `name` names in the directory `parent`.
    pub fn lookup(&self, parent: u64, name: &[u8]) -> Result<Entry, Errno> {
        let path = self.child(parent, name)?;
        let entry = self.entry(&path)?;
        self.tell(&entry.attr);
        Ok(entry)
    }

    /// GETATTR. With a handle, the size is the bytes that handle serves.
    pub fn getattr(&self, inode: u64, handle: Option<u64>) -> Result<(Attr, Duration), Errno> {
        let path = self.path_of(inode)?;
        let mut attr = self.attr(&path)?;
        if let Some(open) = handle.and_then(|h| self.handle(h).ok()) {
            attr.size = open.lock().bytes.len() as u64;
        }
        self.tell(&attr);
        Ok((attr, self.timeout(&path)))
    }

    /// Record the size a reply tells the guest a file has.
    fn tell(&self, attr: &Attr) {
        if attr.kind == Kind::File {
            self.served.lock().told.insert(attr.inode, attr.size);
        }
    }

    /// The negative-entry timeout for a LOOKUP of `name` in `parent` that
    /// answered `ENOENT` ([`Freshness`]); zero when the request names no
    /// path the guest could cache.
    pub fn negative_ttl(&self, parent: u64, name: &[u8]) -> Duration {
        match self.child(parent, name) {
            Ok(path) => self.timeout(&path),
            Err(_) => Duration::ZERO,
        }
    }

    /// READDIRPLUS: [`StoreFs::readdir`]'s entries, each with the reply a
    /// LOOKUP of it answers, so the guest needs no LOOKUP per name.
    pub fn readdirplus(&self, inode: u64, offset: u64) -> Result<Vec<(DirEntry, Entry)>, Errno> {
        let dir = self.directory(inode)?;
        let children = self.query(|conn| self.children(conn, &dir))?;
        let mut out = Vec::new();
        for (i, (name, kind)) in children.into_iter().enumerate().skip(offset as usize) {
            let path = join(&dir, &name);
            // A directory's kind is all its attributes carry; asking the
            // store again would scan for the files under it.
            let (size, mode) = match kind {
                Kind::Directory => {
                    self.observe(&path, Some((Kind::Directory, None)));
                    (0, None)
                }
                Kind::File | Kind::Symlink => match self.served(&path)? {
                    Some(stat) => (stat.size, stat.mode),
                    None => continue,
                },
            };
            let attr = self.attr_of(&path, kind, size, mode);
            self.tell(&attr);
            let entry = DirEntry {
                inode: attr.inode,
                offset: i as u64 + 1,
                kind,
                name,
            };
            let ttl = self.timeout(&path);
            out.push((entry, Entry { attr, ttl }));
        }
        Ok(out)
    }

    /// OPENDIR: the guest may keep the listing it cached for the directory
    /// when the directory lists what the previous OPENDIR of it served.
    pub fn opendir(&self, inode: u64) -> Result<OpenedDir, Errno> {
        let dir = self.directory(inode)?;
        let listing = self.query(|conn| self.children(conn, &dir))?;
        let now = self.clock.now();
        let mut served = self.served.lock();
        let keep_cache = served.listings.get(&inode) == Some(&listing);
        // A listing other than the previous one: entries changed in it.
        if served.listings.insert(inode, listing).is_some() && !keep_cache {
            served.touch(&dir, now);
        }
        Ok(OpenedDir { keep_cache })
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
    /// file. A file this session created and the store does not hold yet
    /// serves its last flushed bytes, and its handle proposes a create.
    ///
    /// OPEN always reaches the store, so every open reads the current blob
    /// (fresh at open). See [`Opened`] for the page-cache flags.
    pub fn open(&self, inode: u64, flags: u32) -> Result<Opened, Errno> {
        let path = self.path_of(inode)?;
        if self.attr(&path)?.kind == Kind::Directory {
            return Err(Errno::EISDIR);
        }
        let writable = flags & open_flags::O_ACCMODE != open_flags::O_RDONLY;
        if writable && !self.scope.propose.covers(&path) {
            return Err(Errno::EACCES);
        }
        let (base, mut bytes) = self.held(&path)?.ok_or(Errno::ENOENT)?;
        let truncate = writable && flags & open_flags::O_TRUNC != 0;
        if truncate {
            bytes.clear();
        }
        let (keep_cache, stale_size) = {
            let mut served = self.served.lock();
            let keep_cache = match &base {
                Base::Blob(blob) if !writable => served.pages.insert(inode, *blob) == Some(*blob),
                _ => {
                    served.pages.remove(&inode);
                    false
                }
            };
            let told = served.told.get(&inode).copied();
            (
                keep_cache,
                told.is_some_and(|size| size != bytes.len() as u64),
            )
        };
        let handle = self.new_handle(Handle {
            path,
            base,
            bytes,
            mode: None,
            dirty: truncate,
            writable,
        });
        // The guest bounds a cached read by the size it was last told; the
        // OPEN reply carries none. Serving another size, reads bypass the
        // cache so they reach the whole blob.
        Ok(Opened {
            handle,
            keep_cache,
            direct_io: writable || stale_size,
        })
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
    /// reads as zeros. Nothing reaches the store until FLUSH or FSYNC, and
    /// a WRITE never asks it: another writer's change since the handle's
    /// base is decided at FLUSH, where the two merge (ADR-0045 T6).
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
    /// file's current blob. A mode with a write handle (`fchmod`) stages on
    /// it; without one (`chmod`) it is a proposal of the file's git mode
    /// against its current blob, or, for a file not landed yet, lands with
    /// it. A directory's or symlink's mode changes nothing. An owner is
    /// `EPERM`; times are not carried.
    pub fn setattr(
        &self,
        inode: u64,
        handle: Option<u64>,
        set: SetAttr,
    ) -> Result<(Attr, Duration), Errno> {
        let path = self.path_of(inode)?;
        if set.uid.is_some() || set.gid.is_some() {
            return Err(Errno::EPERM);
        }
        if let Some(bits) = set.mode {
            self.set_mode(&path, handle, bits)?;
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
                    let (base, mut bytes) = self.held(&path)?.ok_or(Errno::ENOENT)?;
                    bytes.resize(size as usize, 0);
                    self.land(&path, base, bytes, None)?;
                }
            }
        }
        self.getattr(inode, handle)
    }

    /// A SETATTR of the mode `bits` (ADR-0045 T4): see [`StoreFs::setattr`].
    fn set_mode(&self, path: &str, handle: Option<u64>, bits: u32) -> Result<(), Errno> {
        let mode = if bits & 0o111 != 0 {
            Mode::Executable
        } else {
            Mode::Regular
        };
        let stat = self.served(path)?.ok_or(Errno::ENOENT)?;
        if stat.kind != Kind::File {
            return Ok(());
        }
        if !self.scope.propose.covers(path) {
            return Err(Errno::EACCES);
        }
        if let Some(open) = handle.and_then(|h| self.handle(h).ok()) {
            let mut open = open.lock();
            if open.writable {
                open.mode = Some(mode);
                open.dirty = true;
                return Ok(());
            }
        }
        if stat.mode == Some(mode) {
            return Ok(());
        }
        match self.held(path)?.ok_or(Errno::ENOENT)? {
            (base @ Base::Blob(_), bytes) => {
                self.propose(vec![Proposed::write_mode(path, base, bytes, mode)])?;
            }
            _ => {
                self.session
                    .lock()
                    .pending_modes
                    .insert(path.to_string(), mode);
                self.touch(path);
            }
        }
        Ok(())
    }

    /// SYMLINK: a symbolic link `name` in `parent` whose target is
    /// `target`, proposed as a create of the target's bytes with the
    /// symlink mode (ADR-0045 T4). `EEXIST` when the name exists, `EACCES`
    /// outside the proposal scope.
    pub fn symlink(&self, parent: u64, name: &[u8], target: &[u8]) -> Result<Entry, Errno> {
        let path = self.child(parent, name)?;
        if self.kind_of(&path)?.is_some() {
            return Err(Errno::EEXIST);
        }
        if !self.scope.propose.covers(&path) {
            return Err(Errno::EACCES);
        }
        self.propose(vec![Proposed::write_mode(
            path.as_str(),
            Base::Absent,
            target.to_vec(),
            Mode::Symlink,
        )])?;
        let entry = self.entry(&path)?;
        Ok(entry)
    }

    /// READLINK: a symbolic link's target. `EINVAL` for anything else.
    pub fn readlink(&self, inode: u64) -> Result<Vec<u8>, Errno> {
        let path = self.path_of(inode)?;
        if self.attr(&path)?.kind != Kind::Symlink {
            return Err(Errno::EINVAL);
        }
        let (_, bytes) = self.held(&path)?.ok_or(Errno::ENOENT)?;
        Ok(bytes)
    }

    /// CREATE: a new file `name` in `parent`, and a handle on it whose base
    /// is "no file". The file exists for the guest at once; its first FLUSH
    /// proposes it as a create, refused `EEXIST` when another writer
    /// created the path first. `EEXIST` when the name exists, `EACCES`
    /// outside the proposal scope.
    pub fn create(&self, parent: u64, name: &[u8], flags: u32) -> Result<(Entry, u64), Errno> {
        let path = self.child(parent, name)?;
        if self.kind_of(&path)?.is_some() {
            return Err(Errno::EEXIST);
        }
        if !self.scope.propose.covers(&path) {
            return Err(Errno::EACCES);
        }
        self.session.lock().pending.insert(path.clone(), Vec::new());
        self.touch(&path);
        let handle = self.new_handle(Handle {
            path: path.clone(),
            base: Base::Absent,
            bytes: Vec::new(),
            mode: None,
            dirty: true,
            writable: flags & open_flags::O_ACCMODE != open_flags::O_RDONLY,
        });
        let entry = self.entry(&path)?;
        self.tell(&entry.attr);
        Ok((entry, handle))
    }

    /// MKDIR: a directory, proposed (ADR-0045 T4). `EEXIST` when the name
    /// exists, `EACCES` when the scope may propose no path under it.
    pub fn mkdir(&self, parent: u64, name: &[u8]) -> Result<Entry, Errno> {
        let path = self.child(parent, name)?;
        if self.kind_of(&path)?.is_some() {
            return Err(Errno::EEXIST);
        }
        if !self.may_propose_under(&path) {
            return Err(Errno::EACCES);
        }
        self.propose(vec![Proposed::mkdir(path.as_str())])?;
        self.entry(&path)
    }

    /// UNLINK: a delete proposal against the blob the file holds; a file
    /// not landed yet leaves the session. The directory it was in stays,
    /// in the store too (ADR-0045 T1), until RMDIR. `EISDIR` on a
    /// directory, `EACCES` outside the proposal scope.
    pub fn unlink(&self, parent: u64, name: &[u8]) -> Result<(), Errno> {
        let path = self.child(parent, name)?;
        match self.kind_of(&path)? {
            None => return Err(Errno::ENOENT),
            Some(Kind::Directory) => return Err(Errno::EISDIR),
            Some(Kind::File | Kind::Symlink) => {}
        }
        if !self.scope.propose.covers(&path) {
            return Err(Errno::EACCES);
        }
        match self.held(&path)? {
            Some((base @ Base::Blob(_), _)) => {
                self.propose(vec![Proposed::delete(path.as_str(), base)])?;
            }
            _ => {
                let mut session = self.session.lock();
                session.pending.remove(&path);
                session.pending_modes.remove(&path);
                drop(session);
                self.touch(&path);
            }
        }
        Ok(())
    }

    /// RMDIR: an rmdir proposal for a directory that holds nothing.
    /// `ENOTEMPTY` when a file or directory is under it, `ENOTDIR` on a
    /// file, `EACCES` when the scope may propose no path under it.
    pub fn rmdir(&self, parent: u64, name: &[u8]) -> Result<(), Errno> {
        let path = self.child(parent, name)?;
        match self.kind_of(&path)? {
            None => return Err(Errno::ENOENT),
            Some(Kind::File | Kind::Symlink) => return Err(Errno::ENOTDIR),
            Some(Kind::Directory) => {}
        }
        if !self.query(|conn| self.children(conn, &path))?.is_empty() {
            return Err(Errno::ENOTEMPTY);
        }
        if !self.may_propose_under(&path) {
            return Err(Errno::EACCES);
        }
        self.propose(vec![Proposed::rmdir(path.as_str())])?;
        Ok(())
    }

    /// RENAME. A file is moved by one proposal: the delete of the old path
    /// against the blob it holds and the create of the new one with its
    /// bytes and mode, or the replace of the file already there against
    /// that file's blob. Refused whole, so a refused rename leaves the old
    /// path in place. A file not landed yet is proposed at its new name as
    /// a create or replace: a write-temp-then-rename save is one proposal.
    /// A directory is moved by one proposal over every path under it
    /// (ADR-0045 T4). Handles open on the old paths follow the files.
    ///
    /// `EISDIR`, `ENOTDIR`, `ENOTEMPTY` and `EEXIST` (`RENAME_NOREPLACE`)
    /// as `rename(2)`; `EINVAL` for `RENAME_EXCHANGE` and for a directory
    /// moved under itself; `EACCES` outside the proposal scope.
    pub fn rename(
        &self,
        olddir: u64,
        oldname: &[u8],
        newdir: u64,
        newname: &[u8],
        flags: u32,
    ) -> Result<(), Errno> {
        if flags & !rename_flags::RENAME_NOREPLACE != 0 {
            return Err(Errno::EINVAL);
        }
        let from = self.child(olddir, oldname)?;
        let to = self.child(newdir, newname)?;
        let kind = self.kind_of(&from)?.ok_or(Errno::ENOENT)?;
        if from == to {
            return Ok(());
        }
        let target = self.kind_of(&to)?;
        if flags & rename_flags::RENAME_NOREPLACE != 0 && target.is_some() {
            return Err(Errno::EEXIST);
        }
        match kind {
            Kind::Directory => self.rename_directory(&from, &to, target)?,
            Kind::File | Kind::Symlink => self.rename_file(&from, &to, target)?,
        }
        self.inodes.lock().rename_tree(&from, &to);
        Ok(())
    }

    /// RENAME of a directory: one proposal over every path under it
    /// ([`source_write::rename_dir`]); an empty directory at `to` is
    /// replaced.
    fn rename_directory(&self, from: &str, to: &str, target: Option<Kind>) -> Result<(), Errno> {
        if to.starts_with(&join(from, "")) {
            return Err(Errno::EINVAL);
        }
        match target {
            Some(Kind::File | Kind::Symlink) => return Err(Errno::ENOTDIR),
            Some(Kind::Directory) if !self.query(|c| self.children(c, to))?.is_empty() => {
                return Err(Errno::ENOTEMPTY);
            }
            _ => {}
        }
        if !self.may_propose_under(from) || !self.may_propose_under(to) {
            return Err(Errno::EACCES);
        }
        let writes = self
            .store
            .with_read(|conn| source_write::rename_dir(conn, from, to))
            .map_err(|e| {
                log::info!("rename of '{from}' to '{to}' not planned: {e:#}");
                Errno::EIO
            })?;
        self.propose(writes)?;
        let prefix = join(from, "");
        {
            let mut session = self.session.lock();
            let pending: Vec<String> = session
                .pending
                .keys()
                .filter(|p| p.starts_with(&prefix))
                .cloned()
                .collect();
            for path in pending {
                let moved = format!("{to}{}", &path[from.len()..]);
                if let Some(bytes) = session.pending.remove(&path) {
                    session.pending.insert(moved.clone(), bytes);
                }
                if let Some(mode) = session.pending_modes.remove(&path) {
                    session.pending_modes.insert(moved, mode);
                }
            }
        }
        for open in self.handles.lock().values() {
            let mut open = open.lock();
            if open.path.starts_with(&prefix) {
                open.path = format!("{to}{}", &open.path[from.len()..]);
            }
        }
        Ok(())
    }

    /// RENAME of a file: see [`StoreFs::rename`].
    fn rename_file(&self, from: &str, to: &str, target: Option<Kind>) -> Result<(), Errno> {
        if target == Some(Kind::Directory) {
            return Err(Errno::EISDIR);
        }
        let (base, bytes) = self.held(from)?.ok_or(Errno::ENOENT)?;
        let stored = matches!(base, Base::Blob(_));
        if !self.scope.propose.covers(from) || !self.scope.propose.covers(to) {
            return Err(Errno::EACCES);
        }
        let mode = match self.served(from)?.and_then(|s| s.mode) {
            Some(mode) => mode,
            None => self
                .session
                .lock()
                .pending_modes
                .get(from)
                .copied()
                .unwrap_or(Mode::Regular),
        };
        let target_base = match self.held(to)? {
            Some((blob @ Base::Blob(_), _)) => blob,
            _ => Base::Absent,
        };
        let mut writes = Vec::with_capacity(2);
        if stored {
            writes.push(Proposed::delete(from, base));
        }
        writes.push(Proposed::write_mode(to, target_base, bytes, mode));
        let accepted = self.propose(writes)?;
        {
            let mut session = self.session.lock();
            for path in [from, to] {
                session.pending.remove(path);
                session.pending_modes.remove(path);
            }
        }
        // The proposal names `from` only when the store held it.
        if !stored {
            self.touch(from);
        }
        let moved = landed(&accepted, to)?;
        for open in self.handles.lock().values() {
            let mut open = open.lock();
            if open.path == from {
                open.path = to.to_string();
                open.base = moved.clone();
            }
        }
        Ok(())
    }

    /// FLUSH and FSYNC: propose the handle's staged bytes (and staged mode)
    /// against its base. When the store merged them with another writer's
    /// change, the handle serves the merged bytes from then on.
    fn commit(&self, handle: u64) -> Result<(), Errno> {
        let open = self.handle(handle)?;
        let mut open = open.lock();
        if !open.dirty {
            return Ok(());
        }
        let (base, merged) =
            self.land(&open.path, open.base.clone(), open.bytes.clone(), open.mode)?;
        if let Some(merged) = merged {
            open.bytes = merged;
        }
        open.base = base;
        open.mode = None;
        open.dirty = false;
        Ok(())
    }

    /// Land `bytes` (with `mode`, when set) at `path` against `base`,
    /// answering the base a later write is checked against and, when the
    /// store merged them with another writer's change, the merged bytes. A
    /// blob base is a replace. A file not landed yet ([`Base::Absent`])
    /// takes the bytes in the session (what a later open or rename of it
    /// carries) and is proposed as a create; one unlinked since it was
    /// created lands nothing.
    fn land(
        &self,
        path: &str,
        base: Base,
        bytes: Vec<u8>,
        mode: Option<Mode>,
    ) -> Result<(Base, Option<Vec<u8>>), Errno> {
        let mut mode = mode;
        if base == Base::Absent {
            let mut session = self.session.lock();
            let Some(held) = session.pending.get_mut(path) else {
                return Ok((Base::Absent, None));
            };
            held.clone_from(&bytes);
            mode = mode.or_else(|| session.pending_modes.get(path).copied());
        }
        let write = match mode {
            Some(mode) => Proposed::write_mode(path, base, bytes, mode),
            None => Proposed::write(path, base, bytes),
        };
        let accepted = self.propose(vec![write])?;
        {
            let mut session = self.session.lock();
            session.pending.remove(path);
            session.pending_modes.remove(path);
        }
        let landed = landed(&accepted, path)?;
        let merged = match (&landed, accepted.merged.contains(path)) {
            (Base::Blob(blob), true) => {
                let blob = *blob;
                Some(
                    self.query(|conn| store_tree::blob_bytes(conn, blob))?
                        .ok_or(Errno::EIO)?,
                )
            }
            _ => None,
        };
        Ok((landed, merged))
    }

    /// A proposal through the store's writer (ADR-0042), then materialized
    /// into the checkout under the same writer, as the daemon's `propose`
    /// op does.
    fn propose(&self, writes: Vec<Proposed>) -> Result<Accepted, Errno> {
        let mut paths: Vec<String> = writes.iter().map(|w| w.path.clone()).collect();
        // A created path's new parent directories are checked out too.
        let parents: BTreeSet<String> = paths
            .iter()
            .flat_map(|p| {
                store_tree::parents(p)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .collect();
        paths.extend(parents);
        let named = paths.join("', '");
        let accepted = self
            .store
            .with_write(|conn| {
                let accepted = source_write::propose(conn, &self.scope.propose, writes)?;
                if let Some(checkout) = &self.options.checkout {
                    match leyline_cli_lib::checkout::reconcile(
                        conn,
                        &checkout.dir,
                        checkout.lang_filter.as_deref(),
                        Some(&paths),
                    ) {
                        Ok(reconciled) => {
                            if let Some(problem) = reconciled.problems() {
                                log::warn!("proposal to '{named}' landed; {problem}");
                            }
                        }
                        Err(e) => log::warn!(
                            "proposal to '{named}' landed; the checkout was not brought up \
                             to it: {e:#}"
                        ),
                    }
                }
                Ok(accepted)
            })
            .map_err(|e| {
                log::info!("proposal to '{named}' not applied: {e:#}");
                errno_for(&e)
            })?;
        if let Some(problem) = &accepted.post_commit_error {
            log::warn!("proposal to '{named}' landed; after it: {problem}");
        }
        if let Err(e) = self.store.publish() {
            log::warn!("proposal to '{named}' landed; publishing it failed: {e:#}");
        }
        let now = self.clock.now();
        // What each landed path is now, as the guest will be served it.
        let mut landed: Vec<(String, Seen)> = Vec::new();
        for path in accepted.blobs.keys() {
            let stat = self.query(|conn| self.stat_path(conn, path))?;
            landed.push((path.clone(), stat.map(|s| (s.kind, s.blob))));
        }
        landed.extend(accepted.removed.iter().map(|p| (p.clone(), None)));
        landed.extend(accepted.rmdirs.iter().map(|p| (p.clone(), None)));
        landed.extend(
            accepted
                .mkdirs
                .iter()
                .map(|p| (p.clone(), Some((Kind::Directory, None)))),
        );
        let merged: Vec<u64> = accepted.merged.iter().map(|p| self.inode_of(p)).collect();
        let mut served = self.served.lock();
        for (path, seen) in landed {
            served.seen.insert(path.clone(), seen);
            served.record_change(&path, now);
        }
        // A merged file holds bytes no guest page holds: its next open
        // drops the guest's pages and, serving another size than the guest
        // was told, reads directly (ADR-0045 T6).
        for inode in merged {
            served.pages.remove(&inode);
        }
        drop(served);
        Ok(accepted)
    }

    /// The entry and attribute timeout of `path` ([`Freshness`]).
    fn timeout(&self, path: &str) -> Duration {
        let fresh = &self.options.freshness;
        let now = self.clock.now();
        let recent = self
            .served
            .lock()
            .changed
            .get(path)
            .is_some_and(|&at| now.saturating_sub(at) < fresh.window);
        if recent { fresh.changed } else { fresh.settled }
    }

    /// Serve `path` as it is now, recording a change when it was last
    /// served otherwise.
    fn observe(&self, path: &str, seen: Seen) {
        let now = self.clock.now();
        let mut served = self.served.lock();
        if let Some(before) = served.seen.insert(path.to_string(), seen)
            && before != seen
        {
            served.record_change(path, now);
        }
    }

    /// What `path` is now, as [`StoreFs::observe`] serves it.
    fn served(&self, path: &str) -> Result<Option<Stat>, Errno> {
        let stat = self.query(|conn| self.stat_path(conn, path))?;
        self.observe(path, stat.map(|s| (s.kind, s.blob)));
        Ok(stat)
    }

    fn new_handle(&self, handle: Handle) -> u64 {
        let id = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.handles.lock().insert(id, Arc::new(Mutex::new(handle)));
        id
    }

    /// What `path` holds: the stored blob, or a file this session created
    /// that the store does not hold ([`Base::Absent`]), with its bytes.
    fn held(&self, path: &str) -> Result<Option<(Base, Vec<u8>)>, Errno> {
        if let Some((hash, bytes)) = self.query(|conn| blob_of(conn, path))? {
            return Ok(Some((Base::Blob(hash), bytes)));
        }
        Ok(self
            .session
            .lock()
            .pending
            .get(path)
            .map(|bytes| (Base::Absent, bytes.clone())))
    }

    /// The path `name` names in the directory `parent`. `ENOENT` for an
    /// empty name, a name containing `/`, or non-UTF-8.
    fn child(&self, parent: u64, name: &[u8]) -> Result<String, Errno> {
        let dir = self.directory(parent)?;
        let name = std::str::from_utf8(name).map_err(|_| Errno::ENOENT)?;
        if name.is_empty() || name.contains('/') {
            return Err(Errno::ENOENT);
        }
        Ok(join(&dir, name))
    }

    fn entry(&self, path: &str) -> Result<Entry, Errno> {
        let attr = self.attr(path)?;
        Ok(Entry {
            attr,
            ttl: self.timeout(path),
        })
    }

    /// Whether the scope may propose some path under the directory `dir`.
    fn may_propose_under(&self, dir: &str) -> bool {
        match &self.scope.propose {
            Grant::Whole => true,
            Grant::Paths(paths) => {
                let prefix = join(dir, "");
                paths.iter().any(|p| p.starts_with(&prefix))
            }
        }
    }

    fn kind_of(&self, path: &str) -> Result<Option<Kind>, Errno> {
        Ok(self
            .query(|conn| self.stat_path(conn, path))?
            .map(|s| s.kind))
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
            Kind::File | Kind::Symlink => Err(Errno::ENOTDIR),
        }
    }

    /// The attributes of `path`, as [`StoreFs::observe`] serves it.
    fn attr(&self, path: &str) -> Result<Attr, Errno> {
        let stat = self.served(path)?.ok_or(Errno::ENOENT)?;
        Ok(self.attr_of(path, stat.kind, stat.size, stat.mode))
    }

    fn attr_of(&self, path: &str, kind: Kind, size: u64, mode: Option<Mode>) -> Attr {
        let perm = match kind {
            Kind::Directory if self.may_propose_under(path) => 0o755,
            Kind::Directory => 0o555,
            Kind::Symlink => 0o777,
            Kind::File => {
                let read_write = if self.scope.propose.covers(path) {
                    0o644
                } else {
                    0o444
                };
                if mode == Some(Mode::Executable) {
                    read_write | ((read_write & 0o444) >> 2)
                } else {
                    read_write
                }
            }
        };
        Attr {
            inode: self.inode_of(path),
            kind,
            size,
            perm,
            mtime: self.mtime(path),
        }
    }

    /// The time of the last change recorded for `path`, since the Unix
    /// epoch; the clock's origin when none was recorded.
    fn mtime(&self, path: &str) -> Duration {
        let since_origin = self.served.lock().times.get(path).copied();
        self.clock.origin() + since_origin.unwrap_or_default()
    }

    /// A change to `path` that only this session holds (a directory, or a
    /// file the store does not hold): its and its parent directory's times
    /// move ([`Served::touch`]); the guest made it, so its timeouts stay.
    fn touch(&self, path: &str) {
        let now = self.clock.now();
        self.served.lock().touch(path, now);
    }

    /// What `path` is for the guest: an entry of the store's manifest the
    /// scope sees (a file or symlink the scope's `see` covers; a directory
    /// with the whole tree in sight, or with a visible file under it), a
    /// file this session created and has not landed, or nothing. The root
    /// is always a directory.
    fn stat_path(&self, conn: &Connection, path: &str) -> anyhow::Result<Option<Stat>> {
        let dir = Some(Stat {
            kind: Kind::Directory,
            size: 0,
            blob: None,
            mode: None,
        });
        if path.is_empty() {
            return Ok(dir);
        }
        let row: Option<(u32, Option<Vec<u8>>, Option<i64>)> = conn
            .prepare_cached(
                "SELECT t.mode, t.blob_hash, length(b.blob_bytes) FROM _tree t \
                   LEFT JOIN source_blobs b ON b.blob_hash = t.blob_hash WHERE t.path = ?1",
            )?
            .query_row([path], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        match row {
            Some((store_tree::DIRECTORY_MODE, _, _)) => {
                if self.scope.see == Grant::Whole || !self.children(conn, path)?.is_empty() {
                    return Ok(dir);
                }
            }
            Some((git_mode, Some(hash), Some(size))) if self.scope.see.covers(path) => {
                let mode = Mode::from_git(git_mode).unwrap_or(Mode::Regular);
                return Ok(Some(Stat {
                    kind: if mode == Mode::Symlink {
                        Kind::Symlink
                    } else {
                        Kind::File
                    },
                    size: size as u64,
                    blob: Some(blob_hash(path, hash)?),
                    mode: Some(mode),
                }));
            }
            _ => {}
        }
        let session = self.session.lock();
        Ok(session.pending.get(path).map(|bytes| Stat {
            kind: Kind::File,
            size: bytes.len() as u64,
            blob: None,
            mode: Some(
                session
                    .pending_modes
                    .get(path)
                    .copied()
                    .unwrap_or(Mode::Regular),
            ),
        }))
    }

    /// The names directly under the directory `dir`: the store's entries
    /// the scope sees, and this session's files not landed yet.
    fn children(&self, conn: &Connection, dir: &str) -> anyhow::Result<BTreeMap<String, Kind>> {
        let prefix = join(dir, "");
        let mut stmt = conn.prepare_cached(
            "SELECT path, mode FROM _tree WHERE substr(path, 1, length(?1)) = ?1",
        )?;
        let whole = self.scope.see == Grant::Whole;
        let mut out = BTreeMap::new();
        for row in stmt.query_map([&prefix], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?))
        })? {
            let (path, git_mode) = row?;
            let is_dir = git_mode == store_tree::DIRECTORY_MODE;
            let rest = &path[prefix.len()..];
            match rest.split_once('/') {
                // A directory is listed with the whole tree in sight, or
                // when a file the scope sees is under it.
                Some((name, _)) => {
                    if whole || (!is_dir && self.scope.see.covers(&path)) {
                        out.insert(name.to_string(), Kind::Directory);
                    }
                }
                None if is_dir => {
                    if whole {
                        out.insert(rest.to_string(), Kind::Directory);
                    }
                }
                None => {
                    if self.scope.see.covers(&path) {
                        let kind = if Mode::from_git(git_mode) == Some(Mode::Symlink) {
                            Kind::Symlink
                        } else {
                            Kind::File
                        };
                        out.insert(rest.to_string(), kind);
                    }
                }
            }
        }
        let session = self.session.lock();
        // The session's paths are never empty and never end in `/`, so a
        // path under `prefix` is longer than it.
        for path in session.pending.keys().filter(|p| p.starts_with(&prefix)) {
            match path[prefix.len()..].split_once('/') {
                Some((name, _)) => out.insert(name.to_string(), Kind::Directory),
                None => out.insert(path[prefix.len()..].to_string(), Kind::File),
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

/// The blob the file or symlink at `path` holds and its bytes, or `None`
/// when the store holds none there.
fn blob_of(conn: &Connection, path: &str) -> anyhow::Result<Option<(Hash, Vec<u8>)>> {
    let Some(StoreEntry::File { blob, .. }) = store_tree::entry(conn, path)? else {
        return Ok(None);
    };
    Ok(store_tree::blob_bytes(conn, blob)?.map(|bytes| (blob, bytes)))
}

fn blob_hash(path: &str, hash: Vec<u8>) -> anyhow::Result<Hash> {
    let hash = <[u8; 32]>::try_from(hash)
        .map_err(|h| anyhow::anyhow!("'{path}': a {}-byte blob hash", h.len()))?;
    Ok(Hash::from_bytes(hash))
}

/// The base a later write to `path` is checked against: the blob an
/// accepted proposal wrote there.
fn landed(accepted: &Accepted, path: &str) -> Result<Base, Errno> {
    accepted
        .blobs
        .get(path)
        .copied()
        .map(Base::Blob)
        .ok_or(Errno::EIO)
}

/// The errno a proposal that was not applied answers: a create whose path
/// another writer filled first is `EEXIST`; any other stale base (a file
/// that moved, or was deleted, and did not merge) `ESTALE`; a path outside
/// the grant `EACCES`; a path no longer there `ENOENT`; a directory change
/// that does not fit the path `EEXIST`, `ENOTEMPTY`, `ENOTDIR` or `EISDIR`
/// as `mkdir(2)`, `rmdir(2)` and `open(2)` answer; a path under a file
/// `ENOTDIR`; every other refusal and every internal failure `EIO`.
pub fn errno_for(err: &anyhow::Error) -> Errno {
    match err.downcast_ref::<Refusal>() {
        Some(Refusal::Stale(moved)) if moved.iter().all(|m| m.expected.is_none()) => Errno::EEXIST,
        Some(Refusal::Stale(_) | Refusal::Conflict { .. }) => Errno::ESTALE,
        Some(Refusal::OutsideGrant { .. }) => Errno::EACCES,
        Some(Refusal::NoSuchFile { .. }) => Errno::ENOENT,
        Some(Refusal::Exists { .. }) => Errno::EEXIST,
        Some(Refusal::NotEmpty { .. }) => Errno::ENOTEMPTY,
        Some(Refusal::NotADirectory { .. } | Refusal::PathConflict { .. }) => Errno::ENOTDIR,
        Some(Refusal::IsADirectory { .. }) => Errno::EISDIR,
        _ => Errno::EIO,
    }
}
