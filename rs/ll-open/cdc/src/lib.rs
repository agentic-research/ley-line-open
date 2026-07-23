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

use anyhow::{Context, Result};
use leyline_core::{BlobStore, ContentAddressed, Hash};

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

// ── Restriction-addressed re-chunking (ADR-0031's shape, applied to bytes) ────

/// Re-chunk after an edit by rescanning only the affected interval — the
/// restriction the write actually observes — instead of the whole stream.
///
/// ## Why this is exact, not a heuristic
///
/// ADR-0031's result is that *restriction maps earned rent* while the
/// cohomology did not: cache a derived view on the **exact** hash of its input
/// closure, never on an approximate distance. The same shape applies here. A
/// chunk boundary is a *pure function of the bytes from that chunk's own start
/// forward*, bounded by `MAX_CHUNK` — [`next_boundary`] builds a fresh
/// `gearhash::Hasher` per chunk, so no state crosses a boundary. The input
/// closure of "where does chunk k end" is therefore the interval
/// `[start_k, start_k + MAX_CHUNK)`, and nothing else.
///
/// That gives two exact facts, no thresholds involved:
///
/// 1. **Prefix.** Every chunk whose decision window ends at or before the edit
///    (`offset + MAX_CHUNK <= edit_offset`) is bit-identical in the new
///    stream. Keep it.
/// 2. **Resync.** Once a new boundary lands at a position that, shifted back by
///    the length delta, is *also* an old boundary at or beyond the edit's end,
///    every remaining byte is unchanged — so the entire old tail is reusable,
///    offsets shifted. This is CDC's boundary stability stated as an identity
///    rather than an expectation.
///
/// Between those two points the bytes are rescanned normally. Work is
/// O(edit region + resync window), not O(stream).
///
/// **Not yet wired into a caller.** `leyline_fs::chunked::store_content_chunked`
/// still calls [`chunk`] for a full re-chunk on every write, so this function's
/// benefit is currently unrealized in the mount path — it is proven
/// (`fuzz_rechunk_equals_full_rechunk`) and measured
/// (`rechunk_work_is_sublinear_in_stream_length`) but unused outside tests.
/// Treat the complexity claim above as a property of THIS function, not a
/// description of what a write costs today.
///
/// `edit_offset..old_edit_end` is the replaced range **in old coordinates**;
/// `old_len` is the old stream's total length. The result is required to equal
/// `chunk(new_data)` exactly — `fuzz_rechunk_equals_full_rechunk` falsifies
/// that against random edits rather than trusting the argument above.
pub fn rechunk(
    old: &[Chunk],
    new_data: &[u8],
    edit_offset: usize,
    old_edit_end: usize,
    old_len: usize,
) -> Vec<Chunk> {
    rechunk_with_stats(old, new_data, edit_offset, old_edit_end, old_len).0
}

/// What an incremental re-chunk actually cost.
///
/// Exposed because every way this optimization can fail silently produces
/// **correct output**: if the prefix is not kept, or the resync never fires,
/// `rechunk` simply rescans more of the stream and still returns exactly
/// `chunk(new_data)`. Exactness tests are blind to that by construction —
/// mutation testing found four separate mutants that survived the entire
/// suite for precisely this reason. The only way to pin the optimization is
/// to assert on the work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RechunkStats {
    /// Chunks carried over untouched from the old manifest's head.
    pub prefix_kept: usize,
    /// Chunks carried over from the old manifest's tail after resync.
    pub tail_reused: usize,
    /// Chunks whose bytes were hashed again — the real cost.
    pub rehashed: usize,
    /// Bytes fed to the boundary scanner. This is the number the whole
    /// construction exists to keep sublinear in the stream length.
    pub bytes_scanned: usize,
}

