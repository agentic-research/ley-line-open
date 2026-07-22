pub mod cert;
pub mod cert_chain;
pub mod cms;
pub mod error;
pub mod ffi;
// ADR-012 canonical key identifier. Not feature-gated: the derivation and its
// shape gate are needed on both the signing and the (wasm) verifying side, and
// depend only on unconditional deps (ed25519-dalek, sha2, hex).
pub mod kid;
pub mod oid;

// Concrete Ed25519 `RootSigner` over Σ roots (workstream S1). Gated on the
// `root-signer` feature only — the dependency is `optional`, so the default
// wasm artifact stays byte-identical when the feature is off. Unlike `host`,
// this is NOT excluded on wasm32: `leyline-core` and `ed25519-dalek` both
// build for wasm32, and browser-side verification of a signed head is a
// wanted capability, not an accident.
#[cfg(feature = "root-signer")]
pub mod root_signer;

// Host-only sign-only helper (ADR-0019, absorbed from cloister's fork
// under bead ley-line-open-7226e3). Gated on both the `host` Cargo
// feature AND `not(target_arch = "wasm32")` so the wasm verifier path
// stays byte-identical when the feature toggles.
#[cfg(all(feature = "host", not(target_arch = "wasm32")))]
pub mod host;
