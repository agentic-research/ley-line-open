# Inline Ref Extraction During Parse (Go First)

**Date:** 2026-04-10
**Status:** Approved
**Bead:** ley-line-open-371bca (cross-ref extraction)
**Depends on:** CLI extensibility (complete), daemon (complete)

## Problem

Mache's `SitterWalker` uses CGO tree-sitter to extract call sites, definitions, and imports at mount time. This forces mache to link C tree-sitter grammars and parse source files in Go. The goal is to move all tree-sitter work into LLO's `leyline parse` so mache never touches tree-sitter.

Currently `leyline parse` produces `nodes`, `_ast`, `_source`. It does not produce `node_refs` (callers), `node_defs` (definitions), or import mappings.

## Design Principle

Parse once in Rust. Mache reads tables. No CGO, no kernel context switches, no C in the Go binary.

## Architecture

### New Tables

```sql
CREATE TABLE IF NOT EXISTS node_refs (
    token TEXT NOT NULL,
    node_id TEXT NOT NULL,
    source_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_refs_token ON node_refs(token);
CREATE INDEX IF NOT EXISTS idx_refs_node ON node_refs(node_id);

CREATE TABLE IF NOT EXISTS node_defs (
    token TEXT NOT NULL,
    node_id TEXT NOT NULL,
    source_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_defs_token ON node_defs(token);

CREATE TABLE IF NOT EXISTS _imports (
    alias TEXT NOT NULL,
    path TEXT NOT NULL,
    source_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_imports_source ON _imports(source_id);
```

### Extraction Rules (Go)

During `walk_children()`, when encountering these `node_kind` values on the tree-sitter CST:

| CST node_kind | What to extract | Target table |
|---|---|---|
| `function_declaration` | First `identifier` child → token is the function name | `node_defs` |
| `method_declaration` | `field_identifier` child → token is the method name | `node_defs` |
| `type_spec` (inside `type_declaration`) | `type_identifier` child → token is the type name | `node_defs` |
| `call_expression` with `identifier` child | The identifier text → simple call | `node_refs` |
| `call_expression` with `selector_expression` child | `field_identifier` child → call token; `identifier` sibling → package qualifier | `node_refs` (token = "pkg.Func") |
| `import_spec` | `path` child (string literal, strip quotes) → import path; optional `name` child → alias (default: last path segment) | `_imports` |

### Where the Code Lives

**`leyline-ts` crate** (library, not CLI):

- `rs/ll-open/ts/src/refs.rs` — New module. Per-language ref extraction functions. Operates on a tree-sitter `Node` + source bytes + connection. Does NOT walk the tree — called by the walker when it hits a relevant node.
  - `pub fn extract_go_refs(node: &Node, source: &[u8], node_id: &str, source_id: &str, conn: &Connection) -> Result<()>`
  - Inspects `node.kind()`, extracts children, inserts into `node_refs`/`node_defs`/`_imports`

- `rs/ll-open/ts/src/schema.rs` — Add schema + insert functions:
  - `create_refs_schema(conn)` — creates `node_refs`, `node_defs`, `_imports` tables
  - `insert_ref(conn, token, node_id, source_id)`
  - `insert_def(conn, token, node_id, source_id)`
  - `insert_import(conn, alias, path, source_id)`

**`leyline-cli-lib` crate** (CLI):

- `rs/ll-open/cli-lib/src/cmd_parse.rs` — Modify `walk_children()` to call `extract_go_refs()` for each named node when the language is Go. Call `create_refs_schema()` alongside `create_ast_schema()` at the start.

### Language Gating

`extract_go_refs()` is only called when the file's language is Go. Other languages get no refs (for now). This is explicitly not a generic system — each language gets its own extraction function as needed.

Future: `extract_python_refs()`, `extract_js_refs()`, etc. Each is a separate bead.

### Mache Compatibility

Mache already reads `node_refs` and `node_defs` in the fast path:
- `sqlite_graph.go:149` — skips sidecar when `node_refs` exists in main DB
- `sqlite_graph.go:674` — `getCallersFromMainDB()` queries `node_refs`
- `sqlite_graph.go:820` — queries `node_defs` for definition resolution

The `_imports` table is new. Mache doesn't read it yet. It enables qualified call resolution (`auth.Validate` → look up `auth` alias → find the package → find `Validate` in that package's defs). That's a mache-side follow-up.

### Zero-Copy Path

All tables go into the same SQLite database. The serialize → arena → deserialize round-trip works unchanged. No sidecar files, no external state.

### Testing

1. **Unit test in leyline-ts:** Parse a Go file with functions, method, types, calls, imports. Verify `node_refs`, `node_defs`, `_imports` have correct rows.
2. **Integration test in cli-lib:** Parse a multi-file Go project. Verify cross-file refs (file A calls function defined in file B → both appear in tables).
3. **Mache fixture test:** Use the existing `mache_compat_test` fixture to verify mache can read the new tables from an LLO-produced .db.

## What This Does NOT Cover

- Languages other than Go (separate beads per language)
- Mache-side `_imports` reading (mache follow-up)
- Address refs / HCL-style config refs (separate concern)
- LSP enrichment (_lsp* tables — separate bead ley-line-open-3701d6)
