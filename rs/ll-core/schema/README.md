# leyline-schema

Shared SQLite schema — the contract between ley-line crates.

## What's here

- **`create_schema(conn)`** — creates the `nodes` table if it doesn't exist.
- **`insert_node(conn, id, parent_id, name, kind, size, mtime, content)`** — insert a single node.

## The `nodes` table

```sql
CREATE TABLE nodes (
    id        TEXT PRIMARY KEY,
    parent_id TEXT NOT NULL,
    name      TEXT NOT NULL,
    kind      INTEGER NOT NULL,  -- 0 = file, 1 = directory
    size      INTEGER NOT NULL,
    mtime     INTEGER NOT NULL,
    content   TEXT NOT NULL DEFAULT ''
);
```

Every crate that reads or writes arena data uses this schema. `leyline-ts` adds `_ast` and `_source` sidecar tables. `leyline-lsp` adds `_lsp`, `_lsp_defs`, `_lsp_refs`, `_lsp_hover`, and `_lsp_completions`.
