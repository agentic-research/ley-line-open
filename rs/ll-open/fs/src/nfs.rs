use crate::graph::{Graph, Node, RefusalKind};
use async_trait::async_trait;
use nfsserve::nfs::*;
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ROOT_FILEID: fileid3 = 1;

/// How long a truncate must stand with no write after it before it is
/// committed (spliced into its file through the write path).
pub const COMMIT_IDLE: Duration = Duration::from_millis(200);

/// How often the server checks for a standing truncate.
const COMMIT_TICK: Duration = Duration::from_millis(50);

/// The status a refused mount operation answers (ADR-0045 T8): its
/// [`RefusalKind`]'s, as storefs answers the same refusal, or `other` for
/// any other failure.
fn refusal_status(err: &anyhow::Error, other: nfsstat3) -> nfsstat3 {
    match crate::graph::refusal_kind(err) {
        Some(RefusalKind::Stale) => nfsstat3::NFS3ERR_STALE,
        Some(RefusalKind::Exists) => nfsstat3::NFS3ERR_EXIST,
        Some(RefusalKind::NotEmpty) => nfsstat3::NFS3ERR_NOTEMPTY,
        Some(RefusalKind::NotADirectory) => nfsstat3::NFS3ERR_NOTDIR,
        Some(RefusalKind::IsADirectory) => nfsstat3::NFS3ERR_ISDIR,
        Some(RefusalKind::NotFound) => nfsstat3::NFS3ERR_NOENT,
        Some(RefusalKind::OutsideGrant) => nfsstat3::NFS3ERR_ACCES,
        Some(RefusalKind::Invalid) => nfsstat3::NFS3ERR_INVAL,
        Some(RefusalKind::NotInProjection) => nfsstat3::NFS3ERR_PERM,
        None => other,
    }
}

/// Changes received but not yet committed. A client writes a file as
/// SETATTR(size), one UNSTABLE WRITE per chunk, then COMMIT at fsync or
/// close. Splicing each chunk as it arrived published every torn
/// intermediate state of a multi-chunk write and refused chunks whose
/// partial bytes did not parse, so a change stages its bytes in the node's
/// record (reads of that node already see them) and names the node here.
///
/// A written node is committed by the client's COMMIT for it. A truncated
/// node with no write after it has no COMMIT coming — `: > f` sends SETATTR
/// alone — so it is committed once it has stood for [`COMMIT_IDLE`]; a
/// write after the truncate makes the COMMIT its commit. Both are committed
/// before any operation that changes the tree's shape, and when the server
/// is dropped.
///
/// NFSv3 has no opens and names no client on a WRITE, so every change is
/// the graph's [`MOUNT_HANDLE`](crate::graph::MOUNT_HANDLE) edit (bead
/// `ley-line-open-5da276`), checked against one base per file: the hash of
/// the file's stored bytes the client last had from the server (bead
/// `ley-line-open-6116c9`). A client holds UNSTABLE writes until fsync or
/// close, so the server can first see a WRITE after another writer moved
/// the file; the base is taken no later than the READ:
///
/// - a READ of a node of the file, with no edit to the file staged,
///   re-takes the base ([`Graph::note_read`]) before the bytes are served;
/// - a READ while an edit to the file is staged leaves the base the edit
///   was made against;
/// - a landed COMMIT sets the base to the bytes it wrote, and a refused
///   one keeps it, so a retry from the client's cache with no READ in
///   between is refused again;
/// - a WRITE or truncate with no base yet takes one, from the stored bytes.
///
/// A COMMIT proposes the edit made against that base, which the store
/// merges with whatever landed since — a socket proposal, another mount, a
/// batch splice, the COMMIT of another node of the same file — as storefs's
/// FLUSH is merged (ADR-0045 T5, T8). A WRITE or truncate stages only an
/// edit that, merged with what the store holds at that WRITE, lands: one
/// that overlaps a change landed since its base, or whose node's place in
/// the bytes the client read is not known (the file moved before the
/// client touched the node), is refused `NFS3ERR_STALE` before it is
/// staged. That refusal is the one a macOS client reports (`ESTALE` at
/// fsync or close); it ignores a failed COMMIT. A COMMIT refuses only what
/// changed between its last WRITE and itself: an overlapping change landed
/// in that window. Its edit is discarded and the reason recorded in
/// `_errors` for the node, and the client's next READ serves the store's
/// bytes; that window is what a macOS client is not told of at `close`. Not
/// refused: an edit from a cache the client filled with no READ to this
/// server and no landed edit through it (a previous mount or server
/// process), whose base is taken at its WRITE; and, the base being per
/// file, an edit to one node from a cache older than the client's latest
/// READ of another node of the file. The mount holds one base per file it
/// has served, replaced in place. Two clients editing one node share its
/// staged bytes, since nothing on the wire tells them apart.
pub struct Staged {
    graph: Arc<dyn Graph>,
    nodes: Mutex<HashMap<String, Stage>>,
}

/// What a staged node is waiting for.
#[derive(Clone, Copy)]
enum Stage {
    /// Written: the client's COMMIT.
    Written,
    /// Truncated at this instant, with no write since: the idle window.
    Truncated(Instant),
}

impl Staged {
    fn new(graph: Arc<dyn Graph>) -> Self {
        Self {
            graph,
            nodes: Mutex::new(HashMap::new()),
        }
    }

    /// A truncate of a node already written waits, with the write, for its
    /// COMMIT.
    fn stage_truncate(&self, id: &str, now: Instant) {
        self.nodes
            .lock()
            .entry(id.to_string())
            .or_insert(Stage::Truncated(now));
    }

    fn stage_write(&self, id: &str) {
        self.nodes.lock().insert(id.to_string(), Stage::Written);
    }

    /// Commit one node through the graph's flush (the one write path), if
    /// it has anything staged. A refused flush is returned: the node's
    /// bytes stay in its record, unapplied to the source.
    fn commit_node(&self, id: &str) -> anyhow::Result<()> {
        let staged = self.nodes.lock().remove(id).is_some();
        if staged {
            self.graph.flush_node(id)?;
        }
        Ok(())
    }

    /// Commit every staged node. One whose staged bytes do not make a valid
    /// file is logged and left with its change unapplied to the source.
    pub fn commit(&self) {
        let ids: Vec<String> = self.nodes.lock().drain().map(|(id, _)| id).collect();
        for id in ids {
            if let Err(e) = self.graph.flush_node(&id) {
                log::warn!("NFS commit of {id} refused: {e:#}");
            }
        }
    }

