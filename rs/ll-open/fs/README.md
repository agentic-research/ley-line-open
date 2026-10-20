# leyline-fs

Filesystem presentation — mounts the arena as NFS or FUSE.

## What's here

- **`SqliteGraph`** — zero-copy SQLite reader via `sqlite3_deserialize`. Loads arena buffers as in-memory databases without temp files.
- **`SqliteGraphAdapter`** — writer + lock-free reader pool (`ArrayQueue<SqliteGraph>`). Writes go through `Mutex<SqliteGraph>`; reads pop from a pool that auto-sizes 2–8 readers.
- **`HotSwapGraph`** — watches the Controller generation counter; re-opens the graph when the arena is updated.
- **`StagingGraph`** — CoW overlay with a shadow SQLite database. Writes go to the shadow; reads check shadow first, fall through to live. Powers the `.staging/` virtual directory.
- **NFS mount** — userspace NFSv3 via `nessie-nfsserve`, an `nfsserve` fork with COMMIT (default on macOS). A write is proposed at the client's COMMIT (fsync or close); each WRITE first checks that the edit, merged with what the store holds, lands (ADR-0045 T8). The kernel's native NFS client handles page cache and readahead.
- **FUSE mount** — via `fuser` (fallback, default on Linux). Supports read, write, rename, delete and fsync.
- **Writes to a store** (ADR-0045 T8) — the mount presents the projection: a source file is a directory of its syntax nodes. Every change is a proposal through the installed `SourceWriter`, decided as storefs's are: a leaf edit is the edit applied to the bytes its handle read, merged with any change landed since, refused `ESTALE` only where the two overlap, and an unparseable file lands with a diagnostic. Removing a leaf writes its file without it; removing a source file's entry deletes the file; removing a directory is an rmdir; renaming a source file's entry or a directory moves it. A create, mkdir, a syntax node's rename and a mode change are `EPERM`: the projection has nothing to present them as.
- **C FFI** — context-handle API (`leyline_open/close/get_node/list_children/lookup_child/read_content`) with cbindgen header for C/Go consumers.

## Feature flags

- `validate` (default), `fuse` (default), `nfs` (default) — see `Cargo.toml` for dependency mapping. `validate` is the syntax-error report the daemon's `validate` op uses; the mount refuses no write on syntax.
- `splice` — the mount's write path to a store: each operation's `Proposal` goes to the installed `SourceWriter` (ADR-0040 D3's one write path, implemented by `leyline-cli-lib`); with none installed the edit is refused. Without `splice` the mount writes no store.

The `VectorIndex` sidecar previously lived here under a `vec` feature; it's now part of `leyline-cli-lib`'s daemon module so vectors live next to the `EmbeddingPass` framework that produces them.
