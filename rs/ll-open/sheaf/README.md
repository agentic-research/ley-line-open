# leyline-sheaf

Čech-cohomology engine for structural analysis, cache invalidation, and delta synchronization. Domain-independent — the substrate consumers (LLO daemon, mache, cloister) wire their own stalks + restriction maps into the abstractions defined here.

## Core abstractions

- **`CellComplex`** — cochain complex with 0-cells (nodes), 1-cells (edges), 2-cells (faces), restriction maps, and coboundary operators δ⁰ and δ¹.
- **`SheafCache`** — structurally-aware cache. Today's invalidation is driven by an XOR-Merkle proxy plus a bounded restriction-graph BFS — a fast heuristic *shaped by* the sheaf, not the literal δ⁰ output. The defect metric `Σ‖δ⁰‖²` (real sheaf invariant) drives cache *health* monitoring; promoting the cache to real δ⁰-driven invalidation requires wiring `CellComplex::detect_violations` into `SheafCache::on_change`. See the `cache` module docs for the explicit contract.
- **`CellComplex::h0_dimension`** — the algebraic dimension of H⁰ (independent of the current section). For section-dependent consistency analysis, use `CellComplex::consistency_analysis`.

## What `on_change` returns

The list returned by `SheafCache::on_change` always contains:

1. The `changed_regions` the caller passed in (cascade roots — an input fact, not a measurement).
2. BFS-reachable neighbors whose boundary projection moved beyond `DELTA0_EPS` in norm space (δ⁰ mode) OR whose XOR pre-filter fired (heuristic-only mode).

It is a structural answer about the sheaf section, not a per-cache eviction list. UDS / MCP consumers get the same answer in-process callers do and own their own eviction policy on top of it.

## Mathematical foundation

A **sheaf** assigns data (stalks) to topological regions and enforces consistency across boundaries via restriction maps. The coboundary operator δ⁰: C⁰ → C¹ measures disagreement between adjacent stalks; the defect `‖δ⁰(stalks)‖²` is a real H⁰ distance metric — the load-bearing "sheaf-derived" quantity this crate exports. Entries in `ker(δ⁰)` — the zeroth cohomology group H⁰ — are globally consistent.

## ADR-0020 — entity-observation lattice

The sheaf is the structural backbone for ADR-0020 (entity-observation lattice). The crate's `feature = "test-spies"` adds an atomic counter inside `CellComplex::detect_violations` so the L10 `agreement` op Gate 3 test in `leyline-cli-lib` can falsify mechanical-reach claims without leaking into release builds.

## Used by

- **`leyline-cli-lib`** — daemon-side cache + L10 agreement op (ADR-0020).
- **`leyline-hdc`** — HDC-stalked structural sections via `HvCell`.
- **mache** — receives `sheaf.invalidate` events from the daemon; routes via the in-process sheaf router (`SheafSubscriber`).

## Status

The δ⁰-driven invalidation path is implemented but not yet the production wire — current `SheafCache::on_change` uses the XOR pre-filter for the fast path with the BFS bound as the safety net. The defect-monitoring path runs in parallel and feeds the health metric. Promotion to δ⁰-driven invalidation is gated on the falsifiability tests in `leyline-cli-lib::tests` for ADR-0020.

## Sub-components and their risk profiles

"The sheaf cache" is not one thing. Read [ADR-0030](../../../docs/adr/0030-sheaf-over-embeddings.md) and [ADR-0031](../../../docs/adr/0031-restriction-addressed-review-caching.md) before assuming otherwise:

| Module | Status | Wired into the daemon? |
|---|---|---|
| `cache.rs` / `merkle.rs` — the hash-gated invalidation BFS | **Load-bearing, live** (ADR-0030 addendum) | Yes — `daemon/sheaf_ops.rs` consumes `SheafCache::on_change`/`reap` directly |
| `complex.rs` — δ⁰/Čech cohomology, weighted restriction maps | **Live but diagnostic-only** (ADR-0030 verdict: NO-GO on using it for invalidation) | Yes, but only feeds the health metric + `agreement`/`h0_dimension` ops — confirmed off the invalidation path; `RestrictionMap::compose`/`weighted`/`new` have zero non-test callers |
| `restriction_review.rs` — the ADR-0031 toy comparison | **Experiment only** | No — exercised by `tests/restriction_review_toy.rs` |

The `restriction_cache.rs` module (ADR-0031 restriction-addressed derived-view caching) was removed in bead `ley-line-open-afa952`. It had no caller outside its own tests and bench, and its key was ambiguous: unit-separator framing that POSIX paths can contain, and no `parserId` binding (finding `80f037` F5). A future implementation keys length-prefixed fields with `parserId` bound; see ADR-0031 "Removal".

`cache.rs` is the piece most likely under-covered relative to its actual correctness stakes.
