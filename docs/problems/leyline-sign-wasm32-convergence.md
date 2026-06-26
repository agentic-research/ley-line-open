# leyline-sign — wasm32 convergence with cloister's vendored fork

**Status:** ✅ **Shipped 2026-06-25 in v0.5.2** — PRs #115 (`cert_chain` + `lsign_alloc`/`free`), #116 (signingTime omission). Cloister can now de-vendor; see bead `5a06e9` close-comment for the cloister-side path.
**Filed:** 2026-06-25
**Filed-from:** `~/remotes/art/cloister/` audit `cloister-59c60e` (2026-06-24); see also `cloister/docs/adr/0035-cloister-llo-boundary.md`
**Beads:** `ley-line-open-4a9e5a` (cert_chain, closed), `ley-line-open-4ad9da` (wasm32 FFI, closed), `ley-line-open-5a06e9` (host-feature evaluation, closed as recommend-against — host/* stays cloister-side as `cloister-sign-host`).

## Context

cloister carries a vendored copy of `leyline-sign` at `~/remotes/art/cloister/rs/crates/sign/src/`. The 2026-06-24 per-file audit confirmed nothing has silently drifted — every byte of difference is either documented lift metadata (license header from the 2026-05-09 Apache-2.0 → AGPL-3.0 lift) or **intentional cloister-only additions documented inline that are waiting to be PR'd back here**.

`cloister` wants to converge: consume `leyline-sign` from LLO as a git dep (same pattern as `leyline-cas-ffi` per `cloister-713b4e`) and delete the vendored copy. The blocker is three small leyline-sign additions needed for wasm32 consumption — they live in cloister's fork today but belong here.

Until they land here, cloister carries a duplicate AGPL fork. The audit had to read 1270 lines of Rust to confirm "nothing drifted"; that work happens every time someone wonders if the forks are aligned.

## PR 1 — `lsign_alloc` / `lsign_free` wasm32 allocator exports

**File:** `rs/ll-open/sign/src/ffi.rs`
**Size:** ~30 lines + safety docs
**Acceptance:** new `pub extern "C" fn lsign_alloc(size: usize) -> *mut u8` and `pub unsafe extern "C" fn lsign_free(ptr: *mut u8, size: usize)` exports; wasm32 callers can manage linear memory directly (allocate via `lsign_alloc`, copy inputs in, call the FFI function, read output bytes, free via `lsign_free`).

The cloister copy (canonical reference for the patch):

```rust
// ── wasm32 memory management exports ────────────────────────────────────
//
// Without these, a wasm32 consumer has no way to pass byte buffers to the
// verifier — wasm linear memory is opaque to JS without explicit
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
/// same `size`. Double-free or mismatched-size free is undefined behavior.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lsign_free(ptr: *mut u8, size: usize) {
    if !ptr.is_null() && size > 0 {
        unsafe { drop(Vec::from_raw_parts(ptr, 0, size)) };
    }
}
```

**Gating:** these are safe to unconditionally export — native builds (cdylib via cbindgen) get them too, harmless. No feature flag needed.

**Tests:** wasm32 integration test asserting alloc/free roundtrips a byte buffer without UAF (cloister's TS-side wrapper at `cloister/src/wire/signet-verify.ts` is the consumer; covering test exists in cloister).

## PR 2 — `signingTime` omission gated on a Cargo feature

**File:** `rs/ll-open/sign/src/cms.rs`
**Size:** ~20 lines + 30-line regression test
**Acceptance:** new Cargo feature (recommended name: `wasm32-no-signing-time` or just `no-signing-time`) which, when enabled, drops the `signingTime` attribute from CMS SignedAttributes. Default-off → existing LLO consumers see no behavior change. Cloister enables the feature in its `Cargo.toml` for the wasm32 build.

**Why:** wasm32 has no portable host-independent time source. Emitting a fixed-time placeholder across hosts silently collapses the temporal-binding property of any verifier trusting `signingTime`. RFC 5652 §5.3 lists `signingTime` as useful-but-unauthenticated, so omission is spec-legal. Cloister's `ADR-0007` (Interlace) binds temporal context via `cert.not_before` / `not_after` (signed by the master at mint time) and the attestation row's server-timestamped `created_at` instead, so the omission is observed-safe in cloister's deployment.

The cloister copy (canonical reference; un-gate to a feature):

```rust
/// Sign data using CMS/PKCS#7 with Ed25519 and signed attributes (RFC 5652 + RFC 8419).
///
/// Produces a detached CMS signature with contentType and messageDigest signed
/// attributes. The signature is over the DER-encoded SET OF attributes.
///
/// `signingTime` is **omitted by design** when the `no-signing-time` feature
/// is enabled (cloister's wasm32 build). RFC 5652 §5.3 lists signingTime as
/// useful-but-unauthenticated, so omission is spec-legal. The reason: wasm32
/// has no portable host-independent time source; a fixed-time placeholder
/// across hosts would silently collapse the temporal-binding property of any
/// verifier trusting signingTime. Bind temporal context elsewhere (cert
/// not_before / not_after; server-timestamped audit row) instead.
pub fn sign_data(...) -> Result<...> {
    // ...
    let content_type_attr = build_attribute(&oid::ID_CONTENT_TYPE, &encode_oid(&oid::ID_DATA));
    let message_digest_attr =
        build_attribute(&oid::ID_MESSAGE_DIGEST, &encode_octet_string(&digest));

    #[cfg(not(feature = "no-signing-time"))]
    let signing_time_attr = build_attribute(&oid::ID_SIGNING_TIME, &encode_utc_time_now());

    #[cfg(not(feature = "no-signing-time"))]
    let mut attrs = vec![content_type_attr, message_digest_attr, signing_time_attr];
    #[cfg(feature = "no-signing-time")]
    let mut attrs = vec![content_type_attr, message_digest_attr];
    // ...
}
```

Cloister's regression test pinning the omission:

```rust
#[test]
#[cfg(feature = "no-signing-time")]
fn signed_attributes_omits_signing_time() {
    let (cert_der, key) = generate_test_cert_and_key();
    let cms_sig = sign_data(b"omits-signing-time", &cert_der, &key).unwrap();
    let (_oid, sd_bytes) = parse_content_info(&cms_sig).unwrap();
    let sd = parse_signed_data(&sd_bytes).unwrap();
    let si = &sd.signer_infos[0];
    assert_eq!(si.signed_attributes_parsed.len(), 2, "expected 2 attrs (contentType + messageDigest); got {}", si.signed_attributes_parsed.len());
    // ... (assert no signingTime OID present)
}
```

**Cargo.toml addition:**

```toml
[features]
default = []
no-signing-time = []
```

## PR 3 — wasm32-consumption module doc

**File:** `rs/ll-open/sign/src/lib.rs` (and/or `rs/ll-open/sign/src/ffi.rs`)
**Size:** ~10-15 lines of `//!` doc-comment
**Acceptance:** module-level doc explaining how wasm32 consumers (workerd, browsers, WASI hosts) use the FFI exports. References PR 1 (allocator exports) + PR 2 (feature flag).

Suggested text (from cloister's vendored copy):

```rust
//!
//! ## wasm32 consumption
//!
//! The same FFI exports work for both native (cdylib via cbindgen) and
//! wasm32 (workerd / browsers / WASI hosts) consumers. wasm32 callers
//! manage linear memory directly: allocate via `lsign_alloc`, copy inputs
//! in, call the FFI function, read output bytes, free via `lsign_free`.
//! Same calling convention; pointers become 32-bit indices into wasm
//! linear memory.
//!
//! For wasm32 builds that need to drop the CMS `signingTime` attribute
//! (because there's no portable host-independent time source), enable
//! the `no-signing-time` Cargo feature. See the `sign_data` doc for the
//! rationale.
```

## After all three land + LLO releases a version containing them

cloister-side work (cloister-204ac9 / cloister-818f2b will track):

1. Bump `Cargo.toml` / `cluster.toml` LLO pin to the release.
2. Delete `cloister/rs/crates/sign/src/{cert,cms,error,ffi,lib,oid}.rs`.
3. Replace with `pub use leyline_sign::*;` re-export (or direct imports per call site).
4. Keep `cloister/rs/crates/sign/src/{cert_chain.rs, host/, bin/}` — those are cloister-specific bridge crates (ADR-0007 lease chain + ADR-0019 helper). They don't move.
5. Optional follow-up: rename `cloister/rs/crates/sign/` → `cloister/rs/crates/cloister-sign/` to make the post-consolidation scope unambiguous.
6. Update cloister's threat model §15 + ADR-0019 references to point at LLO's canonical leyline-sign.

## Out of scope for these PRs

- Other leyline-* crates beyond `leyline-sign`. This is the only vendored fork today; if another lands, the same convergence pattern applies.
- Bidirectional code flow (cloister → LLO). `cert_chain.rs`, `host/`, `bin/helper.rs` are cloister bridge code; LLO has no use for them.
- Relicense back to Apache-2.0. LLO's NOTICE documents the Apache → AGPL lift; not in scope to reverse.

## References

- `cloister/docs/adr/0035-cloister-llo-boundary.md` — the ADR ratifying this convergence principle.
- `cloister-59c60e` — the cloister-side tracking bead (re-titled 2026-06-24 from "drift" framing to "convergence plan").
- `cloister-713b4e` — the precedent migration (`leyline-cas-ffi` consumed from LLO as a git dep).
- `rs/ll-open/sign/NOTICE` — license relationship (Apache-2.0 → AGPL-3.0 lift, 2026-05-09).
- cloister's vendored copies at `~/remotes/art/cloister/rs/crates/sign/src/{ffi,cms,lib}.rs` — canonical reference for the patch content.
