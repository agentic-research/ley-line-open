//! ADR-0044 open question 1 spike: a virtio-fs device whose FUSE requests are
//! answered from this process's memory, attached to a libkrun microVM.
//!
//! Usage: llo-virtiofs-spike <rootfs-dir> <dax-dir>
//!
//! `<rootfs-dir>` holds the guest program at `/guest` and the mount points
//! `/llo` and `/dax`. The `llo` virtio-fs tag is served by `MemFs` below; no
//! host directory backs it. `<dax-dir>` is attached as an ordinary passthrough
//! share with a DAX window so the guest can probe whether DAX maps on this host.
//!
//! `krun_start_enter` does not return: the process exits with the guest's
//! exit status. Every backend decision is logged to stderr as it happens.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::io;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use krun::virtio_fs::dyn_fs::DynFileSystem;
use krun::virtio_fs::filesystem::{Context, DirEntry, Entry, FsOptions, OpenOptions, SetattrValid};

#[cfg(target_os = "macos")]
type Stat = libc::stat;
#[cfg(target_os = "linux")]
type Stat = libc::stat64;

// The guest is Linux: replies carry Linux errno values whatever the host is.
const LINUX_ENOENT: i32 = 2;
const LINUX_EIO: i32 = 5;
const LINUX_EBADF: i32 = 9;
const LINUX_EACCES: i32 = 13;
const LINUX_ENOTDIR: i32 = 20;
const LINUX_ESTALE: i32 = 116;

const ROOT: u64 = 1;
const TTL: Duration = Duration::from_secs(0);

fn err<T>(errno: i32) -> io::Result<T> {
    Err(io::Error::from_raw_os_error(errno))
}

fn log(msg: impl AsRef<str>) {
    eprintln!("[llo-backend] {}", msg.as_ref());
}

/// One file of the in-memory store: its committed bytes and a version that a
/// commit bumps. `version` stands in for ADR-0042's base check.
struct Node {
    name: &'static str,
    bytes: Vec<u8>,
    version: u64,
    writable: bool,
}

/// Per-open state: the version the open read (its base) and the bytes the
/// guest has written through this handle.
struct Open {
    inode: u64,
    base_version: u64,
    buf: Vec<u8>,
    dirty: bool,
}

struct MemFs {
    nodes: Mutex<HashMap<u64, Node>>,
    opens: Mutex<HashMap<u64, Open>>,
    next_handle: AtomicU64,
}

