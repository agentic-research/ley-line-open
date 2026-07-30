//! C FFI for DSSE envelope VERIFICATION — and deliberately nothing else.
//!
//! This is the surface `leyline_envelope.wasm` exports for browser/edge
//! consumers (notme's and cloister's envelope checks): hand it envelope
//! JSON plus the trusted public key, get back the authenticated statement
//! bytes. Signing is NOT exported over FFI, by design rather than by
//! omission: the wasm consumer is exactly the place key material must
//! never travel to — a signer entry point would invite shipping the
//! private key into an environment whose whole trust model assumes it
//! only ever holds public keys. Signing stays in the pure-Rust API
//! ([`crate::Envelope::sign`]), behind the substrate signer, on the side
//! of the boundary that is allowed to hold keys.
//!
//! Follows the leyline-sign FFI pattern (`rs/ll-open/sign/src/ffi.rs`):
//! buffer-based API through `leyline-ffi-helpers`, with
//! `envelope_alloc`/`envelope_free` mirroring `lsign_alloc`/`lsign_free`
//! for wasm32 linear-memory marshalling. Return conventions:
//!
//! - `>= 0`: bytes written to the output buffer
//! - [`ENVELOPE_ERR_INPUT`] (`-1`): caller-side problem — null pointer,
//!   public key not exactly 32 valid Ed25519 bytes, or output buffer too
//!   small
//! - [`ENVELOPE_ERR_PARSE`] (`-2`): the bytes are not a shape-valid DSSE
//!   envelope (or the authenticated payload is not a valid statement)
//! - [`ENVELOPE_ERR_VERIFY`] (`-3`): shape is valid but a signature does
//!   not verify under the supplied key — the envelope must not be trusted
//!
//! Distinct codes, unlike leyline-sign's single `-1`: a browser verifier
//! surfaces these to a human, and "you passed garbage" vs "this envelope
//! is forged" are different incidents.
//!
//! ## wasm32 consumption
//!
//! Same discipline as leyline-sign: allocate via `envelope_alloc`, copy
//! inputs into linear memory, call `envelope_verify`, read output bytes,
//! free via `envelope_free`. Pointers become 32-bit indices into wasm
//! linear memory.

use crate::{Envelope, VerifyError, VerifyingKey};
use leyline_ffi_helpers::{c_input, c_output};

/// Null pointer, invalid/wrong-length public key, or output buffer too
/// small. The buffer-too-small case shares this code because `c_output`
/// owns that check and its convention is `-1`.
pub const ENVELOPE_ERR_INPUT: i32 = -1;
/// Not a shape-valid DSSE envelope, or (post-verification) the
/// authenticated payload is not a valid in-toto statement.
pub const ENVELOPE_ERR_PARSE: i32 = -2;
/// A signature does not verify under the supplied key.
pub const ENVELOPE_ERR_VERIFY: i32 = -3;

// ── wasm32 memory management exports ────────────────────────────────────
//
// Without these, a wasm32 consumer has no way to pass byte buffers to
// the verifier — wasm linear memory is opaque to JS without explicit
// allocator exports. Same `Vec::with_capacity` + `mem::forget` (alloc)
// and `Vec::from_raw_parts` (dealloc) pairing as lsign_alloc/lsign_free.

/// Allocate `size` bytes in wasm linear memory; return pointer (caller
/// owns and must free via `envelope_free`). Aborts on OOM — the default
/// wasm32 allocator traps rather than returning null.
///
/// # Safety
/// Caller must pair every `envelope_alloc(n)` with exactly one
/// `envelope_free(ptr, n)`. Failing to free leaks linear memory until
/// the wasm instance is destroyed.
#[unsafe(no_mangle)]
pub extern "C" fn envelope_alloc(size: usize) -> *mut u8 {
    let mut buf: Vec<u8> = Vec::with_capacity(size);
    let ptr = buf.as_mut_ptr();
    core::mem::forget(buf);
    ptr
}

/// Free a buffer previously allocated by `envelope_alloc`. The `size`
/// must match the original allocation.
///
/// # Safety
/// `ptr` must be a value previously returned by `envelope_alloc`, with
/// the same `size`. Double-free or mismatched-size free is undefined
/// behavior.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn envelope_free(ptr: *mut u8, size: usize) {
    if !ptr.is_null() && size > 0 {
        // SAFETY: caller contract per this fn's # Safety docstring —
        // `ptr` originated from `envelope_alloc(size)` which built a
        // `Vec<u8>` of capacity `size` and `mem::forget`-ed it.
        // Reconstructing with `(ptr, 0, size)` reclaims the alloc.
        unsafe { drop(Vec::from_raw_parts(ptr, 0, size)) };
    }
}

