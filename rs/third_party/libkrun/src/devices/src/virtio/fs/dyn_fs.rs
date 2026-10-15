// An object-safe virtio-fs backend supplied by the embedding process.
//
// `FileSystem` has associated types and generic `read`/`write`/`readdir`, so it
// cannot be stored as a trait object. `DynFileSystem` is the object-safe
// subset an embedder implements; `DynFs` adapts it to `FileSystem` so the
// existing `Server` dispatches FUSE requests to it unchanged.
//
// Errors are Linux errno values (the guest is Linux). On macOS hosts use
// `crate::virtio::linux_errno::linux_error` to translate host errno values.

use std::collections::HashMap;
use std::ffi::CStr;
use std::fmt;
use std::io;
use std::mem;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(target_os = "macos")]
use crossbeam_channel::{unbounded, Sender};
#[cfg(target_os = "macos")]
use utils::worker_message::WorkerMessage;

use super::bindings;
#[cfg(target_os = "macos")]
use super::filesystem::RemovemappingOne;
use super::filesystem::{
    Context, DirEntry, Entry, Extensions, FileSystem, FsOptions, OpenOptions, SetattrValid,
    ZeroCopyReader, ZeroCopyWriter,
};
#[cfg(target_os = "macos")]
use super::fuse::SetupmappingFlags;

#[cfg(target_os = "macos")]
const LINUX_EIO: i32 = 5;
#[cfg(target_os = "macos")]
const LINUX_EINVAL: i32 = 22;

fn enosys<T>() -> io::Result<T> {
    Err(io::Error::from_raw_os_error(bindings::LINUX_ENOSYS))
}

