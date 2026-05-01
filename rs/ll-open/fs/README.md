# leyline-fs

Filesystem presentation — mounts the arena as NFS or FUSE.

## What's here

- **`SqliteGraph`** — zero-copy SQLite reader via `sqlite3_deserialize`. Loads arena buffers as in-memory databases without temp files.
- **`SqliteGraphAdapter`** — writer + lock-free reader pool (`ArrayQueue<SqliteGraph>`). Writes go through `Mutex<SqliteGraph>`; reads pop from a pool that auto-sizes 2–8 readers.
- **`HotSwapGraph`** — watches the Controller generation counter; re-opens the graph when the arena is updated.
- **`StagingGraph`** — CoW overlay with a shadow SQLite database. Writes go to the shadow; reads check shadow first, fall through to live. Powers the `.staging/` virtual directory.
- **NFS mount** — userspace NFSv3 via `nfsserve` (default on macOS). The kernel's native NFS client handles page cache and readahead.
- **FUSE mount** — via `fuser` (fallback, default on Linux). Supports read, write, create, rename, delete, symlink, and fsync.
- **C FFI** — context-handle API (`leyline_open/close/get_node/list_children/lookup_child/read_content`) with cbindgen header for C/Go consumers.

## Feature flags

- `validate` (default), `fuse` (default), `nfs` (default) — see `Cargo.toml` for dependency mapping.
- `splice` — FUSE write-back triggers `splice_and_reproject` for AST-tracked nodes.

The `VectorIndex` sidecar previously lived here under a `vec` feature; it's now part of `leyline-cli-lib`'s daemon module so vectors live next to the `EmbeddingPass` framework that produces them.
