//! [`StoreFs`] behind the host's NFSv3 client (ADR-0045 T8): the macOS host
//! mount, a loopback NFSv3 server as hf-mount serves one.
//!
//! NFSv3 names a file by its file id and has no OPEN or CLOSE, so the
//! adapter keeps one [`StoreFs`] write handle per file id it has served:
//!
//! - a READ rereads the handle when nothing is staged on it
//!   ([`StoreFs::reread`]), so the base an edit is checked against is no
//!   older than the bytes the client read;
//! - a WRITE stages on that handle, checked against what the store holds
//!   now ([`StoreFs::write_checked`]): an edit the store would refuse is
//!   refused `NFS3ERR_STALE` at the WRITE, which the macOS client reports at
//!   `fsync` and `close`. A SETATTR of the size stages unchecked: a save
//!   truncates before it writes, and the WRITE is checked with what it
//!   makes;
//! - a COMMIT (`fsync`, `close`) is the handle's FLUSH: a moved base merges,
//!   overlapping edits refuse. A COMMIT is refused only for a change landed
//!   between the client's last WRITE and the COMMIT; the macOS client does
//!   not report that refusal, so the staged edit is dropped, the refusal is
//!   logged, and the client's next READ serves the store's bytes;
//! - a truncate with no WRITE after it has no COMMIT coming (`: > f` sends
//!   SETATTR alone), so it is committed once it has stood for
//!   [`COMMIT_IDLE`];
//! - CREATE lands the new, empty file at once: a client may never COMMIT a
//!   file it only created (`touch`).
//!
//! Every operation that changes the tree's shape commits what is staged
//! first, as does dropping the server.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nfsserve::nfs::{
    fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, nfsstring, nfstime3, sattr3,
    set_gid3, set_mode3, set_size3, set_uid3, specdata3,
};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
use parking_lot::Mutex;

use crate::open_flags::O_RDWR;
use crate::{Attr, Errno, Kind, ROOT_INODE, SetAttr, Store, StoreFs};

/// How long a truncate must stand with no write after it before it is
/// committed.
pub const COMMIT_IDLE: Duration = Duration::from_millis(200);

/// What the write handle of a file id holds that the store does not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    /// Nothing: the handle serves what the store held at its last reread.
    Clean,
    /// A WRITE, waiting for the client's COMMIT.
    Written,
    /// A truncate at this instant with no WRITE since: the idle window.
    Truncated(Instant),
}

struct Open {
    handle: u64,
    stage: Stage,
}

/// [`StoreFs`] as an [`NFSFileSystem`]. A file id is the [`StoreFs`] inode.
/// A clone serves the same filesystem and handles: the server keeps one to
/// commit what is staged when it stops.
pub struct HostNfs<S> {
    fs: Arc<StoreFs<S>>,
    opens: Arc<Mutex<HashMap<fileid3, Open>>>,
}

impl<S> Clone for HostNfs<S> {
    fn clone(&self) -> Self {
        Self {
            fs: Arc::clone(&self.fs),
            opens: Arc::clone(&self.opens),
        }
    }
}

/// The status a refused request answers.
pub fn nfs_status(errno: Errno) -> nfsstat3 {
    match errno {
        Errno::EPERM => nfsstat3::NFS3ERR_PERM,
        Errno::ENOENT => nfsstat3::NFS3ERR_NOENT,
        Errno::EACCES => nfsstat3::NFS3ERR_ACCES,
        Errno::EEXIST => nfsstat3::NFS3ERR_EXIST,
        Errno::EXDEV => nfsstat3::NFS3ERR_XDEV,
        Errno::ENOTDIR => nfsstat3::NFS3ERR_NOTDIR,
        Errno::EISDIR => nfsstat3::NFS3ERR_ISDIR,
        Errno::EINVAL => nfsstat3::NFS3ERR_INVAL,
        Errno::ENOTEMPTY => nfsstat3::NFS3ERR_NOTEMPTY,
        Errno::ESTALE => nfsstat3::NFS3ERR_STALE,
        Errno::ENOSYS => nfsstat3::NFS3ERR_NOTSUPP,
        _ => nfsstat3::NFS3ERR_IO,
    }
}

