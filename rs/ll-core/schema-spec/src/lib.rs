//! Vendor-neutral capability specifications for the ley-line substrate.
//!
//! This crate is a *spec-artifact holder*, not a Rust API. It ships the
//! `_traits.capnp` trait library, `_capability-mapping.md`, `_traits.md`,
//! `LAYOUT.md`, and per-capability spec directories (`credential-isolation/v1/`,
//! `build-cache/v1/`, `mcp-tool/v1/`) verbatim from their original home under
//! `cloister/cloister-spec/`. See the crate README for the file inventory.
//!
//! The crate's one load-bearing runtime behaviour is the
//! `verify_vectors_sha256` test: it walks every `VECTORS.sha256` file the
//! specs pin and asserts SHA-256 equality against the vector bytes on disk.
//! Any drift between pinned digests and committed vectors fails
//! `cargo test -p leyline-schema-spec`.
//!
//! # Layout
//!
//! Files ship as siblings of `src/lib.rs`, not inside it:
//!
//! ```text
//! rs/ll-core/schema-spec/
//! ├── _traits.capnp
//! ├── _capability-mapping.md
//! ├── _traits.md
//! ├── LAYOUT.md
//! ├── credential-isolation/v1/     (README, QUICKSTART, wire/, test-vectors/, ref-impl-py/, VECTORS.sha256)
//! ├── build-cache/v1/              (README, wire/, vectors/ with VECTORS.sha256)
//! └── mcp-tool/v1/                 (README, wire/, vectors/)
//! ```
//!
//! # Rust API
//!
//! There is none yet. Future beads may fold in generated capnp bindings
//! from `_traits.capnp` or emit typed rustdoc for each capability; those
//! are out of scope for the move.

/// Root of the schema-spec directory tree, evaluated at compile time.
///
/// Consumers that want to reach a spec file at runtime can construct a
/// path relative to this. Prefer the tests in this crate for validating
/// spec artifacts — this constant is a courtesy for tooling.
pub const SPEC_DIR: &str = env!("CARGO_MANIFEST_DIR");
