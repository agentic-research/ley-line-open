//! Schema definitions for the AST projection tables.
//!
//! Re-exports the shared `nodes` table from `leyline-schema` and adds
//! AST-specific tables (`_source`, `_ast`) that enable bidirectional splicing.

pub use leyline_schema::{
    NODES_INDEXES_DDL, NODES_TABLE_DDL, create_nodes_indexes, create_nodes_table, create_schema,
    dir_nid, ensure_dir_nodes, ensure_file_id, file_nid, file_nid_range, insert_node, intern_kind,
    intern_name, lookup_file_id, nid_file_id, nid_ordinal, node_path, resolve_path,
};

use anyhow::Result;
use rusqlite::{Connection, params};

/// DDL for the `_source` table — tracks source files for splice and content resolution.
///
/// Two modes:
/// - **Inline** (single-file API): `content` is populated, `path` is NULL.
/// - **Reference** (multi-file CLI): `path` is populated, `content` is NULL.
///   Consumers read source from disk via `path` when `content` is NULL.
pub const SOURCE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _source (
    id TEXT PRIMARY KEY,
    language TEXT NOT NULL,
    content BLOB,
    path TEXT,
    content_hash BLOB,
    -- projection-v5 (bead `ley-line-open-17c271`): the file's interned id in
    -- `files`. `nid >> 24` of any node row lands here directly, so joining a
    -- node to its source content is one integer equality instead of a path
    -- render. UNIQUE: one _source row per interned file.
    file_id INTEGER UNIQUE
);";

/// DDL for the `_ast` table — table only, no indexes. Pairs with
/// [`AST_INDEXES_DDL`] for bulk-load callers (see bead
/// `ley-line-open-9ccbc7`).
pub const AST_TABLE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _ast (
    nid INTEGER PRIMARY KEY,
    kind_id INTEGER NOT NULL,
    start_byte INTEGER NOT NULL,
    end_byte INTEGER NOT NULL,
    start_row INTEGER NOT NULL,
    start_col INTEGER NOT NULL,
    end_row INTEGER NOT NULL,
    end_col INTEGER NOT NULL,
    node_hash BLOB REFERENCES node_content(node_hash)
);";

/// DDL for the `_ast` indexes — deferred post-COMMIT for bulk-load.
///
/// projection-v5 has NO `_ast` secondary index for file scoping: a file's
/// rows are `nid BETWEEN (file_id<<24) AND (file_id<<24)|0xFFFFFF`, a
/// PRIMARY KEY range SEARCH. The pre-v5 `idx_ast_source` existed to serve
/// `WHERE source_id = ?`, which the nid range replaces outright (and the
/// TEXT PK's `sqlite_autoindex__ast_1` — 830 MB measured — vanishes with
/// the rowid-aliased INTEGER PK). `idx_ast_node_hash` still lands
/// post-COMMIT via [`create_ir_indexes`].
pub const AST_INDEXES_DDL: &str =
    "-- no _ast secondary indexes in projection-v5 (PK range scan scopes by file)";

/// Combined `_ast` table + index DDL. Preserves the pre-split contract.
pub const AST_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _ast (
    nid INTEGER PRIMARY KEY,
    kind_id INTEGER NOT NULL,
    start_byte INTEGER NOT NULL,
    end_byte INTEGER NOT NULL,
    start_row INTEGER NOT NULL,
    start_col INTEGER NOT NULL,
    end_row INTEGER NOT NULL,
    end_col INTEGER NOT NULL,
    node_hash BLOB REFERENCES node_content(node_hash)
);";

/// Create `nodes`, `_source`, and `_ast` tables + indexes (idempotent).
///
/// For bulk-load callers (e.g. `cmd_parse`), prefer the split
/// [`create_ast_tables`] + [`create_ast_indexes`] pair so the indexes
/// can be deferred until after `COMMIT`.
pub fn create_ast_schema(conn: &Connection) -> Result<()> {
    create_schema(conn)?;
    conn.execute_batch(SOURCE_DDL)?;
    // projection-v5: the occurrence tables carry a `node_hash REFERENCES
    // node_content(node_hash)` FK in their base DDL, so the content tables
    // must exist BEFORE any referencing table is written to.
    conn.execute_batch(NODE_CONTENT_TABLE_DDL)?;
    conn.execute_batch(NODE_CHILD_TABLE_DDL)?;
    conn.execute_batch(AST_DDL)?;
    Ok(())
}

/// Create `nodes`, `_source`, `_ast` tables only — no indexes. Pair
/// with [`create_ast_indexes`] post-`COMMIT` for bulk-load paths.
pub fn create_ast_tables(conn: &Connection) -> Result<()> {
    create_nodes_table(conn)?;
    conn.execute_batch(SOURCE_DDL)?;
    // FK-target ordering — see `create_ast_schema`.
    conn.execute_batch(NODE_CONTENT_TABLE_DDL)?;
    conn.execute_batch(NODE_CHILD_TABLE_DDL)?;
    conn.execute_batch(AST_TABLE_DDL)?;
    Ok(())
}

/// Create `nodes` + `_ast` indexes (idempotent). `_source` has no
/// secondary indexes — its PRIMARY KEY suffices.
pub fn create_ast_indexes(conn: &Connection) -> Result<()> {
    create_nodes_indexes(conn)?;
    conn.execute_batch(AST_INDEXES_DDL)?;
    Ok(())
}

/// Insert or replace a source row with inline content (single-file API).
pub fn insert_source(
    conn: &Connection,
    id: &str,
    language: &str,
    content: &[u8],
    file_id: i64,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO _source (id, language, content, file_id) VALUES (?1, ?2, ?3, ?4)",
        params![id, language, content, file_id],
    )?;
    Ok(())
}

/// Insert or replace a source row with a file path reference (multi-file CLI).
/// No content BLOB is stored — consumers read from disk via `path`.
pub fn insert_source_ref(
    conn: &Connection,
    id: &str,
    language: &str,
    path: &str,
    file_id: i64,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO _source (id, language, path, file_id) VALUES (?1, ?2, ?3, ?4)",
        params![id, language, path, file_id],
    )?;
    Ok(())
}

/// Insert an AST byte-range mapping.
#[allow(clippy::too_many_arguments)]
pub fn insert_ast(
    conn: &Connection,
    nid: i64,
    kind_id: i64,
    start_byte: usize,
    end_byte: usize,
    start_row: usize,
    start_col: usize,
    end_row: usize,
    end_col: usize,
    node_hash: Option<&[u8]>,
) -> Result<()> {
    // rusqlite 0.39 dropped the blanket `ToSql for usize` — bind through
    // `i64` instead. Tree-sitter byte/row/col indices fit comfortably in
    // `i64` (well under 2^63 even for pathological source files), so the
    // cast is lossless.
    conn.execute(
        "INSERT OR REPLACE INTO _ast (nid, kind_id, start_byte, end_byte, \
         start_row, start_col, end_row, end_col, node_hash) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            nid,
            kind_id,
            start_byte as i64,
            end_byte as i64,
            start_row as i64,
            start_col as i64,
            end_row as i64,
            end_col as i64,
            node_hash,
        ],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Refs / Defs / Imports tables
// ---------------------------------------------------------------------------

/// DDL for the `node_refs` table — table only, no indexes.
///
/// `container_node_id` is the node_id of the nearest ancestor whose κ
/// canonical kind is `function` or `method` — i.e. "which function/method
/// does this ref live inside?" NULL for top-level refs (file-scope
/// declarations, imports at the top of a Go file, etc.). Bead
/// `ley-line-open-6e798d` — the load-bearing signal mache's
/// `fan_out_skew` + `untested_function` rules `GROUP BY` on to get
/// per-caller aggregation. Additive column: legacy DBs read it as NULL
/// via `create_container_id_columns`'s idempotent ALTER path.
///
/// `qualifier` (bead `ley-line-open-4dde42`, the `b9d1d5` leftover) is
/// the syntactic receiver/selector text of a qualified call site,
/// carried on the BARE-token row of the dual-emit pair (`fmt.Println(..)`
/// → the `Println` row carries `'fmt'`; `std::process::exit()` → the
/// `exit` row carries `'std::process'`). The qualified-token row and
/// genuinely bare calls carry NULL — one row per qualified call site
/// holds the structural (name, qualifier) pair, so consumers never
/// double-count. NULL (not `''`) for "no qualifier": the additive ALTER
/// backfills NULL on legacy rows, and a second `''` encoding would split
/// the no-qualifier shape in two. Additive column: legacy DBs migrate
/// via `create_qualifier_column`'s idempotent ALTER path.
pub const REFS_TABLE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS node_refs (
    token TEXT NOT NULL,
    -- projection-v5 (bead `ley-line-open-17c271`): integer nid of the
    -- referencing site. `nid >> 24` is the file, so per-file scoping is a
    -- range predicate — the pre-v5 `source_id` column is gone. Injected
    -- nodes (bead `ley-line-open-c822a6`) occupy ordinals PAST their host
    -- file's `_ast` count: real nids in the file's range with no `_ast` or
    -- `nodes` row, exactly as their path-shaped ids had no rows before.
    nid INTEGER NOT NULL,
    container_nid INTEGER,
    -- ADR-0026-adjacent denormalisation (bead `ley-line-open-b4509b`): the
    -- occurrence's own source span and grammar kind, so resolving a
    -- definition or a caller does not JOIN `_ast`. SCIP carries the range
    -- inline on the occurrence for the same reason.
    --
    -- NULLABLE, and NULL is meaningful: injected nodes have no `_ast` row,
    -- so they had no span under the old LEFT JOIN either.
    node_kind TEXT,
    start_byte INTEGER,
    end_byte INTEGER,
    start_row INTEGER,
    start_col INTEGER,
    end_row INTEGER,
    end_col INTEGER,
    qualifier TEXT,
    node_hash BLOB REFERENCES node_content(node_hash)
);";

/// DDL for the `node_refs` indexes — deferred post-COMMIT.
///
/// `idx_refs_container` accelerates `GROUP BY container_nid` — mache's
/// fan_out_skew query is a per-container aggregate over v_refs.
/// `idx_refs_node` also serves the per-file delete: `WHERE nid BETWEEN ?1
/// AND ?2` is a range SEARCH on it.
pub const REFS_INDEXES_DDL: &str = "\
CREATE INDEX IF NOT EXISTS idx_refs_token ON node_refs(token);
CREATE INDEX IF NOT EXISTS idx_refs_node ON node_refs(nid);
CREATE INDEX IF NOT EXISTS idx_refs_container ON node_refs(container_nid) WHERE container_nid IS NOT NULL;";

/// Combined `node_refs` table + index DDL.
pub const REFS_DDL: &str = "\
CREATE TABLE IF NOT EXISTS node_refs (
    token TEXT NOT NULL,
    nid INTEGER NOT NULL,
    container_nid INTEGER,
    node_kind TEXT,
    start_byte INTEGER,
    end_byte INTEGER,
    start_row INTEGER,
    start_col INTEGER,
    end_row INTEGER,
    end_col INTEGER,
    qualifier TEXT,
    node_hash BLOB REFERENCES node_content(node_hash)
);
CREATE INDEX IF NOT EXISTS idx_refs_token ON node_refs(token);
CREATE INDEX IF NOT EXISTS idx_refs_node ON node_refs(nid);
CREATE INDEX IF NOT EXISTS idx_refs_container ON node_refs(container_nid) WHERE container_nid IS NOT NULL;";

