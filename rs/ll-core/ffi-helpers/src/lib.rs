//! Typed helpers for the C-boundary raw-pointer pattern.
//!
//! Bead `ley-line-open-85fb1f` PR 2. Every `extern "C" fn` in LLO
//! that takes `(*const u8, usize)` input buffers or `(*mut u8, usize)`
//! output buffers repeats the SAME shape:
//!
//! ```text
//! if ptr.is_null() { return -1; }
//! let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
//! // ... use slice ...
//! unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), out_buf, data.len()) }
//! ```
//!
//! This crate consolidates the invariant into TWO functions —
//! [`c_input`] and [`c_output`] — so the SAFETY docstring lives in ONE
//! place instead of being duplicated (or absent) at every call site.
//!
//! Both helpers are `unsafe fn` (their inputs are C-owned raw pointers);
//! callers still wrap each use in an `unsafe { }` block, but a single
//! block per outer FFI export can now cover ALL input reads via one
//! grouped call. That collapses N+1 unsafe blocks per export
//! (N inputs + 1 output) down to 2 (inputs + output).
//!
//! Zero deps by design: `leyline-sign`, `leyline-cas-ffi`, and
//! `leyline-fs` are all wasm-targeted, and pulling `leyline-core` in
//! just for these two helpers would drag `memmap2` / `blake3` / `capnp`
//! into their dep trees.

#![no_std]

/// Cast a caller-owned C input buffer `(ptr, len)` into a Rust byte
/// slice. Returns `None` when the pointer is null (defensive; the
/// caller should map this to their error convention — typically `-1`).
///
/// # Safety
///
/// The caller must guarantee that:
///
/// 1. `ptr` is either null OR points to a readable region of exactly
///    `len` bytes. `len == 0` with a null `ptr` returns `None`; some
///    C APIs pass `(null, 0)` to mean "empty" — callers that need
///    that shape should check first.
/// 2. The memory referenced by `ptr` is not mutated by any other
///    thread for the duration of the returned reference's lifetime
///    (`'a` is a caller-supplied lifetime; enforce with a well-scoped
///    binding, e.g. inside the extern `fn` body).
/// 3. `ptr` outlives `'a`.
///
/// These invariants match the standard `slice::from_raw_parts`
/// contract; this wrapper adds only the null check.
pub unsafe fn c_input<'a>(ptr: *const u8, len: usize) -> Option<&'a [u8]> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller's contract (see docstring) — ptr valid for len
    // bytes, non-mutating, outlives `'a`.
    Some(unsafe { core::slice::from_raw_parts(ptr, len) })
}

/// Parse a null-terminated C string `(ptr)` into a Rust `&str`. Returns
/// `None` when the pointer is null or the bytes are not valid UTF-8.
///
/// # Safety
///
/// The caller must guarantee that:
///
/// 1. `ptr` is either null OR points to a NUL-terminated sequence of
///    valid `CStr` bytes (i.e. the byte sequence ends with a `\0` that
///    `CStr::from_ptr` can find).
/// 2. The memory referenced by `ptr` is not mutated by any other
///    thread for the duration of the returned reference's lifetime.
/// 3. `ptr` outlives `'a`.
pub unsafe fn c_cstr<'a>(ptr: *const core::ffi::c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller's contract (see docstring) — ptr non-null,
    // NUL-terminated, non-mutating, outlives `'a`.
    let cstr = unsafe { core::ffi::CStr::from_ptr(ptr) };
    cstr.to_str().ok()
}

/// Cast a caller-owned C handle pointer `(ptr)` into an optional Rust
/// reference. Returns `None` when the pointer is null.
///
/// # Safety
///
/// The caller must guarantee that:
///
/// 1. `ptr` is either null OR points to a valid, initialized `T`.
/// 2. The memory referenced by `ptr` is not mutated by any other
///    thread for the duration of the returned reference's lifetime.
/// 3. `ptr` outlives `'a`.
pub unsafe fn c_ref<'a, T>(ptr: *const T) -> Option<&'a T> {
    // SAFETY: caller's contract (see docstring) — ptr null OR
    // points to a valid `T`; `as_ref` returns `None` on null.
    unsafe { ptr.as_ref() }
}

