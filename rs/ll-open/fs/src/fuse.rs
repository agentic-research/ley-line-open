use crate::graph::{Graph, MOUNT_HANDLE, Node, RefusalKind};
use fuser::{
    FUSE_ROOT_ID, FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData,
    ReplyDirectory, ReplyEntry, ReplyOpen, ReplyWrite, Request,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(1);
const BLOCK_SIZE: u32 = 512;

pub struct LeylineFuse {
    graph: Arc<dyn Graph>,
    ino_to_id: HashMap<u64, String>,
    id_to_ino: HashMap<String, u64>,
    next_ino: u64,
    /// The handle the next open gets. Every open is its own handle, so
    /// each edit is checked against the bytes its own open read; it starts
    /// past [`MOUNT_HANDLE`], which no open uses.
    next_fh: u64,
    /// The handles open for writing on each inode, which a truncate naming
    /// no handle applies to.
    writers: HashMap<u64, Vec<u64>>,
}

impl LeylineFuse {
    pub fn new(graph: Arc<dyn Graph>) -> Self {
        let mut ino_to_id = HashMap::new();
        let mut id_to_ino = HashMap::new();
        // Root inode maps to empty string (root node ID)
        ino_to_id.insert(FUSE_ROOT_ID, String::new());
        id_to_ino.insert(String::new(), FUSE_ROOT_ID);
        Self {
            graph,
            ino_to_id,
            id_to_ino,
            next_ino: FUSE_ROOT_ID + 1,
            next_fh: MOUNT_HANDLE + 1,
            writers: HashMap::new(),
        }
    }

    /// Open `ino` with `flags`: allocate the open's handle, and take the
    /// base of an open for writing (ADR-0042 P2), so a change that lands
    /// while the file is open is refused at the handle's flush.
    ///
    /// `O_TRUNC` reaches the open only when the kernel granted
    /// `FUSE_ATOMIC_O_TRUNC` (`init` asks for it); the truncate is then the
    /// new handle's own edit.
    fn open_handle(&mut self, ino: u64, flags: i32) -> Result<u64, i32> {
        let fh = self.next_fh;
        self.next_fh += 1;
        if !opens_for_writing(flags) {
            return Ok(fh);
        }
        let Some(id) = self.ino_to_id.get(&ino).cloned() else {
            return Ok(fh);
        };
        if let Err(e) = self.graph.note_base_as(fh, &id) {
            log::warn!("open({id}): taking the edit's base failed: {e:#}");
        }
        if flags & libc::O_TRUNC != 0
            && let Err(e) = self.graph.truncate_as(fh, &id)
        {
            log::warn!("open({id}): truncating failed: {e:#}");
            self.release_handle(ino, fh);
            return Err(errno_of(&e, libc::EROFS));
        }
        self.writers.entry(ino).or_default().push(fh);
        Ok(fh)
    }

    /// Truncate `ino` to empty; a refusal is the errno to answer.
    ///
    /// Through handle `fh` when the kernel names one (`ftruncate`). A
    /// truncate naming none is the edit of every handle open for writing
    /// on `ino`: a Linux kernel without `FUSE_ATOMIC_O_TRUNC` sends
    /// `open(O_TRUNC)` as OPEN, then a SETATTR that names no handle. With
    /// no handle open for writing (`truncate(2)` by path) it is complete
    /// when it returns: it lands through the write path at once, and
    /// leaves no edit behind to hide the stored bytes.
    fn truncate_handle(&self, ino: u64, fh: Option<u64>) -> Result<(), i32> {
        let Some(id) = self.ino_to_id.get(&ino) else {
            return Err(libc::ENOENT);
        };
        let open: &[u64] = match fh {
            Some(ref fh) => std::slice::from_ref(fh),
            None => self.writers.get(&ino).map_or(&[], Vec::as_slice),
        };
        if !open.is_empty() {
            for &fh in open {
                self.graph
                    .truncate_as(fh, id)
                    .map_err(|e| errno_of(&e, libc::EROFS))?;
            }
            return Ok(());
        }
        let landed = self
            .graph
            .truncate_as(MOUNT_HANDLE, id)
            .map_err(|e| errno_of(&e, libc::EROFS))
            .and_then(|()| {
                self.graph.flush_node_as(MOUNT_HANDLE, id).map_err(|e| {
                    log::warn!("truncate of {id} failed: {e:#}");
                    flush_errno(&e)
                })
            });
        // A truncate is the only edit FUSE makes as the mount handle, and
        // it is over: refused or landed, nothing of it stays.
        if let Err(e) = self.graph.release(MOUNT_HANDLE) {
            log::warn!("dropping the truncate of {id} failed: {e:#}");
        }
        landed
    }

    /// A SETATTR of the mode `bits` on node `id`: the projection stores no
    /// mode, so the mode it presents is accepted and any other is `EPERM`
    /// (ADR-0045 T8), never accepted and dropped.
    fn check_mode(&self, id: &str, bits: u32) -> Result<(), i32> {
        match self.graph.get_node(id) {
            Ok(Some(node)) if bits & 0o7777 == u32::from(node.perm()) => Ok(()),
            Ok(Some(_)) => Err(libc::EPERM),
            Ok(None) => Err(libc::ENOENT),
            Err(_) => Err(libc::EIO),
        }
    }

    /// The open `fh` of `ino` is closed: drop its edit and base.
    fn release_handle(&mut self, ino: u64, fh: u64) {
        if let Some(open) = self.writers.get_mut(&ino) {
            open.retain(|&w| w != fh);
            if open.is_empty() {
                self.writers.remove(&ino);
            }
        }
        if let Err(e) = self.graph.release(fh) {
            log::warn!("release of handle {fh} failed: {e:#}");
        }
    }

    /// Land handle `fh`'s edit to `ino`; a refusal is the errno to answer.
    fn flush_handle(&self, ino: u64, fh: u64) -> Result<(), i32> {
        let Some(id) = self.ino_to_id.get(&ino) else {
            return Err(libc::ENOENT);
        };
        self.graph.flush_node_as(fh, id).map_err(|e| {
            log::warn!("flush of {id} through handle {fh} failed: {e:#}");
            flush_errno(&e)
        })
    }

    fn ensure_inode(&mut self, id: &str) -> u64 {
        if let Some(&ino) = self.id_to_ino.get(id) {
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.ino_to_id.insert(ino, id.to_string());
        self.id_to_ino.insert(id.to_string(), ino);
        ino
    }

    fn node_to_attr(&self, ino: u64, node: &Node) -> FileAttr {
        let perm = node.perm();
        let (ftype, size) = if node.is_dir {
            (FileType::Directory, 4096)
        } else {
            (FileType::RegularFile, node.size)
        };

        let mtime = if node.mtime_nanos >= 0 {
            UNIX_EPOCH + Duration::from_nanos(node.mtime_nanos as u64)
        } else {
            UNIX_EPOCH
        };

        FileAttr {
            ino,
            size,
            blocks: size.div_ceil(BLOCK_SIZE as u64),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind: ftype,
            perm,
            nlink: if node.is_dir { 2 } else { 1 },
            // SAFETY: `libc::getuid`/`getgid` are POSIX-defined thread-safe
            // wrappers around the `getuid(2)`/`getgid(2)` syscalls. Never
            // fail, never mutate, no aliasing concerns.
            uid: unsafe { libc::getuid() },
            // SAFETY: see `libc::getuid` above.
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: BLOCK_SIZE,
            flags: 0,
        }
    }
}

impl Filesystem for LeylineFuse {
    /// Ask for `FUSE_ATOMIC_O_TRUNC`, so `open(O_TRUNC)` arrives as one
    /// OPEN carrying `O_TRUNC` and its truncate is that open's edit. A
    /// kernel that refuses sends a SETATTR naming no handle instead, which
    /// [`Self::truncate_handle`] gives to the handles open for writing.
    fn init(
        &mut self,
        _req: &Request<'_>,
        config: &mut fuser::KernelConfig,
    ) -> Result<(), libc::c_int> {
        if let Err(missing) = config.add_capabilities(fuser::consts::FUSE_ATOMIC_O_TRUNC) {
            log::info!("kernel lacks FUSE capabilities {missing:#x}: O_TRUNC arrives as SETATTR");
        }
        Ok(())
    }

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_id) = self.ino_to_id.get(&parent).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };

        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        match self.graph.lookup_child(&parent_id, name_str) {
            Ok(Some(node)) => {
                let ino = self.ensure_inode(&node.id);
                let attr = self.node_to_attr(ino, &node);
                reply.entry(&TTL, &attr, 0);
            }
            Ok(None) => reply.error(libc::ENOENT),
            Err(e) => {
                log::error!("lookup({parent_id}/{name_str}): {e}");
                reply.error(libc::EIO);
            }
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let Some(id) = self.ino_to_id.get(&ino).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };

        match self.graph.get_node(&id) {
            Ok(Some(node)) => {
                let attr = self.node_to_attr(ino, &node);
                reply.attr(&TTL, &attr);
            }
            Ok(None) => reply.error(libc::ENOENT),
            Err(e) => {
                log::error!("getattr({id}): {e}");
                reply.error(libc::EIO);
            }
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
        let Some(id) = self.ino_to_id.get(&ino).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };

        let children = match self.graph.list_children(&id) {
            Ok(c) => c,
            Err(e) => {
                log::error!("readdir({id}): {e}");
                reply.error(libc::EIO);
                return;
            }
        };

        let mut entries: Vec<(u64, FileType, String)> = Vec::new();
        // "." and ".."
        entries.push((ino, FileType::Directory, ".".into()));
        let parent_ino = if ino == FUSE_ROOT_ID {
            FUSE_ROOT_ID
        } else {
            ino // simplified — FUSE handles ".." via lookup
        };
        entries.push((parent_ino, FileType::Directory, "..".into()));

        for child in &children {
            let child_ino = self.ensure_inode(&child.id);
            let ftype = if child.is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            entries.push((child_ino, ftype, child.name.clone()));
        }

        for (i, (child_ino, ftype, name)) in entries.iter().enumerate().skip(offset as usize) {
            // reply.add returns true when buffer is full
            if reply.add(*child_ino, (i + 1) as i64, *ftype, name) {
                break;
            }
        }
        reply.ok();
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let Some(id) = self.ino_to_id.get(&ino).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };

        let mut buf = vec![0u8; size as usize];
        match self.graph.read_content_as(fh, &id, &mut buf, offset as u64) {
            Ok(n) => reply.data(&buf[..n]),
            Err(e) => {
                log::error!("read({id}): {e}");
                reply.error(libc::EIO);
            }
        }
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        match self.open_handle(ino, flags) {
            Ok(fh) => reply.opened(fh, 0),
            Err(errno) => reply.error(errno),
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: fuser::ReplyEmpty,
    ) {
        self.release_handle(ino, fh);
        reply.ok();
    }

    fn opendir(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        reply.opened(0, 0);
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let Some(id) = self.ino_to_id.get(&ino).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };

        match self.graph.write_content_as(fh, &id, data, offset as u64) {
            Ok(n) => reply.written(n as u32),
            Err(e) => reply.error(errno_of(&e, libc::EROFS)),
        }
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _lock_owner: u64,
        reply: fuser::ReplyEmpty,
    ) {
        match self.flush_handle(ino, fh) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn fsync(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _datasync: bool,
        reply: fuser::ReplyEmpty,
    ) {
        // Per-node flush first. A refused edit fails the fsync: the client
        // must not be told the write is durable when it was discarded.
        if let Err(errno) = self.flush_handle(ino, fh)
            && errno != libc::ENOENT
        {
            reply.error(errno);
            return;
        }
        // Then persist to arena (durable)
        match self.graph.flush_to_arena() {
            Ok(()) => reply.ok(),
            Err(e) => {
                log::warn!("fsync flush_to_arena failed: {e}");
                reply.error(libc::EIO);
            }
        }
    }

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let Some(id) = self.ino_to_id.get(&ino).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };

        if let Some(bits) = mode
            && let Err(errno) = self.check_mode(&id, bits)
        {
            reply.error(errno);
            return;
        }

        if let Some(0) = size
            && let Err(errno) = self.truncate_handle(ino, fh)
        {
            reply.error(errno);
            return;
        }

        // Re-fetch the node for updated attrs
        match self.graph.get_node(&id) {
            Ok(Some(node)) => {
                let attr = self.node_to_attr(ino, &node);
                reply.attr(&TTL, &attr);
            }
            Ok(None) => reply.error(libc::ENOENT),
            Err(_) => reply.error(libc::EIO),
        }
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: fuser::ReplyCreate,
    ) {
        let Some(parent_id) = self.ino_to_id.get(&parent).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };

        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        match self.graph.create_node(&parent_id, name_str, false) {
            Ok(new_id) => {
                let ino = self.ensure_inode(&new_id);
                let node = Node {
                    id: new_id,
                    name: name_str.to_string(),
                    is_dir: false,
                    size: 0,
                    mtime_nanos: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos() as i64,
                };
                let attr = self.node_to_attr(ino, &node);
                match self.open_handle(ino, libc::O_WRONLY) {
                    Ok(fh) => reply.created(&TTL, &attr, 0, fh, 0),
                    Err(errno) => reply.error(errno),
                }
            }
            Err(e) => reply.error(errno_of(&e, libc::EROFS)),
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
        let Some(parent_id) = self.ino_to_id.get(&parent).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };

        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        match self.graph.create_node(&parent_id, name_str, true) {
            Ok(new_id) => {
                let ino = self.ensure_inode(&new_id);
                let node = Node {
                    id: new_id,
                    name: name_str.to_string(),
                    is_dir: true,
                    size: 0,
                    mtime_nanos: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos() as i64,
                };
                let attr = self.node_to_attr(ino, &node);
                reply.entry(&TTL, &attr, 0);
            }
            Err(e) => reply.error(errno_of(&e, libc::EROFS)),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: fuser::ReplyEmpty) {
        let Some(parent_id) = self.ino_to_id.get(&parent).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };

        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Look up the child to get its ID, then remove it
        let child_id = match self.graph.lookup_child(&parent_id, name_str) {
            Ok(Some(node)) => node.id,
            Ok(None) => {
                reply.error(libc::ENOENT);
                return;
            }
            Err(_) => {
                reply.error(libc::EIO);
                return;
            }
        };

        match self.graph.remove_node(&child_id) {
            Ok(()) => {
                // Clean up inode maps
                if let Some(ino) = self.id_to_ino.remove(&child_id) {
                    self.ino_to_id.remove(&ino);
                }
                reply.ok();
            }
            Err(e) => reply.error(errno_of(&e, libc::EROFS)),
        }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: fuser::ReplyEmpty) {
        // Same logic as unlink for our graph model
        self.unlink(_req, parent, name, reply);
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: fuser::ReplyEmpty,
    ) {
        let Some(parent_id) = self.ino_to_id.get(&parent).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(newparent_id) = self.ino_to_id.get(&newparent).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };
        let (Some(name_str), Some(newname_str)) = (name.to_str(), newname.to_str()) else {
            reply.error(libc::EINVAL);
            return;
        };

        // Look up the source node
        let old_node = match self.graph.lookup_child(&parent_id, name_str) {
            Ok(Some(node)) => node,
            Ok(None) => {
                reply.error(libc::ENOENT);
                return;
            }
            Err(_) => {
                reply.error(libc::EIO);
                return;
            }
        };

        let old_id = old_node.id.clone();
        match self.graph.rename_node(&old_id, &newparent_id, newname_str) {
            Ok(()) => {
                // Update inode maps: remove old ID, add new ID with same ino
                if let Some(ino) = self.id_to_ino.remove(&old_id) {
                    let new_id = if newparent_id.is_empty() {
                        newname_str.to_string()
                    } else {
                        format!("{newparent_id}/{newname_str}")
                    };
                    self.ino_to_id.insert(ino, new_id.clone());
                    self.id_to_ino.insert(new_id, ino);
                }
                reply.ok();
            }
            Err(e) => reply.error(errno_of(&e, libc::EROFS)),
        }
    }
}

