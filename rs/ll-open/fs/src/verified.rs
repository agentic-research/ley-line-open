//! Verify-on-fault serving of the arena payload (bead `ley-line-open-b6a4dd`,
//! the mount half; the tree primitive is [`leyline_core::outboard`]).
//!
//! # Why
//!
//! `verify_arena_root` (lib.rs) attests the arena ONCE, at load: hash the
//! whole payload, compare against `ctrl.current_root`, copy the bytes into
//! SQLite, and never look at the file again. That shape has two costs: the
//! attestation is all-or-nothing O(n) up front, and it says nothing about
//! the mapping AFTER the copy — a writer scribbling on the arena file
//! post-load is invisible to a reader that serves the copy.
//!
//! [`VerifiedArena`] is the fs-verity move instead: keep the mapping, hold
//! the outboard BLAKE3 tree beside it, and gate every page on its way out.
//! A 1 KiB page (BLAKE3's own chunk grid — see the outboard module for why
//! that is NOT the CDC layer's chunk) that this session has not yet served
//! is verified against the trusted root via
//! [`leyline_core::outboard::verify_chunk`]: the leaf CV is recomputed from
//! the bytes as mapped RIGHT NOW and folded through the inclusion proof,
//! BEFORE any byte reaches the caller. A bitmap records success so the
//! second fault of the same page performs zero hashing. A failed
//! verification is a refused read — counted, logged, and never
//! silently-served bytes.
//!
//! # Trust window (stated, not implied)
//!
//! The bitmap is SESSION trust: once a page verifies, later reads serve it
//! from the mapping without re-hashing, so tampering with an
//! already-verified page is not re-detected until a new session. fs-verity
//! gets the stronger property from page-cache immutability; a userspace
//! mapping has no such pin, and re-verifying every read would delete the
//! steady-state-zero property this type exists to provide. The gate's claim
//! is exactly: no byte is ever served from a page that was never verified
//! this session, and the bytes a faulting read returns are the very bytes
//! that verified (the snapshot is served, not re-read from the mapping — no
//! verify/serve race window).
//!
//! # Sibling-CV soundness
//!
//! Proof siblings come from the load-time outboard, whose root was checked
//! bit-for-bit against `ctrl.current_root` in [`VerifiedArena::open`]. A
//! post-load tamper of a SIBLING page does not weaken the faulted page's
//! verification: the fold uses the load-time (root-committed) sibling CVs,
//! so it lands on the trusted root iff the faulted page's recomputed CV
//! equals the root-committed one. The tampered sibling itself is caught on
//! its own first fault.

use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail, ensure};
use blake3::CHUNK_LEN;
use leyline_core::mmap::mmap_read;
use leyline_core::outboard::Outboard;
use leyline_core::substrate::Hash;
use leyline_core::{ArenaHeader, Controller};
use parking_lot::Mutex;

/// The arena payload behind a per-page verification gate.
///
/// Construction is O(payload) hashing (the outboard build — the same work
/// the flat-hash load path already pays) plus a bit-for-bit root check
/// against the control block. After that, the marginal cost of integrity is
/// one 1 KiB hash + O(log n) 64-byte merges per page, paid once per page
/// per session.
pub struct VerifiedArena {
    /// Keeps the arena file mapped for the whole session. The
    /// file-not-truncated invariant (see `leyline_core::mmap`) holds
    /// because arenas are created at fixed size and only ever rewritten
    /// in place through `write_to_arena`.
    mmap: memmap2::Mmap,
    /// Byte offset of the active buffer's payload within the mapping.
    data_offset: usize,
    /// Payload length (`ArenaHeader.data_size`) — what the root commits to.
    data_len: usize,
    /// Trusted root from `ctrl.current_root`, re-checked against nothing
    /// after load: it IS the session's axiom.
    root: Hash,
    /// Load-time tree. Behind a mutex only because `prove` counts merges
    /// through a `Cell` (making `Outboard` `!Sync`); the lock is held for
    /// the O(log n) proof collection, never across hashing or I/O.
    outboard: Mutex<Outboard>,
    /// One bit per page: verified this session. Set-once, lock-free; a
    /// concurrent double-verify of the same page is benign (both folds
    /// check the same bytes against the same root).
    verified: Vec<AtomicU64>,
    /// `verify_chunk` invocations — the observable that lets tests assert
    /// the bitmap actually skips (the outboard module's counted-work
    /// discipline, one layer up).
    verify_calls: AtomicU64,
    /// Refused pages. Observability contract: every increment is paired
    /// with a `log::error!` naming the page and the root.
    verify_failures: AtomicU64,
}