/// DDL for the `node_defs` table — table only, no indexes.
///
/// See `REFS_TABLE_DDL` for `container_node_id` semantics.
///
/// `canonical_kind` (bead follow-up to `ley-line-open-6e798d`, cross-repo
/// signal from mache 2026-07-13) is the κ canonical kind of the
/// definition — one of `function`, `method`, `type`, `constant`,
/// `variable`, `field`, `module`, `import`, `parameter` per
/// `TsLanguage::canonical_kind`. Nullable so pre-migration DBs read as
/// NULL (open-world escape). Load-bearing for consumers that filter
/// dead-code / god-file rules by symbol-scope κ kind — mache's
/// `dead_code` rule on the LLO projection over-reports 321 vs
/// tree-sitter's 5 because it treats every `node_defs` row as a
/// dead-code candidate; adding `WHERE canonical_kind IN ('function',
/// 'method', 'type')` collapses the count without a JOIN through
/// `node_content` (which requires `node_hash` populated on every row).
pub const DEFS_TABLE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS node_defs (
    token TEXT NOT NULL,
    -- projection-v5: integer nid of the defining site; see `REFS_TABLE_DDL`
    -- for the scheme (file scoping by range, injected-node ordinals).
    nid INTEGER NOT NULL,
    container_nid INTEGER,
    -- ADR-0026-adjacent denormalisation (bead `ley-line-open-b4509b`): the
    -- occurrence's own source span and grammar kind, so resolving a
    -- definition or a caller does not JOIN `_ast`. SCIP carries the range
    -- inline on the occurrence for the same reason.
    --
    -- NULLABLE, and NULL is meaningful: injected nodes have no `_ast` row,
    -- so they had no span under the old LEFT JOIN either.
    node_kind TEXT,
    start_byte INTEGER,
    end_byte INTEGER,
    start_row INTEGER,
    start_col INTEGER,
    end_row INTEGER,
    end_col INTEGER,
    canonical_kind TEXT,
    node_hash BLOB REFERENCES node_content(node_hash)
);";

/// DDL for the `node_defs` indexes — deferred post-COMMIT.
///
/// `idx_defs_canonical_kind` accelerates the mache-shaped `SELECT ...
/// FROM node_defs WHERE canonical_kind IN (...)` filter — the load-
/// bearing dead-code/god-file query on the LLO projection.
pub const DEFS_INDEXES_DDL: &str = "\
CREATE INDEX IF NOT EXISTS idx_defs_token ON node_defs(token);
CREATE INDEX IF NOT EXISTS idx_defs_node ON node_defs(nid);
CREATE INDEX IF NOT EXISTS idx_defs_container ON node_defs(container_nid) WHERE container_nid IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_defs_canonical_kind ON node_defs(canonical_kind) WHERE canonical_kind IS NOT NULL;";

/// Combined `node_defs` table + index DDL.
pub const DEFS_DDL: &str = "\
CREATE TABLE IF NOT EXISTS node_defs (
    token TEXT NOT NULL,
    nid INTEGER NOT NULL,
    container_nid INTEGER,
    node_kind TEXT,
    start_byte INTEGER,
    end_byte INTEGER,
    start_row INTEGER,
    start_col INTEGER,
    end_row INTEGER,
    end_col INTEGER,
    canonical_kind TEXT,
    node_hash BLOB REFERENCES node_content(node_hash)
);
CREATE INDEX IF NOT EXISTS idx_defs_token ON node_defs(token);
CREATE INDEX IF NOT EXISTS idx_defs_node ON node_defs(nid);
CREATE INDEX IF NOT EXISTS idx_defs_container ON node_defs(container_nid) WHERE container_nid IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_defs_canonical_kind ON node_defs(canonical_kind) WHERE canonical_kind IS NOT NULL;";

/// DDL for the `_imports` table — table only, no indexes.
pub const IMPORTS_TABLE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _imports (
    alias TEXT NOT NULL,
    path TEXT NOT NULL,
    source_id TEXT NOT NULL
);";

/// DDL for the `_imports` indexes — deferred post-COMMIT.
pub const IMPORTS_INDEXES_DDL: &str =
    "CREATE INDEX IF NOT EXISTS idx_imports_source ON _imports(source_id);";

/// Combined `_imports` table + index DDL.
pub const IMPORTS_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _imports (
    alias TEXT NOT NULL,
    path TEXT NOT NULL,
    source_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_imports_source ON _imports(source_id);";

// ---------------------------------------------------------------------------
// Merkle-AST content-addressed IR (ADR-0027 / mache ADR-0023)
// ---------------------------------------------------------------------------
//
// Replaces the location-keyed `symbol_id` + eager `symbols`/`fact_edges`
// tables with a bottom-up merkle-AST `node_hash`. Net change is mostly
// deletion + one deduped content table (`node_content`), the git-tree
// object (`node_child`), and a `node_hash` column stamped onto the
// occurrence tables that already exist (`_ast`, `node_defs`, `node_refs`).
//
// `node_hash` is intrinsic (a function of κ kind + terminal token +
// ordered child hashes — spans/paths/parse-run node_ids are OUT), so a
// unique subtree is stored once. Two byte-identical functions in different
// files share a `node_hash`; a `a+b` vs `a-b` edit does not (the fold
// includes anonymous operator tokens). The one-to-many invariant: a
// reference's resolved target is a def OCCURRENCE (node_id), NEVER a
// `node_hash` — keying resolution on `node_hash` would silently collapse
// two distinct callees with identical bodies.

/// DDL for `node_content` — one row per UNIQUE subtree, keyed on the
/// merkle-AST `node_hash` (a real single-column PRIMARY KEY). `INSERT OR
/// IGNORE` on the PK == intrinsic dedup: the second occurrence of an
/// identical subtree is silently ignored. `kind` is the hashed canonical
/// κ kind; `raw_kind` is the grammar kind (a content column, NOT hashed).
/// `token` is the terminal UTF-8 text for leaves (NULL for internal
/// nodes); `arity` is the child count.
pub const NODE_CONTENT_TABLE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS node_content (
    node_hash BLOB PRIMARY KEY,
    node_tag  INTEGER NOT NULL,
    kind      TEXT    NOT NULL,
    raw_kind  TEXT    NOT NULL,
    lang      TEXT    NOT NULL,
    token     TEXT,
    arity     INTEGER NOT NULL
);";

/// DDL for `node_child` — the git-tree object. One row per (unique parent,
/// ordinal) edge, deduped per unique parent subtree. `field` is the
/// tree-sitter field name ("name","body",…) or NULL when the child has no
/// field. Both endpoints `REFERENCES node_content(node_hash)`; the
/// post-order fold emits children before parents so FK enforcement holds
/// under `PRAGMA foreign_keys = ON`.
pub const NODE_CHILD_TABLE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS node_child (
    parent_hash BLOB    NOT NULL REFERENCES node_content(node_hash),
    ordinal     INTEGER NOT NULL,
    child_hash  BLOB    NOT NULL REFERENCES node_content(node_hash),
    field       TEXT,
    PRIMARY KEY (parent_hash, ordinal)
);";

/// Index over `_ast.node_hash` — "every location of this exact subtree".
pub const AST_NODE_HASH_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS idx_ast_node_hash ON _ast(node_hash);";

/// Create the merkle-AST IR tables (`node_content`, `node_child`).
/// Idempotent.
///
/// The occurrence tables' `node_hash` columns carry a
/// `REFERENCES node_content(node_hash)` FK, so with `PRAGMA foreign_keys =
/// ON` at write time a `node_hash` pointer that doesn't resolve to a real
/// content row is a loud insert error.
pub fn create_ir_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(NODE_CONTENT_TABLE_DDL)?;
    conn.execute_batch(NODE_CHILD_TABLE_DDL)?;
    // projection-v5: `node_hash` sits in the base DDL of `_ast`,
    // `node_defs`, and `node_refs` — the pre-v5 additive ALTERs are gone
    // (a pre-v5 arena is refused at open, not migrated).
    Ok(())
}

/// Create the deferred merkle-AST IR index (idempotent). Called
/// post-`COMMIT` alongside the other bulk-load index passes. `node_content`
/// and `node_child` are covered by their PRIMARY KEYs; the only extra
/// traversal index is `idx_ast_node_hash`.
pub fn create_ir_indexes(conn: &Connection) -> Result<()> {
    conn.execute_batch(AST_NODE_HASH_INDEX_DDL)?;
    Ok(())
}

/// Create `node_refs`, `node_defs`, and `_imports` tables + indexes
/// (idempotent).
///
/// For bulk-load callers (e.g. `cmd_parse`), prefer
/// [`create_refs_tables`] + [`create_refs_indexes`] so the indexes can
/// be deferred until after `COMMIT`.
pub fn create_refs_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(REFS_DDL)?;
    conn.execute_batch(DEFS_DDL)?;
    conn.execute_batch(IMPORTS_DDL)?;
    Ok(())
}

/// Create `node_refs`, `node_defs`, `_imports` tables only — no
/// indexes. Pair with [`create_refs_indexes`] post-`COMMIT` for
/// bulk-load paths.
pub fn create_refs_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(REFS_TABLE_DDL)?;
    conn.execute_batch(DEFS_TABLE_DDL)?;
    conn.execute_batch(IMPORTS_TABLE_DDL)?;
    Ok(())
}

/// Create indexes for `node_refs`, `node_defs`, and `_imports`
/// (idempotent).
pub fn create_refs_indexes(conn: &Connection) -> Result<()> {
    conn.execute_batch(REFS_INDEXES_DDL)?;
    conn.execute_batch(DEFS_INDEXES_DDL)?;
    conn.execute_batch(IMPORTS_INDEXES_DDL)?;
    Ok(())
}

