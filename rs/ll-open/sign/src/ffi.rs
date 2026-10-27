//! C FFI for CMS/PKCS#7 signing and verification.
//!
//! Follows the leyline-fs pattern: buffer-based API, return conventions:
//! - `>= 0`: bytes written to output buffer
//! - `-1`: error (signing/verification failed, buffer too small)
//!
//! ## wasm32 consumption
//!
//! The same FFI exports work for both native (cdylib via cbindgen) and
//! wasm32 (workerd / browsers / WASI hosts) consumers. wasm32 callers
//! manage linear memory directly: allocate via `lsign_alloc`, copy
//! inputs in, call the FFI function, read output bytes, free via
//! `lsign_free`. Same calling convention; pointers become 32-bit
//! indices into wasm linear memory.

use crate::cms;
use leyline_ffi_helpers::{c_input, c_output};

// ── wasm32 memory management exports ────────────────────────────────────
//
// Without these, a wasm32 consumer has no way to pass byte buffers to
// the verifier — wasm linear memory is opaque to JS without explicit
// allocator exports. These pair with `Vec::with_capacity` + `mem::forget`
// (alloc) and `Vec::from_raw_parts` (dealloc).

/// Allocate `size` bytes in wasm linear memory; return pointer (caller
/// owns and must free via `lsign_free`). Aborts on OOM — the default
/// wasm32 allocator traps rather than returning null.
///
/// # Safety
/// Caller must pair every `lsign_alloc(n)` with exactly one
/// `lsign_free(ptr, n)`. Failing to free leaks linear memory until the
/// wasm instance is destroyed.
#[unsafe(no_mangle)]
pub extern "C" fn lsign_alloc(size: usize) -> *mut u8 {
    let mut buf: Vec<u8> = Vec::with_capacity(size);
    let ptr = buf.as_mut_ptr();
    core::mem::forget(buf);
    ptr
}

/// Free a buffer previously allocated by `lsign_alloc`. The `size` must
/// match the original allocation.
///
/// # Safety
/// `ptr` must be a value previously returned by `lsign_alloc`, with the
/// same `size`. Double-free or mismatched-size free is undefined
/// behavior.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lsign_free(ptr: *mut u8, size: usize) {
    if !ptr.is_null() && size > 0 {
        // SAFETY: caller contract per this fn's # Safety docstring —
        // `ptr` originated from `lsign_alloc(size)` which built a
        // `Vec<u8>` of capacity `size` and `mem::forget`-ed it.
        // Reconstructing with `(ptr, 0, size)` reclaims the alloc.
        unsafe { drop(Vec::from_raw_parts(ptr, 0, size)) };
    }
}

/// Sign data with CMS/PKCS#7 using Ed25519 and signed attributes.
///
/// Returns >= 0 (bytes written to out_buf) on success, -1 on error.
///
/// # Safety
/// All input pointers must be non-null and valid for their stated
/// lengths. `private_key_ptr` must point to exactly 64 bytes (Ed25519
/// keypair). `out_buf` must be non-null and writable for `out_len`
/// bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn leyline_sign_data(
    data_ptr: *const u8,
    data_len: usize,
    cert_der_ptr: *const u8,
    cert_der_len: usize,
    private_key_ptr: *const u8,
    out_buf: *mut u8,
    out_len: usize,
) -> i32 {
    // SAFETY: all input pointers valid for their stated lengths per
    // the outer fn's # Safety docstring; delegated to `c_input`.
    let (Some(data), Some(cert_der), Some(key_slice)) = (unsafe {
        (
            c_input(data_ptr, data_len),
            c_input(cert_der_ptr, cert_der_len),
            c_input(private_key_ptr, 64),
        )
    }) else {
        return -1;
    };
    let key: [u8; 64] = match key_slice.try_into() {
        Ok(k) => k,
        Err(_) => return -1,
    };

    match cms::sign_data(data, cert_der, &key) {
        // SAFETY: out_buf writable for out_len bytes per outer fn's
        // # Safety docstring; delegated to `c_output`.
        Ok(sig) => unsafe { c_output(&sig, out_buf, out_len) },
        Err(_) => -1,
    }
}

