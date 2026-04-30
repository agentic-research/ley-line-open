//! Schema definitions for the AST projection tables.
//!
//! Re-exports the shared `nodes` table from `leyline-schema` and adds
//! AST-specific tables (`_source`, `_ast`) that enable bidirectional splicing.

pub use leyline_schema::{NODES_DDL, create_schema, insert_node};

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

/// DDL for the `_ast` table — maps node IDs to byte ranges in the source.
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

/// Create `nodes`, `_source`, and `_ast` tables (idempotent).
pub fn create_ast_schema(conn: &Connection) -> Result<()> {
    create_schema(conn)?;
    conn.execute_batch(SOURCE_DDL)?;
    conn.execute_batch(AST_DDL)?;
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
    conn.execute(
        "INSERT OR REPLACE INTO _ast (node_id, source_id, node_kind, start_byte, end_byte, \
         start_row, start_col, end_row, end_col) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            node_id, source_id, node_kind, start_byte, end_byte, start_row, start_col, end_row,
            end_col
        ],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Refs / Defs / Imports tables
// ---------------------------------------------------------------------------

/// DDL for the `node_refs` table — stores identifier references.
pub const REFS_DDL: &str = "\
CREATE TABLE IF NOT EXISTS node_refs (
    token TEXT NOT NULL,
    node_id TEXT NOT NULL,
    source_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_refs_token ON node_refs(token);
CREATE INDEX IF NOT EXISTS idx_refs_node ON node_refs(node_id);";

/// DDL for the `node_defs` table — stores identifier definitions.
pub const DEFS_DDL: &str = "\
CREATE TABLE IF NOT EXISTS node_defs (
    token TEXT NOT NULL,
    node_id TEXT NOT NULL,
    source_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_defs_token ON node_defs(token);";

/// DDL for the `_imports` table — stores import/require mappings.
pub const IMPORTS_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _imports (
    alias TEXT NOT NULL,
    path TEXT NOT NULL,
    source_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_imports_source ON _imports(source_id);";

/// Create `node_refs`, `node_defs`, and `_imports` tables (idempotent).
pub fn create_refs_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(REFS_DDL)?;
    conn.execute_batch(DEFS_DDL)?;
    conn.execute_batch(IMPORTS_DDL)?;
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

/// Create `_file_index` and `_meta` tables (idempotent).
pub fn create_index_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(FILE_INDEX_DDL)?;
    conn.execute_batch(META_DDL)?;
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
        Ok((row.get::<_, String>(0)?, (row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)))
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
    match conn.query_row(
        "SELECT value FROM _meta WHERE key = ?1",
        [key],
        |row| row.get::<_, String>(0),
    ) {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Delete all rows for a source file across ALL tables.
///
/// The `nodes` table uses path-prefix deletion because node IDs are structured
/// as `<file>/<ast_path>` (e.g. `main.go/function_declaration_0/identifier`).
pub fn delete_file_rows(conn: &Connection, path: &str) -> Result<()> {
    conn.execute("DELETE FROM nodes WHERE id = ?1 OR id LIKE ?1 || '/%'", [path])?;
    conn.execute("DELETE FROM _ast WHERE source_id = ?1", [path])?;
    conn.execute("DELETE FROM _source WHERE id = ?1", [path])?;
    conn.execute("DELETE FROM node_refs WHERE source_id = ?1", [path])?;
    conn.execute("DELETE FROM node_defs WHERE source_id = ?1", [path])?;
    conn.execute("DELETE FROM _imports WHERE source_id = ?1", [path])?;
    conn.execute("DELETE FROM _file_index WHERE path = ?1", [path])?;
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
        if removed == 0 { break; }
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

        let ref_count: i64 = conn.query_row("SELECT COUNT(*) FROM node_refs", [], |r| r.get(0)).unwrap();
        assert_eq!(ref_count, 1);
        let def_count: i64 = conn.query_row("SELECT COUNT(*) FROM node_defs", [], |r| r.get(0)).unwrap();
        assert_eq!(def_count, 1);
        let import_count: i64 = conn.query_row("SELECT COUNT(*) FROM _imports", [], |r| r.get(0)).unwrap();
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
        let val: String = conn.query_row(
            "SELECT value FROM _meta WHERE key = 'source_root'", [], |r| r.get(0),
        ).unwrap();
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
                [], |r| r.get(0),
            )
            .unwrap();
        assert_eq!(val, "12", "third write must win");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _meta WHERE key = 'tree-sitter_version'",
                [], |r| r.get(0),
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
        let a_nodes: i64 = conn.query_row("SELECT COUNT(*) FROM nodes WHERE id = 'a.go' OR id LIKE 'a.go/%'", [], |r| r.get(0)).unwrap();
        assert_eq!(a_nodes, 0);
        let a_source: i64 = conn.query_row("SELECT COUNT(*) FROM _source WHERE id = 'a.go'", [], |r| r.get(0)).unwrap();
        assert_eq!(a_source, 0);
        let a_refs: i64 = conn.query_row("SELECT COUNT(*) FROM node_refs WHERE source_id = 'a.go'", [], |r| r.get(0)).unwrap();
        assert_eq!(a_refs, 0);
        let a_index: i64 = conn.query_row("SELECT COUNT(*) FROM _file_index WHERE path = 'a.go'", [], |r| r.get(0)).unwrap();
        assert_eq!(a_index, 0);

        // b.go intact
        let b_nodes: i64 = conn.query_row("SELECT COUNT(*) FROM nodes WHERE id = 'b.go' OR id LIKE 'b.go/%'", [], |r| r.get(0)).unwrap();
        assert!(b_nodes >= 2);
        let b_refs: i64 = conn.query_row("SELECT COUNT(*) FROM node_refs WHERE source_id = 'b.go'", [], |r| r.get(0)).unwrap();
        assert_eq!(b_refs, 1);
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
            .query_row("SELECT COUNT(*) FROM nodes WHERE id IN ('ab', 'ab/sub')", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2, "prefix-similar `ab` siblings must survive deletion of `a`");
        let a_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes WHERE id IN ('a', 'a/sub')", [], |r| r.get(0))
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
            upsert_file_index(&conn, &format!("path/{i:04}.go"), i as i64, (i * 7) as i64)
                .unwrap();
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
            current = if i == 0 { format!("d{i}") } else { format!("{current}/d{i}") };
            insert_node(&conn, &current, &parent, &format!("d{i}"), 1, 0, 0, "").unwrap();
        }
        let file_id = format!("{current}/leaf.go");
        insert_node(&conn, &file_id, &current, "leaf.go", 1, 0, 0, "").unwrap();

        // Delete the file — every dir in the chain is now orphaned.
        conn.execute("DELETE FROM nodes WHERE id = ?1", [&file_id]).unwrap();

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

        conn.execute("DELETE FROM nodes WHERE id = 'src/pkg/a.go'", []).unwrap();

        let removed = sweep_orphaned_dirs(&conn).unwrap();
        assert_eq!(removed, 2, "should remove src/pkg and src");

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 1, "only root node should remain");
    }
}
