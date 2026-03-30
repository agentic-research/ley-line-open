# ley-line-open

Open-source data plane primitives for agentic systems. Extracted from [ley-line](https://github.com/agentic-research/ley-line).

## Crates

### Tier 1: Infrastructure (`ll-core/`)

| Crate | Purpose |
|-------|---------|
| `leyline-core` | Arena header (`repr(C)`, bytemuck), controller (mmap'd control block) |
| `leyline-schema` | Shared `nodes` table DDL — the contract between crates |
| `leyline-public-schema` | Protobuf definition of nodes schema (cross-language contract) |

### Tier 2: Projection Engine (`ll-open/`)

| Crate | Purpose |
|-------|---------|
| `leyline-fs` | SqliteGraph (zero-copy `sqlite3_deserialize`), Graph trait, reader pool, NFS/FUSE mount, C FFI bridge |
| `leyline-ts` | Tree-sitter AST projection + bidirectional splice |
| `leyline-lsp` | LSP client — spawns language servers, projects symbols + diagnostics into nodes |

## Build

```bash
cd rs
cargo build
cargo test
```

## C FFI

`leyline-fs` builds as a staticlib (`libleyline_fs.a`) with a C header (`include/leyline_fs.h`):

```bash
cd rs
cargo build -p leyline-fs --lib
# Header: rs/ll-open/fs/include/leyline_fs.h
# Library: rs/target/debug/libleyline_fs.a
```

## Schema Contract

The `nodes` table is the shared data contract:

```sql
CREATE TABLE IF NOT EXISTS nodes (
    id TEXT PRIMARY KEY,
    parent_id TEXT,
    name TEXT NOT NULL,
    kind INTEGER NOT NULL,
    size INTEGER DEFAULT 0,
    mtime INTEGER NOT NULL,
    record JSON
);
CREATE INDEX IF NOT EXISTS idx_parent_name ON nodes(parent_id, name);
```

Defined once in `leyline-schema`. Used by [mache](https://github.com/agentic-research/mache) and ley-line.

## License

AGPL-3.0 — see [LICENSE](LICENSE).
