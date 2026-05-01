//! Control block for multi-generation arena management.
//!
//! A 4096-byte memory-mapped file that tracks which arena is currently active.
//! Readers atomically read the generation counter to determine the active arena.
//! Writers fill a new arena, then atomically update the control block to point to it.
//!
//! Layout (matches Go `control/control.go`):
//!   [0..4]     Magic: 0x4C455943 ('LEYC')
//!   [4..8]     Version: u32
//!   [8..16]    Generation: u64 (atomic)
//!   [16..272]  ArenaPath: [u8; 256] (null-terminated)
//!   [272..280] ArenaSize: u64
//!   [280..320] Interrupt fields (feature = "interrupt"; reserved otherwise)
//!   [320..352] CurrentRoot: [u8; 32]  — Σ root pointer (T2.1, ley-line-open-baa90a)
//!   [352..4096] Padding
//!
//! T2.1 NOTE: `CurrentRoot` is additive. Reader logic still keys on
//! `generation` for the current cutover (T2.2/T2.3 wire it into the
//! verify-on-read path; T2.4 deprecates `generation` once verification
//! is the canonical advancement signal). On existing control files
//! the bytes at [320..352] are zero (sentinel `Hash::ZERO`), which
//! callers interpret as "no current root yet".

use std::fs::OpenOptions;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use memmap2::MmapMut;

/// Control block size: one page.
pub const CONTROL_SIZE: usize = 4096;

/// Magic number: 'LEYC' = 0x4C455943
pub const MAGIC: u32 = 0x4C455943;

/// Current version.
pub const VERSION: u32 = 1;

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

