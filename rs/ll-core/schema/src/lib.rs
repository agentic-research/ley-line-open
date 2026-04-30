//! Canonical `nodes` table schema shared by all ley-line crates.
//!
//! This is the contract: mache writes it, leyline-fs reads it, leyline-ts
//! projects tree-sitter ASTs into it. One definition, no drift.

use anyhow::Result;
use rusqlite::{Connection, params};

/// The `nodes` table DDL — the shared contract across ley-line and mache.
///
/// ```sql
/// CREATE TABLE IF NOT EXISTS nodes (
///     id TEXT PRIMARY KEY,
///     parent_id TEXT,
///     name TEXT NOT NULL,
///     kind INTEGER NOT NULL,   -- 0=file, 1=dir
///     size INTEGER DEFAULT 0,
///     mtime INTEGER NOT NULL,
///     record_id TEXT,          -- optional: FK into results table (mache lazy loading)
///     record JSON,
///     source_file TEXT         -- optional: originating source file path (mache file tracking)
/// );
/// ```
///
/// The `record_id` and `source_file` columns are nullable and default to NULL.
/// They are used by mache's SQLiteGraph for lazy content resolution and
/// incremental re-ingestion tracking. Ley-line crates that don't need these
/// features can ignore them — `insert_node()` leaves them NULL.
pub const NODES_DDL: &str = "\
CREATE TABLE IF NOT EXISTS nodes (
    id TEXT PRIMARY KEY,
    parent_id TEXT,
    name TEXT NOT NULL,
    kind INTEGER NOT NULL,
    size INTEGER DEFAULT 0,
    mtime INTEGER NOT NULL,
    record_id TEXT,
    record JSON,
    source_file TEXT
);
CREATE INDEX IF NOT EXISTS idx_parent_name ON nodes(parent_id, name);
-- Partial index: ley-line's parse paths leave source_file NULL (only
-- mache's lazy-resolution flow populates it). A full index over a NULL-
-- only column adds B-tree pages per row to every registry-repo db
-- without ever serving a query. WHERE source_file IS NOT NULL skips
-- those rows entirely; the index materializes only when mache (or any
-- future caller) actually populates the column.
CREATE INDEX IF NOT EXISTS idx_source_file ON nodes(source_file) WHERE source_file IS NOT NULL;";

/// Create the `nodes` table and index (idempotent).
pub fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(NODES_DDL)?;
    Ok(())
}

