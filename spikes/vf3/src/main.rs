//! V-F3 VMM: boots one libkrun guest whose `/work` is a virtio-fs share served
//! from this process's memory by [`MemFs`], then runs `/vf3guest` in it.
//!
//! Configuration is environment only (the binary is re-run once per guest
//! boot by `run.sh`):
//!
//! - `VF3_CORPUS`  host directory loaded into memory before boot
//! - `VF3_ROOTFS`  guest root filesystem (host directory, libkrun passthrough)
//! - `VF3_VARIANT` comma list: `ttl=<secs>`, `plus` (READDIRPLUS), `cache`
//!   (page cache for read-only opens, cached directories), `dax` (per-inode
//!   DAX for files loaded from the corpus; the guest mounts `dax=inode`)
//! - `VF3_GUEST`   space-separated `KEY=VALUE` pairs passed to the guest's
//!   environment (`VF3_MODE`, `VF3_N`, `VF3_WORKLOADS`, ...)
//! - `VF3_VCPUS`, `VF3_RAM_MIB`
//!
//! Semantics follow `rs/ll-open/storefs` (ADR-0044 V2): Linux errno, a per-
//! handle base taken at OPEN, writes staged per handle and landed at FLUSH or
//! FSYNC. Unlike storefs it supports CREATE, MKDIR, UNLINK, RENAME and full
//! SETATTR so git, tar and the storm workloads run. Two additions answer
//! falsifier questions:
//!
//! - WRITE checks the handle's base against the file's current version and
//!   answers `ESTALE` when another writer moved it.
//! - The control file `/.vf3ctl` (inode 2, never listed) changes the store
//!   "from another writer": `bump <path>` appends a line to the file and
//!   advances its version; `refuse <path>` makes UNLINK and RENAME of that
//!   path answer `ESTALE`.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{CStr, CString};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use leyline_runtime::DigestRef;
use leyline_runtime::backends::libkrun::api::virtio_fs::filesystem::{
    Context, DirEntry, Entry, Extensions, FsOptions, OpenOptions, SetattrValid,
};
use leyline_runtime::backends::libkrun::api::{DynFileSystem, LinkedKrunApi, prepare_vm};
use leyline_runtime::backends::libkrun::plan::{KrunConfig, ResolvedRootfs};

type Stat = libc::stat;

// Linux values: the guest is Linux whatever the host is.
const EPERM: i32 = 1;
const ENOENT: i32 = 2;
const EBADF: i32 = 9;
const EEXIST: i32 = 17;
const ENOTDIR: i32 = 20;
const EISDIR: i32 = 21;
const EINVAL: i32 = 22;
const ENOTEMPTY: i32 = 39;
const ESTALE: i32 = 116;
const O_ACCMODE: u32 = 3;
const O_RDONLY: u32 = 0;
const O_TRUNC: u32 = 0o1000;
const RENAME_NOREPLACE: u32 = 1;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const DT_DIR: u32 = 4;
const DT_REG: u32 = 8;
/// `FUSE_ATTR_DAX`: the kernel maps this inode through the DAX window when
/// mounted `dax=inode`.
const FUSE_ATTR_DAX: u32 = 1 << 1;
const DAX_CHUNK: usize = 2 << 20;

const ROOT: u64 = 1;
const CTL: u64 = 2;
const CTL_NAME: &[u8] = b".vf3ctl";

fn err<T>(errno: i32) -> io::Result<T> {
    Err(io::Error::from_raw_os_error(errno))
}

#[derive(Clone, Copy, Debug, Default)]
struct Variant {
    ttl: Duration,
    plus: bool,
    cache: bool,
    dax: bool,
}

impl Variant {
    fn parse(spec: &str) -> Self {
        let mut v = Variant::default();
        for part in spec.split(',').filter(|p| !p.is_empty()) {
            match part {
                "plus" => v.plus = true,
                "cache" => v.cache = true,
                "dax" => v.dax = true,
                p if p.starts_with("ttl=") => {
                    v.ttl = Duration::from_secs(p[4..].parse().expect("ttl seconds"))
                }
                p => panic!("unknown variant flag {p}"),
            }
        }
        v
    }
}

enum Kind {
    Dir(BTreeMap<Vec<u8>, u64>),
    File(Arc<Vec<u8>>),
}

