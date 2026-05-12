# Rust workspace — `rs/`

The Rust workspace for ley-line-open. Two tiers: `ll-core/` (infrastructure) and `ll-open/` (projection engine). Top-level [README](../README.md) covers project framing; this file is the orientation map for the workspace.

## Tier 1: Infrastructure (`ll-core/`)

| Crate | Purpose |
|---|---|
| [`leyline-core`](ll-core/core/) | Arena header + Controller (mmap'd control block, `current_root: [u8; 32]`) |
| [`leyline-schema`](ll-core/schema/) | Shared SQLite `nodes` table DDL — local SQL projection contract |
| [`leyline-public-schema`](ll-core/public-schema/) | Cap'n Proto schema for the daemon UDS + MCP wire (`daemon.capnp`) |
| [`leyline-schema-capnp`](ll-core/schema-capnp/) | Cap'n Proto schemas for the Σ event log (AstNode, SourceFile, BindingRecord, Head) |

## Tier 2: Projection engine (`ll-open/`)

| Crate | Purpose |
|---|---|
| [`leyline-fs`](ll-open/fs/) | SqliteGraph (zero-copy `sqlite3_deserialize`), Graph trait, reader pool, NFS/FUSE mount |
| [`leyline-ts`](ll-open/ts/) | Tree-sitter AST projection + bidirectional splice |
| [`leyline-lsp`](ll-open/lsp/) | LSP client — spawns language servers, projects symbols + diagnostics; emits `BindingRecord` capnp event log |
| [`leyline-hdc`](ll-open/hdc/) | Hyperdimensional computing — per-scope hypervectors for structural code search (experimental) |
| [`leyline-vcs`](ll-open/vcs/) | jj sidecar — automatic versioning of arena snapshots |
| [`leyline-sign`](ll-open/sign/) | CMS signing primitives + gpgsm-compatible binary for jj commit signing |
| [`leyline-cli-lib`](ll-open/cli-lib/) | Daemon: living SQLite db + arena flip + Σ root advance + MCP/UDS surfaces |
| [`leyline-cli`](ll-open/cli/) | `leyline` binary — `parse`, `lsp`, `daemon`, `serve`, `inspect` subcommands |

## Tier-isolation gate

`ll-core/*` crates MUST compile without `ll-open/*`. CI gate: `task tier:isolation` builds `leyline-core`, `leyline-schema`, `leyline-public-schema`, `leyline-schema-capnp` in isolation. If a `ll-core/*` Cargo.toml gains a `ll-open/*` path dep, the gate fails.

## Build

```bash
# From the workspace root (rs/):
cargo build --workspace
cargo test --workspace

# Or via the Taskfile from the repo root (preferred — wires pkg-config for macFUSE-T on macOS):
cd .. && task ci      # check + clippy + fmt + FFI staticlib + tier isolation + test
cd .. && task install # release + codesign + install to ~/.local/bin
```

## Cap'n Proto toolchain

Exact-pinned per [ADR-0014 §3](../docs/adr/0014-capnp-as-protocol.md):

- `capnp = "=0.25.0"`
- `capnpc = "=0.25.0"`
- `capnp-json = "=0.1.0"` (daemon wire codec only)

System `capnp` binary required for `build.rs` codegen — `brew install capnp` (macOS) or `apt-get install capnproto libcapnp-dev` (Ubuntu). The `libcapnp-dev` package ships the standard schema includes (`/usr/include/capnp/c++.capnp` etc.) that `capnp-json`'s build script needs.
