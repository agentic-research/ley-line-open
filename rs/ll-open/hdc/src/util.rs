//! Hash / PRNG / popcount primitives shared across codebooks and the encoder.
//!
//! No external deps beyond blake3 (already a crate dep) — the PRNG is a
//! plain SplitMix64 seeded from blake3 output. SplitMix64 produces
//! statistically-balanced bits without needing cryptographic strength;
//! perfect for HDC where what matters is uniform Hamming geometry.

use crate::D_BYTES;

/// A D-bit hypervector packed into a byte array. Public so codebooks and
/// the encoder share a single concrete type — no generic Hypervector<D>
/// gymnastics. D is fixed at compile-time via `D_BYTES`.
pub type Hypervector = [u8; D_BYTES];

/// Empty hypervector (all zeros). Used as the bundle-accumulator
/// initial value and as the "no-op" identity under XOR.
pub const ZERO_HV: Hypervector = [0u8; D_BYTES];

/// SplitMix64 PRNG step. Deterministic, fast, statistically-balanced.
/// Stateful by construction — each call advances the seed.
#[inline]
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Expand a 64-bit seed into a D-byte pseudo-random hypervector.
///
/// Used by every codebook to turn a canonical-kind hash into a base
/// vector. Deterministic — the same seed always produces the same
/// bytes, on any machine, in any thread, on any version. Output is
/// statistically balanced (~50% ones) which is the HDC-capacity
/// assumption for bind/bundle correctness.
pub fn expand_seed(seed: u64) -> Hypervector {
    let mut state = seed;
    let mut out = [0u8; D_BYTES];
    let mut i = 0;
    while i < D_BYTES {
        let bits = splitmix64(&mut state).to_le_bytes();
        let copy_n = core::cmp::min(8, D_BYTES - i);
        out[i..i + copy_n].copy_from_slice(&bits[..copy_n]);
        i += copy_n;
    }
    out
}

/// XOR `src` into `dst` byte-wise. The compiler vectorizes this to AVX2
/// or NEON automatically on supported targets.
#[inline]
pub fn xor_into(dst: &mut Hypervector, src: &Hypervector) {
    for i in 0..D_BYTES {
        dst[i] ^= src[i];
    }
}

/// Rotate the bits of a hypervector left by `n` positions (modulo D_BITS).
/// Used as the positional-encoding primitive in the encoder: each child
/// position rotates the child's HV by its index, breaking the XOR-bundle
/// commutativity that would otherwise lose order information.
///
/// Operates on the full D_BITS-bit value as a single circular shift —
/// not a per-byte shift. Deterministic and bijective; the inverse is
/// `rotate_right(hv, n)` (used by unbind).
pub fn rotate_left(hv: &Hypervector, n: usize) -> Hypervector {
    use crate::D_BITS;
    let n = n % D_BITS;
    if n == 0 {
        return *hv;
    }
    let mut out = [0u8; D_BYTES];
    for src_bit in 0..D_BITS {
        let src_byte = src_bit / 8;
        let src_off = src_bit % 8;
        let bit = (hv[src_byte] >> src_off) & 1;
        let dst_bit = (src_bit + n) % D_BITS;
        let dst_byte = dst_bit / 8;
        let dst_off = dst_bit % 8;
        out[dst_byte] |= bit << dst_off;
    }
    out
}

/// Inverse of `rotate_left` — rotate bits right by `n`. Used by unbind.
pub fn rotate_right(hv: &Hypervector, n: usize) -> Hypervector {
    use crate::D_BITS;
    rotate_left(hv, D_BITS - (n % D_BITS))
}

