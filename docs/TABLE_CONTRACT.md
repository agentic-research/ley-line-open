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

Counts are non-test, non-writer `FROM`/`JOIN` references as of 2026-08-19 (the `node_content` and `node_child` rows re-checked at integration `22290da9`).
They are a floor, not an audit: dynamic SQL and consumers outside this
workspace are invisible to them. Re-derive rather than trust when it matters.

| Table | LLO readers | Notes |
|-------|------------:|-------|
| `nodes` | 41+ | The nodes-table graph (`fs/src/graph.rs`: the FFI mache links, vcs), `ts/pyproject.rs`, `ll-core/schema`. NOT droppable. The host mount no longer reads it: since ADR-0045 T8 it serves `_tree` through storefs. |
| `_ast` | 34 | `fs/src/graph.rs`, `ts/src/splice.rs`, `lsp/src/project.rs`, `daemon/ops.rs`. `find_definition` stopped needing it at `projection-v3`, but the graph and splice paths still read it directly. |
| `node_defs` / `node_refs` | 32 | The symbol layer. Also the agent-facing one: `find_definition` returns `node_id` STRINGS straight through MCP, so these ids are user-visible output, not an internal key. |
| `source_blobs` | 11 | |
| `node_content` | **0** | No SELECT anywhere outside the writer. Survives as the FK target of `_ast`/`node_defs`/`node_refs.content_id` (`projection-v10`) and `node_edge`, and as the one place `node_hash` is stored. |
| `node_child` | **0** | No SELECT, and nothing FKs into it. A view over `node_edge` as of `projection-v9`; 4.35M edges on the 8000-file kibana slice, 515 MB hash-keyed before interning. **Do not treat as droppable**: its `field` column is the ONLY place a tree-sitter field name survives, and mache needs it (`ley-line-open-87ff3a`). See "Recovering a node's tree-sitter field" below. |
A zero here means "nothing in this workspace SELECTs it", which is not the same
as "safe to delete": `node_content` is an FK target and `node_child` carries the
field names. It does mean the row count is not being paid for a query. The two
other zeros this table used to list, `capnp_blobs` and `_ast_blob`, were
retired at `projection-v8` (ADR-0040 D5, bead `ley-line-open-f30fdf`): they had
no reader in this workspace or in mache, and the capability that justified them
(ADR-0026 §6.F1 resolution of an `_ast` row into a capnp blob) had no caller.

`node_child` was the fourth — the one zero with no stated justification, and so
the obvious thing to delete when someone goes looking for row count. It now has
one: it is the sole carrier of tree-sitter field names, which nothing in this
workspace reads and an external consumer cannot reconstruct without it. That is
exactly the failure mode this table exists to prevent, arrived at from the other
direction — a reader count of zero measured INSIDE the workspace said "free to
drop" about the answer to a P1 request from outside it.

### Recovering a node's tree-sitter field

Tree-sitter distinguishes some children by FIELD rather than by kind. Rust's
`impl_item` is the motivating case: `impl Trait for Type` has two
`type_identifier` children, one under `trait:` and one under `type:`. Neither
node kind nor document order separates them, and `_ast` carries no field column
— so a consumer naming a method by its receiver gets the TRAIT for every trait
impl unless it reads the field.

`node_child.field` carries it, for every grammar. It is captured as
`TreeCursor::field_name()` in `cmd_parse.rs` and written with the edge, so it is
not Rust-specific: Go's `receiver:`, Python's `bases:` and TypeScript's
`implements:` all land the same way.

Join on the HASHES, scoped by the parent:

```sql
SELECT nc.field
  FROM node_child nc
 WHERE nc.parent_hash = :parent_node_hash
   AND nc.child_hash  = :child_node_hash;
```

As of `projection-v9` (bead `ley-line-open-9b7d28`) `node_child` is a view over
the interned `node_edge`, so the query above runs unchanged. The same lookup
against the stored table, for a reader that holds content ids or wants the plan
spelled out — each hash resolves through `node_content`'s `node_hash` index,
then one primary-key probe on `node_edge`:

```sql
SELECT e.field
  FROM node_edge e
  JOIN node_content p ON p.content_id = e.parent_id
  JOIN node_content c ON c.content_id = e.child_id
 WHERE p.node_hash = :parent_node_hash
   AND c.node_hash = :child_node_hash;
```

Two rules, both measured on a 460-file Rust arena at `projection-v5`:

