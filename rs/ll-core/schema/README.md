# leyline-schema

Shared SQLite schema — the contract between ley-line crates.

## What's here

- **`create_schema(conn)`** — creates the `nodes` table + indexes (idempotent).
- **`insert_node(conn, id, parent_id, name, kind, size, mtime, record)`** — insert a single node. Uses `INSERT OR REPLACE` so re-inserts overwrite.
- **`set_meta(conn, key, value)`** / **`get_meta(conn, key)`** — `_meta` key/value accessors. `set_meta` lives here for cross-crate use (ts + cli-lib both call it); `get_meta` returns `Ok(None)` for missing keys and propagates SQL errors as `Err`.

## The `nodes` table

```sql
CREATE TABLE IF NOT EXISTS nodes (
    id          TEXT PRIMARY KEY,
    parent_id   TEXT,
    name        TEXT NOT NULL,
    kind        INTEGER NOT NULL,   -- 0 = file, 1 = directory
    size        INTEGER DEFAULT 0,
    mtime       INTEGER NOT NULL,
    record_id   TEXT,                -- optional: FK into results table (mache lazy loading)
    record      JSON,
    source_file TEXT                 -- optional: originating source file (mache file tracking)
);
CREATE INDEX IF NOT EXISTS idx_parent_name ON nodes(parent_id, name);
CREATE INDEX IF NOT EXISTS idx_source_file ON nodes(source_file)
    WHERE source_file IS NOT NULL;
```

`record_id` and `source_file` are nullable — ley-line's parse paths leave them `NULL`; mache populates them. `idx_source_file` is partial so the index materializes only when `source_file` is populated.

Every crate that reads or writes arena data uses this schema. `leyline-ts` adds `_ast`, `_source`, `node_refs`, `node_defs`, `_imports`, `_file_index`, and `_meta` sidecar tables. `leyline-lsp` adds `_lsp`, `_lsp_defs`, `_lsp_refs`, `_lsp_hover`, and `_lsp_completions`.