// Manual impl: the interesting state is the gate's, not a hex dump of the
// mapping (memmap2's Debug is fine, but "which root, how many pages, how
// much verified" is what a log line needs).
impl std::fmt::Debug for VerifiedArena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifiedArena")
            .field("data_len", &self.data_len)
            .field("root", &self.root)
            .field("pages", &self.pages())
            .field("verify_calls", &self.verify_calls())
            .field("verify_failures", &self.verify_failures())
            .finish_non_exhaustive()
    }
}

impl VerifiedArena {
    /// Map the arena named by `control_path` and build the session gate.
    ///
    /// Refuses (fail-closed, mirroring `verify_arena_root`'s posture):
    /// - a header that does not validate;
    /// - a non-empty payload under the zero-sentinel root (the downgrade
    ///   hole an attacker could ride);
    /// - a payload whose outboard root is not bit-identical to
    ///   `ctrl.current_root` — tamper between publish and load.
    pub fn open(control_path: &Path) -> Result<Self> {
        let controller = Controller::open_or_create(control_path)?;
        let arena_path = controller.arena_path();
        let file = std::fs::File::open(&arena_path)
            .with_context(|| format!("open arena file {arena_path}"))?;
        let mmap = mmap_read(&file)?;

        let header: &ArenaHeader =
            bytemuck::from_bytes(&mmap[..std::mem::size_of::<ArenaHeader>()]);
        let file_size = mmap.len() as u64;
        let offset = header
            .validate_header(file_size)
            .context("arena header validation failed")? as usize;
        let buf_size = ArenaHeader::buffer_size(file_size) as usize;
        let data_len = header.data_size as usize;
        if data_len > buf_size {
            bail!(
                "ArenaHeader.data_size ({data_len}) > active buffer size ({buf_size}). \
                 Header corruption — refusing to build the verify gate."
            );
        }

        let root_bytes = controller.current_root();
        if data_len > 0 && root_bytes == [0u8; 32] {
            bail!(
                "arena has data (data_size = {data_len}) but current_root is the zero \
                 sentinel. Substrate identity is missing — refusing to serve \
                 unverifiable bytes. Producer must publish via set_arena_with_root."
            );
        }
        let root = Hash::from_bytes(root_bytes);

        let data = &mmap[offset..offset + data_len];
        let outboard = Outboard::build(data);
        if root_bytes != [0u8; 32] && outboard.root() != root {
            bail!(
                "arena root mismatch at load — substrate corruption detected. \
                 current_root expected BLAKE3 {} (first 8 hex), payload's outboard \
                 root is {} (first 8 hex). Refusing to serve any page.",
                crate::hex_short_8(&root_bytes),
                crate::hex_short_8(outboard.root().as_bytes()),
            );
        }

        let n_pages = data_len.div_ceil(CHUNK_LEN);
        Ok(Self {
            mmap,
            data_offset: offset,
            data_len,
            root,
            outboard: Mutex::new(outboard),
            verified: (0..n_pages.div_ceil(64))
                .map(|_| AtomicU64::new(0))
                .collect(),
            verify_calls: AtomicU64::new(0),
            verify_failures: AtomicU64::new(0),
        })
    }

    /// Payload length in bytes — what the root commits to.
    pub fn len(&self) -> usize {
        self.data_len
    }

