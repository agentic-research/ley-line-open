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

## `SheafCache` is advisory

`SheafCache` is advisory: a region it keeps is a candidate for reuse, not a valid value, and its keep decision must not gate reuse. Validity belongs to a key over the facts the derived value reads, such as `leyline-cache`'s `RestrictionKey` (ADR-0031).

`task cache:ablation` (the `sheaf_restriction_replay` bench) replays 60 commits ending at e4690505: 10,751 call-target review lookups, 617 of them with a changed value (bead `ley-line-open-f04610`).

| arm | hit rate | false skips (stale value served) | false misses |
|---|---|---|---|
| `RestrictionKey` (`leyline-cache`) | 94.0% | 0 | 32 |
| `SheafCache`, heuristic | 67.9% | 154 | 2,990 |
| `SheafCache`, δ⁰ (interface digest in the agreement coordinates) | 79.3% | 201 | 1,808 |

Every false skip is a region whose own bytes did not change and whose value moved through a fact outside them: a def of a called token appearing or disappearing elsewhere in the corpus, or an import edit in the same file. The restriction graph holds no edge to either fact, so neither mode can see it. δ⁰ prunes the heuristic's over-reach (1,182 fewer false misses) and with it the 47 regions the over-reach had evicted by coincidence. `tests/sheaf_cache_advisory.rs` pins one such case, shrunk from the replay, and asserts this paragraph's first sentence in both this README and the `cache` module doc.

No shipped consumer serves a value because `SheafCache` kept it. The daemon's cache is `SheafCache<HashStalk, ()>` and holds no derived value; `sheaf_invalidate` returns regions to recompute; mache evicts per-node content for the returned regions on top of the edited file's nodes it re-ingests, so an unlisted region keeps only content derived from its own unchanged bytes. The cascade is an eviction hint: it can widen an eviction, never justify a reuse.

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
| `cache.rs` / `merkle.rs` — the hash-gated invalidation BFS | **Live, advisory** (ADR-0030 addendum; see "`SheafCache` is advisory" above) | Yes — `daemon/sheaf_ops.rs` consumes `SheafCache::on_change`/`reap` directly, as eviction hints over a `()`-valued cache |
| `complex.rs` — δ⁰/Čech cohomology, weighted restriction maps | **Live but diagnostic-only** (ADR-0030 verdict: NO-GO on using it for invalidation) | Yes, but only feeds the health metric + `agreement`/`h0_dimension` ops — confirmed off the invalidation path; `RestrictionMap::compose`/`weighted`/`new` have zero non-test callers |
| `restriction_review.rs` — the ADR-0031 toy comparison | **Experiment only** | No — exercised by `tests/restriction_review_toy.rs` |

The exact restriction-addressed derived-view cache (ADR-0031) that lived here as `restriction_cache.rs` is now the [`leyline-cache`](../cache/) crate (bead `ley-line-open-754aa1`), with a `PartitionSpec` key that binds `parserId`. It shares no code with this crate. The `sheaf_restriction_replay` bench measures `SheafCache` against it on the same git replay (`task cache:ablation`); `leyline-cache` is a dev-dependency here only.

`cache.rs` is the piece most likely under-covered relative to its actual correctness stakes.
