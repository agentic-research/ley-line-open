# ADR-0039 — Multi-vector retrieval over CAS: kernel-delegated caching instead of a bespoke index

**Status:** Proposed (2026-08-07) — falsification ladder below has not run yet. No implementation exists.
**Bead:** `ley-line-open-6b0276`
**Related:**
- ADR-0030 (sheaf-over-embeddings NO-GO — establishes that a lossy/heuristic layer must sit off the correctness path, the same posture this ADR needs for a caching layer)
- ADR-0031 (restriction-addressed caching — establishes "exact, not approximate" as the house standard this ADR extends from correctness-caching into storage/index design)
- `leyline-fs/README.md` (kernel-delegated caching precedent: "the kernel's native NFS client handles page cache and readahead")
- `leyline-cdc` (BLAKE3-addressed, boundary-stable, SIMD-fast chunking — the storage primitive this ADR proposes reusing)
- `leyline-text-search` (the crate that currently imports `witchcraft`, and the proximate reason this question exists)

---

## Thesis

> **LLO already delegates caching to the kernel everywhere else in the system. A multi-vector retrieval index should be a first-class citizen of that model — content-addressed blobs exposed as ordinary files — not a second, foreign caching system bolted on beside it.**

## Context

`leyline-text-search`'s `WitchcraftEngine` (feature-gated, off by default, `NullEngine` is the production default) wraps the `witchcraft` crate for XTR-WARP late-interaction retrieval over unstructured text. Its `DB` (`witchcraft/src/db.rs`) is a WAL-mode SQLite file, hashed with SHA-256, with its own `SCHEMA_VERSION`/`APP_ID` pragma and `.wal`/`.shm` sidecar files — a complete, self-contained, **application-managed** cache/index with zero relationship to LLO's Σ substrate (BLAKE3 content-addressing, mmap'd arena, `current_root`). `leyline-text-search`'s own README already has to build an explicit containment wall around it: engine storage must live outside the arena directory, and re-indexing must never advance `current_root`.

That containment isn't just a naming mismatch. LLO's actual caching philosophy, verified in the code that ships today, is: **hand the OS a file, let the page cache manage residency.** The arena is mmap'd, not application-buffered. The NFS mount's own README states it plainly: "the kernel's native NFS client handles page cache and readahead." Even the sheaf's own `on_change` isn't a data cache — it's an invalidation *signal*; residency is the kernel's job throughout the rest of the system. Witchcraft's WAL-SQLite is the one place in this dependency closure that reinvents that job itself.

## The claim, and why it is not one claim

"Expose token-level embeddings as content-addressed blobs through the existing FUSE/NFS mount instead of importing a bespoke index" bundles three independently falsifiable sub-claims. Any one of them dying kills the idea in its current form without saying anything about the other two:

1. **Performance.** Per-file FUSE/NFS reads for scattered per-token embeddings may carry overhead that dwarfs whatever the page cache buys, compared to a tuned index (witchcraft's own SQLite B-tree) that packs everything into locality-friendly pages on purpose.
2. **Compression.** Witchcraft's `packops.rs` / `rans64.rs` exist specifically to shrink multi-vector embeddings — high-entropy floating-point data, not the kind of repeated-byte-sequence content CDC dedup is built for. "Content-addressed" does not imply "small." A naive blob-per-embedding scheme could simply be *larger* on disk than witchcraft's packed+rANS-compressed format, independent of any caching question.
3. **Indexing, not caching.** XTR-WARP's actual contribution is a compressed *index* that prunes most candidates before scoring. If late-interaction retrieval fundamentally needs sub-linear search structure, kernel-delegated caching does not fix an architecture still doing a near-linear scan — that is an algorithm problem, not a residency problem, and no amount of "the kernel decides what's hot" resolves it.

## Proof ladder (falsifiable, cheapest-first)

**Rung 0 — precedent check.** Does LLO already have latency numbers for many-small-reads through the existing FUSE/NFS mount or the mmap'd arena, at a scale comparable to a late-interaction scan (hundreds to thousands of scattered reads per query)? If this data already exists, it is the cheapest possible signal, before any new code is written.

**Rung 1 — toy-scale kill switch (~1 day).** Store N synthetic per-token embeddings as individual content-addressed blobs under the mount vs. packed in one SQLite/mmap'd file (witchcraft's own shape). Measure random-access latency retrieving ~1000 scattered vectors, simulating one late-interaction scan. **If per-file overhead is already an order of magnitude worse at toy scale, the idea dies here** — matches ADR-0030 Rung 1's shape: cheap, binary, kills the thesis before real investment.

**Rung 2 — real measurement (real corpus).** If Rung 1 survives: A/B against `WitchcraftEngine` itself (already runnable locally with the `engine-witchcraft` feature) on a real corpus. Same table shape as ADR-0031's `policy | latency | recompute_saved`: end-to-end query latency **and** on-disk footprint, both measured, neither assumed. This is where sub-claim 2 (compression) gets a real number instead of a guess.

**Rung 3 — correctness invariant (parallel, cheap).** Witchcraft's WAL-SQLite gets ACID transactions for free. "Files on a mount" does not, automatically. Does any code path read a content-addressed blob mid-write under this design? Needs its own explicit test — same pattern as ADR-0030 Rung 3, which proved the sheaf's lossy optimizer was safe only because `node_hash` caught false-negatives underneath it. Here the analogous floor would be: a partially-written blob must never resolve to a valid content address (BLAKE3 addressing over the complete bytes already gives this for free if blobs are written-then-renamed rather than written-in-place — needs verification, not assumption).

**Rung 4 (gates sub-claim 3, distinct methodology from 0-3) — indexing sufficiency.** Construct a query pattern where XTR-WARP's compressed index would prune to O(log n) candidates. Check whether a flat content-addressed scan is fundamentally worse at any realistic corpus size. **This is the rung most likely to actually kill the idea**: if yes, the kernel-caching benefit is real but irrelevant, because the design is still scanning too much regardless of how cheaply each scan step is served.

**Falsification verdict rule**, matching ADR-0030/0031's own discipline: any single rung failing kills this ADR's proposal in its current form. It does not automatically revive witchcraft as the answer — `leyline-text-search` stays correctly deferred from publishing either way (see Non-goals), and the honest fallback is "keep the containment wall, keep it optional, revisit later," not "therefore import witchcraft's index design as-is."

## Addendum — encoder backend is a separate, downstream decision

Producing the per-token embeddings in the first place (what witchcraft's `Embedder` does today via `T5EncoderModel` + candle) is **not gated on this ADR**. It only becomes a live question if `5-whats #4` from the design discussion this ADR grew out of — whether unstructured-text search beyond `leyline-chat-embed`'s existing fastembed/MiniLM path is a validated need at all — resolves yes. Kept separate deliberately, same discipline ADR-0030 used to keep the cohomology question separate from the restriction-map question that turned out to be the actual result.

Worth recording now so it isn't re-derived later: on Apple Silicon, the unified memory architecture means host-resident (mmap'd/paged) data does not need an explicit host→device copy before device compute — MLX is designed around exactly that property, unlike CUDA-shaped frameworks (including candle's CUDA backend), which assume separate host/device memory pools. That would let a content-addressed, kernel-cached blob hand off to an MLX computation with less friction than into a CUDA-style pipeline — reinforcing this ADR's storage design specifically on Mac, where LLO's own release matrix and this session's own dev environment already live.

This is not a free upgrade, and the tradeoff should stay explicit rather than assumed away: MLX is Apple Silicon only today (`aarch64-apple-darwin`); Linux/CUDA support is roadmapped, not shipped, across every Rust MLX binding checked (`mlx-rs`, `mlxrs`, `mlx-rust` — fragmented, multiple competing unofficial bindings, none dominant as of 2026-08-07). Choosing MLX for a from-scratch encoder means either the capability is Mac-only (precedented — `witchcraft` itself already splits `t5-quantized` vs `t5-openvino` by platform) or LLO maintains two encoder backends. That choice is not this ADR's to make; it is downstream of whether an own-encoder is ever built at all.

## Consequences

- If the ladder survives through Rung 4: `leyline-cdc` and `leyline-core` acquire a new consumer pattern (embeddings as first-class content-addressed blobs), and any future multi-vector retrieval work has a template that doesn't require importing a foreign caching philosophy.
- If it dies at any rung: the honest outcome is documented here, `leyline-text-search` keeps its current containment wall and optional-feature status, and this ADR's negative result is itself the deliverable — same posture ADR-0030 took toward its own NO-GO.

## Non-goals

- Not deciding whether unstructured-text search beyond `leyline-chat-embed` is a validated product need — that question is a precondition, not part of this ladder.
- Not deciding an encoder backend (candle, MLX, or otherwise) — see Addendum.
- Not proposing to remove or replace `leyline-text-search`/`WitchcraftEngine` today; it stays correctly deferred from the crates.io publish batch regardless of this ADR's outcome.