struct Node {
    parent: u64,
    name: Vec<u8>,
    kind: Kind,
    mode: u32,
    mtime: (i64, i64),
    version: u64,
    /// Loaded from the corpus (DAX-eligible), as opposed to created by the guest.
    corpus: bool,
    refuse_ns: bool,
}

struct Handle {
    ino: u64,
    base_version: u64,
    base: Arc<Vec<u8>>,
    staged: Option<Vec<u8>>,
    writable: bool,
}

struct DaxRegion {
    addr: usize,
}

pub struct MemFs {
    v: Variant,
    nodes: RwLock<Vec<Option<Node>>>,
    handles: Mutex<HashMap<u64, Handle>>,
    next_fh: AtomicU64,
    dax: Mutex<HashMap<u64, DaxRegion>>,
    stats: Mutex<BTreeMap<&'static str, u64>>,
}

fn now() -> (i64, i64) {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    (d.as_secs() as i64, d.subsec_nanos() as i64)
}

impl MemFs {
    fn load(root: &Path, v: Variant) -> (Self, usize, usize) {
        let mut nodes: Vec<Option<Node>> = vec![None, None, None];
        nodes[ROOT as usize] = Some(Node {
            parent: ROOT,
            name: Vec::new(),
            kind: Kind::Dir(BTreeMap::new()),
            mode: S_IFDIR | 0o755,
            mtime: now(),
            version: 1,
            corpus: true,
            refuse_ns: false,
        });
        let mut files = 0usize;
        let mut bytes = 0usize;
        let mut stack = vec![(root.to_path_buf(), ROOT)];
        while let Some((dir, ino)) = stack.pop() {
            let mut entries: Vec<_> = std::fs::read_dir(&dir)
                .expect("read corpus dir")
                .map(|e| e.expect("dir entry"))
                .collect();
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let meta = std::fs::symlink_metadata(entry.path()).expect("corpus metadata");
                let name = entry.file_name().into_encoded_bytes();
                let child = nodes.len() as u64;
                let mtime = (meta.mtime(), meta.mtime_nsec());
                let node = if meta.is_dir() {
                    stack.push((entry.path(), child));
                    Node {
                        parent: ino,
                        name: name.clone(),
                        kind: Kind::Dir(BTreeMap::new()),
                        mode: S_IFDIR | (meta.mode() & 0o7777),
                        mtime,
                        version: 1,
                        corpus: true,
                        refuse_ns: false,
                    }
                } else if meta.is_file() {
                    let content = std::fs::read(entry.path()).expect("corpus file");
                    files += 1;
                    bytes += content.len();
                    Node {
                        parent: ino,
                        name: name.clone(),
                        kind: Kind::File(Arc::new(content)),
                        mode: S_IFREG | (meta.mode() & 0o7777),
                        mtime,
                        version: 1,
                        corpus: true,
                        refuse_ns: false,
                    }
                } else {
                    continue;
                };
                nodes.push(Some(node));
                if let Some(Node {
                    kind: Kind::Dir(children),
                    ..
                }) = nodes[ino as usize].as_mut()
                {
                    children.insert(name, child);
                }
            }
        }
        let fs = MemFs {
            v,
            nodes: RwLock::new(nodes),
            handles: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            dax: Mutex::new(HashMap::new()),
            stats: Mutex::new(BTreeMap::new()),
        };
        (fs, files, bytes)
    }

    fn count(&self, op: &'static str) {
        *self.stats.lock().unwrap().entry(op).or_default() += 1;
    }

    fn stat_of(ino: u64, node: &Node, size_override: Option<usize>) -> Stat {
        // SAFETY: stat is plain old data; all-zero is a valid value.
        let mut st: Stat = unsafe { std::mem::zeroed() };
        st.st_ino = ino;
        st.st_mode = node.mode as _;
        st.st_blksize = 4096;
        st.st_mtime = node.mtime.0;
        st.st_mtime_nsec = node.mtime.1;
        st.st_ctime = node.mtime.0;
        st.st_ctime_nsec = node.mtime.1;
        st.st_atime = node.mtime.0;
        st.st_atime_nsec = node.mtime.1;
        match &node.kind {
            Kind::Dir(_) => st.st_nlink = 2,
            Kind::File(content) => {
                let size = size_override.unwrap_or(content.len());
                st.st_nlink = 1;
                st.st_size = size as _;
                st.st_blocks = size.div_ceil(512) as _;
            }
        }
        st
    }

    fn ctl_stat() -> Stat {
        // SAFETY: as above.
        let mut st: Stat = unsafe { std::mem::zeroed() };
        st.st_ino = CTL;
        st.st_mode = (S_IFREG | 0o666) as _;
        st.st_nlink = 1;
        st.st_blksize = 4096;
        st
    }

    fn entry(&self, ino: u64, node: &Node) -> Entry {
        let dax = self.v.dax && node.corpus && matches!(node.kind, Kind::File(_));
        Entry {
            inode: ino,
            generation: 0,
            attr: Self::stat_of(ino, node, None),
            attr_flags: if dax { FUSE_ATTR_DAX } else { 0 },
            attr_timeout: self.v.ttl,
            entry_timeout: self.v.ttl,
        }
    }

    fn resolve(nodes: &[Option<Node>], path: &str) -> io::Result<u64> {
        let mut ino = ROOT;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            match nodes[ino as usize].as_ref().map(|n| &n.kind) {
                Some(Kind::Dir(children)) => {
                    ino = *children.get(part.as_bytes()).ok_or(io::Error::from_raw_os_error(ENOENT))?
                }
                _ => return err(ENOTDIR),
            }
        }
        Ok(ino)
    }

    /// The control channel: another writer changes the store.
    fn control(&self, command: &str) -> io::Result<()> {
        let mut nodes = self.nodes.write().unwrap();
        for line in command.lines() {
            if let Some(label) = line.trim().strip_prefix("stats") {
                let stats = self.stats.lock().unwrap();
                let ops: Vec<String> = stats.iter().map(|(k, v)| format!("{k}={v}")).collect();
                eprintln!("VF3OPS{label} {}", ops.join(" "));
                continue;
            }
            match line.trim() {
                "reset" => {
                    self.stats.lock().unwrap().clear();
                    continue;
                }
                _ => {}
            }
            let (verb, path) = line.split_once(' ').ok_or(io::Error::from_raw_os_error(EINVAL))?;
            if verb == "create" {
                // Another writer adds a file the guest has not seen.
                let (dir, name) = path.trim().rsplit_once('/').unwrap_or(("", path.trim()));
                let parent = Self::resolve(&nodes, dir)?;
                let ino = nodes.len() as u64;
                let Some(Node {
                    kind: Kind::Dir(children),
                    ..
                }) = nodes[parent as usize].as_mut()
                else {
                    return err(ENOTDIR);
                };
                children.insert(name.as_bytes().to_vec(), ino);
                nodes.push(Some(Node {
                    parent,
                    name: name.as_bytes().to_vec(),
                    kind: Kind::File(Arc::new(b"created by another writer\n".to_vec())),
                    mode: S_IFREG | 0o644,
                    mtime: now(),
                    version: 1,
                    corpus: false,
                    refuse_ns: false,
                }));
                eprintln!("vf3: control create {path} -> inode {ino}");
                continue;
            }
            let ino = Self::resolve(&nodes, path.trim())?;
            let node = nodes[ino as usize].as_mut().unwrap();
            match verb {
                "bump" => {
                    let Kind::File(content) = &node.kind else {
                        return err(EISDIR);
                    };
                    let mut next = content.as_ref().clone();
                    next.extend_from_slice(b"changed by another writer\n");
                    node.kind = Kind::File(Arc::new(next));
                    node.version += 1;
                    node.mtime = now();
                }
                "refuse" => node.refuse_ns = true,
                _ => return err(EINVAL),
            }
            eprintln!("vf3: control {verb} {path} -> inode {ino} v{}", node.version);
        }
        Ok(())
    }

    fn new_handle(&self, h: Handle) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.handles.lock().unwrap().insert(fh, h);
        fh
    }

    fn file_open_options(&self, writable: bool) -> OpenOptions {
        if writable || !self.v.cache {
            OpenOptions::DIRECT_IO
        } else {
            OpenOptions::KEEP_CACHE
        }
    }

    /// FLUSH and FSYNC: land the staged bytes if the base still holds.
    fn land(&self, handle: u64) -> io::Result<()> {
        let mut handles = self.handles.lock().unwrap();
        let h = handles.get_mut(&handle).ok_or(io::Error::from_raw_os_error(EBADF))?;
        if h.ino == CTL || h.staged.is_none() {
            return Ok(());
        }
        let mut nodes = self.nodes.write().unwrap();
        let node = nodes[h.ino as usize].as_mut().ok_or(io::Error::from_raw_os_error(ENOENT))?;
        if node.version != h.base_version {
            eprintln!(
                "vf3: land inode {} base v{} but store at v{} -> ESTALE",
                h.ino, h.base_version, node.version
            );
            return err(ESTALE);
        }
        let landed = Arc::new(h.staged.take().unwrap());
        node.kind = Kind::File(landed.clone());
        node.version += 1;
        node.mtime = now();
        h.base_version = node.version;
        h.base = landed;
        Ok(())
    }

    fn dump_stats(&self) {
        let stats = self.stats.lock().unwrap();
        let line: Vec<String> = stats.iter().map(|(k, v)| format!("{k}={v}")).collect();
        eprintln!("vf3: backend ops {}", line.join(" "));
    }
}

