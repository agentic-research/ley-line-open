# Table Contract: Schema Partition for Enrichment Layers

> **Post-T8 status (2026-05-08, ADR-0014):** the SQL tables described
> here are *local projections*, not the cross-process contract. The
> typed Cap'n Proto schemas in `rs/ll-core/schema-capnp/schemas/`
> (`AstNode`, `SourceFile`, `BindingRecord`, `Head`) are the contract
> consumed by mache, future workerd, and future control-room. SQL
> tables are an optimization for in-process queries; column names are
> not the protocol.
>
> `_lsp_refs` in particular is **read-only legacy** as of T8.9 (commit
> `9d3a3b4`). New LLO writes go to `${db}.bindings.capnp` exclusively;
> the table DDL is retained only so consumers reading pre-T8.9 `.db`
> files can still SELECT against it.

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
| `_lsp_refs` | **Legacy / read-only.** Find-references results (node_id → ref_uri, line/col). New writes retired at T8.9 (`9d3a3b4`); contract migrated to `BindingRecord` capnp event log at `${db}.bindings.capnp`. DDL retained for legacy `.db` read compatibility. |
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

```mermaid
flowchart TD
  subgraph LLO[ley-line-open]
    direction TB
    ts[TreeSitterPass<br/>owns: nodes, _ast, _source,<br/>node_refs, node_defs, _imports]
    lsp[LspEnrichmentPass<br/>owns: _lsp, _lsp_defs,<br/>_lsp_hover, _lsp_completions<br/><i>+ BindingRecord capnp log</i>]
  end
  subgraph LL[ley-line · private]
    direction TB
    embed[EmbeddingPass<br/>owns: node_embeddings<br/>sidecar .vec.db]
    sheaf[SheafPass<br/>owns: _sheaf*<br/>depends: tree-sitter]
  end
  ts --> living
  lsp --> living
  embed --> living
  sheaf --> living
  lsp --> capnp[(${db}.bindings.capnp<br/>BindingRecord log)]
  ts --> capnp_ast[(${db}.ast.capnp<br/>${db}.source.capnp<br/>${db}.head.capnp)]
  living[Living database<br/>:memory: SQLite + Mutex<br/>arena flip on snapshot] --> mache_sql[mache: generation poll<br/>SQL projection]
  capnp --> mache_capnp[mache: BindingRecord<br/>cross-runtime contract]
  capnp_ast --> mache_capnp
  classDef llo fill:#0b3d2e,stroke:#1ed896,color:#e8f7ee;
  classDef llp fill:#2a1245,stroke:#a06bff,color:#ede1ff;
  classDef substrate fill:#1a2747,stroke:#5a8eed,color:#e3edff;
  class ts,lsp llo;
  class embed,sheaf llp;
  class capnp,capnp_ast,mache_capnp substrate;
```

Pre-T8 the living database was the only cross-process surface, and SQL column
names were the contract. Post-T8 (this commit thread, 2026-05-08), the living
db remains for fast in-process queries, but the *cross-process contract*
moved to canonical-encoded capnp segment files at `${db}.{bindings,ast,source,head}.capnp`.
mache (and any future consumer) reads those directly.

## Rules

1. **Disjoint writes**: `A.writes() ∩ B.writes() = ∅` for any two passes A, B.
2. **Atomic layer writes**: All writes from a single pass run in one SQLite transaction.
3. **Causal basis**: Each layer records `{name}_parse_basis` in `_meta` — the
   `parse_version` it was computed against. Staleness = basis < current parse_version.
4. **Optional tables**: Consumers (mache, FUSE) must handle missing enrichment
   tables gracefully. Check `SELECT 1 FROM sqlite_master WHERE name = ?`.
5. **The .db file is the contract**: LL opens the .db, adds its tables, closes.
   LLO never needs to know about LL's tables.
