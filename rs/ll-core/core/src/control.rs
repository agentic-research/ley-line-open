//! Control block for content-addressed Σ substrate identity (V3).
//!
//! A 4096-byte memory-mapped file naming the currently-active arena.
//! Substrate identity is `current_root` — BLAKE3 over the live arena
//! payload. Polling readers compare `current_root()` for change
//! detection.
//!
//! Layout (matches Go `internal/control/control.go`):
//!   [0..4]     Magic: 0x4C455943 ('LEYC')
//!   [4..8]     Version: u32 (must be 3)
//!   [8..16]    Sequence: AtomicU64 seqlock counter (private; odd while
//!                                    a publish is in progress, even at
//!                                    rest; formerly the V1 `generation`)
//!   [16..272]  ArenaPath: [u8; 256] (null-terminated)
//!   [272..280] ArenaSize: u64
//!   [280..320] Interrupt fields (feature = "interrupt"; reserved otherwise)
//!   [320..352] CurrentRoot: [u8; 32]  — Σ root pointer
//!   [352..4096] Padding
//!
//! # Publication protocol (seqlock)
//!
//! The payload (path, size, root) is 296 bytes; no hardware writes it
//! atomically, so a reader that overlaps a publish could otherwise copy
//! half of the old root and half of the new one. V3 makes the overlap
//! detectable:
//!
//! - The writer increments the sequence to ODD (AcqRel) before touching
//!   the payload and to EVEN (Release) after the last payload byte.
//! - The reader loads the sequence (Acquire); if it is odd a publish is
//!   in flight and it yields and retries. It copies the payload, issues
//!   an Acquire fence, reloads the sequence, and accepts the copy only
//!   when the two loads are equal. Any publish that began or ended during
//!   the copy changes the sequence, so a torn copy is always rejected.
//!
//! V2 (T2.4) wrote the payload and then bumped the counter once; a
//! reader could not tell an in-flight publish from a finished one. V1
//! exposed `generation` as a public counter. Old binaries reading new
//! files (or vice versa) hit the explicit VERSION-mismatch error in
//! `open_or_create`.

use std::fs::OpenOptions;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::mmap::mmap_write;
use anyhow::{Context, Result, bail};
use memmap2::MmapMut;

/// One consistent copy of the control block payload, as the seqlock
/// reader hands it out (see `Controller::snapshot`).
struct Snapshot {
    path: [u8; ARENA_PATH_LEN],
    size: [u8; 8],
    root: [u8; CURRENT_ROOT_LEN],
}

/// Control block size: one page.
pub const CONTROL_SIZE: usize = 4096;

/// Magic number: 'LEYC' = 0x4C455943
pub const MAGIC: u32 = 0x4C455943;

/// **T2.4 — Σ content-addressed substrate, breaking version 2.**
///
/// V1 (pre-T2.4) exposed `generation: u64` as the public substrate
/// identity. V2 removed generation from the public API;
/// `current_root` (BLAKE3 of arena bytes) IS the substrate identity.
/// V3 turns the private counter at `OFF_GENERATION` into a seqlock
/// sequence: odd while `set_arena*` is writing the payload, even at
/// rest. Readers retry while it is odd or when it changes under them,
/// so a read never returns a half-published root (see the module
/// docs). Callers cannot access this counter; all polling is by root.
///
/// Every VERSION bump means old `.ctrl` files are rejected by new
/// binaries and vice versa. 2 → 3 is a deliberate breakpoint: the V2
/// reader cannot detect an in-flight publish, and a V3 reader must not
/// trust a V2 writer that never marks one. Coordinate with mache's
/// `internal/control` reader (bead ley-line-open-49a1ef).
pub const VERSION: u32 = 3;

// Field offsets (matching Go's #[repr(C)] layout)
const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 4;
const OFF_GENERATION: usize = 8;
const OFF_ARENA_PATH: usize = 16;
const ARENA_PATH_LEN: usize = 256;
const OFF_ARENA_SIZE: usize = 272;

// Interrupt control fields (feature = "interrupt"), bytes [280..320]
#[cfg(feature = "interrupt")]
const OFF_INTERRUPT_FLAGS: usize = 280;
#[cfg(feature = "interrupt")]
const OFF_INTERRUPT_EPOCH: usize = 288;
#[cfg(feature = "interrupt")]
const OFF_INTERRUPT_ACK: usize = 296;
#[cfg(feature = "interrupt")]
const OFF_PAYLOAD_OFFSET: usize = 304;
#[cfg(feature = "interrupt")]
const OFF_PAYLOAD_LEN: usize = 312;

/// CurrentRoot: 32-byte BLAKE3 content address of the active arena
/// payload. **Post-T2.4 this is the substrate's sole public identity.**
/// Polling readers (HotSwapGraph) compare via `current_root()`.
const OFF_CURRENT_ROOT: usize = 320;
const CURRENT_ROOT_LEN: usize = 32;

// Compile-time invariant: the sequence slot must be 8-byte aligned for
// the AtomicU64 cast in `Controller::sequence` to be sound. mmap is page-aligned, so any 8-byte-aligned offset within
// it gives an 8-byte-aligned pointer. If a future field reorder violates
// this, the cast becomes UB on architectures requiring naturally-aligned
// atomics (e.g. aarch64 LSE) — fail compilation instead.
const _: () = assert!(
    OFF_GENERATION.is_multiple_of(8),
    "seqlock sequence must be 8-byte aligned"
);
const _: () = assert!(
    OFF_ARENA_SIZE.is_multiple_of(8),
    "ArenaSize must be 8-byte aligned"
);

/// Controller manages a memory-mapped control file.
pub struct Controller {
    mmap: MmapMut,
}

impl Controller {
    /// Open or create a control file at the given path.
    pub fn open_or_create(path: &Path) -> Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("create control dir")?;
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .context("open control file")?;