impl DynFileSystem for MemFs {
    fn init(&self, capable: FsOptions) -> io::Result<FsOptions> {
        let mut want = FsOptions::empty();
        if self.v.plus {
            want |= FsOptions::DO_READDIRPLUS;
        }
        if self.v.dax {
            want |= FsOptions::HAS_INODE_DAX;
        }
        eprintln!(
            "vf3: init variant={:?} kernel_capable_plus={} capable_inode_dax={}",
            self.v,
            capable.contains(FsOptions::DO_READDIRPLUS),
            capable.contains(FsOptions::HAS_INODE_DAX)
        );
        Ok(want)
    }

    fn destroy(&self) {
        self.dump_stats();
    }

    fn lookup(&self, _ctx: Context, parent: u64, name: &CStr) -> io::Result<Entry> {
        self.count("lookup");
        if parent == ROOT && name.to_bytes() == CTL_NAME {
            return Ok(Entry {
                inode: CTL,
                generation: 0,
                attr: Self::ctl_stat(),
                attr_flags: 0,
                attr_timeout: Duration::ZERO,
                entry_timeout: Duration::ZERO,
            });
        }
        let nodes = self.nodes.read().unwrap();
        let Some(Node {
            kind: Kind::Dir(children),
            ..
        }) = nodes.get(parent as usize).and_then(|n| n.as_ref())
        else {
            return err(ENOTDIR);
        };
        match children.get(name.to_bytes()) {
            Some(&ino) => Ok(self.entry(ino, nodes[ino as usize].as_ref().unwrap())),
            // Negative entry: inode 0, cached for the entry timeout.
            None if !self.v.ttl.is_zero() => Ok(Entry {
                inode: 0,
                generation: 0,
                // SAFETY: plain old data.
                attr: unsafe { std::mem::zeroed() },
                attr_flags: 0,
                attr_timeout: self.v.ttl,
                entry_timeout: self.v.ttl,
            }),
            None => err(ENOENT),
        }
    }