/// Insert a reference row.
///
/// `container_node_id` = node_id of the nearest enclosing function/method
/// ancestor (per κ canonical kind); `None` for top-level refs. Bead
/// `ley-line-open-6e798d`.
///
/// `qualifier` = receiver/selector text on the BARE-token row of a
/// qualified call's dual-emit pair; `None` on the qualified-token row
/// and on genuinely bare calls. See `REFS_TABLE_DDL`. Bead
/// `ley-line-open-4dde42`.
pub fn insert_ref(
    conn: &Connection,
    token: &str,
    nid: i64,
    container_nid: Option<i64>,
    qualifier: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO node_refs (token, nid, container_nid, qualifier) VALUES (?1, ?2, ?3, ?4)",
        params![token, nid, container_nid, qualifier],
    )?;
    Ok(())
}

/// Insert a definition row.
///
/// `container_node_id` = node_id of the nearest enclosing function/method
/// ancestor (per κ canonical kind); `None` for top-level defs. Bead
/// `ley-line-open-6e798d`.
///
/// `canonical_kind` = κ canonical kind of the def itself
/// (`function`/`method`/`type`/`constant`/`variable`/`field`/etc.).
/// `None` when the extractor emitted a raw kind that has no κ mapping.
/// Enables consumers (mache's `dead_code` / `god_file` rules) to
/// filter by symbol-scope κ kind without a JOIN through
/// `node_content.kind`.
pub fn insert_def(
    conn: &Connection,
    token: &str,
    nid: i64,
    container_nid: Option<i64>,
    canonical_kind: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO node_defs (token, nid, container_nid, canonical_kind) VALUES (?1, ?2, ?3, ?4)",
        params![token, nid, container_nid, canonical_kind],
    )?;
    Ok(())
}

// The pre-v5 additive-ALTER migrations (`create_canonical_kind_column`,
// `create_container_id_columns`, `create_qualifier_column`,
// `create_occurrence_span_columns`) are gone: projection-v5 re-keys the
// occurrence tables outright, every column they stamped is in the base DDL,
// and a pre-v5 arena is refused at open (`_meta.projection_schema_version`
// mismatch) rather than migrated — the projection is derived-only, so a
// stale arena is rebuilt by a cold reparse, never patched in place.

/// Insert an import row.
pub fn insert_import(conn: &Connection, alias: &str, path: &str, source_id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO _imports (alias, path, source_id) VALUES (?1, ?2, ?3)",
        params![alias, path, source_id],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// ADR-0026 pointer store — Phase 1 dual-write (bead ley-line-open-3e87ad)
// ---------------------------------------------------------------------------
//
// Content-addressed pointer store: SQL projection becomes a lightweight index
// (`_ast_pointer`) into content-addressed capnp blobs (`capnp_blobs`) held in
// Σ. The row-projected `_ast` schema stays populated in Phase 1 for
// backward-compat + F1 round-trip integrity; Phase 2 migrates consumer reads.
//
// Blob unit: **per-file** (ADR-0026 §2.2 fallback — safer default; per-
// semantic-unit refinement is Phase 2).

/// DDL for `capnp_blobs` — content-addressed blob store. One row per unique
/// per-file blob keyed on BLAKE3(canonical(AstNodeList)).
pub const CAPNP_BLOBS_DDL: &str = "\
CREATE TABLE IF NOT EXISTS capnp_blobs (
    blob_hash BLOB PRIMARY KEY,
    blob_bytes BLOB NOT NULL
);";

/// DDL for `_ast_blob` — the file-to-blob map for the ADR-0026 pointer
/// store. ONE row per source file, not per AST node.
///
/// This replaces `_ast_pointer`, which carried one row per AstNode and did
/// not, in practice, store a pointer. Measured on an 8000-file TypeScript
/// arena, each of its 3 150 849 rows held:
///
///   node_id         232 bytes   already the PK of `_ast`
///   source_id        27.6 bytes already a column of `_ast`
///   blob_hash        32 bytes   only 8000 DISTINCT values across all rows
///   offset_in_blob   int        a dense 0..n-1 per file, verified with no
///                               gaps or duplicates in 8000/8000 files
///   kind             int        `semantic_kind_tag(node_kind)`, a pure
///                               function of a column `_ast` already has
///
/// ~294 bytes per row to address a record of ~370 — the pointer was 80% the
/// size of its referent — costing 927 MB plus an 830 MB shadow index for a
/// mapping that is `blob(file)` plus an ordinal. Both survive here: the file
/// keys this table (by its interned `file_id` as of projection-v5), and the
/// ordinal is the nid's low 24 bits — a node's index into its file's
/// `AstNodeList.nodes` IS `nid & 0xFFFFFF`, stored nowhere.
///
/// The ADR-0026 §6.F1 resolution capability is unchanged: any `_ast` row
/// still resolves to its capnp record, via `nid >> 24 → blob_hash` here and
/// `nid & 0xFFFFFF` as the index into that blob's `AstNodeList.nodes`.
///
/// Bead `ley-line-open-17c271`.
pub const AST_BLOB_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _ast_blob (
    file_id INTEGER PRIMARY KEY,
    blob_hash BLOB NOT NULL
);";

/// Create the pointer-store tables (idempotent). Must run alongside the
/// existing row-projected schema; Phase 1 is dual-write.
///
/// projection-v5 folds the pre-v5 `_ast.blob_ord` column into the key
/// itself: a node's index into its file's `AstNodeList.nodes` IS its
/// pre-order ordinal, i.e. `nid & 0xFFFFFF`. One dense `0..n-1` per file,
/// defined once, stored nowhere.
pub fn create_pointer_store_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(CAPNP_BLOBS_DDL)?;
    conn.execute_batch(AST_BLOB_DDL)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// ADR-0028 source_blobs — Phase 1 dual-store (bead ley-line-open-9e4416)
// ---------------------------------------------------------------------------
//
// Content-addressed source storage: `_source` gains a byte-identical companion
// (`source_blobs`) keyed on BLAKE3(bytes). `_source.content_hash` (populated
// already for the Σ head chain) becomes the FK-shaped pointer into
// `source_blobs`. Phase 1 is dual-store — `_source` still populated as before,
// `source_blobs` populated additively; consumer migration is Phase 2, drop of
// `_source.source` is Phase 3.
//
// Blob unit: per-file (ADR-0028 §2.2). Sub-file dedup via CDC (ley-line
// ADR-014) is a downstream refinement.

/// DDL for `source_blobs` — content-addressed source byte store. One row per
/// UNIQUE byte content keyed on BLAKE3(blob_bytes). `byte_len` is a stored
/// generated column so consumers can filter by size without materializing the
/// blob (index scan + covering `byte_len` predicate). Populated by
/// `INSERT OR IGNORE`, so byte-identical source content across files/repos
/// deduplicates at insert time.
pub const SOURCE_BLOBS_DDL: &str = "\
CREATE TABLE IF NOT EXISTS source_blobs (
    blob_hash BLOB PRIMARY KEY,
    blob_bytes BLOB NOT NULL,
    byte_len INTEGER GENERATED ALWAYS AS (length(blob_bytes)) STORED
);";

/// Create the ADR-0028 source-blobs table (idempotent). Runs alongside the
/// existing `_source` schema; Phase 1 is dual-store.
pub fn create_source_blobs_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(SOURCE_BLOBS_DDL)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Arena-resident query blobs — override .scm store (bead ley-line-open-e72629)
// ---------------------------------------------------------------------------
//
// An arena may carry OVERRIDE tree-sitter `.scm` query blobs that replace the
// compiled-in `queries/<lang>/tags.scm` defaults for a language, behind a
// BLAKE3-hash allowlist (operator-controlled env `LLO_TRUSTED_QUERY_HASHES`).
// Storage mirrors source_blobs/capnp_blobs: a content-addressed blob table
// plus an FK-shaped pointer. Everything durable lives inside the one .db, so
// an arena moves/backs-up as a single file — a sidecar would silently detach.
//
// `query_blobs` keys on BLAKE3(blob_bytes); the resolver verifies bytes hash
// to their key before trusting a row. `_queries` points a (lang, kind) at a
// blob. `kind` is 'tags' today (the extraction query); the column leaves room
// for 'injections' without a schema change.

/// DDL for `query_blobs` — content-addressed override-query store. One row per
/// UNIQUE `.scm` blob keyed on BLAKE3(blob_bytes).
pub const QUERY_BLOBS_DDL: &str = "\
CREATE TABLE IF NOT EXISTS query_blobs (
    blob_hash BLOB PRIMARY KEY,
    blob_bytes BLOB NOT NULL
);";

/// DDL for `_queries` — pointer from (lang, kind) to an override blob. The
/// FK-shaped `blob_hash` references `query_blobs`, same discipline as
/// `_source.content_hash → source_blobs.blob_hash`.
pub const QUERIES_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _queries (
    lang TEXT NOT NULL,
    kind TEXT NOT NULL DEFAULT 'tags',
    blob_hash BLOB NOT NULL REFERENCES query_blobs(blob_hash),
    PRIMARY KEY (lang, kind)
);";

/// Create the override-query tables (idempotent). Runs alongside the existing
/// schema; an arena with no override rows behaves exactly as the compiled-in
/// defaults.
pub fn create_query_blob_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(QUERY_BLOBS_DDL)?;
    conn.execute_batch(QUERIES_DDL)?;
    Ok(())
}

/// One arena-resident `tags` override: the pointer row joined to its blob
/// bytes. `blob_hash` is the pointer's stored key; the resolver re-hashes
/// `blob_bytes` and rejects any row where the two disagree.
pub struct QueryOverrideRow {
    pub lang: String,
    pub blob_hash: Vec<u8>,
    pub blob_bytes: Vec<u8>,
}