/// Sign data with CMS/PKCS#7 using Ed25519 PureEdDSA (no signed attributes).
///
/// Returns >= 0 (bytes written to out_buf) on success, -1 on error.
///
/// # Safety
/// All input pointers must be non-null and valid for their stated
/// lengths. `private_key_ptr` must point to exactly 64 bytes (Ed25519
/// keypair). `out_buf` must be non-null and writable for `out_len`
/// bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn leyline_sign_data_without_attributes(
    data_ptr: *const u8,
    data_len: usize,
    cert_der_ptr: *const u8,
    cert_der_len: usize,
    private_key_ptr: *const u8,
    out_buf: *mut u8,
    out_len: usize,
) -> i32 {
    // SAFETY: all input pointers valid for their stated lengths per
    // the outer fn's # Safety docstring; delegated to `c_input`.
    let (Some(data), Some(cert_der), Some(key_slice)) = (unsafe {
        (
            c_input(data_ptr, data_len),
            c_input(cert_der_ptr, cert_der_len),
            c_input(private_key_ptr, 64),
        )
    }) else {
        return -1;
    };
    let key: [u8; 64] = match key_slice.try_into() {
        Ok(k) => k,
        Err(_) => return -1,
    };

    match cms::sign_data_without_attributes(data, cert_der, &key) {
        // SAFETY: out_buf writable for out_len bytes per outer fn's
        // # Safety docstring; delegated to `c_output`.
        Ok(sig) => unsafe { c_output(&sig, out_buf, out_len) },
        Err(_) => -1,
    }
}

/// Verify a CMS/PKCS#7 detached signature.
///
/// On success, writes the signer certificate DER to `cert_out_buf` and returns
/// the number of bytes written (>= 0). Returns -1 on verification failure.
///
/// # Safety
/// All input pointers must be non-null and valid for their stated
/// lengths. `cert_out_buf` must be non-null and writable for
/// `cert_out_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn leyline_verify(
    cms_sig_ptr: *const u8,
    cms_sig_len: usize,
    data_ptr: *const u8,
    data_len: usize,
    cert_out_buf: *mut u8,
    cert_out_len: usize,
) -> i32 {
    // SAFETY: all input pointers valid for their stated lengths per
    // the outer fn's # Safety docstring; delegated to `c_input`.
    let (Some(cms_sig), Some(data)) = (unsafe {
        (
            c_input(cms_sig_ptr, cms_sig_len),
            c_input(data_ptr, data_len),
        )
    }) else {
        return -1;
    };

    match cms::verify(cms_sig, data, &cms::VerifyOptions::default()) {
        // SAFETY: cert_out_buf writable for cert_out_len bytes per
        // outer fn's # Safety docstring; delegated to `c_output`.
        Ok(cert_der) => unsafe { c_output(&cert_der, cert_out_buf, cert_out_len) },
        Err(_) => -1,
    }
}

// ── Cert-chain verify FFI export ─────────────────────────────────────────
//
// Used by consumers (originally cloister's lease middleware,
// cloister-bd7770) via the wasm32 build of this crate. See cert_chain.rs
// for verify + claims-extraction logic; this file just exposes the C-FFI
// shim.
//
// Output format: hand-rolled JSON string (see cert_chain::claims_to_json)
// — claims are small (~200 bytes), and adding serde_json to the wasm
// build adds substantial size for a flat struct.

