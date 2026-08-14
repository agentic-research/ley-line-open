# leyline-cdc

Content-defined chunking (CDC) — GearHash rolling hash with xet-compatible parameters, over the Σ substrate's BLAKE3 content addressing. Produces blob hashes; produces **no** root.

## What's here

- **`chunk` / `chunk_into`** — split a byte buffer into CDC chunks. Composes HuggingFace's [`gearhash`] crate (the SIMD-accelerated rolling hash) with xet's published boundary rule: target ~64 KiB, clamped to `[8, 128]` KiB.
- **`Chunk`** — one chunk, addressed by σ = BLAKE3 (`leyline_core::ContentAddressed`) — the same base as xet's `MerkleHash`, so chunk identity aligns with xet's scheme rather than introducing a foreign one.
- **`rechunk` / `rechunk_with_stats`** — re-chunk after an `Edit`, reusing unaffected chunks.
- **`read_range` / `read_range_into` / `SelectedRange`** — bounded range reads over a chunk `Manifest` without materializing the whole blob.
- **`reconstruct`** — rebuild the full byte buffer from a `Manifest` + `BlobStore`.

## The falsifiable benefit — boundary stability

The load-bearing property, falsified in the tests: an insert/delete in one region of a stream changes only the chunks *in that region* — every chunk outside it keeps an identical σ hash. Fixed-size chunking fails this (an insert shifts every downstream boundary). Boundary stability is what makes chunk-level dedup pay off: a small edit to a large file re-stores `O(1)` chunks, not `O(file size)`. See `boundary_stability_localizes_an_edit` and `beats_fixed_size_chunking_under_an_insert`.

## Used by

- **`leyline-fs`** — via its `cdc` optional feature (`dep:leyline-cdc`), gating the chunk-manifest-backed range-read path (`fs/src/chunked.rs`, `fs/src/gc.rs`).

## Correctness stance

This is one of the two crates covered by `task mutants`'s full-allowlist run (not just diff-scoped) — content-addressed chunking is exactly the kind of logic where a silent-wrong-output mutant survives a naive test suite.