    /// True for a fresh (never-published) arena.
    pub fn is_empty(&self) -> bool {
        self.data_len == 0
    }

    /// The trusted root this session verifies against.
    pub fn root(&self) -> Hash {
        self.root
    }

    /// Number of 1 KiB pages the gate covers. Zero for an empty payload —
    /// there is nothing to serve, so nothing to verify.
    pub fn pages(&self) -> usize {
        self.data_len.div_ceil(CHUNK_LEN)
    }

    /// `verify_chunk` invocations so far — asserts the bitmap's skip.
    pub fn verify_calls(&self) -> u64 {
        self.verify_calls.load(Ordering::Relaxed)
    }

    /// Refused pages so far — the failure counter paired with the error log.
    pub fn verify_failures(&self) -> u64 {
        self.verify_failures.load(Ordering::Relaxed)
    }

    /// The whole payload, live mapping view.
    fn data(&self) -> &[u8] {
        &self.mmap[self.data_offset..self.data_offset + self.data_len]
    }

    /// Byte span of page `index`, clamped to the payload (the last page is
    /// short unless the payload ends on the 1 KiB grid).
    fn page_span(&self, index: usize) -> Range<usize> {
        let start = index * CHUNK_LEN;
        start..(start + CHUNK_LEN).min(self.data_len)
    }

    fn is_verified(&self, index: usize) -> bool {
        self.verified[index / 64].load(Ordering::Acquire) & (1u64 << (index % 64)) != 0
    }

    fn mark_verified(&self, index: usize) {
        self.verified[index / 64].fetch_or(1u64 << (index % 64), Ordering::Release);
    }

    /// The fault gate. Bitmap hit → `Ok(None)`, zero hashing. Miss →
    /// snapshot the page from the live mapping, verify the SNAPSHOT against
    /// the trusted root, mark the bit, and hand the snapshot back so the
    /// caller serves exactly the bytes that verified. Failure → counted,
    /// logged, refused.
    fn fault_page(&self, index: usize) -> Result<Option<Vec<u8>>> {
        ensure!(
            index < self.pages(),
            "page index {index} out of range for {} pages",
            self.pages()
        );
        if self.is_verified(index) {
            return Ok(None);
        }
        let snapshot = self.data()[self.page_span(index)].to_vec();
        let proof = self.outboard.lock().prove(index)?;
        self.verify_calls.fetch_add(1, Ordering::Relaxed);
        match leyline_core::outboard::verify_chunk(
            self.root,
            self.data_len,
            index,
            &snapshot,
            &proof,
        ) {
            Ok(()) => {
                self.mark_verified(index);
                Ok(Some(snapshot))
            }
            Err(e) => {
                self.verify_failures.fetch_add(1, Ordering::Relaxed);
                log::error!(
                    "verify-on-fault REFUSED page {index}: bytes in the mapping do not \
                     verify against root {} — refusing to serve",
                    crate::hex_short_8(self.root.as_bytes()),
                );
                Err(e).with_context(|| {
                    format!("verify-on-fault refused page {index} (arena tampered after publish?)")
                })
            }
        }
    }

    /// Verify page `index` against the trusted root if this session has not
    /// already, without serving bytes — the gate for callers that read the
    /// mapping directly (a caller doing so accepts the trust-window caveat
    /// in the module docs; [`VerifiedArena::read_at`] does not, because it
    /// serves the verified snapshot itself on the faulting read).
    pub fn verify_page(&self, index: usize) -> Result<()> {
        self.fault_page(index).map(|_| ())
    }

    /// Serve `buf.len()` bytes starting at `offset`, verifying every
    /// overlapping page this session has not yet verified BEFORE writing a
    /// single byte to `buf`. Returns the byte count (short at end of
    /// payload, `0` past it — `Graph::read_content` range semantics). On
    /// any refused page the destination is untouched: the crate's
    /// fail-closed discipline ("failed read partially modified
    /// destination" is the chunked module's pinned regression).
    pub fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        if offset >= self.data_len || buf.is_empty() {
            return Ok(0);
        }
        let end = offset.saturating_add(buf.len()).min(self.data_len);
        let first = offset / CHUNK_LEN;
        let last = (end - 1) / CHUNK_LEN;

