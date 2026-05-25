//! C FFI for `leyline-cas-ffi`.
//!
//! Follows the `leyline-sign` (rs/ll-open/sign/src/ffi.rs) pattern:
//! buffer-based API, return conventions:
//! - `>= 0`: bytes written to output buffer (always 32 for hash fns)
//! - `-1`:  error (null pointer, output buffer too small)

use leyline_core::substrate::ContentAddressed;

/// Helper — write fixed-size bytes into a caller-provided output buffer.
/// Returns byte count on success, -1 if the buffer is too small.
/// Lifted from rs/ll-open/sign/src/ffi.rs:write_out so the convention
/// stays uniform across LLO's FFI crates.
unsafe fn write_out(data: &[u8], out_buf: *mut u8, out_len: usize) -> i32 {
    if data.len() > out_len {
        return -1;
    }
    unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), out_buf, data.len()) };
    data.len() as i32
}

/// Compute the substrate hash (BLAKE3-256) of `in_ptr..in_ptr+in_len`
/// and write the 32-byte digest into `out_buf`.
///
/// **Substrate guarantee**: the algorithm is BLAKE3 per Σ §3.4. This
/// FFI is the consumer-side entry point that delegates to
/// `leyline-core::ContentAddressed for [u8]` — every consumer (cloister
/// today, future cross-runtime callers later) computes the same bytes
/// for the same input, byte-for-byte across language boundaries.
///
/// Returns `32` on success (bytes written), `-1` on error (null
/// pointer in any argument, or `out_len < 32`).
///
/// # Safety
///
/// - `in_ptr` MUST be non-null and point to a readable buffer of at
///   least `in_len` bytes. `in_len == 0` is allowed; `in_ptr` must
///   still be non-null (empty slices have a non-null sentinel pointer
///   in well-behaved callers).
/// - `out_buf` MUST be non-null and writable for at least 32 bytes.
/// - The input and output buffers MUST NOT overlap. (Hashing reads
///   the input twice in some BLAKE3 micro-arch paths; aliased buffers
///   are UB.)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn leyline_hash_bytes(
    in_ptr: *const u8,
    in_len: usize,
    out_buf: *mut u8,
    out_len: usize,
) -> i32 {
    if in_ptr.is_null() || out_buf.is_null() {
        return -1;
    }
    let input = unsafe { std::slice::from_raw_parts(in_ptr, in_len) };
    let h = input.hash();
    unsafe { write_out(h.as_bytes(), out_buf, out_len) }
}

#[cfg(test)]
mod tests {
    //! FFI surface tested from the caller's point of view: raw
    //! pointers, return-code branches, byte-for-byte equality with the
    //! in-Rust hash. These tests are the executable contract for the
    //! consumer (cloister will hold against the SAME byte sequences).
    use super::*;

    fn hash_via_ffi(input: &[u8]) -> [u8; 32] {
        let mut out = [0u8; 32];
        let rc = unsafe {
            leyline_hash_bytes(
                input.as_ptr(),
                input.len(),
                out.as_mut_ptr(),
                out.len(),
            )
        };
        assert_eq!(rc, 32, "leyline_hash_bytes returned {rc}, expected 32");
        out
    }

    #[test]
    fn ffi_hash_matches_in_rust_hash() {
        for input in [
            b"".as_slice(),
            b"a".as_slice(),
            b"hello cas-ffi".as_slice(),
            &[0u8; 1024][..],
            // A sample of bytes that exercise BLAKE3's multi-block path.
            &(0..=255u8).collect::<Vec<u8>>()[..],
        ] {
            let via_ffi = hash_via_ffi(input);
            let via_trait = input.hash();
            assert_eq!(
                &via_ffi[..],
                via_trait.as_bytes(),
                "FFI hash diverged from ContentAddressed::hash for input of len {}",
                input.len(),
            );
        }
    }

    #[test]
    fn ffi_hash_byte_equal_to_direct_blake3() {
        // Substrate cross-check: the FFI hash MUST equal `blake3::hash`
        // applied to the same bytes. If this test ever fails, ll-core's
        // BLAKE3 lock has been broken silently (or the FFI is computing
        // the wrong σ — either is a substrate-correctness incident).
        let input = b"substrate-cross-check";
        let via_ffi = hash_via_ffi(input);
        let via_blake3 = *blake3::hash(input).as_bytes();
        assert_eq!(via_ffi, via_blake3);
    }

    #[test]
    fn ffi_rejects_null_input_pointer() {
        let mut out = [0u8; 32];
        let rc = unsafe {
            leyline_hash_bytes(std::ptr::null(), 0, out.as_mut_ptr(), out.len())
        };
        assert_eq!(rc, -1);
    }

    #[test]
    fn ffi_rejects_null_output_pointer() {
        let input = b"x";
        let rc = unsafe {
            leyline_hash_bytes(input.as_ptr(), input.len(), std::ptr::null_mut(), 32)
        };
        assert_eq!(rc, -1);
    }

    #[test]
    fn ffi_rejects_undersized_output_buffer() {
        let input = b"x";
        let mut out = [0u8; 31]; // one byte short
        let rc = unsafe {
            leyline_hash_bytes(input.as_ptr(), input.len(), out.as_mut_ptr(), out.len())
        };
        assert_eq!(rc, -1);
    }

    #[test]
    fn ffi_accepts_oversized_output_buffer_and_writes_exactly_32() {
        let input = b"y";
        let mut out = [0xFFu8; 64];
        let rc = unsafe {
            leyline_hash_bytes(input.as_ptr(), input.len(), out.as_mut_ptr(), out.len())
        };
        assert_eq!(rc, 32);
        // The first 32 bytes equal the hash; the rest are untouched.
        assert_eq!(&out[..32], input.hash().as_bytes());
        assert!(out[32..].iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn ffi_empty_input_returns_blake3_of_empty() {
        // Even for zero-length input the FFI must produce the 32-byte
        // BLAKE3 of empty (af1349b9...). Common edge in OCI manifests
        // (empty layers).
        let mut out = [0u8; 32];
        let dummy = [0u8; 1]; // dummy non-null pointer for empty slice
        let rc = unsafe {
            leyline_hash_bytes(dummy.as_ptr(), 0, out.as_mut_ptr(), out.len())
        };
        assert_eq!(rc, 32);
        assert_eq!(&out[..], blake3::hash(&[]).as_bytes());
    }
}
