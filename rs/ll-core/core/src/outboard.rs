//! Outboard BLAKE3 tree: incremental root maintenance + per-chunk proofs
//! (bead `ley-line-open-b6a4dd` — the content-address half of the sub-file
//! premise; the chunk-storage half shipped as `ley-line-open-f8ebe7`).
//!
//! # Why
//!
//! `current_root = BLAKE3(payload)` is the arena's identity, and today an
//! edit pays a full re-hash to advance it — O(file) at the content-address
//! layer even after the CDC layer learned to hash only its rescan window.
//! BLAKE3 is internally a Merkle tree over fixed 1 KiB chunks, so the root
//! is maintainable incrementally: re-hash only the chunks an edit dirtied,
//! then re-merge parents (64-byte compressions, ~1000× cheaper per byte
//! than hashing). The same tree yields per-chunk inclusion proofs — the
//! primitive per-page verify-on-fault needs (the fs-verity move: verify
//! each page as it is demand-loaded, against the root alone).
//!
//! # The load-bearing identity
//!
//! **`Outboard::root()` is bit-identical to [`blake3::hash`] of the same
//! bytes.** This is the bead's acceptance criterion and the module's whole
//! value: the incremental root IS the arena identity, not a parallel
//! scheme. The `hazmat` docs prescribe exactly this falsifier ("the best
//! way to catch mistakes is to compare your root output to `blake3::hash`
//! of the same input"), and `root_is_bit_identical_to_blake3_hash_at_every
//! _tree_shape` runs it across every boundary shape the tree has.
//!
//! Built on `blake3::hazmat` — the crate's own subtree API — so the chunk
//! chaining values and parent merges are the reference implementation's,
//! not a reimplementation. Tree shape follows the BLAKE3 spec via
//! [`blake3::hazmat::left_subtree_len`].
//!
//! # What this module deliberately is not (yet)
//!
//! - Internal parent nodes are re-merged on every [`Outboard::root`] call
//!   rather than cached: O(chunks/1024) 64-byte compressions per root —
//!   microseconds at arena scale. The BYTES-hashed cost (the expensive
//!   part) is O(dirty) via [`Outboard::update`]; caching interior nodes
//!   for O(log n) merges is a later optimization with the same API.
//! - Mount-path verify-on-fault wiring is the bead's remaining half; this
//!   is the primitive it consumes.

use anyhow::{Result, ensure};
use blake3::CHUNK_LEN;
use blake3::hazmat::{self, ChainingValue, HasherExt, Mode};

use crate::substrate::Hash;

/// One step of an inclusion proof: the sibling subtree's chaining value and
/// which side of the merge it sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProofStep {
    /// Chaining value of the sibling subtree at this level.
    pub sibling: ChainingValue,
    /// True when the sibling is the LEFT input of the merge (i.e. the
    /// proven chunk lives in the right subtree at this level).
    pub sibling_is_left: bool,
}

/// Maintained leaf layer of the BLAKE3 tree over one buffer.
///
/// Chunks are BLAKE3's own fixed 1 KiB input chunks — NOT the CDC layer's
/// content-defined chunks. The two decompositions serve different layers:
/// CDC chunks dedupe storage; these chunks are the hash tree's leaves.
#[derive(Debug, Clone)]
pub struct Outboard {
    /// Non-root chaining value per 1 KiB chunk. Empty only for `len == 0`.
    leaf_cvs: Vec<ChainingValue>,
    /// Total buffer length in bytes.
    len: usize,
    /// Root for the degenerate `<= 1` chunk tree, where the single chunk
    /// is finalized with the root flag and no parent merge exists. Kept
    /// current by [`Outboard::build`] / [`Outboard::update`].
    small_root: Option<Hash>,
}

/// Non-root chaining value of the 1 KiB chunk at `index` covering `bytes`.
fn leaf_cv(index: usize, bytes: &[u8]) -> ChainingValue {
    let mut hasher = blake3::Hasher::new();
    hasher.set_input_offset((index * CHUNK_LEN) as u64);
    hasher.update(bytes);
    hasher.finalize_non_root()
}

/// Chaining value of the subtree whose leaves are `cvs`, spanning
/// `byte_len` bytes. Splits per the BLAKE3 spec (left subtree is the
/// largest power-of-two chunk count strictly inside the input).
fn subtree_cv(cvs: &[ChainingValue], byte_len: usize) -> ChainingValue {
    if cvs.len() == 1 {
        return cvs[0];
    }
    let left_bytes = hazmat::left_subtree_len(byte_len as u64) as usize;
    let left_chunks = left_bytes / CHUNK_LEN;
    hazmat::merge_subtrees_non_root(
        &subtree_cv(&cvs[..left_chunks], left_bytes),
        &subtree_cv(&cvs[left_chunks..], byte_len - left_bytes),
        Mode::Hash,
    )
}

