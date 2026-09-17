# leyline-schema

Shared SQLite schema — the contract between ley-line crates. This README
describes `projection-v6` (v0.20.0). The full table-by-table contract,
including the sidecar tables other crates add, is `docs/TABLE_CONTRACT.md`.

## Node ids

Every row is keyed by an integer `nid`, and a file's rows are one
contiguous range:

```text
nid = (file_id << 24) | ordinal      file rows and AST rows; ordinal 0 is the file itself
nid = -dir_id                        directory rows
```

`file_id` and `dir_id` are rowids in the interning tables below, so a row's
file is `nid >> 24` and a file's rows are `nid BETWEEN (file_id << 24) AND
(file_id << 24) | 0xFFFFFF` — a primary-key range search, no secondary
index. Helpers: `file_nid`, `dir_nid`, `nid_file_id`, `nid_dir_id`,
`nid_ordinal`, `file_nid_range`; the constants are `NID_ORDINAL_BITS` (24)
and `NID_ORDINAL_MASK`.

## What's here

- **`create_schema(conn)`** — creates the interning tables, `nodes`, its
  indexes and the display views (idempotent), and seeds the root directory.
  Bulk loaders call **`create_nodes_table`** before the load and
  **`create_nodes_indexes`** after `COMMIT`, which is the same schema in two
  phases.
- **`insert_node(conn, nid, parent_nid, name_id, kind_id, kind, ord, size,
  mtime, record)`** — insert one node row (`INSERT OR REPLACE`, so a
  reparse rewrites rows in place).
- **Interning**: `intern_name`, `intern_kind(lang, raw_kind)`,
  `intern_dir_chain(dir_path)`, `ensure_file_id(rel_path)`,
  `ensure_dir_nodes(rel_path, mtime)`, `lookup_file_id(rel_path)`.
- **Paths**: `node_path(conn, nid)` renders one row's path;
  `resolve_path(conn, path)` is its inverse. The `v_node_path` view renders
  every row in one scan (bulk export, mache) — do not use it for point
  lookups, a recursive view cannot prune to one `nid`.
- **A file's rows have one owner**: `FILE_KEYED_TABLES` lists every table
  keyed by a file and how (`FileKey::NidRange` with its extra nid columns,
  `FileKey::FileId`, or `FileKey::SourcePath`); `delete_file_rows`,
  `delete_file_rows_by_id`, `move_file_rows` and
  `refresh_source_paths_under_dir` iterate it. A splice, a mount `rm` or
  `mv`, and a scoped reparse all go through these; `table_exists` probes
  the optional sidecars.
- **`set_meta` / `get_meta`** — `_meta` key/value accessors. `get_meta`
  returns `Ok(None)` for a missing key and propagates SQL errors.
- **`SQLITE_MAX_BOUND_PARAMS`** — the one bound-parameter ceiling every
  `IN (...)` builder in the workspace uses (32 766, pinned against the
  linked SQLite by a test).

## The tables

```sql
-- Interning: append-only. A row is never deleted or renumbered, because
-- files.file_id feeds nids and a reused rowid would re-bind a dead file's
-- range. dirs.dir_id = 1 is the root ("" , parent NULL).
CREATE TABLE IF NOT EXISTS names (name_id INTEGER PRIMARY KEY, text TEXT NOT NULL UNIQUE);
CREATE TABLE IF NOT EXISTS kinds (kind_id INTEGER PRIMARY KEY, lang TEXT NOT NULL,
                                  raw_kind TEXT NOT NULL, UNIQUE(lang, raw_kind));
CREATE TABLE IF NOT EXISTS dirs  (dir_id INTEGER PRIMARY KEY, parent_dir_id INTEGER,
                                  name_id INTEGER NOT NULL,
                                  CHECK (dir_id = 1 OR parent_dir_id IS NOT NULL),
                                  UNIQUE(parent_dir_id, name_id));
CREATE TABLE IF NOT EXISTS files (file_id INTEGER PRIMARY KEY, dir_id INTEGER NOT NULL,
                                  name_id INTEGER NOT NULL, UNIQUE(dir_id, name_id));

CREATE TABLE IF NOT EXISTS nodes (
    nid         INTEGER PRIMARY KEY,
    parent_nid  INTEGER,
    name_id     INTEGER,            -- filesystem rows; NULL for AST rows
    kind_id     INTEGER,            -- AST rows and the file row; NULL for directories
    kind        INTEGER NOT NULL,   -- 0 = file, 1 = directory
    ord         INTEGER NOT NULL DEFAULT 0,
    size        INTEGER DEFAULT 0,  -- the file's real size on its file row
    mtime       INTEGER NOT NULL,   -- the file's real mtime on its file row
    record_id   TEXT,               -- optional: mache lazy resolution
    record      TEXT,               -- leaf token text / file content (TEXT, not JSON)
    source_file TEXT                -- optional: mache file tracking
);
CREATE INDEX IF NOT EXISTS idx_parent_kind_ord ON nodes(parent_nid, kind_id, ord);
CREATE INDEX IF NOT EXISTS idx_source_file ON nodes(source_file) WHERE source_file IS NOT NULL;
```

Names and paths are not stored per row: an AST row's display name derives
from `kind_id` + `ord` (`v_node_name`), a filesystem row's from `name_id`.
`record_id` and `source_file` are nullable; ley-line's writers leave them
`NULL` and mache populates them.

Every crate that reads or writes arena data uses this schema. `leyline-ts`
adds `_ast`, `_ast_blob`, `_source`, `node_refs`, `node_defs`, `_imports`
and `_cfg`; `leyline-lsp` adds `_lsp`, `_lsp_defs`, `_lsp_refs`,
`_lsp_hover` and `_lsp_completions`. All of them key by `nid`, `file_id`
or the file's relative path — exactly the three shapes `FILE_KEYED_TABLES`
enumerates. `_meta.projection_schema_version` is `projection-v6`; a binary
refuses an older label at parse open and asks for a cold reparse.