    fn getattr(&self, _ctx: Context, inode: u64, handle: Option<u64>) -> io::Result<(Stat, Duration)> {
        self.count("getattr");
        if inode == CTL {
            return Ok((Self::ctl_stat(), Duration::ZERO));
        }
        let size = handle.and_then(|fh| {
            self.handles
                .lock()
                .unwrap()
                .get(&fh)
                .map(|h| h.staged.as_ref().map(|s| s.len()).unwrap_or(h.base.len()))
        });
        let nodes = self.nodes.read().unwrap();
        let node = nodes.get(inode as usize).and_then(|n| n.as_ref()).ok_or(io::Error::from_raw_os_error(ENOENT))?;
        Ok((Self::stat_of(inode, node, size), self.v.ttl))
    }

    fn setattr(
        &self,
        _ctx: Context,
        inode: u64,
        attr: Stat,
        handle: Option<u64>,
        valid: SetattrValid,
    ) -> io::Result<(Stat, Duration)> {
        self.count("setattr");
        if inode == CTL {
            return Ok((Self::ctl_stat(), Duration::ZERO));
        }
        let mut size_override = None;
        if valid.contains(SetattrValid::SIZE) {
            let size = attr.st_size as usize;
            let staged_on_handle = match handle {
                Some(fh) => {
                    let mut handles = self.handles.lock().unwrap();
                    match handles.get_mut(&fh) {
                        Some(h) if h.writable => {
                            let staged = h.staged.get_or_insert_with(|| h.base.as_ref().clone());
                            staged.resize(size, 0);
                            size_override = Some(size);
                            true
                        }
                        _ => false,
                    }
                }
                None => false,
            };
            if !staged_on_handle {
                // truncate(2) with no handle: a change of its own.
                let mut nodes = self.nodes.write().unwrap();
                let node = nodes[inode as usize].as_mut().ok_or(io::Error::from_raw_os_error(ENOENT))?;
                let Kind::File(content) = &node.kind else {
                    return err(EISDIR);
                };
                let mut next = content.as_ref().clone();
                next.resize(size, 0);
                node.kind = Kind::File(Arc::new(next));
                node.version += 1;
                node.mtime = now();
            }
        }
        let mut nodes = self.nodes.write().unwrap();
        let node = nodes[inode as usize].as_mut().ok_or(io::Error::from_raw_os_error(ENOENT))?;
        if valid.contains(SetattrValid::MODE) {
            node.mode = (node.mode & !0o7777) | (attr.st_mode as u32 & 0o7777);
        }
        if valid.contains(SetattrValid::MTIME) {
            node.mtime = (attr.st_mtime, attr.st_mtime_nsec);
        }
        if valid.contains(SetattrValid::MTIME_NOW) {
            node.mtime = now();
        }
        Ok((Self::stat_of(inode, node, size_override), self.v.ttl))
    }

