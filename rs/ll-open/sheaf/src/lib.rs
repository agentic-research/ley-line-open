//! # leyline-sheaf
//!
//! Domain-independent Čech cohomology engine for structural analysis, cache
//! invalidation, and delta synchronization.
//!
//! ## Core abstractions
//!
//! - [`CellComplex`]: Cochain complex with 0-cells (nodes), 1-cells (edges),
//!   2-cells (faces), restriction maps, and coboundary operators δ⁰, δ¹.
//! - [`SheafCache`]: Structurally-aware cache. Invalidation today is driven
//!   by an XOR-Merkle proxy plus a bounded restriction-graph BFS — a fast
//!   heuristic shaped by the sheaf, not the literal δ⁰ output. The crate's
//!   defect metric `Σ‖δ⁰‖²` (real sheaf invariant) drives cache *health*
//!   monitoring; promoting the cache to real δ⁰-driven invalidation requires
//!   wiring [`CellComplex::detect_violations`] into [`SheafCache::on_change`].
//!   See [`cache`] module docs for the explicit contract.
//!
//!   The list returned by [`SheafCache::on_change`] always contains the
//!   `changed_regions` the caller passed in (the cascade roots), plus any
//!   BFS-reachable neighbors whose boundary projection moved beyond
//!   `DELTA0_EPS` in norm space (δ⁰ mode) or whose XOR pre-filter fired
//!   (heuristic-only). The cascade roots appear in the list even when their
//!   own boundary is unchanged — they are an input fact the caller
//!   asserted, not a measurement. It is a structural answer about the
//!   sheaf section, not a per-cache eviction list — UDS / MCP consumers
//!   get the same answer in-process callers do and own their own eviction
//!   policy on top of it.
//!
//! ## Mathematical foundation
//!
//! A **sheaf** assigns data (stalks) to topological regions and enforces
//! consistency across boundaries via restriction maps. The coboundary
//! operator δ⁰: C⁰ → C¹ measures disagreement between adjacent stalks; the
//! defect `‖δ⁰(stalks)‖²` is a real H⁰ distance metric and the load-bearing
//! "sheaf-derived" quantity this crate exports. Entries in `ker(δ⁰)` — the
//! zeroth cohomology group H⁰ — are globally consistent.
//!
//! For the dimension of H⁰ as an algebraic invariant (independent of the
//! current section), use [`CellComplex::h0_dimension`].
//! [`CellComplex::consistency_analysis`] returns a section-dependent
//! partition + defect — useful as a cache heuristic, not literal H⁰.
//!
//! ## Provenance
//!
//! Lifted from the private `ley-line` repo into ley-line-open (AGPL-3)
//! on 2026-05-13 (bead `ley-line-open-ae7a35`). The crate is
//! domain-independent — it carries no LLO-specific assumptions; LLO
//! consumes it as a math library through the daemon's enrichment +
//! cache surfaces.

pub mod cache;
pub mod complex;
pub mod learn;
pub mod merkle;
pub mod sparse;
pub mod topology;

pub use cache::{CacheEntry, RestrictionEdge, SheafCache};
pub use complex::{Cell, Cell2, CellComplex, RestrictionMap, Stalk, Violation};
pub use learn::CoChangeTracker;
pub use merkle::{compute_merkle_root, hash_node};
pub use sparse::SparseOps;
pub use topology::{RegionId, RestrictionGraph};
