# leyline-lsp

LSP client — spawn language servers and project their analysis into SQLite.

## What's here

- **`LspClient`** — async client that spawns any LSP server over stdio, performs the JSON-RPC handshake, and provides typed request methods:
  - `document_symbols(uri)` — hierarchical symbol tree
  - `definition(uri, line, col)` — go-to-definition locations
  - `references(uri, line, col)` — find all references
  - `hover(uri, line, col)` — type signatures and documentation
  - `completion(uri, line, col)` — completion items
  - `drain_notifications()` — collect pushed diagnostics
- **`protocol`** — JSON-RPC framing (Request, Notification, Response) + re-exports from `lsp-types` 0.95.
- **`project`** — two projection modes:
  - **Standalone** — creates `/symbols/…` hierarchy + `/diagnostics/{severity}/…` tree in the `nodes` table.
  - **Merge** — enriches an existing tree-sitter AST database with LSP semantics via line-range matching.

## SQLite tables

| Table              | Purpose                                                |
| ------------------ | ------------------------------------------------------ |
| `_lsp`             | Symbol kind, detail, line ranges, diagnostics per node |
| `_lsp_defs`        | Go-to-definition results (node → definition locations) |
| `_lsp_refs`        | Find-references results (node → reference locations)   |
| `_lsp_hover`       | Hover text per node                                    |
| `_lsp_completions` | Completion items per position                          |

## Data flow

```
gopls / pyright / rust-analyzer (JSON-RPC stdio)
  → LspClient (spawn, handshake, query)
  → DocumentSymbol[] + Diagnostic[] + Hover + Location[]
  → project_lsp() or merge_lsp_into_ast()
  → SQLite (nodes + _lsp* tables)
  → ley-line arena → NFS/FUSE mount
  → agents read /symbols/*, /diagnostics/*
```

## Usage (via CLI)

```bash
# Standalone
leyline lsp --server pyright-langserver --server-args "--stdio" \
  --input app.py --output lsp.db

# Merge with tree-sitter AST
leyline lsp --server pyright-langserver --server-args "--stdio" \
  --input app.py --output merged.db --merge-db ast.db
```
