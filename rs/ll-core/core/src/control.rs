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
//!   [280..4096] Padding

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