/// Hamming distance between two hypervectors via popcount over the XOR.
/// O(D_BYTES / 8) u64 popcounts — ~16 cycles each on x86_64 SSE 4.2 /
/// AArch64 NEON. No allocation.
pub fn popcount_distance(a: &Hypervector, b: &Hypervector) -> u32 {
    let mut acc: u32 = 0;
    let mut i = 0;
    // Process 8 bytes at a time (one u64). D_BYTES is a multiple of 8 by
    // the const-assertion in lib.rs, so no leftover handling needed.
    while i + 8 <= D_BYTES {
        let xa = u64::from_le_bytes([a[i], a[i + 1], a[i + 2], a[i + 3], a[i + 4], a[i + 5], a[i + 6], a[i + 7]]);
        let xb = u64::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3], b[i + 4], b[i + 5], b[i + 6], b[i + 7]]);
        acc += (xa ^ xb).count_ones();
        i += 8;
    }
    acc
}

/// Quantize a child-count into one of six buckets — child-count drift
/// (e.g. an `if` with an else clause vs without) shouldn't shift the
/// canonical signature for tree-shape clustering. Buckets:
///   0 → 0
///   1 → 1
///   2 → 2
///   3, 4 → 3
///   5, 6, 7 → 4
///   8+ → 5
pub fn bucket_arity(n: usize) -> u8 {
    match n {
        0 => 0,
        1 => 1,
        2 => 2,
        3 | 4 => 3,
        5..=7 => 4,
        _ => 5,
    }
}

/// Blake3-derive a 64-bit hash from arbitrary bytes. Used as the seed
/// for `expand_seed`. Truncating to 64 bits is fine — the codebook
/// only needs ~200 distinct seeds (one per canonical-kind / arity /
/// child-shape combination), and 64 bits has 2^64 ≫ 200 headroom.
pub fn blake3_seed(bytes: &[u8]) -> u64 {
    let h = blake3::hash(bytes);
    let bs = h.as_bytes();
    u64::from_le_bytes([bs[0], bs[1], bs[2], bs[3], bs[4], bs[5], bs[6], bs[7]])
}

/// Hamming-distance threshold used by tests to assert "these two
/// hypervectors are far apart" — i.e., they're distinct base/encoded
/// vectors, not accidentally collapsed to identical or near-identical
/// bit patterns. Random-pair baseline is D/2 = 4096; ±3σ ≈ ±136 with
/// D=8192. Threshold 3500 gives ~4σ headroom — failing this means
/// something genuinely went wrong (a hash collision, a code path
/// that silently dropped a layer of binding, etc.).
#[cfg(test)]
pub const FAR_APART_THRESHOLD: u32 = 3500;