1. **Never join on ordinal.** `nodes.ord` counts NAMED children only and is
   dense (0, 1, 2); `node_child.ordinal` counts ALL children including unnamed
   tokens such as `impl` and `for` (1, 3, 4). They do not correspond.
2. **`(parent_hash, child_hash)` is not a key.** A parent holding two
   identical-content children in different roles maps to more than one field:
   30 such pairs out of 204 947 (0.015%). For `impl_item` it is **zero** —
   Rust puts traits and types in one namespace, so `impl Foo for Foo` cannot
   exist — but a grammar that commonly repeats identical children under
   different fields would have a higher rate. Measure before relying on it as a
   general per-node lookup.

### A fielded child's grammar kind at `projection-v12`

`node_content` holds the κ `kind`, not the grammar kind or the language
(ADR-0047 D-iii): the same subtree appears in several files and languages, and
a content row cannot say which. Read a child's grammar kind from its
occurrence. A named child of the parent occurrence `:nid` is the `nodes` row
under it whose `_ast.content_id` is the edge's `child_id`; an anonymous child
(an operator, keyword or punctuation token) has no occurrence row, and its
grammar kind is its content row's `kind`, because the merkle-ast-v3 fold
applies κ to named nodes only (`an_anonymous_tokens_kind_is_its_grammar_kind`,
`rs/ll-open/cli-lib/tests/ir_producer.rs`; JavaScript's `class` keyword was
stored as κ `type` before v3):

```sql
SELECT e.field,
       COALESCE((SELECT k.raw_kind
                   FROM nodes n
                   JOIN _ast ca ON ca.nid = n.nid
                   JOIN kinds k ON k.kind_id = ca.kind_id
                  WHERE n.parent_nid = :nid AND ca.content_id = e.child_id
                  ORDER BY n.ord
                  LIMIT 1),
                c.kind) AS child_raw_kind
  FROM node_edge e
  JOIN node_content c ON c.content_id = e.child_id
 WHERE e.parent_id = :parent_content_id AND e.field IS NOT NULL AND e.field != ''
 ORDER BY e.ordinal;
```

`:parent_content_id` is the parent's `_ast.content_id`. Two named siblings with
the same `content_id` are the same κ tree, not necessarily the same grammar
kind: κ maps several grammar kinds to one. The query then returns the first
sibling's grammar kind for both. No case is known: every κ collapse class
checked differs in an anonymous keyword child (`struct` against `union`,
`const` against `static`), so its members never share a `content_id`; no test
pins that. `a_fielded_childs_grammar_kind_is_read_from_its_occurrence`
(`rs/ll-open/cli-lib/tests/ir_producer.rs`) checks this query against
tree-sitter on every fielded edge of a Rust, Go and Python tree, including
children whose κ kind differs (`parameters` → `parameter`).

An injected-language occurrence (bead `ley-line-open-c822a6`, e.g. SQL in a Go
string) has no `_ast` row: `_ast` is the host file's tree, and the host's
occurrence layer is independent of the injection pass. Its occurrence rows are
its `node_defs`/`node_refs` rows, and since `projection-v12` they carry
`kind_id` like `_ast` does (bead `ley-line-open-cc5c55`). So a def or ref's
language and grammar kind, host or injected, is one join:

```sql
SELECT k.lang, k.raw_kind
  FROM node_defs d JOIN kinds k ON k.kind_id = d.kind_id
 WHERE d.token = :token;   -- a Go file's injected `CREATE TABLE users` → ('sql', 'create_table')
```

For a host occurrence `node_defs.kind_id` equals its `_ast` row's `kind_id`
(`inj_injected_occurrence_carries_its_language_and_grammar_kind`,
`rs/ll-open/cli-lib/tests/injection_extraction_test.rs`). An injected node that
is neither a def nor a ref has no occurrence row; its content row holds its κ
kind.

### Reading `node_hash` at `projection-v10`

As of `projection-v10` (bead `ley-line-open-738dd9`) `_ast`, `node_refs` and
`node_defs` point at their subtree by `content_id`, the arena-local handle of
its `node_content` row, instead of storing the 32-byte `node_hash` on every
occurrence. `node_content.node_hash NOT NULL UNIQUE` is still the identity and
is stored once per distinct subtree; no hash moved. The v9 rule holds:
`content_id` never leaves the database. Two arenas number one subtree
differently, so compare across arenas by `node_hash` and never by id.

