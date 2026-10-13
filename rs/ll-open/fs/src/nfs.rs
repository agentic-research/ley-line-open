use crate::graph::{Graph, Node};
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
        let (ftype, mode, size) = if node.is_dir {
            (ftype3::NF3DIR, 0o755, 4096u64)
        } else {
            (ftype3::NF3REG, 0o644, node.size)
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
    /// is the first RPC of a truncating write). Mode, owner and times are
    /// derived from the projection, not stored, so changes to them are
    /// accepted and have no effect.
    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let node_id = self.resolve_id(id)?;

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
                .map_err(|_| nfsstat3::NFS3ERR_ROFS)?;
            if keep > 0 {
                self.graph
                    .write_content(&node_id, &kept, 0)
                    .map_err(|_| nfsstat3::NFS3ERR_IO)?;
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
    /// before it is answered. A refused commit fails the write.
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
            .map_err(|_| nfsstat3::NFS3ERR_IO)?;
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
    /// named, since a file is spliced whole. A refused splice is an I/O
    /// error, so the fsync or close reports it.
    async fn commit(&self, id: fileid3, _offset: u64, _count: u32) -> Result<(), nfsstat3> {
        let node_id = self.resolve_id(id)?;
        self.staged.commit_node(&node_id).map_err(|e| {
            log::warn!("NFS commit of {node_id} refused: {e:#}");
            nfsstat3::NFS3ERR_IO
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
            .map_err(|_| nfsstat3::NFS3ERR_ROFS)?;
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
            .map_err(|_| nfsstat3::NFS3ERR_ROFS)?;
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
            .map_err(|_| nfsstat3::NFS3ERR_ROFS)?;
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
            .map_err(|_| nfsstat3::NFS3ERR_ROFS)?;

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
            .map_err(|_| nfsstat3::NFS3ERR_ROFS)?;

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
        use crate::graph::SourceWriter;
        use crate::graph::SqliteGraphAdapter;
        use crate::graph::fixtures::ProjectingWriter;
        use rusqlite::Connection;

        /// Records every file the commit hands the write path, then projects
        /// it as the real writer would.
        #[derive(Default)]
        struct Recorder {
            writes: Mutex<Vec<Vec<u8>>>,
        }

        impl SourceWriter for Recorder {
            fn write(
                &self,
                conn: &Connection,
                source_id: &str,
                bytes: Vec<u8>,
                base: Option<leyline_core::Hash>,
            ) -> anyhow::Result<()> {
                self.writes.lock().push(bytes.clone());
                ProjectingWriter.write(conn, source_id, bytes, base)
            }
        }

        fn html_nfs() -> (LeylineNfs, Arc<Recorder>) {
            let db = leyline_ts::parse_with_source(
                b"<p>hello</p>\n",
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
            let mut ino = ROOT_FILEID;
            for name in ["test.html", "element", "text"] {
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

        /// A write past the end zero-fills the gap, as POSIX does.
        #[tokio::test]
        async fn a_write_past_the_end_zero_fills() {
            let (nfs, recorder) = html_nfs();
            let ino = text_leaf(&nfs).await;
            nfs.write(ino, 7, b"x").await.unwrap();
            assert_eq!(read_all(&nfs, ino).await, b"hello\0\0x");
            // The zero-filled file reaches the write path, which refuses
            // HTML with NULs in it; the COMMIT reports that refusal. The
            // refused edit is discarded: the stored file is projected again,
            // so the leaf reads as it did before the write.
            assert!(matches!(
                nfs.commit(ino, 0, 0).await,
                Err(nfsstat3::NFS3ERR_IO)
            ));
            assert_eq!(
                *recorder.writes.lock(),
                vec![b"<p>hello\0\0x</p>\n".to_vec(), b"<p>hello</p>\n".to_vec()]
            );
            assert_eq!(read_all(&nfs, ino).await, b"hello");
        }

        /// An operation that changes the tree's shape commits staged writes
        /// first, so it never runs against a half-applied file.
        #[tokio::test]
        async fn a_structural_operation_commits_staged_writes_first() {
            let (nfs, recorder) = html_nfs();
            let ino = text_leaf(&nfs).await;
            nfs.setattr(ino, size(0)).await.unwrap();
            nfs.write(ino, 0, b"world").await.unwrap();
            nfs.mkdir(ROOT_FILEID, &b"newdir".to_vec().into())
                .await
                .unwrap();
            assert_eq!(*recorder.writes.lock(), vec![b"<p>world</p>\n".to_vec()]);
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