    fn mkdir(
        &self,
        _ctx: Context,
        parent: u64,
        name: &CStr,
        mode: u32,
        umask: u32,
        _extensions: Extensions,
    ) -> io::Result<Entry> {
        self.count("mkdir");
        let mut nodes = self.nodes.write().unwrap();
        let ino = nodes.len() as u64;
        let Some(Node {
            kind: Kind::Dir(children),
            ..
        }) = nodes.get_mut(parent as usize).and_then(|n| n.as_mut())
        else {
            return err(ENOTDIR);
        };
        if children.contains_key(name.to_bytes()) {
            return err(EEXIST);
        }
        children.insert(name.to_bytes().to_vec(), ino);
        let node = Node {
            parent,
            name: name.to_bytes().to_vec(),
            kind: Kind::Dir(BTreeMap::new()),
            mode: S_IFDIR | (mode & !umask & 0o7777),
            mtime: now(),
            version: 1,
            corpus: false,
            refuse_ns: false,
        };
        let entry = self.entry(ino, &node);
        nodes.push(Some(node));
        Ok(entry)
    }

    fn unlink(&self, _ctx: Context, parent: u64, name: &CStr) -> io::Result<()> {
        self.count("unlink");
        let mut nodes = self.nodes.write().unwrap();
        let ino = match nodes.get(parent as usize).and_then(|n| n.as_ref()).map(|n| &n.kind) {
            Some(Kind::Dir(children)) => *children.get(name.to_bytes()).ok_or(io::Error::from_raw_os_error(ENOENT))?,
            _ => return err(ENOTDIR),
        };
        match nodes[ino as usize].as_ref().unwrap() {
            n if n.refuse_ns => {
                eprintln!("vf3: unlink inode {ino} refused -> ESTALE");
                return err(ESTALE);
            }
            Node { kind: Kind::Dir(_), .. } => return err(EISDIR),
            _ => {}
        }
        if let Some(Node {
            kind: Kind::Dir(children),
            ..
        }) = nodes[parent as usize].as_mut()
        {
            children.remove(name.to_bytes());
        }
        nodes[ino as usize] = None;
        Ok(())
    }

    fn rmdir(&self, _ctx: Context, parent: u64, name: &CStr) -> io::Result<()> {
        self.count("rmdir");
        let mut nodes = self.nodes.write().unwrap();
        let ino = match nodes.get(parent as usize).and_then(|n| n.as_ref()).map(|n| &n.kind) {
            Some(Kind::Dir(children)) => *children.get(name.to_bytes()).ok_or(io::Error::from_raw_os_error(ENOENT))?,
            _ => return err(ENOTDIR),
        };
        match &nodes[ino as usize].as_ref().unwrap().kind {
            Kind::Dir(children) if !children.is_empty() => return err(ENOTEMPTY),
            Kind::Dir(_) => {}
            Kind::File(_) => return err(ENOTDIR),
        }
        if let Some(Node {
            kind: Kind::Dir(children),
            ..
        }) = nodes[parent as usize].as_mut()
        {
            children.remove(name.to_bytes());
        }
        nodes[ino as usize] = None;
        Ok(())
    }

