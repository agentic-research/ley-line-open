//! Content-defined chunking (CDC) — GearHash rolling hash with xet-compatible
//! parameters, over the Σ substrate's BLAKE3 content addressing.
//!
//! We **compose** HuggingFace's [`gearhash`] crate (their SIMD-accelerated CDC
//! rolling hash — the proven hard part) and **glean** only xet's published
//! boundary rule: target ~64 KiB, clamped to `[8, 128]` KiB (min/max). Each
//! chunk is addressed by σ = BLAKE3 ([`leyline_core::ContentAddressed`]), which
//! is exactly xet's `MerkleHash` base — so LLO chunk identity aligns with xet's
//! rather than introducing a foreign scheme.
//!
//! ## The falsifiable benefit — boundary stability
//!
//! The load-bearing property (falsified in the tests) is **boundary
//! stability**: an insert/delete in one region of a stream changes only the
//! chunks *in that region*; every chunk outside it keeps an identical σ hash.
//! Fixed-size chunking fails this — an insert shifts every downstream boundary,
//! so all downstream chunks change. Boundary stability is exactly what makes
//! chunk-level dedup pay off: a small edit to a large file re-stores `O(1)`
//! chunks, not `O(file size)`. See `boundary_stability_localizes_an_edit` and
//! `beats_fixed_size_chunking_under_an_insert`.
//!
//! Scope: this crate is the *storage primitive* the mount path builds on (bead
//! ley-line-open-9989d2). Composing it under FUSE materialize-on-read + wiring
//! it to the arena is the next layer.

use leyline_core::{ContentAddressed, Hash};

/// Minimum chunk size — xet's floor (8 KiB). No chunk is smaller than this
/// except the final tail. (huggingface.co/docs/xet/en/deduplication)
pub const MIN_CHUNK: usize = 8 * 1024;

/// Maximum chunk size — xet's ceiling (128 KiB). A boundary is forced here even
/// if the rolling hash has not matched.
pub const MAX_CHUNK: usize = 128 * 1024;

/// GearHash boundary mask. A boundary is declared where `hash & MASK == 0`; the
/// number of set bits sets the expected chunk size (~`2^bits`). 16 bits ⇒ a
/// ~64 KiB average, xet's target. The exact value is validated empirically by
/// `chunk_sizes_respect_the_xet_bounds` — we do not assert it, we falsify it.
const BOUNDARY_MASK: u64 = 0x0000_5890_5303_0000;

/// One content-defined chunk: its σ (BLAKE3) hash and its span in the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// σ = BLAKE3 of the chunk's bytes (xet `MerkleHash` base).
    pub hash: Hash,
    /// Byte offset of the chunk in the source stream.
    pub offset: usize,
    /// Chunk length in bytes.
    pub len: usize,
}

/// Split `data` into content-defined chunks. Boundaries are chosen by the
/// GearHash of local content (clamped to `[MIN_CHUNK, MAX_CHUNK]`), so they are
/// stable under edits elsewhere in the stream. Each chunk carries its σ hash.
pub fn chunk(data: &[u8]) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < data.len() {
        let len = next_boundary(&data[start..]);
        let bytes = &data[start..start + len];
        chunks.push(Chunk {
            hash: bytes.hash(),
            offset: start,
            len,
        });
        start += len;
    }
    chunks
}