        // Pass 1: fault + verify everything the read touches, keeping the
        // snapshots of pages verified by THIS read.
        let mut snapshots: Vec<(usize, Vec<u8>)> = Vec::new();
        for index in first..=last {
            if let Some(snapshot) = self.fault_page(index)? {
                snapshots.push((index, snapshot));
            }
        }

        // Pass 2: copy. A page this read verified is served from its
        // snapshot (the attested bytes); a page verified on an earlier
        // fault is served from the mapping (the session-trust window).
        for index in first..=last {
            let span = self.page_span(index);
            let lo = span.start.max(offset);
            let hi = span.end.min(end);
            let snapshot = snapshots
                .iter()
                .find(|(i, _)| *i == index)
                .map(|(_, s)| s.as_slice());
            let page = snapshot.unwrap_or(&self.data()[span.clone()]);
            buf[lo - offset..hi - offset].copy_from_slice(&page[lo - span.start..hi - span.start]);
        }
        Ok(end - offset)
    }

    /// The whole payload through the gate — the arena-load call-site's
    /// shape (`SqliteGraphAdapter::from_arena_verified`): every page is
    /// proof-verified on its way into the returned buffer.
    pub fn read_all(&self) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; self.data_len];
        let n = self.read_at(0, &mut buf)?;
        debug_assert_eq!(n, self.data_len, "full read must cover the payload");
        Ok(buf)
    }
}

