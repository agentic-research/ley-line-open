//! Schema definitions for the AST projection tables.
//!
//! Re-exports the shared `nodes` table from `leyline-schema` and adds
//! AST-specific tables (`_source`, `_ast`) that enable bidirectional splicing.

pub use leyline_schema::{
    NODES_DDL, NODES_INDEXES_DDL, NODES_TABLE_DDL, create_nodes_indexes, create_nodes_table,
    create_schema, insert_node,
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
    path TEXT
);";

/// DDL for the `_ast` table — table only, no indexes. Pairs with
/// [`AST_INDEXES_DDL`] for bulk-load callers (see bead
/// `ley-line-open-9ccbc7`).
pub const AST_TABLE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _ast (
    node_id TEXT PRIMARY KEY,
    source_id TEXT NOT NULL,
    node_kind TEXT NOT NULL,
    start_byte INTEGER NOT NULL,
    end_byte INTEGER NOT NULL,
    start_row INTEGER NOT NULL,
    start_col INTEGER NOT NULL,
    end_row INTEGER NOT NULL,
    end_col INTEGER NOT NULL
);";

/// DDL for the `_ast` indexes — deferred post-COMMIT for bulk-load.
pub const AST_INDEXES_DDL: &str = "CREATE INDEX IF NOT EXISTS idx_ast_source ON _ast(source_id);";

/// Combined `_ast` table + index DDL. Preserves the pre-split contract.
pub const AST_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _ast (
    node_id TEXT PRIMARY KEY,
    source_id TEXT NOT NULL,
    node_kind TEXT NOT NULL,
    start_byte INTEGER NOT NULL,
    end_byte INTEGER NOT NULL,
    start_row INTEGER NOT NULL,
    start_col INTEGER NOT NULL,
    end_row INTEGER NOT NULL,
    end_col INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_ast_source ON _ast(source_id);";

/// Create `nodes`, `_source`, and `_ast` tables + indexes (idempotent).
///
/// For bulk-load callers (e.g. `cmd_parse`), prefer the split
/// [`create_ast_tables`] + [`create_ast_indexes`] pair so the indexes
/// can be deferred until after `COMMIT`.
pub fn create_ast_schema(conn: &Connection) -> Result<()> {
    create_schema(conn)?;
    conn.execute_batch(SOURCE_DDL)?;
    conn.execute_batch(AST_DDL)?;
    Ok(())
}

/// Create `nodes`, `_source`, `_ast` tables only — no indexes. Pair
/// with [`create_ast_indexes`] post-`COMMIT` for bulk-load paths.
pub fn create_ast_tables(conn: &Connection) -> Result<()> {
    create_nodes_table(conn)?;
    conn.execute_batch(SOURCE_DDL)?;
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
pub fn insert_source(conn: &Connection, id: &str, language: &str, content: &[u8]) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO _source (id, language, content) VALUES (?1, ?2, ?3)",
        params![id, language, content],
    )?;
    Ok(())
}

/// Insert or replace a source row with a file path reference (multi-file CLI).
/// No content BLOB is stored — consumers read from disk via `path`.
pub fn insert_source_ref(conn: &Connection, id: &str, language: &str, path: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO _source (id, language, path) VALUES (?1, ?2, ?3)",
        params![id, language, path],
    )?;
    Ok(())
}

