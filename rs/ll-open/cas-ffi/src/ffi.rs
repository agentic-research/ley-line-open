//! C FFI for `leyline-cas-ffi`.
//!
//! Follows the `leyline-sign` (rs/ll-open/sign/src/ffi.rs) pattern:
//! buffer-based API, return conventions:
//! - `>= 0`: bytes written to output buffer (always 32 for hash fns)
//! - `-1`:  error (null pointer, output buffer too small)

use leyline_core::substrate::{ContentAddressed, derive_key};
use leyline_ffi_helpers::{c_input, c_output};

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
    // SAFETY: input pointer valid for in_len bytes per outer fn's
    // # Safety docstring; delegated to `c_input`.
    let Some(input) = (unsafe { c_input(in_ptr, in_len) }) else {
        return -1;
    };
    let h = input.hash();
    // SAFETY: out_buf writable for out_len bytes per outer fn's # Safety
    // docstring; delegated to `c_output`.
    unsafe { c_output(h.as_bytes(), out_buf, out_len) }
}

/// Compute BLAKE3 `derive_key(context, input)` and write the 32-byte key
/// into `out_buf`: the keyed primitive under every keyed Σ construction,
/// delegated to `leyline-core::substrate::derive_key` (bead
/// `ley-line-open-12e252`; the prerequisite of run-id v2, ADR-0047 D-i).
///
/// `context` is the construction's context string: `ctx_len` bytes of
/// UTF-8, not NUL-terminated. An empty context names no construction and
/// is refused, as is a context that is not UTF-8 (BLAKE3 defines the
/// context as a string). Pinned by the `deriveKey` cases of
/// `schema-spec/sigma/v1/test-vectors/constructions.json`, which are
/// BLAKE3's own published vectors.
///
/// Returns `32` on success (bytes written), `-1` on error (a null pointer
/// in any argument, an empty or non-UTF-8 context, or `out_len < 32`). On
/// error nothing is written.
///
/// # Safety
///
/// - `ctx_ptr` MUST be non-null and point to a readable buffer of at
///   least `ctx_len` bytes.
/// - `in_ptr` MUST be non-null and point to a readable buffer of at
///   least `in_len` bytes. `in_len == 0` is allowed; `in_ptr` must still
///   be non-null.
/// - `out_buf` MUST be non-null and writable for at least `out_len` bytes.
/// - The output buffer MUST NOT overlap either input buffer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn leyline_derive_key(
    ctx_ptr: *const u8,
    ctx_len: usize,
    in_ptr: *const u8,
    in_len: usize,
    out_buf: *mut u8,
    out_len: usize,
) -> i32 {
    // SAFETY: context pointer valid for ctx_len bytes per outer fn's
    // # Safety docstring; delegated to `c_input`.
    let Some(context) = (unsafe { c_input(ctx_ptr, ctx_len) }) else {
        return -1;
    };
    let Ok(context) = std::str::from_utf8(context) else {
        return -1;
    };
    if context.is_empty() {
        return -1;
    }
    // SAFETY: input pointer valid for in_len bytes per outer fn's
    // # Safety docstring; delegated to `c_input`.
    let Some(input) = (unsafe { c_input(in_ptr, in_len) }) else {
        return -1;
    };
    let key = derive_key(context, input);
    // SAFETY: out_buf writable for out_len bytes per outer fn's # Safety
    // docstring; delegated to `c_output`.
    unsafe { c_output(key.as_bytes(), out_buf, out_len) }
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
        let rc =
            unsafe { leyline_hash_bytes(input.as_ptr(), input.len(), out.as_mut_ptr(), out.len()) };
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
        let rc = unsafe { leyline_hash_bytes(std::ptr::null(), 0, out.as_mut_ptr(), out.len()) };
        assert_eq!(rc, -1);
    }

    #[test]
    fn ffi_rejects_null_output_pointer() {
        let input = b"x";
        let rc =
            unsafe { leyline_hash_bytes(input.as_ptr(), input.len(), std::ptr::null_mut(), 32) };
        assert_eq!(rc, -1);
    }

    #[test]
    fn ffi_rejects_undersized_output_buffer() {
        let input = b"x";
        let mut out = [0u8; 31]; // one byte short
        let rc =
            unsafe { leyline_hash_bytes(input.as_ptr(), input.len(), out.as_mut_ptr(), out.len()) };
        assert_eq!(rc, -1);
    }

    #[test]
    fn ffi_accepts_oversized_output_buffer_and_writes_exactly_32() {
        let input = b"y";
        let mut out = [0xFFu8; 64];
        let rc =
            unsafe { leyline_hash_bytes(input.as_ptr(), input.len(), out.as_mut_ptr(), out.len()) };
        assert_eq!(rc, 32);
        // The first 32 bytes equal the hash; the rest are untouched.
        assert_eq!(&out[..32], input.hash().as_bytes());
        assert!(out[32..].iter().all(|&b| b == 0xFF));
    }

    // ── leyline_derive_key (bead ley-line-open-12e252) ─────────────────

    fn derive_key_via_ffi(context: &[u8], input: &[u8]) -> [u8; 32] {
        let mut out = [0u8; 32];
        let rc = unsafe {
            leyline_derive_key(
                context.as_ptr(),
                context.len(),
                input.as_ptr(),
                input.len(),
                out.as_mut_ptr(),
                out.len(),
            )
        };
        assert_eq!(rc, 32, "leyline_derive_key returned {rc}, expected 32");
        out
    }

    fn sigma_vectors() -> serde_json::Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../ll-core/schema-spec/sigma/v1/test-vectors/constructions.json");
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("Σ vectors must ship with the schema: {e}"));
        serde_json::from_slice(&bytes).expect("Σ vectors are JSON")
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The C ABI reproduces the checked-in `deriveKey` vectors (BLAKE3's
    /// own test vectors), byte for byte — what cloister's run-id v2 relies
    /// on when it derives through the wasm build of this function.
    #[test]
    fn ffi_derive_key_matches_the_sigma_vectors() {
        let v = sigma_vectors();
        let cases = v["deriveKey"].as_array().expect("deriveKey cases");
        assert!(
            !cases.is_empty(),
            "deriveKey has no cases: it gates nothing"
        );
        for case in cases {
            let context = case["context"].as_str().unwrap();
            let len = case["inputLen"].as_u64().unwrap() as usize;
            let input: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            assert_eq!(
                hex(&derive_key_via_ffi(context.as_bytes(), &input)),
                case["key"].as_str().unwrap(),
                "leyline_derive_key drifted for {}",
                case["name"]
            );
        }
    }

    #[test]
    fn ffi_derive_key_is_keyed_by_the_context() {
        let a = derive_key_via_ffi(b"leyline/test/a", b"x");
        let b = derive_key_via_ffi(b"leyline/test/b", b"x");
        assert_ne!(a, b, "two contexts must give two keys");
        assert_eq!(a, blake3::derive_key("leyline/test/a", b"x"));
        assert_ne!(&a, blake3::hash(b"x").as_bytes(), "keyed is not σ_bytes");
    }

    #[test]
    fn ffi_derive_key_accepts_empty_input_with_a_dummy_pointer() {
        let dummy = [0u8; 1];
        let mut out = [0u8; 32];
        let ctx = b"leyline/test/empty";
        let rc = unsafe {
            leyline_derive_key(
                ctx.as_ptr(),
                ctx.len(),
                dummy.as_ptr(),
                0,
                out.as_mut_ptr(),
                out.len(),
            )
        };
        assert_eq!(rc, 32);
        assert_eq!(out, blake3::derive_key("leyline/test/empty", b""));
    }

    /// Every refusal returns -1 and leaves the output buffer untouched.
    #[test]
    fn ffi_derive_key_refuses_bad_arguments() {
        let ctx = b"leyline/test/refuse";
        let input = b"x";
        let call = |ctx_ptr: *const u8,
                    ctx_len: usize,
                    in_ptr: *const u8,
                    out_ptr: *mut u8,
                    out_len: usize| unsafe {
            leyline_derive_key(ctx_ptr, ctx_len, in_ptr, input.len(), out_ptr, out_len)
        };
        let mut out = [0xAAu8; 32];
        let o = out.as_mut_ptr();
        // Null context, null input, null output.
        assert_eq!(call(std::ptr::null(), 0, input.as_ptr(), o, 32), -1);
        assert_eq!(call(ctx.as_ptr(), ctx.len(), std::ptr::null(), o, 32), -1);
        assert_eq!(
            call(
                ctx.as_ptr(),
                ctx.len(),
                input.as_ptr(),
                std::ptr::null_mut(),
                32
            ),
            -1
        );
        // An empty context names no construction.
        assert_eq!(call(ctx.as_ptr(), 0, input.as_ptr(), o, 32), -1);
        // A context that is not UTF-8 is not a BLAKE3 context string.
        let bad = [0xFFu8, 0xFE];
        assert_eq!(call(bad.as_ptr(), bad.len(), input.as_ptr(), o, 32), -1);
        // One byte short.
        assert_eq!(call(ctx.as_ptr(), ctx.len(), input.as_ptr(), o, 31), -1);
        assert!(out.iter().all(|&b| b == 0xAA), "a refusal writes nothing");
    }

    #[test]
    fn ffi_derive_key_writes_exactly_32_into_an_oversized_buffer() {
        let ctx = b"leyline/test/oversized";
        let mut out = [0xFFu8; 64];
        let rc = unsafe {
            leyline_derive_key(
                ctx.as_ptr(),
                ctx.len(),
                b"y".as_ptr(),
                1,
                out.as_mut_ptr(),
                out.len(),
            )
        };
        assert_eq!(rc, 32);
        assert_eq!(
            &out[..32],
            &blake3::derive_key("leyline/test/oversized", b"y")
        );
        assert!(out[32..].iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn ffi_empty_input_returns_blake3_of_empty() {
        // Even for zero-length input the FFI must produce the 32-byte
        // BLAKE3 of empty (af1349b9...). Common edge in OCI manifests
        // (empty layers).
        let mut out = [0u8; 32];
        let dummy = [0u8; 1]; // dummy non-null pointer for empty slice
        let rc = unsafe { leyline_hash_bytes(dummy.as_ptr(), 0, out.as_mut_ptr(), out.len()) };
        assert_eq!(rc, 32);
        assert_eq!(&out[..], blake3::hash(&[]).as_bytes());
    }
}