/// Verify an end-entity cert is signed by master_pubkey + write parsed
/// claims as JSON into `claims_out_buf`. Returns claims byte length on
/// success, -1 on failure.
///
/// JSON shape (compact, no whitespace):
///   {"epk":"<base64url>","nb":<unix-secs>,"na":<unix-secs>,
///    "pr":"signet","ep":<u32>,"pf":"<utf8>","sc":"<utf8>","cd":"<base64url>"}
///
/// `pr` appears only for Signet-profile certs (notme / signet); absent
/// means the Interlace profile. `ep`, `pf`, `sc`, `cd` are optional;
/// `pf` and `sc` never appear for a Signet-profile cert, whose `.1.5` /
/// `.1.6` mean something else (bead `ley-line-open-9ac66d`).
///
/// # Safety
/// All input pointers must be non-null and valid for their stated
/// lengths. `claims_out_buf` must be non-null and writable for
/// `claims_out_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn leyline_verify_cert_chain(
    cert_der_ptr: *const u8,
    cert_der_len: usize,
    master_pubkey_ptr: *const u8,
    master_pubkey_len: usize,
    claims_out_buf: *mut u8,
    claims_out_len: usize,
) -> i32 {
    // SAFETY: all input pointers valid for their stated lengths per
    // the outer fn's # Safety docstring; delegated to `c_input`.
    let (Some(cert_der), Some(master_pubkey)) = (unsafe {
        (
            c_input(cert_der_ptr, cert_der_len),
            c_input(master_pubkey_ptr, master_pubkey_len),
        )
    }) else {
        return -1;
    };

    match crate::cert_chain::verify_cert_chain(cert_der, master_pubkey) {
        Ok(claims) => {
            let json = crate::cert_chain::claims_to_json(&claims);
            // SAFETY: claims_out_buf writable for claims_out_len bytes
            // per outer fn's # Safety docstring; delegated to `c_output`.
            unsafe { c_output(json.as_bytes(), claims_out_buf, claims_out_len) }
        }
        Err(_) => -1,
    }
}