Measured on a cold parse of this repository's `rs/` tree (859 files parsed,
12.83 MB of source; dbstat, MiB):

| Object | v9 | v10 |
|---|---:|---:|
| `_ast` | 76.75 | 39.88 |
| `idx_ast_node_hash` → `idx_ast_content` | 55.06 | 18.25 |
| `node_refs` | 11.13 | 7.81 |
| `node_defs` | 2.81 | 2.06 |
| arena | 321.19 | 243.44 |

The `_ast`, `node_content`, edge, def, ref and def-to-ref join sets are equal
by hash between the two arenas (`EXCEPT` empty both ways).

The views `v_ast_hash`, `v_node_refs_hash` and `v_node_defs_hash` keep every v9
read working by renaming the table. Reads that should move to the base tables:

| v9 read | v10 read |
|---|---|
| `SELECT nid FROM _ast WHERE node_hash = :h` | `SELECT a.nid FROM node_content c JOIN _ast a ON a.content_id = c.content_id WHERE c.node_hash = :h` (the `node_hash` UNIQUE probe, then `idx_ast_content`) |
| `SELECT ..., a.node_hash FROM _ast a WHERE a.nid BETWEEN :lo AND :hi` | `SELECT ..., c.node_hash FROM _ast a LEFT JOIN node_content c ON c.content_id = a.content_id WHERE a.nid BETWEEN :lo AND :hi` (or `FROM v_ast_hash`, which plans the same) |
| `JOIN node_content c ON c.node_hash = a.node_hash` (likewise `r.`/`d.`) | `JOIN node_content c ON c.content_id = a.content_id` — an integer primary-key probe instead of a BLOB index probe |
| `LEFT JOIN node_content pc ON pc.node_hash = a.node_hash LEFT JOIN node_edge e ON e.parent_id = pc.content_id` | `LEFT JOIN node_edge e ON e.parent_id = a.content_id` |
| `d.node_hash = r.node_hash`, `GROUP BY a.node_hash` (one arena) | `d.content_id = r.content_id`, `GROUP BY a.content_id`, rendering the hash at the edge. Within one arena `content_id` and `node_hash` are in bijection, so the grouping is identical. |
| `_ast` on the RIGHT of a `LEFT JOIN`, reading `node_hash` | keep `LEFT JOIN _ast a` and add `LEFT JOIN node_content c ON c.content_id = a.content_id`; never `LEFT JOIN v_ast_hash`, which SQLite materializes whole |

`_cfg` / `_cfg_edge` keep `node_hash`: they are keyed BY a subtree (the
`(node_hash, block_id)` primary key is their dedup identity and `_cfg_edge`'s
FK target), where the occurrence tables point AT one. `leyline parse` does not
write them (bead `ley-line-open-a0fadd`), so they hold no bytes in a parsed
arena.

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
- `current_root` on the daemon wire (the `status`, `reparse`, `snapshot`,
  `enrich` and `load` responses and the `daemon.sheaf.invalidate`
  event) follows one absence rule: `root_published` is always present, and
  `current_root` is present exactly when it is `true`. When nothing is
  published the key is omitted; no zero or empty spelling stands in for it.
  On the event, an unreadable controller sends `root_error` in place of both
  (bead `ley-line-open-80f037`; the rule is on `StatusResponse.rootPublished`
  in `daemon.capnp`).