fn status<T>(result: Result<T, Errno>) -> Result<T, nfsstat3> {
    result.map_err(nfs_status)
}

/// The owner every node is presented with: this process's.
fn owner() -> (u32, u32) {
    // SAFETY: `getuid(2)` and `getgid(2)` cannot fail and touch no memory.
    unsafe { (libc::getuid(), libc::getgid()) }
}

fn nfs_time(since_epoch: Duration) -> nfstime3 {
    nfstime3 {
        seconds: since_epoch.as_secs() as u32,
        nseconds: since_epoch.subsec_nanos(),
    }
}

/// A node's NFS attributes.
pub fn fattr(attr: &Attr) -> fattr3 {
    let (uid, gid) = owner();
    let mtime = nfs_time(attr.mtime);
    fattr3 {
        ftype: match attr.kind {
            Kind::File => ftype3::NF3REG,
            Kind::Directory => ftype3::NF3DIR,
            Kind::Symlink => ftype3::NF3LNK,
        },
        mode: attr.perm,
        nlink: attr.nlink(),
        uid,
        gid,
        size: attr.size,
        used: attr.size,
        rdev: specdata3::default(),
        fsid: 0,
        fileid: attr.inode,
        atime: mtime,
        mtime,
        ctime: mtime,
    }
}

/// The owner a SETATTR names, unless it is the owner every node is
/// presented with: setting that one changes nothing.
fn owner_change(uid: &set_uid3, gid: &set_gid3) -> (Option<u32>, Option<u32>) {
    let (own_uid, own_gid) = owner();
    let uid = match uid {
        set_uid3::uid(uid) if *uid != own_uid => Some(*uid),
        _ => None,
    };
    let gid = match gid {
        set_gid3::gid(gid) if *gid != own_gid => Some(*gid),
        _ => None,
    };
    (uid, gid)
}

fn name(filename: &filename3) -> &[u8] {
    &filename.0
}