/// Verify `leaf` through a path of intermediates to the Ed25519 trust
/// anchor `anchor_pubkey` and write the leaf's claims as JSON (same shape
/// as `leyline_verify_cert_chain`) into `claims_out_buf`. Returns claims
/// byte length on success, -1 on failure.
///
/// `intermediates` is zero or more DER certificates concatenated, the
/// leaf's issuer first and the anchor-signed certificate last (at most
/// `cert_chain::MAX_INTERMEDIATES`). With `intermediates_len == 0` the
/// pointer is not read and this is `leyline_verify_cert_chain`. For a
/// path, `nb`/`na` are the intersection of every certificate's validity,
/// `ep` is the minimum epoch (every certificate carries one, or none
/// does), and `cd` appears only when every certificate carries the same
/// confinementDigest. A missing `pf`/`sc` means "not read" under
/// `"pr":"signet"`, never "unrestricted"; check `pr` first.
///
/// `expected_profile` binds the anchor to the profile it issues: `0` accepts
/// either profile, `1` requires Interlace, `2` requires Signet (notme /
/// signet), and any other value fails (`CertProfile::FFI_*`). A caller that
/// knows which profile an anchor issues should pass it, so the issuer cannot
/// present the other profile's claims.
///
/// # Safety
/// All input pointers must be non-null and valid for their stated
/// lengths (`intermediates_ptr` may be null when `intermediates_len` is
/// 0). `claims_out_buf` must be non-null and writable for
/// `claims_out_len` bytes.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn leyline_verify_cert_path(
    leaf_der_ptr: *const u8,
    leaf_der_len: usize,
    intermediates_ptr: *const u8,
    intermediates_len: usize,
    anchor_pubkey_ptr: *const u8,
    anchor_pubkey_len: usize,
    expected_profile: u32,
    claims_out_buf: *mut u8,
    claims_out_len: usize,
) -> i32 {
    let Ok(expected) = crate::cert_chain::CertProfile::from_ffi_code(expected_profile) else {
        return -1;
    };
    // SAFETY: input pointers valid for their stated lengths per the outer
    // fn's # Safety docstring; delegated to `c_input`.
    let (Some(leaf_der), Some(anchor)) = (unsafe {
        (
            c_input(leaf_der_ptr, leaf_der_len),
            c_input(anchor_pubkey_ptr, anchor_pubkey_len),
        )
    }) else {
        return -1;
    };
    let intermediates_buf: &[u8] = if intermediates_len == 0 {
        &[]
    } else {
        // SAFETY: as above.
        match unsafe { c_input(intermediates_ptr, intermediates_len) } {
            Some(b) => b,
            None => return -1,
        }
    };
    let Ok(intermediates) = crate::cert_chain::split_concatenated_der(intermediates_buf) else {
        return -1;
    };

    match crate::cert_chain::verify_cert_path_expecting(leaf_der, &intermediates, anchor, expected)
    {
        Ok(claims) => {
            let json = crate::cert_chain::claims_to_json(&claims);
            // SAFETY: claims_out_buf writable for claims_out_len bytes
            // per outer fn's # Safety docstring; delegated to `c_output`.
            unsafe { c_output(json.as_bytes(), claims_out_buf, claims_out_len) }
        }
        Err(_) => -1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reuse the test cert builder from cms module.
    fn generate_test_cert_and_key() -> (Vec<u8>, [u8; 64]) {
        let signing_key = crate::cms::tests::random_signing_key();
        let cert_der = crate::cms::tests::build_self_signed_cert(&signing_key);
        let keypair_bytes = signing_key.to_keypair_bytes();
        (cert_der, keypair_bytes)
    }

    #[test]
    fn ffi_sign_and_verify() {
        let (cert_der, key) = generate_test_cert_and_key();
        let data = b"FFI round-trip test";

        let mut sig_buf = vec![0u8; 4096];
        let sig_len = unsafe {
            leyline_sign_data(
                data.as_ptr(),
                data.len(),
                cert_der.as_ptr(),
                cert_der.len(),
                key.as_ptr(),
                sig_buf.as_mut_ptr(),
                sig_buf.len(),
            )
        };
        assert!(sig_len > 0, "signing should succeed, got {}", sig_len);

        let mut cert_buf = vec![0u8; 4096];
        let cert_len = unsafe {
            leyline_verify(
                sig_buf.as_ptr(),
                sig_len as usize,
                data.as_ptr(),
                data.len(),
                cert_buf.as_mut_ptr(),
                cert_buf.len(),
            )
        };
        assert!(
            cert_len > 0,
            "verification should succeed, got {}",
            cert_len
        );
        assert_eq!(&cert_buf[..cert_len as usize], &cert_der);
    }

    #[test]
    fn ffi_buffer_too_small() {
        let (cert_der, key) = generate_test_cert_and_key();
        let data = b"test";

        // Tiny buffer should return -1
        let mut sig_buf = vec![0u8; 10];
        let sig_len = unsafe {
            leyline_sign_data(
                data.as_ptr(),
                data.len(),
                cert_der.as_ptr(),
                cert_der.len(),
                key.as_ptr(),
                sig_buf.as_mut_ptr(),
                sig_buf.len(),
            )
        };
        assert_eq!(sig_len, -1, "should fail with small buffer");
    }

    // Real notme certificates (bead ley-line-open-9ac66d); provenance in
    // tests/fixtures/notme/PROVENANCE.md.
    const ROOT_PUB: &[u8] = include_bytes!("../tests/fixtures/notme/root.pub");
    const ISSUING_CA: &[u8] = include_bytes!("../tests/fixtures/notme/issuing-ca.der");
    const TASK_SIGNING: &[u8] = include_bytes!("../tests/fixtures/notme/task-signing.der");
    const DIRECT_SIGNING: &[u8] = include_bytes!("../tests/fixtures/notme/direct-signing.der");

    use crate::cert_chain::CertProfile;

    /// Call the path export; returns (rc, JSON written).
    fn path_call(
        leaf: &[u8],
        intermediates: *const u8,
        intermediates_len: usize,
        anchor: &[u8],
        expected_profile: u32,
    ) -> (i32, String) {
        let mut out = vec![0u8; 1024];
        let n = unsafe {
            leyline_verify_cert_path(
                leaf.as_ptr(),
                leaf.len(),
                intermediates,
                intermediates_len,
                anchor.as_ptr(),
                anchor.len(),
                expected_profile,
                out.as_mut_ptr(),
                out.len(),
            )
        };
        let json = if n > 0 {
            String::from_utf8(out[..n as usize].to_vec()).unwrap()
        } else {
            String::new()
        };
        (n, json)
    }

    fn notme_path(expected_profile: u32) -> (i32, String) {
        path_call(
            TASK_SIGNING,
            ISSUING_CA.as_ptr(),
            ISSUING_CA.len(),
            ROOT_PUB,
            expected_profile,
        )
    }

    #[test]
    fn ffi_notme_path_writes_signet_claims() {
        let (n, json) = notme_path(CertProfile::FFI_ANY);
        assert!(n > 0, "ffi path verify returned {n}");
        assert!(json.contains("\"pr\":\"signet\""), "{json}");
        assert!(json.contains("\"ep\":3"), "{json}");
        assert!(!json.contains("\"pf\""), "{json}");
        assert!(!json.contains("\"sc\""), "{json}");
    }

    #[test]
    fn ffi_path_without_intermediates_matches_chain_export() {
        let (n, path_json) = path_call(
            DIRECT_SIGNING,
            core::ptr::null(),
            0,
            ROOT_PUB,
            CertProfile::FFI_ANY,
        );
        let mut chain_out = vec![0u8; 1024];
        let m = unsafe {
            leyline_verify_cert_chain(
                DIRECT_SIGNING.as_ptr(),
                DIRECT_SIGNING.len(),
                ROOT_PUB.as_ptr(),
                ROOT_PUB.len(),
                chain_out.as_mut_ptr(),
                chain_out.len(),
            )
        };
        assert!(n > 0 && n == m);
        assert_eq!(path_json.as_bytes(), &chain_out[..m as usize]);
    }

    #[test]
    fn ffi_path_rejects_trailing_bytes_and_missing_tier() {
        let any = CertProfile::FFI_ANY;
        let mut bad = ISSUING_CA.to_vec();
        bad.push(0x30);
        assert_eq!(
            path_call(TASK_SIGNING, bad.as_ptr(), bad.len(), ROOT_PUB, any).0,
            -1
        );
        // The task leaf does not chain to the root without its tier.
        assert_eq!(
            path_call(TASK_SIGNING, core::ptr::null(), 0, ROOT_PUB, any).0,
            -1
        );
        // A non-empty intermediates length with a null pointer is refused.
        assert_eq!(
            path_call(TASK_SIGNING, core::ptr::null(), 5, ROOT_PUB, any).0,
            -1
        );
    }

    /// The notme anchor bound to the Signet profile accepts its own path and
    /// refuses to be read as an Interlace issuer.
    #[test]
    fn ffi_expected_profile_binds_the_notme_anchor() {
        assert!(notme_path(CertProfile::FFI_SIGNET).0 > 0);
        assert_eq!(notme_path(CertProfile::FFI_INTERLACE).0, -1);
        // Unknown codes fail rather than widen to "any".
        for code in [3, 7, u32::MAX] {
            assert_eq!(notme_path(code).0, -1, "code {code}");
        }
    }

    /// An Interlace-shaped leaf signed by the notme anchor key is refused
    /// when the caller binds that anchor to Signet, and accepted only when
    /// bound to Interlace (or unbound).
    #[test]
    fn ffi_expected_profile_refuses_interlace_claims_from_a_signet_anchor() {
        use ed25519_dalek::SigningKey;
        let anchor = SigningKey::from_bytes(&[0x42; 32]);
        let leaf_key = SigningKey::from_bytes(&[0x43; 32]);
        let leaf = crate::cert_chain::tests_helpers::mint_test_cert(
            &anchor,
            &leaf_key,
            1_700_000_000,
            1_700_000_300,
            Some(3),
            Some("sha256:forged-peer"),
            Some("bead_create:*"),
            None,
        );
        let pk = anchor.verifying_key();
        let call = |code| path_call(&leaf, core::ptr::null(), 0, pk.as_bytes(), code);
        assert_eq!(call(CertProfile::FFI_SIGNET).0, -1);
        let (n, json) = call(CertProfile::FFI_INTERLACE);
        assert!(
            n > 0 && json.contains("\"pf\":\"sha256:forged-peer\""),
            "{json}"
        );
        assert_eq!(call(CertProfile::FFI_ANY), (n, json));
    }
}