        let meta = file.metadata().context("stat control file")?;
        if meta.len() < CONTROL_SIZE as u64 {
            file.set_len(CONTROL_SIZE as u64)
                .context("truncate control file")?;
        }

        let mut mmap = mmap_write(&file)?;

        // Initialize if new (magic == 0)
        let existing_magic = u32::from_ne_bytes(
            mmap[OFF_MAGIC..OFF_MAGIC + 4]
                .try_into()
                .expect("4-byte slice ⇒ [u8; 4]"),
        );

        if existing_magic == 0 {
            mmap[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_ne_bytes());
            mmap[OFF_VERSION..OFF_VERSION + 4].copy_from_slice(&VERSION.to_ne_bytes());
        } else if existing_magic != MAGIC {
            bail!("invalid control block magic: 0x{:08X}", existing_magic);
        } else {
            // VERSION mismatch is a hard error. Each version changes
            // what the counter at [8..16] means (V1 public generation,
            // V2 publish-then-bump, V3 seqlock); reading one as another
            // would silently misinterpret it — refuse explicitly.
            let existing_version = u32::from_ne_bytes(
                mmap[OFF_VERSION..OFF_VERSION + 4]
                    .try_into()
                    .expect("4-byte slice ⇒ [u8; 4]"),
            );
            if existing_version != VERSION {
                bail!(
                    "control block VERSION mismatch: file has v{}, this binary expects v{}. \
                     v3 publishes the control block under a seqlock (odd sequence while a \
                     publish is in flight); older readers cannot detect an in-flight publish \
                     and older writers never mark one, so the versions do not interoperate. \
                     Upgrade every leyline and mache process sharing this control block.",
                    existing_version,
                    VERSION
                );
            }
        }