- A watcher of the daemon's `--source` checkout must treat `Rename` or
  `Remove` on a path that still exists as a change to that path: an accepted
  proposal lands by stage-then-rename (a hidden staging sibling renamed over
  the target, `rs/ll-open/cli-lib/src/checkout.rs:188` and `:575`), which
  fsnotify/kqueue reports as a rename or remove of the target (ADR-0045 T7,
  bead `ley-line-open-514d0a`).

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
| `names` / `dirs` / `files` / `kinds` | Interning tables (`projection-v5`). Every path component, directory link, file, and tree-sitter kind stored ONCE; all four are append-only — rows are never deleted or renumbered, which is what makes `file_id` reuse impossible and a directory rename a ONE-row `UPDATE dirs SET name_id`. The rule is enforced in the database: `*_append_only_delete` and `*_append_only_update` triggers abort any DELETE and any UPDATE that changes a row's id (`INTERN_GUARDS_DDL`, bead `ley-line-open-80f037`). `dirs.dir_id = 1` is the root ("", parent NULL). |
| `nodes` | Hierarchical node tree (nid, parent_nid, name_id, kind_id, kind, ord, size, mtime, record_id, record, source_file; ley-line's own writers leave `record_id` and `source_file` NULL, which remain for mache's lazy-resolution flow). `name_id` interned for filesystem rows, NULL for AST rows — an AST node's display name derives from its kind + per-kind rank among siblings by `ord` (`{raw_kind}[_{k}]`, the pre-v5 `needs_suffix` scheme, now computed at read time). `ord` is the sibling index in SOURCE order — `ORDER BY ord` is the correct sibling ordering (`ORDER BY name` put `_10` before `_2`). |
| `_ast` | AST positions (nid → kind_id, byte/row/col ranges, `content_id`). `content_id INTEGER REFERENCES node_content(content_id)` replaced the 32-byte `node_hash` at `projection-v10` (bead `ley-line-open-738dd9`); read the hash through `v_ast_hash` or a join (see "Reading `node_hash` at projection-v10" below), and find every location of a subtree with `idx_ast_content`. No `source_id` (the file is `nid >> 24`), no `blob_ord` (the ordinal is `nid & 0xFFFFFF`). |
| `_source` | Source file metadata (`id`, `language`, `content` (inline mode) or `path` (reference mode), `content_hash`, and `file_id` — the interned integer that keys every node row's high bits). As of `projection-v7` (ADR-0045 T1) one row per file the parse PROJECTED: a file of no grammar, a binary or an oversized file has a `_tree` entry and no `_source` row. `content_hash` equals the path's `_tree.blob_hash`. List every file from `_tree`, not from here. |
| `node_refs` | Token references (token → nid, `content_id` (`node_hash` before `projection-v10`), container_nid, qualifier, and since `projection-v3` the occurrence's own `node_kind` + `start_byte`/`end_byte`/`start_row`/`start_col`/`end_row`/`end_col`). `qualifier` (v0.7.9, bead `ley-line-open-4dde42`) = receiver/selector text on the BARE-token row of a qualified call's dual-emit pair (`fmt.Println(..)` → the `Println` row carries `'fmt'`); NULL on the qualified-token row and on bare calls. Injected-subtree occurrences carry real nids past their host file's `_ast` count — fact-row keys with no `nodes`/`_ast` row, exactly as their path-shaped ids had no rows before. `kind_id` (`projection-v12`, bead `ley-line-open-cc5c55`) → `kinds(lang, raw_kind)` is the occurrence's own language and grammar kind, written on every row the parse writes: its `_ast` row's `kind_id` for a host occurrence, the injected grammar's for an injected one. |
| `node_defs` | Token definitions (token → nid, `content_id` (`node_hash` before `projection-v10`), container_nid, canonical_kind, and since `projection-v3` the occurrence's own `node_kind` + span columns as on `node_refs`). The span is carried here rather than JOINed from `_ast` — SCIP's `Occurrence` shape (bead `ley-line-open-b4509b`). NULL span means the locator has no `_ast` row (injected nodes). `kind_id` as on `node_refs` (`projection-v12`). No `qualifier` column. |
| `_imports` | Import statements (alias, path, source_id) |
| `_meta` | Key-value metadata (source_root, parse_time, version vectors, and `derived_from` = `v2 <storeRoot hex> <parserId hex> <projection_schema_version>`, the derivation the projection reproduces under `leyline rebuild`; ADR-0040 D6, ADR-0047 D-ii. The leading `v2` is the format discriminator: the retired v1 value (three fields, `treeRoot` first, before projection-v11) is refused by version, never read as v2). `unbound_facts` = the count of `node_refs` rows whose token has no `node_defs` row, as the head's `unboundFacts` records it; written with the rows by every parse pass (bead `ley-line-open-8c8ded`) |

**Merkle-AST IR** (added v0.6.0 per ADR-0027):

| Table | Purpose |
|-------|---------|
| `node_content` | One row per UNIQUE subtree — `content_id INTEGER PRIMARY KEY` (as of `projection-v9`; arena-local, assigned by the writer, NOT an identity: two arenas number one subtree differently, and nothing outside the database may cite it), `node_hash BLOB NOT NULL UNIQUE` (κ kind + terminal + child hashes, keyed under `derive_key("leyline/ast-node/v3", …)` since merkle-ast-v3 — the identity, stored once per subtree; the FK target of `_cfg`, while `node_edge` (v9) and `_ast`/`node_defs`/`node_refs` (v10) reference `content_id`), `node_tag`, `kind`, `token`, `arity`. Every column is a function of `node_hash`: `raw_kind` and `lang` left at `projection-v12` (ADR-0047 D-iii, bead `ley-line-open-b5d354`) because the hash does not determine them, so the row held whichever file's occurrence landed first. Read them per occurrence through `_ast.kind_id → kinds`; see "A fielded child's grammar kind at `projection-v12`" below. Byte-identical subtrees dedup cross-file to one row. |
| `node_edge` | Git-tree object, interned (`projection-v9`, bead `ley-line-open-9b7d28`): `(parent_id, ordinal) PRIMARY KEY → child_id, field`, `WITHOUT ROWID`. Both endpoints REFERENCE `node_content(content_id)` (FK-enforced). One row per child of each unique parent, anonymous tokens included. A node's preimage is recoverable by one join: `ORDER BY ordinal`, map ids to `node_hash`, refold. |
| `node_child` | **A read-only VIEW** (`projection-v9`) with the v8 table's columns — `parent_hash`, `ordinal`, `child_hash`, `field` — over `node_edge ⋈ node_content ×2`. Every v8 read (`WHERE parent_hash = ?`, `JOIN node_content c ON c.node_hash = nc.child_hash`, `ORDER BY ordinal`) runs unchanged, and an inner join or `IN` filter on `parent_hash` plans as two index probes. **Do not put it on the right of a `LEFT JOIN`:** SQLite cannot flatten a join view there and materializes all of it (kibana slice, one file: 7.7 s through the view, 0.11 s against `node_edge`). Write `LEFT JOIN node_edge e ON e.parent_id = a.content_id LEFT JOIN node_content cc ON cc.content_id = e.child_id` instead (`projection-v10`: `_ast` holds the parent's `content_id` itself; at v9 it took `LEFT JOIN node_content pc ON pc.node_hash = a.node_hash` first). A write to the view fails; the writer is `node_edge`. |
| `v_ast_hash` / `v_node_refs_hash` / `v_node_defs_hash` | **Read-only VIEWs** (`projection-v10`, bead `ley-line-open-738dd9`) with the column sets of `_ast`, `node_refs` and `node_defs` — `node_hash` in place of `content_id`, rendered by `LEFT JOIN node_content ON content_id` (the v9 sets, plus `kind_id` on the refs and defs views since `projection-v12`). `content_id` is not a column of any of them. Same planning caveat as `node_child`: on the right of a `LEFT JOIN` they materialize; join the base table instead. Not named `v_ast` because mache installs a TEMP `v_ast` with its own vocabulary on every connection, and a TEMP view shadows a main-schema one. |

**Pointer store — retired at `projection-v8`** (ADR-0040 D5, bead `ley-line-open-f30fdf`; ADR-0026 Phase 1 reversed). `capnp_blobs` (one canonical `AstNodeList` blob per file) and `_ast_blob` (`file_id → blob_hash`) are no longer created or written. They were the second stored copy of the syntax tree — 812.8 MB, 35.6% of the 2,284.6 MB kibana arena (bead `ley-line-open-17c271`'s pinned slice) — with no reader. The tree is stored once in the SQL rows above and `node_content`/`node_child`, keyed by the location-free `node_hash`. No identity moved: `Head.rootHash` folds the `.source.capnp`/`.ast.capnp`/`.bindings.capnp` segment files, whose bytes never depended on the blob, and `treeRoot` (retired by ADR-0047 M1, now `storeRoot`) folds `_tree`. A `projection-v7` arena is refused at parse open and rebuilt cold.

**Source blobs** (added v0.6.0 per ADR-0028 Phase 1 dual-store):

| Table | Purpose |
|-------|---------|
| `source_blobs` | Content-addressed byte store — `blob_hash BLOB PRIMARY KEY`, `blob_bytes BLOB`, `byte_len INTEGER` (stored generated column). F-git compat with `git cat-file blob`. |

**The store's manifest and the parse's diagnostics** (added at `projection-v7` per ADR-0045 T1, T3; DDL in `rs/ll-open/cli-lib/src/tree.rs`). A `projection-v6` arena is refused at parse open and rebuilt cold.

| Table | Purpose |
|-------|---------|
| `_tree` | One row per entry the store holds — `path TEXT PRIMARY KEY` (relative, `/`-separated, no `''`/`.`/`..`), `mode INTEGER NOT NULL` (git mode: `0o100644` regular, `0o100755` executable, `0o120000` symlink whose blob is its target, `0o040000` directory), `blob_hash BLOB` (the `source_blobs` key; NULL for a directory), `mtime INTEGER` and `size INTEGER` (the walk's stat cache; not identity). Every file of any kind is an entry, projected or not; directories are explicit, so an empty directory is an entry, and the set is closed under parents. `storeRoot` (`leyline/store-tree/v1`, ADR-0047 D-ii) folds it as a directory Merkle tree: one node per directory over its children's `(name, git mode, blob hash or child node)`. |
| `_parse_diagnostics` | One row per file the parse projects that did not parse cleanly — `path TEXT PRIMARY KEY` (the `_tree` path), `blob_hash BLOB NOT NULL` (the bytes described), `projected INTEGER NOT NULL` (`1`: projected as tree-sitter parsed it, error nodes included; `0`: stored with no projection rows — a NUL byte in the first 8 KiB or larger than `MAX_PARSE_FILE_SIZE`), `start_byte`, `end_byte`, `start_row` (0-based), `start_col` (the first `ERROR`/`MISSING` node; NULL when not projected), `message TEXT NOT NULL`. A function of the bytes and the grammar: a proposal and a cold parse of one tree write the same rows, and a file whose next version parses cleanly loses its row in the same transaction. |

**CDC derived chunk cache** (per `rs/ll-open/fs/src/chunk_pool.rs` and `rs/ll-open/fs/src/blob_chunked.rs`, activated by `daemon --cdc` / `leyline cdc`):

These tables are **private and derived**. They are not part of the SQL projection
ABI, no consumer reads them (mache reads none of them), and changes to them do
not bump `leyline-schema`. `source_blobs` stays authoritative; the manifests index
its whole-file rows at or above the 8 KiB chunking floor.

| Table | Purpose |
|-------|---------|
| `content_chunks` | Content-defined chunks keyed by blob hash, 8–128 KiB (`rs/ll-open/cdc/src/lib.rs:29-40`). Reads verify σ before returning and fail closed on mismatch (`chunk_pool.rs`). |
| `blob_manifest` | Per-blob chunk sequence, keyed `(blob_hash, seq)`. No freshness witness: `source_blobs` rows are immutable, so a manifest's existence is its freshness. **Has no root** — it is a lookup index, not an identity domain. ADR-0032 §D3's `manifestRoot` is proposed, not implemented. |

The per-node `nodes` target and its `content_manifest`, `content_manifest_meta`
and `content_generation` tables were removed in `ley-line-open-afa952`. Nothing
reads those tables in a database that still carries them, and `leyline cdc gc`
collects the chunks only they referenced.

**Daemon checkout** (bead `ley-line-open-60f39f`, `rs/ll-open/cli-lib/src/checkout.rs`):

These tables are **private to the daemon** that owns a `--source`
directory. They are not part of the SQL projection ABI, no consumer reads
them, `leyline parse` never creates them, and changes to them do not bump
`leyline-schema`. They live in `live.db`, so a published arena image
carries them.

| Table | Purpose |
|-------|---------|
| `_checkout_index` | `path TEXT PRIMARY KEY` → `blob_hash BLOB`: the blob last written into the source directory or read from it — git's index. Startup, the watcher, `reparse` and every accepted proposal compare the directory and `_source.content_hash` each against it. |
| `_checkout_conflict` | Paths whose directory bytes and stored bytes both moved, to different bytes, since the index: `disk_hash` (NULL when deleted on disk), `stored_hash` (NULL when a proposal deleted the file), `index_hash`. Neither side is overwritten; the row leaves when the two agree. |

`_tree` paths are byte-distinct, and a checkout keeps them distinct on disk: a proposal that
creates a path the checkout's file system folds onto another entry (case, or Unicode
normalization, as the default APFS volume folds both) is refused `aliases_existing` before it
commits, naming both paths, so `_tree` and the directory never hold two entries for one file
(ADR-0043 R3, path aliasing; bead `ley-line-open-23a26c`). Folding is probed on the checkout's
directory, not assumed from the platform.

**Analysis substrate** (added v0.7.2 per `docs/decades/analysis-substrate.md`) — a *derived analysis facility*, not an identity domain; the word "substrate" here is the decade's name, not a claim about content addressing:

| Table | Purpose |
|-------|---------|
| `_cfg` | Intra-procedural CFG basic blocks — `(node_hash, block_id) PRIMARY KEY`, `source_id TEXT` (indexed by `idx_cfg_source`), `block_kind TEXT` (one of `CFG_CANONICAL_KINDS`), `entry_offset`/`exit_offset` byte offsets, `complexity INTEGER` nullable. Node_hash FK to `node_content`. The key is reflow-invariant, but it is not a `node_hash`: `emit_cfg_for_source` keys a body by a plain-BLAKE3 stub fold and inserts a `node_content` row for it, which does not re-derive under merkle-ast-v3 (`ley-line-open-cc7855`). The parse writes no `_cfg` rows. |
| `_cfg_edge` | Directed edges between CFG blocks. Composite FK on both endpoints referencing `_cfg(node_hash, block_id)`. `edge_kind TEXT` (free-form in v0.7.2; T3 taint will canonicalize). |

Population TODO (bead `ley-line-open-a0fadd`): T1.b3-followup wires `cfg::emit_cfg_for_source` into `cmd_parse`'s rayon-worker + batched-insert plumbing so `_cfg` populates on parse. Until then, schema is present but empty on parse output; `_cfg` is only populated by consumers calling `leyline_ts::cfg::emit_cfg_for_source` directly.

### LSP enrichment (extension layer)

Produced by `LspEnrichmentPass` (registered via `DaemonExt::enrichment_passes`).
Depends on tree-sitter. Tables are optional — queries degrade gracefully.

| Table | Purpose |
|-------|---------|
| `_lsp` | Core symbol metadata (nid, symbol_kind, detail, start/end line and column, diagnostics) |
| `_lsp_defs` | Go-to-definition results (nid, def_token → def_uri, line/col) |
| `_lsp_refs` | **Legacy / read-only.** Find-references results (nid, referrer_nid, ref_token → ref_uri, line/col). New writes retired at T8.9 (`9d3a3b4`); contract migrated to `BindingRecord` capnp event log at `${db}.bindings.capnp`. DDL retained for legacy `.db` read compatibility. |
| `_lsp_hover` | Hover documentation (nid → hover_text) |
| `_lsp_completions` | Completion items (nid → label, kind, detail, documentation, sort_text) |

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
| `_cfg*` | analysis-substrate CFG layer (leyline-ts; T1 of `analysis-substrate` decade) |
| `_dfg*` | analysis-substrate DFG layer (T2 of `analysis-substrate` decade; not yet shipped) |
| `_taint*` | analysis-substrate taint fixpoint (T3 of `analysis-substrate` decade; not yet shipped) |
| `node_content` / `node_edge` / `node_child` (view) | ADR-0027 merkle-AST IR (base; no prefix) |
| `v_ast_hash` / `v_node_refs_hash` / `v_node_defs_hash` (views) | `projection-v10` `node_hash`-rendering compatibility views (base) |
| `names` / `dirs` / `files` / `kinds` | projection-v5 interning tables (base; no prefix) |
| `source_blobs` | ADR-0028 content-addressed source (base; no prefix) |
| `query_blobs` / `_queries` | arena-resident override tree-sitter queries: `blob_hash` (BLAKE3) → `blob_bytes`, and `(lang, kind)` → `blob_hash` (base; leyline-ts) |
| `content_chunks` / `blob_manifest` | CDC derived chunk cache — private, not part of the SQL projection ABI |
| `_checkout*` | daemon checkout of its `--source` directory — private, not part of the SQL projection ABI |

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

One op, `query`, is SKIPPED in the Go half, with the drift reason as the
skip message: the schema declares positional `rows: List(QueryRow)` and the
handler emits column-keyed maps, which no additive schema change reconciles.
The Rust half still runs it. Every other fixture's `go_drift_skip` is null;
bead A-2 (`b631c8`) reconciled the others additively.

## Rules

1. **Disjoint writes**: `A.writes() ∩ B.writes() = ∅` for any two passes A, B.
2. **Atomic layer writes**: All writes from a single pass run in one SQLite transaction.
3. **Causal basis** (not implemented): no layer records a parse basis in
   `_meta` today. A projection's derivation is `_meta.derived_from`; an
   enrichment layer's staleness is not tracked.
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
   `blob_manifest`) are a derived cache. They may change shape without a
   `leyline-schema` bump, no consumer may depend on them, and `source_blobs`
   remains authoritative for their contents.