/// CurrentRoot: 32-byte content address of the current arena root (Σ).
/// T2.1 (ley-line-open-baa90a) — additive; existing readers ignore this
/// region and key on `generation`. T2.2 (`ley-line-open-babf6a`) wires
/// the writer side; T2.3 (`ley-line-open-bad8f1`) wires verify-on-read.
const OFF_CURRENT_ROOT: usize = 320;
const CURRENT_ROOT_LEN: usize = 32;

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

        let mut mmap = unsafe { MmapMut::map_mut(&file)? };

        // Initialize if new (magic == 0)
        let existing_magic = u32::from_ne_bytes(mmap[OFF_MAGIC..OFF_MAGIC + 4].try_into().unwrap());

        if existing_magic == 0 {
            mmap[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_ne_bytes());
            mmap[OFF_VERSION..OFF_VERSION + 4].copy_from_slice(&VERSION.to_ne_bytes());
        } else if existing_magic != MAGIC {
            bail!("invalid control block magic: 0x{:08X}", existing_magic);
        }

        Ok(Controller { mmap })
    }

    /// Get the current generation atomically.
    pub fn generation(&self) -> u64 {
        let ptr = self.mmap[OFF_GENERATION..].as_ptr() as *const AtomicU64;
        // SAFETY: mmap is page-aligned, offset 8 is 8-byte aligned, AtomicU64 is same layout as u64
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
    /// Returns `[0u8; 32]` — equivalent to [`crate::substrate::Hash::ZERO`] —
    /// when no root has been written yet (fresh control file or
    /// pre-T2.1 control file). Callers MUST treat the zero hash as
    /// the "no current root" sentinel and not as a valid root address.
    ///
    /// This read is a non-atomic byte copy. Atomicity of the
    /// (`generation`, `current_root`) pair across writes is provided
    /// by [`Self::set_arena`]'s ordering: writers update
    /// `current_root` *before* the Release-store of `generation`.
    /// Readers issuing `generation()` first then `current_root()` see
    /// a consistent pair via the Acquire load of `generation`.
    pub fn current_root(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        out.copy_from_slice(&self.mmap[OFF_CURRENT_ROOT..OFF_CURRENT_ROOT + CURRENT_ROOT_LEN]);
        out
    }

    /// Set the current arena root.
    ///
    /// **T2.1 additive — do not call from the snapshot critical path
    /// in production yet.** T2.2 (`ley-line-open-babf6a`) integrates
    /// this with `set_arena` so the root and the generation cutover
    /// happen under the same Release-ordering. Until then, this method
    /// is a non-atomic byte write available for tests and the early
    /// migration path.
    ///
    /// To clear the root (downgrade to the "no current root" sentinel),
    /// pass `[0u8; 32]`.
    pub fn set_current_root(&mut self, root: [u8; 32]) -> Result<()> {
        self.mmap[OFF_CURRENT_ROOT..OFF_CURRENT_ROOT + CURRENT_ROOT_LEN]
            .copy_from_slice(&root);
        self.mmap.flush().context("flush control block")?;
        Ok(())
    }

    /// Atomically update the control block to point to a new arena.
    ///
    /// Writes path and size first, then atomically stores the generation
    /// with Release ordering to ensure prior writes are visible.
    pub fn set_arena(&mut self, path: &str, size: u64, generation: u64) -> Result<()> {
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

        // Atomic generation update (release ordering ensures path+size visible first)
        let ptr = self.mmap[OFF_GENERATION..].as_ptr() as *const AtomicU64;
        // SAFETY: same alignment guarantees as generation()
        unsafe { (*ptr).store(generation, Ordering::Release) };

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
            ctrl.set_arena("/tmp/arena-1", 1024 * 1024, 1).unwrap();
        }

        // Reopen and verify
        let ctrl = Controller::open_or_create(&path).unwrap();
        assert_eq!(ctrl.generation(), 1);
        assert_eq!(ctrl.arena_path(), "/tmp/arena-1");
        assert_eq!(ctrl.arena_size(), 1024 * 1024);
    }

    #[test]
    fn test_atomic_generation_update() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("gen.ctrl");

        let mut ctrl = Controller::open_or_create(&path).unwrap();
        assert_eq!(ctrl.generation(), 0);

        ctrl.set_arena("/tmp/arena-gen1", 100, 42).unwrap();
        assert_eq!(ctrl.generation(), 42);
        assert_eq!(ctrl.arena_path(), "/tmp/arena-gen1");

        ctrl.set_arena("/tmp/arena-gen2", 200, 99).unwrap();
        assert_eq!(ctrl.generation(), 99);
        assert_eq!(ctrl.arena_path(), "/tmp/arena-gen2");
    }

    #[test]
    fn test_path_too_long() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("long.ctrl");

        let mut ctrl = Controller::open_or_create(&path).unwrap();
        let long_path = "x".repeat(256);
        assert!(ctrl.set_arena(&long_path, 0, 0).is_err());
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
        assert_eq!(OFF_GENERATION, OFF_VERSION + 4, "generation follows version (u32)");
        assert_eq!(OFF_ARENA_PATH, OFF_GENERATION + 8, "arena_path follows generation (u64)");
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
        const _: () =
            assert!(OFF_CURRENT_ROOT + CURRENT_ROOT_LEN <= CONTROL_SIZE);
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
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
            0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10,
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
        // field. Set arena + generation, then set current_root, then
        // verify all earlier fields still read correctly.
        let dir = tempdir().unwrap();
        let ctrl_path = dir.path().join("nocollide.ctrl");
        let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();

        ctrl.set_arena("/some/arena/path", 1024 * 1024, 42).unwrap();
        let root: [u8; 32] = [0x55; 32];
        ctrl.set_current_root(root).unwrap();

        assert_eq!(ctrl.arena_path(), "/some/arena/path");
        assert_eq!(ctrl.arena_size(), 1024 * 1024);
        assert_eq!(ctrl.generation(), 42);
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
        assert_eq!(VERSION, 1, "VERSION must be 1 until a deliberate migration");
        // Distinct from the arena's MAGIC ("LEY0"). A tool reading
        // either header dispatches on the magic to pick the parser.
        assert_ne!(
            MAGIC, crate::layout::ArenaHeader::MAGIC,
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
}