/// A mounted FUSE session; dropping it unmounts.
pub type FuseSession = fuser::BackgroundSession;

/// Mount the FUSE filesystem in the background. Returns a session handle
/// that unmounts when dropped — hold it for as long as the mount should
/// live.
///
/// No `auto_unmount`: on libfuse 2 that option is implemented by
/// `fusermount` adding `allow_other`, which a stock Linux refuses unless
/// `/etc/fuse.conf` sets `user_allow_other`, so every mount failed with
/// "option allow_other only allowed if 'user_allow_other' is set" before
/// serving a byte (found by `cli-lib/tests/mount_round_trip.rs`, bead
/// ley-line-open-aed167). The session's `Drop` is the unmount; a process
/// that dies without dropping it leaves the mountpoint for `fusermount -u`,
/// the same trade every unprivileged FUSE mount makes.
pub fn mount_fuse(graph: Arc<dyn Graph>, mountpoint: &Path) -> anyhow::Result<FuseSession> {
    let fs = LeylineFuse::new(graph);
    let options = vec![MountOption::FSName("leyline".into())];
    let session = fuser::spawn_mount2(fs, mountpoint, &options)?;
    Ok(session)
}

/// Whether `open(2)` flags ask for write access (`O_WRONLY` or `O_RDWR`).
fn opens_for_writing(flags: i32) -> bool {
    flags & libc::O_ACCMODE != libc::O_RDONLY
}