/// FUSE request handlers for a virtio-fs device served from the embedder's memory.
#[allow(unused_variables, clippy::too_many_arguments)]
pub trait DynFileSystem: Send + Sync {
    fn init(&self, capable: FsOptions) -> io::Result<FsOptions> {
        Ok(FsOptions::empty())
    }
    fn destroy(&self) {}
    fn lookup(&self, ctx: Context, parent: u64, name: &CStr) -> io::Result<Entry> {
        enosys()
    }
    fn forget(&self, ctx: Context, inode: u64, count: u64) {}
    fn getattr(
        &self,
        ctx: Context,
        inode: u64,
        handle: Option<u64>,
    ) -> io::Result<(bindings::stat64, Duration)> {
        enosys()
    }
    fn setattr(
        &self,
        ctx: Context,
        inode: u64,
        attr: bindings::stat64,
        handle: Option<u64>,
        valid: SetattrValid,
    ) -> io::Result<(bindings::stat64, Duration)> {
        enosys()
    }
    fn mkdir(
        &self,
        ctx: Context,
        parent: u64,
        name: &CStr,
        mode: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        enosys()
    }
    fn unlink(&self, ctx: Context, parent: u64, name: &CStr) -> io::Result<()> {
        enosys()
    }
    fn rmdir(&self, ctx: Context, parent: u64, name: &CStr) -> io::Result<()> {
        enosys()
    }
    fn rename(
        &self,
        ctx: Context,
        olddir: u64,
        oldname: &CStr,
        newdir: u64,
        newname: &CStr,
        flags: u32,
    ) -> io::Result<()> {
        enosys()
    }
    fn open(
        &self,
        ctx: Context,
        inode: u64,
        kill_priv: bool,
        flags: u32,
    ) -> io::Result<(Option<u64>, OpenOptions)> {
        Ok((None, OpenOptions::empty()))
    }
    fn create(
        &self,
        ctx: Context,
        parent: u64,
        name: &CStr,
        mode: u32,
        kill_priv: bool,
        flags: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<(Entry, Option<u64>, OpenOptions)> {
        enosys()
    }
    fn read(
        &self,
        ctx: Context,
        inode: u64,
        handle: u64,
        w: &mut dyn io::Write,
        size: u32,
        offset: u64,
        lock_owner: Option<u64>,
        flags: u32,
    ) -> io::Result<usize> {
        enosys()
    }
    fn write(
        &self,
        ctx: Context,
        inode: u64,
        handle: u64,
        r: &mut dyn io::Read,
        size: u32,
        offset: u64,
        lock_owner: Option<u64>,
        delayed_write: bool,
        kill_priv: bool,
        flags: u32,
    ) -> io::Result<usize> {
        enosys()
    }
    fn flush(&self, ctx: Context, inode: u64, handle: u64, lock_owner: u64) -> io::Result<()> {
        enosys()
    }
    fn fsync(&self, ctx: Context, inode: u64, datasync: bool, handle: u64) -> io::Result<()> {
        enosys()
    }
    fn release(
        &self,
        ctx: Context,
        inode: u64,
        flags: u32,
        handle: u64,
        flush: bool,
        flock_release: bool,
        lock_owner: Option<u64>,
    ) -> io::Result<()> {
        Ok(())
    }
    fn statfs(&self, ctx: Context, inode: u64) -> io::Result<bindings::statvfs64> {
        // SAFETY: statvfs64 is plain old data.
        let mut st: bindings::statvfs64 = unsafe { mem::zeroed() };
        st.f_namemax = 255;
        st.f_bsize = 512;
        Ok(st)
    }
    fn opendir(
        &self,
        ctx: Context,
        inode: u64,
        flags: u32,
    ) -> io::Result<(Option<u64>, OpenOptions)> {
        Ok((None, OpenOptions::empty()))
    }
    fn readdir(
        &self,
        ctx: Context,
        inode: u64,
        handle: u64,
        size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(DirEntry) -> io::Result<usize>,
    ) -> io::Result<()> {
        enosys()
    }
    /// READDIRPLUS: each entry with its `Entry`, so the guest needs no LOOKUP
    /// per name. Advertise `FsOptions::DO_READDIRPLUS` from `init` to receive
    /// it. Each returned `Entry` counts as one lookup (the kernel FORGETs it).
    fn readdirplus(
        &self,
        ctx: Context,
        inode: u64,
        handle: u64,
        size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(DirEntry, Entry) -> io::Result<usize>,
    ) -> io::Result<()> {
        enosys()
    }
    fn releasedir(&self, ctx: Context, inode: u64, flags: u32, handle: u64) -> io::Result<()> {
        Ok(())
    }
    fn access(&self, ctx: Context, inode: u64, mask: u32) -> io::Result<()> {
        enosys()
    }

    /// DAX: return the host address of `len` bytes holding the file's contents
    /// from `foffset`. The memory must be host-page aligned and stay valid
    /// until `dax_unmapped` is called with the same address. macOS only: the
    /// VMM maps it into the guest's DAX window with `hv_vm_map`.
    fn dax_host_addr(
        &self,
        ctx: Context,
        inode: u64,
        handle: u64,
        foffset: u64,
        len: u64,
        writable: bool,
    ) -> io::Result<u64> {
        enosys()
    }

    /// DAX: the guest no longer maps `host_addr`.
    fn dax_unmapped(&self, host_addr: u64, len: u64) {}
}

/// A cloneable handle to an embedder-supplied backend.
#[derive(Clone)]
pub struct CustomFsBackend(pub Arc<dyn DynFileSystem>);

impl fmt::Debug for CustomFsBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CustomFsBackend")
    }
}

/// Adapts a `DynFileSystem` to the generic `FileSystem` the `Server` drives.
/// The second field tracks live DAX windows (guest addr -> host addr, len);
/// only the macOS mapping path uses it.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub struct DynFs(pub Arc<dyn DynFileSystem>, Mutex<HashMap<u64, (u64, u64)>>);

impl DynFs {
    pub fn new(backend: Arc<dyn DynFileSystem>) -> Self {
        DynFs(backend, Mutex::new(HashMap::new()))
    }
}

impl FileSystem for DynFs {
    type Inode = u64;
    type Handle = u64;