impl MemFs {
    fn new(token: &str) -> Self {
        let mut nodes = HashMap::new();
        let mut add = |ino: u64, name: &'static str, bytes: Vec<u8>, writable: bool| {
            nodes.insert(
                ino,
                Node {
                    name,
                    bytes,
                    version: 1,
                    writable,
                },
            );
        };
        add(2, "hello.txt", format!("{token}\n").into_bytes(), false);
        add(3, "ok.txt", b"key=base\n".to_vec(), true);
        add(4, "stale.txt", b"key=base\n".to_vec(), true);
        add(5, "syntax.txt", b"key=base\n".to_vec(), true);
        MemFs {
            nodes: Mutex::new(nodes),
            opens: Mutex::new(HashMap::new()),
            next_handle: AtomicU64::new(1),
        }
    }

    fn stat(ino: u64, node: Option<&Node>) -> Stat {
        // SAFETY: stat is plain old data.
        let mut st: Stat = unsafe { std::mem::zeroed() };
        st.st_ino = ino as _;
        match node {
            None => {
                st.st_mode = (libc::S_IFDIR | 0o755) as _;
                st.st_nlink = 2;
            }
            Some(n) => {
                let perm = if n.writable { 0o644 } else { 0o444 };
                st.st_mode = (libc::S_IFREG | perm) as _;
                st.st_nlink = 1;
                st.st_size = n.bytes.len() as _;
                st.st_blocks = n.bytes.len().div_ceil(512) as _;
            }
        }
        st.st_blksize = 4096;
        st
    }

    fn entry(ino: u64, node: &Node) -> Entry {
        Entry {
            inode: ino,
            generation: 0,
            attr: Self::stat(ino, Some(node)),
            attr_flags: 0,
            attr_timeout: TTL,
            entry_timeout: TTL,
        }
    }

    /// ADR-0042 stand-in: a proposal lands only if its base is current and its
    /// bytes parse as `key=value` lines.
    fn propose(&self, handle: u64, op: &str) -> io::Result<()> {
        let mut opens = self.opens.lock().unwrap();
        let open = opens
            .get_mut(&handle)
            .ok_or(io::Error::from_raw_os_error(LINUX_EBADF))?;
        if !open.dirty {
            log(format!("{op} handle={handle}: nothing written, ok"));
            return Ok(());
        }
        let mut nodes = self.nodes.lock().unwrap();
        let node = nodes.get_mut(&open.inode).unwrap();
        if node.version != open.base_version {
            log(format!(
                "{op} handle={handle} {}: base v{} but store at v{} -> ESTALE",
                node.name, open.base_version, node.version
            ));
            return err(LINUX_ESTALE);
        }
        let parses = std::str::from_utf8(&open.buf)
            .map(|s| s.lines().all(|l| l.contains('=')))
            .unwrap_or(false);
        if !parses {
            log(format!(
                "{op} handle={handle} {}: does not parse -> EIO",
                node.name
            ));
            return err(LINUX_EIO);
        }
        node.bytes = open.buf.clone();
        node.version += 1;
        open.base_version = node.version;
        open.dirty = false;
        log(format!(
            "{op} handle={handle} {}: committed v{} ({} bytes)",
            node.name,
            node.version,
            node.bytes.len()
        ));
        Ok(())
    }
}

impl DynFileSystem for MemFs {
    fn init(&self, _capable: FsOptions) -> io::Result<FsOptions> {
        log("INIT");
        Ok(FsOptions::empty())
    }

    fn lookup(&self, _ctx: Context, parent: u64, name: &CStr) -> io::Result<Entry> {
        if parent != ROOT {
            return err(LINUX_ENOTDIR);
        }
        let nodes = self.nodes.lock().unwrap();
        let found = nodes
            .iter()
            .find(|(_, n)| n.name.as_bytes() == name.to_bytes());
        match found {
            Some((&ino, node)) => Ok(Self::entry(ino, node)),
            None => err(LINUX_ENOENT),
        }
    }

    fn getattr(&self, _ctx: Context, inode: u64, _h: Option<u64>) -> io::Result<(Stat, Duration)> {
        if inode == ROOT {
            return Ok((Self::stat(ROOT, None), TTL));
        }
        let nodes = self.nodes.lock().unwrap();
        match nodes.get(&inode) {
            Some(n) => Ok((Self::stat(inode, Some(n)), TTL)),
            None => err(LINUX_ENOENT),
        }
    }

    fn setattr(
        &self,
        ctx: Context,
        inode: u64,
        _attr: Stat,
        handle: Option<u64>,
        _valid: SetattrValid,
    ) -> io::Result<(Stat, Duration)> {
        self.getattr(ctx, inode, handle)
    }

    fn open(
        &self,
        _ctx: Context,
        inode: u64,
        _kill_priv: bool,
        flags: u32,
    ) -> io::Result<(Option<u64>, OpenOptions)> {
        let mut nodes = self.nodes.lock().unwrap();
        let node = nodes
            .get_mut(&inode)
            .ok_or(io::Error::from_raw_os_error(LINUX_ENOENT))?;
        let wants_write = (flags as i32 & libc::O_ACCMODE) != libc::O_RDONLY;
        if wants_write && !node.writable {
            return err(LINUX_EACCES);
        }
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        let base_version = node.version;
        log(format!(
            "OPEN {} handle={handle} base=v{base_version} write={wants_write}",
            node.name
        ));
        if node.name == "stale.txt" && wants_write {
            // Another writer commits after this open read its base.
            node.version += 1;
            log(format!(
                "  concurrent commit moves stale.txt to v{}",
                node.version
            ));
        }
        self.opens.lock().unwrap().insert(
            handle,
            Open {
                inode,
                base_version,
                buf: node.bytes.clone(),
                dirty: false,
            },
        );
        Ok((Some(handle), OpenOptions::DIRECT_IO))
    }

