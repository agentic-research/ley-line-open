pub mod cert;
pub mod cert_chain;
pub mod cms;
pub mod error;
pub mod ffi;
pub mod oid;

// Host-only sign-only helper (ADR-0019, absorbed from cloister's fork
// under bead ley-line-open-7226e3). Gated on both the `host` Cargo
// feature AND `not(target_arch = "wasm32")` so the wasm verifier path
// stays byte-identical when the feature toggles.
#[cfg(all(feature = "host", not(target_arch = "wasm32")))]
pub mod host;