/// Read every `kind = 'tags'` override from the arena, joining the pointer to
/// its blob bytes. A legacy arena with no `_queries`/`query_blobs` tables
/// (the JOIN's prepare fails) yields an empty vec — absence of the tables is
/// "no overrides", never an error.
pub fn read_query_overrides(conn: &Connection) -> Result<Vec<QueryOverrideRow>> {
    let mut stmt = match conn.prepare(
        "SELECT q.lang, q.blob_hash, b.blob_bytes \
         FROM _queries q JOIN query_blobs b ON b.blob_hash = q.blob_hash \
         WHERE q.kind = 'tags' ORDER BY q.lang",
    ) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()),
    };
    let rows = stmt
        .query_map([], |r| {
            Ok(QueryOverrideRow {
                lang: r.get(0)?,
                blob_hash: r.get(1)?,
                blob_bytes: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Analysis-substrate: _cfg + _cfg_edge (decade `dataflow-substrate` T1.b2)
// bead `ley-line-open-46d46b`
// ---------------------------------------------------------------------------
//
// Intra-procedural control-flow-graph tables the CFG builder (T1.b3) emits.
// Additive to the existing schema — no consumer read yet in this bead; that
// lands with the builder in T1.b3.
//
// Keying discipline: `_cfg.node_hash` REFERENCES `node_content(node_hash)`
// (ADR-0027 merkle-AST IR), so a CFG row without a corresponding subtree in
// the content-addressed store is a loud FK error under
// `PRAGMA foreign_keys = ON`. `_cfg_edge` carries a composite FK to
// `_cfg(node_hash, block_id)` for the same reason: an edge to a
// non-existent block is caught at insert time, not at query.
//
// `block_kind` is a κ-canonical CFG kind — one of the 10 entries in
// `crate::languages::CFG_CANONICAL_KINDS` (T1.b1, bead `46aef2`). The DDL
// doesn't enforce membership via CHECK (SQLite CHECK constraints would need
// listing all 10 literals inline, which drifts from the Rust-side const);
// the builder (T1.b3) is the invariant-holder here, and a pin test in T1.b3
// asserts every emitted `block_kind` lives in the const array.
//
// `complexity` is stamped by T1.b4 (McCabe cyclomatic complexity as a
// materialized `_cfg.complexity` column). Nullable so T1.b3 can land the
// builder before T1.b4 wires the computation.

/// DDL for `_cfg` — table only, no indexes. One row per basic block in the
/// intra-procedural CFG of a function-body subtree, keyed on
/// `(node_hash, block_id)`. `node_hash` is the function-body subtree's
/// merkle address (ADR-0027); `block_id` is a walk-local index. Two
/// byte-identical function bodies share ALL their `_cfg` rows — dedupes
/// cross-file for the same reason `node_content` does. `source_id` is
/// denormalized alongside for cheap "CFG blocks in this file" queries
/// (see `idx_cfg_source`).
pub const CFG_TABLE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _cfg (
    node_hash BLOB NOT NULL REFERENCES node_content(node_hash),
    source_id TEXT NOT NULL,
    block_id INTEGER NOT NULL,
    block_kind TEXT NOT NULL,
    entry_offset INTEGER NOT NULL,
    exit_offset INTEGER NOT NULL,
    complexity INTEGER,
    PRIMARY KEY (node_hash, block_id)
);";

/// DDL for `_cfg_edge` — table only, no indexes. One row per directed edge
/// between two basic blocks. FK is composite (endpoints of the edge each
/// point at a `_cfg(node_hash, block_id)` row). `edge_kind` is a free-form
/// tag the builder stamps (e.g. `fallthrough`, `taken`, `not_taken`,
/// `back`, `throw`) — not κ-canonical in this bead; the builder decides
/// the closed set once it lands (T1.b3).
pub const CFG_EDGE_TABLE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _cfg_edge (
    from_node_hash BLOB NOT NULL,
    from_block_id INTEGER NOT NULL,
    to_node_hash BLOB NOT NULL,
    to_block_id INTEGER NOT NULL,
    edge_kind TEXT NOT NULL,
    FOREIGN KEY (from_node_hash, from_block_id) REFERENCES _cfg(node_hash, block_id),
    FOREIGN KEY (to_node_hash, to_block_id) REFERENCES _cfg(node_hash, block_id)
);";

/// DDL for the `_cfg` + `_cfg_edge` indexes — deferred post-COMMIT for
/// bulk-load, matching the existing schema pattern. Successor lookup
/// (`(from_node_hash, from_block_id)`) is the load-bearing traversal for
/// T3 taint fixpoint (`iterate` over successors); predecessor lookup
/// (`(to_node_hash, to_block_id)`) is needed for T2 dominance/phi
/// placement. `_cfg.source_id` for "give me all CFG blocks in this file"
/// smell-rule queries.
pub const CFG_INDEXES_DDL: &str = "\
CREATE INDEX IF NOT EXISTS idx_cfg_source ON _cfg(source_id);
CREATE INDEX IF NOT EXISTS idx_cfg_edge_from ON _cfg_edge(from_node_hash, from_block_id);
CREATE INDEX IF NOT EXISTS idx_cfg_edge_to ON _cfg_edge(to_node_hash, to_block_id);";

/// Combined `_cfg` + `_cfg_edge` table + index DDL. Preserves the
/// pre-split contract offered by the sibling `AST_DDL`, `REFS_DDL`, etc.
pub const CFG_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _cfg (
    node_hash BLOB NOT NULL REFERENCES node_content(node_hash),
    source_id TEXT NOT NULL,
    block_id INTEGER NOT NULL,
    block_kind TEXT NOT NULL,
    entry_offset INTEGER NOT NULL,
    exit_offset INTEGER NOT NULL,
    complexity INTEGER,
    PRIMARY KEY (node_hash, block_id)
);
CREATE TABLE IF NOT EXISTS _cfg_edge (
    from_node_hash BLOB NOT NULL,
    from_block_id INTEGER NOT NULL,
    to_node_hash BLOB NOT NULL,
    to_block_id INTEGER NOT NULL,
    edge_kind TEXT NOT NULL,
    FOREIGN KEY (from_node_hash, from_block_id) REFERENCES _cfg(node_hash, block_id),
    FOREIGN KEY (to_node_hash, to_block_id) REFERENCES _cfg(node_hash, block_id)
);
CREATE INDEX IF NOT EXISTS idx_cfg_source ON _cfg(source_id);
CREATE INDEX IF NOT EXISTS idx_cfg_edge_from ON _cfg_edge(from_node_hash, from_block_id);
CREATE INDEX IF NOT EXISTS idx_cfg_edge_to ON _cfg_edge(to_node_hash, to_block_id);";

/// Create the `_cfg` + `_cfg_edge` tables (idempotent), no indexes.
/// Pair with [`create_cfg_indexes`] post-`COMMIT` on bulk-load paths.
///
/// Depends on `node_content` (ADR-0027 merkle-AST IR) existing on the
/// same connection — the FK `_cfg.node_hash REFERENCES node_content` errors
/// at CREATE TABLE time if the target is missing. Call after
/// [`create_ir_tables`].
pub fn create_cfg_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(CFG_TABLE_DDL)?;
    conn.execute_batch(CFG_EDGE_TABLE_DDL)?;
    Ok(())
}

/// Create `_cfg` + `_cfg_edge` indexes (idempotent). Deferred
/// post-COMMIT for bulk-load per the existing pattern
/// ([`create_ast_indexes`], [`create_refs_indexes`]).
pub fn create_cfg_indexes(conn: &Connection) -> Result<()> {
    conn.execute_batch(CFG_INDEXES_DDL)?;
    Ok(())
}

/// Create `_cfg`, `_cfg_edge`, and their indexes (idempotent). For
/// callers that don't need the deferred-index split.
pub fn create_cfg_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(CFG_DDL)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// File-index & meta tables (incremental reparse)
// ---------------------------------------------------------------------------

/// DDL for the `_file_index` table — tracks file mtime/size for incremental reparse.
pub const FILE_INDEX_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _file_index (
    path TEXT PRIMARY KEY,
    mtime INTEGER NOT NULL,
    size INTEGER NOT NULL
);";

/// DDL for the `_meta` table — key/value store for parse metadata.
pub const META_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);";

/// Create `_file_index` and `_meta` tables (idempotent). Neither table
/// has secondary indexes — PRIMARY KEY suffices for both.
pub fn create_index_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(FILE_INDEX_DDL)?;
    conn.execute_batch(META_DDL)?;
    Ok(())
}

/// Create every secondary index across `nodes`, `_ast`, `node_refs`,
/// `node_defs`, and `_imports`. Idempotent (`IF NOT EXISTS`), so it's
/// safe to call on an already-indexed connection (used by `cmd_parse`
/// after `COMMIT` to defer index maintenance out of the bulk-insert
/// hot path — see bead `ley-line-open-9ccbc7`).
pub fn create_post_load_indexes(conn: &Connection) -> Result<()> {
    create_ast_indexes(conn)?;
    create_refs_indexes(conn)?;
    Ok(())
}

/// Variant of [`create_post_load_indexes`] that omits `idx_source_file`.
/// Ley-line's `cmd_parse` never populates the `nodes.source_file`
/// column (that's mache's lazy-resolution flow), so the partial index
/// `WHERE source_file IS NOT NULL` materializes to zero rows yet still
/// pays a 535K-row scan on the mache 765-file bench (~45 ms) to
/// evaluate the predicate against every row. Skipping here is safe
/// because:
///   - mache builds its own schema with the indexes mache needs
///     (via mache's own DDL, not via `create_post_load_indexes_*`).
///   - Any ley-line code path that needs `idx_source_file` will
///     trigger its creation via `create_nodes_indexes` (still
///     idempotent), so semantics are preserved.
///
/// See bead `ley-line-open-cbbedf` Attack 3.
pub fn create_post_load_indexes_skip_unused(conn: &Connection) -> Result<()> {
    // Just `idx_parent_kind_ord` from the nodes-indexes pair — the second
    // (`idx_source_file`) is the unused one we're skipping. The display
    // views land here too (post-COMMIT, zero insert-phase cost).
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_parent_kind_ord ON nodes(parent_nid, kind_id, ord);",
    )?;
    conn.execute_batch(leyline_schema::V_NODE_PATH_DDL)?;
    conn.execute_batch(AST_INDEXES_DDL)?;
    create_refs_indexes(conn)?;
    Ok(())
}

/// Insert or replace a file-index row.
pub fn upsert_file_index(conn: &Connection, path: &str, mtime: i64, size: i64) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO _file_index (path, mtime, size) VALUES (?1, ?2, ?3)",
        params![path, mtime, size],
    )?;
    Ok(())
}

/// Read the full file index into a HashMap.
pub fn read_file_index(conn: &Connection) -> Result<std::collections::HashMap<String, (i64, i64)>> {
    let mut stmt = conn.prepare("SELECT path, mtime, size FROM _file_index")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            (row.get::<_, i64>(1)?, row.get::<_, i64>(2)?),
        ))
    })?;
    let mut map = std::collections::HashMap::new();
    for row in rows {
        let (path, (mtime, size)) = row?;
        map.insert(path, (mtime, size));
    }
    Ok(map)
}

/// Insert or replace a meta key/value pair.
pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO _meta (key, value) VALUES (?1, ?2)",
        params![key, value],
    )?;
    Ok(())
}

