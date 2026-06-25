# leyline-hdc

Hyperdimensional computing (HDC) for structural code search. Encodes parsed AST subtrees into fixed-size hypervectors; queries by popcount-Hamming distance over packed BLOBs.

**Stability.** Substrate identity stable at v0.5.0 (ADR-0024). Backward-incompatible with v0.4.x hypervectors — any cached HDC index must be regenerated. The compositional-vs-distance use-mode question is the subject of ADR-0025; see "Status" below.

## What's here

- **`encoder`** — `EncoderNode` → `Hypervector` via the v0.5.0 substrate-identity pipeline (bundle composition over child positions; permute-encoded position; seeded leaves carrying char-trigram bundles of leaf token text). `encode_fresh()` / `encode_tree()` are the public entry points; `SubtreeCache` memoizes subtree HVs across encode calls.
- **`canonical`** — AST canonicalization (per-language kind→canonical maps; rule-driven node grouping) feeding the encoder's deterministic input.
- **`codebook`** — `AstCodebook` plus per-layer codebooks (module, sheaf-derived). `canonical_signature_bytes` is the fp-quantize hash that groups equivalent shapes; v0.5.0 dropped `sorted_child_kinds` from the signature (bundle composition already carries that information; the hash was structurally redundant + brittle to immaterial AST variation).
- **`combined`** — packs per-layer HVs into a single byte-aligned BLOB column for SIMD-friendly popcount comparisons across layers.
- **`query`** — popcount-distance similarity over BLOBs. SQLite UDF for in-query distance (`hv_distance`). `unbind_child_at_position` and `explain_cluster_centroid` were retired in v0.5.0 along with the `content_role` algebra.
- **`sheaf`** — `HvCell` and the bit-level Hamming-agreement primitive used by `leyline-sheaf` for HDC-stalked sections.
- **`schema`** — `_hdc*` sidecar table DDL.
- **`sql_udf`** — SQLite UDF registration for `hv_distance`, `hv_canonical_hash`, etc.

## Dimensionality

`D = 8192` bits per hypervector — `1024` bytes per BLOB. Single source of truth: `D_BITS` const in `lib.rs`. Math-friend review margin: ~7× capacity headroom per layer for typical AST function sizes (50-150 nodes), so flat bundles stay discriminable without recursive bundling.

## Layers

`LayerKind` enumerates the topology axes encoded. As of v0.5.0:

| Layer | What it encodes |
|---|---|
| `Ast` | Canonical AST shape (kind alphabet + bundle composition of subtree HVs) |
| `Module` | Module/file hierarchy fingerprint |
| `Semantic` | Charikar simhash projection from dense embeddings (when `vec` feature is active) |
| `Temporal` | Time-series fingerprint over edit / access patterns |
| `Hir` | High-level intermediate representation hashes (when LSP is wired) |
| `Lex` | Lexical-token fingerprint independent of structure |
| `Fs` | Filesystem-layout fingerprint |

Layers are stored in `_hdc.layer_kind` as TEXT so the alphabet can be extended without a migration.

## v0.5.0 substrate-identity rewrite (ADR-0024)

The encoder identity changed from v0.4.x in four ways, shipped together:

1. **Bundle composition replaces XOR-bind** for child composition. Bundle is similarity-dampening; XOR is similarity-perfect-transmitting. Tree composition wants dampening — children sharing structure should make their parents *more* similar, not equidistant.
2. **`content_role` dropped** along with the unbind algebra. Child positions are carried by permute (rotation), not by XOR with a role HV.
3. **fp-quantize over `canonical_signature_bytes` drops `sorted_child_kinds`.** The kinds are still encoded in bundle composition's behavior; carrying them in the hash too made the canonical group identity brittle to immaterial AST variation.
4. **Seeded leaves.** Leaves with content (identifiers, literals) compose their HV from the deterministic kind+identity vector PLUS a char-trigram bundle of the leaf token text (Kanerva 2009 text-encoding pattern). Gives HDC its first explicit lexical channel.

Empirical signature post-rewrite (Phase 0B-real on the LLO daemon corpus, 36 ground-truth groups, K=10):

| Architecture | Recall@10 | Δ vs vec-alone |
|---|---:|---:|
| vec-alone | 0.518 | (baseline) |
| HDC-alone | 0.375 | -27.6% |
| Score-fusion α=0.20 | **0.556** | **+7.3%** |
| Kernel-RBF α=0.40 | **0.557** | **+7.7%** |

The complementary-modality claim holds under weighted score-fusion; bare HDC and naive equal-weight fusions (RRF, filter-rerank) underperform. See `rs/ll-open/cli-lib/tests/phase_0b_real_ground_truth.rs` for the validation gate (asserts `best_fusion_sweep > vec_alone + 0.02`).

## Status

v0.5.0 settled the **distance-retrieval** mode of HDC. ADR-0025 ("HDC compositional-vs-distance use modes: validate or remove") frames the next research arc: does HDC have value beyond distance retrieval (compositional bind/unbind queries, archetype codebook classification, sequence encoding via permute)?

The decision is pre-committed against the empirical record:

- **Compositional value confirmed** → invest fully in v0.6
- **No compositional value, fusion lift holds** → keep as cheap second voice; deprecate compositional roadmap
- **Neither** → clean removal; ADR-0026 documents post-mortem

See `docs/adr/0025-hdc-compositional-validation.md` for phasing + pre-registered falsification thresholds.

## Cost vs vec

| | HDC | vec (fastembed MiniLM-L6) |
|---|---|---|
| Per-item storage | 1 KB (8192 bits) | 1.5 KB (384 floats × 4) |
| Encoding cost | ~μs (tree walk + bundle) | ~10-100 ms (model inference) |
| Distance per comparison | ~100 ns (popcount-Hamming) | ~μs (cosine over 384 floats) |
| Model artifact | 0 MB | ~100 MB |
| Cold start | 0 ms | 1-3 s |

HDC is ~10-100× cheaper to encode and ~10× cheaper to compare; no model dependency. Cost gap matters most in low-resource deployments (wasm / edge / embedded). Doesn't justify HDC by itself if compositional value isn't there — see ADR-0025's cost-framing section.

## Used by

- `leyline-cli-lib` via the `hdc` feature flag (`DaemonContext.enrichment_passes`).
- Daemon ops (`op_hdc_search`, etc.) — see `rs/ll-open/cli-lib/src/daemon/`.
- `leyline-sheaf` for HDC-stalked structural sections (`HvCell`).
