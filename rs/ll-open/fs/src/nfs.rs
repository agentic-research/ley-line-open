use crate::graph::{Graph, Node};
use async_trait::async_trait;
use nfsserve::nfs::*;
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const ROOT_FILEID: fileid3 = 1;

pub struct LeylineNfs {
    graph: Arc<dyn Graph>,
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
            graph,
            ino_to_id: Mutex::new(ino_to_id),
            id_to_ino: Mutex::new(id_to_ino),
            next_ino: Mutex::new(ROOT_FILEID + 1),
        }
    }

    fn ensure_ino(&self, id: &str) -> u64 {
        let mut id_map = self.id_to_ino.lock().unwrap();
        if let Some(&ino) = id_map.get(id) {
            return ino;
        }
        let mut next = self.next_ino.lock().unwrap();
        let ino = *next;
        *next += 1;
        id_map.insert(id.to_string(), ino);
        self.ino_to_id.lock().unwrap().insert(ino, id.to_string());
        ino
    }

    fn resolve_id(&self, ino: fileid3) -> Result<String, nfsstat3> {
        self.ino_to_id
            .lock()
            .unwrap()
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

        // Directories use dynamic mtime so the NFS client always invalidates
        // its readdir cache. Without this, the synthetic root (mtime=0) causes
        // stale empty listings after data is loaded or reverted.
        let mtime = if node.is_dir {
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
            uid: unsafe { libc::getuid() },
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
            uid: unsafe { libc::getuid() },
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

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let node_id = self.resolve_id(id)?;

        // Only handle truncate (size = 0)
        if let set_size3::size(0) = setattr.size {
            self.graph
                .truncate(&node_id)
                .map_err(|_| nfsstat3::NFS3ERR_ROFS)?;
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

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        let node_id = self.resolve_id(id)?;
        self.graph
            .write_content(&node_id, data, offset)
            .map_err(|_| nfsstat3::NFS3ERR_IO)?;

        // Best-effort splice — NFS has no per-file close/flush
        if let Err(e) = self.graph.flush_node(&node_id) {
            log::debug!("NFS splice deferred for {node_id}: {e}");
        }

        let node = self
            .graph
            .get_node(&node_id)
            .map_err(|_| nfsstat3::NFS3ERR_IO)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;
        Ok(self.node_to_fattr3(id, &node))
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        _attr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
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
        let mut id_map = self.id_to_ino.lock().unwrap();
        if let Some(ino) = id_map.remove(&node.id) {
            self.ino_to_id.lock().unwrap().remove(&ino);
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
        let mut id_map = self.id_to_ino.lock().unwrap();
        if let Some(ino) = id_map.remove(&old_id) {
            id_map.insert(new_id.clone(), ino);
            self.ino_to_id.lock().unwrap().insert(ino, new_id);
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

/// Start an NFS server on the given address, serving the provided graph.
/// Returns the actual port the server bound to.
/// The server runs in the background until the returned handle is dropped/aborted.
pub async fn serve_nfs(
    graph: Arc<dyn Graph>,
    listen_addr: &str,
) -> anyhow::Result<(u16, tokio::task::JoinHandle<()>)> {
    let fs = LeylineNfs::new(graph);
    let listener = NFSTcpListener::bind(listen_addr, fs).await?;
    let port = listener.get_listen_port();
    let handle = tokio::spawn(async move {
        if let Err(e) = listener.handle_forever().await {
            log::error!("NFS server error: {e}");
        }
    });
    Ok((port, handle))
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
}
