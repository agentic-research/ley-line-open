//! Typed helpers for `memmap2::Mmap` / `MmapMut` construction.
//!
//! Bead `ley-line-open-85fb1f` PR 3. `memmap2::Mmap::map(&file)` is
//! `unsafe fn` because the OS-level invariant is genuinely broad: the
//! backing file must not be truncated (or otherwise resized) by any
//! process while the mmap is live, or reads/writes into the mapped
//! region become undefined behavior. That invariant is the SAME every
//! time this pattern is used in LLO — six call sites in ll-core +
//! ll-open crates repeat the same one-line `unsafe { Mmap::map(&file)? }`.
//!
//! This module consolidates them behind two safe wrappers so:
//!
//! 1. The unsafe LIVES in ONE place with a docstring explaining the
//!    file-truncation contract.
//! 2. Call sites become plain `mmap_read(&file)?` — no unsafe block
//!    to reason about at each site.
//!
//! Callers must still uphold the file-not-truncated invariant, but
//! that lives in the helper's docstring rather than being re-documented
//! (or forgotten) at every use.

use memmap2::{Mmap, MmapMut};
use std::fs::File;
use std::io;

/// Memory-map `file` read-only.
///
/// # Safety contract (delegated from `memmap2::Mmap::map`)
///
/// Callers MUST guarantee that the file backing `file` is not
/// truncated or resized by any process while the returned `Mmap` is
/// live. Violating this invariant is undefined behavior: reads from
/// the mapped region may SIGBUS or return arbitrary bytes.
///
/// Enforce this by:
///   - Opening the file with an exclusive lock (`flock` / arena-lock),
///     OR
///   - Guaranteeing single-writer via the arena's write-path
///     (leyline-core's `Controller` snapshot semantics), OR
///   - Only calling this on immutable trees (never modified after
///     creation).
///
/// The returned `Mmap` is Send + Sync per `memmap2`'s contract; callers
/// can share it across threads without further synchronization.
pub fn mmap_read(file: &File) -> io::Result<Mmap> {
    // SAFETY: caller upholds the file-not-truncated invariant per this
    // fn's docstring; `Mmap::map` documents no other preconditions.
    unsafe { Mmap::map(file) }
}

/// Memory-map `file` for writable access.
///
/// # Safety contract (delegated from `memmap2::MmapMut::map_mut`)
///
/// Same file-not-truncated invariant as [`mmap_read`], plus: the
/// caller MUST have exclusive write access to the file (no other
/// process opens it for write, and no other `MmapMut` aliases the
/// same region within this process). Violating exclusive-write is
/// data-race UB.
///
/// LLO's arena layout uses this from the writer path only, which is
/// serialized on a single tokio worker per the daemon's
/// `LiveDb::with_write` shape.
pub fn mmap_write(file: &File) -> io::Result<MmapMut> {
    // SAFETY: caller upholds the file-not-truncated + exclusive-write
    // invariants per this fn's docstring.
    unsafe { MmapMut::map_mut(file) }
}

#[cfg(test)]
mod tests {
    //! Load-bearing tests for [`mmap_read`] and [`mmap_write`]. The
    //! goal is to prove the helpers preserve `memmap2`'s semantics
    //! byte-for-byte — callers shouldn't need to reason about the
    //! wrapper, only about the OS-level invariant.
    use super::*;
    use std::io::{Seek, SeekFrom, Write};
    use tempfile::NamedTempFile;

    #[test]
    fn mmap_read_returns_file_bytes() {
        let mut tf = NamedTempFile::new().unwrap();
        tf.write_all(b"hello mmap").unwrap();
        tf.flush().unwrap();
        let file = File::open(tf.path()).unwrap();
        let m = mmap_read(&file).expect("mmap_read");
        assert_eq!(&m[..], b"hello mmap");
    }

    #[test]
    fn mmap_write_reflects_writes_back_to_file() {
        let mut tf = NamedTempFile::new().unwrap();
        tf.write_all(&[0u8; 16]).unwrap();
        tf.flush().unwrap();

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(tf.path())
            .unwrap();
        let mut m = mmap_write(&file).expect("mmap_write");
        m[..5].copy_from_slice(b"abcde");
        m.flush().unwrap();
        drop(m);

        let mut file2 = File::open(tf.path()).unwrap();
        file2.seek(SeekFrom::Start(0)).unwrap();
        let mut buf = [0u8; 16];
        std::io::Read::read_exact(&mut file2, &mut buf).unwrap();
        assert_eq!(&buf[..5], b"abcde");
    }

    #[test]
    fn mmap_read_of_empty_file_yields_empty_slice() {
        let tf = NamedTempFile::new().unwrap();
        let file = File::open(tf.path()).unwrap();
        // memmap2 accepts empty files and returns a zero-length slice
        // — pin that behavior so a future refactor can't quietly
        // change it (some callers depend on `mmap.len() == 0`
        // detecting an empty arena).
        let m = mmap_read(&file).expect("mmap_read empty");
        assert_eq!(m.len(), 0);
    }
}