    /// Commit every truncate that has stood for `idle` before `now`.
    /// Returns whether it committed any.
    pub fn commit_if_idle(&self, now: Instant, idle: Duration) -> bool {
        let due: Vec<String> = {
            let mut nodes = self.nodes.lock();
            let due: Vec<String> = nodes
                .iter()
                .filter(|(_, stage)| {
                    matches!(stage, Stage::Truncated(t) if now.duration_since(*t) >= idle)
                })
                .map(|(id, _)| id.clone())
                .collect();
            for id in &due {
                nodes.remove(id);
            }
            due
        };
        for id in &due {
            if let Err(e) = self.graph.flush_node(id) {
                log::warn!("NFS commit of {id} refused: {e:#}");
            }
        }
        !due.is_empty()
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        self.commit();
    }
}

pub struct LeylineNfs {
    graph: Arc<dyn Graph>,
    staged: Arc<Staged>,
    ino_to_id: Mutex<HashMap<u64, String>>,
    id_to_ino: Mutex<HashMap<String, u64>>,
    next_ino: Mutex<u64>,
}

impl LeylineNfs {
    pub fn new(graph: Arc<dyn Graph>) -> Self {
        let mut ino_to_id = HashMap::new();
        let mut id_to_ino = HashMap::new();
        ino_to_id.insert(ROOT_FILEID, String::new());
        id_to_ino.insert(String::new(), ROOT_FILEID);
        Self {
            staged: Arc::new(Staged::new(graph.clone())),
            graph,
            ino_to_id: Mutex::new(ino_to_id),
            id_to_ino: Mutex::new(id_to_ino),
            next_ino: Mutex::new(ROOT_FILEID + 1),
        }
    }

    /// The server's staged writes, shared with its idle committer.
    pub fn staged(&self) -> Arc<Staged> {
        self.staged.clone()
    }

    fn ensure_ino(&self, id: &str) -> u64 {
        let mut id_map = self.id_to_ino.lock();
        if let Some(&ino) = id_map.get(id) {
            return ino;
        }
        let mut next = self.next_ino.lock();
        let ino = *next;
        *next += 1;
        id_map.insert(id.to_string(), ino);
        self.ino_to_id.lock().insert(ino, id.to_string());
        ino
    }

    fn resolve_id(&self, ino: fileid3) -> Result<String, nfsstat3> {
        self.ino_to_id
            .lock()
            .get(&ino)
            .cloned()
            .ok_or(nfsstat3::NFS3ERR_STALE)
    }

    fn node_to_fattr3(&self, ino: fileid3, node: &Node) -> fattr3 {
        let mode = u32::from(node.perm());
        let (ftype, size) = if node.is_dir {
            (ftype3::NF3DIR, 4096u64)
        } else {
            (ftype3::NF3REG, node.size)
        };

        // An NFSv3 client reuses a cached listing while the directory's mtime
        // is unchanged, so a directory whose stored mtime does not move with
        // its entries — the synthetic root (mtime 0), a structural directory
        // — reports the current time, and the client always relists it. A
        // directory whose mtime does move with them — a source file's entry —
        // reports its own, which is the file's mtime (bead ca51fa). Reporting
        // the current time for those too hid the file's mtime from every
        // macOS mount. A failed check takes the moving mtime: a stale listing
        // is worse than a wrong timestamp.
        let moving = node.is_dir
            && !self
                .graph
                .mtime_tracks_entries(&node.id)
                .unwrap_or_else(|e| {
                    log::warn!("mtime check for {}: {e:#}", node.id);
                    false
                });
        let mtime = if moving {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            nfstime3 {
                seconds: now.as_secs() as u32,
                nseconds: now.subsec_nanos(),
            }
        } else if node.mtime_nanos >= 0 {
            nfstime3 {
                seconds: (node.mtime_nanos / 1_000_000_000) as u32,
                nseconds: (node.mtime_nanos % 1_000_000_000) as u32,
            }
        } else {
            nfstime3::default()
        };

        fattr3 {
            ftype,
            mode,
            nlink: if node.is_dir { 2 } else { 1 },
            // SAFETY: POSIX-defined thread-safe syscall wrappers; never fail,
            // never mutate, no aliasing concerns. Same shape as `fs::fuse`.
            uid: unsafe { libc::getuid() },
            // SAFETY: see `libc::getuid` above.
            gid: unsafe { libc::getgid() },
            size,
            used: size,
            rdev: specdata3::default(),
            fsid: 0,
            fileid: ino,
            atime: mtime,
            mtime,
            ctime: mtime,
        }
    }

    fn now_fattr3(&self, ino: fileid3, is_dir: bool) -> fattr3 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let mtime = nfstime3 {
            seconds: now.as_secs() as u32,
            nseconds: now.subsec_nanos(),
        };
        let (ftype, mode, size) = if is_dir {
            (ftype3::NF3DIR, 0o755, 4096u64)
        } else {
            (ftype3::NF3REG, 0o644, 0u64)
        };
        fattr3 {
            ftype,
            mode,
            nlink: if is_dir { 2 } else { 1 },
            // SAFETY: POSIX-defined thread-safe syscall wrapper; see above.
            uid: unsafe { libc::getuid() },
            // SAFETY: POSIX-defined thread-safe syscall wrapper; see above.
            gid: unsafe { libc::getgid() },
            size,
            used: size,
            rdev: specdata3::default(),
            fsid: 0,
            fileid: ino,
            atime: mtime,
            mtime,
            ctime: mtime,
        }
    }
}

#[async_trait]
impl NFSFileSystem for LeylineNfs {
    fn root_dir(&self) -> fileid3 {
        ROOT_FILEID
    }

    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let parent_id = self.resolve_id(dirid)?;
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        // Handle "." and ".."
        if name == "." {
            return Ok(dirid);
        }
        if name == ".." {
            // For simplicity, find parent by stripping last component from parent_id
            if parent_id.is_empty() {
                return Ok(ROOT_FILEID);
            }
            let parent_of = match parent_id.rfind('/') {
                Some(pos) => &parent_id[..pos],
                None => "",
            };
            return Ok(self.ensure_ino(parent_of));
        }

