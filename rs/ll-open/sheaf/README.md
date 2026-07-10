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
