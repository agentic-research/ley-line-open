# leyline-cli

The `leyline` binary — entry point for parse, daemon and inspect operations. Thin wrapper around `leyline-cli-lib`.

## What's here

- **`parse`** — cold-parse a source directory into a SQLite arena (`.arena` + `.ctrl` pair). Tree-sitter ASTs, source projection, node tree.
- **`daemon`** — long-running daemon. Hosts the living db + UDS control socket + MCP HTTP transport. Snapshots to the arena on every parse/enrich/load; readers hot-swap via the Σ root chain. With `--mount <dir>` (a `mount` build) it mounts the store's files there: NFS on macOS, FUSE on Linux (`leyline-storefs` host mount, ADR-0045 T8).
- **`inspect`** — dump arena header, controller state, segment file metadata. Diagnostic / forensic tool.
- **`lsp`** — invoke the LSP enrichment pass against a parsed arena (typically called from `daemon`'s `enrich` op; exposed standalone for one-shot LSP runs).

## Feature flags

- `lsp` (default) — language-server enrichment via [`leyline-lsp`](../lsp/)
- `mount` — `daemon --mount`: the store's filesystem behind the host's FUSE or NFS client via [`leyline-storefs`](../storefs/) (links libfuse: fuse-t on macOS, libfuse on Linux)
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