/// Insert a single node row.
#[allow(clippy::too_many_arguments)]
pub fn insert_node(
    conn: &Connection,
    id: &str,
    parent_id: &str,
    name: &str,
    kind: i32,
    size: i64,
    mtime: i64,
    record: &str,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO nodes (id, parent_id, name, kind, size, mtime, record) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, parent_id, name, kind, size, mtime, record],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_schema_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        create_schema(&conn).unwrap(); // second call must not fail
    }

    #[test]
    fn insert_and_query() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        insert_node(&conn, "a", "", "a", 1, 0, 1000, "{}").unwrap();

        let (name, kind): (String, i32) = conn
            .query_row("SELECT name, kind FROM nodes WHERE id = 'a'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(name, "a");
        assert_eq!(kind, 1);
    }

    #[test]
    fn duplicate_id_overwrites_on_upsert() {
        // insert_node uses INSERT OR REPLACE so a re-inserted id
        // overwrites the existing row. The ingest pipeline relies on
        // this: parse_into_conn re-runs over the same source dir
        // produce identical rows + INSERT OR REPLACE no-ops them,
        // and a changed file simply rewrites its row in place. Pin
        // both halves: second call succeeds, AND the row reflects
        // the second insert's values.
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        insert_node(&conn, "dup", "", "dup", 1, 0, 100, "").unwrap();
        // Second insert with same id MUST succeed (INSERT OR REPLACE).
        insert_node(&conn, "dup", "", "dup", 1, 99, 200, "updated").unwrap();
        let (size, mtime, record): (i64, i64, String) = conn
            .query_row(
                "SELECT size, mtime, record FROM nodes WHERE id = 'dup'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(size, 99, "second insert's size must replace");
        assert_eq!(mtime, 200, "second insert's mtime must replace");
        assert_eq!(record, "updated", "second insert's record must replace");
    }

    #[test]
    fn create_schema_creates_both_indexes() {
        // Scale-problem pin. The two indexes do real work at scale — on
        // the helm/charts ingest (4.5k YAML files, 629k nodes),
        // idx_parent_name alone is 185 MB and accelerates every parent→
        // children walk. parent_child_index_lookup uses 4 rows where
        // SQLite can full-scan instantly, so a refactor that DROP'd
        // either index from NODES_DDL would still pass that test. Pin
        // existence directly via sqlite_master.
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();

        for index_name in ["idx_parent_name", "idx_source_file"] {
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
    fn idx_source_file_is_partial_on_not_null() {
        // Schema-bloat pin. Ley-line's production parse paths leave
        // source_file NULL (only mache's lazy-resolution flow ever
        // populates it). A full index over a NULL-only column would add
        // B-tree pages per row to every registry-repo db without
        // serving a query. We make idx_source_file a partial index so
        // it materializes only when source_file is actually populated.
        //
        // Pin the partial predicate explicitly — sqlite_master.sql
        // contains the original CREATE INDEX statement verbatim, so a
        // refactor that drops `WHERE source_file IS NOT NULL` would
        // surface here as a substring miss.
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();

        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='index' AND name='idx_source_file'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            sql.contains("WHERE source_file IS NOT NULL"),
            "idx_source_file must be partial (WHERE source_file IS NOT NULL); got: {sql}",
        );
    }

    #[test]
    fn idx_source_file_indexes_only_non_null_rows() {
        // Behavioral pin: insert a mix of NULL and non-NULL source_file
        // rows, query the index via EXPLAIN QUERY PLAN to confirm
        // SQLite uses idx_source_file only for non-NULL lookups. The
        // partial-index optimization relies on this.
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        // Insert rows: 3 with NULL source_file (what insert_node does),
        // 1 with explicit source_file = 'foo.go'.
        insert_node(&conn, "n1", "", "n1", 0, 0, 0, "").unwrap();
        insert_node(&conn, "n2", "", "n2", 0, 0, 0, "").unwrap();
        insert_node(&conn, "n3", "", "n3", 0, 0, 0, "").unwrap();
        conn.execute(
            "INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record, source_file) \
             VALUES ('n4', '', 'n4', 0, 0, 0, '', 'foo.go')",
            [],
        )
        .unwrap();

        // Lookup by non-NULL source_file MUST be able to use the index.
        let plan: String = conn
            .query_row(
                "EXPLAIN QUERY PLAN SELECT id FROM nodes WHERE source_file = 'foo.go'",
                [],
                |r| r.get::<_, String>(3),
            )
            .unwrap();
        assert!(
            plan.contains("idx_source_file"),
            "non-NULL lookup must use partial index; plan: {plan}",
        );

        // Sanity: the matching row is found.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE source_file = 'foo.go'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn parent_child_index_lookup() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        insert_node(&conn, "root", "", "root", 1, 0, 0, "").unwrap();
        insert_node(&conn, "root/a", "root", "a", 0, 10, 1, "").unwrap();
        insert_node(&conn, "root/b", "root", "b", 0, 20, 2, "").unwrap();
        insert_node(&conn, "other/c", "other", "c", 0, 5, 3, "").unwrap();

        // idx_parent_name index should accelerate this query.
        let mut stmt = conn
            .prepare("SELECT name FROM nodes WHERE parent_id = ?1 ORDER BY name")
            .unwrap();
        let children: Vec<String> = stmt
            .query_map(["root"], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(children, vec!["a", "b"]);
    }

    #[test]
    fn json_record_round_trip() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        let json = r#"{"lang":"go","lines":42}"#;
        insert_node(&conn, "f", "", "f", 0, 100, 500, json).unwrap();

        let record: String = conn
            .query_row("SELECT record FROM nodes WHERE id = 'f'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(record, json);
    }

    #[test]
    fn nodes_ddl_constant_matches_create_schema() {
        // Verify the NODES_DDL constant and create_schema() produce identical tables.
        let conn1 = Connection::open_in_memory().unwrap();
        conn1.execute_batch(NODES_DDL).unwrap();

        let conn2 = Connection::open_in_memory().unwrap();
        create_schema(&conn2).unwrap();

        // Both should accept the same insert.
        for conn in [&conn1, &conn2] {
            insert_node(conn, "x", "", "x", 0, 1, 2, "ok").unwrap();
            let name: String = conn
                .query_row("SELECT name FROM nodes WHERE id = 'x'", [], |r| r.get(0))
                .unwrap();
            assert_eq!(name, "x");
        }
    }
}