    fn read(
        &self,
        _ctx: Context,
        inode: u64,
        handle: u64,
        w: &mut dyn io::Write,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> io::Result<usize> {
        let opens = self.opens.lock().unwrap();
        let bytes = match opens.get(&handle) {
            Some(o) => o.buf.clone(),
            None => self.nodes.lock().unwrap()[&inode].bytes.clone(),
        };
        let start = (offset as usize).min(bytes.len());
        let end = (start + size as usize).min(bytes.len());
        w.write_all(&bytes[start..end])?;
        log(format!(
            "READ inode={inode} off={offset} -> {} bytes from memory",
            end - start
        ));
        Ok(end - start)
    }

    fn write(
        &self,
        _ctx: Context,
        _inode: u64,
        handle: u64,
        r: &mut dyn io::Read,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _delayed_write: bool,
        _kill_priv: bool,
        _flags: u32,
    ) -> io::Result<usize> {
        let mut data = vec![0u8; size as usize];
        r.read_exact(&mut data)?;
        let mut opens = self.opens.lock().unwrap();
        let open = opens
            .get_mut(&handle)
            .ok_or(io::Error::from_raw_os_error(LINUX_EBADF))?;
        let end = offset as usize + data.len();
        if open.buf.len() < end {
            open.buf.resize(end, 0);
        }
        open.buf[offset as usize..end].copy_from_slice(&data);
        open.buf.truncate(end);
        open.dirty = true;
        log(format!(
            "WRITE handle={handle} off={offset} len={size} (held, not committed)"
        ));
        Ok(data.len())
    }

    fn flush(&self, _ctx: Context, _inode: u64, handle: u64, _lock_owner: u64) -> io::Result<()> {
        self.propose(handle, "FLUSH")
    }

    fn fsync(&self, _ctx: Context, _inode: u64, _datasync: bool, handle: u64) -> io::Result<()> {
        self.propose(handle, "FSYNC")
    }

    fn release(
        &self,
        _ctx: Context,
        _inode: u64,
        _flags: u32,
        handle: u64,
        _flush: bool,
        _flock_release: bool,
        _lock_owner: Option<u64>,
    ) -> io::Result<()> {
        if let Some(open) = self.opens.lock().unwrap().remove(&handle) {
            log(format!(
                "RELEASE handle={handle} dirty_discarded={}",
                open.dirty
            ));
        }
        Ok(())
    }

    fn readdir(
        &self,
        _ctx: Context,
        inode: u64,
        _handle: u64,
        _size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(DirEntry) -> io::Result<usize>,
    ) -> io::Result<()> {
        if inode != ROOT {
            return err(LINUX_ENOTDIR);
        }
        let nodes = self.nodes.lock().unwrap();
        let mut inos: Vec<_> = nodes.keys().copied().collect();
        inos.sort();
        for (i, ino) in inos.into_iter().enumerate().skip(offset as usize) {
            let added = add_entry(DirEntry {
                ino,
                offset: i as u64 + 1,
                type_: libc::DT_REG as u32,
                name: nodes[&ino].name.as_bytes(),
            })?;
            if added == 0 {
                break;
            }
        }
        Ok(())
    }

    fn opendir(
        &self,
        _ctx: Context,
        _inode: u64,
        _flags: u32,
    ) -> io::Result<(Option<u64>, OpenOptions)> {
        Ok((Some(0), OpenOptions::empty()))
    }

    fn access(&self, _ctx: Context, _inode: u64, _mask: u32) -> io::Result<()> {
        Ok(())
    }

    /// DAX from memory: copy the committed bytes into page-aligned anonymous
    /// memory and hand its address to the VMM, which `hv_vm_map`s it into the
    /// guest's window. The store's real blob arena would be mapped in place.
    fn dax_host_addr(
        &self,
        _ctx: Context,
        inode: u64,
        _handle: u64,
        foffset: u64,
        len: u64,
        writable: bool,
    ) -> io::Result<u64> {
        if writable {
            // Writes through a DAX window bypass WRITE/FLUSH; refuse them.
            log(format!("SETUPMAPPING inode={inode} writable -> EACCES"));
            return err(LINUX_EACCES);
        }
        let nodes = self.nodes.lock().unwrap();
        let node = nodes
            .get(&inode)
            .ok_or(io::Error::from_raw_os_error(LINUX_ENOENT))?;
        // SAFETY: fresh private anonymous mapping, released in dax_unmapped.
        let p = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return err(LINUX_EIO);
        }
        let start = (foffset as usize).min(node.bytes.len());
        let n = (node.bytes.len() - start).min(len as usize);
        // SAFETY: p has len bytes; n <= len.
        unsafe { ptr::copy_nonoverlapping(node.bytes[start..].as_ptr(), p as *mut u8, n) };
        log(format!(
            "SETUPMAPPING {} foffset={foffset} len={len} -> host memory {p:p} ({n} bytes of content)",
            node.name
        ));
        Ok(p as u64)
    }

