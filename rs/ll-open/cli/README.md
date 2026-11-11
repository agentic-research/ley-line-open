# leyline-cli

The `leyline` binary. Thin wrapper around `leyline-cli-lib`.

## What's here

- **`parse`** — parse a source directory into a .db (`nodes`, `_ast`, `_source` and the fact tables), with its Cap'n Proto segment files and `head.capnp` beside it.
- **`daemon`** — long-running daemon. Hosts the living db + UDS control socket + MCP transport (HTTP and UDS). Snapshots to the arena on every parse/enrich/load; readers hot-swap via the Σ root chain. With `--mount <dir>` (a `mount` build) it mounts the store's files there: NFS on macOS, FUSE on Linux (`leyline-storefs` host mount, ADR-0045 T8).
- **`execution-daemon`** — the first-party execution/v1 daemon: `--backend native|micro-vm`, `--worker`, `--catalog`, `--run-root`, and either `--cas-root` or `--cas-volume`. It verifies evidence with `--trust-key` and `--evidence-store`; with neither it refuses every start, and `--allow-unverified-evidence` (local fixtures only) authenticates nothing.
- **`execution`** — client for the execution/v1 operations over a daemon socket (`capabilities`, `provision`, `status`, `inspect`, `collect`, `cleanup`, `start`, `cancel`).
- **`rootfs`** — `import-oci` (a digest-pinned OCI image, from `--layout` or `--from-registry`, into a content-addressed execution rootfs) and `provision-cas` (a case-sensitive CAS root).
- **`load`**, **`cdc`** (`enable`, `gc`), **`verify-head`**, **`rebuild`**, **`splice`**, **`doctor`**, **`self`** (`install`, `update`, `where`).
- **`inspect`** — look up a node in the arena's active SQLite buffer, or run `--query` SQL against it.
- **`lsp`** — run one language server over one source file and write (or `--merge-db` into) a .db.

## Feature flags

Default: `lsp`, `validate`, `hdc`, `cdc`, `sheaf`.

- `lsp` (default) — language-server enrichment via [`leyline-lsp`](../lsp/)
- `mount` — `daemon --mount`: the store's filesystem behind the host's NFS client via [`leyline-storefs`](../storefs/) (`--backend nfs`; pure Rust, links no system library)
- `fuse` — `mount` plus `--backend fuse`, the host's FUSE client (links libfuse: fuse-t on macOS, libfuse on Linux). Linux's default backend is FUSE, so a Linux build without `fuse` mounts only with `--backend nfs`
- `vec` — embedding sidecar + `vec_search` op via `sqlite-vec`
- `text-search` — the `text_search` op
- `all` — every op, without the host mount; `full` — `all` plus `mount` and `fuse`

## Used by

- End users (CLI invocation)
- Distroless OCI image (`ghcr.io/agentic-research/ley-line-open:v<VERSION>`) — default CMD is `daemon --mcp-port 8384 --mcp-bind 0.0.0.0 --mcp-allow-public`

## Build

```bash
cargo build --release
# or via the Taskfile (preferred — handles pkg-config wiring for macFUSE-less hosts):
task install
```