        Ok(Controller { mmap })
    }

    /// Reference the `AtomicU64` embedded in the mapped control block
    /// at `offset`. Callers use it as a safe atomic — the single
    /// unsafe block here carries the invariant for every atomic slot
    /// in the control layout (sync counter, interrupt flags/epoch/ack,
    /// payload offset/len).
    ///
    /// # Safety contract (delegated from the raw pointer cast)
    ///
    /// The invariant that lets this be a SAFE function:
    ///
    /// - `mmap` is page-aligned by `memmap2` construction, and every
    ///   `OFF_*` constant referenced in this module is compile-time
    ///   asserted to be 8-byte aligned (see the `assert!` calls near
    ///   the top of the module). So `offset` is 8-byte aligned within
    ///   a page-aligned base — the pointer is naturally aligned for
    ///   `AtomicU64`.
    /// - `AtomicU64` and `u64` have identical layout (Rust guarantees
    ///   this per `std::sync::atomic` docs); constructing an
    ///   `&AtomicU64` from a `*const u64` reinterpret is sound.
    /// - The returned reference is tied to `&self`, so it cannot
    ///   outlive the mapping.
    ///
    /// Debug-asserts alignment so a future OFF_* addition that isn't
    /// 8-byte aligned fails loud in tests instead of silently going
    /// UB on aarch64 LSE. Bounds-check the offset the same way.
    fn atomic_at(&self, offset: usize) -> &AtomicU64 {
        debug_assert_eq!(
            offset % 8,
            0,
            "atomic_at: offset {offset} must be 8-byte aligned"
        );
        debug_assert!(
            offset + 8 <= self.mmap.len(),
            "atomic_at: offset {offset} + 8 exceeds mmap len {}",
            self.mmap.len(),
        );
        let ptr = self.mmap[offset..].as_ptr() as *const AtomicU64;
        // SAFETY: `mmap` is page-aligned; every OFF_* constant used
        // via this helper is 8-byte aligned by compile-time assertion;
        // AtomicU64 and u64 share layout per the Rust atomic-types
        // guarantee; `&self` bounds the returned reference to the
        // mapping's lifetime.
        unsafe { &*ptr }
    }

    /// The seqlock sequence (see the module docs). Not exposed in the
    /// public API; public callers compare `current_root()` for identity
    /// and change detection.
    fn sequence(&self) -> &AtomicU64 {
        self.atomic_at(OFF_GENERATION)
    }

    /// Copy `dst.len()` payload bytes starting at `offset` out of the
    /// mapping with volatile loads. The writer may be storing into the
    /// same bytes from another thread or process; the seqlock around
    /// this copy decides whether the result is kept, and the volatile
    /// loads keep the compiler from caching or splitting the copy in
    /// ways the sequence check could not see.
    fn volatile_copy_out(&self, offset: usize, dst: &mut [u8]) {
        assert!(offset + dst.len() <= self.mmap.len());
        let src = self.mmap[offset..].as_ptr();
        for (i, slot) in dst.iter_mut().enumerate() {
            // SAFETY: `offset + i < mmap.len()` by the assertion above,
            // and the mapping outlives `&self`.
            *slot = unsafe { std::ptr::read_volatile(src.add(i)) };
        }
    }

    /// Store `src` into the mapping at `offset` with volatile stores.
    /// Counterpart of `volatile_copy_out`; called only between
    /// `seq_begin` and `seq_end`.
    fn volatile_copy_in(&mut self, offset: usize, src: &[u8]) {
        assert!(offset + src.len() <= self.mmap.len());
        let dst = self.mmap[offset..].as_mut_ptr();
        for (i, &byte) in src.iter().enumerate() {
            // SAFETY: `offset + i < mmap.len()` by the assertion above,
            // and `&mut self` gives exclusive access to the mapping
            // within this process.
            unsafe { std::ptr::write_volatile(dst.add(i), byte) };
        }
    }

    /// A consistent copy of the published payload: `(path, size, root)`
    /// exactly as some single `set_arena*` call left them.
    ///
    /// Seqlock read side: load the sequence (Acquire); retry while it is
    /// odd (a publish is in flight); copy the payload; Acquire fence;
    /// reload the sequence; keep the copy only if the sequence did not
    /// move. A writer that began or finished a publish during the copy
    /// changed the sequence, so a torn copy never escapes.
    fn snapshot(&self) -> Snapshot {
        let seq = self.sequence();
        loop {
            let before = seq.load(Ordering::Acquire);
            if before & 1 == 1 {
                std::thread::yield_now();
                continue;
            }
            let mut snap = Snapshot {
                path: [0u8; ARENA_PATH_LEN],
                size: [0u8; 8],
                root: [0u8; CURRENT_ROOT_LEN],
            };
            self.volatile_copy_out(OFF_ARENA_PATH, &mut snap.path);
            self.volatile_copy_out(OFF_ARENA_SIZE, &mut snap.size);
            self.volatile_copy_out(OFF_CURRENT_ROOT, &mut snap.root);
            std::sync::atomic::fence(Ordering::Acquire);
            let after = seq.load(Ordering::Relaxed);
            if before == after {
                return snap;
            }
        }
    }

    /// Get the path to the currently active arena.
    pub fn arena_path(&self) -> String {
        let bytes = self.snapshot().path;
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(ARENA_PATH_LEN);
        String::from_utf8_lossy(&bytes[..end]).to_string()
    }

    /// Get the size of the currently active arena.
    pub fn arena_size(&self) -> u64 {
        u64::from_ne_bytes(self.snapshot().size)
    }

    /// Read the current arena root (Σ root pointer).
    ///
    /// **This is the substrate's primary identity field.**
    /// Returns `[0u8; 32]` — the zero sentinel — when no root has
    /// been published yet (fresh control file). Callers comparing
    /// roots for change detection (e.g. HotSwapGraph polling) treat
    /// `current_root() != cached_root` as a publish event.
    ///
    /// The returned root is always one that some `set_arena_with_root`
    /// call published in full (seqlock, see the module docs). **The
    /// consistency is per-call:** a following `arena_path()` or
    /// `arena_size()` is its own snapshot and may already reflect a
    /// later publish. See `HotSwapGraph::maybe_swap` for the reference
    /// polling implementation.
    pub fn current_root(&self) -> [u8; 32] {
        self.snapshot().root
    }

    /// **Test-only, unfenced root setter** (gated `#[cfg(test)]`).
    /// Production code uses [`Self::set_arena_with_root`], which
    /// writes under the Release-store of the sync counter so polling
    /// readers observe a consistent snapshot. The cfg-gate
    /// structurally prevents production callers from reaching this
    /// unfenced path; this method only exists for tests that need
    /// to seed or clobber the root region directly.
    #[cfg(test)]
    fn set_current_root(&mut self, root: [u8; 32]) -> Result<()> {
        self.mmap[OFF_CURRENT_ROOT..OFF_CURRENT_ROOT + CURRENT_ROOT_LEN].copy_from_slice(&root);
        self.mmap.flush().context("flush control block")?;
        Ok(())
    }

    /// **Test-only, unsynchronized root view**: the root bytes as they
    /// sit in the mapping right now, ignoring the seqlock. Lets a test
    /// prove that a writer held mid-publish really has left a torn root
    /// behind, which is what the seqlock reader must hide.
    #[cfg(test)]
    fn unsynchronized_root(&self) -> [u8; 32] {
        let mut out = [0u8; CURRENT_ROOT_LEN];
        self.volatile_copy_out(OFF_CURRENT_ROOT, &mut out);
        out
    }

    /// **Re-advertise without publishing new content.** Writes path
    /// and size to the control block under the seqlock but **preserves
    /// the existing `current_root` unchanged**. Used for the snapshot's
    /// step-2 re-advertisement (file grow without commit) and for test
    /// fixtures that don't need a published root.
    ///
    /// Polling readers (HotSwapGraph) compare `current_root` to
    /// detect change. Since this method preserves the root, the read
    /// side sees no change → no swap.
    ///
    /// To publish new content (advance the substrate), use
    /// [`Self::set_arena_with_root`].
    pub fn set_arena(&mut self, path: &str, size: u64) -> Result<()> {
        if path.len() >= ARENA_PATH_LEN {
            bail!(
                "arena path too long (max {} bytes, got {})",
                ARENA_PATH_LEN - 1,
                path.len()
            );
        }

        self.seq_begin();
        self.write_path_and_size(path, size);
        self.seq_end();

        // Flush to disk
        self.mmap.flush().context("flush control block")?;

        Ok(())
    }

    /// Payload writes shared by `set_arena*`. Caller holds the seqlock
    /// (sequence odd). `path.len() < ARENA_PATH_LEN` is checked by the
    /// callers before they take the lock, so a bail never leaves the
    /// sequence odd.
    fn write_path_and_size(&mut self, path: &str, size: u64) {
        // Path, null-terminated.
        self.volatile_copy_in(OFF_ARENA_PATH, path.as_bytes());
        self.volatile_copy_in(OFF_ARENA_PATH + path.len(), &[0]);
        self.volatile_copy_in(OFF_ARENA_SIZE, &size.to_ne_bytes());
    }

    /// Seqlock write side, entry: sequence even → odd. AcqRel so the
    /// payload stores that follow cannot be reordered before the mark;
    /// a reader that sees the odd value knows a publish is in flight.
    ///
    /// `fetch_add` rather than load-modify-store so concurrent writers
    /// (cross-process publishers — exactly what mmap-backed control
    /// blocks enable) cannot lose increments. The substrate's intended
    /// invariant is a single writer per `(path, size, root)` advance;
    /// the counter's soundness does not rely on it.
    fn seq_begin(&mut self) {
        let prev = self.sequence().fetch_add(1, Ordering::AcqRel);
        debug_assert_eq!(prev & 1, 0, "seq_begin: a publish was already in flight");
    }

    /// Seqlock write side, exit: sequence odd → even. Release publishes
    /// every payload store before it to a reader whose Acquire fence
    /// follows its copy.
    fn seq_end(&mut self) {
        let prev = self.sequence().fetch_add(1, Ordering::Release);
        debug_assert_eq!(prev & 1, 1, "seq_end: no publish was in flight");
    }

    /// **T2.4: atomic publish of (path, size, current_root) under a
    /// single Release-ordering.**
    ///
    /// This is the substrate's content-addressed advance primitive —
    /// `current_root` IS the published state. Plain byte writes for
    /// path, size, and root are followed by a Release-store of the
    /// private sync counter. Polling readers do an Acquire-load on
    /// the same counter inside `current_root()`, establishing the
    /// happens-before edge that makes the byte writes visible.
    ///
    /// Use this in the snapshot critical path:
    ///
    /// ```ignore
    /// let root = blake3::hash(&db_bytes).into();
    /// ctrl.set_arena_with_root(&arena_path, new_size, root)?;
    /// ```
    ///
    /// Σ root semantics (decade `ley-line-open-9d30ac` §3.4):
    /// `current_root = BLAKE3(arena_buffer)`. Locked to BLAKE3.
    ///
    /// Polling readers (HotSwapGraph) detect this advance by
    /// comparing `current_root()` against their cached value.
    /// Different root → swap. Same root → no swap (idempotent
    /// re-publish or no-op snapshot).
    pub fn set_arena_with_root(
        &mut self,
        path: &str,
        size: u64,
        current_root: [u8; 32],
    ) -> Result<()> {
        self.set_arena_with_root_hooked(path, size, current_root, &mut || {})
    }

    /// `set_arena_with_root` with a seam between the two halves of the
    /// root write. Production passes a no-op; the concurrent-publish test
    /// uses it to hold the writer mid-root so a reader is GUARANTEED to
    /// overlap the write — the race the protocol must make unobservable,
    /// made deterministic instead of probabilistic (bead
    /// ley-line-open-49a1ef).
    fn set_arena_with_root_hooked(
        &mut self,
        path: &str,
        size: u64,
        current_root: [u8; 32],
        mid_root: &mut dyn FnMut(),
    ) -> Result<()> {
        if path.len() >= ARENA_PATH_LEN {
            bail!(
                "arena path too long (max {} bytes, got {})",
                ARENA_PATH_LEN - 1,
                path.len()
            );
        }

        self.seq_begin();
        self.write_path_and_size(path, size);
        let half = CURRENT_ROOT_LEN / 2;
        self.volatile_copy_in(OFF_CURRENT_ROOT, &current_root[..half]);
        mid_root();
        self.volatile_copy_in(OFF_CURRENT_ROOT + half, &current_root[half..]);
        self.seq_end();

        // Flush to disk
        self.mmap.flush().context("flush control block")?;

        Ok(())
    }

    // -- Interrupt control (feature-gated) ----------------------------------

    /// Read the current interrupt flags atomically.
    #[cfg(feature = "interrupt")]
    pub fn interrupt_flags(&self) -> u64 {
        self.atomic_at(OFF_INTERRUPT_FLAGS).load(Ordering::Acquire)
    }

    /// Set interrupt bits (OR into existing flags) and bump the epoch.
    #[cfg(feature = "interrupt")]
    pub fn set_interrupt(&self, bits: u64) {
        self.atomic_at(OFF_INTERRUPT_FLAGS)
            .fetch_or(bits, Ordering::Release);
        self.atomic_at(OFF_INTERRUPT_EPOCH)
            .fetch_add(1, Ordering::Release);
    }

    /// Clear specific interrupt bits after handling.
    #[cfg(feature = "interrupt")]
    pub fn clear_interrupt(&self, bits: u64) {
        self.atomic_at(OFF_INTERRUPT_FLAGS)
            .fetch_and(!bits, Ordering::Release);
    }

    /// Read the interrupt epoch (monotonically increasing signal counter).
    #[cfg(feature = "interrupt")]
    pub fn interrupt_epoch(&self) -> u64 {
        self.atomic_at(OFF_INTERRUPT_EPOCH).load(Ordering::Acquire)
    }

    /// Acknowledge processing up to the given epoch.
    #[cfg(feature = "interrupt")]
    pub fn ack_interrupt(&self, epoch: u64) {
        self.atomic_at(OFF_INTERRUPT_ACK)
            .store(epoch, Ordering::Release);
    }

    /// Read the last acknowledged epoch.
    #[cfg(feature = "interrupt")]
    pub fn interrupt_ack(&self) -> u64 {
        self.atomic_at(OFF_INTERRUPT_ACK).load(Ordering::Acquire)
    }

    /// Get the sidecar payload location (offset, length).
    #[cfg(feature = "interrupt")]
    pub fn payload_location(&self) -> (u64, u64) {
        let offset = self.atomic_at(OFF_PAYLOAD_OFFSET).load(Ordering::Acquire);
        let len = self.atomic_at(OFF_PAYLOAD_LEN).load(Ordering::Acquire);
        (offset, len)
    }

    /// Set the sidecar payload location. Call before setting interrupt flags.
    #[cfg(feature = "interrupt")]
    pub fn set_payload_location(&self, offset: u64, len: u64) {
        self.atomic_at(OFF_PAYLOAD_OFFSET)
            .store(offset, Ordering::Release);
        self.atomic_at(OFF_PAYLOAD_LEN)
            .store(len, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use tempfile::tempdir;

    const ROOT_A: [u8; 32] = [0x11; 32];
    const ROOT_B: [u8; 32] = [0x22; 32];

    /// The falsifier mache's reader could not pass against the old
    /// write-then-bump protocol (bead ley-line-open-49a1ef): a reader that
    /// overlaps a publish must never see a root that was never published.
    /// The writer is held between the two halves of its root write until
    /// a reader has started a read during the hold, so the overlap is
    /// certain rather than one-in-five-hundred. With write-then-bump the
    /// reader returns half of A and half of B; with the seqlock it waits
    /// out the odd counter and returns a whole root.
    #[test]
    fn a_reader_overlapping_a_publish_never_sees_a_torn_root() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("seq.ctrl");
        let mut writer = Controller::open_or_create(&path).unwrap();
        writer.set_arena_with_root("/tmp/a", 1, ROOT_A).unwrap();
        let reader = Controller::open_or_create(&path).unwrap();
        let probe = Controller::open_or_create(&path).unwrap();

        let attempts = Arc::new(AtomicU64::new(0));
        let completed = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let torn = Arc::new(AtomicU64::new(0));
        let reader_thread = {
            let (attempts, completed, stop, torn) = (
                attempts.clone(),
                completed.clone(),
                stop.clone(),
                torn.clone(),
            );
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    attempts.fetch_add(1, Ordering::AcqRel);
                    let root = reader.current_root();
                    if root != ROOT_A && root != ROOT_B {
                        torn.fetch_add(1, Ordering::AcqRel);
                    }
                    completed.fetch_add(1, Ordering::AcqRel);
                }
            })
        };

        const PUBLISHES: u64 = 16;
        for i in 0..PUBLISHES {
            let (root, prev) = if i.is_multiple_of(2) {
                (ROOT_B, ROOT_A)
            } else {
                (ROOT_A, ROOT_B)
            };
            let (attempts, completed) = (attempts.clone(), completed.clone());
            writer
                .set_arena_with_root_hooked("/tmp/a", 1, root, &mut || {
                    // The mapping really is torn at this point: new first
                    // half, old second half. This is the state the seqlock
                    // reader must never hand out.
                    let raw = probe.unsynchronized_root();
                    assert_ne!(
                        raw, prev,
                        "hook must sit after some of the new root was written"
                    );
                    assert_ne!(
                        raw, root,
                        "hook must sit before the whole new root was written"
                    );
                    let half = CURRENT_ROOT_LEN / 2;
                    assert_eq!(raw[..half], root[..half], "hook sits after the first half");
                    assert_eq!(
                        raw[half..],
                        prev[half..],
                        "hook sits before the second half"
                    );
                    // Hold the half-written root until a read that STARTED
                    // during the hold has either finished (old protocol:
                    // it finished torn) or is still waiting on us (seqlock:
                    // it spins on the odd counter), bounded so neither
                    // outcome can hang the test.
                    let started = attempts.load(Ordering::Acquire);
                    let done = completed.load(Ordering::Acquire);
                    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
                    while attempts.load(Ordering::Acquire) <= started
                        && std::time::Instant::now() < deadline
                    {
                        std::thread::yield_now();
                    }
                    while completed.load(Ordering::Acquire) <= done
                        && std::time::Instant::now() < deadline
                    {
                        std::thread::yield_now();
                    }
                })
                .unwrap();
        }
        stop.store(true, Ordering::Release);
        reader_thread.join().unwrap();

        assert_eq!(
            torn.load(Ordering::Acquire),
            0,
            "a reader overlapping a publish observed a root that was never published"
        );
        assert!(
            completed.load(Ordering::Acquire) >= PUBLISHES,
            "the reader must have read across every publish"
        );
    }

    /// A shorter path published over a longer one must read back exactly:
    /// the null terminator has to land right after the new path, not
    /// anywhere else in the 256-byte region.
    #[test]
    fn a_shorter_path_published_over_a_longer_one_reads_back_exactly() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("paths.ctrl");
        let mut ctrl = Controller::open_or_create(&path).unwrap();
        ctrl.set_arena("/arena/with/a/rather/long/path.db", 7)
            .unwrap();
        assert_eq!(ctrl.arena_path(), "/arena/with/a/rather/long/path.db");
        ctrl.set_arena("/short", 7).unwrap();
        assert_eq!(ctrl.arena_path(), "/short");
        ctrl.set_arena_with_root("/mid/len.db", 7, ROOT_A).unwrap();
        assert_eq!(ctrl.arena_path(), "/mid/len.db");
        assert_eq!(ctrl.current_root(), ROOT_A);
    }

    /// The acceptance stress: at least 10^5 reads concurrent with a
    /// publishing writer, zero never-published roots.
    #[test]
    fn concurrent_publish_stress_yields_only_published_roots() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("stress.ctrl");
        let mut writer = Controller::open_or_create(&path).unwrap();
        writer.set_arena_with_root("/tmp/a", 1, ROOT_A).unwrap();
        let reader = Controller::open_or_create(&path).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let published = Arc::new(AtomicU64::new(0));
        let writer_thread = {
            let (stop, published) = (stop.clone(), published.clone());
            std::thread::spawn(move || {
                let mut i = 0u64;
                while !stop.load(Ordering::Acquire) {
                    let root = if i.is_multiple_of(2) { ROOT_B } else { ROOT_A };
                    writer.set_arena_with_root("/tmp/a", 1, root).unwrap();
                    i += 1;
                    published.store(i, Ordering::Release);
                }
            })
        };
        // Read until both bounds hold: at least 10^5 reads, and enough
        // publishes behind them that the reads overlapped real writes
        // (each publish also flushes the mapping, so the writer is far
        // slower than the reader).
        const MIN_READS: u64 = 100_000;
        const MIN_PUBLISHES: u64 = 8;
        let (mut reads, mut torn) = (0u64, 0u64);
        while reads < MIN_READS || published.load(Ordering::Acquire) < MIN_PUBLISHES {
            let root = reader.current_root();
            if root != ROOT_A && root != ROOT_B {
                torn += 1;
            }
            reads += 1;
        }
        stop.store(true, Ordering::Release);
        writer_thread.join().unwrap();
        assert_eq!(
            torn, 0,
            "{torn} of {reads} reads returned a never-published root"
        );
    }

    #[test]
    fn test_create_and_read() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.ctrl");

        {
            let mut ctrl = Controller::open_or_create(&path).unwrap();
            ctrl.set_arena("/tmp/arena-1", 1024 * 1024).unwrap();
        }

        // Reopen and verify path/size persist. T2.4 removed `generation`
        // from the public API; identity is `current_root`.
        let ctrl = Controller::open_or_create(&path).unwrap();
        assert_eq!(ctrl.arena_path(), "/tmp/arena-1");
        assert_eq!(ctrl.arena_size(), 1024 * 1024);
    }

    /// T2.4: re-advertise via `set_arena` does NOT change `current_root`.
    /// HotSwapGraph polling reads root, so identity is preserved.
    #[test]
    fn set_arena_re_advertise_preserves_root() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("readv.ctrl");

        let mut ctrl = Controller::open_or_create(&path).unwrap();
        let root: [u8; 32] = [0xEF; 32];
        ctrl.set_arena_with_root("/tmp/a", 100, root).unwrap();
        assert_eq!(ctrl.current_root(), root);

        // Re-advertise (different size, same content). Root unchanged.
        ctrl.set_arena("/tmp/a", 200).unwrap();
        assert_eq!(ctrl.arena_size(), 200);
        assert_eq!(
            ctrl.current_root(),
            root,
            "T2.4: set_arena (re-advertise) must not change current_root"
        );
    }

    #[test]
    fn test_path_too_long() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("long.ctrl");

        let mut ctrl = Controller::open_or_create(&path).unwrap();
        let long_path = "x".repeat(256);
        assert!(ctrl.set_arena(&long_path, 0).is_err());
    }

    #[test]
    fn control_field_offsets_consistent_layout() {
        // The offset constants (OFF_MAGIC, OFF_VERSION, …) define the
        // exact byte layout of every .ctrl file. A typo (e.g.
        // OFF_GENERATION=12 instead of 8) would mis-read every
        // existing file. Pin the values AND the consistency relations
        // so a future field addition has to thread the offsets
        // correctly. Format on disk:
        //   [0..4]    magic (u32)
        //   [4..8]    version (u32)
        //   [8..16]   generation (u64)
        //   [16..272] arena_path (256 bytes, NUL-padded)
        //   [272..280] arena_size (u64)
        //   [280..]   interrupt fields when feature enabled
        assert_eq!(OFF_MAGIC, 0);
        assert_eq!(OFF_VERSION, OFF_MAGIC + 4, "version follows magic (u32)");
        assert_eq!(
            OFF_GENERATION,
            OFF_VERSION + 4,
            "generation follows version (u32)"
        );
        assert_eq!(
            OFF_ARENA_PATH,
            OFF_GENERATION + 8,
            "arena_path follows generation (u64)"
        );
        assert_eq!(ARENA_PATH_LEN, 256, "arena path is fixed 256 bytes");
        assert_eq!(
            OFF_ARENA_SIZE,
            OFF_ARENA_PATH + ARENA_PATH_LEN,
            "arena_size follows arena_path",
        );
        // arena_size occupies 8 bytes (u64). Promote to a const-time
        // assert so the check fires at compile time rather than per
        // test-run; clippy (rightly) flags runtime asserts on
        // compile-time-constant expressions.
        const _: () = assert!(OFF_ARENA_SIZE + 8 <= CONTROL_SIZE);
    }

    #[test]
    fn current_root_layout_pin() {
        // Σ root pointer lives at OFF_CURRENT_ROOT = 320, occupies 32
        // bytes. T2.1 (ley-line-open-baa90a) places it after the
        // interrupt block (which reserves 280..320 even when the
        // feature is off — those bytes are unused but the offset is
        // disk-format reserved).
        //
        // A future field that mis-overlaps OFF_CURRENT_ROOT would
        // silently corrupt every .ctrl's root on first write. Pin the
        // value AND the relation to the interrupt block AND the bound
        // against CONTROL_SIZE.
        assert_eq!(OFF_CURRENT_ROOT, 320, "current_root at offset 320");
        assert_eq!(CURRENT_ROOT_LEN, 32, "current_root is 32 bytes (BLAKE3)");
        const _: () = assert!(OFF_CURRENT_ROOT + CURRENT_ROOT_LEN <= CONTROL_SIZE);
        // Reserved gap [280..320] for interrupt fields, regardless of
        // feature. current_root must not collide. Const assert at
        // compile-time so a refactor that moved OFF_CURRENT_ROOT below
        // the interrupt block would fail to build.
        const _: () = assert!(OFF_CURRENT_ROOT >= 320);
    }

    #[test]
    fn fresh_control_has_zero_current_root() {
        // T2.1 contract: a freshly opened control file has
        // current_root = [0; 32], the "no current root yet" sentinel.
        // Every reader treats Hash::ZERO as "fall back to non-root
        // path" — a refactor that initialized current_root to garbage
        // would silently advertise a valid-looking root that no blob
        // store has.
        let dir = tempdir().unwrap();
        let ctrl_path = dir.path().join("fresh.ctrl");
        let ctrl = Controller::open_or_create(&ctrl_path).unwrap();
        assert_eq!(
            ctrl.current_root(),
            [0u8; 32],
            "fresh control file must have zero current_root (sentinel)",
        );
    }

    #[test]
    fn current_root_round_trips_through_set() {
        // T2.1 reader/writer pairing: set + get produces the same
        // bytes. Pin both directions so a refactor that introduced
        // byte-order swapping or accidental truncation would surface
        // here.
        let dir = tempdir().unwrap();
        let ctrl_path = dir.path().join("rt.ctrl");
        let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();

        let root: [u8; 32] = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55,
            0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0xfe, 0xdc, 0xba, 0x98,
            0x76, 0x54, 0x32, 0x10,
        ];
        ctrl.set_current_root(root).unwrap();
        assert_eq!(ctrl.current_root(), root);
    }

    #[test]
    fn current_root_persists_across_reopen() {
        // T2.1: current_root is stored in mmap and survives Controller
        // re-open (which is how a fresh process picks up the previous
        // state). Pin so a refactor that kept current_root in an
        // in-memory cache only would surface here.
        let dir = tempdir().unwrap();
        let ctrl_path = dir.path().join("persist.ctrl");
        let root: [u8; 32] = [0xab; 32];
        {
            let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
            ctrl.set_current_root(root).unwrap();
            // Drop ctrl, mmap unmaps + flushes
        }
        let ctrl2 = Controller::open_or_create(&ctrl_path).unwrap();
        assert_eq!(
            ctrl2.current_root(),
            root,
            "current_root must persist across Controller re-open",
        );
    }

    #[test]
    fn current_root_does_not_collide_with_existing_fields() {
        // Drift guard: writing current_root must not corrupt any other
        // field. Set arena, then set current_root, then verify all
        // earlier fields still read correctly. T2.4 removed
        // generation; the OFF_GENERATION slot is now the private sync
        // counter, no longer asserted on.
        let dir = tempdir().unwrap();
        let ctrl_path = dir.path().join("nocollide.ctrl");
        let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();

        ctrl.set_arena("/some/arena/path", 1024 * 1024).unwrap();
        let root: [u8; 32] = [0x55; 32];
        ctrl.set_current_root(root).unwrap();

        assert_eq!(ctrl.arena_path(), "/some/arena/path");
        assert_eq!(ctrl.arena_size(), 1024 * 1024);
        assert_eq!(ctrl.current_root(), root);
    }

    #[test]
    fn control_disk_format_constants() {
        // Sister disk-format-stability pin to layout.rs's MAGIC +
        // VERSION + HEADER_SIZE triplet. CONTROL_SIZE (4096) is the
        // exact byte size of every .ctrl file on disk; bumping it
        // invalidates every existing controller. MAGIC = 0x4C455943
        // = ASCII "LEYC" (big-endian) — distinct from arena's "LEY0"
        // so a tool reading either can dispatch on the magic. VERSION
        // = 1 until a deliberate migration ships.
        assert_eq!(CONTROL_SIZE, 4096, "CONTROL_SIZE pinned at one OS page");
        assert_eq!(MAGIC, 0x4C455943, "MAGIC must be ASCII 'LEYC'");
        let bytes = MAGIC.to_be_bytes();
        assert_eq!(bytes, *b"LEYC", "MAGIC bytes must spell 'LEYC'");
        assert_eq!(
            VERSION, 3,
            "VERSION must be 3 (breaking — seqlock publication, bead ley-line-open-49a1ef)"
        );
        // Distinct from the arena's MAGIC ("LEY0"). A tool reading
        // either header dispatches on the magic to pick the parser.
        assert_ne!(
            MAGIC,
            crate::layout::ArenaHeader::MAGIC,
            "control + arena MAGIC must differ for dispatch",
        );
    }

    #[test]
    fn test_invalid_magic() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.ctrl");

        // Write garbage magic
        std::fs::write(&path, [0xFF; CONTROL_SIZE]).unwrap();

        let result = Controller::open_or_create(&path);
        assert!(result.is_err());
    }

    #[cfg(feature = "interrupt")]
    mod interrupt_tests {
        use super::*;
        use crate::interrupt;

        #[test]
        fn test_set_and_read_interrupt_flags() {
            let dir = tempdir().unwrap();
            let path = dir.path().join("irq.ctrl");

            let ctrl = Controller::open_or_create(&path).unwrap();
            assert_eq!(ctrl.interrupt_flags(), 0);

            ctrl.set_interrupt(interrupt::HALT);
            assert_eq!(ctrl.interrupt_flags() & interrupt::HALT, interrupt::HALT);
            assert_eq!(ctrl.interrupt_epoch(), 1);

            ctrl.set_interrupt(interrupt::COHERENCE_ALERT);
            assert_eq!(
                ctrl.interrupt_flags(),
                interrupt::HALT | interrupt::COHERENCE_ALERT
            );
            assert_eq!(ctrl.interrupt_epoch(), 2);
        }

        #[test]
        fn test_clear_interrupt_flags() {
            let dir = tempdir().unwrap();
            let path = dir.path().join("irq_clear.ctrl");

            let ctrl = Controller::open_or_create(&path).unwrap();
            ctrl.set_interrupt(interrupt::HALT | interrupt::PAUSE | interrupt::REDIRECT);
            assert_eq!(ctrl.interrupt_flags().count_ones(), 3);

            ctrl.clear_interrupt(interrupt::HALT);
            assert_eq!(
                ctrl.interrupt_flags(),
                interrupt::PAUSE | interrupt::REDIRECT
            );

            ctrl.clear_interrupt(interrupt::PAUSE | interrupt::REDIRECT);
            assert_eq!(ctrl.interrupt_flags(), 0);
        }

        #[test]
        fn test_cross_process_interrupt_visibility() {
            let dir = tempdir().unwrap();
            let path = dir.path().join("irq_cross.ctrl");

            // Writer
            let writer = Controller::open_or_create(&path).unwrap();
            writer.set_interrupt(interrupt::COHERENCE_ALERT);

            // Reader (separate Controller instance, same file)
            let reader = Controller::open_or_create(&path).unwrap();
            assert_ne!(reader.interrupt_flags() & interrupt::COHERENCE_ALERT, 0);
            assert_eq!(reader.interrupt_epoch(), 1);
        }

        #[test]
        fn test_ack_protocol() {
            let dir = tempdir().unwrap();
            let path = dir.path().join("irq_ack.ctrl");

            let ctrl = Controller::open_or_create(&path).unwrap();
            assert_eq!(ctrl.interrupt_ack(), 0);

            ctrl.set_interrupt(interrupt::HALT);
            let epoch = ctrl.interrupt_epoch();
            ctrl.ack_interrupt(epoch);
            assert_eq!(ctrl.interrupt_ack(), epoch);
        }

        #[test]
        fn test_payload_location() {
            let dir = tempdir().unwrap();
            let path = dir.path().join("irq_payload.ctrl");

            let ctrl = Controller::open_or_create(&path).unwrap();
            assert_eq!(ctrl.payload_location(), (0, 0));

            ctrl.set_payload_location(4096, 2048);
            ctrl.set_interrupt(interrupt::REDIRECT);

            let (offset, len) = ctrl.payload_location();
            assert_eq!(offset, 4096);
            assert_eq!(len, 2048);
        }
    }

    /// T2.2/T2.4: `set_arena_with_root` writes path, size, and
    /// current_root atomically. After the call, all three reflect the
    /// new values. Pin the basic API contract.
    #[test]
    fn set_arena_with_root_writes_all_fields() {
        let dir = tempdir().unwrap();
        let ctrl_path = dir.path().join("t22-basic.ctrl");
        let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();

        let root: [u8; 32] = [0xAB; 32];
        ctrl.set_arena_with_root("/some/arena", 4096, root).unwrap();

        assert_eq!(ctrl.arena_path(), "/some/arena");
        assert_eq!(ctrl.arena_size(), 4096);
        assert_eq!(ctrl.current_root(), root);
    }

    /// T2.2/T2.4: cross-Controller visibility — a fresh `Controller`
    /// opened after the writer's `set_arena_with_root` returns sees
    /// the committed (path, size, current_root). HotSwapGraph reader
    /// path depends on this: writer publishes via flush; subsequent
    /// readers see consistent state.
    #[test]
    fn set_arena_with_root_visible_across_controllers() {
        let dir = tempdir().unwrap();
        let ctrl_path = dir.path().join("t22-visible.ctrl");

        // Writer.
        {
            let mut w = Controller::open_or_create(&ctrl_path).unwrap();
            w.set_arena_with_root("/some/arena", 4096, [0xCD; 32])
                .unwrap();
        }

        let r = Controller::open_or_create(&ctrl_path).unwrap();
        assert_eq!(r.current_root(), [0xCD; 32]);
        assert_eq!(r.arena_path(), "/some/arena");
        assert_eq!(r.arena_size(), 4096);
    }

    /// T2.2/T2.4: writer-monotone advancement under concurrent writes.
    /// If the reader observes `current_root` value V at iteration N,
    /// then V is from iteration N or later (writer-races-ahead is OK).
    /// V being from an *earlier* iteration is the bug case — the
    /// Release-store of the sync counter would not be publishing the
    /// prior writes to current_root.
    #[test]
    fn set_arena_with_root_root_never_stale_under_writer_race() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering as AOrd};
        use std::thread;

        let dir = tempdir().unwrap();
        let ctrl_path = dir.path().join("t24-monotone.ctrl");

        let mut writer = Controller::open_or_create(&ctrl_path).unwrap();
        writer.set_arena_with_root("/x", 8, [0u8; 32]).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let stop_reader = stop.clone();
        let path_for_reader = ctrl_path.clone();

        let reader = thread::spawn(move || {
            let r = Controller::open_or_create(&path_for_reader).unwrap();
            let mut last_seen_root_byte: u8 = 0;
            let mut regressions = 0usize;
            let mut samples = 0usize;
            while !stop_reader.load(AOrd::Acquire) {
                let root = r.current_root();
                samples += 1;
                // Writer monotone: writes root[0] = 1, 2, 3, …, 50 in order.
                // Once reader has seen root[0] = K, subsequent reads must
                // see root[0] >= K (writer never goes backward).
                if root[0] > 0 && root[0] < last_seen_root_byte {
                    regressions += 1;
                }
                if root[0] > last_seen_root_byte {
                    last_seen_root_byte = root[0];
                }
            }
            (regressions, samples)
        });

        for n in 1u8..=50 {
            writer.set_arena_with_root("/x", 8, [n; 32]).unwrap();
            std::thread::sleep(std::time::Duration::from_micros(100));
        }
        stop.store(true, AOrd::Release);

        let (regressions, samples) = reader.join().unwrap();
        assert!(samples > 0, "reader observed no samples — invalid test");
        assert_eq!(
            regressions, 0,
            "T2.4 monotone invariant violated: reader observed root[0] \
             go backward across {samples} samples. Means current_root \
             writes are not properly fenced by the sync counter Release.",
        );
    }

    /// VERSION mismatch on an existing .ctrl is a hard error. A V2
    /// file (write-then-bump, no in-flight marker) must not be read by
    /// a V3 seqlock binary, and the error must name both versions.
    #[test]
    fn open_rejects_mismatched_version() {
        for old in [1u32, 2] {
            let dir = tempdir().unwrap();
            let path = dir.path().join(format!("v{old}-old.ctrl"));

            // Hand-write an old control block: correct MAGIC, old version.
            let mut buf = vec![0u8; CONTROL_SIZE];
            buf[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_ne_bytes());
            buf[OFF_VERSION..OFF_VERSION + 4].copy_from_slice(&old.to_ne_bytes());
            std::fs::write(&path, &buf).unwrap();

            let result = Controller::open_or_create(&path);
            let err = match result {
                Ok(_) => panic!("expected VERSION mismatch error for v{old}"),
                Err(e) => e,
            };
            let msg = format!("{err:#}");
            assert!(
                msg.contains("VERSION mismatch")
                    && msg.contains(&format!("file has v{old}"))
                    && msg.contains("expects v3"),
                "error must name the file's v{old} and the binary's v3 (got: {msg})",
            );
        }
    }
}