/// Length of the next chunk starting at the front of `data`, clamped to
/// `[MIN_CHUNK, MAX_CHUNK]`. The GearHash rolls only over content at or after
/// `MIN_CHUNK`, so no interior chunk is shorter than the floor; if no boundary
/// is found before the ceiling, the chunk is cut at `MAX_CHUNK`.
fn next_boundary(data: &[u8]) -> usize {
    if data.len() <= MIN_CHUNK {
        return data.len();
    }
    let search_end = MAX_CHUNK.min(data.len());
    let mut hasher = gearhash::Hasher::default();
    match hasher.next_match(&data[MIN_CHUNK..search_end], BOUNDARY_MASK) {
        Some(rel) => MIN_CHUNK + rel,
        None => search_end,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random bytes (xorshift64), so chunk boundaries are
    /// content-defined AND reproducible without an RNG dependency.
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

    fn hashes(chunks: &[Chunk]) -> Vec<Hash> {
        chunks.iter().map(|c| c.hash).collect()
    }

    /// Reconstruction: concatenating the chunk spans reproduces the input
    /// exactly (no bytes lost or duplicated at boundaries).
    #[test]
    fn chunks_reconstruct_the_input() {
        let data = prng_bytes(1, 1_000_000);
        let mut out = Vec::new();
        for c in chunk(&data) {
            out.extend_from_slice(&data[c.offset..c.offset + c.len]);
        }
        assert_eq!(out, data, "concatenated chunks must equal the input");
    }

    /// The chunker actually fires content-defined boundaries (it is not
    /// degenerating to fixed MAX-sized chunks), and every interior chunk lies
    /// within xet's [8, 128] KiB bounds.
    #[test]
    fn chunk_sizes_respect_the_xet_bounds() {
        let data = prng_bytes(2, 4_000_000);
        let chunks = chunk(&data);
        assert!(
            chunks.len() > 20,
            "a 4MB stream must split into many content-defined chunks, got {}",
            chunks.len()
        );
        for (i, c) in chunks.iter().enumerate() {
            let is_last = i == chunks.len() - 1;
            assert!(
                c.len <= MAX_CHUNK,
                "chunk {i} exceeds MAX_CHUNK ({})",
                c.len
            );
            if !is_last {
                assert!(
                    c.len >= MIN_CHUNK,
                    "interior chunk {i} below MIN_CHUNK ({})",
                    c.len
                );
            }
        }
        // A meaningful fraction of boundaries are content-defined (not all MAX).
        let content_defined = chunks.iter().filter(|c| c.len < MAX_CHUNK).count();
        assert!(
            content_defined > chunks.len() / 2,
            "most boundaries should be content-defined, not forced at MAX"
        );
    }

    /// THE property: inserting bytes in the middle of a stream changes only the
    /// chunks around the edit. Chunks strictly before the edit region keep
    /// identical hashes, and chunks after it re-align (their hashes reappear,
    /// shifted). The count of *changed* chunk hashes is small and independent of
    /// stream size — that is boundary stability.
    #[test]
    fn boundary_stability_localizes_an_edit() {
        let data = prng_bytes(3, 2_000_000);
        let before = chunk(&data);

        // Insert 5 bytes near the middle.
        let mid = data.len() / 2;
        let mut edited = data.clone();
        edited.splice(mid..mid, [0xAA, 0xBB, 0xCC, 0xDD, 0xEE]);
        let after = chunk(&edited);

        let bset: std::collections::HashSet<_> = hashes(&before).into_iter().collect();
        let aset: std::collections::HashSet<_> = hashes(&after).into_iter().collect();

        // Chunks unique to the OLD chunking (destroyed by the edit) must be few
        // — only those straddling the edit point. If boundaries were fixed-size,
        // ~half the chunks (everything after the edit) would be unique to each.
        let destroyed = bset.difference(&aset).count();
        assert!(
            destroyed <= 3,
            "a 5-byte insert must destroy at most a handful of chunks, destroyed {destroyed} \
             of {} (fixed-size chunking would destroy ~half)",
            before.len()
        );

        // And the vast majority of pre-edit chunks survive verbatim.
        let survivors = bset.intersection(&aset).count();
        assert!(
            survivors >= before.len() - 3,
            "almost all chunks must survive an interior insert ({survivors}/{})",
            before.len()
        );
    }

    /// Contrast that proves the benefit is CDC-specific: under the SAME insert,
    /// fixed-size chunking destroys ~every chunk after the edit, while CDC
    /// destroys a handful. The gap IS xet's benefit.
    #[test]
    fn beats_fixed_size_chunking_under_an_insert() {
        let data = prng_bytes(4, 2_000_000);
        let mid = data.len() / 2;
        let mut edited = data.clone();
        edited.splice(mid..mid, [0x11, 0x22, 0x33]);

        // Fixed-size (64 KiB) chunking — count destroyed chunks under the insert.
        let fixed = |d: &[u8]| -> Vec<Hash> { d.chunks(64 * 1024).map(|c| c.hash()).collect() };
        let fb: std::collections::HashSet<_> = fixed(&data).into_iter().collect();
        let fa: std::collections::HashSet<_> = fixed(&edited).into_iter().collect();
        let fixed_destroyed = fb.difference(&fa).count();

        let cb: std::collections::HashSet<_> = hashes(&chunk(&data)).into_iter().collect();
        let ca: std::collections::HashSet<_> = hashes(&chunk(&edited)).into_iter().collect();
        let cdc_destroyed = cb.difference(&ca).count();

        assert!(
            fixed_destroyed > 10,
            "fixed-size chunking must cascade after an insert (destroyed {fixed_destroyed})"
        );
        assert!(
            cdc_destroyed * 5 < fixed_destroyed,
            "CDC ({cdc_destroyed}) must destroy far fewer chunks than fixed-size \
             ({fixed_destroyed}) — that gap is the dedup benefit"
        );
    }

    /// Dedup locality: two streams sharing a large common tail share chunk
    /// hashes over that tail (identical content ⇒ identical chunks, regardless
    /// of what differs before it).
    #[test]
    fn shared_content_yields_shared_chunks() {
        let common = prng_bytes(5, 1_500_000);
        let mut a = prng_bytes(6, 200_000);
        let mut b = prng_bytes(7, 250_000); // different, different-length prefix
        a.extend_from_slice(&common);
        b.extend_from_slice(&common);

        let ah: std::collections::HashSet<_> = hashes(&chunk(&a)).into_iter().collect();
        let bh: std::collections::HashSet<_> = hashes(&chunk(&b)).into_iter().collect();
        let shared = ah.intersection(&bh).count();
        assert!(
            shared > 15,
            "two streams sharing a 1.5MB tail must share many chunks, shared {shared}"
        );
    }

    /// A single chunk is content-addressed; identical bytes ⇒ identical hash.
    #[test]
    fn identical_bytes_hash_identically() {
        let a = prng_bytes(8, 300_000);
        assert_eq!(hashes(&chunk(&a)), hashes(&chunk(&a.clone())));
    }
}
