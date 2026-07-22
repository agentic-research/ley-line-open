# Turso × LLO Arena — Compatibility Falsifier (Result)

**Date:** 2026-07-21 · **Verdict: Path A is real** (Turso as write-side engine under content-addressed vanilla-SQLite snapshots), gated on two small, contributable feature gaps.

Method: fable-authored 4-falsifier protocol, run against a **real LLO arena** (`leyline parse` of `rs/ll-open/sheaf/src` → 19,261 `nodes` / 19,260 `_ast` / 336 `node_defs` / 1,773 `node_refs`). Oracle = stock `sqlite3` 3.51.0. Engine under test = `tursodb` built from source (`tursodatabase/turso`, release build).

## Results matrix

| Falsifier | Result | Evidence |
|---|---|---|
| **F1 — read-compat** | **PASS** ✅ | `tursodb` and `sqlite3` return byte-identical result sets across schema dump, 7 table counts, and a 200-row JOIN incl. `hex(node_hash)` blob bytes. |
| **F2 — deserialize-from-buffer** | **FAIL now / implementable** | `sqlite3_deserialize` is a `todo!()` stub (panics across FFI). But the `DatabaseStorage` trait + `OpenOptions::storage` seam already exist and were designed for pluggable storage — a buffer-backed impl is idiomatic (~1–2 wk zero-copy PR). **Not architecturally blocked.** |
| **F3 — write-vanilla** | **PASS** ✅ | Turso-written file: `file(1)` = "SQLite 3.x database (SQLite version 3047000)"; stock `sqlite3` **and** LLO's pure-Go `modernc.org/sqlite` both read it — `integrity_check=ok`, values exact (`1000\|2997\|1498500`, blob `00000000DEADBEEF`). |
| **F4 — output determinism** | **BYTE-IDENTICAL** ✅ | Two identical writes → `cmp` byte-for-byte identical + logically identical. Canonicalization *could* move engine-side. |
| **F3b — feature boundary** | prizes exist, flag-gated | `CREATE MATERIALIZED VIEW` → "experimental, enable with `--experimental-views`". `BEGIN CONCURRENT` → "supported when MVCC is enabled". The brief's two prizes (live views, concurrent MVCC) are present and maturing. |

**Decision-table verdict:** F1 PASS + F3 PASS + F4 identical → **Path A, full prize**, with an F2 file-handoff caveat that an upstream PR removes.

## Two gaps found — both trivially worked around AND clean OSS contributions

1. **`GENERATED ... STORED` columns unsupported.** LLO's arena trips this on exactly one non-load-bearing convenience column, `source_blobs.byte_len = length(blob_bytes)`. Turso's parser rejects the DDL on open ("Stored generated columns are not supported"). Fix (LLO-side): drop the generated-ness — make it a plain column or compute at query time. Fix (upstream): parser support. The fact tables mache reads use no generated columns; with `source_blobs` made vanilla, F1 passes cleanly.
2. **`sqlite3_deserialize` / `sqlite3_serialize` stubbed** (panic). Feasibility (from source investigation): **moderate, not blocked.** The pager talks to `DatabaseStorage` (`core/storage/database.rs:70-92`), `Database::open` accepts caller storage via `OpenOptions::storage` (`core/lib.rs:1231-1239`, doc-commented "for a remote page server service"), `:memory:` already implements the full `File` trait over a page `BTreeMap` (`core/io/memory.rs`), and the checksum feature that would reject vanilla images is off by default. Path: PR1 = copy-based `deserialize`/`serialize` in `bindings/c` (~2–4 d, kills the panic); PR2 = a ~150-line `BufferStorage: DatabaseStorage` for true zero-copy attach (~1–2 wk). No open upstream issue/PR exists — file the issue first for maintainer signal.

## What this means

Turso can be the **write-side MVCC engine** that builds LLO epochs, then serialize to **vanilla SQLite snapshots** that LLO's existing stock + modernc readers consume unchanged — the composite "branch to work, snapshot to attest." The value semantics (content-addressed immutable read side) are untouched; only the mutable writer changes. The `sqlite3_deserialize` gap is the only thing between "file-handoff write side" and "zero-copy buffer attach," and it's a bounded, idiomatic upstream contribution that also happens to be pure LLO leverage.

**Governing rule stands:** depend, don't fork for production; keep the vanilla-format snapshot as the reversible exit — validated here, since two independent stock readers parse Turso output byte-for-byte.

## Follow-through: `sqlite3_deserialize` implemented on a fork

The one gap that wasn't an LLO-side one-liner — `sqlite3_deserialize` (a `todo!()` stub that aborts across FFI) — was implemented as a copy-based MVP and verified end-to-end:

- Investigated the engine: the pager talks to a `DatabaseStorage` trait, `Database::open` accepts caller storage via `OpenOptions::storage` (doc-commented "for a remote page server service"), and `MemoryIO` caches files by path. So a buffer-backed open is idiomatic, not a fight with the architecture — **the stub is unimplemented, not blocked.**
- Implemented copy-based `deserialize`: copy the caller image into a fresh `MemoryIO`, seed via `File::pwrite`, open a `Database` mirroring `sqlite3_open(":memory:")`, swap the connection (`SQLITE_BUSY` if statements open).
- **Verified:** opens a real 19,261-node LLO arena from an in-memory buffer and queries it; plus a self-contained round-trip test (build a DB through the C API → read bytes → deserialize → query). `fmt`/`clippy` clean.
- Fork: `jamestexas/turso` @ `feat/sqlite3-deserialize-mvp`. Copy-based is the MVP; the LLO-relevant **zero-copy** variant (borrowed-buffer `DatabaseStorage` over `OpenOptions::storage`, avoiding the image copy) is the follow-up that actually buys the attach economics.

**Honest scope of the win:** the copy-based version proves the API path works end-to-end; it does not yet deliver LLO's zero-copy mmap-attach performance (that's the follow-up). Path A is *viable and started*, not *adopted* — the vanilla-format snapshot stays the reversible exit throughout.

## Reproduction

Artifacts under `~/.cache/turso-llo-probe/`: `arena.db` (real arena), `arena_compat.db` (source_blobs dropped), `battery.sql`, `f1_*.out`, `f3_*.db`, `f3go/`. `tursodb` at `~/remotes/art/turso/target/release/tursodb`.