    fn rename(
        &self,
        _ctx: Context,
        olddir: u64,
        oldname: &CStr,
        newdir: u64,
        newname: &CStr,
        flags: u32,
    ) -> io::Result<()> {
        self.count("rename");
        if flags & !RENAME_NOREPLACE != 0 {
            return err(EINVAL);
        }
        let mut nodes = self.nodes.write().unwrap();
        let lookup = |nodes: &Vec<Option<Node>>, dir: u64, name: &[u8]| -> io::Result<Option<u64>> {
            match nodes.get(dir as usize).and_then(|n| n.as_ref()).map(|n| &n.kind) {
                Some(Kind::Dir(children)) => Ok(children.get(name).copied()),
                _ => err(ENOTDIR),
            }
        };
        let ino = lookup(&nodes, olddir, oldname.to_bytes())?.ok_or(io::Error::from_raw_os_error(ENOENT))?;
        if nodes[ino as usize].as_ref().unwrap().refuse_ns {
            eprintln!("vf3: rename inode {ino} refused -> ESTALE");
            return err(ESTALE);
        }
        let target = lookup(&nodes, newdir, newname.to_bytes())?;
        if let Some(target) = target {
            if flags & RENAME_NOREPLACE != 0 {
                return err(EEXIST);
            }
            if let Kind::Dir(children) = &nodes[target as usize].as_ref().unwrap().kind {
                if !children.is_empty() {
                    return err(ENOTEMPTY);
                }
            }
            nodes[target as usize] = None;
        }
        if let Some(Node {
            kind: Kind::Dir(children),
            ..
        }) = nodes[olddir as usize].as_mut()
        {
            children.remove(oldname.to_bytes());
        }
        if let Some(Node {
            kind: Kind::Dir(children),
            ..
        }) = nodes[newdir as usize].as_mut()
        {
            children.insert(newname.to_bytes().to_vec(), ino);
        }
        let node = nodes[ino as usize].as_mut().unwrap();
        node.parent = newdir;
        node.name = newname.to_bytes().to_vec();
        Ok(())
    }

    fn open(&self, _ctx: Context, inode: u64, _kill_priv: bool, flags: u32) -> io::Result<(Option<u64>, OpenOptions)> {
        self.count("open");
        let writable = flags & O_ACCMODE != O_RDONLY;
        if inode == CTL {
            let fh = self.new_handle(Handle {
                ino: CTL,
                base_version: 0,
                base: Arc::new(Vec::new()),
                staged: None,
                writable: true,
            });
            return Ok((Some(fh), OpenOptions::DIRECT_IO));
        }
        let nodes = self.nodes.read().unwrap();
        let node = nodes.get(inode as usize).and_then(|n| n.as_ref()).ok_or(io::Error::from_raw_os_error(ENOENT))?;
        let Kind::File(content) = &node.kind else {
            return err(EISDIR);
        };
        let staged = (writable && flags & O_TRUNC != 0).then(Vec::new);
        let h = Handle {
            ino: inode,
            base_version: node.version,
            base: content.clone(),
            staged,
            writable,
        };
        drop(nodes);
        Ok((Some(self.new_handle(h)), self.file_open_options(writable)))
    }

    fn create(
        &self,
        _ctx: Context,
        parent: u64,
        name: &CStr,
        mode: u32,
        _kill_priv: bool,
        flags: u32,
        umask: u32,
        _extensions: Extensions,
    ) -> io::Result<(Entry, Option<u64>, OpenOptions)> {
        self.count("create");
        let mut nodes = self.nodes.write().unwrap();
        let ino = nodes.len() as u64;
        let Some(Node {
            kind: Kind::Dir(children),
            ..
        }) = nodes.get_mut(parent as usize).and_then(|n| n.as_mut())
        else {
            return err(ENOTDIR);
        };
        if children.contains_key(name.to_bytes()) {
            // The kernel sends CREATE only after a negative lookup; O_EXCL
            // semantics fall out of this.
            return err(EEXIST);
        }
        children.insert(name.to_bytes().to_vec(), ino);
        let content = Arc::new(Vec::new());
        let node = Node {
            parent,
            name: name.to_bytes().to_vec(),
            kind: Kind::File(content.clone()),
            mode: S_IFREG | (mode & !umask & 0o7777),
            mtime: now(),
            version: 1,
            corpus: false,
            refuse_ns: false,
        };
        let entry = self.entry(ino, &node);
        nodes.push(Some(node));
        drop(nodes);
        let writable = flags & O_ACCMODE != O_RDONLY;
        let fh = self.new_handle(Handle {
            ino,
            base_version: 1,
            base: content,
            staged: None,
            writable,
        });
        Ok((entry, Some(fh), self.file_open_options(writable)))
    }