        let node = self
            .graph
            .lookup_child(&parent_id, name)
            .map_err(|_| nfsstat3::NFS3ERR_IO)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;
        Ok(self.ensure_ino(&node.id))
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let node_id = self.resolve_id(id)?;
        let node = self
            .graph
            .get_node(&node_id)
            .map_err(|_| nfsstat3::NFS3ERR_IO)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;
        Ok(self.node_to_fattr3(id, &node))
    }

    /// A size change truncates to that size and is staged like a write (it
    /// is the first RPC of a truncating write). The projection stores no
    /// mode: the mode a node is presented with is accepted, and any other
    /// is `NFS3ERR_PERM` (ADR-0045 T8). Owner and times are derived from the
    /// projection, not stored, so changes to them have no effect.
    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let node_id = self.resolve_id(id)?;

        if let set_mode3::mode(bits) = setattr.mode {
            let node = self
                .graph
                .get_node(&node_id)
                .map_err(|_| nfsstat3::NFS3ERR_IO)?
                .ok_or(nfsstat3::NFS3ERR_NOENT)?;
            if bits & 0o7777 != u32::from(node.perm()) {
                return Err(nfsstat3::NFS3ERR_PERM);
            }
        }

        if let set_size3::size(size) = setattr.size {
            let keep = usize::try_from(size).map_err(|_| nfsstat3::NFS3ERR_FBIG)?;
            let mut kept = vec![0u8; keep];
            let n = if keep == 0 {
                0
            } else {
                self.graph
                    .read_content(&node_id, &mut kept, 0)
                    .map_err(|_| nfsstat3::NFS3ERR_IO)?
            };
            // Growing zero-fills, as POSIX truncate does.
            kept[n..].fill(0);
            self.graph
                .truncate(&node_id)
                .map_err(|e| refusal_status(&e, nfsstat3::NFS3ERR_ROFS))?;
            if keep > 0 {
                self.graph
                    .write_content(&node_id, &kept, 0)
                    .map_err(|e| refusal_status(&e, nfsstat3::NFS3ERR_IO))?;
            }
            self.staged.stage_truncate(&node_id, Instant::now());
        }

        // Re-fetch updated attrs
        let node = self
            .graph
            .get_node(&node_id)
            .map_err(|_| nfsstat3::NFS3ERR_IO)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;
        Ok(self.node_to_fattr3(id, &node))
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let node_id = self.resolve_id(id)?;
        // The base is taken before the bytes are read: a change landing in
        // between leaves it older than what the client gets, so the edit is
        // refused rather than landed over a change it did not read. A base
        // that cannot be taken fails the READ: serving bytes no base covers
        // would let the client's edit of them land unchecked.
        self.graph.note_read(&node_id).map_err(|e| {
            log::warn!("NFS read of {node_id}: base not taken: {e:#}");
            nfsstat3::NFS3ERR_IO
        })?;
        let mut buf = vec![0u8; count as usize];
        let n = self
            .graph
            .read_content(&node_id, &mut buf, offset)
            .map_err(|_| nfsstat3::NFS3ERR_IO)?;
        buf.truncate(n);
        let eof = n < count as usize;
        Ok((buf, eof))
    }

    /// An UNSTABLE write: the server's WRITE calls [`Self::write_stable`].
    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        Ok(self.write_stable(id, offset, data, false).await?.0)
    }

    /// Stage the chunk. An UNSTABLE write is answered UNSTABLE and waits for
    /// the client's COMMIT; a FILE_SYNC or DATA_SYNC write is committed
    /// before it is answered. A refused commit fails the write. A write
    /// that, merged with what the store holds now, does not land is
    /// refused `NFS3ERR_STALE` before it is staged: macOS reports a failed
    /// WRITE at fsync, and ignores a failed COMMIT (bead
    /// `ley-line-open-6116c9`, ADR-0045 T8).
    async fn write_stable(
        &self,
        id: fileid3,
        offset: u64,
        data: &[u8],
        stable: bool,
    ) -> Result<(fattr3, bool), nfsstat3> {
        let node_id = self.resolve_id(id)?;
        self.graph
            .write_content(&node_id, data, offset)
            .map_err(|e| {
                log::debug!("NFS write to {node_id} refused: {e:#}");
                refusal_status(&e, nfsstat3::NFS3ERR_IO)
            })?;
        self.staged.stage_write(&node_id);
        if stable {
            self.commit(id, 0, 0).await?;
        }

        let node = self
            .graph
            .get_node(&node_id)
            .map_err(|_| nfsstat3::NFS3ERR_IO)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;
        Ok((self.node_to_fattr3(id, &node), stable))
    }

    /// The client's COMMIT (at fsync or close): splice the node's staged
    /// bytes into its file. The whole node is committed whatever range is
    /// named, since a file is spliced whole. A refused splice fails the
    /// COMMIT, so the fsync or close reports it: `NFS3ERR_STALE` (`ESTALE`
    /// at the client) when the file moved since the edit's base was taken,
    /// `NFS3ERR_IO` otherwise.
    async fn commit(&self, id: fileid3, _offset: u64, _count: u32) -> Result<(), nfsstat3> {
        let node_id = self.resolve_id(id)?;
        self.staged.commit_node(&node_id).map_err(|e| {
            log::warn!("NFS commit of {node_id} refused: {e:#}");
            refusal_status(&e, nfsstat3::NFS3ERR_IO)
        })
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        _attr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        self.staged.commit();
        let parent_id = self.resolve_id(dirid)?;
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        let new_id = self
            .graph
            .create_node(&parent_id, name, false)
            .map_err(|e| refusal_status(&e, nfsstat3::NFS3ERR_ROFS))?;
        let ino = self.ensure_ino(&new_id);
        Ok((ino, self.now_fattr3(ino, false)))
    }

    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<fileid3, nfsstat3> {
        self.staged.commit();
        let parent_id = self.resolve_id(dirid)?;
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        // If it already exists, return existing
        if let Ok(Some(node)) = self.graph.lookup_child(&parent_id, name) {
            return Ok(self.ensure_ino(&node.id));
        }

        let new_id = self
            .graph
            .create_node(&parent_id, name, false)
            .map_err(|e| refusal_status(&e, nfsstat3::NFS3ERR_ROFS))?;
        Ok(self.ensure_ino(&new_id))
    }

    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        self.staged.commit();
        let parent_id = self.resolve_id(dirid)?;
        let name = std::str::from_utf8(dirname).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        let new_id = self
            .graph
            .create_node(&parent_id, name, true)
            .map_err(|e| refusal_status(&e, nfsstat3::NFS3ERR_ROFS))?;
        let ino = self.ensure_ino(&new_id);
        Ok((ino, self.now_fattr3(ino, true)))
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        self.staged.commit();
        let parent_id = self.resolve_id(dirid)?;
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        let node = self
            .graph
            .lookup_child(&parent_id, name)
            .map_err(|_| nfsstat3::NFS3ERR_IO)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        self.graph
            .remove_node(&node.id)
            .map_err(|e| refusal_status(&e, nfsstat3::NFS3ERR_ROFS))?;

        // Clean up inode maps
        let mut id_map = self.id_to_ino.lock();
        if let Some(ino) = id_map.remove(&node.id) {
            self.ino_to_id.lock().remove(&ino);
        }
        Ok(())
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        self.staged.commit();
        let from_parent = self.resolve_id(from_dirid)?;
        let to_parent = self.resolve_id(to_dirid)?;
        let from_name = std::str::from_utf8(from_filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let to_name = std::str::from_utf8(to_filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        let node = self
            .graph
            .lookup_child(&from_parent, from_name)
            .map_err(|_| nfsstat3::NFS3ERR_IO)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        let old_id = node.id.clone();
        self.graph
            .rename_node(&old_id, &to_parent, to_name)
            .map_err(|e| refusal_status(&e, nfsstat3::NFS3ERR_ROFS))?;

        // Update inode maps
        let new_id = if to_parent.is_empty() {
            to_name.to_string()
        } else {
            format!("{to_parent}/{to_name}")
        };
        let mut id_map = self.id_to_ino.lock();
        if let Some(ino) = id_map.remove(&old_id) {
            id_map.insert(new_id.clone(), ino);
            self.ino_to_id.lock().insert(ino, new_id);
        }
        Ok(())
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let parent_id = self.resolve_id(dirid)?;
        let children = self
            .graph
            .list_children(&parent_id)
            .map_err(|_| nfsstat3::NFS3ERR_IO)?;

        let mut entries = Vec::new();
        let mut skip = start_after != 0;

        for child in &children {
            let child_ino = self.ensure_ino(&child.id);

            if skip {
                if child_ino == start_after {
                    skip = false;
                }
                continue;
            }

            entries.push(DirEntry {
                fileid: child_ino,
                name: child.name.as_bytes().into(),
                attr: self.node_to_fattr3(child_ino, child),
            });

            if entries.len() >= max_entries {
                return Ok(ReadDirResult {
                    entries,
                    end: false,
                });
            }
        }

        Ok(ReadDirResult { entries, end: true })
    }

    async fn symlink(
        &self,
        _dirid: fileid3,
        _linkname: &filename3,
        _symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn readlink(&self, _id: fileid3) -> Result<nfspath3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }
}

