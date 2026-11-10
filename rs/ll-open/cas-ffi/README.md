# leyline-cas-ffi

Wasm32-callable C FFI for ll-core CAS (content-addressed storage) primitives. The substrate's BLAKE3 hash and its keyed `derive_key` primitive, exposed to non-Rust consumers (cloister via workerd's cdylib loader; future Swift / TS hosts).

**License:** AGPL-3.0-or-later. **Bead:** `ley-line-open-0c8c0b`.

## Why this exists

ley-line's Σ Merkle-CAS substrate (§3.4) locks on BLAKE3 for all content-addressing. Before this crate, cloister re-hashed in TypeScript via `@noble/hashes` — a drift surface against the substrate invariant. This crate gives non-Rust consumers a single way to compute the substrate hash, ensuring the wire-level digest matches what Rust-side `leyline_core::substrate::ContentAddressed` would produce.

Mirrors `leyline-sign` (ADR-0019) as the precedent pattern: "Rust substrate primitives compiled to wasm32 and consumed by cloister via workerd."

## What's here

- **`ffi`** — C-ABI functions, declared in [`include/leyline_cas_ffi.h`](include/leyline_cas_ffi.h) (cbindgen output; `tests/c_header.rs` compiles a probe against it):
  - `leyline_hash_bytes(in_ptr, in_len, out_buf, out_len) -> i32` — σ_bytes, BLAKE3 of the input.
  - `leyline_derive_key(ctx_ptr, ctx_len, in_ptr, in_len, out_buf, out_len) -> i32` — BLAKE3 `derive_key(context, input)`, the keyed primitive every keyed Σ construction is built on. The context is UTF-8, not NUL-terminated, and must be non-empty. The prerequisite of run-id v2 (ADR-0047 D-i): cloister derives `run_id` through it. Pinned by the `deriveKey` cases of `ll-core/schema-spec/sigma/v1/test-vectors/constructions.json` (BLAKE3's own test vectors). Bead `ley-line-open-12e252`.

  Both write 32 bytes and return `32`, or return `-1` (writing nothing) on a null pointer, a refused argument, or `out_len < 32`.
- **Crate-type triple** — `cdylib` (wasm32 + dlopen from TS), `staticlib` (Swift/C consumers), `rlib` (in-workspace Rust composition). Matches `leyline-sign/Cargo.toml`.

## Out of scope

SHA-256 is deliberately NOT here. Web Crypto / OpenSSL / equivalent provide hardware-accelerated SHA-256 everywhere this FFI runs; pulling SHA-256 through wasm32 would be slower without buying any substrate invariant (SHA-256 is OCI ecosystem compat, not a substrate commitment).

Future additions (separate beads): `verify_claimed_digest` for substrate-side composition; streaming-hash for incremental hashing without buffering.

## Safety

Each public FFI function documents its own safety contract on pointer + length arguments. Internally uses `std::slice::from_raw_parts` which is unsound under aliased mutable pointers — callers MUST NOT pass overlapping input/output buffers. Crate tests cover the well-behaved-caller path; the FFI's safety story rests on documented contracts the consumer (cloister) is expected to honor.

## Used by

- **cloister** — wasm32 bridge crate (`cloister/rs/crates/cas`, which depends on this) emits `cloister_cas.wasm` for the workerd bundle pipeline.