    fn read(
        &self,
        _ctx: Context,
        _inode: u64,
        handle: u64,
        w: &mut dyn io::Write,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> io::Result<usize> {
        self.count("read");
        let handles = self.handles.lock().unwrap();
        let h = handles.get(&handle).ok_or(io::Error::from_raw_os_error(EBADF))?;
        let bytes: &[u8] = h.staged.as_deref().unwrap_or(h.base.as_slice());
        let start = (offset as usize).min(bytes.len());
        let end = start.saturating_add(size as usize).min(bytes.len());
        w.write_all(&bytes[start..end])?;
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
        self.count("write");
        let mut data = vec![0u8; size as usize];
        r.read_exact(&mut data)?;
        let mut handles = self.handles.lock().unwrap();
        let h = handles.get_mut(&handle).ok_or(io::Error::from_raw_os_error(EBADF))?;
        if h.ino == CTL {
            drop(handles);
            let command = String::from_utf8_lossy(&data).into_owned();
            if let Err(e) = self.control(&command) {
                eprintln!("vf3: control {command:?} failed: {e}");
                return Err(e);
            }
            return Ok(data.len());
        }
        if !h.writable {
            return err(EBADF);
        }
        // The base check at WRITE (V-F3 D): another writer moved the file.
        let current = self.nodes.read().unwrap()[h.ino as usize].as_ref().map(|n| n.version);
        if current != Some(h.base_version) {
            eprintln!(
                "vf3: write inode {} base v{} but store at v{:?} -> ESTALE",
                h.ino, h.base_version, current
            );
            return err(ESTALE);
        }
        let staged = h.staged.get_or_insert_with(|| h.base.as_ref().clone());
        let offset = offset as usize;
        if staged.len() < offset + data.len() {
            staged.resize(offset + data.len(), 0);
        }
        staged[offset..offset + data.len()].copy_from_slice(&data);
        Ok(data.len())
    }

    fn flush(&self, _ctx: Context, _inode: u64, handle: u64, _lock_owner: u64) -> io::Result<()> {
        self.count("flush");
        self.land(handle)
    }

    fn fsync(&self, _ctx: Context, _inode: u64, _datasync: bool, handle: u64) -> io::Result<()> {
        self.count("fsync");
        self.land(handle)
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
        self.count("release");
        self.handles.lock().unwrap().remove(&handle);
        Ok(())
    }

    fn opendir(&self, _ctx: Context, _inode: u64, _flags: u32) -> io::Result<(Option<u64>, OpenOptions)> {
        self.count("opendir");
        let opts = if self.v.cache {
            OpenOptions::CACHE_DIR | OpenOptions::KEEP_CACHE
        } else {
            OpenOptions::empty()
        };
        Ok((None, opts))
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
        self.count("readdir");
        let nodes = self.nodes.read().unwrap();
        let Some(Node {
            kind: Kind::Dir(children),
            ..
        }) = nodes.get(inode as usize).and_then(|n| n.as_ref())
        else {
            return err(ENOTDIR);
        };
        for (i, (name, &ino)) in children.iter().enumerate().skip(offset as usize) {
            let type_ = match nodes[ino as usize].as_ref().unwrap().kind {
                Kind::Dir(_) => DT_DIR,
                Kind::File(_) => DT_REG,
            };
            let written = add_entry(DirEntry {
                ino,
                offset: i as u64 + 1,
                type_,
                name,
            })?;
            if written == 0 {
                break;
            }
        }
        Ok(())
    }

    fn readdirplus(
        &self,
        _ctx: Context,
        inode: u64,
        _handle: u64,
        _size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(DirEntry, Entry) -> io::Result<usize>,
    ) -> io::Result<()> {
        self.count("readdirplus");
        let nodes = self.nodes.read().unwrap();
        let Some(Node {
            kind: Kind::Dir(children),
            ..
        }) = nodes.get(inode as usize).and_then(|n| n.as_ref())
        else {
            return err(ENOTDIR);
        };
        for (i, (name, &ino)) in children.iter().enumerate().skip(offset as usize) {
            let node = nodes[ino as usize].as_ref().unwrap();
            let type_ = match node.kind {
                Kind::Dir(_) => DT_DIR,
                Kind::File(_) => DT_REG,
            };
            let written = add_entry(
                DirEntry {
                    ino,
                    offset: i as u64 + 1,
                    type_,
                    name,
                },
                self.entry(ino, node),
            )?;
            if written == 0 {
                break;
            }
        }
        Ok(())
    }

    fn releasedir(&self, _ctx: Context, _inode: u64, _flags: u32, _handle: u64) -> io::Result<()> {
        Ok(())
    }

    fn access(&self, _ctx: Context, _inode: u64, _mask: u32) -> io::Result<()> {
        Ok(())
    }

    fn dax_host_addr(
        &self,
        _ctx: Context,
        inode: u64,
        _handle: u64,
        foffset: u64,
        len: u64,
        writable: bool,
    ) -> io::Result<u64> {
        self.count("dax_map");
        if writable {
            // ADR-0044 V3: a write through a mapping carries no WRITE/FLUSH.
            return err(EPERM);
        }
        let mut dax = self.dax.lock().unwrap();
        if let Some(region) = dax.get(&inode) {
            return Ok((region.addr + foffset as usize) as u64);
        }
        let nodes = self.nodes.read().unwrap();
        let Some(Node {
            kind: Kind::File(content),
            ..
        }) = nodes.get(inode as usize).and_then(|n| n.as_ref())
        else {
            return err(EINVAL);
        };
        let size = (content.len().max(foffset as usize + len as usize)).div_ceil(DAX_CHUNK) * DAX_CHUNK;
        // SAFETY: a fresh anonymous private mapping; never unmapped while the
        // VMM runs (the guest may still map it).
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANON | libc::MAP_PRIVATE,
                -1,
                0,
            )
        };
        if addr == libc::MAP_FAILED {
            return err(EINVAL);
        }
        // SAFETY: `addr` holds `size >= content.len()` writable bytes.
        unsafe { std::ptr::copy_nonoverlapping(content.as_ptr(), addr as *mut u8, content.len()) };
        dax.insert(inode, DaxRegion { addr: addr as usize });
        Ok(addr as u64 + foffset)
    }
}

