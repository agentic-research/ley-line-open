# Table Contract: Schema Partition for Enrichment Layers

The living database is the union of tables owned by independent enrichment
layers. Each layer owns a disjoint set of tables — no two layers write to
the same table. This is the **Schema Partition Invariant**.

## Layer Ownership

### Tree-sitter (base layer, LLO)

Produced by `parse_into_conn`. Always present.

| Table | Purpose |
|-------|---------|
| `nodes` | Hierarchical node tree (id, parent_id, name, kind, size, record) |
| `_ast` | AST positions (node_id → source_id, node_kind, byte/row/col ranges) |
| `_source` | Source file metadata (id → language, abs path) |
| `node_refs` | Token references (token → node_id, source_id) |
| `node_defs` | Token definitions (token → node_id, source_id) |
| `_imports` | Import statements (alias, path, source_id) |
| `_file_index` | Incremental parse index (path → mtime, size) |
| `_meta` | Key-value metadata (source_root, parse_time, version vectors) |

### LSP enrichment (extension layer)

Produced by `LspEnrichmentPass` (registered via `DaemonExt::enrichment_passes`).
Depends on tree-sitter. Tables are optional — queries degrade gracefully.

| Table | Purpose |
|-------|---------|
| `_lsp` | Core symbol metadata (node_id, symbol_kind, detail, line ranges, diagnostics) |
| `_lsp_defs` | Go-to-definition results (node_id → def_uri, line/col) |
| `_lsp_refs` | Find-references results (node_id → ref_uri, line/col) |
| `_lsp_hover` | Hover documentation (node_id → hover_text) |
| `_lsp_completions` | Completion items (node_id → label, kind, detail) |

### Embeddings (extension layer)

Produced by `EmbeddingPass`. Depends on tree-sitter. Stored in a **sidecar
database** (not the living db) because `vec0` virtual tables cannot survive
`sqlite3_serialize`/`deserialize`.

| Table | Database | Purpose |
|-------|----------|---------|
| `node_embeddings` | sidecar `.vec.db` | vec0 virtual table (node_id → float[N] embedding) |

### Reserved prefixes

| Prefix | Owner |
|--------|-------|
| `_ast*` | tree-sitter layer |
| `_lsp*` | LSP layer |
| `_vec*` | embedding layer |
| `_sheaf*` | sheaf cache (ley-line private) |
| `_errors` | validation layer (leyline-fs write path) |

## Composition Model

```
ley-line-open (LLO)              ley-line (LL, private)
┌──────────────────┐            ┌──────────────────────┐
│ TreeSitterPass    │            │ LspEnrichmentPass    │
│ owns: nodes, _ast│            │ owns: _lsp*          │
│       _source,   │            ├──────────────────────┤
│       node_refs, │            │ EmbeddingPass        │
│       node_defs, │            │ owns: node_embeddings│
│       _imports   │            │ (sidecar db)         │
└────────┬─────────┘            ├──────────────────────┤
         │                      │ SheafPass            │
         │ depends_on: []       │ owns: _sheaf*        │
         │                      └──────┬───────────────┘
         │                             │ depends_on: ["tree-sitter"]
         ▼                             ▼
   ┌─────────────────────────────────────┐
   │        Living Database              │
   │  :memory: SQLite (Mutex)            │
   │  ───────────────────────────        │
   │  serialize() → arena (crash safe)   │
   │  arena → mache (generation poll)    │
   └─────────────────────────────────────┘
```

## Rules

1. **Disjoint writes**: `A.writes() ∩ B.writes() = ∅` for any two passes A, B.
2. **Atomic layer writes**: All writes from a single pass run in one SQLite transaction.
3. **Causal basis**: Each layer records `{name}_parse_basis` in `_meta` — the
   `parse_version` it was computed against. Staleness = basis < current parse_version.
4. **Optional tables**: Consumers (mache, FUSE) must handle missing enrichment
   tables gracefully. Check `SELECT 1 FROM sqlite_master WHERE name = ?`.
5. **The .db file is the contract**: LL opens the .db, adds its tables, closes.
   LLO never needs to know about LL's tables.