/// A running NFS server. Dropping it stops the server and commits every
/// staged write before the drop returns, so whoever drops it — the mount
/// guard, after unmounting — can publish knowing the last write is in.
/// Before, the server task was detached: unmount returned with a write
/// still inside the idle window, the daemon's final snapshot missed it, and
/// it committed whenever the runtime happened to drop the task.
pub struct NfsServer {
    port: u16,
    staged: Arc<Staged>,
    task: tokio::task::JoinHandle<()>,
}

impl NfsServer {
    /// The port the server bound (the one asked for, or the one picked for 0).
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for NfsServer {
    fn drop(&mut self) {
        self.task.abort();
        self.staged.commit();
    }
}

/// Start an NFS server on the given address, serving the provided graph.
/// The server runs in the background until the returned [`NfsServer`] is
/// dropped.
pub async fn serve_nfs(graph: Arc<dyn Graph>, listen_addr: &str) -> anyhow::Result<NfsServer> {
    let fs = LeylineNfs::new(graph);
    let staged = fs.staged();
    let listener = NFSTcpListener::bind(listen_addr, fs).await?;
    let port = listener.get_listen_port();
    let idle = staged.clone();
    let task = tokio::spawn(async move {
        // The idle committer runs beside the server and ends with it.
        let committer = async {
            let mut tick = tokio::time::interval(COMMIT_TICK);
            loop {
                tick.tick().await;
                idle.commit_if_idle(Instant::now(), COMMIT_IDLE);
            }
        };
        tokio::select! {
            served = listener.handle_forever() => {
                if let Err(e) = served {
                    log::error!("NFS server error: {e}");
                }
            }
            () = committer => {}
        }
    });
    Ok(NfsServer { port, staged, task })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::MemoryGraph;

    fn test_nfs() -> LeylineNfs {
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
        g.add_node(
            Node {
                id: "docs/guide".into(),
                name: "guide".into(),
                is_dir: false,
                size: 4,
                mtime_nanos: 3_000_000_000,
            },
            "docs",
            Some(b"text".to_vec()),
        );
        LeylineNfs::new(Arc::new(g))
    }

    #[test]
    fn inode_allocation() {
        let nfs = test_nfs();
        // Root is pre-allocated
        assert_eq!(nfs.resolve_id(ROOT_FILEID).unwrap(), "");

        // Lazy allocation
        let ino1 = nfs.ensure_ino("readme");
        assert_eq!(ino1, 2);
        let ino2 = nfs.ensure_ino("docs");
        assert_eq!(ino2, 3);

        // Idempotent
        assert_eq!(nfs.ensure_ino("readme"), 2);
    }

    #[tokio::test]
    async fn nfs_lookup_and_getattr() {
        let nfs = test_nfs();

        // Lookup readme under root
        let ino = nfs
            .lookup(ROOT_FILEID, &b"readme".to_vec().into())
            .await
            .unwrap();
        assert!(ino > ROOT_FILEID);

        // getattr
        let attr = nfs.getattr(ino).await.unwrap();
        assert_eq!(attr.size, 11);
        assert!(matches!(attr.ftype, ftype3::NF3REG));
        assert_eq!(attr.fileid, ino);

        // Lookup missing
        let err = nfs
            .lookup(ROOT_FILEID, &b"nonexistent".to_vec().into())
            .await
            .unwrap_err();
        assert!(matches!(err, nfsstat3::NFS3ERR_NOENT));
    }

    #[tokio::test]
    async fn nfs_read_content() {
        let nfs = test_nfs();
        let ino = nfs
            .lookup(ROOT_FILEID, &b"readme".to_vec().into())
            .await
            .unwrap();

        let (data, eof) = nfs.read(ino, 0, 1024).await.unwrap();
        assert_eq!(data, b"hello world");
        assert!(eof);

        // Offset read
        let (data, _) = nfs.read(ino, 6, 1024).await.unwrap();
        assert_eq!(data, b"world");
    }

    #[tokio::test]
    async fn nfs_readdir_pagination() {
        let nfs = test_nfs();

        // Full listing — root has 2 direct children: readme, docs
        let result = nfs.readdir(ROOT_FILEID, 0, 100).await.unwrap();
        assert!(result.end);
        assert_eq!(result.entries.len(), 2);

        // Paginate: max 1 entry
        let page1 = nfs.readdir(ROOT_FILEID, 0, 1).await.unwrap();
        assert_eq!(page1.entries.len(), 1);
        assert!(!page1.end);

        // Page 2: start after first entry
        let after = page1.entries[0].fileid;
        let page2 = nfs.readdir(ROOT_FILEID, after, 10).await.unwrap();
        assert_eq!(page2.entries.len(), 1);
        assert!(page2.end);
    }

    #[tokio::test]
    async fn nfs_lookup_dot_dotdot() {
        let nfs = test_nfs();

        // "." on root
        let ino = nfs
            .lookup(ROOT_FILEID, &b".".to_vec().into())
            .await
            .unwrap();
        assert_eq!(ino, ROOT_FILEID);

        // ".." on root
        let ino = nfs
            .lookup(ROOT_FILEID, &b"..".to_vec().into())
            .await
            .unwrap();
        assert_eq!(ino, ROOT_FILEID);

        // Lookup into docs, then ".."
        let docs_ino = nfs
            .lookup(ROOT_FILEID, &b"docs".to_vec().into())
            .await
            .unwrap();
        let parent_ino = nfs.lookup(docs_ino, &b"..".to_vec().into()).await.unwrap();
        assert_eq!(parent_ino, ROOT_FILEID);
    }

    // ---- Write sequences, replayed as a client sends them (bead b6d217) ----
    //
    // The NFSFileSystem methods called in-process, in the order a client's
    // RPCs arrive (hf-mount's `NFSAdapter` tests are the model), over the real
    // SQLite graph with a source writer installed. No kernel, no timers: the
    // idle commit is driven with explicit instants.

    #[cfg(feature = "splice")]
    mod write_sequences {
        use super::*;
        use crate::graph::SqliteGraphAdapter;
        use crate::graph::fixtures::CheckingWriter;
        use crate::graph::{Proposal, SourceWriter};
        use rusqlite::Connection;

        /// Records every file the commit hands the write path, then checks
        /// its base and projects it as the real writer would.
        #[derive(Default)]
        struct Recorder {
            writes: Mutex<Vec<Vec<u8>>>,
        }

        impl SourceWriter for Recorder {
            fn propose(&self, conn: &Connection, proposal: Proposal) -> anyhow::Result<()> {
                if let Proposal::Write { bytes, .. } = &proposal {
                    self.writes.lock().push(bytes.clone());
                }
                CheckingWriter.propose(conn, proposal)
            }

            fn check(&self, conn: &Connection, proposal: &Proposal) -> anyhow::Result<()> {
                CheckingWriter.check(conn, proposal)
            }
        }

        fn html_nfs() -> (LeylineNfs, Arc<Recorder>) {
            nfs_over(b"<p>hello</p>\n")
        }

        fn nfs_over(html: &[u8]) -> (LeylineNfs, Arc<Recorder>) {
            let db = leyline_ts::parse_with_source(
                html,
                leyline_ts::languages::TsLanguage::Html,
                "test.html",
            )
            .unwrap();
            let mut adapter = SqliteGraphAdapter::new_writable(&db).unwrap();
            let recorder = Arc::new(Recorder::default());
            adapter.set_source_writer(recorder.clone());
            (LeylineNfs::new(Arc::new(adapter)), recorder)
        }

        /// The `<p>`'s text leaf, looked up one component at a time as a
        /// client walks a path.
        async fn text_leaf(nfs: &LeylineNfs) -> fileid3 {
            walk(nfs, &["test.html", "element", "text"]).await
        }

        async fn walk(nfs: &LeylineNfs, path: &[&str]) -> fileid3 {
            let mut ino = ROOT_FILEID;
            for name in path {
                ino = nfs
                    .lookup(ino, &name.as_bytes().to_vec().into())
                    .await
                    .unwrap();
            }
            ino
        }

        fn size(k: u64) -> sattr3 {
            sattr3 {
                size: set_size3::size(k),
                ..sattr3::default()
            }
        }

        async fn read_all(nfs: &LeylineNfs, ino: fileid3) -> Vec<u8> {
            nfs.read(ino, 0, 4096).await.unwrap().0
        }

        /// Committing truncates as if the idle window had passed.
        fn commit_now(nfs: &LeylineNfs) -> bool {
            nfs.staged()
                .commit_if_idle(Instant::now() + COMMIT_IDLE, COMMIT_IDLE)
        }

        /// A truncating write in two UNSTABLE chunks — SETATTR(size 0),
        /// WRITE, WRITE, COMMIT — reaches the write path once, whole, at the
        /// COMMIT, and the idle window does not commit it first. Before, the
        /// server answered every WRITE FILE_SYNC, had no COMMIT, and spliced
        /// on a 200 ms quiet timer (bead 3c4893).
        #[tokio::test]
        async fn unstable_writes_commit_once_whole_at_the_commit() {
            let (nfs, recorder) = html_nfs();
            let ino = text_leaf(&nfs).await;
            nfs.getattr(ino).await.unwrap();
            nfs.setattr(ino, size(0)).await.unwrap();
            let (_, stable) = nfs.write_stable(ino, 0, b"wor", false).await.unwrap();
            assert!(!stable, "an unstable write is answered UNSTABLE");
            let (attr, _) = nfs.write_stable(ino, 3, b"ld", false).await.unwrap();
            assert_eq!(attr.size, 5);
            assert_eq!(
                read_all(&nfs, ino).await,
                b"world",
                "reads see staged bytes"
            );
            assert!(!commit_now(&nfs), "the idle window leaves a write alone");
            assert!(
                recorder.writes.lock().is_empty(),
                "nothing commits before the COMMIT"
            );

            nfs.commit(ino, 0, 0).await.unwrap();
            assert_eq!(*recorder.writes.lock(), vec![b"<p>world</p>\n".to_vec()]);
            assert_eq!(read_all(&nfs, text_leaf(&nfs).await).await, b"world");
            nfs.commit(ino, 0, 0).await.unwrap();
            assert_eq!(
                recorder.writes.lock().len(),
                1,
                "a second COMMIT has nothing to commit"
            );
        }

        /// A FILE_SYNC write is committed before it is answered, and is
        /// answered FILE_SYNC.
        #[tokio::test]
        async fn a_file_sync_write_commits_before_it_returns() {
            let (nfs, recorder) = html_nfs();
            let ino = text_leaf(&nfs).await;
            nfs.setattr(ino, size(0)).await.unwrap();
            let (_, stable) = nfs.write_stable(ino, 0, b"bye", true).await.unwrap();
            assert!(stable, "a committed write is answered FILE_SYNC");
            assert_eq!(*recorder.writes.lock(), vec![b"<p>bye</p>\n".to_vec()]);
        }

        /// A COMMIT the write path refuses fails, so the client's fsync or
        /// close reports the error instead of claiming the bytes are kept.
        #[tokio::test]
        async fn a_refused_commit_fails() {
            let db = leyline_ts::parse_with_source(
                b"<p>hello</p>\n",
                leyline_ts::languages::TsLanguage::Html,
                "test.html",
            )
            .unwrap();
            // No source writer: every edit to a parsed file is refused.
            let nfs = LeylineNfs::new(Arc::new(SqliteGraphAdapter::new_writable(&db).unwrap()));
            let ino = text_leaf(&nfs).await;
            nfs.write_stable(ino, 0, b"HELLO", false).await.unwrap();
            assert!(matches!(
                nfs.commit(ino, 0, 0).await,
                Err(nfsstat3::NFS3ERR_IO)
            ));
            assert!(matches!(
                nfs.write_stable(ino, 0, b"HELLO", true).await,
                Err(nfsstat3::NFS3ERR_IO)
            ));
        }

        /// Two clients edit two nodes of one file with UNSTABLE writes; the
        /// second's COMMIT lands. The first client's edit was made against
        /// the file before it, so its COMMIT is refused `NFS3ERR_STALE`
        /// (the client's fsync or close sees `ESTALE`) and the node reads
        /// as stored. Before, the first COMMIT dropped the file's base and
        /// the second landed unchecked over bytes its client never read.
        #[tokio::test]
        async fn a_commit_against_a_file_that_moved_is_refused_stale() {
            let (nfs, recorder) = nfs_over(b"<p>hello</p>\n<p>there</p>\n");
            let x = walk(&nfs, &["test.html", "element_0", "text"]).await;
            let y = walk(&nfs, &["test.html", "element_1", "text"]).await;
            nfs.write(x, 0, b"world").await.unwrap();
            nfs.write(y, 0, b"again").await.unwrap();
            nfs.commit(y, 0, 0).await.unwrap();
            assert!(matches!(
                nfs.commit(x, 0, 0).await,
                Err(nfsstat3::NFS3ERR_STALE)
            ));
            assert_eq!(
                recorder.writes.lock().len(),
                2,
                "both COMMITs reach the write path, which checks the base"
            );
            assert_eq!(read_all(&nfs, x).await, b"hello");
            assert_eq!(read_all(&nfs, y).await, b"again");
        }

        /// Another writer lands `text` over node `ino` — a socket proposal,
        /// as far as the mount can tell: it reaches the write path without
        /// passing through the NFS server.
        fn land_elsewhere(nfs: &LeylineNfs, ino: fileid3, text: &str) {
            let id = nfs.resolve_id(ino).unwrap();
            nfs.graph
                .batch_splice(&[(id, Some(text.to_string()))])
                .unwrap();
        }

        /// Whether a WRITE was refused as stale.
        fn is_stale<T>(r: Result<T, nfsstat3>) -> bool {
            matches!(r, Err(nfsstat3::NFS3ERR_STALE))
        }

        /// The edit a macOS client makes: it READs the node, holds its
        /// change as UNSTABLE writes until fsync, and the server first sees
        /// the WRITE after another writer moved the file. The WRITE is
        /// refused `NFS3ERR_STALE`, nothing is staged, and nothing of the
        /// edit lands (bead `ley-line-open-6116c9`). The WRITE, not the
        /// COMMIT, carries the refusal: macOS reports a failed WRITE at
        /// fsync and ignores a failed COMMIT. Before, the base was taken at
        /// the WRITE, from the moved file, and the edit landed over the
        /// change its client never read.
        #[tokio::test]
        async fn an_edit_read_before_the_file_moved_is_refused_stale() {
            let (nfs, recorder) = nfs_over(b"<p>hello</p>\n<p>there</p>\n");
            let x = walk(&nfs, &["test.html", "element_0", "text"]).await;
            let y = walk(&nfs, &["test.html", "element_1", "text"]).await;
            assert_eq!(read_all(&nfs, x).await, b"hello");
            land_elsewhere(&nfs, y, "moved");
            assert!(is_stale(nfs.write(x, 0, b"world").await));
            nfs.commit(x, 0, 0).await.unwrap();
            assert_eq!(read_all(&nfs, x).await, b"hello", "nothing landed");
            assert_eq!(read_all(&nfs, y).await, b"moved");
            assert_eq!(
                recorder.writes.lock().len(),
                1,
                "only the move reached the write path"
            );
        }

        /// A truncate from a client that read the file before it moved is
        /// refused the same way, before anything is staged.
        #[tokio::test]
        async fn a_truncate_read_before_the_file_moved_is_refused_stale() {
            let (nfs, recorder) = nfs_over(b"<p>hello</p>\n<p>there</p>\n");
            let x = walk(&nfs, &["test.html", "element_0", "text"]).await;
            let y = walk(&nfs, &["test.html", "element_1", "text"]).await;
            assert_eq!(read_all(&nfs, x).await, b"hello");
            land_elsewhere(&nfs, y, "moved");
            assert!(is_stale(nfs.setattr(x, size(0)).await));
            assert!(!commit_now(&nfs), "nothing staged");
            assert_eq!(read_all(&nfs, x).await, b"hello");
            assert_eq!(recorder.writes.lock().len(), 1);
        }

        /// A refusal keeps the base: a client that retries from its cache,
        /// with no READ in between, is refused again. A READ re-takes it,
        /// and the edit made after that lands. Before, the refusal dropped
        /// the base and the retry's WRITE took one from the moved file.
        #[tokio::test]
        async fn a_retry_without_a_read_after_a_stale_refusal_is_refused() {
            let (nfs, _recorder) = nfs_over(b"<p>hello</p>\n<p>there</p>\n");
            let x = walk(&nfs, &["test.html", "element_0", "text"]).await;
            let y = walk(&nfs, &["test.html", "element_1", "text"]).await;
            assert_eq!(read_all(&nfs, x).await, b"hello");
            nfs.write(x, 0, b"early").await.unwrap();
            land_elsewhere(&nfs, y, "moved");
            assert!(matches!(
                nfs.commit(x, 0, 0).await,
                Err(nfsstat3::NFS3ERR_STALE)
            ));
            for _ in 0..2 {
                assert!(is_stale(nfs.write(x, 0, b"world").await));
            }
            assert_eq!(read_all(&nfs, x).await, b"hello");
            nfs.write(x, 0, b"world").await.unwrap();
            nfs.commit(x, 0, 0).await.unwrap();
            assert_eq!(read_all(&nfs, x).await, b"world");
            assert_eq!(read_all(&nfs, y).await, b"moved");
        }

        /// A READ with no change after it: the edit lands.
        #[tokio::test]
        async fn an_edit_read_from_the_current_file_lands() {
            let (nfs, recorder) = html_nfs();
            let ino = text_leaf(&nfs).await;
            assert_eq!(read_all(&nfs, ino).await, b"hello");
            nfs.write(ino, 0, b"world").await.unwrap();
            nfs.commit(ino, 0, 0).await.unwrap();
            assert_eq!(*recorder.writes.lock(), vec![b"<p>world</p>\n".to_vec()]);
        }

        /// A READ after the file moved, with nothing staged, re-takes the
        /// base: the client has read the moved file, and its edit lands.
        #[tokio::test]
        async fn a_read_after_the_file_moved_refreshes_the_base() {
            let (nfs, _recorder) = nfs_over(b"<p>hello</p>\n<p>there</p>\n");
            let x = walk(&nfs, &["test.html", "element_0", "text"]).await;
            let y = walk(&nfs, &["test.html", "element_1", "text"]).await;
            assert_eq!(read_all(&nfs, x).await, b"hello");
            land_elsewhere(&nfs, y, "moved");
            assert_eq!(read_all(&nfs, x).await, b"hello");
            nfs.write(x, 0, b"world").await.unwrap();
            nfs.commit(x, 0, 0).await.unwrap();
            assert_eq!(read_all(&nfs, x).await, b"world");
            assert_eq!(read_all(&nfs, y).await, b"moved");
        }

        /// A READ of another node of the file while an edit to it is staged
        /// leaves the base alone: the staged edit was made against the
        /// bytes read before it, and the file moved since.
        #[tokio::test]
        async fn a_read_while_an_edit_is_staged_keeps_its_base() {
            let (nfs, _recorder) = nfs_over(b"<p>a</p>\n<p>b</p>\n<p>c</p>\n");
            let x = walk(&nfs, &["test.html", "element_0", "text"]).await;
            let y = walk(&nfs, &["test.html", "element_1", "text"]).await;
            let z = walk(&nfs, &["test.html", "element_2", "text"]).await;
            assert_eq!(read_all(&nfs, x).await, b"a");
            nfs.write(x, 0, b"A").await.unwrap();
            land_elsewhere(&nfs, z, "moved");
            assert_eq!(read_all(&nfs, y).await, b"b");
            assert!(
                is_stale(nfs.write(x, 1, b"!").await),
                "a further WRITE to the stale edit is refused"
            );
            assert!(matches!(
                nfs.commit(x, 0, 0).await,
                Err(nfsstat3::NFS3ERR_STALE)
            ));
            assert_eq!(read_all(&nfs, x).await, b"a");
            assert_eq!(read_all(&nfs, z).await, b"moved");
        }

        /// After the mount's own edit lands, its base is the file it wrote:
        /// the client's cache holds those bytes and it need not READ again.
        /// Another writer then moves the file, and an edit the client makes
        /// from its cache is refused `NFS3ERR_STALE` at its WRITE. Before, the landed
        /// edit dropped the base and the next WRITE took one from the moved
        /// file.
        #[tokio::test]
        async fn an_edit_after_the_mounts_own_landed_edit_is_checked() {
            let (nfs, _recorder) = nfs_over(b"<p>hello</p>\n<p>there</p>\n");
            let x = walk(&nfs, &["test.html", "element_0", "text"]).await;
            let y = walk(&nfs, &["test.html", "element_1", "text"]).await;
            assert_eq!(read_all(&nfs, x).await, b"hello");
            nfs.write(x, 0, b"world").await.unwrap();
            nfs.commit(x, 0, 0).await.unwrap();
            land_elsewhere(&nfs, y, "moved");
            assert!(is_stale(nfs.write(x, 0, b"again").await));
            assert_eq!(read_all(&nfs, x).await, b"world");
            assert_eq!(read_all(&nfs, y).await, b"moved");
        }

        /// A truncate with no write after it — `: > f` sends SETATTR alone,
        /// and no COMMIT follows — commits once the idle window has passed,
        /// and not before.
        #[tokio::test]
        async fn a_bare_truncate_commits_after_the_idle_window() {
            let (nfs, recorder) = html_nfs();
            assert!(!commit_now(&nfs), "nothing staged");
            let ino = text_leaf(&nfs).await;
            nfs.setattr(ino, size(3)).await.unwrap();
            assert!(
                !nfs.staged()
                    .commit_if_idle(Instant::now(), Duration::from_secs(3600))
            );
            assert!(recorder.writes.lock().is_empty());
            assert!(commit_now(&nfs));
            assert_eq!(*recorder.writes.lock(), vec![b"<p>hel</p>\n".to_vec()]);
        }

        /// `: > f`, a SETATTR to size 0 alone, commits the empty node once
        /// the idle window has passed (bead `ley-line-open-8a26d8`).
        /// Before, the commit of an empty edit returned success without
        /// landing, and the empty edit stayed staged in the mount.
        #[tokio::test]
        async fn a_bare_truncate_to_empty_commits() {
            let (nfs, recorder) = html_nfs();
            let ino = text_leaf(&nfs).await;
            nfs.setattr(ino, size(0)).await.unwrap();
            assert!(commit_now(&nfs));
            assert_eq!(*recorder.writes.lock(), vec![b"<p></p>\n".to_vec()]);
        }

        #[tokio::test]
        async fn a_read_before_the_write_does_not_disturb_it() {
            let (nfs, recorder) = html_nfs();
            let ino = text_leaf(&nfs).await;
            assert_eq!(read_all(&nfs, ino).await, b"hello");
            nfs.setattr(ino, size(0)).await.unwrap();
            nfs.write(ino, 0, b"bye").await.unwrap();
            nfs.commit(ino, 0, 0).await.unwrap();
            assert_eq!(*recorder.writes.lock(), vec![b"<p>bye</p>\n".to_vec()]);
        }

        /// SETATTR to a nonzero size truncates to that size. Before, every
        /// size but 0 was ignored and reported as done.
        #[tokio::test]
        async fn a_nonzero_truncate_keeps_that_many_bytes() {
            let (nfs, recorder) = html_nfs();
            let ino = text_leaf(&nfs).await;
            let attr = nfs.setattr(ino, size(2)).await.unwrap();
            assert_eq!(attr.size, 2);
            assert_eq!(read_all(&nfs, ino).await, b"he");
            assert!(commit_now(&nfs));
            assert_eq!(*recorder.writes.lock(), vec![b"<p>he</p>\n".to_vec()]);
        }

        /// A write past the end zero-fills the gap, as POSIX does, and the
        /// zero-filled file lands: the mount refuses nothing on what the
        /// bytes parse as (ADR-0045 T3, T8).
        #[tokio::test]
        async fn a_write_past_the_end_zero_fills() {
            let (nfs, recorder) = html_nfs();
            let ino = text_leaf(&nfs).await;
            nfs.write(ino, 7, b"x").await.unwrap();
            assert_eq!(read_all(&nfs, ino).await, b"hello\0\0x");
            nfs.commit(ino, 0, 0).await.unwrap();
            assert_eq!(
                *recorder.writes.lock(),
                vec![b"<p>hello\0\0x</p>\n".to_vec()]
            );
        }

        /// An operation that changes the tree's shape commits staged writes
        /// first, so it never runs against a half-applied file, even one
        /// the projection then refuses: a new directory is nothing a
        /// store's projection presents (ADR-0045 T8), so it is
        /// `NFS3ERR_PERM`, and so are a new file and a mode change.
        #[tokio::test]
        async fn a_structural_operation_commits_staged_writes_first() {
            let (nfs, recorder) = html_nfs();
            let ino = text_leaf(&nfs).await;
            nfs.setattr(ino, size(0)).await.unwrap();
            nfs.write(ino, 0, b"world").await.unwrap();
            assert!(matches!(
                nfs.mkdir(ROOT_FILEID, &b"newdir".to_vec().into()).await,
                Err(nfsstat3::NFS3ERR_PERM)
            ));
            assert_eq!(*recorder.writes.lock(), vec![b"<p>world</p>\n".to_vec()]);
            let name = b"new.html".to_vec().into();
            assert!(matches!(
                nfs.create(ROOT_FILEID, &name, sattr3::default()).await,
                Err(nfsstat3::NFS3ERR_PERM)
            ));
            assert!(matches!(
                nfs.create_exclusive(ROOT_FILEID, &name).await,
                Err(nfsstat3::NFS3ERR_PERM)
            ));
            let ino = text_leaf(&nfs).await;
            let mode = |bits| sattr3 {
                mode: set_mode3::mode(bits),
                ..sattr3::default()
            };
            assert!(matches!(
                nfs.setattr(ino, mode(0o755)).await,
                Err(nfsstat3::NFS3ERR_PERM)
            ));
            nfs.setattr(ino, mode(0o644)).await.unwrap();
        }

        /// A WRITE whose edit merges with a change landed since the
        /// client's READ is staged, and its COMMIT lands the merge; one that
        /// overlaps the change is refused `NFS3ERR_STALE` at the WRITE
        /// (ADR-0045 T8).
        #[tokio::test]
        async fn a_write_that_merges_is_staged_and_an_overlapping_one_refused() {
            let db = leyline_ts::parse_with_source(
                b"<p>hello</p>\n<p>there</p>\n",
                leyline_ts::languages::TsLanguage::Html,
                "test.html",
            )
            .unwrap();
            let mut adapter = SqliteGraphAdapter::new_writable(&db).unwrap();
            adapter.set_source_writer(Arc::new(crate::graph::fixtures::MergingWriter));
            let nfs = LeylineNfs::new(Arc::new(adapter));
            let x = walk(&nfs, &["test.html", "element_0", "text"]).await;
            let y = walk(&nfs, &["test.html", "element_1", "text"]).await;
            assert_eq!(read_all(&nfs, x).await, b"hello");
            land_elsewhere(&nfs, y, "moved");
            nfs.write(x, 0, b"HELLO").await.unwrap();
            nfs.commit(x, 0, 0).await.unwrap();
            assert_eq!(read_all(&nfs, y).await, b"moved");
            assert_eq!(read_all(&nfs, x).await, b"HELLO");

            land_elsewhere(&nfs, x, "other");
            assert!(is_stale(nfs.write(x, 0, b"mine!").await));
            assert_eq!(read_all(&nfs, x).await, b"other");
        }

        /// Dropping the server commits what is staged; a shutdown inside the
        /// idle window does not lose the write.
        #[tokio::test]
        async fn dropping_the_server_commits_staged_writes() {
            let (nfs, recorder) = html_nfs();
            let ino = text_leaf(&nfs).await;
            nfs.setattr(ino, size(0)).await.unwrap();
            nfs.write(ino, 0, b"world").await.unwrap();
            drop(nfs);
            assert_eq!(*recorder.writes.lock(), vec![b"<p>world</p>\n".to_vec()]);
        }

        /// A source file's entry reports the file's stored mtime (bead
        /// ca51fa); the structural directory above it and the root, whose
        /// stored mtimes do not move with their entries, report the current
        /// time so the client relists them. Before, every directory reported
        /// the current time, the source file's entry included.
        #[tokio::test]
        async fn a_source_file_reports_its_mtime_and_the_directories_above_it_move() {
            let db = leyline_ts::parse_with_source(
                b"<p>hello</p>\n",
                leyline_ts::languages::TsLanguage::Html,
                "web/test.html",
            )
            .unwrap();
            let graph: Arc<dyn Graph> = Arc::new(SqliteGraphAdapter::new_writable(&db).unwrap());
            let stored = graph.get_node("web/test.html").unwrap().unwrap();
            assert!(stored.is_dir, "a source file's entry is a directory");
            let nfs = LeylineNfs::new(graph);
            let web = nfs
                .lookup(ROOT_FILEID, &b"web".to_vec().into())
                .await
                .unwrap();
            let file = nfs
                .lookup(web, &b"test.html".to_vec().into())
                .await
                .unwrap();
            let before = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as u32;

            let file_attr = nfs.getattr(file).await.unwrap();
            assert_eq!(
                (file_attr.mtime.seconds, file_attr.mtime.nseconds),
                (
                    (stored.mtime_nanos / 1_000_000_000) as u32,
                    (stored.mtime_nanos % 1_000_000_000) as u32
                ),
                "the source file's entry reports the file's own mtime"
            );
            for (name, ino) in [("root", ROOT_FILEID), ("web", web)] {
                let attr = nfs.getattr(ino).await.unwrap();
                assert!(
                    attr.mtime.seconds >= before,
                    "{name} reports the current time, got {}",
                    attr.mtime.seconds
                );
            }
        }

        /// Dropping a running server commits its staged writes before the
        /// drop returns, not whenever the runtime later drops the task. On
        /// this single-threaded runtime the server task has not run once by
        /// the drop, so only the drop itself can have committed.
        #[tokio::test]
        async fn dropping_a_running_server_commits_before_it_returns() {
            let db = leyline_ts::parse_with_source(
                b"<p>hello</p>\n",
                leyline_ts::languages::TsLanguage::Html,
                "test.html",
            )
            .unwrap();
            let mut adapter = SqliteGraphAdapter::new_writable(&db).unwrap();
            let recorder = Arc::new(Recorder::default());
            adapter.set_source_writer(recorder.clone());
            let graph: Arc<dyn Graph> = Arc::new(adapter);
            let leaf = "test.html/element/text";
            graph.truncate(leaf).unwrap();
            graph.write_content(leaf, b"world", 0).unwrap();

            let server = serve_nfs(graph, "127.0.0.1:0").await.unwrap();
            assert_ne!(
                server.port(),
                0,
                "port 0 asks for one; the bound one is reported"
            );
            server.staged.stage_write(leaf);
            drop(server);
            assert_eq!(*recorder.writes.lock(), vec![b"<p>world</p>\n".to_vec()]);
        }
    }
}