/// Insert an AST byte-range mapping.
#[allow(clippy::too_many_arguments)]
pub fn insert_ast(
    conn: &Connection,
    node_id: &str,
    source_id: &str,
    node_kind: &str,
    start_byte: usize,
    end_byte: usize,
    start_row: usize,
    start_col: usize,
    end_row: usize,
    end_col: usize,
) -> Result<()> {
    // rusqlite 0.39 dropped the blanket `ToSql for usize` — bind through
    // `i64` instead. Tree-sitter byte/row/col indices fit comfortably in
    // `i64` (well under 2^63 even for pathological source files), so the
    // cast is lossless.
    conn.execute(
        "INSERT OR REPLACE INTO _ast (node_id, source_id, node_kind, start_byte, end_byte, \
         start_row, start_col, end_row, end_col) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            node_id,
            source_id,
            node_kind,
            start_byte as i64,
            end_byte as i64,
            start_row as i64,
            start_col as i64,
            end_row as i64,
            end_col as i64,
        ],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Refs / Defs / Imports tables
// ---------------------------------------------------------------------------

/// DDL for the `node_refs` table — table only, no indexes.
pub const REFS_TABLE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS node_refs (
    token TEXT NOT NULL,
    node_id TEXT NOT NULL,
    source_id TEXT NOT NULL
);";

/// DDL for the `node_refs` indexes — deferred post-COMMIT.
pub const REFS_INDEXES_DDL: &str = "\
CREATE INDEX IF NOT EXISTS idx_refs_token ON node_refs(token);
CREATE INDEX IF NOT EXISTS idx_refs_node ON node_refs(node_id);";

/// Combined `node_refs` table + index DDL.
pub const REFS_DDL: &str = "\
CREATE TABLE IF NOT EXISTS node_refs (
    token TEXT NOT NULL,
    node_id TEXT NOT NULL,
    source_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_refs_token ON node_refs(token);
CREATE INDEX IF NOT EXISTS idx_refs_node ON node_refs(node_id);";

/// DDL for the `node_defs` table — table only, no indexes.
pub const DEFS_TABLE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS node_defs (
    token TEXT NOT NULL,
    node_id TEXT NOT NULL,
    source_id TEXT NOT NULL
);";

/// DDL for the `node_defs` indexes — deferred post-COMMIT.
pub const DEFS_INDEXES_DDL: &str = "CREATE INDEX IF NOT EXISTS idx_defs_token ON node_defs(token);";

/// Combined `node_defs` table + index DDL.
pub const DEFS_DDL: &str = "\
CREATE TABLE IF NOT EXISTS node_defs (
    token TEXT NOT NULL,
    node_id TEXT NOT NULL,
    source_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_defs_token ON node_defs(token);";

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
pub fn insert_ref(conn: &Connection, token: &str, node_id: &str, source_id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO node_refs (token, node_id, source_id) VALUES (?1, ?2, ?3)",
        params![token, node_id, source_id],
    )?;
    Ok(())
}

/// Insert a definition row.
pub fn insert_def(conn: &Connection, token: &str, node_id: &str, source_id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO node_defs (token, node_id, source_id) VALUES (?1, ?2, ?3)",
        params![token, node_id, source_id],
    )?;
    Ok(())
}

/// Insert an import row.
pub fn insert_import(conn: &Connection, alias: &str, path: &str, source_id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO _imports (alias, path, source_id) VALUES (?1, ?2, ?3)",
        params![alias, path, source_id],
    )?;
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
    // Just `idx_parent_name` from the nodes-indexes pair — the second
    // (`idx_source_file`) is the unused one we're skipping.
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_parent_name ON nodes(parent_id, name);")?;
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
    conn.execute(
        "DELETE FROM nodes WHERE id = ?1 OR id LIKE ?1 || '/%'",
        [path],
    )?;
    conn.execute("DELETE FROM _ast WHERE source_id = ?1", [path])?;
    conn.execute("DELETE FROM _source WHERE id = ?1", [path])?;
    conn.execute("DELETE FROM node_refs WHERE source_id = ?1", [path])?;
    conn.execute("DELETE FROM node_defs WHERE source_id = ?1", [path])?;
    conn.execute("DELETE FROM _imports WHERE source_id = ?1", [path])?;
    conn.execute("DELETE FROM _file_index WHERE path = ?1", [path])?;
    delete_lsp_rows_for_path(conn, path)?;
    Ok(())
}

/// Delete `_lsp*` rows whose `node_id` is in the deleted file's path
/// namespace. Tables created by leyline-lsp's `create_lsp_schema` are
/// optional; we discover their presence via `sqlite_master` and skip
/// missing ones so callers that never enabled LSP enrichment pay
/// nothing.
///
/// Without this cleanup, `_lsp*` rows accumulate at registry scale as
/// files churn — every file deleted+reparsed leaves the prior LSP
/// enrichment as orphans keyed by node_ids that no longer resolve.
fn delete_lsp_rows_for_path(conn: &Connection, path: &str) -> Result<()> {
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
        // Both equal-match and prefix-match: the file's "root" node_id
        // (the path itself) AND every descendant
        // (`<path>/<ast_path>`).
        let sql = format!("DELETE FROM {table} WHERE node_id = ?1 OR node_id LIKE ?1 || '/%'",);
        conn.execute(&sql, [path])?;
    }
    Ok(())
}

