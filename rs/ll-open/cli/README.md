# leyline-cli

The `leyline` binary — entry point for parse, daemon, serve, and inspect operations. Thin wrapper around `leyline-cli-lib`.

## What's here

- **`parse`** — cold-parse a source directory into a SQLite arena (`.arena` + `.ctrl` pair). Tree-sitter ASTs, source projection, node tree.
- **`daemon`** — long-running daemon. Hosts the living db + UDS control socket + MCP HTTP transport. Snapshots to the arena on every parse/enrich/load; readers hot-swap via the Σ root chain.
- **`serve`** — start the daemon with FUSE/NFS mount enabled (`--features mount`). Exposes the arena as a virtual filesystem.
- **`inspect`** — dump arena header, controller state, segment file metadata. Diagnostic / forensic tool.
- **`lsp`** — invoke the LSP enrichment pass against a parsed arena (typically called from `daemon`'s `enrich` op; exposed standalone for one-shot LSP runs).

## Feature flags

- `lsp` (default) — language-server enrichment via [`leyline-lsp`](../lsp/)
- `mount` — FUSE/NFS filesystem presentation via [`leyline-fs`](../fs/) (macFUSE-T on macOS, libfuse on Linux)
- `vec` — embedding sidecar + `vec_search` op via `sqlite-vec`

## Used by

- End users (CLI invocation)
- Distroless OCI image (`ley-line-open:0.3.0`) — default CMD is `daemon --mcp-port 8384 --mcp-bind 0.0.0.0`

## Build

```bash
cargo build --release
# or via the Taskfile (preferred — handles pkg-config wiring for macFUSE-less hosts):
task install
```