impl Outboard {
    /// Build the leaf layer from the full buffer. O(len) hashing — the
    /// one-time cost [`Outboard::update`] amortizes away thereafter.
    pub fn build(data: &[u8]) -> Self {
        let mut out = Self {
            leaf_cvs: Vec::with_capacity(data.len().div_ceil(CHUNK_LEN)),
            len: data.len(),
            small_root: None,
        };
        for (index, chunk) in data.chunks(CHUNK_LEN).enumerate() {
            out.leaf_cvs.push(leaf_cv(index, chunk));
        }
        out.refresh_small_root(data);
        out
    }

    fn refresh_small_root(&mut self, data: &[u8]) {
        self.small_root = if self.leaf_cvs.len() <= 1 {
            Some(Hash::from_bytes(*blake3::hash(data).as_bytes()))
        } else {
            None
        };
    }

    /// Re-hash exactly the chunks an in-place edit dirtied, returning how
    /// many were re-hashed — callers assert on the work, not the outcome
    /// (the CDC layer's `RechunkStats` lesson).
    ///
    /// `data` is the FULL post-edit buffer; `dirty` is the byte range the
    /// edit touched. If the buffer's length changed, every chunk from the
    /// start of `dirty` onward is re-derived (a length change shifts all
    /// downstream bytes relative to the fixed 1 KiB chunk grid, unlike the
    /// CDC layer's content-defined boundaries). The arena's page-write
    /// case — fixed-size in-place writes — stays O(dirty).
    pub fn update(&mut self, data: &[u8], dirty: std::ops::Range<usize>) -> Result<usize> {
        ensure!(
            dirty.start <= dirty.end && dirty.end <= data.len(),
            "dirty range {dirty:?} does not lie within the {}-byte buffer",
            data.len()
        );
        let first_chunk = dirty.start / CHUNK_LEN;
        let n_chunks = data.len().div_ceil(CHUNK_LEN);

        let rehashed = if data.len() == self.len {
            // In-place edit: splice recomputed CVs over exactly the chunks
            // the dirty range overlaps; the untouched tail keeps its CVs.
            let last_chunk_exclusive = if dirty.is_empty() {
                first_chunk
            } else {
                ((dirty.end - 1) / CHUNK_LEN + 1).min(n_chunks)
            };
            for index in first_chunk..last_chunk_exclusive {
                let start = index * CHUNK_LEN;
                let end = (start + CHUNK_LEN).min(data.len());
                self.leaf_cvs[index] = leaf_cv(index, &data[start..end]);
            }
            last_chunk_exclusive.saturating_sub(first_chunk)
        } else {
            // Length changed: the fixed 1 KiB grid past the edit start no
            // longer matches the stored leaves — re-derive to the end.
            let first = first_chunk.min(n_chunks);
            self.leaf_cvs.truncate(first);
            for index in first..n_chunks {
                let start = index * CHUNK_LEN;
                let end = (start + CHUNK_LEN).min(data.len());
                self.leaf_cvs.push(leaf_cv(index, &data[start..end]));
            }
            n_chunks - first
        };
        self.len = data.len();
        self.refresh_small_root(data);
        Ok(rehashed)
    }

    /// Total buffer length this outboard describes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True for a zero-length buffer.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The BLAKE3 root — **bit-identical to `blake3::hash` of the buffer**.
    pub fn root(&self) -> Hash {
        if let Some(small) = self.small_root {
            return small;
        }
        let left_bytes = hazmat::left_subtree_len(self.len as u64) as usize;
        let left_chunks = left_bytes / CHUNK_LEN;
        let root = hazmat::merge_subtrees_root(
            &subtree_cv(&self.leaf_cvs[..left_chunks], left_bytes),
            &subtree_cv(&self.leaf_cvs[left_chunks..], self.len - left_bytes),
            Mode::Hash,
        );
        Hash::from_bytes(*root.as_bytes())
    }

    /// Inclusion proof for the 1 KiB chunk at `index`: sibling chaining
    /// values from the leaf's level up to (and including) the root merge.
    /// Empty for a `<= 1` chunk tree, where the root IS the chunk hash.
    pub fn prove(&self, index: usize) -> Result<Vec<ProofStep>> {
        // An empty buffer still has a well-defined "chunk 0": the empty
        // chunk whose direct hash IS the root, proof-free.
        ensure!(
            index < self.leaf_cvs.len().max(1),
            "chunk index {index} out of range for {} chunks",
            self.leaf_cvs.len()
        );
        let mut steps = Vec::new();
        if self.leaf_cvs.len() <= 1 {
            return Ok(steps);
        }
        collect_proof(&self.leaf_cvs, self.len, index, &mut steps);
        // Collected root-down; verification folds leaf-up.
        steps.reverse();
        Ok(steps)
    }
}

