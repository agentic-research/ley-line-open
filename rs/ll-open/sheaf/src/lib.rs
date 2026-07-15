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
//! ## What's load-bearing vs what's a proxy
//!
//! Read this before the code — it answers "is Čech cohomology
//! load-bearing, or résumé-driven math?"
//!
//! **Load-bearing (real signal, wire-checked).** The defect metric
//! `Σ‖δ⁰(stalks)‖²` is a genuine H⁰ distance — the coboundary operator
//! δ⁰ is the actual sheaf-cohomology invariant and drives the
//! per-cache-entry health metric that `daemon.sheaf.health` exports.
//! [`CellComplex::h0_dimension`] computes the algebraic H⁰ dimension
//! (independent of the current section) and is used by the ADR-0020
//! agreement op.
//!
//! **Proxy for the wire path.** `SheafCache::on_change` uses an
//! XOR-Merkle pre-filter + a bounded restriction-graph BFS as its
//! fast path; the δ⁰-driven eviction is implemented but gated on the
//! ADR-0020 falsifiability pass. The XOR pre-filter is honest — it's
//! shaped by the sheaf but is not the literal δ⁰ output. The health
//! metric (real δ⁰) runs in parallel so we can tell when the proxy
//! diverges from truth.
//!
//! ## Kill criteria
//!
//! Falsification suite lives at `tests/falsifiability_gates.rs`
//! (already shipping). Two invariants guard the load-bearing claims:
//!
//! - **Claim 1**: `CellComplex::detect_violations` returns exactly the
//!   entries whose stalk change moves the boundary projection past
//!   `DELTA0_EPS` in norm space.
//! - **Claim 2**: `SheafCache::on_change` invalidates only
//!   restriction-graph-reachable entries (no false positives, no
//!   silent under-eviction on graph-transitive changes).
//!
//! Runtime falsification harness: the `sheaf_ablation` daemon op logs
//! per-invalidation deltas; `docs/research/sheaf-ablation-study.md`
//! documents the ablation methodology and the 91× reframing — a
//! negative result made into a positive claim (the correct
//! kill-criteria shape). ADR-0020 Gate 3 uses the `test-spies`
//! feature's atomic reach-counter to falsify mechanical-reach claims
//! on the `agreement` op.
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