/// The byte range an arena flip actually changed — the writer half of the
/// outboard seam. `Outboard::update` wants a dirty range; the flip path has
/// only old-bytes/new-bytes, so derive the tightest truthful range: common
/// prefix trimmed always, common suffix trimmed only when the lengths match
/// (a length change shifts every downstream byte against the fixed 1 KiB
/// grid, and `update` re-derives from `dirty.start` onward in that case —
/// a trimmed suffix would be a lie the tree cannot absorb).
pub(crate) fn dirty_span(prev: &[u8], next: &[u8]) -> Range<usize> {
    let prefix = prev
        .iter()
        .zip(next.iter())
        .take_while(|(a, b)| a == b)
        .count();
    if prev.len() != next.len() {
        return prefix..next.len();
    }
    let suffix = prev[prefix..]
        .iter()
        .rev()
        .zip(next[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    prefix..next.len() - suffix
}

#[cfg(test)]
mod tests {
    use super::*;
    use leyline_core::layout::{create_arena, write_to_arena};

    /// Deterministic bytes; no RNG dependency (the outboard module's helper).
    fn prng_bytes(seed: u64, n: usize) -> Vec<u8> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 24) as u8
            })
            .collect()
    }

    /// Publish `payload` into a fresh arena + control pair, exactly the way
    /// the producer does (write to the inactive buffer, flip, advance the
    /// root), returning the control path.
    fn publish_arena(dir: &Path, payload: &[u8]) -> std::path::PathBuf {
        let ctrl_path = dir.join("verify.ctrl");
        let arena_path = dir.join("verify.arena");
        let arena_size = 4096 + 2 * (payload.len().max(4096) as u64).next_multiple_of(4096);
        let mut mmap = create_arena(&arena_path, arena_size).unwrap();
        write_to_arena(&mut mmap, payload).unwrap();
        drop(mmap);
        let root: [u8; 32] = blake3::hash(payload).into();
        Controller::open_or_create(&ctrl_path)
            .unwrap()
            .set_arena_with_root(arena_path.to_str().unwrap(), arena_size, root)
            .unwrap();
        ctrl_path
    }

    /// Byte offset of the ACTIVE payload inside the arena file, read the
    /// same way the loader reads it — so tamper tests flip live bytes, not
    /// the stale half of the double buffer.
    fn active_payload_offset(ctrl_path: &Path) -> (std::path::PathBuf, u64) {
        let controller = Controller::open_or_create(ctrl_path).unwrap();
        let arena_path = std::path::PathBuf::from(controller.arena_path());
        let bytes = std::fs::read(&arena_path).unwrap();
        let header: &ArenaHeader =
            bytemuck::from_bytes(&bytes[..std::mem::size_of::<ArenaHeader>()]);
        let offset = header.validate_header(bytes.len() as u64).unwrap();
        (arena_path, offset)
    }

    /// Flip one byte of the published payload via a direct file write —
    /// the "behind the mount's back" tamper. The mapping is MAP_SHARED, so
    /// the write is visible to the live `VerifiedArena` without a re-open.
    fn flip_payload_byte(ctrl_path: &Path, payload_byte: u64) {
        use std::io::{Read, Seek, SeekFrom, Write};
        let (arena_path, offset) = active_payload_offset(ctrl_path);
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(arena_path)
            .unwrap();
        file.seek(SeekFrom::Start(offset + payload_byte)).unwrap();
        let mut b = [0u8; 1];
        file.read_exact(&mut b).unwrap();
        file.seek(SeekFrom::Start(offset + payload_byte)).unwrap();
        file.write_all(&[b[0] ^ 0x01]).unwrap();
        file.flush().unwrap();
    }

    /// An untampered arena serves byte-identical content through the gate,
    /// and the work is exactly one verification per page — then zero: a
    /// second full read re-verifies nothing (the bitmap's whole claim,
    /// asserted on the counter, not narrated).
    #[test]
    fn untampered_arena_serves_byte_identical_content_with_counted_work() {
        let dir = tempfile::tempdir().unwrap();
        let payload = prng_bytes(0xB6A4, 5 * CHUNK_LEN + 300);
        let ctrl = publish_arena(dir.path(), &payload);

        let arena = VerifiedArena::open(&ctrl).unwrap();
        assert_eq!(arena.len(), payload.len());
        assert!(!arena.is_empty(), "a published payload is not empty");
        assert_eq!(arena.pages(), 6);

        let mut buf = vec![0u8; payload.len()];
        assert_eq!(arena.read_at(0, &mut buf).unwrap(), payload.len());
        assert_eq!(buf, payload, "gate must serve byte-identical content");
        assert_eq!(arena.verify_calls(), 6, "one verification per page");

        // Steady state: the same read again, and an unaligned slice, cost
        // zero further verifications.
        let mut again = vec![0u8; payload.len()];
        arena.read_at(0, &mut again).unwrap();
        let mut slice = vec![0u8; 700];
        let n = arena.read_at(CHUNK_LEN + 100, &mut slice).unwrap();
        assert_eq!(n, 700);
        assert_eq!(&slice[..n], &payload[CHUNK_LEN + 100..CHUNK_LEN + 800]);
        assert_eq!(arena.verify_calls(), 6, "verified pages must not re-verify");
        assert_eq!(arena.verify_failures(), 0);

        // The Debug view is the gate's log-line surface — it must carry the
        // counters, not a hex dump of the mapping.
        let debug = format!("{arena:?}");
        assert!(debug.contains("verify_calls"), "{debug}");
        assert!(debug.contains("pages"), "{debug}");
    }

    /// A payload that fills the buffer half EXACTLY is still in bounds —
    /// the `data_size > buf_size` guard is strictly-greater, and this shape
    /// is the only one that can tell `>` from `>=`.
    #[test]
    fn payload_filling_the_buffer_exactly_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        // publish_arena sizes each half to next_multiple_of(4096), so an
        // 8 KiB payload occupies its half completely.
        let payload = prng_bytes(0xF111, 8 * CHUNK_LEN);
        let ctrl = publish_arena(dir.path(), &payload);

        let arena = VerifiedArena::open(&ctrl).unwrap();
        assert_eq!(arena.len(), payload.len());
        let mut buf = vec![0u8; payload.len()];
        arena.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, payload);
    }

    /// A header claiming more data than the buffer holds is corruption and
    /// must be refused BEFORE any slice math trusts it.
    #[test]
    fn data_size_exceeding_the_buffer_is_refused() {
        use std::io::{Seek, SeekFrom, Write};
        let dir = tempfile::tempdir().unwrap();
        let payload = prng_bytes(0xC0, 2 * CHUNK_LEN);
        let ctrl = publish_arena(dir.path(), &payload);

        // data_size lives at header bytes [16..24] (magic 4 + version 1 +
        // active_buffer 1 + padding 2 + sequence 8). Claim one byte more
        // than the buffer half can hold.
        let (arena_path, _) = active_payload_offset(&ctrl);
        let file_size = std::fs::metadata(&arena_path).unwrap().len();
        let buf_size = ArenaHeader::buffer_size(file_size);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&arena_path)
            .unwrap();
        file.seek(SeekFrom::Start(16)).unwrap();
        file.write_all(&(buf_size + 1).to_ne_bytes()).unwrap();
        file.flush().unwrap();

        let err = VerifiedArena::open(&ctrl)
            .err()
            .expect("oversized data_size must be refused");
        assert!(err.to_string().contains("Header corruption"), "{err:#}");
    }

    /// The bitmap skip at page granularity: faulting page 2 alone, then a
    /// read spanning pages 1..=3, verifies only the two NEW pages.
    #[test]
    fn second_fault_of_the_same_page_skips_reverification() {
        let dir = tempfile::tempdir().unwrap();
        let payload = prng_bytes(0xDD, 6 * CHUNK_LEN);
        let ctrl = publish_arena(dir.path(), &payload);
        let arena = VerifiedArena::open(&ctrl).unwrap();

        arena.verify_page(2).unwrap();
        assert_eq!(arena.verify_calls(), 1);
        arena.verify_page(2).unwrap();
        assert_eq!(arena.verify_calls(), 1, "second fault must be a bitmap hit");

        let mut buf = vec![0u8; 3 * CHUNK_LEN];
        arena.read_at(CHUNK_LEN, &mut buf).unwrap();
        assert_eq!(&buf, &payload[CHUNK_LEN..4 * CHUNK_LEN]);
        assert_eq!(
            arena.verify_calls(),
            3,
            "spanning read verifies pages 1 and 3 only — 2 is already attested"
        );
    }

    /// THE falsifier: a page tampered behind the mount's back (direct file
    /// write after the gate is up) is REFUSED — error path, counted,
    /// destination untouched — never silently-served bytes. Pages already
    /// attested keep serving.
    #[test]
    fn tampered_page_is_refused_not_served() {
        let dir = tempfile::tempdir().unwrap();
        let payload = prng_bytes(0xFA17, 8 * CHUNK_LEN + 123);
        let ctrl = publish_arena(dir.path(), &payload);
        let arena = VerifiedArena::open(&ctrl).unwrap();

        // Attest page 0 before the tamper: the session's trusted set.
        arena.verify_page(0).unwrap();

        // Flip one byte inside page 5, which the session has NOT served.
        flip_payload_byte(&ctrl, (5 * CHUNK_LEN + 17) as u64);

        let before = vec![0xA5u8; 2 * CHUNK_LEN];
        let mut buf = before.clone();
        let err = arena.read_at(4 * CHUNK_LEN, &mut buf).unwrap_err();
        assert!(
            format!("{err:#}").contains("refused page 5"),
            "error must name the refused page: {err:#}"
        );
        assert_eq!(buf, before, "refused read must not modify the destination");
        assert_eq!(arena.verify_failures(), 1, "refusal must be counted");

        // The already-attested page still serves (session trust window,
        // documented) and an untouched page still verifies clean.
        let mut ok = vec![0u8; CHUNK_LEN];
        arena.read_at(0, &mut ok).unwrap();
        assert_eq!(&ok, &payload[..CHUNK_LEN]);
        arena.verify_page(7).unwrap();
        assert_eq!(arena.verify_failures(), 1);
    }

    /// Root mismatch at load: tamper BEFORE the gate is built and `open`
    /// itself refuses — no VerifiedArena exists to serve anything.
    #[test]
    fn root_mismatch_at_load_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let payload = prng_bytes(0x0AD, 3 * CHUNK_LEN);
        let ctrl = publish_arena(dir.path(), &payload);
        flip_payload_byte(&ctrl, CHUNK_LEN as u64);

        let err = VerifiedArena::open(&ctrl).unwrap_err();
        assert!(err.to_string().contains("root mismatch at load"), "{err:#}");
    }

    /// The zero-sentinel downgrade hole is closed on this path too: data
    /// without a published root is unverifiable and refused, while a fresh
    /// empty arena (nothing published) loads as a zero-page gate.
    #[test]
    fn zero_root_with_data_is_refused_and_empty_arena_loads() {
        let dir = tempfile::tempdir().unwrap();
        let arena_path = dir.path().join("legacy.arena");
        let ctrl_path = dir.path().join("legacy.ctrl");
        let arena_size = 4096 + 2 * 4096;
        let mut mmap = create_arena(&arena_path, arena_size).unwrap();

        // Fresh arena, nothing published: loads, serves nothing.
        Controller::open_or_create(&ctrl_path)
            .unwrap()
            .set_arena(arena_path.to_str().unwrap(), arena_size)
            .unwrap();
        let empty = VerifiedArena::open(&ctrl_path).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.pages(), 0);
        let mut buf = [0u8; 16];
        assert_eq!(empty.read_at(0, &mut buf).unwrap(), 0);
        assert!(empty.verify_page(0).is_err(), "no pages exist to verify");
        drop(empty);

        // Data published via the legacy rootless path: refused.
        write_to_arena(&mut mmap, b"data with no root").unwrap();
        drop(mmap);
        let err = VerifiedArena::open(&ctrl_path).unwrap_err();
        assert!(err.to_string().contains("zero"), "{err:#}");
    }

    /// The gate is shared across FUSE/NFS handler threads.
    #[test]
    fn verified_arena_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<VerifiedArena>();
    }

    /// `dirty_span` is the writer flip path's truthfulness contract with
    /// `Outboard::update`: tightest span on equal lengths, no suffix trim
    /// on a resize, empty span (at the end) for identical bytes.
    #[test]
    fn dirty_span_is_tight_and_truthful() {
        // Equal length: prefix AND suffix trimmed.
        let a = b"aaaa-bbbb-cccc".to_vec();
        let b = b"aaaa-BXBB-cccc".to_vec();
        assert_eq!(dirty_span(&a, &b), 5..9);

        // Identical: empty range, positioned so update() rehashes nothing.
        assert_eq!(dirty_span(&a, &a), a.len()..a.len());

        // Resize: suffix must NOT be trimmed even though the tails match —
        // the grid shifted underneath it.
        let grew = b"aaaa-bbbb-cccc-dddd".to_vec();
        assert_eq!(dirty_span(&a, &grew), 14..19);
        let shrank = b"aaaa-bb".to_vec();
        assert_eq!(dirty_span(&a, &shrank), 7..7);
        // A shrink whose divergence starts mid-way: everything from the
        // first differing byte.
        let shifted = b"aaaa-Xbb".to_vec();
        assert_eq!(dirty_span(&a, &shifted), 5..8);

        // The contract end-to-end: update() with this span lands on the
        // reference hash for every shape above.
        for next in [&b, &grew, &shrank, &shifted] {
            let mut ob = Outboard::build(&a);
            ob.update(next, dirty_span(&a, next)).unwrap();
            assert_eq!(
                *ob.root().as_bytes(),
                *blake3::hash(next).as_bytes(),
                "dirty_span fed update() off the reference hash"
            );
        }
    }
}