impl<S: Store> HostNfs<S> {
    pub fn new(fs: Arc<StoreFs<S>>) -> Self {
        Self {
            fs,
            opens: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The filesystem this server serves.
    pub fn fs(&self) -> &StoreFs<S> {
        &self.fs
    }

    /// The write handle of `id`, opened when it has none.
    fn handle(&self, id: fileid3) -> Result<u64, nfsstat3> {
        if let Some(open) = self.opens.lock().get(&id) {
            return Ok(open.handle);
        }
        let handle = status(self.fs.open(id, O_RDWR))?.handle;
        let mut opens = self.opens.lock();
        match opens.get(&id) {
            // Another request opened one meanwhile: keep that one.
            Some(open) => {
                let kept = open.handle;
                drop(opens);
                self.fs.release(handle);
                Ok(kept)
            }
            None => {
                opens.insert(
                    id,
                    Open {
                        handle,
                        stage: Stage::Clean,
                    },
                );
                Ok(handle)
            }
        }
    }

    /// The handle of `id` when something is staged on it.
    fn staged(&self, id: fileid3) -> Option<u64> {
        self.opens
            .lock()
            .get(&id)
            .filter(|open| open.stage != Stage::Clean)
            .map(|open| open.handle)
    }

    /// Record that `id`'s handle staged a change: a WRITE, or a truncate
    /// at `now`. A truncate after a WRITE waits with it for its COMMIT.
    fn stage(&self, id: fileid3, change: Stage) {
        if let Some(open) = self.opens.lock().get_mut(&id) {
            open.stage = match (open.stage, change) {
                (Stage::Written, Stage::Truncated(_)) => Stage::Written,
                _ => change,
            };
        }
    }

    /// Drop `id`'s handle and anything staged on it.
    fn forget(&self, id: fileid3) {
        if let Some(open) = self.opens.lock().remove(&id) {
            self.fs.release(open.handle);
        }
    }

    /// Land what is staged on `id`'s handle. A refusal drops the staged
    /// edit: the client's next READ serves the store's bytes.
    fn commit_id(&self, id: fileid3) -> Result<(), Errno> {
        let Some(handle) = self.staged(id) else {
            return Ok(());
        };
        match self.fs.flush(handle) {
            Ok(()) => {
                if let Some(open) = self.opens.lock().get_mut(&id) {
                    open.stage = Stage::Clean;
                }
                Ok(())
            }
            Err(errno) => {
                log::warn!("NFS commit of file {id} refused ({errno:?}); its edit is dropped");
                self.forget(id);
                Err(errno)
            }
        }
    }

    /// Land everything staged. A refusal is logged and its edit dropped.
    pub fn commit_all(&self) {
        let ids: Vec<fileid3> = self
            .opens
            .lock()
            .iter()
            .filter(|(_, open)| open.stage != Stage::Clean)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let _ = self.commit_id(id);
        }
    }

    /// Land every truncate that has stood for `idle` with no WRITE after it,
    /// as of `now`. Answers whether it landed any.
    pub fn commit_if_idle(&self, now: Instant, idle: Duration) -> bool {
        let due: Vec<fileid3> = self
            .opens
            .lock()
            .iter()
            .filter(|(_, open)| {
                matches!(open.stage, Stage::Truncated(at) if now.duration_since(at) >= idle)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in &due {
            let _ = self.commit_id(*id);
        }
        !due.is_empty()
    }

    /// The attributes of `id`, with the size of what its handle stages.
    fn attr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let (attr, _) = status(self.fs.getattr(id, self.staged(id)))?;
        Ok(fattr(&attr))
    }
}

#[async_trait]
impl<S: Store + 'static> NFSFileSystem for HostNfs<S> {
    fn root_dir(&self) -> fileid3 {
        ROOT_INODE
    }

    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        match name(filename) {
            b"." => Ok(dirid),
            b".." => status(self.fs.parent(dirid)),
            name => Ok(status(self.fs.lookup(dirid, name))?.attr.inode),
        }
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        self.attr(id)
    }