/// Write `data` into a caller-owned C output buffer `(out_buf, out_len)`.
/// Returns bytes written on success, or `-1` when the buffer is null
/// or too small. Matches the shape leyline-fs / leyline-sign / cas-ffi
/// all use for their FFI return convention.
///
/// # Safety
///
/// The caller must guarantee that:
///
/// 1. `out_buf` is either null OR writable for exactly `out_len` bytes.
/// 2. `out_buf` (if non-null) is not aliased by any other reference
///    for the duration of this call.
/// 3. `out_buf` outlives this call.
///
/// A `null` `out_buf` returns `-1` without touching memory.
pub unsafe fn c_output(data: &[u8], out_buf: *mut u8, out_len: usize) -> i32 {
    if out_buf.is_null() || data.len() > out_len {
        return -1;
    }
    // SAFETY: caller's contract (see docstring) — out_buf non-null +
    // writable for out_len bytes (>= data.len(), checked); data slice
    // guaranteed valid by Rust's `&[u8]` type. Non-overlapping is
    // trivially satisfied — `data` is a Rust slice, `out_buf` is a
    // C-owned buffer with no aliasing by contract.
    unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), out_buf, data.len()) };
    data.len() as i32
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use core::ptr;

    #[test]
    fn c_input_null_pointer_returns_none() {
        // SAFETY: null-ptr path never dereferences.
        let out = unsafe { c_input(ptr::null(), 0) };
        assert!(out.is_none());
        // SAFETY: null-ptr path never dereferences even with non-zero len.
        let out = unsafe { c_input(ptr::null(), 42) };
        assert!(out.is_none());
    }

    #[test]
    fn c_input_valid_pointer_returns_slice() {
        let buf = [1u8, 2, 3, 4];
        // SAFETY: buf lives past the borrow; we read a valid, non-null slice.
        let out = unsafe { c_input(buf.as_ptr(), buf.len()) };
        assert_eq!(out, Some(&buf[..]));
    }

    #[test]
    fn c_input_zero_len_returns_empty_slice() {
        let buf = [1u8];
        // SAFETY: ptr is valid, len=0 → empty slice.
        let out = unsafe { c_input(buf.as_ptr(), 0) };
        assert_eq!(out, Some(&[][..]));
    }

    #[test]
    fn c_output_null_buffer_returns_neg_one() {
        // SAFETY: null-buf path never dereferences.
        let rc = unsafe { c_output(b"data", ptr::null_mut(), 4) };
        assert_eq!(rc, -1);
    }

    #[test]
    fn c_output_buffer_too_small_returns_neg_one() {
        let mut buf = [0u8; 2];
        // SAFETY: writable for 2 bytes, but data.len()=4 > 2 → returns -1 without writing.
        let rc = unsafe { c_output(b"data", buf.as_mut_ptr(), 2) };
        assert_eq!(rc, -1);
        // Not written to.
        assert_eq!(buf, [0, 0]);
    }

    #[test]
    fn c_output_success_writes_bytes_and_returns_count() {
        let mut buf = [0u8; 8];
        // SAFETY: buf is writable for 8 bytes; data.len()=4 fits.
        let rc = unsafe { c_output(b"data", buf.as_mut_ptr(), buf.len()) };
        assert_eq!(rc, 4);
        assert_eq!(&buf[..4], b"data");
    }

    #[test]
    fn c_output_empty_data_returns_zero_no_write() {
        let mut buf = [0xFFu8; 4];
        // SAFETY: buf is writable for 4 bytes; empty data writes nothing.
        let rc = unsafe { c_output(&[], buf.as_mut_ptr(), buf.len()) };
        assert_eq!(rc, 0);
        assert_eq!(buf, [0xFF; 4]);
    }

    // ── c_cstr ─────────────────────────────────────────────────────

    #[test]
    fn c_cstr_null_returns_none() {
        // SAFETY: null-ptr path never dereferences.
        let out = unsafe { c_cstr(core::ptr::null()) };
        assert!(out.is_none());
    }

    #[test]
    fn c_cstr_valid_returns_str() {
        let cs = std::ffi::CString::new("hello").unwrap();
        // SAFETY: cs lives past the borrow; well-formed NUL-terminated bytes.
        let out = unsafe { c_cstr(cs.as_ptr()) };
        assert_eq!(out, Some("hello"));
    }

    #[test]
    fn c_cstr_invalid_utf8_returns_none() {
        let bytes: &[u8] = &[0xFF, 0xFE, 0x00];
        // SAFETY: bytes lives past the borrow; ends in NUL. UTF-8 will fail.
        let out = unsafe { c_cstr(bytes.as_ptr() as *const core::ffi::c_char) };
        assert!(out.is_none());
    }

    // ── c_ref ──────────────────────────────────────────────────────

    #[test]
    fn c_ref_null_returns_none() {
        // SAFETY: null-ptr path never dereferences.
        let out: Option<&u32> = unsafe { c_ref(core::ptr::null()) };
        assert!(out.is_none());
    }

    #[test]
    fn c_ref_valid_returns_ref() {
        let x: u32 = 42;
        // SAFETY: x outlives the borrow; ptr is valid + initialized.
        let out: Option<&u32> = unsafe { c_ref(&x as *const u32) };
        assert_eq!(out, Some(&42));
    }
}
