# Table Contract: Schema Partition for Enrichment Layers

> **What this document is (2026-07-27, [ADR-0032](adr/0032-declared-decompositions.md) §D4):**
> the tables described here are the **SQL projection ABI**. That ABI is a
> *contract* — it is authoritative for which tables, columns, and indexes a
> consumer may rely on. It is **not an identity domain**: it has no root hash,
> it must never be given one, and it must not be described as "the substrate".
> Byte integrity belongs to the **SQLite arena snapshot root**; parse-run
> provenance belongs to the **Cap'n Proto segment root**; a single payload
> belongs to a **blob hash**. See
> [ARCHITECTURE.md § Authority model](ARCHITECTURE.md#authority-model), which is
> the arbiter if this document appears to disagree with it.
>
> **Two different things are both real, and conflating them is the historical
> defect this note exists to fix.** The SQL projection ABI is a live contract:
> mache opens the projection and reads `nodes.record` directly, no cgo
> (`mache/internal/ingest/ast_walker_nodes.go:23`). Separately, the *typed wire*
> contract is the Cap'n Proto schemas in `rs/ll-core/schema-capnp/schemas/`
> (`AstNode`, `SourceFile`, `BindingRecord`, `Head`). Neither displaces the
> other; "SQL is not the contract" was always too strong, and "the .db file is
> the contract" was always too strong in the other direction.
>
> `_lsp_refs` in particular is **read-only legacy** as of T8.9 (commit
> `9d3a3b4`). New LLO writes go to `${db}.bindings.capnp` exclusively;
> the table DDL is retained only so consumers reading pre-T8.9 `.db`
> files can still SELECT against it.

The living database is the union of tables owned by independent enrichment
layers. Each layer owns a disjoint set of tables — no two layers write to
the same table. This is the **Schema Partition Invariant**.

## Who READS each table (measured, 2026-08-19)

This document has always recorded which layer OWNS and WRITES each table — the
Schema Partition Invariant below. It recorded nothing about who READS them, and
that asymmetry has cost real design time: the node-key work (bead
`ley-line-open-17c271`) had to reconstruct the read surface by grepping a
consumer repo, and the consumer had to answer four questions LLO could not
answer about its own ABI. A producer contract is only half a contract.

Counts are non-test, non-writer `FROM`/`JOIN` references at the stated commit.
They are a floor, not an audit: dynamic SQL and consumers outside this
workspace are invisible to them. Re-derive rather than trust when it matters.

| Table | LLO readers | Notes |
|-------|------------:|-------|
| `nodes` | 41+ | Filesystem projection (`fs/src/graph.rs`), `ts/pyproject.rs`, `ll-core/schema`. Load-bearing for mount — NOT droppable. |
| `_ast` | 34 | `fs/src/graph.rs`, `ts/src/splice.rs`, `lsp/src/project.rs`, `daemon/ops.rs`. `find_definition` stopped needing it at `projection-v3`, but the mount and splice paths still read it directly. |
| `node_defs` / `node_refs` | 32 | The symbol layer. Also the agent-facing one: `find_definition` returns `node_id` STRINGS straight through MCP, so these ids are user-visible output, not an internal key. |
| `source_blobs` | 11 | |
| `node_content` | **0** | No SELECT anywhere. Survives as the FK target of `node_defs`/`node_refs.node_hash`. |
| `node_child` | **0** | No SELECT, and nothing FKs into it. 3.41M rows / 405 MB on an 8000-file arena. |
| `capnp_blobs` | **0** | No SELECT. Load-bearing only for the ADR-0026 §6.F1 resolution capability via `_ast_blob`. |
| `_ast_blob` | **0** | Same — written and gated by F1, not queried. |

A zero here means "nothing in this workspace SELECTs it", which is not the same
as "safe to delete": three of the four zeros are load-bearing for an FK or an
ADR-stated capability. It does mean the row count is not being paid for a query.

### Known external consumers

mache reads this projection directly (no cgo) and is the reason several of these
columns exist. Its measured dependencies, as of `mache-93e84b`:

- `node_id` is rendered to agents through MCP (`find_definition`, `get_impact`,
  `get_dataflow`). Any change to its shape must retain a way to reconstruct the
  displayed path. `projection-v5` honors this at the DAEMON boundary: ops keep
  accepting and emitting display-path strings, resolved/rendered via
  `resolve_path`/`node_path`; only the stored key changed. mache's DIRECT SQL
  reads (~900 sites, `mache-93e84b`) migrate with the projection: `parent_id`
  → `parent_nid`, `ORDER BY name` → `ORDER BY ord` (fixes the live ≥10-sibling
  ordering bug), the prefix-LIKE ancestry scan
  (`internal/smells/smell_incremental.go:245`) → span containment, and
  `internal/fixturedb` regenerates against the v5 DDL.
- `parent_id` direct-child listing (`WHERE parent_id = ?`, backing every
  `list_directory`) becomes `WHERE parent_nid = ?` with names joined from
  `v_node_name`. Depth-1 is still a stored relation — `parent_nid` is a real
  column as of `projection-v5` (the v4 derived-from-id trick is meaningless
  for an integer key).
- `internal/fixturedb/schema_leyline.go` pins this DDL byte-identically and has
  a conformance test, so any column change here surfaces there as drift at
  whatever release mache re-pins to.

## Layer Ownership

### Tree-sitter (base layer, LLO)

Produced by `parse_into_conn`. Always present.

As of `projection-v5` (bead `ley-line-open-17c271`) the node key is a
file-scoped integer: `nid = (file_id << 24) | ordinal` for files (ordinal 0 =
the file's own node, the AST root) and their AST nodes; `nid = -dir_id` for
directories. `ordinal` is the pre-order rank within the file's parse — the
same dense `0..n-1` the pointer store addresses blobs with, so
`nid & 0xFFFFFF` IS the blob index. Per-file scoping everywhere is
`nid BETWEEN (file_id<<24) AND (file_id<<24)|0xFFFFFF` — a PK range SEARCH,
never a prefix-LIKE. Display paths are DERIVED: `v_node_path`/`v_node_name`
(bulk views) and `node_path`/`resolve_path` (point resolvers, in
`leyline-schema`). A pre-v5 arena is refused at parse open and rebuilt cold;
the projection is derived-only, so no in-place migration exists.

| Table | Purpose |
|-------|---------|
| `names` / `dirs` / `files` / `kinds` | Interning tables (`projection-v5`). Every path component, directory link, file, and tree-sitter kind stored ONCE; all four are append-only — rows are never deleted or renumbered, which is what makes `file_id` reuse impossible and a directory rename a ONE-row `UPDATE dirs SET name_id`. `dirs.dir_id = 1` is the root ("", parent NULL). |
| `nodes` | Hierarchical node tree (nid, parent_nid, name_id, kind_id, kind, ord, size, mtime, record). `name_id` interned for filesystem rows, NULL for AST rows — an AST node's display name derives from its kind + per-kind rank among siblings by `ord` (`{raw_kind}[_{k}]`, the pre-v5 `needs_suffix` scheme, now computed at read time). `ord` is the sibling index in SOURCE order — `ORDER BY ord` is the correct sibling ordering (`ORDER BY name` put `_10` before `_2`). |
| `_ast` | AST positions (nid → kind_id, byte/row/col ranges, node_hash). No `source_id` (the file is `nid >> 24`), no `blob_ord` (the ordinal is `nid & 0xFFFFFF`). |
| `_source` | Source file metadata (id → language, abs path, `file_id` — the interned integer that keys every node row's high bits) |
| `node_refs` | Token references (token → nid, node_hash, container_nid, qualifier, and since `projection-v3` the occurrence's own `node_kind` + `start_byte`/`end_byte`/`start_row`/`start_col`/`end_row`/`end_col`). `qualifier` (v0.7.9, bead `ley-line-open-4dde42`) = receiver/selector text on the BARE-token row of a qualified call's dual-emit pair (`fmt.Println(..)` → the `Println` row carries `'fmt'`); NULL on the qualified-token row and on bare calls. Injected-subtree occurrences carry real nids past their host file's `_ast` count — fact-row keys with no `nodes`/`_ast` row, exactly as their path-shaped ids had no rows before. |
| `node_defs` | Token definitions (token → nid, node_hash, container_nid, canonical_kind, and since `projection-v3` the occurrence's own `node_kind` + span columns as on `node_refs`). The span is carried here rather than JOINed from `_ast` — SCIP's `Occurrence` shape (bead `ley-line-open-b4509b`). NULL span means the locator has no `_ast` row (injected nodes). No `qualifier` column. |
| `_imports` | Import statements (alias, path, source_id) |
| `_file_index` | Incremental parse index (path → mtime, size) |
| `_meta` | Key-value metadata (source_root, parse_time, version vectors) |

**Merkle-AST IR** (added v0.6.0 per ADR-0027):

| Table | Purpose |
|-------|---------|
| `node_content` | One row per UNIQUE subtree, keyed on `node_hash BLOB PRIMARY KEY` (κ kind + terminal + child hashes). `INSERT OR IGNORE` dedups byte-identical subtrees cross-file. |
| `node_child` | Git-tree object: (parent_hash, ordinal) → child_hash edges. Both endpoints REFERENCE `node_content(node_hash)` (FK-enforced). |

**Pointer store** (added v0.6.0 per ADR-0026 Phase 1 dual-write):

| Table | Purpose |
|-------|---------|
| `capnp_blobs` | Content-addressed blob store — `blob_hash BLOB PRIMARY KEY`, `blob_bytes BLOB`. |
| `_ast_blob` | File-to-blob map — `file_id INTEGER PRIMARY KEY` (as of `projection-v5`), `blob_hash BLOB`. ONE row per source file. Replaced the row-per-AstNode `_ast_pointer` in `projection-v2` (bead `ley-line-open-17c271`): that table carried a node_id and source_id already on the `_ast` row, a blob_hash with only 8000 distinct values across 3.15M rows, and an offset that was provably the dense array index — ~294 bytes to address a ~370-byte record. Resolution is `nid >> 24 → blob_hash`, indexed at `nid & 0xFFFFFF`. |

**Source blobs** (added v0.6.0 per ADR-0028 Phase 1 dual-store):

| Table | Purpose |
|-------|---------|
| `source_blobs` | Content-addressed byte store — `blob_hash BLOB PRIMARY KEY`, `blob_bytes BLOB`, `byte_len INTEGER` (stored generated column). F-git compat with `git cat-file blob`. |

**CDC derived chunk cache** (per `rs/ll-open/fs/src/chunked.rs:83-121`, activated by `daemon --cdc` / `leyline cdc`):

These tables are **private and derived**. They are not part of the SQL projection
ABI, no consumer reads them (mache reads none of them), and changes to them do
not bump `leyline-schema`. `nodes.record` stays authoritative; a missing or stale
manifest falls back to it (`chunked.rs:936-946`, freshness gate `:789`).

| Table | Purpose |
|-------|---------|
| `content_chunks` | Content-defined chunks keyed by blob hash, 8–128 KiB (`rs/ll-open/cdc/src/lib.rs:29-40`). Reads verify σ before returning and fail closed on mismatch (`chunked.rs:166-183`). |
| `content_manifest` | Per-node chunk sequence, keyed `(node_id, seq)`. **Has no root** — it is a lookup index, not an identity domain. ADR-0032 §D3's `manifestRoot` is proposed, not implemented. |
| `content_manifest_meta` | Freshness witness per node — the basis a manifest was built against, so a stale manifest is refused rather than trusted. |

**Analysis substrate** (added v0.7.2 per `docs/decades/analysis-substrate.md`) — a *derived analysis facility*, not an identity domain; the word "substrate" here is the decade's name, not a claim about content addressing:

| Table | Purpose |
|-------|---------|
| `_cfg` | Intra-procedural CFG basic blocks — `(node_hash, block_id) PRIMARY KEY`, `block_kind TEXT` (one of `CFG_CANONICAL_KINDS`), `complexity INTEGER` nullable. Node_hash FK to `node_content`. Reflow-invariant identity via merkle-AST. |
| `_cfg_edge` | Directed edges between CFG blocks. Composite FK on both endpoints referencing `_cfg(node_hash, block_id)`. `edge_kind TEXT` (free-form in v0.7.2; T3 taint will canonicalize). |

Population TODO (bead `ley-line-open-a0fadd`): T1.b3-followup wires `cfg::emit_cfg_for_source` into `cmd_parse`'s rayon-worker + batched-insert plumbing so `_cfg` populates on parse. Until then, schema is present but empty on parse output; `_cfg` is only populated by consumers calling `leyline_ts::cfg::emit_cfg_for_source` directly.

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
| `_cfg*` | analysis-substrate CFG layer (leyline-ts; T1 of `analysis-substrate` decade) |
| `_dfg*` | analysis-substrate DFG layer (T2 of `analysis-substrate` decade; not yet shipped) |
| `_taint*` | analysis-substrate taint fixpoint (T3 of `analysis-substrate` decade; not yet shipped) |
| `node_content` / `node_child` | ADR-0027 merkle-AST IR (base; no prefix) |
| `_ast_blob` / `capnp_blobs` | ADR-0026 pointer store (base; no prefix on blobs) |
| `names` / `dirs` / `files` / `kinds` | projection-v5 interning tables (base; no prefix) |
| `source_blobs` | ADR-0028 content-addressed source (base; no prefix) |
| `content_chunks` / `content_manifest` / `content_manifest_meta` | CDC derived chunk cache — private, not part of the SQL projection ABI |

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
  living[Living database<br/>:memory: SQLite + Mutex<br/>arena flip on snapshot] --> mache_sql[mache: opens the db,<br/>reads nodes.record<br/>SQL projection ABI]
  capnp --> mache_capnp[mache: BindingRecord<br/>pure-Go capnp decode]
  capnp_ast -.->|not read by mache today| mache_capnp
  classDef llo fill:#0b3d2e,stroke:#1ed896,color:#e8f7ee;
  classDef llp fill:#2a1245,stroke:#a06bff,color:#ede1ff;
  classDef capnpwire fill:#1a2747,stroke:#5a8eed,color:#e3edff;
  class ts,lsp llo;
  class embed,sheaf llp;
  class capnp,capnp_ast,mache_capnp capnpwire;
```

Pre-T8 the living database was the only cross-process surface, and SQL column
names were the whole contract. Post-T8 (2026-05-08) a second, typed contract was
added alongside it: canonical-encoded capnp segment files at
`${db}.{bindings,ast,source,head}.capnp`. Both are live today.

What mache actually reads, as of `leyline-schema v0.10.3` (verified, because this
paragraph previously overstated it):

| Surface | Read by mache? | Evidence |
|---|---|---|
| SQL projection ABI (`nodes` ⋈ `_ast`, incl. `nodes.record`) | **yes**, opens the db directly | `mache/internal/ingest/ast_walker_nodes.go:23` |
| `${db}.bindings.capnp` | **yes**, pure-Go capnp decode | `mache/internal/lsp/binding_log.go:97,159-166` |
| `ast.capnp` / `source.capnp` / `head.capnp` | **no** — marked "future" in mache | `mache/internal/lsp/binding_log.go:1-2` |
| Daemon ops over UDS | **yes** — the dominant live-query path | `mache/internal/leyline/socket.go` |
| CDC tables | **no** | no reference anywhere in the mache tree |

## Cross-runtime drift gates

Two distinct cross-process contracts run through this repo, each gated by a
cross-runtime fixture suite in CI:

| Surface | Encoding | Rust fixtures | Go gate | Bead |
|---|---|---|---|---|
| **Cap'n Proto segment root** — capnp segment files (`bindings.capnp`, `ast.capnp`, `source.capnp`, `head.capnp`) | canonical capnp binary | `rs/ll-core/schema-capnp/tests/fixtures/*.bin` | `clients/go/leyline-schema/binding/binding_test.go` decodes via the typed capnp Go bindings | T8.10 / `6b7d43` |
| **Daemon protocol** — UDS request/response JSON per `daemon.capnp` | JSON-as-carrier (per cloister `interlace-spec/0.1.0/README.md`) over UDS | `rs/ll-open/cli-lib/tests/fixtures/daemon-protocol.json` | `clients/go/leyline-schema/daemon/daemon_protocol_test.go` decodes via hand-written JSON-tagged structs that mirror `daemon.capnp` | A-1 / `b5a77b` |

Both gates are wired into `.github/workflows/leyline-schema-go.yml`. The
Cap'n Proto segment root gate asserts byte-equality on canonical encoding (T8.10's
falsifiable claim F8.6.4 — direct: byte-equal decode in both runtimes).

The daemon protocol gate is a **two-step chain** through the fixture file,
because the JSON wire is built by handlers at runtime (not byte-equal):

1. **Rust half (runtime):** spawns the daemon, sends each fixture's
   request, asserts the live response contains every required key.
   Pins **handler ↔ fixture**.
2. **Go half (offline):** strict-unmarshals each fixture's `response`
   payload into the matching typed Go binding. No daemon round-trip.
   Pins **fixture ↔ schema**.

Composing the two yields **handler ↔ schema** transitively. Either half
failing means the chain broke. The fixture file is the deliberate
intermediate artifact, not an artifact of the implementation.

Ops with known schema↔reality drift (`get_node` snake_case, `status` missing
fields, etc.) are SKIPPED in the Go half with the drift reason as the skip
message; the Rust half still runs for them. Bead A-2 (`b631c8`) reconciles
the schema additively; each `go_drift_skip` flipping to null converts a
skip to a pass.

## Rules

1. **Disjoint writes**: `A.writes() ∩ B.writes() = ∅` for any two passes A, B.
2. **Atomic layer writes**: All writes from a single pass run in one SQLite transaction.
3. **Causal basis**: Each layer records `{name}_parse_basis` in `_meta` — the
   `parse_version` it was computed against. Staleness = basis < current parse_version.
4. **Optional tables**: Consumers (mache, FUSE) must handle missing enrichment
   tables gracefully. Check `SELECT 1 FROM sqlite_master WHERE name = ?`.
5. **SQL projection ABI**: LL and LLO may add/query documented tables in the
   same SQLite projection without treating the database file as a content
   identity. Per ADR-0032 §D4 the SQL projection ABI is authoritative for
   tables, columns, and indexes; it **has no root and must never be given
   one**, and it must not be called "the substrate". The identity domains are
   elsewhere and stay separate: the **SQLite arena snapshot root**
   (`current_root`) is authoritative for the byte image of one snapshot and for
   nothing about logical content; a **blob hash** is authoritative for one
   payload; the **Cap'n Proto segment root** (`Head.rootHash`) is authoritative
   for which parse run produced a set of segments and must not claim to name
   the same thing as `current_root`.
6. **Private derived tables are not ABI**: the CDC tables (`content_chunks`,
   `content_manifest`, `content_manifest_meta`) are a derived cache. They may
   change shape without a `leyline-schema` bump, no consumer may depend on
   them, and `nodes.record` remains authoritative for their contents.