/// The verification logic behind [`envelope_verify`], over safe slices.
///
/// Split out of the extern shim so the error mapping is exercised by
/// ordinary lib tests (and by the diff-scoped mutants gate, which does
/// not mutate `unsafe fn` bodies) — the shim keeps only the raw-pointer
/// marshalling that `c_input`/`c_output` already own.
///
/// On success the returned bytes are the AUTHENTICATED PAYLOAD exactly
/// as signed — not a re-serialization. This crate's serde sorts JSON
/// map keys, so re-encoding a foreign statement (rosary's envelopes
/// preserve predicate key order) would hand the caller bytes that never
/// existed on any wire and no longer digest-match the signature. The
/// payload is still parsed as a statement AFTER signature verification
/// ([`Envelope::verify`]'s only-authenticated-bytes-reach-the-parser
/// rule), so what comes back is both byte-faithful and shape-valid.
fn verify_to_statement_json(envelope_json: &[u8], pubkey: &[u8]) -> Result<Vec<u8>, i32> {
    let raw: [u8; 32] = pubkey.try_into().map_err(|_| ENVELOPE_ERR_INPUT)?;
    let key = VerifyingKey::from_bytes(&raw).map_err(|_| ENVELOPE_ERR_INPUT)?;
    let envelope = Envelope::from_json_slice(envelope_json).map_err(|_| ENVELOPE_ERR_PARSE)?;
    match envelope.verify(&key) {
        // The statement parsed — return the exact bytes it parsed FROM.
        Ok(_statement) => Ok(envelope.payload),
        Err(VerifyError::SignatureInvalid) => Err(ENVELOPE_ERR_VERIFY),
        // Signature valid but the signed bytes are not a statement: a
        // content problem, not a trust problem — same code as the outer
        // parse so callers see one "malformed" class.
        Err(VerifyError::Payload(_)) => Err(ENVELOPE_ERR_PARSE),
    }
}