    /// A size stages a truncate on the file id's handle; a mode is a
    /// proposal of the file's git mode, staged with what the handle stages;
    /// an owner other than the one presented is `NFS3ERR_PERM`; times are
    /// not carried.
    ///
    /// The truncate is not checked: a save truncates and then writes, and
    /// the empty file between the two is no edit the client means. The WRITE
    /// after it is checked with the bytes the save makes; a truncate with no
    /// WRITE after it is decided when it lands.
    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let (uid, gid) = owner_change(&setattr.uid, &setattr.gid);
        if uid.is_some() || gid.is_some() {
            return Err(nfsstat3::NFS3ERR_PERM);
        }
        if let set_size3::size(size) = setattr.size {
            let handle = self.handle(id)?;
            let truncate = SetAttr {
                size: Some(size),
                ..SetAttr::default()
            };
            status(self.fs.setattr(id, Some(handle), truncate))?;
            self.stage(id, Stage::Truncated(Instant::now()));
        }
        if let set_mode3::mode(bits) = setattr.mode {
            let mode = SetAttr {
                mode: Some(bits),
                ..SetAttr::default()
            };
            status(self.fs.setattr(id, self.staged(id), mode))?;
        }
        self.attr(id)
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let handle = self.handle(id)?;
        status(self.fs.reread(handle))?;
        let data = status(self.fs.read(handle, offset, count))?;
        let (attr, _) = status(self.fs.getattr(id, Some(handle)))?;
        let eof = offset + data.len() as u64 >= attr.size;
        Ok((data, eof))
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        Ok(self.write_stable(id, offset, data, false).await?.0)
    }

    /// Stage the chunk, checked. An UNSTABLE write waits for the client's
    /// COMMIT; a FILE_SYNC or DATA_SYNC write is committed before it is
    /// answered, and a refused commit fails it. A refused WRITE drops what
    /// the handle staged before it (the save's truncate), so nothing of the
    /// refused save lands.
    async fn write_stable(
        &self,
        id: fileid3,
        offset: u64,
        data: &[u8],
        stable: bool,
    ) -> Result<(fattr3, bool), nfsstat3> {
        let handle = self.handle(id)?;
        if let Err(errno) = self.fs.write_checked(handle, offset, data) {
            self.forget(id);
            return Err(nfs_status(errno));
        }
        self.stage(id, Stage::Written);
        if stable {
            status(self.commit_id(id))?;
        }
        Ok((self.attr(id)?, stable))
    }

    /// The client's COMMIT (`fsync`, `close`): the file id's FLUSH, whatever
    /// range is named, since a file lands whole.
    async fn commit(&self, id: fileid3, _offset: u64, _count: u32) -> Result<(), nfsstat3> {
        status(self.commit_id(id))
    }

    /// CREATE lands the empty file, with the attributes' mode.
    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        attr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        self.commit_all();
        let (entry, handle) = status(self.fs.create(dirid, name(filename), O_RDWR))?;
        let id = entry.attr.inode;
        if let set_mode3::mode(bits) = attr.mode {
            let mode = SetAttr {
                mode: Some(bits),
                ..SetAttr::default()
            };
            if let Err(errno) = self.fs.setattr(id, Some(handle), mode) {
                self.fs.release(handle);
                return Err(nfs_status(errno));
            }
        }
        if let Err(errno) = self.fs.flush(handle) {
            self.fs.release(handle);
            return Err(nfs_status(errno));
        }
        self.opens.lock().insert(
            id,
            Open {
                handle,
                stage: Stage::Clean,
            },
        );
        Ok((id, self.attr(id)?))
    }

    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<fileid3, nfsstat3> {
        if let Ok(entry) = self.fs.lookup(dirid, name(filename)) {
            return Ok(entry.attr.inode);
        }
        Ok(self.create(dirid, filename, sattr3::default()).await?.0)
    }

    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        self.commit_all();
        let entry = status(self.fs.mkdir(dirid, name(dirname)))?;
        Ok((entry.attr.inode, fattr(&entry.attr)))
    }

    /// REMOVE and RMDIR both arrive here: a directory is an RMDIR.
    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        self.commit_all();
        let entry = status(self.fs.lookup(dirid, name(filename)))?;
        if entry.attr.kind == Kind::Directory {
            status(self.fs.rmdir(dirid, name(filename)))
        } else {
            self.forget(entry.attr.inode);
            status(self.fs.unlink(dirid, name(filename)))
        }
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        self.commit_all();
        if let Ok(replaced) = self.fs.lookup(to_dirid, name(to_filename)) {
            self.forget(replaced.attr.inode);
        }
        status(self.fs.rename(
            from_dirid,
            name(from_filename),
            to_dirid,
            name(to_filename),
            0,
        ))
    }

    /// The directory's entries after the one whose id is `start_after`.
    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let listed = status(self.fs.readdirplus(dirid, 0))?;
        let skip = match start_after {
            0 => 0,
            after => {
                listed
                    .iter()
                    .position(|(dirent, _)| dirent.inode == after)
                    .ok_or(nfsstat3::NFS3ERR_BAD_COOKIE)?
                    + 1
            }
        };
        let rest = &listed[skip..];
        let entries: Vec<DirEntry> = rest
            .iter()
            .take(max_entries)
            .map(|(dirent, entry)| DirEntry {
                fileid: dirent.inode,
                name: nfsstring(dirent.name.as_bytes().to_vec()),
                attr: fattr(&entry.attr),
            })
            .collect();
        Ok(ReadDirResult {
            end: entries.len() == rest.len(),
            entries,
        })
    }

    async fn symlink(
        &self,
        dirid: fileid3,
        linkname: &filename3,
        symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        self.commit_all();
        let entry = status(self.fs.symlink(dirid, name(linkname), &symlink.0))?;
        Ok((entry.attr.inode, fattr(&entry.attr)))
    }

    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsstat3> {
        Ok(nfsstring(status(self.fs.readlink(id))?))
    }
}