/// Walk from the top of the subtree down to `index`, recording the sibling
/// at each level (top-first; caller reverses for leaf-up folding).
fn collect_proof(cvs: &[ChainingValue], byte_len: usize, index: usize, out: &mut Vec<ProofStep>) {
    if cvs.len() == 1 {
        return;
    }
    let left_bytes = hazmat::left_subtree_len(byte_len as u64) as usize;
    let left_chunks = left_bytes / CHUNK_LEN;
    if index < left_chunks {
        out.push(ProofStep {
            sibling: subtree_cv(&cvs[left_chunks..], byte_len - left_bytes),
            sibling_is_left: false,
        });
        collect_proof(&cvs[..left_chunks], left_bytes, index, out);
    } else {
        out.push(ProofStep {
            sibling: subtree_cv(&cvs[..left_chunks], left_bytes),
            sibling_is_left: true,
        });
        collect_proof(
            &cvs[left_chunks..],
            byte_len - left_bytes,
            index - left_chunks,
            out,
        );
    }
}

/// Verify that `chunk_bytes` is the 1 KiB chunk at `index` of the buffer
/// whose BLAKE3 root is `root`, using an inclusion proof from
/// [`Outboard::prove`]. This is the verify-on-fault primitive: no other
/// part of the buffer is needed.
///
/// `total_len` is the full buffer length the root commits to — it decides
/// whether the chunk is the tree's single (root-flagged) chunk and lets
/// short final chunks verify at the correct length.
pub fn verify_chunk(
    root: Hash,
    total_len: usize,
    index: usize,
    chunk_bytes: &[u8],
    proof: &[ProofStep],
) -> Result<()> {
    let n_chunks = total_len.div_ceil(CHUNK_LEN).max(1);
    ensure!(index < n_chunks, "chunk index out of range");
    let expected_len = if index + 1 == n_chunks {
        total_len - index * CHUNK_LEN
    } else {
        CHUNK_LEN
    };
    ensure!(
        chunk_bytes.len() == expected_len,
        "chunk {index} must be {expected_len} bytes for a {total_len}-byte buffer, got {}",
        chunk_bytes.len()
    );

    if n_chunks == 1 {
        ensure!(proof.is_empty(), "single-chunk tree takes no proof");
        let got = Hash::from_bytes(*blake3::hash(chunk_bytes).as_bytes());
        ensure!(got == root, "chunk hash does not match the root");
        return Ok(());
    }

    ensure!(!proof.is_empty(), "multi-chunk tree requires a proof");
    let mut cv = leaf_cv(index, chunk_bytes);
    for (level, step) in proof.iter().enumerate() {
        let is_last = level + 1 == proof.len();
        let (left, right) = if step.sibling_is_left {
            (&step.sibling, &cv)
        } else {
            (&cv, &step.sibling)
        };
        if is_last {
            let got = hazmat::merge_subtrees_root(left, right, Mode::Hash);
            ensure!(
                Hash::from_bytes(*got.as_bytes()) == root,
                "proof does not fold to the root"
            );
            return Ok(());
        }
        cv = hazmat::merge_subtrees_non_root(left, right, Mode::Hash);
    }
    unreachable!("loop returns on the last step, which the ensure above guarantees exists");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic bytes; no RNG dependency.
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

    fn blake3_ref(data: &[u8]) -> Hash {
        Hash::from_bytes(*blake3::hash(data).as_bytes())
    }

    /// THE acceptance criterion (b6a4dd): the incremental root is the
    /// arena identity, bit for bit, at every tree shape — empty, single
    /// partial/full chunk, every boundary around each power-of-two chunk
    /// count, and odd multi-level shapes.
    #[test]
    fn root_is_bit_identical_to_blake3_hash_at_every_tree_shape() {
        let mut sizes = vec![0, 1, 2, 1023, 1024, 1025];
        for pow in 1..=6 {
            let chunks = 1usize << pow;
            for delta in [-1i64, 0, 1] {
                let n = (chunks * CHUNK_LEN) as i64 + delta;
                sizes.push(usize::try_from(n).unwrap());
            }
            // Odd (non-power-of-two) chunk counts exercise the uneven split.
            sizes.push((chunks + 1) * CHUNK_LEN + 7);
        }
        for size in sizes {
            let data = prng_bytes(size as u64 + 1, size);
            assert_eq!(
                Outboard::build(&data).root(),
                blake3_ref(&data),
                "root diverged from blake3::hash at size {size}"
            );
        }
    }

    /// In-place edits: the root tracks blake3::hash exactly, and the work
    /// is exactly the dirtied chunk span — asserted on the count, not a
    /// threshold (the RechunkStats discipline).
    #[test]
    fn update_tracks_blake3_and_rehashes_exactly_the_dirty_chunks() {
        let size = 37 * CHUNK_LEN + 511;
        let mut data = prng_bytes(0xB6A4, size);
        let mut ob = Outboard::build(&data);

        let mut s = 0xDD00u64;
        let mut rng = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        for _ in 0..50 {
            let start = (rng() as usize) % size;
            let len = ((rng() as usize) % 4096).min(size - start);
            for (i, b) in data[start..start + len].iter_mut().enumerate() {
                *b ^= (rng() >> (i % 8)) as u8 | 1;
            }
            let rehashed = ob.update(&data, start..start + len).unwrap();
            let expected = if len == 0 {
                0
            } else {
                (start + len - 1) / CHUNK_LEN + 1 - start / CHUNK_LEN
            };
            assert_eq!(rehashed, expected, "work must equal the dirty chunk span");
            assert_eq!(ob.root(), blake3_ref(&data), "root diverged after edit");
        }
    }

    /// Length-changing updates re-derive from the edit start onward and
    /// still land on the reference hash — append, truncate, and cross the
    /// single-chunk/multi-chunk boundary in both directions.
    #[test]
    fn update_handles_length_changes_including_the_small_tree_boundary() {
        let cases: &[(usize, usize)] = &[
            (0, 1),
            (1, 0),
            (100, 1024),
            (1024, 1025),
            (1025, 1024),
            (3 * CHUNK_LEN, 5 * CHUNK_LEN + 9),
            (5 * CHUNK_LEN + 9, 2 * CHUNK_LEN),
        ];
        for &(from, to) in cases {
            let old = prng_bytes(7 + from as u64, from);
            let mut ob = Outboard::build(&old);
            let new = prng_bytes(11 + to as u64, to);
            let dirty_start = old
                .iter()
                .zip(new.iter())
                .position(|(a, b)| a != b)
                .unwrap_or(old.len().min(new.len()));
            ob.update(&new, dirty_start..new.len()).unwrap();
            assert_eq!(
                ob.root(),
                blake3_ref(&new),
                "root diverged after {from} -> {to} byte resize"
            );
        }
    }

    /// The verify-on-fault primitive: every chunk of a multi-level tree
    /// verifies against the root alone; a flipped byte, a wrong index, and
    /// a truncated proof are all refused.
    #[test]
    fn every_chunk_proves_against_the_root_and_corruption_is_refused() {
        let size = 13 * CHUNK_LEN + 300;
        let data = prng_bytes(0xFA017, size);
        let ob = Outboard::build(&data);
        let root = ob.root();
        let n_chunks = size.div_ceil(CHUNK_LEN);

        for index in 0..n_chunks {
            let start = index * CHUNK_LEN;
            let end = (start + CHUNK_LEN).min(size);
            let proof = ob.prove(index).unwrap();
            verify_chunk(root, size, index, &data[start..end], &proof)
                .unwrap_or_else(|e| panic!("chunk {index} must verify: {e:#}"));

            let mut corrupt = data[start..end].to_vec();
            corrupt[0] ^= 0x01;
            assert!(
                verify_chunk(root, size, index, &corrupt, &proof).is_err(),
                "corrupt chunk {index} must be refused"
            );
        }

        // Wrong index with the right bytes: the offset is part of the CV.
        let proof0 = ob.prove(0).unwrap();
        assert!(
            verify_chunk(root, size, 1, &data[..CHUNK_LEN], &proof0).is_err(),
            "a chunk presented at the wrong index must be refused"
        );

        // Truncated proof never folds to the root.
        let proof3 = ob.prove(3).unwrap();
        assert!(
            verify_chunk(
                root,
                size,
                3,
                &data[3 * CHUNK_LEN..4 * CHUNK_LEN],
                &proof3[..proof3.len() - 1]
            )
            .is_err(),
            "a truncated proof must be refused"
        );
    }

    /// Single-chunk and empty trees: proofs are empty, verification is the
    /// direct hash, and mismatched shapes are refused.
    #[test]
    fn small_trees_verify_directly_with_empty_proofs() {
        for size in [0usize, 1, 512, 1024] {
            let data = prng_bytes(size as u64 + 99, size);
            let ob = Outboard::build(&data);
            let root = ob.root();
            let proof = ob.prove(0).unwrap();
            assert!(proof.is_empty());
            verify_chunk(root, size, 0, &data, &proof).unwrap();
        }
        // A proof where none belongs is refused rather than ignored.
        let data = prng_bytes(5, 100);
        let ob = Outboard::build(&data);
        let bogus = [ProofStep {
            sibling: [0u8; 32],
            sibling_is_left: false,
        }];
        assert!(verify_chunk(ob.root(), 100, 0, &data, &bogus).is_err());
    }
}
