# leyline-fs

The nodes-table graph — the projection's tree read by display path — with its
arena hot-swap, chunk-backed range reads and GC, behind a C FFI. The host
mount is not here: since ADR-0045 T8 it is `leyline-storefs` behind FUSE and
NFS adapters, serving the store's files (`_tree`), not this tree.

## What's here

- **`SqliteGraph`** — zero-copy SQLite reader via `sqlite3_deserialize`. Loads arena buffers as in-memory databases without temp files.
- **`SqliteGraphAdapter`** — writer + lock-free reader pool (`ArrayQueue<SqliteGraph>`). Writes go through `Mutex<SqliteGraph>`; reads pop from a pool that auto-sizes 2–8 readers. A store (a database with `_source`) refuses every write: only a proposal through `leyline_cli_lib::source_write` changes a store, so its projection stays a function of its stored bytes.
- **`HotSwapGraph`** — polls the Controller's `current_root`; re-opens the graph when the arena is republished.
- **`StagingGraph`** — CoW overlay with a shadow SQLite database. Writes go to the shadow; reads check shadow first, fall through to live.
- **C FFI** — context-handle API (`leyline_open/close/get_node/list_children/lookup_child/read_content`) with cbindgen header for C/Go consumers.

## Feature flags

- `validate` (default) — the syntax-error report the daemon's `validate` op uses.
- `cdc` — chunk-backed content storage: range reads touch only overlapping chunks; transactional GC.
- `verify` — verify-on-fault arena serving (not shipping; bead `ley-line-open-b6a4dd`).

The `VectorIndex` sidecar previously lived here under a `vec` feature; it's now part of `leyline-cli-lib`'s daemon module so vectors live next to the `EmbeddingPass` framework that produces them.