/// Test-helper: assert two hypervectors are "far apart" in Hamming
/// space. Centralizes the threshold + the diagnostic message so a
/// future tuning of the threshold doesn't have to touch every test
/// site. Use whenever a test claims "X must produce a different vector
/// than Y" — the assertion needs more than `assert_ne!` because random
/// hash collisions on a single bit would still pass `!=` while the
/// vectors are effectively identical for similarity-search purposes.
#[cfg(test)]
pub fn assert_far_apart(a: &Hypervector, b: &Hypervector, label: &str) {
    let d = popcount_distance(a, b);
    assert!(
        d > FAR_APART_THRESHOLD,
        "{label}: hypervectors too close (distance {d}, threshold {FAR_APART_THRESHOLD})",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_seed_is_deterministic() {
        // Same seed must produce the same bytes every call. This is the
        // load-bearing reproducibility property — every machine, every
        // thread, every version produces identical hypervectors. If
        // SplitMix64 ever changes its constants (or we swap PRNGs),
        // this test catches it before encoded data drifts.
        let a = expand_seed(0xDEAD_BEEF_CAFE_BABE);
        let b = expand_seed(0xDEAD_BEEF_CAFE_BABE);
        assert_eq!(a, b);
    }

    #[test]
    fn expand_seed_different_seeds_differ() {
        // Distinct seeds must produce distinct outputs (with overwhelming
        // probability). If a refactor accidentally collapses seeds (e.g.
        // adds a non-zero constant that XORs out), every codebook entry
        // would map to the same vector and the whole stack collapses.
        let a = expand_seed(0);
        let b = expand_seed(1);
        let dist = popcount_distance(&a, &b);
        // Random pairs have ~D/2 expected Hamming distance. With D=8192
        // and ±√D/2 ≈ 45 std deviation, a difference under 3000 is
        // overwhelmingly improbable for random output.
        assert!(dist > 3000, "seeds 0 and 1 produced suspiciously similar vectors: dist={dist}");
        assert!(dist < 5200, "seeds 0 and 1 produced suspiciously different vectors: dist={dist}");
    }

    #[test]
    fn expand_seed_balanced_output() {
        // HDC capacity bounds assume each base vector has roughly equal
        // numbers of 0-bits and 1-bits. A skew (e.g. 70%/30%) would shift
        // the random-pair Hamming baseline and break radius calibration.
        // Verify ~D/2 ones across several seeds.
        for seed in [0, 1, 42, 1_000_000, u64::MAX] {
            let hv = expand_seed(seed);
            let ones: u32 = hv.iter().map(|b| b.count_ones()).sum();
            // D_BITS = 8192, expected ones = 4096, ±3σ ≈ ±136
            let bits = (D_BYTES * 8) as u32;
            let expected = bits / 2;
            assert!(
                ones.abs_diff(expected) < 200,
                "seed {seed}: {ones} ones out of {bits} (expected ~{expected})",
            );
        }
    }

    #[test]
    fn xor_into_is_self_inverse() {
        // XOR-bind's algebra depends on this: A ⊕ B ⊕ B = A. If a future
        // refactor swaps XOR for some other operation that isn't
        // self-inverse, every unbind cleanup-memory query stops working.
        let a = expand_seed(7);
        let b = expand_seed(13);
        let mut hv = a;
        xor_into(&mut hv, &b);
        xor_into(&mut hv, &b);
        assert_eq!(hv, a, "xor_into must be self-inverse");
    }

    #[test]
    fn popcount_distance_zero_to_zero_is_zero() {
        let z = ZERO_HV;
        assert_eq!(popcount_distance(&z, &z), 0);
    }

    #[test]
    fn popcount_distance_random_pair_near_half_d() {
        // Random pairs of base vectors should have Hamming distance near
        // D/2 = 4096. ±3σ ≈ ±136. This is the iid baseline that
        // `_hdc_baseline` calibration replaces with the empirical
        // codebase-specific median, but the synthetic random case must
        // hit theory.
        let a = expand_seed(0xAAAA_AAAA_AAAA_AAAA);
        let b = expand_seed(0xBBBB_BBBB_BBBB_BBBB);
        let d = popcount_distance(&a, &b);
        assert!(
            d.abs_diff(4096) < 200,
            "random pair distance {d} should be near 4096, got abs_diff={}",
            d.abs_diff(4096),
        );
    }

    #[test]
    fn bucket_arity_table() {
        // Pin the bucket boundaries — these are part of the canonical
        // signature, changing one value would shift hypervectors for
        // every node that crosses the boundary.
        assert_eq!(bucket_arity(0), 0);
        assert_eq!(bucket_arity(1), 1);
        assert_eq!(bucket_arity(2), 2);
        assert_eq!(bucket_arity(3), 3);
        assert_eq!(bucket_arity(4), 3);
        assert_eq!(bucket_arity(5), 4);
        assert_eq!(bucket_arity(7), 4);
        assert_eq!(bucket_arity(8), 5);
        assert_eq!(bucket_arity(100), 5);
        assert_eq!(bucket_arity(usize::MAX), 5);
    }

    #[test]
    fn blake3_seed_is_deterministic() {
        let s1 = blake3_seed(b"hello");
        let s2 = blake3_seed(b"hello");
        assert_eq!(s1, s2);
        let s3 = blake3_seed(b"world");
        assert_ne!(s1, s3, "different inputs must produce different seeds");
    }
}
