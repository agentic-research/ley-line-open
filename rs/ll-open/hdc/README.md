# leyline-hdc

Hyperdimensional computing for structural code search — per-scope hypervectors over multiple topology layers (AST, module, semantic, temporal, optionally HIR) plus a combined-view BLOB for fast prefilter queries.

**Experimental.** Storage substrate ships today (`hdc-1`); codebooks and full encoders land in subsequent beads. See `ley-line-open-96b1a9` for the full design rationale.

## What's here

- **`HdcPass`** — `EnrichmentPass` implementation. Builds hypervectors from `nodes` + `_ast` and writes `_hdc*` sidecar tables.
- **`encoder`** — per-layer hypervector construction (random projection + recursive hierarchical bundling for deep trees).
- **`codebook`** — per-layer learned codebooks (frozen after training).
- **`combined`** — pack per-layer vectors into one byte-aligned BLOB for SIMD popcount.
- **`query`** — popcount-distance similarity over BLOBs. SQLite UDF for in-query distance.
- **`canonical`** / **`sheaf`** — internal AST normalization passes.

## Dimensionality

D = 8192 bits per vector — 1024 bytes BLOB, byte-aligned for SIMD popcount. Math-friend review: D=8192 leaves ~7× capacity margin per layer for typical AST function sizes (50-150 nodes), so flat bundles stay discriminable. Deeper trees use recursive (hierarchical) bundling.

## Used by

- `leyline-cli-lib` via the `hdc` feature flag (`DaemonContext.enrichment_passes`)
