use crate::graph::{Graph, Node};
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
        }
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
        let (ftype, perm, size) = if node.is_dir {
            (FileType::Directory, 0o755, 4096)
        } else {
            (FileType::RegularFile, 0o644, node.size)
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
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: BLOCK_SIZE,
            flags: 0,
        }
    }
}

impl Filesystem for LeylineFuse {
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
        _fh: u64,
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
        match self.graph.read_content(&id, &mut buf, offset as u64) {
            Ok(n) => reply.data(&buf[..n]),
            Err(e) => {
                log::error!("read({id}): {e}");
                reply.error(libc::EIO);
            }
        }
    }

    fn open(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        reply.opened(0, 0);
    }

    fn opendir(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        reply.opened(0, 0);
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
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

        match self.graph.write_content(&id, data, offset as u64) {
            Ok(n) => reply.written(n as u32),
            Err(_) => reply.error(libc::EROFS),
        }
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: fuser::ReplyEmpty,
    ) {
        let Some(id) = self.ino_to_id.get(&ino).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };
        match self.graph.flush_node(&id) {
            Ok(()) => reply.ok(),
            Err(e) => {
                log::warn!("flush_node failed for {id}: {e}");
                reply.error(libc::EIO);
            }
        }
    }

    fn fsync(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: fuser::ReplyEmpty,
    ) {
        // Per-node flush first
        if let Some(id) = self.ino_to_id.get(&ino).cloned()
            && let Err(e) = self.graph.flush_node(&id)
        {
            log::warn!("fsync flush_node failed for {id}: {e}");
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
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
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

        if let Some(0) = size
            && self.graph.truncate(&id).is_err()
        {
            reply.error(libc::EROFS);
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
                reply.created(&TTL, &attr, 0, 0, 0);
            }
            Err(_) => reply.error(libc::EROFS),
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
            Err(_) => reply.error(libc::EROFS),
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
            Err(_) => reply.error(libc::EROFS),
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
            Err(_) => reply.error(libc::EROFS),
        }
    }
}

/// Mount the FUSE filesystem in the background. Returns a session handle
/// that unmounts automatically when dropped.
pub fn mount_fuse(
    graph: Arc<dyn Graph>,
    mountpoint: &Path,
) -> anyhow::Result<fuser::BackgroundSession> {
    let fs = LeylineFuse::new(graph);
    let options = vec![
        MountOption::FSName("leyline".into()),
        MountOption::AutoUnmount,
    ];
    let session = fuser::spawn_mount2(fs, mountpoint, &options)?;
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::MemoryGraph;

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