/// The errno a refused mount operation answers (ADR-0045 T8): its
/// [`RefusalKind`]'s, as storefs answers the same refusal, or `other` for
/// any other failure.
fn errno_of(err: &anyhow::Error, other: i32) -> i32 {
    match crate::graph::refusal_kind(err) {
        Some(RefusalKind::Stale) => libc::ESTALE,
        Some(RefusalKind::Exists) => libc::EEXIST,
        Some(RefusalKind::NotEmpty) => libc::ENOTEMPTY,
        Some(RefusalKind::NotADirectory) => libc::ENOTDIR,
        Some(RefusalKind::IsADirectory) => libc::EISDIR,
        Some(RefusalKind::NotFound) => libc::ENOENT,
        Some(RefusalKind::OutsideGrant) => libc::EACCES,
        Some(RefusalKind::Invalid) => libc::EINVAL,
        Some(RefusalKind::NotInProjection) => libc::EPERM,
        None => other,
    }
}

/// The errno a refused flush answers: `ESTALE` when the edit does not merge
/// with what the file holds now, `EIO` for a failure that is no refusal.
fn flush_errno(err: &anyhow::Error) -> i32 {
    errno_of(err, libc::EIO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::MemoryGraph;

    #[test]
    fn write_access_is_read_from_the_access_mode_bits() {
        assert!(!opens_for_writing(libc::O_RDONLY));
        assert!(opens_for_writing(libc::O_WRONLY));
        assert!(opens_for_writing(libc::O_RDWR));
        assert!(opens_for_writing(libc::O_WRONLY | libc::O_TRUNC));
        assert!(!opens_for_writing(libc::O_RDONLY | libc::O_NONBLOCK));
    }

    /// Each refusal reaches the client as its errno, wrapped in context or
    /// not; every other failure as the operation's own errno.
    #[test]
    fn each_refusal_answers_its_errno_and_others_the_operations() {
        let refused = |kind| {
            anyhow::Error::new(crate::graph::Refused {
                kind,
                path: "a.go".to_string(),
                reason: "test".to_string(),
            })
        };
        for (kind, errno) in [
            (RefusalKind::Stale, libc::ESTALE),
            (RefusalKind::Exists, libc::EEXIST),
            (RefusalKind::NotEmpty, libc::ENOTEMPTY),
            (RefusalKind::NotADirectory, libc::ENOTDIR),
            (RefusalKind::IsADirectory, libc::EISDIR),
            (RefusalKind::NotFound, libc::ENOENT),
            (RefusalKind::OutsideGrant, libc::EACCES),
            (RefusalKind::Invalid, libc::EINVAL),
            (RefusalKind::NotInProjection, libc::EPERM),
        ] {
            assert_eq!(errno_of(&refused(kind), libc::EROFS), errno, "{kind:?}");
        }
        assert_eq!(
            flush_errno(&refused(RefusalKind::Stale).context("flush")),
            libc::ESTALE
        );
        assert_eq!(flush_errno(&anyhow::anyhow!("disk full")), libc::EIO);
    }

    /// A mode change is refused `EPERM`: the projection stores none. The
    /// mode a node is presented with is accepted.
    #[test]
    fn a_mode_other_than_the_presented_one_is_refused() {
        let fs = LeylineFuse::new(test_graph());
        assert_eq!(fs.check_mode("readme", 0o100644), Ok(()));
        assert_eq!(fs.check_mode("readme", 0o100755), Err(libc::EPERM));
        assert_eq!(fs.check_mode("docs", 0o040755), Ok(()));
        assert_eq!(fs.check_mode("docs", 0o040700), Err(libc::EPERM));
        assert_eq!(fs.check_mode("nope", 0o644), Err(libc::ENOENT));
    }

    /// Each open is its own handle and edits against its own base (bead
    /// `ley-line-open-5da276`): two opens of one file get distinct handles,
    /// each reads its own edit, and once one handle's edit lands the
    /// other's flush answers `ESTALE` instead of success. Before, every open
    /// was handle 0, so the second edit took the first's file as its base
    /// and landed over bytes its writer never read.
    #[test]
    #[cfg(feature = "splice")]
    fn two_opens_edit_against_their_own_bases() {
        let db = leyline_ts::parse_with_source(
            b"<p>hello</p>",
            leyline_ts::languages::TsLanguage::Html,
            "test.html",
        )
        .unwrap();
        let mut adapter = crate::graph::SqliteGraphAdapter::new_writable(&db).unwrap();
        adapter.set_source_writer(Arc::new(crate::graph::fixtures::CheckingWriter));
        let mut fs = LeylineFuse::new(Arc::new(adapter));
        let ino = fs.ensure_inode("test.html/element/text");
        let read = |fs: &LeylineFuse, fh: u64| {
            let mut buf = [0u8; 64];
            let n = fs
                .graph
                .read_content_as(fh, "test.html/element/text", &mut buf, 0)
                .unwrap();
            buf[..n].to_vec()
        };

        let a = fs.open_handle(ino, libc::O_RDWR).unwrap();
        let b = fs.open_handle(ino, libc::O_WRONLY).unwrap();
        assert_ne!(a, b, "each open is its own handle");
        assert!(a != MOUNT_HANDLE && b != MOUNT_HANDLE);
        fs.graph
            .write_content_as(a, "test.html/element/text", b"world", 0)
            .unwrap();
        assert_eq!(read(&fs, a), b"world");
        assert_eq!(read(&fs, b), b"hello");
        assert_eq!(fs.flush_handle(ino, a), Ok(()));
        fs.graph
            .write_content_as(b, "test.html/element/text", b"there", 0)
            .unwrap();
        assert_eq!(fs.flush_handle(ino, b), Err(libc::ESTALE));
        assert_eq!(read(&fs, MOUNT_HANDLE), b"world");
        assert_eq!(fs.flush_handle(ino + 1, a), Err(libc::ENOENT));
    }

    #[cfg(feature = "splice")]
    const LEAF: &str = "test.html/element/text";

    /// A mount over `<p>hello</p>` whose write path checks bases, and the
    /// `<p>`'s text leaf.
    #[cfg(feature = "splice")]
    fn html_fuse() -> (LeylineFuse, u64) {
        let db = leyline_ts::parse_with_source(
            b"<p>hello</p>",
            leyline_ts::languages::TsLanguage::Html,
            "test.html",
        )
        .unwrap();
        let mut adapter = crate::graph::SqliteGraphAdapter::new_writable(&db).unwrap();
        adapter.set_source_writer(Arc::new(crate::graph::fixtures::CheckingWriter));
        let mut fs = LeylineFuse::new(Arc::new(adapter));
        let ino = fs.ensure_inode(LEAF);
        (fs, ino)
    }

    /// What a read through `fh` returns, and the size a stat reports.
    #[cfg(feature = "splice")]
    fn seen(fs: &LeylineFuse, fh: u64) -> (Vec<u8>, u64) {
        let mut buf = [0u8; 64];
        let n = fs.graph.read_content_as(fh, LEAF, &mut buf, 0).unwrap();
        let size = fs.graph.get_node(LEAF).unwrap().map_or(0, |n| n.size);
        (buf[..n].to_vec(), size)
    }

    /// `open(O_TRUNC)`, write, fsync, close, as a kernel that hands
    /// `O_TRUNC` to the open (`FUSE_ATOMIC_O_TRUNC`, which `init` asks
    /// for) sends it: the open truncates its own handle's edit, and the
    /// write lands alone and reads back. Before, the open ignored
    /// `O_TRUNC` and the write landed over the old text (`hillo`).
    #[test]
    #[cfg(feature = "splice")]
    fn an_open_with_o_trunc_truncates_its_own_edit() {
        let (mut fs, ino) = html_fuse();
        let fh = fs.open_handle(ino, libc::O_WRONLY | libc::O_TRUNC).unwrap();
        fs.graph.write_content_as(fh, LEAF, b"hi", 0).unwrap();
        assert_eq!(fs.flush_handle(ino, fh), Ok(()));
        fs.release_handle(ino, fh);
        assert_eq!(seen(&fs, MOUNT_HANDLE), (b"hi".to_vec(), 2));
    }

    /// The same, as a Linux kernel without `FUSE_ATOMIC_O_TRUNC` sends it:
    /// OPEN, then SETATTR(size 0) naming no handle, then WRITE, FSYNC,
    /// RELEASE. The truncate is the open writer's, so the write lands alone
    /// and reads back. Before, the truncate became an edit of the mount
    /// handle that nothing flushed: the write landed over the old text and
    /// a stat reported size 0, so the read-back was empty (PR #453's Linux
    /// `mount_round_trip` failure).
    #[test]
    #[cfg(feature = "splice")]
    fn a_truncate_naming_no_handle_after_an_open_is_the_open_writers() {
        let (mut fs, ino) = html_fuse();
        let fh = fs.open_handle(ino, libc::O_WRONLY).unwrap();
        assert_eq!(fs.truncate_handle(ino, None), Ok(()));
        fs.graph.write_content_as(fh, LEAF, b"hi", 0).unwrap();
        assert_eq!(fs.flush_handle(ino, fh), Ok(()));
        fs.release_handle(ino, fh);
        assert_eq!(seen(&fs, MOUNT_HANDLE), (b"hi".to_vec(), 2));
    }

    /// A truncate by path with no file open is complete when it returns:
    /// it lands, every reader sees it, and no edit is left behind to hide
    /// the stored bytes. Before, it stayed an unflushed edit of the mount
    /// handle: other handles read `hello` while a stat reported size 0.
    #[test]
    #[cfg(feature = "splice")]
    fn a_truncate_by_path_lands_at_once() {
        let (fs, ino) = html_fuse();
        assert_eq!(fs.truncate_handle(ino, None), Ok(()));
        assert_eq!(seen(&fs, 7), (Vec::new(), 0));
        assert_eq!(seen(&fs, MOUNT_HANDLE), (Vec::new(), 0));
    }

    fn test_graph() -> Arc<dyn Graph> {
        let mut g = MemoryGraph::new();
        g.add_node(
            Node {
                id: "".into(),
                name: "".into(),
                is_dir: true,
                size: 0,
                mtime_nanos: 0,
            },
            "",
            None,
        );
        g.add_node(
            Node {
                id: "readme".into(),
                name: "readme".into(),
                is_dir: false,
                size: 11,
                mtime_nanos: 1_000_000_000,
            },
            "",
            Some(b"hello world".to_vec()),
        );
        g.add_node(
            Node {
                id: "docs".into(),
                name: "docs".into(),
                is_dir: true,
                size: 0,
                mtime_nanos: 2_000_000_000,
            },
            "",
            None,
        );
        Arc::new(g)
    }

    #[test]
    fn inode_allocation() {
        let graph = test_graph();
        let mut fs = LeylineFuse::new(graph);

        // Root is pre-allocated
        assert_eq!(fs.ino_to_id.get(&FUSE_ROOT_ID), Some(&String::new()));
        assert_eq!(fs.id_to_ino.get(""), Some(&FUSE_ROOT_ID));

        // Lazy allocation
        let ino1 = fs.ensure_inode("readme");
        assert_eq!(ino1, 2);
        let ino2 = fs.ensure_inode("docs");
        assert_eq!(ino2, 3);

        // Idempotent
        assert_eq!(fs.ensure_inode("readme"), 2);
    }

    #[test]
    fn node_to_attr_file() {
        let graph = test_graph();
        let fs = LeylineFuse::new(graph);

        let node = Node {
            id: "f".into(),
            name: "f".into(),
            is_dir: false,
            size: 100,
            mtime_nanos: 1_000_000_000,
        };
        let attr = fs.node_to_attr(5, &node);
        assert_eq!(attr.ino, 5);
        assert_eq!(attr.size, 100);
        assert_eq!(attr.kind, FileType::RegularFile);
        assert_eq!(attr.perm, 0o644);
        assert_eq!(attr.nlink, 1);
    }

    #[test]
    fn node_to_attr_dir() {
        let graph = test_graph();
        let fs = LeylineFuse::new(graph);

        let node = Node {
            id: "d".into(),
            name: "d".into(),
            is_dir: true,
            size: 0,
            mtime_nanos: 0,
        };
        let attr = fs.node_to_attr(7, &node);
        assert_eq!(attr.ino, 7);
        assert_eq!(attr.size, 4096);
        assert_eq!(attr.kind, FileType::Directory);
        assert_eq!(attr.perm, 0o755);
        assert_eq!(attr.nlink, 2);
    }
}