    #[cfg(target_os = "macos")]
    fn setupmapping(
        &self,
        ctx: Context,
        inode: u64,
        handle: u64,
        foffset: u64,
        len: u64,
        flags: u64,
        moffset: u64,
        guest_shm_base: u64,
        shm_size: u64,
        map_sender: &Option<Sender<WorkerMessage>>,
    ) -> io::Result<()> {
        let sender = map_sender
            .as_ref()
            .ok_or_else(|| io::Error::from_raw_os_error(bindings::LINUX_ENOSYS))?;
        if moffset.checked_add(len).is_none_or(|end| end > shm_size) {
            return Err(io::Error::from_raw_os_error(LINUX_EINVAL));
        }
        let writable = flags & SetupmappingFlags::WRITE.bits() != 0;
        let host_addr = self
            .0
            .dax_host_addr(ctx, inode, handle, foffset, len, writable)?;
        let guest_addr = guest_shm_base + moffset;
        let (reply_sender, reply_receiver) = unbounded();
        sender
            .send(WorkerMessage::GpuAddMapping(
                reply_sender,
                host_addr,
                guest_addr,
                len,
            ))
            .map_err(|_| io::Error::from_raw_os_error(LINUX_EIO))?;
        if !reply_receiver.recv().unwrap_or(false) {
            self.0.dax_unmapped(host_addr, len);
            return Err(io::Error::from_raw_os_error(LINUX_EINVAL));
        }
        if let Some((old, old_len)) = self.1.lock().unwrap().insert(guest_addr, (host_addr, len)) {
            self.0.dax_unmapped(old, old_len);
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn removemapping(
        &self,
        _ctx: Context,
        requests: Vec<RemovemappingOne>,
        guest_shm_base: u64,
        shm_size: u64,
        map_sender: &Option<Sender<WorkerMessage>>,
    ) -> io::Result<()> {
        let sender = map_sender
            .as_ref()
            .ok_or_else(|| io::Error::from_raw_os_error(bindings::LINUX_ENOSYS))?;
        for req in requests {
            if req
                .moffset
                .checked_add(req.len)
                .is_none_or(|end| end > shm_size)
            {
                return Err(io::Error::from_raw_os_error(LINUX_EINVAL));
            }
            let guest_addr = guest_shm_base + req.moffset;
            let (reply_sender, reply_receiver) = unbounded();
            sender
                .send(WorkerMessage::GpuRemoveMapping(
                    reply_sender,
                    guest_addr,
                    req.len,
                ))
                .map_err(|_| io::Error::from_raw_os_error(LINUX_EIO))?;
            if !reply_receiver.recv().unwrap_or(false) {
                return Err(io::Error::from_raw_os_error(LINUX_EINVAL));
            }
            if let Some((host_addr, len)) = self.1.lock().unwrap().remove(&guest_addr) {
                self.0.dax_unmapped(host_addr, len);
            }
        }
        Ok(())
    }

    fn init(&self, capable: FsOptions) -> io::Result<FsOptions> {
        self.0.init(capable)
    }
    fn destroy(&self) {
        self.0.destroy()
    }
    fn lookup(&self, ctx: Context, parent: u64, name: &CStr) -> io::Result<Entry> {
        self.0.lookup(ctx, parent, name)
    }
    fn forget(&self, ctx: Context, inode: u64, count: u64) {
        self.0.forget(ctx, inode, count)
    }
    fn getattr(
        &self,
        ctx: Context,
        inode: u64,
        handle: Option<u64>,
    ) -> io::Result<(bindings::stat64, Duration)> {
        self.0.getattr(ctx, inode, handle)
    }
    fn setattr(
        &self,
        ctx: Context,
        inode: u64,
        attr: bindings::stat64,
        handle: Option<u64>,
        valid: SetattrValid,
    ) -> io::Result<(bindings::stat64, Duration)> {
        self.0.setattr(ctx, inode, attr, handle, valid)
    }
    fn mkdir(
        &self,
        ctx: Context,
        parent: u64,
        name: &CStr,
        mode: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        self.0.mkdir(ctx, parent, name, mode, umask, extensions)
    }
    fn unlink(&self, ctx: Context, parent: u64, name: &CStr) -> io::Result<()> {
        self.0.unlink(ctx, parent, name)
    }
    fn rmdir(&self, ctx: Context, parent: u64, name: &CStr) -> io::Result<()> {
        self.0.rmdir(ctx, parent, name)
    }
    fn rename(
        &self,
        ctx: Context,
        olddir: u64,
        oldname: &CStr,
        newdir: u64,
        newname: &CStr,
        flags: u32,
    ) -> io::Result<()> {
        self.0.rename(ctx, olddir, oldname, newdir, newname, flags)
    }
    fn open(
        &self,
        ctx: Context,
        inode: u64,
        kill_priv: bool,
        flags: u32,
    ) -> io::Result<(Option<u64>, OpenOptions)> {
        self.0.open(ctx, inode, kill_priv, flags)
    }
    fn create(
        &self,
        ctx: Context,
        parent: u64,
        name: &CStr,
        mode: u32,
        kill_priv: bool,
        flags: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<(Entry, Option<u64>, OpenOptions)> {
        self.0
            .create(ctx, parent, name, mode, kill_priv, flags, umask, extensions)
    }
    fn read<W: io::Write + ZeroCopyWriter>(
        &self,
        ctx: Context,
        inode: u64,
        handle: u64,
        mut w: W,
        size: u32,
        offset: u64,
        lock_owner: Option<u64>,
        flags: u32,
    ) -> io::Result<usize> {
        self.0
            .read(ctx, inode, handle, &mut w, size, offset, lock_owner, flags)
    }
    fn write<R: io::Read + ZeroCopyReader>(
        &self,
        ctx: Context,
        inode: u64,
        handle: u64,
        mut r: R,
        size: u32,
        offset: u64,
        lock_owner: Option<u64>,
        delayed_write: bool,
        kill_priv: bool,
        flags: u32,
    ) -> io::Result<usize> {
        self.0.write(
            ctx,
            inode,
            handle,
            &mut r,
            size,
            offset,
            lock_owner,
            delayed_write,
            kill_priv,
            flags,
        )
    }
    fn flush(&self, ctx: Context, inode: u64, handle: u64, lock_owner: u64) -> io::Result<()> {
        self.0.flush(ctx, inode, handle, lock_owner)
    }
    fn fsync(&self, ctx: Context, inode: u64, datasync: bool, handle: u64) -> io::Result<()> {
        self.0.fsync(ctx, inode, datasync, handle)
    }
    fn release(
        &self,
        ctx: Context,
        inode: u64,
        flags: u32,
        handle: u64,
        flush: bool,
        flock_release: bool,
        lock_owner: Option<u64>,
    ) -> io::Result<()> {
        self.0
            .release(ctx, inode, flags, handle, flush, flock_release, lock_owner)
    }
    fn statfs(&self, ctx: Context, inode: u64) -> io::Result<bindings::statvfs64> {
        self.0.statfs(ctx, inode)
    }
    fn opendir(
        &self,
        ctx: Context,
        inode: u64,
        flags: u32,
    ) -> io::Result<(Option<u64>, OpenOptions)> {
        self.0.opendir(ctx, inode, flags)
    }
    fn readdir<F>(
        &self,
        ctx: Context,
        inode: u64,
        handle: u64,
        size: u32,
        offset: u64,
        mut add_entry: F,
    ) -> io::Result<()>
    where
        F: FnMut(DirEntry) -> io::Result<usize>,
    {
        self.0
            .readdir(ctx, inode, handle, size, offset, &mut add_entry)
    }
    fn readdirplus<F>(
        &self,
        ctx: Context,
        inode: u64,
        handle: u64,
        size: u32,
        offset: u64,
        mut add_entry: F,
    ) -> io::Result<()>
    where
        F: FnMut(DirEntry, Entry) -> io::Result<usize>,
    {
        self.0
            .readdirplus(ctx, inode, handle, size, offset, &mut add_entry)
    }
    fn releasedir(&self, ctx: Context, inode: u64, flags: u32, handle: u64) -> io::Result<()> {
        self.0.releasedir(ctx, inode, flags, handle)
    }
    fn access(&self, ctx: Context, inode: u64, mask: u32) -> io::Result<()> {
        self.0.access(ctx, inode, mask)
    }
}
