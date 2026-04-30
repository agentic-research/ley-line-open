use anyhow::{Context, Result};
use bytemuck::{Pod, Zeroable};
use memmap2::MmapMut;
use serde::{Deserialize, Serialize};

#[repr(C)]
#[derive(Debug, Copy, Clone, Pod, Zeroable, Serialize, Deserialize)]
pub struct ArenaHeader {
    /// Magic bytes "LEY0" (0x4C455930)
    pub magic: u32,
    /// Schema version (Expect 1)
    pub version: u8,
    /// Index of the active double-buffer (0 or 1)
    pub active_buffer: u8,
    /// Explicit padding to align struct size (and align sequence to 8 bytes)
    pub padding: [u8; 2],
    /// Monotonically increasing sequence number
    pub sequence: u64,
}

impl ArenaHeader {
    pub const MAGIC: u32 = 0x4C455930;
    pub const VERSION: u8 = 1;
    pub const HEADER_SIZE: u64 = 4096;

    /// Calculate the byte offset of the active buffer within the arena file.
    pub fn active_buffer_offset(&self, file_size: u64) -> Option<u64> {
        if self.magic != Self::MAGIC || self.version != Self::VERSION || self.active_buffer > 1 {
            return None;
        }
        let buffer_size = Self::buffer_size(file_size);
        Some(Self::HEADER_SIZE + self.active_buffer as u64 * buffer_size)
    }

    /// Calculate the size of each buffer half.
    pub fn buffer_size(file_size: u64) -> u64 {
        (file_size - Self::HEADER_SIZE) / 2
    }
}

/// Write data to the inactive arena buffer and flip the header.
///
/// This is the shared primitive used by both `leyline load` and the receiver
/// to atomically update a double-buffered arena:
/// 1. Identify the inactive buffer (opposite of `header.active_buffer`)
/// 2. Write `data` into the inactive buffer, zero-pad the remainder
/// 3. Flip `active_buffer`, increment `sequence`
/// 4. Write the updated header and flush
pub fn write_to_arena(mmap: &mut MmapMut, data: &[u8]) -> Result<()> {
    let file_size = mmap.len() as u64;
    let buf_size = ArenaHeader::buffer_size(file_size) as usize;
    anyhow::ensure!(
        data.len() <= buf_size,
        "db too large for arena buffer ({} > {})",
        data.len(),
        buf_size
    );

    // Read current header
    let header: ArenaHeader = *bytemuck::from_bytes(&mmap[..std::mem::size_of::<ArenaHeader>()]);
    let pending = if header.active_buffer == 0 {
        1usize
    } else {
        0usize
    };
    let offset = ArenaHeader::HEADER_SIZE as usize + (pending * buf_size);

    // Write data + zero-pad remainder
    mmap[offset..offset + data.len()].copy_from_slice(data);
    mmap[offset + data.len()..offset + buf_size].fill(0);

    // Build updated header
    let new_header = ArenaHeader {
        magic: ArenaHeader::MAGIC,
        version: ArenaHeader::VERSION,
        active_buffer: pending as u8,
        padding: [0; 2],
        sequence: header.sequence + 1,
    };
    let header_bytes = bytemuck::bytes_of(&new_header);
    mmap[..header_bytes.len()].copy_from_slice(header_bytes);
    mmap.flush().context("flush arena after write")?;

    Ok(())
}

