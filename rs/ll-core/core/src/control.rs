//! Control block for content-addressed Σ substrate identity (T2.4 V2).
//!
//! A 4096-byte memory-mapped file naming the currently-active arena.
//! Substrate identity is `current_root` — BLAKE3 over the live arena
//! payload. Polling readers compare `current_root()` for change
//! detection; an atomic Acquire-load on a private sync counter fences
//! the byte reads against the writer's Release-store inside
//! `set_arena*`.
//!
//! Layout (matches Go `internal/control/control.go` post-T2.4):
//!   [0..4]     Magic: 0x4C455943 ('LEYC')
//!   [4..8]     Version: u32 (must be 2)
//!   [8..16]    Sync atom: AtomicU64 (private — Acquire/Release fence;
//!                                    formerly the V1 `generation` field)
//!   [16..272]  ArenaPath: [u8; 256] (null-terminated)
//!   [272..280] ArenaSize: u64
//!   [280..320] Interrupt fields (feature = "interrupt"; reserved otherwise)
//!   [320..352] CurrentRoot: [u8; 32]  — Σ root pointer
//!   [352..4096] Padding
//!
//! V1 (pre-T2.4) exposed `generation` as a public counter; V2 removes
//! that surface entirely. Old binaries reading new files (or vice
//! versa) hit the explicit VERSION-mismatch error in `open_or_create`.

use std::fs::OpenOptions;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::mmap::mmap_write;
use anyhow::{Context, Result, bail};
use memmap2::MmapMut;

/// Control block size: one page.
pub const CONTROL_SIZE: usize = 4096;

/// Magic number: 'LEYC' = 0x4C455943
pub const MAGIC: u32 = 0x4C455943;

/// **T2.4 — Σ content-addressed substrate, breaking version 2.**
///
/// V1 (pre-T2.4) exposed `generation: u64` as the public substrate
/// identity. V2 removes generation from the public API entirely;
/// `current_root` (BLAKE3 of arena bytes) IS the substrate identity.
/// The byte slot at `OFF_GENERATION` is preserved as a *private*
/// monotone sync counter — readers Acquire-load it inside
/// `current_root()` to fence the subsequent byte reads, and writers
/// Release-store it inside `set_arena*` to publish prior plain stores.
/// Callers cannot access this counter; all polling is by root.
///
/// Bumping VERSION 1 → 2 means old `.ctrl` files are rejected by new
/// binaries and vice versa. This is a deliberate breakpoint pairing
/// with mache's CGO-elimination cutover (decade `9d30ac` T2,
/// epic `mache-36d961`). Coordinate releases.
pub const VERSION: u32 = 2;

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

