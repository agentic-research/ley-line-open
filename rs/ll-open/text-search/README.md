# leyline-text-search

Unstructured-text semantic search backend abstraction for ley-line-open. Default `NullEngine` keeps the daemon op surface compiling and consistently-shaped; richer engines (today: Witchcraft XTR-WARP) ship behind feature flags.

**License:** AGPL-3.0-or-later.

## Why this exists

LLO's existing single-vector KNN op (`vec_search`, backed by `sqlite-vec` + fastembed/MiniLM) stays as the dense-retrieval surface for source code. This crate adds a separate retrieval surface for **unstructured text** — chat transcripts, docs, prose corpora — where late-interaction retrieval (XTR-WARP) outperforms single-vector cosine.

Two engines, one trait, one daemon op surface. The daemon talks to whatever's wired in; clients always see a structured response.

## Substrate contract — sidecar by construction

Every engine MUST be sidecar to the Σ Merkle-CAS substrate:

1. Engine storage path lives OUTSIDE the arena directory.
2. Engine never writes a `*.bindings.capnp` segment.
3. Re-indexing a corpus never advances `current_root`.

`tests/substrate_non_leak.rs` asserts (1) directly via the trait's `storage_path()` accessor — every engine impl reports a path; the gate refuses any path under the arena. (2) is structurally guaranteed by the crate not depending on capnp at all. (3) is a daemon-level property whose gate belongs next to a real `DaemonContext` in `leyline-cli-lib` integration tests; tracked as follow-up.

## Engines shipped

| Engine | Feature flag | What it does |
|---|---|---|
| `NullEngine` | (default) | Every op returns `Error::NotImplemented`. The daemon op surface compiles; clients see a structured "not configured" error instead of an "unknown op" 404. |
| `WitchcraftEngine` | `engine-witchcraft` | XTR-WARP late-interaction retrieval via the upstream `witchcraft` crate. Constructor needs a T5 assets directory; see the module docstring for the deployment-time knob. |

Pulling the Witchcraft engine in adds `candle`, `tokenizers`, and `safetensors` (the witchcraft transitive set) — off by default to keep the base build small.

## Used by

- **`leyline-cli-lib`** — daemon ops `op_text_search` and friends call the configured `TextSearchEngine` trait impl.

## Status

`NullEngine` is the production default. `WitchcraftEngine` is feature-gated for opt-in deployment when the T5 assets and additional dependency weight are acceptable. The substrate-non-leak gate in `tests/` runs on both engines.