/// Remove directory nodes (kind = 1) that have no children, iterating until
/// no more orphans remain. Returns the total number of rows removed.
pub fn sweep_orphaned_dirs(conn: &Connection) -> Result<usize> {
    let mut total = 0;
    loop {
        let removed = conn.execute(
            "DELETE FROM nodes WHERE kind = 1 AND id != '' \
             AND id NOT IN (SELECT DISTINCT parent_id FROM nodes WHERE parent_id IS NOT NULL AND parent_id != '')",
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

        insert_ref(&conn, "Println", "main.go/call_expression", "main.go").unwrap();
        insert_def(&conn, "Add", "main.go/function_declaration", "main.go").unwrap();
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

    #[test]
    fn delete_file_rows_cleans_all_tables() {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        create_index_schema(&conn).unwrap();

        // Two files
        insert_node(&conn, "", "", "", 1, 0, 0, "").unwrap();
        insert_node(&conn, "a.go", "", "a.go", 1, 0, 0, "").unwrap();
        insert_node(&conn, "a.go/func", "a.go", "func", 0, 10, 0, "body").unwrap();
        insert_node(&conn, "b.go", "", "b.go", 1, 0, 0, "").unwrap();
        insert_node(&conn, "b.go/func", "b.go", "func", 0, 10, 0, "body").unwrap();
        insert_source(&conn, "a.go", "go", b"package a").unwrap();
        insert_source(&conn, "b.go", "go", b"package b").unwrap();
        insert_ref(&conn, "Foo", "a.go/call", "a.go").unwrap();
        insert_ref(&conn, "Bar", "b.go/call", "b.go").unwrap();
        insert_def(&conn, "Foo", "a.go/func", "a.go").unwrap();
        insert_def(&conn, "Bar", "b.go/func", "b.go").unwrap();
        upsert_file_index(&conn, "a.go", 100, 50).unwrap();
        upsert_file_index(&conn, "b.go", 200, 60).unwrap();

        delete_file_rows(&conn, "a.go").unwrap();

        // a.go gone
        let a_nodes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE id = 'a.go' OR id LIKE 'a.go/%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(a_nodes, 0);
        let a_source: i64 = conn
            .query_row("SELECT COUNT(*) FROM _source WHERE id = 'a.go'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(a_source, 0);
        let a_refs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM node_refs WHERE source_id = 'a.go'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(a_refs, 0);
        let a_index: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _file_index WHERE path = 'a.go'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(a_index, 0);

        // b.go intact
        let b_nodes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE id = 'b.go' OR id LIKE 'b.go/%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(b_nodes >= 2);
        let b_refs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM node_refs WHERE source_id = 'b.go'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(b_refs, 1);
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
                node_id TEXT PRIMARY KEY,
                symbol_kind TEXT,
                detail TEXT,
                start_line INTEGER NOT NULL,
                start_col INTEGER NOT NULL,
                end_line INTEGER NOT NULL,
                end_col INTEGER NOT NULL,
                diagnostics TEXT
            );
            CREATE TABLE _lsp_defs (node_id TEXT, def_uri TEXT, def_start_line INT, def_start_col INT, def_end_line INT, def_end_col INT);
            CREATE TABLE _lsp_refs (node_id TEXT, ref_uri TEXT, ref_start_line INT, ref_start_col INT, ref_end_line INT, ref_end_col INT);
            CREATE TABLE _lsp_hover (node_id TEXT PRIMARY KEY, hover_text TEXT);
            CREATE TABLE _lsp_completions (node_id TEXT, label TEXT, kind TEXT, detail TEXT, documentation TEXT, sort_text TEXT);",
        )
        .unwrap();

        // Two files' worth of LSP rows. Use the file's own path as one
        // of the node_ids and a descendant for the other.
        conn.execute(
            "INSERT INTO _lsp (node_id, symbol_kind, detail, start_line, start_col, end_line, end_col) \
             VALUES ('a.go/func', 'function', 'a-detail', 0, 0, 1, 0), \
                    ('b.go/func', 'function', 'b-detail', 0, 0, 1, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO _lsp_hover (node_id, hover_text) VALUES ('a.go/func', 'a-hover'), ('b.go/func', 'b-hover')",
            [],
        )
        .unwrap();

        // Pre-condition: a.go's LSP rows exist.
        let a_pre: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _lsp WHERE node_id LIKE 'a.go%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(a_pre, 1, "pre-condition: a.go LSP row should exist");

        delete_file_rows(&conn, "a.go").unwrap();

        // a.go's LSP rows: gone.
        let a_lsp: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _lsp WHERE node_id LIKE 'a.go%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(a_lsp, 0, "_lsp rows for a.go must be cleaned up");
        let a_hover: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _lsp_hover WHERE node_id LIKE 'a.go%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(a_hover, 0, "_lsp_hover rows for a.go must be cleaned up");

        // b.go's LSP rows: intact.
        let b_lsp: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _lsp WHERE node_id LIKE 'b.go%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(b_lsp, 1, "_lsp rows for b.go must NOT be cleaned up");
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

        insert_node(&conn, "a.go", "", "a.go", 1, 0, 0, "").unwrap();
        upsert_file_index(&conn, "a.go", 100, 50).unwrap();

        // delete_file_rows must succeed even without _lsp* tables.
        delete_file_rows(&conn, "a.go").unwrap();
        let a_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes WHERE id = 'a.go'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(a_count, 0);
    }

    #[test]
    fn delete_file_rows_does_not_match_prefix_siblings() {
        // Scale-problem pin. The LIKE clause `id LIKE ?1 || '/%'` is
        // designed to delete descendants of `?1` — but at registry
        // scale (50k+ files) prefix-similar names are common. E.g.,
        // "templates" and "templates_dir", or "a.go" and "a.go.bak".
        // A refactor that simplified to `LIKE ?1 || '%'` (dropping
        // the slash) would silently delete every file whose name
        // starts with the same string. Pin via deliberately
        // prefix-similar siblings.
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        create_index_schema(&conn).unwrap();

        // "a" and "ab" — would collide under `LIKE 'a%'` but must NOT
        // collide under `LIKE 'a/%'`.
        insert_node(&conn, "a", "", "a", 1, 0, 0, "").unwrap();
        insert_node(&conn, "a/sub", "a", "sub", 0, 1, 0, "x").unwrap();
        insert_node(&conn, "ab", "", "ab", 1, 0, 0, "").unwrap();
        insert_node(&conn, "ab/sub", "ab", "sub", 0, 1, 0, "y").unwrap();

        // Delete "a" — should remove "a" and "a/sub" only.
        delete_file_rows(&conn, "a").unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE id IN ('ab', 'ab/sub')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 2,
            "prefix-similar `ab` siblings must survive deletion of `a`"
        );
        let a_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE id IN ('a', 'a/sub')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(a_count, 0, "`a` and its descendants must be gone");
    }

    #[test]
    fn ts_schema_creates_all_indexes() {
        // Scale-problem pin completing the index-existence triplet
        // (leyline-schema ✓, leyline-lsp ✓, leyline-ts ←). Five
        // indexes accelerate per-source AST lookup, ref/def token
        // search, and per-source import enumeration. At registry-
        // scale (helm/charts: 4.5k files, 629k _ast rows) idx_ast_
        // source is the difference between O(N) full-scan and O(log
        // N) point lookup per file. A refactor DROP'ing any silently
        // degrades query latency on every populated db.
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();
        create_refs_schema(&conn).unwrap();
        create_index_schema(&conn).unwrap();
        for index_name in [
            "idx_ast_source",
            "idx_refs_token",
            "idx_refs_node",
            "idx_defs_token",
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

        // Build a deeply-nested chain: ""→d0→d0/d1→...→d0/.../d29→file.
        insert_node(&conn, "", "", "", 1, 0, 0, "").unwrap();
        let mut current = String::new();
        for i in 0..30 {
            let parent = current.clone();
            current = if i == 0 {
                format!("d{i}")
            } else {
                format!("{current}/d{i}")
            };
            insert_node(&conn, &current, &parent, &format!("d{i}"), 1, 0, 0, "").unwrap();
        }
        let file_id = format!("{current}/leaf.go");
        insert_node(&conn, &file_id, &current, "leaf.go", 1, 0, 0, "").unwrap();

        // Delete the file — every dir in the chain is now orphaned.
        conn.execute("DELETE FROM nodes WHERE id = ?1", [&file_id])
            .unwrap();

        let removed = sweep_orphaned_dirs(&conn).unwrap();
        assert_eq!(removed, 30, "must sweep all 30 nested dirs");
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1, "only root node should remain");
    }

    #[test]
    fn sweep_orphaned_dirs_removes_empty_parents() {
        let conn = Connection::open_in_memory().unwrap();
        create_ast_schema(&conn).unwrap();

        insert_node(&conn, "", "", "", 1, 0, 0, "").unwrap();
        insert_node(&conn, "src", "", "src", 1, 0, 0, "").unwrap();
        insert_node(&conn, "src/pkg", "src", "pkg", 1, 0, 0, "").unwrap();
        insert_node(&conn, "src/pkg/a.go", "src/pkg", "a.go", 1, 0, 0, "").unwrap();

        conn.execute("DELETE FROM nodes WHERE id = 'src/pkg/a.go'", [])
            .unwrap();

        let removed = sweep_orphaned_dirs(&conn).unwrap();
        assert_eq!(removed, 2, "should remove src/pkg and src");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "only root node should remain");
    }
}