/// Read a meta key. Returns `Ok(None)` when the row is absent. SQL errors
/// (broken connection, missing _meta table, etc.) propagate as `Err`.
///
/// Counterpart to `set_meta`. Centralizes the `SELECT value FROM _meta`
/// query so callers can't independently drift on column name or NULL
/// handling.
pub fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    match conn.query_row("SELECT value FROM _meta WHERE key = ?1", [key], |row| {
        row.get::<_, String>(0)
    }) {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Delete all rows for a source file across ALL tables.
///
/// The `nodes` table uses path-prefix deletion because node IDs are structured
/// as `<file>/<ast_path>` (e.g. `main.go/function_declaration_0/identifier`).
///
/// Optional `_lsp*` tables are handled defensively: if LSP enrichment has
/// run on this database the tables exist and rows keyed by node_id need
/// to follow the file deletion (otherwise stale `_lsp*` rows orphan and
/// accumulate at registry-repo scale across file churn). If LSP has
/// never run, the tables don't exist and we skip.
pub fn delete_file_rows(conn: &Connection, path: &str) -> Result<()> {
    // projection-v5: node-level rows are scoped by the file's nid range — a
    // PRIMARY KEY (or `nid`-index) range SEARCH, replacing the pre-v5
    // prefix-LIKE that planned as a SCAN and could over-match on an
    // unanchored prefix. A path the arena never interned owns no node rows,
    // so only the file-level TEXT-keyed tables need touching in that case.
    if let Some(file_id) = leyline_schema::lookup_file_id(conn, path)? {
        let (lo, hi) = leyline_schema::file_nid_range(file_id);
        conn.execute(
            "DELETE FROM nodes WHERE nid BETWEEN ?1 AND ?2",
            params![lo, hi],
        )?;
        conn.execute(
            "DELETE FROM _ast WHERE nid BETWEEN ?1 AND ?2",
            params![lo, hi],
        )?;
        conn.execute(
            "DELETE FROM node_refs WHERE nid BETWEEN ?1 AND ?2",
            params![lo, hi],
        )?;
        conn.execute(
            "DELETE FROM node_defs WHERE nid BETWEEN ?1 AND ?2",
            params![lo, hi],
        )?;
        // ADR-0026 pointer store (Phase 1 dual-write, bead
        // `ley-line-open-3e87ad`). Skip cleanly when the tables don't exist —
        // the pointer store is additive.
        if pointer_store_present(conn) {
            conn.execute("DELETE FROM _ast_blob WHERE file_id = ?1", [file_id])?;
            // capnp_blobs is keyed on blob_hash (content-addressed). Orphaned
            // blobs are ignored here — a Phase 2/3 GC sweep collects blobs no
            // `_ast_blob` row references; reparse recreates via INSERT OR
            // IGNORE, so nothing accumulates per file.
        }
        delete_lsp_rows_for_file(conn, lo, hi)?;
        // The `files` interning row is deliberately NOT deleted: file_id
        // assignment is append-only, so a re-created path re-binds to its
        // old id and a dead id is never reused by an unrelated file.
    }
    conn.execute("DELETE FROM _source WHERE id = ?1", [path])?;
    conn.execute("DELETE FROM _imports WHERE source_id = ?1", [path])?;
    conn.execute("DELETE FROM _file_index WHERE path = ?1", [path])?;
    // ADR-0028 source_blobs (Phase 1 dual-store, bead `ley-line-open-9e4416`).
    // Content-addressed — same orphan discipline as capnp_blobs.
    Ok(())
}

/// True when the pointer-store tables (`_ast_blob`) exist on this
/// connection. Additive-schema guard for `delete_file_rows`: older
/// databases predate the pointer store, and legacy paths that call
/// `delete_file_rows` without first running `create_pointer_store_tables`
/// must not error on the missing table.
fn pointer_store_present(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='_ast_blob'",
        [],
        |r| r.get::<_, bool>(0),
    )
    .unwrap_or(false)
}

/// Delete `_lsp*` rows in the deleted file's nid range. Tables created by
/// leyline-lsp's `create_lsp_schema` are optional; we discover their
/// presence via `sqlite_master` and skip missing ones so callers that never
/// enabled LSP enrichment pay nothing.
///
/// projection-v5: the `_lsp*` tables carry NO file column — file scoping
/// was prefix-LIKE on the path-shaped `node_id` and is now a range
/// predicate on the integer `nid`, with no new column (bead
/// `ley-line-open-17c271`).
///
/// Without this cleanup, `_lsp*` rows accumulate at registry scale as
/// files churn — every file deleted+reparsed leaves the prior LSP
/// enrichment as orphans keyed by nids that no longer resolve.
fn delete_lsp_rows_for_file(conn: &Connection, lo: i64, hi: i64) -> Result<()> {
    // Feature-gated tables — skip cleanly when absent.
    const LSP_TABLES: &[&str] = &[
        "_lsp",
        "_lsp_defs",
        "_lsp_refs",
        "_lsp_hover",
        "_lsp_completions",
    ];
    for table in LSP_TABLES {
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if !exists {
            continue;
        }
        let sql = format!("DELETE FROM {table} WHERE nid BETWEEN ?1 AND ?2");
        conn.execute(&sql, params![lo, hi])?;
    }
    Ok(())
}

