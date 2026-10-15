//! A read-only virtio-fs backend over files held in this process's memory:
//! the smallest `DynFileSystem` that lets a guest mount a share, look a file
//! up, and read it. Test fixture for the linked VMM's custom-share seam
//! (ADR-0044 phase A, bead `ley-line-open-f1c46f`); the store-backed backend
//! is a separate crate.
//!
//! Lives in `tests/memfs/` so only the test binaries that declare `mod memfs;`
//! compile it.

use std::ffi::CStr;
use std::io;
use std::time::Duration;

use leyline_runtime::backends::libkrun::api::DynFileSystem;
use leyline_runtime::backends::libkrun::api::virtio_fs::filesystem::{Context, Entry, OpenOptions};

#[cfg(target_os = "macos")]
type Stat = libc::stat;
#[cfg(target_os = "linux")]
type Stat = libc::stat64;

// The guest is Linux: replies carry Linux errno values whatever the host is.
const LINUX_ENOENT: i32 = 2;
const LINUX_EACCES: i32 = 13;
const LINUX_ENOTDIR: i32 = 20;

/// FUSE's root inode.
const ROOT: u64 = 1;

/// Files in one flat directory; file `i` is inode `i + 2`.
pub struct MemFs {
    files: Vec<(String, Vec<u8>)>,
}

impl MemFs {
    pub fn new(files: impl IntoIterator<Item = (String, Vec<u8>)>) -> Self {
        Self {
            files: files.into_iter().collect(),
        }
    }

    fn file(&self, inode: u64) -> io::Result<&[u8]> {
        inode
            .checked_sub(2)
            .and_then(|index| self.files.get(index as usize))
            .map(|(_, bytes)| bytes.as_slice())
            .ok_or_else(|| io::Error::from_raw_os_error(LINUX_ENOENT))
    }

    fn stat(inode: u64, file: Option<&[u8]>) -> Stat {
        // SAFETY: stat is plain old data; all-zero is a valid value.
        let mut st: Stat = unsafe { std::mem::zeroed() };
        st.st_ino = inode as _;
        st.st_blksize = 4096;
        match file {
            None => {
                st.st_mode = (libc::S_IFDIR | 0o555) as _;
                st.st_nlink = 2;
            }
            Some(bytes) => {
                st.st_mode = (libc::S_IFREG | 0o444) as _;
                st.st_nlink = 1;
                st.st_size = bytes.len() as _;
                st.st_blocks = bytes.len().div_ceil(512) as _;
            }
        }
        st
    }
}

impl DynFileSystem for MemFs {
    fn lookup(&self, _ctx: Context, parent: u64, name: &CStr) -> io::Result<Entry> {
        if parent != ROOT {
            return Err(io::Error::from_raw_os_error(LINUX_ENOTDIR));
        }
        let index = self
            .files
            .iter()
            .position(|(file, _)| file.as_bytes() == name.to_bytes())
            .ok_or_else(|| io::Error::from_raw_os_error(LINUX_ENOENT))?;
        let inode = index as u64 + 2;
        Ok(Entry {
            inode,
            generation: 0,
            attr: Self::stat(inode, Some(&self.files[index].1)),
            attr_flags: 0,
            attr_timeout: Duration::ZERO,
            entry_timeout: Duration::ZERO,
        })
    }

    fn getattr(
        &self,
        _ctx: Context,
        inode: u64,
        _handle: Option<u64>,
    ) -> io::Result<(Stat, Duration)> {
        if inode == ROOT {
            return Ok((Self::stat(ROOT, None), Duration::ZERO));
        }
        Ok((Self::stat(inode, Some(self.file(inode)?)), Duration::ZERO))
    }

    fn open(
        &self,
        _ctx: Context,
        inode: u64,
        _kill_priv: bool,
        flags: u32,
    ) -> io::Result<(Option<u64>, OpenOptions)> {
        self.file(inode)?;
        if flags as i32 & libc::O_ACCMODE != libc::O_RDONLY {
            return Err(io::Error::from_raw_os_error(LINUX_EACCES));
        }
        Ok((None, OpenOptions::DIRECT_IO))
    }

    #[allow(clippy::too_many_arguments)]
    fn read(
        &self,
        _ctx: Context,
        inode: u64,
        _handle: u64,
        w: &mut dyn io::Write,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> io::Result<usize> {
        let bytes = self.file(inode)?;
        let start = (offset as usize).min(bytes.len());
        let end = start.saturating_add(size as usize).min(bytes.len());
        w.write_all(&bytes[start..end])?;
        Ok(end - start)
    }

    fn flush(&self, _ctx: Context, _inode: u64, _handle: u64, _lock_owner: u64) -> io::Result<()> {
        Ok(())
    }

    fn access(&self, _ctx: Context, _inode: u64, _mask: u32) -> io::Result<()> {
        Ok(())
    }
}