fn c(value: &str) -> CString {
    CString::new(value).expect("no NUL")
}

fn main() {
    let corpus = PathBuf::from(std::env::var("VF3_CORPUS").expect("VF3_CORPUS"));
    let rootfs = PathBuf::from(std::env::var("VF3_ROOTFS").expect("VF3_ROOTFS"));
    let variant = Variant::parse(&std::env::var("VF3_VARIANT").unwrap_or_default());
    let guest_env = std::env::var("VF3_GUEST").unwrap_or_default();
    let vcpus: u8 = std::env::var("VF3_VCPUS").ok().map(|v| v.parse().unwrap()).unwrap_or(4);
    let ram_mib: u32 = std::env::var("VF3_RAM_MIB").ok().map(|v| v.parse().unwrap()).unwrap_or(2048);

    let started = std::time::Instant::now();
    let (backend, files, bytes) = MemFs::load(&corpus, variant);
    eprintln!(
        "vf3: loaded {files} files, {bytes} bytes in {:?}; variant {variant:?}",
        started.elapsed()
    );

    let mut environment = vec![
        c("PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"),
        c("HOME=/root"),
        c("VF3_ROOT=/work"),
        c(&format!("VF3_MOUNT_OPTS={}", if variant.dax { "dax=inode" } else { "" })),
    ];
    environment.extend(guest_env.split_whitespace().map(c));

    let config = KrunConfig {
        run_id: "vf3".into(),
        rootfs: ResolvedRootfs {
            digest: DigestRef {
                algorithm: "blake3-256".into(),
                value: "0".repeat(64),
            },
            canonical_path: rootfs.canonicalize().expect("canonical rootfs"),
        },
        executable: c("/vf3guest"),
        arguments: Vec::new(),
        environment,
        workdir: c("/"),
        vcpus,
        ram_mib,
        wall_time_ms: 600_000,
        tsi_features: 0,
        port_map: Vec::new(),
        vsock_unix_map: Vec::new(),
    };
    let dax_window = if variant.dax { 8u64 << 30 } else { 0 };
    let vm = prepare_vm(&LinkedKrunApi, &config).expect("prepare the VM");
    vm.add_virtiofs_backend("vf3", dax_window, Arc::new(backend))
        .expect("attach the in-memory share");
    let error = vm.enter().expect_err("krun_start_enter returned");
    panic!("the VMM did not start: {error}");
}
