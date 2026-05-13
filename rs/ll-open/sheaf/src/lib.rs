//! # leyline-sheaf
//!
//! Domain-independent Čech cohomology engine for structural analysis, cache
//! invalidation, and delta synchronization.
//!
//! ## Core abstractions
//!
//! - [`CellComplex`]: Cochain complex with 0-cells (nodes), 1-cells (edges),
//!   2-cells (faces), restriction maps, and coboundary operators δ⁰, δ¹.
//! - [`SheafCache`]: Cache whose invalidation is driven by the coboundary
//!   operator d⁰ — only structurally affected entries are evicted.
//!
//! ## Mathematical foundation
//!
//! A **sheaf** assigns data (stalks) to topological regions and enforces
//! consistency across boundaries via restriction maps. The coboundary
//! operator δ⁰: C⁰ → C¹ measures disagreement between adjacent stalks.
//! Entries in ker(δ⁰) — the zeroth cohomology group H⁰ — are globally
//! consistent and remain cached. Everything outside H⁰ is invalidated.
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
