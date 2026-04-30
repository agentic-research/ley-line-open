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
CREATE INDEX IF NOT EXISTS idx_source_file ON nodes(source_file);";

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
        // Scale-problem pin. The two indexes (idx_parent_name and
        // idx_source_file) do real work at scale — on the helm/charts
        // ingest (4.5k YAML files, 629k nodes), idx_parent_name
        // alone is 185 MB and accelerates every parent→children
        // walk. parent_child_index_lookup uses 4 rows where SQLite
        // can full-scan instantly, so a refactor that DROP'd either
        // index from NODES_DDL would still pass that test. Pin
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