/// Create a fresh arena file with an initialized header and the given total size.
///
/// Returns the writable mmap. The header is set to magic/version with
/// active_buffer=0 and sequence=0. Both buffers are zeroed.
pub fn create_arena(path: &std::path::Path, arena_size: u64) -> Result<MmapMut> {
    use std::fs::OpenOptions;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .context("open arena file")?;
    file.set_len(arena_size).context("set arena file length")?;
    let mut mmap = unsafe { MmapMut::map_mut(&file)? };

    // Initialize header if fresh (magic == 0)
    let existing_magic = u32::from_ne_bytes(mmap[..4].try_into().unwrap());
    if existing_magic == 0 {
        let header = ArenaHeader {
            magic: ArenaHeader::MAGIC,
            version: ArenaHeader::VERSION,
            active_buffer: 0,
            padding: [0; 2],
            sequence: 0,
        };
        let header_bytes = bytemuck::bytes_of(&header);
        mmap[..header_bytes.len()].copy_from_slice(header_bytes);
        mmap.flush().context("flush initial arena header")?;
    }

    Ok(mmap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn write_to_arena_flips_buffer() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.arena");

        // 4096 header + 2 * 4096 buffers = 12288
        let arena_size = 4096 + 4096 * 2;
        let mut mmap = create_arena(&path, arena_size as u64).unwrap();

        // Verify initial state
        let h: ArenaHeader = *bytemuck::from_bytes(&mmap[..std::mem::size_of::<ArenaHeader>()]);
        assert_eq!(h.active_buffer, 0);
        assert_eq!(h.sequence, 0);

        // Write "hello" to arena — should go to buffer 1 (inactive), then flip
        write_to_arena(&mut mmap, b"hello").unwrap();

        let h: ArenaHeader = *bytemuck::from_bytes(&mmap[..std::mem::size_of::<ArenaHeader>()]);
        assert_eq!(h.active_buffer, 1);
        assert_eq!(h.sequence, 1);

        // Verify data in buffer 1
        let buf1_offset = 4096 + 4096; // header + buf0
        assert_eq!(&mmap[buf1_offset..buf1_offset + 5], b"hello");
        // Remainder zero-padded
        assert!(
            mmap[buf1_offset + 5..buf1_offset + 4096]
                .iter()
                .all(|&b| b == 0)
        );

        // Buffer 0 should still be empty
        assert!(mmap[4096..4096 + 4096].iter().all(|&b| b == 0));

        // Second write goes to buffer 0, flips back
        write_to_arena(&mut mmap, b"world").unwrap();

        let h: ArenaHeader = *bytemuck::from_bytes(&mmap[..std::mem::size_of::<ArenaHeader>()]);
        assert_eq!(h.active_buffer, 0);
        assert_eq!(h.sequence, 2);
        assert_eq!(&mmap[4096..4096 + 5], b"world");

        // Buffer 1 still has "hello"
        assert_eq!(&mmap[buf1_offset..buf1_offset + 5], b"hello");
    }

    #[test]
    fn write_to_arena_rejects_oversized_data() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("small.arena");

        let arena_size = 4096 + 100 * 2; // 100-byte buffers
        let mut mmap = create_arena(&path, arena_size as u64).unwrap();

        let big_data = vec![0xAB; 200]; // larger than buffer
        let result = write_to_arena(&mut mmap, &big_data);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too large"));
    }

    #[test]
    fn arena_header_size_constant_is_4096() {
        // ArenaHeader::HEADER_SIZE = 4096 is a disk-format constant
        // baked into every arena file: buffer_size and active_buffer_
        // offset both subtract / add this value. Bumping it silently
        // shifts every buffer offset, making prior arenas unreadable.
        // Sister pin to MAGIC + VERSION literals. 4096 = one OS page,
        // load-bearing for mmap alignment.
        assert_eq!(ArenaHeader::HEADER_SIZE, 4096);
    }

    #[test]
    fn arena_header_magic_and_version_literals() {
        // ArenaHeader::MAGIC and VERSION are baked into every arena
        // file on disk. Bumping either silently invalidates every
        // existing file — daemons and tools would fail to read prior
        // arenas with no clear migration path. The
        // active_buffer_offset_rejects_bad_header test pins the
        // rejection behavior; this pins the literal values directly
        // so a typo in the constant is caught at the unit level
        // rather than only via downstream parsing failures.
        assert_eq!(
            ArenaHeader::MAGIC,
            0x4C455930,
            "MAGIC must be ASCII bytes 'LEY0' = 0x4C455930",
        );
        // Sanity: those bytes are literally L, E, Y, 0 in big-endian.
        let bytes = ArenaHeader::MAGIC.to_be_bytes();
        assert_eq!(bytes, *b"LEY0", "MAGIC bytes must spell 'LEY0'");
        assert_eq!(ArenaHeader::VERSION, 1, "VERSION must be 1 until a deliberate migration");
    }

    #[test]
    fn buffer_size_calculation() {
        // 4096 header + 2 * N buffers
        assert_eq!(ArenaHeader::buffer_size(4096 + 4096 * 2), 4096);
        assert_eq!(ArenaHeader::buffer_size(4096 + 65536 * 2), 65536);
        assert_eq!(ArenaHeader::buffer_size(4096 + 1024 * 2), 1024);
    }

    #[test]
    fn active_buffer_offset_valid_header() {
        let h = ArenaHeader {
            magic: ArenaHeader::MAGIC,
            version: ArenaHeader::VERSION,
            active_buffer: 0,
            padding: [0; 2],
            sequence: 5,
        };
        let file_size = 4096 + 4096 * 2;
        // Buffer 0 starts right after the header.
        assert_eq!(h.active_buffer_offset(file_size), Some(4096));

        let h1 = ArenaHeader {
            active_buffer: 1,
            ..h
        };
        // Buffer 1 starts after header + buffer 0.
        assert_eq!(h1.active_buffer_offset(file_size), Some(4096 + 4096));
    }

    #[test]
    fn active_buffer_offset_rejects_bad_header() {
        let base = ArenaHeader {
            magic: ArenaHeader::MAGIC,
            version: ArenaHeader::VERSION,
            active_buffer: 0,
            padding: [0; 2],
            sequence: 0,
        };
        let file_size = 4096 + 4096 * 2;

        // Bad magic
        let bad_magic = ArenaHeader {
            magic: 0xDEADBEEF,
            ..base
        };
        assert_eq!(bad_magic.active_buffer_offset(file_size), None);

        // Bad version
        let bad_ver = ArenaHeader {
            version: 99,
            ..base
        };
        assert_eq!(bad_ver.active_buffer_offset(file_size), None);

        // Bad active_buffer (must be 0 or 1)
        let bad_buf = ArenaHeader {
            active_buffer: 2,
            ..base
        };
        assert_eq!(bad_buf.active_buffer_offset(file_size), None);
    }

    #[test]
    fn create_arena_is_idempotent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("idem.arena");
        let arena_size: u64 = 4096 + 4096 * 2;

        // First create — initializes header.
        let mut mmap = create_arena(&path, arena_size).unwrap();
        write_to_arena(&mut mmap, b"data-v1").unwrap();
        drop(mmap);

        // Second create — must NOT clobber existing data.
        let mmap = create_arena(&path, arena_size).unwrap();
        let h: ArenaHeader = *bytemuck::from_bytes(&mmap[..std::mem::size_of::<ArenaHeader>()]);
        assert_eq!(h.magic, ArenaHeader::MAGIC);
        assert_eq!(h.sequence, 1, "existing arena should preserve sequence");
        assert_eq!(
            h.active_buffer, 1,
            "existing arena should preserve active buffer"
        );
    }

    #[test]
    fn read_active_buffer_after_write() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("readback.arena");
        let arena_size: u64 = 4096 + 4096 * 2;

        let mut mmap = create_arena(&path, arena_size).unwrap();
        write_to_arena(&mut mmap, b"sqlite-bytes-here").unwrap();

        // Read back the active buffer using the header.
        let h: ArenaHeader = *bytemuck::from_bytes(&mmap[..std::mem::size_of::<ArenaHeader>()]);
        let offset = h.active_buffer_offset(arena_size).unwrap() as usize;
        let buf_size = ArenaHeader::buffer_size(arena_size) as usize;
        let active = &mmap[offset..offset + buf_size];

        assert_eq!(&active[..17], b"sqlite-bytes-here");
        // Rest is zero-padded.
        assert!(active[17..].iter().all(|&b| b == 0));
    }
}