/// [`rechunk`], reporting the work it did. See [`RechunkStats`].
pub fn rechunk_with_stats(
    old: &[Chunk],
    new_data: &[u8],
    edit_offset: usize,
    old_edit_end: usize,
    old_len: usize,
) -> (Vec<Chunk>, RechunkStats) {
    // A shrinking edit can move the tail before the region we would keep;
    // the resync check below handles growth and shrinkage alike, but the
    // delta must be computed against the true old length.
    let delta = new_data.len() as isize - old_len as isize;

    // (1) Prefix whose decision windows never touch the edit.
    let mut keep = 0usize;
    while keep < old.len() && old[keep].offset + MAX_CHUNK <= edit_offset {
        keep += 1;
    }
    let mut out: Vec<Chunk> = old[..keep].to_vec();
    let mut pos = out.last().map_or(0, |c| c.offset + c.len);
    let scan_start = pos;
    let mut rehashed = 0usize;

    while pos < new_data.len() {
        // (2) Resync: has the boundary sequence rejoined the old one, past
        // the edit? If so the remaining chunks are the old ones, shifted.
        let old_pos = pos as isize - delta;
        if old_pos >= old_edit_end as isize
            && let Ok(idx) = old.binary_search_by_key(&(old_pos as usize), |c| c.offset)
        {
            out.extend(old[idx..].iter().map(|c| Chunk {
                hash: c.hash,
                offset: (c.offset as isize + delta) as usize,
                len: c.len,
            }));
            let stats = RechunkStats {
                prefix_kept: keep,
                tail_reused: old.len() - idx,
                rehashed,
                bytes_scanned: pos - scan_start,
            };
            return (out, stats);
        }

        let len = next_boundary(&new_data[pos..]);
        let bytes = &new_data[pos..pos + len];
        out.push(Chunk {
            hash: bytes.hash(),
            offset: pos,
            len,
        });
        pos += len;
        rehashed += 1;
    }
    let stats = RechunkStats {
        prefix_kept: keep,
        tail_reused: 0,
        rehashed,
        bytes_scanned: pos - scan_start,
    };
    (out, stats)
}

// ── Materialize-on-read: store chunks, serve ranges from only the chunks ──────
//
// This is the layer the FUSE mount needs (bead 87bf00 next step): a mount
// serving a small read of a large file must fetch only the chunks covering that
// byte range, not the whole file. Mirrors xet's file-reconstruction (a manifest
// of chunk hashes + spans; reconstruct by fetching the relevant chunks from CAS
// and concatenating).

/// Chunk `data` and store each chunk's bytes in `store` (content-addressed),
/// returning the manifest that reconstructs it. Idempotent per chunk — identical
/// chunks across files/versions dedup in the store.
pub fn chunk_into<S: BlobStore>(data: &[u8], store: &mut S) -> Result<Vec<Chunk>> {
    let chunks = chunk(data);
    for c in &chunks {
        let stored = store
            .put(&data[c.offset..c.offset + c.len])
            .context("store chunk")?;
        debug_assert_eq!(
            stored, c.hash,
            "stored σ must equal the manifest chunk hash"
        );
    }
    Ok(chunks)
}

/// Reconstruct the byte range `[offset, offset+len)` from a chunk manifest,
/// fetching **only** the chunks that overlap the range. A read outside the file
/// yields the clamped overlap (possibly empty). Verify-on-read is the
/// `BlobStore` contract, so returned chunk bytes are σ-verified.
pub fn read_range<S: BlobStore>(
    chunks: &[Chunk],
    store: &S,
    offset: usize,
    len: usize,
) -> Result<Vec<u8>> {
    let end = offset.saturating_add(len);
    let mut out = Vec::with_capacity(len.min(1 << 20));
    for c in chunks {
        let c_end = c.offset + c.len;
        if c_end <= offset || c.offset >= end {
            continue; // no overlap — do NOT fetch this chunk
        }
        let bytes = store
            .get(c.hash)
            .context("get chunk")?
            .with_context(|| format!("chunk {:?} missing from store", c.hash))?;
        let lo = offset.saturating_sub(c.offset); // start within this chunk
        let hi = end.min(c_end) - c.offset; // end within this chunk
        out.extend_from_slice(&bytes[lo..hi]);
    }
    Ok(out)
}