/// Verify a DSSE envelope against an Ed25519 public key and write the
/// authenticated statement JSON to `out_buf`.
///
/// `envelope_json` is the compact envelope wire form
/// ([`crate::Envelope::to_json_vec`]'s shape); `pubkey` is the raw
/// 32-byte Ed25519 public key. Every signature in the envelope must
/// verify under that key (the `keyid` hint is never consulted — see the
/// crate docs). Returns the payload byte length written on success, or
/// a negative code per the module docs.
///
/// # Safety
/// All input pointers must be non-null and valid for their stated
/// lengths. `out_buf` must be non-null and writable for `out_len`
/// bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn envelope_verify(
    envelope_json_ptr: *const u8,
    envelope_json_len: usize,
    pubkey_ptr: *const u8,
    pubkey_len: usize,
    out_buf: *mut u8,
    out_len: usize,
) -> i32 {
    // SAFETY: all input pointers valid for their stated lengths per
    // the outer fn's # Safety docstring; delegated to `c_input`.
    let (Some(envelope_json), Some(pubkey)) = (unsafe {
        (
            c_input(envelope_json_ptr, envelope_json_len),
            c_input(pubkey_ptr, pubkey_len),
        )
    }) else {
        return ENVELOPE_ERR_INPUT;
    };

    match verify_to_statement_json(envelope_json, pubkey) {
        // SAFETY: out_buf writable for out_len bytes per outer fn's
        // # Safety docstring; delegated to `c_output`.
        Ok(payload) => unsafe { c_output(&payload, out_buf, out_len) },
        Err(code) => code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Statement;
    use crate::tests::{ROSARY_ENVELOPE, vector_pubkey, vector_signer, vector_statement};

    /// Drive the real extern entry (raw pointers and all), not the safe
    /// core — the falsifier is "nothing panics or misbehaves across the
    /// C boundary", which only holds if the boundary is what runs.
    fn verify_via_ffi(envelope_json: &[u8], pubkey: &[u8]) -> Result<Vec<u8>, i32> {
        let mut out = vec![0u8; 64 * 1024];
        let rc = unsafe {
            envelope_verify(
                envelope_json.as_ptr(),
                envelope_json.len(),
                pubkey.as_ptr(),
                pubkey.len(),
                out.as_mut_ptr(),
                out.len(),
            )
        };
        if rc < 0 {
            Err(rc)
        } else {
            out.truncate(rc as usize);
            Ok(out)
        }
    }

    #[test]
    fn ffi_round_trips_to_the_exact_signed_payload_bytes() {
        let stmt = vector_statement();
        let signer = vector_signer();
        let env_json = Envelope::sign(&stmt, &signer).to_json_vec();
        let payload = verify_via_ffi(&env_json, signer.verifying_key().as_bytes())
            .expect("valid envelope + right key verifies over FFI");
        // Byte-exact: the authenticated payload as signed, which for a
        // statement this crate serialized is the statement's own bytes.
        assert_eq!(payload, stmt.to_json_vec());
        assert_eq!(
            Statement::from_json_slice(&payload).expect("payload parses"),
            stmt
        );
    }

    /// The rosary golden envelope through the C entry: proves the FFI
    /// hands back the FOREIGN payload bytes verbatim (their predicate
    /// key order survives — a re-serialization under this workspace's
    /// sorted-map serde would not be byte-identical).
    #[test]
    fn ffi_verifies_the_rosary_golden_envelope() {
        let payload = verify_via_ffi(ROSARY_ENVELOPE.as_bytes(), vector_pubkey().as_bytes())
            .expect("golden envelope verifies over FFI");
        let expected = Envelope::from_json_slice(ROSARY_ENVELOPE.as_bytes())
            .expect("parse")
            .payload;
        assert_eq!(payload, expected);
        let stmt = Statement::from_json_slice(&payload).expect("payload parses");
        assert_eq!(stmt.predicate()["bead_id"], "rosary-test");
        assert_eq!(stmt.predicate_type(), "https://rosary.dev/Handoff/v1");
    }

    #[test]
    fn ffi_wrong_key_returns_verify_error() {
        let env_json = Envelope::sign(&vector_statement(), &vector_signer()).to_json_vec();
        let other = crate::Ed25519RootSigner::from_seed(&[9u8; 32]);
        assert_eq!(
            verify_via_ffi(&env_json, other.verifying_key().as_bytes()),
            Err(ENVELOPE_ERR_VERIFY)
        );
    }

    #[test]
    fn ffi_tampered_payload_returns_verify_error() {
        let signer = vector_signer();
        let env_json = Envelope::sign(&vector_statement(), &signer).to_json_vec();
        // Swap one payload character for another BASE64URL-ALPHABET
        // character, so the envelope still PARSES (mid-string swaps
        // cannot break no-pad canonicality) — only verification can
        // refuse it.
        let tampered = String::from_utf8(env_json).expect("json is utf8").replacen(
            "eyJfdHlwZSI6",
            "eyJfdHlwZSI5",
            1,
        );
        assert_eq!(
            verify_via_ffi(tampered.as_bytes(), signer.verifying_key().as_bytes()),
            Err(ENVELOPE_ERR_VERIFY)
        );
    }

    /// Malformed and shape-invalid inputs come back as clean error codes
    /// — nothing panics across the C boundary.
    #[test]
    fn ffi_malformed_envelope_returns_parse_error() {
        let key = vector_pubkey();
        assert_eq!(
            verify_via_ffi(b"not json at all", key.as_bytes()),
            Err(ENVELOPE_ERR_PARSE)
        );
        assert_eq!(verify_via_ffi(b"", key.as_bytes()), Err(ENVELOPE_ERR_PARSE));
        // Shape-valid JSON, invalid envelope (empty signatures).
        let unsigned =
            br#"{"payloadType":"application/vnd.in-toto+json","payload":"e30","signatures":[]}"#;
        assert_eq!(
            verify_via_ffi(unsigned, key.as_bytes()),
            Err(ENVELOPE_ERR_PARSE)
        );
    }

    #[test]
    fn ffi_wrong_length_pubkey_returns_input_error() {
        let env_json = Envelope::sign(&vector_statement(), &vector_signer()).to_json_vec();
        assert_eq!(
            verify_via_ffi(&env_json, &[0u8; 31]),
            Err(ENVELOPE_ERR_INPUT)
        );
        assert_eq!(
            verify_via_ffi(&env_json, &[0u8; 33]),
            Err(ENVELOPE_ERR_INPUT)
        );
    }

    #[test]
    fn ffi_null_pointers_return_input_error() {
        let key = vector_pubkey();
        let mut out = vec![0u8; 16];
        // SAFETY: null-input paths never dereference; c_input refuses them.
        let rc = unsafe {
            envelope_verify(
                core::ptr::null(),
                0,
                key.as_bytes().as_ptr(),
                32,
                out.as_mut_ptr(),
                out.len(),
            )
        };
        assert_eq!(rc, ENVELOPE_ERR_INPUT);
        let env_json = b"{}";
        // SAFETY: null pubkey pointer never dereferenced.
        let rc = unsafe {
            envelope_verify(
                env_json.as_ptr(),
                env_json.len(),
                core::ptr::null(),
                32,
                out.as_mut_ptr(),
                out.len(),
            )
        };
        assert_eq!(rc, ENVELOPE_ERR_INPUT);
    }

    #[test]
    fn ffi_output_buffer_too_small_returns_input_error() {
        let signer = vector_signer();
        let env_json = Envelope::sign(&vector_statement(), &signer).to_json_vec();
        let mut out = vec![0u8; 4];
        // SAFETY: valid inputs; out_buf writable for its stated length.
        let rc = unsafe {
            envelope_verify(
                env_json.as_ptr(),
                env_json.len(),
                signer.verifying_key().as_bytes().as_ptr(),
                32,
                out.as_mut_ptr(),
                out.len(),
            )
        };
        assert_eq!(rc, ENVELOPE_ERR_INPUT);
    }

    #[test]
    fn alloc_free_round_trip() {
        let ptr = envelope_alloc(64);
        assert!(!ptr.is_null());
        // SAFETY: ptr owns 64 writable bytes per envelope_alloc's contract.
        unsafe {
            for i in 0..64u8 {
                ptr.add(i as usize).write(i);
            }
            assert_eq!(ptr.add(63).read(), 63);
            envelope_free(ptr, 64);
        }
        // The null/zero guard: neither call may touch memory.
        // SAFETY: guarded no-op paths.
        unsafe {
            envelope_free(core::ptr::null_mut(), 64);
            envelope_free(envelope_alloc(0), 0);
        }
    }
}