    fn dax_unmapped(&self, host_addr: u64, len: u64) {
        log(format!(
            "REMOVEMAPPING host memory {host_addr:#x} len={len}"
        ));
        // SAFETY: host_addr/len came from dax_host_addr.
        unsafe { libc::munmap(host_addr as *mut libc::c_void, len as usize) };
    }
}

fn check(name: &str, ret: i32) {
    if ret < 0 {
        eprintln!("{name} failed: {}", io::Error::from_raw_os_error(-ret));
        std::process::exit(1);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (rootfs, dax_dir) = match args.as_slice() {
        [_, r, d] => (r.clone(), d.clone()),
        _ => {
            eprintln!("usage: llo-virtiofs-spike <rootfs-dir> <dax-dir>");
            std::process::exit(2);
        }
    };

    // Bytes that exist only in this process: no file anywhere holds them.
    let token = format!(
        "from-llo-memory pid={} t={}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    log(format!("token: {token}"));
    let backend = Arc::new(MemFs::new(&token));

    let root = CString::new(rootfs).unwrap();
    let dax = CString::new(dax_dir).unwrap();
    let dax_tag = CString::new("hostdax").unwrap();
    let exec = CString::new("/guest").unwrap();
    let argv0 = CString::new("guest").unwrap();
    let expect = CString::new(format!("EXPECT={token}")).unwrap();
    let argv = [argv0.as_ptr(), ptr::null()];
    let envp = [expect.as_ptr(), ptr::null()];

    // SAFETY: every pointer is a live NUL-terminated string or a NULL-terminated
    // array of them; libkrun copies what it keeps.
    if let Some(level) = std::env::var("LLO_SPIKE_KRUN_LOG")
        .ok()
        .and_then(|l| l.parse().ok())
    {
        krun::krun_set_log_level(level);
    }
    unsafe {
        let ctx = krun::krun_create_ctx();
        check("krun_create_ctx", ctx);
        let ctx = ctx as u32;
        check("krun_set_vm_config", krun::krun_set_vm_config(ctx, 1, 512));
        check("krun_set_root", krun::krun_set_root(ctx, root.as_ptr()));
        check(
            "krun_add_virtiofs_backend",
            krun::krun_add_virtiofs_backend(ctx, "llo", 0, backend.clone()),
        );
        // The same backend behind a second device that has a 64 MiB DAX window.
        check(
            "krun_add_virtiofs_backend(dax)",
            krun::krun_add_virtiofs_backend(ctx, "llodax", 1 << 26, backend),
        );
        check(
            "krun_add_virtiofs2",
            krun::krun_add_virtiofs2(ctx, dax_tag.as_ptr(), dax.as_ptr(), 1 << 26),
        );
        check(
            "krun_set_exec",
            krun::krun_set_exec(ctx, exec.as_ptr(), argv.as_ptr(), envp.as_ptr()),
        );
        check("krun_start_enter", krun::krun_start_enter(ctx));
    }
}