/// Reconstruct the whole file from its chunk manifest.
///
/// Asks for an unbounded range rather than computing the file length from the
/// last chunk. Computing it would make the result depend on that arithmetic
/// being right *and* on `read_range` clamping it — and since `read_range`
/// clamps, an over-large total produces identical output, so no test could
/// catch the arithmetic going wrong. (Found by mutation testing: `c.offset +
/// c.len` → `c.offset * c.len` survived the whole suite.) `usize::MAX` states
/// "every chunk" directly; the per-chunk overlap check does the rest.
pub fn reconstruct<S: BlobStore>(chunks: &[Chunk], store: &S) -> Result<Vec<u8>> {
    read_range(chunks, store, 0, usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use leyline_core::MemBlobStore;
    use std::cell::Cell;

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

    // ── materialize-on-read falsifiers ───────────────────────────────────────

    /// A `BlobStore` that counts `get` calls, so a test can assert a range read
    /// fetches only the chunks it needs (not the whole file).
    struct CountingStore {
        inner: MemBlobStore,
        gets: Cell<usize>,
    }
    impl CountingStore {
        fn new() -> Self {
            Self {
                inner: MemBlobStore::new(),
                gets: Cell::new(0),
            }
        }
    }
    impl BlobStore for CountingStore {
        fn put(&mut self, bytes: &[u8]) -> Result<Hash> {
            self.inner.put(bytes)
        }
        fn get(&self, h: Hash) -> Result<Option<Vec<u8>>> {
            self.gets.set(self.gets.get() + 1);
            self.inner.get(h)
        }
        fn contains(&self, h: Hash) -> Result<bool> {
            self.inner.contains(h)
        }
    }

    /// Store → full reconstruct round-trips byte-for-byte.
    #[test]
    fn full_reconstruct_equals_input() {
        let data = prng_bytes(10, 3_000_000);
        let mut store = CountingStore::new();
        let manifest = chunk_into(&data, &mut store).unwrap();
        assert_eq!(reconstruct(&manifest, &store).unwrap(), data);
    }

    /// A range read returns exactly `data[offset..offset+len]`, for ranges that
    /// straddle boundaries, sit inside one chunk, and clamp past EOF.
    #[test]
    fn range_read_returns_the_correct_subbytes() {
        let data = prng_bytes(11, 3_000_000);
        let mut store = CountingStore::new();
        let manifest = chunk_into(&data, &mut store).unwrap();

        for &(off, len) in &[
            (0usize, 100usize),
            (1_000_000, 500_000), // straddles many chunks
            (data.len() - 10, 10),
            (data.len() - 5, 1000), // clamps past EOF
            (123_456, 4096),
        ] {
            let got = read_range(&manifest, &store, off, len).unwrap();
            let end = (off + len).min(data.len());
            assert_eq!(got, &data[off..end], "range ({off},{len}) mismatch");
        }
    }

    /// THE materialize-on-read property: a small read of a large file touches
    /// only the chunks overlapping the requested range — NOT the whole file.
    /// This is what lets the FUSE mount serve a 4 KiB read of a 100 MB file
    /// without fetching 100 MB.
    #[test]
    fn range_read_fetches_only_overlapping_chunks() {
        let data = prng_bytes(12, 8_000_000); // ~120+ chunks at ~64 KiB
        let mut store = CountingStore::new();
        let manifest = chunk_into(&data, &mut store).unwrap();
        assert!(manifest.len() > 50, "need a many-chunk file for this test");

        // A 4 KiB read in the middle overlaps at most 2 chunks (it can straddle
        // one boundary). Count the store gets it triggers.
        store.gets.set(0);
        let mid = data.len() / 2;
        let got = read_range(&manifest, &store, mid, 4096).unwrap();
        assert_eq!(got, &data[mid..mid + 4096]);

        let fetched = store.gets.get();
        assert!(
            fetched <= 2,
            "a 4KiB read must fetch <=2 chunks, fetched {fetched} of {} — \
             materialize-on-read must not touch the whole file",
            manifest.len()
        );
    }

    /// The whole claim, stated as an identity: an incremental re-chunk must be
    /// *bit-identical* to a full re-chunk of the same bytes. Not "close", not
    /// "usually" — equal. If it ever differs, a manifest built incrementally
    /// describes different chunks than one built from scratch, and two writers
    /// of the same content would disagree about its chunk identity.
    #[test]
    fn fuzz_rechunk_equals_full_rechunk() {
        const SEED: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut st = SEED;
        let mut rng = move || {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            st
        };

        for case in 0..200u64 {
            let old_len = match case % 4 {
                0 => (rng() % 40_000) as usize,
                1 => (rng() % 300_000) as usize,
                _ => (rng() % 2_000_000) as usize,
            };
            let old_data: Vec<u8> = (0..old_len).map(|_| (rng() >> 24) as u8).collect();
            let old_chunks = chunk(&old_data);

            let edit_offset = if old_len == 0 {
                0
            } else {
                (rng() as usize) % (old_len + 1)
            };
            let max_span = old_len - edit_offset;
            let old_edit_end = edit_offset
                + if max_span == 0 {
                    0
                } else {
                    (rng() as usize) % (max_span + 1)
                };
            let ins_len = match case % 3 {
                0 => 0,
                1 => (rng() % 50) as usize,
                _ => (rng() % 200_000) as usize,
            };
            let insert: Vec<u8> = (0..ins_len).map(|_| (rng() >> 24) as u8).collect();

            let mut new_data = Vec::with_capacity(old_len);
            new_data.extend_from_slice(&old_data[..edit_offset]);
            new_data.extend_from_slice(&insert);
            new_data.extend_from_slice(&old_data[old_edit_end..]);

            let (incremental, stats) =
                rechunk_with_stats(&old_chunks, &new_data, edit_offset, old_edit_end, old_len);
            let full = chunk(&new_data);

            assert_eq!(
                incremental, full,
                "case {case} (seed {SEED:#x}): incremental diverged \
                 (old_len {old_len}, edit {edit_offset}..{old_edit_end}, ins {ins_len})"
            );

            // Stats must exactly account for the output. Every chunk is either
            // kept from the head, reused from the tail, or rehashed — no
            // double-counting, nothing unaccounted. Asserting this on every
            // case is what makes the counters trustworthy enough to gate the
            // performance claims on; threshold assertions alone let arithmetic
            // slips in the bookkeeping survive (mutation-verified).
            assert_eq!(
                stats.prefix_kept + stats.tail_reused + stats.rehashed,
                incremental.len(),
                "case {case}: stats do not account for the output: {stats:?}"
            );
            // bytes_scanned must equal the span the scanner actually walked:
            // the rehashed chunks, contiguous from the end of the kept prefix.
            let scanned: usize = incremental[stats.prefix_kept..stats.prefix_kept + stats.rehashed]
                .iter()
                .map(|c| c.len)
                .sum();
            assert_eq!(
                stats.bytes_scanned, scanned,
                "case {case}: bytes_scanned disagrees with the rehashed span: {stats:?}"
            );
        }
    }

    /// Degenerate shapes, enumerated rather than sampled — the fuzzer's random
    /// offsets hit these only by luck.
    #[test]
    fn rechunk_handles_degenerate_edits() {
        let body: Vec<u8> = {
            let mut st = 7u64;
            (0..500_000)
                .map(|_| {
                    st ^= st << 13;
                    st ^= st >> 7;
                    st ^= st << 17;
                    (st >> 24) as u8
                })
                .collect()
        };

        for &old_len in &[
            0usize,
            1,
            MIN_CHUNK - 1,
            MIN_CHUNK,
            MIN_CHUNK + 1,
            MAX_CHUNK,
            500_000,
        ] {
            let old_data = &body[..old_len.min(body.len())];
            let old_chunks = chunk(old_data);
            let n = old_data.len();

            let cases: Vec<(usize, usize, usize)> = vec![
                (0, 0, 0),                   // no-op
                (0, 0, 10),                  // insert at start
                (0, n, 0),                   // delete everything
                (0, n, 32),                  // replace everything
                (n, n, 10),                  // append at EOF
                (n.saturating_sub(1), n, 0), // delete last byte
                (n / 2, n / 2, 3),           // small insert mid
                (n / 2, n, 0),               // truncate at mid
            ];

            for (eo, ee, ins) in cases {
                if eo > n || ee > n || eo > ee {
                    continue;
                }
                let mut new_data = Vec::new();
                new_data.extend_from_slice(&old_data[..eo]);
                new_data.extend(std::iter::repeat_n(0xABu8, ins));
                new_data.extend_from_slice(&old_data[ee..]);

                assert_eq!(
                    rechunk(&old_chunks, &new_data, eo, ee, n),
                    chunk(&new_data),
                    "old_len {old_len}, edit {eo}..{ee}, ins {ins}"
                );
            }
        }
    }

    /// Repetitive data produces a very different boundary density than random
    /// bytes — long constant runs suppress mask matches, pushing chunks to the
    /// MAX_CHUNK clamp, where the resync logic behaves differently.
    #[test]
    fn rechunk_exact_on_repetitive_data() {
        let mut body: Vec<u8> = Vec::new();
        body.extend(std::iter::repeat_n(0u8, 400_000)); // all MAX_CHUNK-clamped
        body.extend(std::iter::repeat_n(0xFFu8, 300_000));
        let block = body[..50_000].to_vec();
        body.extend_from_slice(&block); // exact repeat

        let old_chunks = chunk(&body);
        for &(eo, ee, ins) in &[
            (0usize, 0usize, 5usize),
            (200_000, 200_010, 0),
            (400_000, 400_000, 1),
            (650_000, 700_000, 90_000),
            (body.len(), body.len(), 7),
        ] {
            let mut new_data = Vec::new();
            new_data.extend_from_slice(&body[..eo]);
            new_data.extend(std::iter::repeat_n(0x5Au8, ins));
            new_data.extend_from_slice(&body[ee..]);
            assert_eq!(
                rechunk(&old_chunks, &new_data, eo, ee, body.len()),
                chunk(&new_data),
                "repetitive: edit {eo}..{ee}, ins {ins}"
            );
        }
    }

    /// The point of doing it incrementally: a small edit deep in a large stream
    /// must not rehash the whole thing.
    #[test]
    fn rechunk_reuses_almost_everything_for_a_small_deep_edit() {
        let mut st = 0xDEAD_BEEFu64;
        let old_data: Vec<u8> = (0..8_000_000)
            .map(|_| {
                st ^= st << 13;
                st ^= st >> 7;
                st ^= st << 17;
                (st >> 24) as u8
            })
            .collect();
        let old_chunks = chunk(&old_data);
        assert!(old_chunks.len() > 60, "need a many-chunk stream");

        let at = old_data.len() / 2;
        let mut new_data = old_data.clone();
        new_data.splice(at..at, [1u8, 2, 3]);

        let incremental = rechunk(&old_chunks, &new_data, at, at, old_data.len());
        assert_eq!(incremental, chunk(&new_data), "must still be exact");

        let old_hashes: std::collections::HashSet<_> = old_chunks.iter().map(|c| c.hash).collect();
        let rehashed = incremental
            .iter()
            .filter(|c| !old_hashes.contains(&c.hash))
            .count();
        assert!(
            rehashed <= 3,
            "a 3-byte edit should force rehashing <=3 of {} chunks, forced {rehashed}",
            incremental.len()
        );
    }

    /// Pins the WORK, not just the output. Every way the optimization can fail
    /// still yields correct bytes (it just rescans more), so exactness tests
    /// cannot see a broken prefix/resync — mutation testing proved that with
    /// four surviving mutants. These assertions are what kill them.
    #[test]
    fn rechunk_work_is_sublinear_in_stream_length() {
        let mut st = 0x1234_5678u64;
        let old_data: Vec<u8> = (0..8_000_000)
            .map(|_| {
                st ^= st << 13;
                st ^= st >> 7;
                st ^= st << 17;
                (st >> 24) as u8
            })
            .collect();
        let old_chunks = chunk(&old_data);

        let at = old_data.len() / 2;
        let mut new_data = old_data.clone();
        new_data.splice(at..at, [1u8, 2, 3]);

        let (out, stats) = rechunk_with_stats(&old_chunks, &new_data, at, at, old_data.len());
        assert_eq!(out, chunk(&new_data), "must remain exact");

        // The prefix must actually be kept: roughly half the chunks precede a
        // mid-stream edit. Zero here means the prefix logic is dead.
        assert!(
            stats.prefix_kept > old_chunks.len() / 4,
            "prefix not kept: {stats:?} of {} chunks",
            old_chunks.len()
        );
        // The tail must actually resync, or we rescanned to EOF.
        assert!(
            stats.tail_reused > old_chunks.len() / 4,
            "tail never resynced: {stats:?}"
        );
        // And the scan itself must be a small window, not the stream. Two
        // MAX_CHUNKs of slack covers the prefix-boundary-to-resync span.
        assert!(
            stats.bytes_scanned <= 4 * MAX_CHUNK,
            "scanned {} bytes of an 8MB stream: {stats:?}",
            stats.bytes_scanned
        );
        // Derived, not guessed: the rescan window is bounded by the slack
        // above, and no chunk inside it is shorter than MIN_CHUNK, so the
        // count cannot exceed window/MIN_CHUNK. (Measured here: 14 chunks over
        // ~138 KB — the window fills with near-minimum chunks, so a bound of
        // "a handful" would be wrong. An earlier hand-picked `<= 4` failed for
        // exactly that reason.)
        let max_rehashed = (4 * MAX_CHUNK).div_ceil(MIN_CHUNK);
        assert!(
            stats.rehashed >= 1,
            "a real edit must rehash at least one chunk: {stats:?}"
        );
        assert!(
            stats.rehashed <= max_rehashed,
            "rehashed {} chunks, bound is {max_rehashed}: {stats:?}",
            stats.rehashed
        );
    }

    /// An equal-length replacement (delta == 0) spanning a chunk boundary.
    ///
    /// This is the case that separates "resync past the edit" from "resync
    /// anywhere": with delta == 0 the scan's very first position is itself an
    /// old boundary, so a resync test that forgets to require *past the edit*
    /// splices the old tail straight over the edited region and returns
    /// pre-edit bytes. Exactness catches it only if the edit is non-empty and
    /// the lengths match — which no random case reliably produces.
    #[test]
    fn rechunk_equal_length_replacement_does_not_resync_inside_the_edit() {
        let mut st = 0xC0FF_EEu64;
        let old_data: Vec<u8> = (0..2_000_000)
            .map(|_| {
                st ^= st << 13;
                st ^= st >> 7;
                st ^= st << 17;
                (st >> 24) as u8
            })
            .collect();
        let old_chunks = chunk(&old_data);

        // Replace a 300KB span (several chunks) with the SAME number of
        // different bytes, so delta == 0.
        let (eo, ee) = (400_000usize, 700_000usize);
        let mut new_data = old_data.clone();
        for (i, b) in new_data[eo..ee].iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        assert_eq!(
            new_data.len(),
            old_data.len(),
            "delta must be 0 for this case"
        );

        let out = rechunk(&old_chunks, &new_data, eo, ee, old_data.len());
        assert_eq!(
            out,
            chunk(&new_data),
            "resynced inside the edited region — returned pre-edit bytes"
        );
    }
}
