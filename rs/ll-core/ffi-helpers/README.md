# leyline-ffi-helpers

Typed helpers for the C-boundary raw-pointer pattern used by every LLO `extern "C" fn` (bead `ley-line-open-85fb1f` PR 2).

## The problem this solves

Every `extern "C" fn` in LLO that takes `(*const u8, usize)` input buffers or `(*mut u8, usize)` output buffers repeats the same shape:

```text
if ptr.is_null() { return -1; }
let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
// ... use slice ...
unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), out_buf, data.len()) }
```

This crate consolidates the invariant into a handful of functions so the SAFETY docstring lives in one place instead of being duplicated (or silently absent) at every call site.

## What's here

- **`c_input`** — `unsafe fn(*const u8, usize) -> Option<&[u8]>`. Null-checked, zero-length-safe.
- **`c_cstr`** — `unsafe fn(*const c_char) -> Option<&str>`. Null-checked C-string-to-`&str` with UTF-8 validation.
- **`c_ref`** — `unsafe fn(*const T) -> Option<&T>`. Null-checked typed pointer dereference.
- **`c_output`** — `unsafe fn(&[u8], *mut u8, usize) -> i32`. Bounds-checked copy into a caller-owned output buffer.

All four are still `unsafe fn` — their inputs are C-owned raw pointers — but a single `unsafe { }` block per outer FFI export can now cover every input read via one audited implementation, rather than each export re-deriving the SAFETY argument.

## Used by

- **`leyline-cas-ffi`**, **`leyline-envelope`**, **`leyline-fs`**, **`leyline-sign`** — every crate exposing a C ABI surface.