// Compile-time invariant: the sync atom slot must be 8-byte aligned for
// the AtomicU64 cast in sync_counter_acquire / bump_sync_counter_release
// to be sound. mmap is page-aligned, so any 8-byte-aligned offset within
// it gives an 8-byte-aligned pointer. If a future field reorder violates
// this, the cast becomes UB on architectures requiring naturally-aligned
// atomics (e.g. aarch64 LSE) — fail compilation instead.
const _: () = assert!(
    OFF_GENERATION.is_multiple_of(8),
    "sync atom must be 8-byte aligned"
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
        let existing_magic = u32::from_ne_bytes(mmap[OFF_MAGIC..OFF_MAGIC + 4].try_into().unwrap());

        if existing_magic == 0 {
            mmap[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_ne_bytes());
            mmap[OFF_VERSION..OFF_VERSION + 4].copy_from_slice(&VERSION.to_ne_bytes());
        } else if existing_magic != MAGIC {
            bail!("invalid control block magic: 0x{:08X}", existing_magic);
        } else {
            // VERSION mismatch is a hard error. V1 controllers
            // exposed a public `generation` API that v0.2.0 removed;
            // reading a V1 file as V2 (or vice versa) would silently
            // misinterpret the sync atom slot — refuse explicitly.
            let existing_version =
                u32::from_ne_bytes(mmap[OFF_VERSION..OFF_VERSION + 4].try_into().unwrap());
            if existing_version != VERSION {
                bail!(
                    "control block VERSION mismatch: file has v{}, this binary expects v{}. \
                     LLO v0.2.0 removed the V1 `generation` field from the public API; \
                     old binaries cannot read new files and vice versa. Coordinate LLO + \
                     mache release cutover (LLO v0.2.0 + mache v0.8.0 ship together).",
                    existing_version,
                    VERSION
                );
            }
        }

        Ok(Controller { mmap })
    }

    /// **T2.4 internal sync atom — Acquire load.** Not exposed in the
    /// public API. Pairs with the writer's Release-store inside
    /// `set_arena*` to fence the plain byte reads of `current_root`,
    /// `arena_path`, and `arena_size`. Public callers compare
    /// `current_root()` for identity / change detection.
    fn sync_counter_acquire(&self) -> u64 {
        let ptr = self.mmap[OFF_GENERATION..].as_ptr() as *const AtomicU64;
        // SAFETY: mmap is page-aligned, offset 8 is 8-byte aligned,
        // AtomicU64 is same layout as u64.
        unsafe { (*ptr).load(Ordering::Acquire) }
    }

    /// Get the path to the currently active arena.
    pub fn arena_path(&self) -> String {
        let bytes = &self.mmap[OFF_ARENA_PATH..OFF_ARENA_PATH + ARENA_PATH_LEN];
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(ARENA_PATH_LEN);
        String::from_utf8_lossy(&bytes[..end]).to_string()
    }

    /// Get the size of the currently active arena.
    pub fn arena_size(&self) -> u64 {
        u64::from_ne_bytes(
            self.mmap[OFF_ARENA_SIZE..OFF_ARENA_SIZE + 8]
                .try_into()
                .unwrap(),
        )
    }

    /// Read the current arena root (Σ root pointer).
    ///
    /// **T2.4: this is the substrate's primary identity field.**
    /// Returns `[0u8; 32]` — the zero sentinel — when no root has
    /// been published yet (fresh control file). Callers comparing
    /// roots for change detection (e.g. HotSwapGraph polling) treat
    /// `current_root() != cached_root` as a publish event.
    ///
    /// Internally Acquire-loads the private sync counter, fencing
    /// the plain byte reads of `current_root` against the writer's
    /// Release-store inside `set_arena*`. **The Acquire is per-call:**
    /// it pairs with the Release for the bytes read inside this
    /// method only. Subsequent calls to `arena_path()` / `arena_size()`
    /// are NOT fenced relative to a concurrent writer; under polling
    /// the safe pattern is to dispatch on root change, then re-open a
    /// fresh `Controller` (single mmap snapshot of the file). See
    /// `HotSwapGraph::maybe_swap` for the reference impl.
    pub fn current_root(&self) -> [u8; 32] {
        // Acquire fence on the internal sync counter. Any prior
        // writer-Release-store happens-before the byte reads below.
        let _ = self.sync_counter_acquire();
        let mut out = [0u8; 32];
        out.copy_from_slice(&self.mmap[OFF_CURRENT_ROOT..OFF_CURRENT_ROOT + CURRENT_ROOT_LEN]);
        out
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

    /// **T2.4: re-advertise without publishing new content.** Writes
    /// path and size to the control block, increments the internal
    /// sync counter via Release-store, but **preserves the existing
    /// `current_root` unchanged**. Used for the snapshot's step-2
    /// re-advertisement (file grow without commit) and for test
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

        // Write path (null-terminated)
        self.mmap[OFF_ARENA_PATH..OFF_ARENA_PATH + path.len()].copy_from_slice(path.as_bytes());
        self.mmap[OFF_ARENA_PATH + path.len()] = 0;

        // Write size
        self.mmap[OFF_ARENA_SIZE..OFF_ARENA_SIZE + 8].copy_from_slice(&size.to_ne_bytes());

        // Bump the internal sync counter via Release-store. Readers
        // doing Acquire-load on the counter see all prior writes
        // (path, size). current_root is *not* modified here. The
        // Release-store itself is the ordering — no separate fence
        // needed.
        self.bump_sync_counter_release();

        // Flush to disk
        self.mmap.flush().context("flush control block")?;

        Ok(())
    }

    /// Internal: atomically increment the sync counter and publish
    /// via Release ordering. Pairs with `sync_counter_acquire`.
    ///
    /// Uses `fetch_add(1, Release)` rather than load-modify-store so
    /// concurrent writers (cross-process publishers — exactly what
    /// mmap-backed control blocks enable) cannot lose increments.
    /// The substrate's intended invariant is single-writer per
    /// `(path, size, root)` advance, but the underlying byte slot is
    /// process-shared and we should not rely on the invariant for
    /// soundness of the counter itself. Release ordering still gives
    /// the happens-before pair with `sync_counter_acquire` for the
    /// plain byte writes preceding this call.
    fn bump_sync_counter_release(&mut self) {
        let ptr = self.mmap[OFF_GENERATION..].as_ptr() as *const AtomicU64;
        // SAFETY: mmap is page-aligned, OFF_GENERATION is 8-byte
        // aligned (compile-time asserted at the top of this module),
        // AtomicU64 has the same layout as u64.
        unsafe {
            let _ = (*ptr).fetch_add(1, Ordering::Release);
        }
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
        if path.len() >= ARENA_PATH_LEN {
            bail!(
                "arena path too long (max {} bytes, got {})",
                ARENA_PATH_LEN - 1,
                path.len()
            );
        }

        // Write path (null-terminated)
        self.mmap[OFF_ARENA_PATH..OFF_ARENA_PATH + path.len()].copy_from_slice(path.as_bytes());
        self.mmap[OFF_ARENA_PATH + path.len()] = 0;

        // Write size
        self.mmap[OFF_ARENA_SIZE..OFF_ARENA_SIZE + 8].copy_from_slice(&size.to_ne_bytes());

        // T2.2: Write current_root BEFORE the Release-store of
        // sync counter. Plain byte copy; the Release publishes it.
        self.mmap[OFF_CURRENT_ROOT..OFF_CURRENT_ROOT + CURRENT_ROOT_LEN]
            .copy_from_slice(&current_root);

        // T2.4: bump the internal sync counter via Release-store.
        // The Release-store itself fences the prior plain byte writes
        // (path, size, current_root) — readers doing the paired
        // Acquire-load inside `current_root()` see them all. No
        // separate hardware fence needed.
        self.bump_sync_counter_release();

        // Flush to disk
        self.mmap.flush().context("flush control block")?;

        Ok(())
    }

    // -- Interrupt control (feature-gated) ----------------------------------

    /// Read the current interrupt flags atomically.
    #[cfg(feature = "interrupt")]
    pub fn interrupt_flags(&self) -> u64 {
        let ptr = self.mmap[OFF_INTERRUPT_FLAGS..].as_ptr() as *const AtomicU64;
        unsafe { (*ptr).load(Ordering::Acquire) }
    }

    /// Set interrupt bits (OR into existing flags) and bump the epoch.
    #[cfg(feature = "interrupt")]
    pub fn set_interrupt(&self, bits: u64) {
        let flags_ptr = self.mmap[OFF_INTERRUPT_FLAGS..].as_ptr() as *const AtomicU64;
        let epoch_ptr = self.mmap[OFF_INTERRUPT_EPOCH..].as_ptr() as *const AtomicU64;
        unsafe {
            (*flags_ptr).fetch_or(bits, Ordering::Release);
            (*epoch_ptr).fetch_add(1, Ordering::Release);
        }
    }

    /// Clear specific interrupt bits after handling.
    #[cfg(feature = "interrupt")]
    pub fn clear_interrupt(&self, bits: u64) {
        let ptr = self.mmap[OFF_INTERRUPT_FLAGS..].as_ptr() as *const AtomicU64;
        unsafe { (*ptr).fetch_and(!bits, Ordering::Release) };
    }

    /// Read the interrupt epoch (monotonically increasing signal counter).
    #[cfg(feature = "interrupt")]
    pub fn interrupt_epoch(&self) -> u64 {
        let ptr = self.mmap[OFF_INTERRUPT_EPOCH..].as_ptr() as *const AtomicU64;
        unsafe { (*ptr).load(Ordering::Acquire) }
    }

    /// Acknowledge processing up to the given epoch.
    #[cfg(feature = "interrupt")]
    pub fn ack_interrupt(&self, epoch: u64) {
        let ptr = self.mmap[OFF_INTERRUPT_ACK..].as_ptr() as *const AtomicU64;
        unsafe { (*ptr).store(epoch, Ordering::Release) };
    }

    /// Read the last acknowledged epoch.
    #[cfg(feature = "interrupt")]
    pub fn interrupt_ack(&self) -> u64 {
        let ptr = self.mmap[OFF_INTERRUPT_ACK..].as_ptr() as *const AtomicU64;
        unsafe { (*ptr).load(Ordering::Acquire) }
    }

    /// Get the sidecar payload location (offset, length).
    #[cfg(feature = "interrupt")]
    pub fn payload_location(&self) -> (u64, u64) {
        let off_ptr = self.mmap[OFF_PAYLOAD_OFFSET..].as_ptr() as *const AtomicU64;
        let len_ptr = self.mmap[OFF_PAYLOAD_LEN..].as_ptr() as *const AtomicU64;
        unsafe {
            let offset = (*off_ptr).load(Ordering::Acquire);
            let len = (*len_ptr).load(Ordering::Acquire);
            (offset, len)
        }
    }

    /// Set the sidecar payload location. Call before setting interrupt flags.
    #[cfg(feature = "interrupt")]
    pub fn set_payload_location(&self, offset: u64, len: u64) {
        let off_ptr = self.mmap[OFF_PAYLOAD_OFFSET..].as_ptr() as *const AtomicU64;
        let len_ptr = self.mmap[OFF_PAYLOAD_LEN..].as_ptr() as *const AtomicU64;
        unsafe {
            (*off_ptr).store(offset, Ordering::Release);
            (*len_ptr).store(len, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
            VERSION, 2,
            "T2.4: VERSION must be 2 (breaking — generation removed from public API)"
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

    /// T2.4: VERSION mismatch on existing .ctrl is a hard error. Old
    /// V1 controllers (pre-T2.4) cannot be read by new V2 binaries —
    /// the breaking-change discipline ADR-0014-style.
    #[test]
    fn open_rejects_mismatched_version() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("v1-old.ctrl");

        // Hand-write a fake V1 control block: correct MAGIC, version=1.
        let mut buf = vec![0u8; CONTROL_SIZE];
        buf[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_ne_bytes());
        buf[OFF_VERSION..OFF_VERSION + 4].copy_from_slice(&1u32.to_ne_bytes());
        std::fs::write(&path, &buf).unwrap();

        let result = Controller::open_or_create(&path);
        let err = match result {
            Ok(_) => panic!("expected VERSION mismatch error"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("VERSION mismatch") && msg.contains("v1") && msg.contains("v2"),
            "T2.4 error must clearly identify the V1→V2 breaking change (got: {msg})",
        );
    }
}