/// Remove directory nodes (negative nids) that have no remaining children,
/// iterating until no more orphans remain. Returns the total number of rows
/// removed. The `dirs` interning rows stay — like `files`, the dir-id
/// assignment is append-only; this sweeps only the presentation rows in
/// `nodes`.
pub fn sweep_orphaned_dirs(conn: &Connection) -> Result<usize> {
    let mut total = 0;
    loop {
        let removed = conn.execute(
            "DELETE FROM nodes WHERE nid < -1 \
             AND nid NOT IN (SELECT DISTINCT parent_nid FROM nodes WHERE parent_nid IS NOT NULL)",
            [],
        )?;
        if removed == 0 {
            break;
        }
        total += removed;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_schema_creates_tables() {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();

        let file_id = ensure_file_id(&conn, "main.go").unwrap();
        insert_ref(&conn, "Println", file_nid(file_id, 3), None, None).unwrap();
        insert_def(&conn, "Add", file_nid(file_id, 1), None, None).unwrap();
        insert_import(&conn, "fmt", "fmt", "main.go").unwrap();

        let ref_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM node_refs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ref_count, 1);
        let def_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM node_defs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(def_count, 1);
        let import_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _imports", [], |r| r.get(0))
            .unwrap();
        assert_eq!(import_count, 1);
    }

    #[test]
    fn file_index_roundtrip() {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        create_index_schema(&conn).unwrap();

        upsert_file_index(&conn, "main.go", 1000, 500).unwrap();
        upsert_file_index(&conn, "util.go", 2000, 300).unwrap();

        let index = read_file_index(&conn).unwrap();
        assert_eq!(index.len(), 2);
        assert_eq!(index["main.go"], (1000, 500));
        assert_eq!(index["util.go"], (2000, 300));

        // Upsert overwrites
        upsert_file_index(&conn, "main.go", 3000, 600).unwrap();
        let index = read_file_index(&conn).unwrap();
        assert_eq!(index["main.go"], (3000, 600));
    }

    #[test]
    fn meta_roundtrip() {
        let conn = Connection::open_in_memory().unwrap();
        create_index_schema(&conn).unwrap();

        set_meta(&conn, "source_root", "/tmp/project").unwrap();
        let val: String = conn
            .query_row(
                "SELECT value FROM _meta WHERE key = 'source_root'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(val, "/tmp/project");
    }

    #[test]
    fn meta_upsert_overwrites_existing_key() {
        // _meta uses TEXT PRIMARY KEY on key + INSERT OR REPLACE in
        // set_meta. Pin the overwrite path: subsequent set_meta on
        // the same key replaces the value, doesn't error or duplicate.
        // Load-bearing for the daemon's `tree-sitter_version` /
        // `lsp_version` / per-pass-version meta tracking — these are
        // bumped on every successful pass.
        let conn = Connection::open_in_memory().unwrap();
        create_index_schema(&conn).unwrap();

        set_meta(&conn, "tree-sitter_version", "1").unwrap();
        set_meta(&conn, "tree-sitter_version", "5").unwrap();
        set_meta(&conn, "tree-sitter_version", "12").unwrap();

        let val: String = conn
            .query_row(
                "SELECT value FROM _meta WHERE key = 'tree-sitter_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(val, "12", "third write must win");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _meta WHERE key = 'tree-sitter_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "must not duplicate rows");
    }

    #[test]
    fn get_meta_roundtrip_and_missing_key() {
        // Counterpart to meta_roundtrip: pin get_meta's three-way
        // contract. Drift here would silently change every
        // enrichment-pass version-tracking decision.
        let conn = Connection::open_in_memory().unwrap();
        create_index_schema(&conn).unwrap();

        // Missing key → Ok(None), NOT Err.
        assert_eq!(get_meta(&conn, "absent_key").unwrap(), None);

        // Round-trip: set then get returns the exact value.
        set_meta(&conn, "k1", "v1").unwrap();
        assert_eq!(get_meta(&conn, "k1").unwrap(), Some("v1".to_string()));

        // Overwrite: get reflects the latest set.
        set_meta(&conn, "k1", "v2").unwrap();
        assert_eq!(get_meta(&conn, "k1").unwrap(), Some("v2".to_string()));
    }

    #[test]
    fn get_meta_propagates_sql_errors() {
        // Drift guard against the silent-swallow pattern. If `_meta`
        // doesn't exist (caller has the wrong connection / pre-schema
        // database), get_meta MUST return Err so callers can see and
        // log it. Callers that want "treat missing-table as None" can
        // .ok() at the call site — making the choice explicit.
        let conn = Connection::open_in_memory().unwrap();
        // Note: no create_index_schema call.
        let r = get_meta(&conn, "any");
        assert!(
            r.is_err(),
            "missing _meta table must propagate as Err, got {r:?}",
        );
    }

    /// Mint a minimal two-row file (file node + one leaf) plus a ref and a
    /// def, the v5 way. Returns the file's nid range.
    fn mint_file(conn: &Connection, rel: &str, token: &str) -> (i64, i64) {
        let file_id = ensure_file_id(conn, rel).unwrap();
        let dir_id = ensure_dir_nodes(conn, rel, 1).unwrap();
        let base = file_nid(file_id, 0);
        let name = rel.rsplit_once('/').map(|(_, n)| n).unwrap_or(rel);
        let name_id = intern_name(conn, name).unwrap();
        let k_root = intern_kind(conn, "go", "source_file").unwrap();
        let k_fn = intern_kind(conn, "go", "function_declaration").unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO nodes (nid, parent_nid, name_id, kind_id, kind, ord, mtime, record) \
             VALUES (?1, ?2, ?3, ?4, 1, 0, 1, '')",
            params![base, dir_nid(dir_id), name_id, k_root],
        )
        .unwrap();
        insert_node(
            conn,
            base + 1,
            Some(base),
            None,
            Some(k_fn),
            0,
            0,
            4,
            1,
            "body",
        )
        .unwrap();
        insert_source(conn, rel, "go", b"package x", file_id).unwrap();
        insert_ref(conn, token, base + 1, None, None).unwrap();
        insert_def(conn, token, base + 1, None, None).unwrap();
        upsert_file_index(conn, rel, 100, 50).unwrap();
        file_nid_range(file_id)
    }

    fn rows_in_range(conn: &Connection, table: &str, key: &str, lo: i64, hi: i64) -> i64 {
        conn.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE {key} BETWEEN ?1 AND ?2"),
            params![lo, hi],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn delete_file_rows_cleans_all_tables() {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        create_index_schema(&conn).unwrap();

        // Two files
        let (a_lo, a_hi) = mint_file(&conn, "a.go", "Foo");
        let (b_lo, b_hi) = mint_file(&conn, "b.go", "Bar");

        delete_file_rows(&conn, "a.go").unwrap();

        // a.go gone
        assert_eq!(rows_in_range(&conn, "nodes", "nid", a_lo, a_hi), 0);
        assert_eq!(rows_in_range(&conn, "node_refs", "nid", a_lo, a_hi), 0);
        assert_eq!(rows_in_range(&conn, "node_defs", "nid", a_lo, a_hi), 0);
        let a_source: i64 = conn
            .query_row("SELECT COUNT(*) FROM _source WHERE id = 'a.go'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(a_source, 0);
        let a_index: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _file_index WHERE path = 'a.go'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(a_index, 0);

        // b.go intact
        assert_eq!(rows_in_range(&conn, "nodes", "nid", b_lo, b_hi), 2);
        assert_eq!(rows_in_range(&conn, "node_refs", "nid", b_lo, b_hi), 1);

        // The interning row survives deletion: a re-created a.go re-binds to
        // its old file_id (append-only assignment, no id reuse).
        assert!(lookup_file_id(&conn, "a.go").unwrap().is_some());
    }

    #[test]
    fn delete_file_rows_cleans_lsp_tables_when_present() {
        // Cross-crate cleanup pin. _lsp* tables are created by leyline-
        // lsp::project::create_lsp_schema; if LSP enrichment ran at
        // least once they exist on the connection, and rows are keyed
        // by node_id (matching the file's path namespace). Without
        // explicit cleanup, _lsp* rows accumulate as files churn at
        // registry scale — every file delete+reparse cycle leaves the
        // prior LSP enrichment as orphaned rows.
        //
        // Simulate the leyline-lsp schema in-place (we can't use it
        // directly without inverting the dep graph; the schema is
        // simple enough to recreate here with the same column shapes).
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        create_index_schema(&conn).unwrap();
        conn.execute_batch(
            "CREATE TABLE _lsp (
                nid INTEGER PRIMARY KEY,
                symbol_kind TEXT,
                detail TEXT,
                start_line INTEGER NOT NULL,
                start_col INTEGER NOT NULL,
                end_line INTEGER NOT NULL,
                end_col INTEGER NOT NULL,
                diagnostics TEXT
            );
            CREATE TABLE _lsp_defs (nid INTEGER, def_uri TEXT, def_start_line INT, def_start_col INT, def_end_line INT, def_end_col INT);
            CREATE TABLE _lsp_refs (nid INTEGER, ref_uri TEXT, ref_start_line INT, ref_start_col INT, ref_end_line INT, ref_end_col INT);
            CREATE TABLE _lsp_hover (nid INTEGER PRIMARY KEY, hover_text TEXT);
            CREATE TABLE _lsp_completions (nid INTEGER, label TEXT, kind TEXT, detail TEXT, documentation TEXT, sort_text TEXT);",
        )
        .unwrap();

        let (a_lo, a_hi) = mint_file(&conn, "a.go", "Foo");
        let (b_lo, b_hi) = mint_file(&conn, "b.go", "Bar");

        // Two files' worth of LSP rows, keyed by nids in each file's range.
        conn.execute(
            "INSERT INTO _lsp (nid, symbol_kind, detail, start_line, start_col, end_line, end_col) \
             VALUES (?1, 'function', 'a-detail', 0, 0, 1, 0), \
                    (?2, 'function', 'b-detail', 0, 0, 1, 0)",
            params![a_lo + 1, b_lo + 1],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO _lsp_hover (nid, hover_text) VALUES (?1, 'a-hover'), (?2, 'b-hover')",
            params![a_lo + 1, b_lo + 1],
        )
        .unwrap();

        // Pre-condition: a.go's LSP rows exist.
        assert_eq!(
            rows_in_range(&conn, "_lsp", "nid", a_lo, a_hi),
            1,
            "pre-condition: a.go LSP row should exist"
        );

        delete_file_rows(&conn, "a.go").unwrap();

        // a.go's LSP rows: gone.
        assert_eq!(
            rows_in_range(&conn, "_lsp", "nid", a_lo, a_hi),
            0,
            "_lsp rows for a.go must be cleaned up"
        );
        assert_eq!(
            rows_in_range(&conn, "_lsp_hover", "nid", a_lo, a_hi),
            0,
            "_lsp_hover rows for a.go must be cleaned up"
        );

        // b.go's LSP rows: intact.
        assert_eq!(
            rows_in_range(&conn, "_lsp", "nid", b_lo, b_hi),
            1,
            "_lsp rows for b.go must NOT be cleaned up"
        );
    }

    #[test]
    fn delete_file_rows_skips_lsp_tables_when_absent() {
        // The optional _lsp* cleanup must NOT error when the tables
        // don't exist (i.e. LSP enrichment never ran on this database).
        // Without the IF EXISTS guard, every parse-pass deletion on a
        // never-LSP'd db would hit "no such table: _lsp" and error.
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        create_index_schema(&conn).unwrap();
        // Note: NO _lsp* tables created.

        let (a_lo, a_hi) = mint_file(&conn, "a.go", "Foo");

        // delete_file_rows must succeed even without _lsp* tables.
        delete_file_rows(&conn, "a.go").unwrap();
        assert_eq!(rows_in_range(&conn, "nodes", "nid", a_lo, a_hi), 0);
    }

    #[test]
    fn delete_file_rows_does_not_touch_the_adjacent_range() {
        // Range-boundary pin, the v5 descendant of the pre-v5
        // prefix-sibling trap ("a" vs "ab" under an unanchored LIKE). Two
        // CONSECUTIVE file_ids own adjacent nid ranges; an off-by-one in
        // `file_nid_range` (`.. (file_id+1) << 24` inclusive instead of
        // `.. base | 0xFFFFFF`) deletes the first row of the NEXT file.
        // Plant that exact row: the second file's node at ordinal 0.
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        create_index_schema(&conn).unwrap();

        let (a_lo, a_hi) = mint_file(&conn, "a.go", "Foo");
        let (b_lo, b_hi) = mint_file(&conn, "b.go", "Bar");
        assert_eq!(
            a_hi + 1,
            b_lo,
            "fixture: consecutive file_ids must own adjacent ranges, or this \
             pin asserts nothing"
        );

        delete_file_rows(&conn, "a.go").unwrap();

        assert_eq!(rows_in_range(&conn, "nodes", "nid", a_lo, a_hi), 0);
        assert_eq!(
            rows_in_range(&conn, "nodes", "nid", b_lo, b_hi),
            2,
            "the adjacent file's rows — its ordinal-0 node above all — must \
             survive deletion of its neighbour"
        );
    }

    #[test]
    fn ts_schema_creates_all_indexes() {
        // Scale-problem pin completing the index-existence triplet
        // (leyline-schema ✓, leyline-lsp ✓, leyline-ts ←). These
        // indexes accelerate ref/def token search, per-file occurrence
        // deletes (nid range on idx_refs_node/idx_defs_node), and
        // per-source import enumeration. A refactor DROP'ing any
        // silently degrades query latency on every populated db.
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        create_index_schema(&conn).unwrap();
        // projection-v5: no idx_ast_source — `_ast` file scoping is a PK
        // range; idx_refs_node/idx_defs_node serve the per-file deletes.
        for index_name in [
            "idx_refs_token",
            "idx_refs_node",
            "idx_defs_token",
            "idx_defs_node",
            "idx_imports_source",
        ] {
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='index' AND name=?1",
                    [index_name],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(exists, "missing index: {index_name}");
        }
    }

    #[test]
    fn read_file_index_handles_thousand_entries() {
        // Scale-problem pin. read_file_index loads ALL _file_index
        // rows into a HashMap at once — at 50k files (a registry-
        // sized repo) this is ~3 MB held in memory per call. The
        // existing roundtrip test covers 2 entries, which can't catch
        // a refactor that introduced a LIMIT, an early break, or a
        // chunked read that silently dropped the tail. Pin: 1000
        // entries round-trip identity (a refactor stopping at
        // SQLite's default page-size boundary would catch here).
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        create_index_schema(&conn).unwrap();

        for i in 0..1000 {
            upsert_file_index(&conn, &format!("path/{i:04}.go"), i as i64, (i * 7) as i64).unwrap();
        }

        let index = read_file_index(&conn).unwrap();
        assert_eq!(index.len(), 1000, "must read every row, no truncation");
        // Spot-check the first, middle, and last entries.
        assert_eq!(index["path/0000.go"], (0, 0));
        assert_eq!(index["path/0500.go"], (500, 500 * 7));
        assert_eq!(index["path/0999.go"], (999, 999 * 7));
    }

    #[test]
    fn sweep_orphaned_dirs_handles_deep_nesting() {
        // Scale-problem pin. sweep_orphaned_dirs runs DELETE in a
        // loop until no rows are removed — depth-N nesting needs N
        // iterations because each pass only deletes the
        // currently-leaf dirs. Helm-charts ingest sweeps 2k+ orphan
        // dirs across many depths; a 50k-file registry repo could
        // hit depth 20+. Pin: a 30-deep chain terminates and removes
        // all 30 orphan dirs in one call. A refactor that capped
        // iterations or used a single non-recursive DELETE would
        // leave deep orphans behind.
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();

        // Build a deeply-nested chain: root→d0→d0/d1→...→d0/.../d29→file.
        let rel = (0..30)
            .map(|i| format!("d{i}"))
            .collect::<Vec<_>>()
            .join("/")
            + "/leaf.go";
        let file_id = ensure_file_id(&conn, &rel).unwrap();
        let dir_id = ensure_dir_nodes(&conn, &rel, 1).unwrap();
        let base = file_nid(file_id, 0);
        let name_id = intern_name(&conn, "leaf.go").unwrap();
        insert_node(
            &conn,
            base,
            Some(dir_nid(dir_id)),
            Some(name_id),
            None,
            1,
            0,
            0,
            1,
            "",
        )
        .unwrap();

        // Delete the file — every dir in the chain is now orphaned.
        conn.execute("DELETE FROM nodes WHERE nid = ?1", [base])
            .unwrap();

        let removed = sweep_orphaned_dirs(&conn).unwrap();
        assert_eq!(removed, 30, "must sweep all 30 nested dirs");
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1, "only the root dir row should remain");
    }

    // ---------------------------------------------------------------------
    // T1.b2 — _cfg + _cfg_edge schema DDL (bead `ley-line-open-46d46b`)
    // ---------------------------------------------------------------------

    /// Insert a `node_content` row for a synthetic subtree so the
    /// `_cfg.node_hash` FK has a real target to point at. The tests
    /// here don't care about the content-addressing semantics — just
    /// that the FK resolves.
    fn insert_test_node_content(conn: &Connection, node_hash: &[u8]) {
        conn.execute(
            "INSERT OR IGNORE INTO node_content (node_hash, node_tag, kind, raw_kind, lang, token, arity) \
             VALUES (?1, 1, 'function', 'function_declaration', 'go', NULL, 0)",
            rusqlite::params![node_hash],
        )
        .unwrap();
    }

    #[test]
    fn schema_cfg_ddl_creates_tables() {
        // Bead ley-line-open-46d46b. Pin the additive DDL — `_cfg` and
        // `_cfg_edge` exist after create_cfg_schema, indexes registered,
        // idempotent on repeat call.
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        create_ir_tables(&conn).unwrap();
        create_cfg_schema(&conn).unwrap();

        for table in ["_cfg", "_cfg_edge"] {
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(exists, "table missing: {table}");
        }
        for index_name in ["idx_cfg_source", "idx_cfg_edge_from", "idx_cfg_edge_to"] {
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='index' AND name=?1",
                    [index_name],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(exists, "index missing: {index_name}");
        }

        // Idempotent — second call must succeed (uses IF NOT EXISTS).
        create_cfg_schema(&conn).unwrap();
    }

    #[test]
    fn schema_cfg_ddl_enforces_foreign_keys() {
        // Bead ley-line-open-46d46b. FK-enforcement is the whole point
        // of the additive schema — a `_cfg` row with `node_hash` that
        // has no `node_content` target MUST error at insert, not
        // silently corrupt the analysis substrate.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        create_ir_tables(&conn).unwrap();
        create_cfg_schema(&conn).unwrap();

        let orphan_hash = &[0xFFu8; 32][..];
        let insert_result = conn.execute(
            "INSERT INTO _cfg (node_hash, source_id, block_id, block_kind, entry_offset, exit_offset) \
             VALUES (?1, 'a.go', 0, 'branch', 0, 42)",
            rusqlite::params![orphan_hash],
        );
        assert!(
            insert_result.is_err(),
            "orphan _cfg.node_hash MUST error under PRAGMA foreign_keys=ON, got Ok",
        );

        // Companion positive case: with a real node_content row, the
        // insert succeeds.
        let real_hash = &[0x11u8; 32][..];
        insert_test_node_content(&conn, real_hash);
        conn.execute(
            "INSERT INTO _cfg (node_hash, source_id, block_id, block_kind, entry_offset, exit_offset) \
             VALUES (?1, 'a.go', 0, 'branch', 0, 42)",
            rusqlite::params![real_hash],
        )
        .unwrap();
    }

    #[test]
    fn schema_cfg_ddl_edge_fks_enforce_endpoints() {
        // Bead ley-line-open-46d46b. Companion of the previous test for
        // the composite FK on `_cfg_edge`. An edge to a block_id that
        // doesn't exist in `_cfg` MUST error.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        create_ir_tables(&conn).unwrap();
        create_cfg_schema(&conn).unwrap();

        let hash = &[0x22u8; 32][..];
        insert_test_node_content(&conn, hash);

        // Insert one real block; edges to block_id=999 must error.
        conn.execute(
            "INSERT INTO _cfg (node_hash, source_id, block_id, block_kind, entry_offset, exit_offset) \
             VALUES (?1, 'a.go', 0, 'branch', 0, 42)",
            rusqlite::params![hash],
        )
        .unwrap();

        let bad_edge = conn.execute(
            "INSERT INTO _cfg_edge (from_node_hash, from_block_id, to_node_hash, to_block_id, edge_kind) \
             VALUES (?1, 0, ?1, 999, 'fallthrough')",
            rusqlite::params![hash],
        );
        assert!(
            bad_edge.is_err(),
            "_cfg_edge.to_block_id=999 with no matching _cfg row MUST error, got Ok",
        );
    }

    #[test]
    fn schema_cfg_ddl_complexity_column_is_nullable() {
        // Bead ley-line-open-46d46b. T1.b3 (CFG builder) lands the
        // schema BEFORE T1.b4 (cyclomatic complexity) wires the
        // computation. `_cfg.complexity` MUST accept NULL so T1.b3 can
        // ship without stamping the column, and T1.b4's UPDATE fills
        // it in later. Pin the nullable contract so a future refactor
        // adding NOT NULL breaks the phasing loudly.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        create_ir_tables(&conn).unwrap();
        create_cfg_schema(&conn).unwrap();

        let hash = &[0x33u8; 32][..];
        insert_test_node_content(&conn, hash);

        // Insert with NULL complexity — must succeed.
        conn.execute(
            "INSERT INTO _cfg (node_hash, source_id, block_id, block_kind, entry_offset, exit_offset, complexity) \
             VALUES (?1, 'a.go', 0, 'branch', 0, 42, NULL)",
            rusqlite::params![hash],
        )
        .unwrap();

        // Read back NULL as Option<i64>::None.
        let stored: Option<i64> = conn
            .query_row(
                "SELECT complexity FROM _cfg WHERE node_hash = ?1 AND block_id = 0",
                rusqlite::params![hash],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, None, "complexity must round-trip as NULL");

        // Update with a real complexity — must succeed and be visible
        // to a subsequent read.
        conn.execute(
            "UPDATE _cfg SET complexity = ?1 WHERE node_hash = ?2 AND block_id = 0",
            rusqlite::params![7i64, hash],
        )
        .unwrap();
        let updated: Option<i64> = conn
            .query_row(
                "SELECT complexity FROM _cfg WHERE node_hash = ?1 AND block_id = 0",
                rusqlite::params![hash],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(updated, Some(7));
    }

    #[test]
    fn schema_cfg_ddl_primary_key_dedupes_identical_blocks() {
        // Bead ley-line-open-46d46b. Two byte-identical function
        // bodies produce the same `node_hash` (ADR-0027 merkle-AST
        // dedup); the CFG built for that body is a pure function of
        // the hash, so both should collapse to ONE `_cfg` row set —
        // not two separately-keyed copies. The `(node_hash, block_id)`
        // PRIMARY KEY is the enforcer: `INSERT OR IGNORE` on the second
        // parse of the same body is a no-op.
        //
        // This is the dedup story that the whole differential-dataflow
        // arrangement in T3 hinges on — pin loudly.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        create_ir_tables(&conn).unwrap();
        create_cfg_schema(&conn).unwrap();

        let hash = &[0x44u8; 32][..];
        insert_test_node_content(&conn, hash);

        // First parse.
        conn.execute(
            "INSERT INTO _cfg (node_hash, source_id, block_id, block_kind, entry_offset, exit_offset) \
             VALUES (?1, 'a.go', 0, 'branch', 0, 42)",
            rusqlite::params![hash],
        )
        .unwrap();

        // Second parse — same body, different file. INSERT OR IGNORE
        // must silently keep the first row.
        conn.execute(
            "INSERT OR IGNORE INTO _cfg (node_hash, source_id, block_id, block_kind, entry_offset, exit_offset) \
             VALUES (?1, 'b.go', 0, 'branch', 0, 42)",
            rusqlite::params![hash],
        )
        .unwrap();

        let row_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _cfg WHERE node_hash = ?1",
                rusqlite::params![hash],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            row_count, 1,
            "PRIMARY KEY (node_hash, block_id) must collapse identical bodies to one row"
        );

        // The first-writer's source_id wins under INSERT OR IGNORE.
        let source_id: String = conn
            .query_row(
                "SELECT source_id FROM _cfg WHERE node_hash = ?1 AND block_id = 0",
                rusqlite::params![hash],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            source_id, "a.go",
            "first-writer's source_id must win under INSERT OR IGNORE",
        );
    }

    /// `create_ir_tables` builds the ADR-0027 content tables and the base
    /// DDL's `node_hash` FK is enforceable. (The pre-v5 additive-ALTER
    /// mutation story this doc used to tell — `has_column` guards, inverted
    /// idempotence — died with the migrations themselves in projection-v5;
    /// the surviving contract is table existence, idempotence, and a live
    /// FK.)
    #[test]
    fn create_ir_tables_builds_content_tables_and_fk_holds() {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        conn.execute_batch(DEFS_TABLE_DDL).unwrap();
        conn.execute_batch(REFS_TABLE_DDL).unwrap();
        create_ir_tables(&conn).unwrap();

        for table in ["node_content", "node_child"] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "{table} MUST exist after create_ir_tables");
        }

        // Idempotent — runs on every parse.
        create_ir_tables(&conn).unwrap();

        // The base-DDL `node_hash` FK is enforceable: with foreign_keys ON, a
        // pointer at a nonexistent content row is a loud insert error, and a
        // resolving pointer succeeds.
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let dangling = conn.execute(
            "INSERT INTO _ast (nid, kind_id, start_byte, end_byte, start_row, start_col, end_row, end_col, node_hash) \
             VALUES (1, 1, 0, 1, 0, 0, 0, 1, X'00')",
            [],
        );
        assert!(
            dangling.is_err(),
            "a node_hash with no node_content row must fail under FK enforcement"
        );
        insert_test_node_content(&conn, &[0u8; 32]);
        conn.execute(
            "INSERT INTO _ast (nid, kind_id, start_byte, end_byte, start_row, start_col, end_row, end_col, node_hash) \
             VALUES (1, 1, 0, 1, 0, 0, 0, 1, ?1)",
            params![[0u8; 32]],
        )
        .unwrap();
    }

    /// **The v5 base DDL carries every occurrence column at birth.**
    ///
    /// Pre-v5, five additive ALTER migrations stamped these columns onto
    /// legacy arenas, and each survived body-replaced-by-`Ok(())` mutation at
    /// least once. projection-v5 removed the migrations outright — a pre-v5
    /// arena is refused at open, not patched — so the surviving guarantee is
    /// simpler and pinned here: the columns exist on a FRESH arena, which is
    /// the only arena shape v5 code ever writes.
    #[test]
    fn base_ddl_carries_every_occurrence_column() {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        for (table, cols) in [
            (
                "_ast",
                &["nid", "kind_id", "start_byte", "end_col", "node_hash"][..],
            ),
            (
                "node_defs",
                &[
                    "token",
                    "nid",
                    "container_nid",
                    "node_kind",
                    "start_byte",
                    "end_col",
                    "canonical_kind",
                    "node_hash",
                ][..],
            ),
            (
                "node_refs",
                &[
                    "token",
                    "nid",
                    "container_nid",
                    "node_kind",
                    "start_byte",
                    "end_col",
                    "qualifier",
                    "node_hash",
                ][..],
            ),
        ] {
            for col in cols {
                assert!(
                    has_column(&conn, table, col).unwrap(),
                    "{table}.{col} MUST exist in the base DDL — every INSERT \
                     naming it fails at runtime otherwise"
                );
            }
        }
    }

    /// Test-support probe (the production ALTER migrations that used it are
    /// gone as of projection-v5).
    fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
            [table, column],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    #[test]
    fn has_column_distinguishes_present_from_absent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (a INTEGER, b TEXT);")
            .unwrap();

        assert!(has_column(&conn, "t", "a").unwrap(), "column a is present");
        assert!(has_column(&conn, "t", "b").unwrap(), "column b is present");
        assert!(
            !has_column(&conn, "t", "nope").unwrap(),
            "column `nope` is absent"
        );
        assert!(
            !has_column(&conn, "t", "").unwrap(),
            "the empty name matches nothing"
        );
    }

    /// The ADR-0026 pointer store is `capnp_blobs` + the per-file `_ast_blob`
    /// map; the ordinal into a blob's `AstNodeList.nodes` is `nid & 0xFFFFFF`
    /// as of projection-v5 (no stored column). Nothing asserted the
    /// constructor actually built the tables, so replacing its body with
    /// `Ok(())` passed — an arena would then silently lack the resolution
    /// path entirely.
    #[test]
    fn create_pointer_store_tables_builds_and_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();

        assert!(
            !pointer_store_present(&conn),
            "precondition: the pointer store does not exist yet"
        );

        create_pointer_store_tables(&conn).unwrap();

        for table in ["capnp_blobs", "_ast_blob"] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "{table} MUST exist after create_pointer_store_tables");
        }
        assert!(
            has_column(&conn, "_ast_blob", "file_id").unwrap(),
            "_ast_blob keys on the interned file_id as of projection-v5"
        );
        assert!(
            pointer_store_present(&conn),
            "pointer_store_present MUST report the store it just built; a \
             constant-false gates the per-file delete in delete_file_rows off \
             and leaks a stale _ast_blob row on every reparse"
        );

        // Second call must be a no-op.
        create_pointer_store_tables(&conn).unwrap();
    }

    /// Review-gate 3 of the identity ladder (bead `ley-line-open-17c271`):
    /// file-scoped deletes must plan as an index/PK range SEARCH, never a
    /// SCAN. The pre-v5 prefix-LIKE (`node_id LIKE ?1 || '/%'`) planned as
    /// a full SCAN on every one of these tables — no `case_sensitive_like`,
    /// non-literal prefix — measured 1,370–2,000× slower than a PK seek.
    #[test]
    fn file_scoped_deletes_plan_as_search_not_scan() {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        create_index_schema(&conn).unwrap();
        create_ast_indexes(&conn).unwrap();
        create_refs_indexes(&conn).unwrap();
        let (a_lo, a_hi) = mint_file(&conn, "a.go", "Foo");

        for sql in [
            "DELETE FROM nodes WHERE nid BETWEEN ?1 AND ?2",
            "DELETE FROM _ast WHERE nid BETWEEN ?1 AND ?2",
            "DELETE FROM node_refs WHERE nid BETWEEN ?1 AND ?2",
            "DELETE FROM node_defs WHERE nid BETWEEN ?1 AND ?2",
        ] {
            let plan: Vec<String> = {
                let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
                stmt.query_map(params![a_lo, a_hi], |r| r.get::<_, String>(3))
                    .unwrap()
                    .map(|r| r.unwrap())
                    .collect()
            };
            let rendered = plan.join(" | ");
            assert!(
                rendered.contains("SEARCH"),
                "{sql}: plan must SEARCH an index or the PK; got {rendered:?}"
            );
            assert!(
                !rendered.contains("SCAN"),
                "{sql}: plan must not SCAN the table; got {rendered:?}"
            );
        }
    }

    #[test]
    fn sweep_orphaned_dirs_removes_empty_parents() {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();

        let rel = "src/pkg/a.go";
        let file_id = ensure_file_id(&conn, rel).unwrap();
        let dir_id = ensure_dir_nodes(&conn, rel, 1).unwrap();
        let base = file_nid(file_id, 0);
        let name_id = intern_name(&conn, "a.go").unwrap();
        insert_node(
            &conn,
            base,
            Some(dir_nid(dir_id)),
            Some(name_id),
            None,
            1,
            0,
            0,
            1,
            "",
        )
        .unwrap();

        conn.execute("DELETE FROM nodes WHERE nid = ?1", [base])
            .unwrap();

        let removed = sweep_orphaned_dirs(&conn).unwrap();
        assert_eq!(removed, 2, "should remove src/pkg and src");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "only the root dir row should remain");
    }

    // ---------------------------------------------------------------------
    // Schema-construction EFFECTS.
    //
    // Every `create_*` here returns `Result<()>`, so a test that only
    // `.unwrap()`s the call observes nothing: a body replaced with `Ok(())`
    // passes it. Each test below therefore starts from a BARE connection —
    // no sibling `create_*` that would have made the tables anyway — and
    // asserts the objects the function is responsible for actually landed.
    // ---------------------------------------------------------------------

    /// Whether `sqlite_master` holds an object of this type under this name.
    fn has_object(conn: &Connection, kind: &str, name: &str) -> bool {
        conn.query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = ?1 AND name = ?2",
            params![kind, name],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn create_ast_tables_creates_every_table_the_ast_pass_writes_to() {
        // Bare connection ON PURPOSE. The sibling `create_ast_schema` builds
        // the same five tables, so a test that called it first would pass
        // against a `create_ast_tables` that did nothing at all.
        let conn = Connection::open_in_memory().unwrap();
        for table in ["nodes", "_source", "node_content", "node_child", "_ast"] {
            assert!(
                !has_object(&conn, "table", table),
                "precondition: {table} must not exist before the call",
            );
        }

        create_ast_tables(&conn).unwrap();

        for table in ["nodes", "_source", "node_content", "node_child", "_ast"] {
            assert!(
                has_object(&conn, "table", table),
                "{table} MUST exist after create_ast_tables",
            );
        }

        // Usable, not merely present: the bulk-load pass writes a source row
        // and an `_ast` row through exactly these tables.
        let file_id = ensure_file_id(&conn, "main.go").unwrap();
        insert_source(&conn, "main.go", "go", b"package main\n", file_id).unwrap();
        insert_ast(&conn, file_nid(file_id, 0), 1, 0, 13, 0, 0, 1, 0, None).unwrap();

        // Idempotent — `cmd_parse` calls it once per invocation on a
        // possibly-existing arena.
        create_ast_tables(&conn).unwrap();
    }

    #[test]
    fn insert_source_ref_writes_a_path_row_with_no_inline_content() {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_tables(&conn).unwrap();

        insert_source_ref(&conn, "pkg/main.go", "go", "/repo/pkg/main.go", 7).unwrap();

        let (id, language, path, content, file_id): (
            String,
            String,
            Option<String>,
            Option<Vec<u8>>,
            i64,
        ) = conn
            .query_row(
                "SELECT id, language, path, content, file_id FROM _source",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(id, "pkg/main.go");
        assert_eq!(language, "go");
        assert_eq!(path.as_deref(), Some("/repo/pkg/main.go"));
        assert_eq!(
            content, None,
            "reference mode stores NO inline content — consumers read from disk",
        );
        assert_eq!(
            file_id, 7,
            "the interned file id is what `nid >> 24` joins against",
        );

        // INSERT OR REPLACE: re-registering the same id updates in place.
        insert_source_ref(&conn, "pkg/main.go", "go", "/elsewhere/main.go", 7).unwrap();
        let (n, path): (i64, Option<String>) = conn
            .query_row("SELECT COUNT(*), MAX(path) FROM _source", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(n, 1, "re-registration must replace, not duplicate");
        assert_eq!(path.as_deref(), Some("/elsewhere/main.go"));
    }

    #[test]
    fn create_ir_tables_creates_the_content_tables_on_a_bare_connection() {
        // The companion test above this one
        // (`create_ir_tables_builds_content_tables_and_fk_holds`) calls
        // `create_ast_schema` first, which ALSO emits
        // NODE_CONTENT_TABLE_DDL / NODE_CHILD_TABLE_DDL — so it cannot see
        // the difference between this function working and doing nothing.
        // This one starts bare, which is the only way to observe the effect.
        let conn = Connection::open_in_memory().unwrap();
        assert!(!has_object(&conn, "table", "node_content"));
        assert!(!has_object(&conn, "table", "node_child"));

        create_ir_tables(&conn).unwrap();

        assert!(
            has_object(&conn, "table", "node_content"),
            "node_content MUST exist after create_ir_tables",
        );
        assert!(
            has_object(&conn, "table", "node_child"),
            "node_child MUST exist after create_ir_tables",
        );

        // Usable: a content row and an edge referencing it both insert.
        insert_test_node_content(&conn, &[0xABu8; 32]);
        conn.execute(
            "INSERT INTO node_child (parent_hash, ordinal, child_hash, field) \
             VALUES (?1, 0, ?1, 'body')",
            params![&[0xABu8; 32][..]],
        )
        .unwrap();

        create_ir_tables(&conn).unwrap(); // idempotent
    }

    #[test]
    fn create_post_load_indexes_skip_unused_creates_its_indexes_and_skips_idx_source_file() {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_tables(&conn).unwrap();
        create_refs_tables(&conn).unwrap();

        // Tables only so far — the whole point of the post-load pass is that
        // none of these exist during the bulk insert.
        for index in [
            "idx_parent_kind_ord",
            "idx_refs_token",
            "idx_refs_node",
            "idx_refs_container",
        ] {
            assert!(
                !has_object(&conn, "index", index),
                "precondition: {index} must not exist before the post-load pass",
            );
        }
        assert!(!has_object(&conn, "view", "v_node_path"));

        create_post_load_indexes_skip_unused(&conn).unwrap();

        for index in [
            "idx_parent_kind_ord",
            "idx_refs_token",
            "idx_refs_node",
            "idx_refs_container",
        ] {
            assert!(
                has_object(&conn, "index", index),
                "{index} MUST exist after create_post_load_indexes_skip_unused",
            );
        }
        assert!(
            has_object(&conn, "view", "v_node_path"),
            "the display view lands post-COMMIT too",
        );

        // The "skip_unused" half of the contract (bead
        // `ley-line-open-cbbedf` Attack 3): `idx_source_file` is the partial
        // index ley-line never populates, and it must NOT be built here.
        assert!(
            !has_object(&conn, "index", "idx_source_file"),
            "idx_source_file is the index this variant exists to skip",
        );

        create_post_load_indexes_skip_unused(&conn).unwrap(); // idempotent
    }
}
