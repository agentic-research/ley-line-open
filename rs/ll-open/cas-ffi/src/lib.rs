//! Wasm32-callable FFI for ll-core CAS primitives.
//!
//! Bead `ley-line-open-0c8c0b`. Mirrors `leyline-sign` (ADR-0019) as the
//! existing precedent for "Rust substrate primitives compiled to wasm32
//! and consumed by cloister via the workerd cdylib loader." Cloister's
//! pre-this-bead implementation re-hashed in TypeScript via `@noble/hashes`
//! — that drifted off the substrate's BLAKE3 lock (Σ §3.4, enforced by
//! `leyline-core::ContentAddressed for [u8]`). This crate gives the
//! consumer side a single way to compute the substrate hash.
//!
//! ## Scope
//!
//! v0.1 exposes one FFI function: `leyline_hash_bytes`. SHA-256 is
//! intentionally NOT here — Web Crypto / OpenSSL / etc. are
//! hardware-accelerated everywhere this FFI runs; pulling SHA-256
//! through wasm32 would be slower without buying any substrate
//! invariant (SHA-256 is OCI ecosystem compat, not a substrate
//! commitment).
//!
//! Future additions (separate beads): combined `verify_claimed_digest`
//! once a consumer needs it composed substrate-side; streaming hash
//! once a consumer needs to hash incrementally without buffering.
//!
//! ## Safety
//!
//! Every public FFI function documents its own safety contract on the
//! pointer + length arguments. Internally we use
//! `std::slice::from_raw_parts` which is unsound when called with
//! aliased mutable pointers — callers MUST NOT pass overlapping
//! input/output buffers. The lib tests cover the well-behaved-caller
//! path; the FFI's safety story rests on documented contracts the
//! consumer (cloister) is expected to honor.

pub mod ffi;

#[cfg(test)]
mod tests {
    use leyline_core::substrate::ContentAddressed;

    #[test]
    fn empty_input_hash_matches_blake3_of_empty() {
        let bytes: &[u8] = &[];
        let h_via_trait = bytes.hash();
        let h_via_blake3 = blake3::hash(bytes);
        assert_eq!(h_via_trait.as_bytes(), h_via_blake3.as_bytes());
    }

    #[test]
    fn known_input_hash_is_deterministic() {
        let bytes: &[u8] = b"hello cas-ffi";
        let h1 = bytes.hash();
        let h2 = bytes.hash();
        assert_eq!(h1.as_bytes(), h2.as_bytes());
    }

    #[test]
    fn distinct_inputs_produce_distinct_hashes() {
        let a: &[u8] = b"alpha";
        let b: &[u8] = b"beta";
        assert_ne!(a.hash().as_bytes(), b.hash().as_bytes());
    }
}
