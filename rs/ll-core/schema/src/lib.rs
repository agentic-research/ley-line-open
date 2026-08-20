//! Canonical `nodes` table schema shared by all ley-line crates.
//!
//! This is the contract: mache writes it, leyline-fs reads it, leyline-ts
//! projects tree-sitter ASTs into it. One definition, no drift.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

/// The `nodes` table DDL (table only, no indexes) — the shared contract
/// across ley-line and mache.
///
/// ```sql
/// CREATE TABLE IF NOT EXISTS nodes (
///     id TEXT PRIMARY KEY,
///     parent_id TEXT GENERATED ALWAYS AS (...) VIRTUAL,  -- derived from id+name
///     name TEXT NOT NULL,
///     kind INTEGER NOT NULL,   -- 0=file, 1=dir
///     size INTEGER DEFAULT 0,
///     mtime INTEGER NOT NULL,
///     record_id TEXT,          -- optional: FK into results table (mache lazy loading)
///     record TEXT,             -- node content. TEXT, not JSON: see below.
///     source_file TEXT         -- optional: originating source file path (mache file tracking)
/// );
/// ```
///
/// # `record` is `TEXT`, deliberately — do not "restore" `JSON`
///
/// It was declared `JSON` until 2026-07-27. SQLite assigns column affinity by
/// substring match: a declared type containing none of INT, CHAR, CLOB, TEXT,
/// BLOB, REAL, FLOA, or DOUB gets **NUMERIC** affinity. `JSON` contains none
/// of them, so every value that looked like a number was silently coerced:
///
/// ```text
/// '007'   -> 7      typeof=integer   length 3 -> 1
/// '1. '   -> 1      typeof=integer   length 3 -> 1
/// '3.14'  -> 3.14   typeof=real
/// 'hello' -> 'hello' typeof=text
/// ```
///
/// Measured across three real projections: ~9,400 leaves per corpus stored
/// with the wrong SQLite type and ~800-1,100 with the wrong BYTE LENGTH. That
/// is silent corruption in the cross-runtime contract — "mache writes it,
/// leyline-fs reads it" — and it also aborted `leyline cdc enable` on every
/// real corpus, because activation's `size == record.len()` guard correctly
/// failed closed on damage the DDL had caused upstream.
///
/// `record` is not JSON in any case. Producers write a node's CONTENT: an AST
/// leaf token from `leyline-ts`, raw file bytes from `leyline-fs`'s
/// `write_content`, a PEP 508 version specifier from `leyline-ts`'s pyproject
/// projection. `TEXT` stores every one of them as given. What the column's
/// contract SHOULD be is bead `ley-line-open-edec5b`; this only stops the
/// corruption in the meantime.
///
/// Bead `ley-line-open-f7966d`.
///
/// The `record_id` and `source_file` columns are nullable and default to NULL.
/// They are used by mache's SQLiteGraph for lazy content resolution and
/// incremental re-ingestion tracking. Ley-line crates that don't need these
/// features can ignore them — `insert_node()` leaves them NULL.
/// The one definition of how a node's parent is derived from its own row.
///
/// A macro rather than a `const` so it can be spliced into the DDL string
/// literals with `concat!`. Three copies of this expression would otherwise
/// exist — `NODES_TABLE_DDL`, `NODES_DDL`, and the legacy-arena migration —
/// and a drifted copy does not fail loudly: it silently rebinds every parent
/// edge in the graph.
macro_rules! parent_id_derivation {
    () => {
        "CASE WHEN length(id) > length(name) \
             THEN substr(id, 1, length(id) - length(name) - 1) \
             ELSE '' END"
    };
}

/// The derivation expression as a runtime string, for the legacy-arena
/// migration and for the test that pins it against the DDL.
pub const PARENT_ID_DERIVATION: &str = parent_id_derivation!();

/// The column body of the `nodes` table — everything between the parentheses.
///
/// Separate from the `CREATE TABLE` wrapper so the legacy-arena migration can
/// build its replacement table from the SAME text. It must: SQLite appends an
/// `ALTER TABLE ADD COLUMN` at the END of the column order, so a
/// drop-and-re-add migration leaves `parent_id` last while a fresh table has
/// it second — and `SELECT *` would then hand a consumer its columns in an
/// order that depends on the arena's history.
macro_rules! nodes_columns {
    () => {
        concat!(
            "
    id TEXT PRIMARY KEY,
    -- DERIVED, not stored (bead `ley-line-open-17c271`). A node's parent is
    -- the prefix of its own id with the trailing `/<name>` removed, and both
    -- `id` and `name` are already on this row, so storing it was a third copy
    -- of what the row can compute. Measured on a 3 150 850-node arena: the
    -- table goes 1 577 MB -> 914 MB, a saving of 663 MB (-42%). Verified
    -- against the stored column on that same arena first: 3 150 850 of
    -- 3 150 850 rows agree, 0 mismatches.
    --
    -- VIRTUAL, so it costs no bytes IN THE TABLE. `idx_parent_name` still
    -- materializes the computed value — measured unchanged at 731 MB — which
    -- is precisely why lookups did not get slower: `WHERE parent_id = ?`
    -- still plans as `SEARCH nodes USING INDEX idx_parent_name (parent_id=?)`.
    -- Every read consumer — LLO's mount path, mache's ListChildren, the C
    -- FFI, the daemon's `Node.parentId` — is untouched, including `SELECT *`,
    -- which still returns this column in this position.
    --
    -- WRITERS are not untouched: naming `parent_id` in an INSERT or UPDATE is
    -- now an error at prepare time. That is the point — a caller could
    -- previously store a parent inconsistent with the id it was inserting.
    --
    -- The flip side is that `id = name` (root) or `id` ending in `/` || `name`
    -- is now LOAD-BEARING where it was merely true before. Every writer
    -- maintains it by construction (`create_node` builds its id as the parent,
    -- a slash, then the name), but a row that violates it gets the wrong
    -- parent silently rather than the one it stored.
    --
    -- Root-level rows yield '' (the empty string), not NULL; the CASE
    -- preserves that convention exactly.
    parent_id TEXT GENERATED ALWAYS AS (",
            parent_id_derivation!(),
            ") VIRTUAL,
    name TEXT NOT NULL,
    kind INTEGER NOT NULL,
    size INTEGER DEFAULT 0,
    mtime INTEGER NOT NULL,
    record_id TEXT,
    record TEXT,
    source_file TEXT
"
        )
    };
}

/// The `nodes` table DDL (table only, no indexes).
pub const NODES_TABLE_DDL: &str =
    concat!("CREATE TABLE IF NOT EXISTS nodes (", nodes_columns!(), ");");

/// The column body alone, for the migration that rebuilds the table.
pub const NODES_COLUMNS: &str = nodes_columns!();

/// Every column the table actually STORES, in declaration order — i.e.
/// `NODES_COLUMNS` minus the derived one. This is what the migration copies,
/// and the order `SELECT *` returns.
pub const NODES_STORED_COLUMNS: &[&str] = &[
    "id",
    "name",
    "kind",
    "size",
    "mtime",
    "record_id",
    "record",
    "source_file",
];

/// The `nodes` table indexes (no table) — deferred post-load to avoid
/// paying B-tree maintenance per INSERT during bulk parse.
///
/// `idx_source_file` is partial: ley-line's parse paths leave `source_file`
/// NULL (only mache's lazy-resolution flow populates it). A full index
/// over a NULL-only column would add B-tree pages per row to every
/// registry-repo db without ever serving a query. `WHERE source_file IS
/// NOT NULL` skips those rows entirely; the index materializes only when
/// mache (or any future caller) actually populates the column.
pub const NODES_INDEXES_DDL: &str = "\
CREATE INDEX IF NOT EXISTS idx_parent_name ON nodes(parent_id, name);
CREATE INDEX IF NOT EXISTS idx_source_file ON nodes(source_file) WHERE source_file IS NOT NULL;";

/// Combined `nodes` table + index DDL. Preserves the pre-split contract for
/// callers that want the schema fully materialized in one batch.
/// `cmd_parse` instead calls `NODES_TABLE_DDL` (insert phase) and
/// `NODES_INDEXES_DDL` (post-COMMIT) separately — see bead
/// `ley-line-open-9ccbc7`.
pub const NODES_DDL: &str = concat!(
    "CREATE TABLE IF NOT EXISTS nodes (",
    nodes_columns!(),
    ");
CREATE INDEX IF NOT EXISTS idx_parent_name ON nodes(parent_id, name);
-- Partial index: see `NODES_INDEXES_DDL` for rationale.
CREATE INDEX IF NOT EXISTS idx_source_file ON nodes(source_file) WHERE source_file IS NOT NULL;"
);

/// Create the `nodes` table and indexes (idempotent).
///
/// For bulk-load callers (e.g. `cmd_parse`), prefer the split
/// [`create_nodes_table`] + [`create_nodes_indexes`] pair so the
/// indexes can be deferred until after `COMMIT`.
pub fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(NODES_DDL)?;
    // No-op on a fresh table; converts a legacy arena's stored column.
    migrate_parent_id_to_generated(conn)?;
    Ok(())
}

/// Create only the `nodes` table — no indexes. Pair with
/// [`create_nodes_indexes`] (called post-`COMMIT`) for bulk-load paths
/// where index maintenance during INSERT dominates the wall clock.
pub fn create_nodes_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(NODES_TABLE_DDL)?;
    // No-op on a fresh table; converts a legacy arena's stored column.
    migrate_parent_id_to_generated(conn)?;
    Ok(())
}

/// Create only the `nodes` indexes — no table. Idempotent (`IF NOT
/// EXISTS`), so safe to call on a connection that already has them.
pub fn create_nodes_indexes(conn: &Connection) -> Result<()> {
    conn.execute_batch(NODES_INDEXES_DDL)?;
    Ok(())
}

/// How `parent_id` exists on an arena's `nodes` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentIdShape {
    /// No `nodes` table, or no `parent_id` column on it.
    Absent,
    /// A legacy arena: a real column holding a second copy of the parent path.
    Stored,
    /// Current: derived from `id` and `name` on read, occupying no bytes.
    Generated,
}

/// Report how `parent_id` exists on `conn`'s `nodes` table.
///
/// Deliberately `pragma_table_xinfo` and NOT `pragma_table_info`: the latter
/// omits VIRTUAL generated columns entirely, so it reports a migrated arena as
/// having no `parent_id` at all. Any probe that reaches for `table_info` here
/// concludes the column is missing and re-runs the migration forever.
pub fn parent_id_shape(conn: &Connection) -> Result<ParentIdShape> {
    let hidden: Option<i64> = conn
        .query_row(
            "SELECT hidden FROM pragma_table_xinfo('nodes') WHERE name = 'parent_id'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(match hidden {
        None => ParentIdShape::Absent,
        // 0 = ordinary column; 2 = VIRTUAL generated; 3 = STORED generated.
        Some(2 | 3) => ParentIdShape::Generated,
        Some(_) => ParentIdShape::Stored,
    })
}

/// Convert a legacy arena's stored `parent_id` into the derived column.
/// Returns whether it migrated anything.
///
/// `CREATE TABLE IF NOT EXISTS` is a no-op against a table that already
/// exists, so without this an arena written before this change keeps its
/// stored column — and every INSERT, which no longer names `parent_id`,
/// leaves it NULL. Directory listing is `WHERE parent_id = ?`, so such an
/// arena would go quietly EMPTY rather than fail loudly.
///
/// Refuses rather than migrates if any row's stored parent disagrees with the
/// derivation. The change rests on `id` being `parent_id || '/' || name`,
/// which every writer maintains by construction; an arena where that does not
/// hold is one where dropping the column silently rebinds parent edges, and a
/// projection is cheaper to rebuild than to debug.
pub fn migrate_parent_id_to_generated(conn: &Connection) -> Result<bool> {
    if parent_id_shape(conn)? != ParentIdShape::Stored {
        return Ok(false);
    }

    // COALESCE so a root row stored as NULL rather than '' is not counted as a
    // rebinding; `IS NOT` (not `<>`) so NULLs compare rather than propagate.
    let mismatches: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM nodes \
             WHERE COALESCE(parent_id, '') IS NOT ({PARENT_ID_DERIVATION})"
        ),
        [],
        |r| r.get(0),
    )?;
    if mismatches > 0 {
        anyhow::bail!(
            "refusing to migrate `nodes.parent_id`: {mismatches} row(s) store a \
             parent that is not the prefix of their own id, so dropping the \
             column would rebind those edges. Re-parse the repository to \
             rebuild the projection instead."
        );
    }

    // Rebuild rather than ALTER. `ALTER TABLE ... ADD COLUMN` appends, so a
    // drop-and-re-add would leave `parent_id` as the LAST column while a fresh
    // arena has it second — `SELECT *` would then return columns in an order
    // that depends on whether the arena was migrated or built from scratch.
    // Renaming the old table out of the way and creating `nodes` from the
    // shipped DDL makes a migrated arena indistinguishable from a fresh one,
    // down to the `sqlite_master.sql` text that consumers pin.
    //
    // Only the columns BOTH tables store are carried: an arena old enough to
    // predate `record_id` or `source_file` still migrates.
    let legacy: std::collections::HashSet<String> = {
        let mut stmt =
            conn.prepare("SELECT name FROM pragma_table_xinfo('nodes') WHERE hidden = 0")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    let carried = NODES_STORED_COLUMNS
        .iter()
        .filter(|c| legacy.contains(**c))
        .copied()
        .collect::<Vec<_>>()
        .join(", ");

    // A SAVEPOINT so the rebuild is atomic whether or not the caller already
    // holds a transaction: a crash between DROP and RENAME would otherwise
    // leave an arena with no `nodes` table at all.
    conn.execute_batch(&format!(
        "SAVEPOINT migrate_parent_id;
         DROP TABLE IF EXISTS nodes_pre_generated_parent;
         DROP INDEX IF EXISTS idx_parent_name;
         ALTER TABLE nodes RENAME TO nodes_pre_generated_parent;
         CREATE TABLE IF NOT EXISTS nodes ({NODES_COLUMNS});
         INSERT INTO nodes ({carried}) SELECT {carried} FROM nodes_pre_generated_parent;
         DROP TABLE nodes_pre_generated_parent;
         RELEASE migrate_parent_id;"
    ))?;
    conn.execute_batch(NODES_INDEXES_DDL)?;
    Ok(true)
}

/// Insert a single node row.
#[allow(clippy::too_many_arguments)]
pub fn insert_node(
    conn: &Connection,
    id: &str,
    name: &str,
    kind: i32,
    size: i64,
    mtime: i64,
    record: &str,
) -> Result<()> {
    // `parent_id` is no longer a parameter: it is a GENERATED column derived
    // from `id` and `name`, so SQLite rejects an INSERT that names it. Dropping
    // it from the signature also removes a way to get it WRONG — a caller could
    // previously pass a parent inconsistent with the id it was inserting
    // (bead `ley-line-open-17c271`).
    conn.execute(
        "INSERT OR REPLACE INTO nodes (id, name, kind, size, mtime, record) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, name, kind, size, mtime, record],
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
        insert_node(&conn, "a", "a", 1, 0, 1000, "{}").unwrap();

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
        insert_node(&conn, "dup", "dup", 1, 0, 100, "").unwrap();
        // Second insert with same id MUST succeed (INSERT OR REPLACE).
        insert_node(&conn, "dup", "dup", 1, 99, 200, "updated").unwrap();
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
        insert_node(&conn, "n1", "n1", 0, 0, 0, "").unwrap();
        insert_node(&conn, "n2", "n2", 0, 0, 0, "").unwrap();
        insert_node(&conn, "n3", "n3", 0, 0, 0, "").unwrap();
        conn.execute(
            "INSERT INTO nodes (id, name, kind, size, mtime, record, source_file) VALUES ('n4', 'n4', 0, 0, 0, '', 'foo.go')",
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
        insert_node(&conn, "root", "root", 1, 0, 0, "").unwrap();
        insert_node(&conn, "root/a", "a", 0, 10, 1, "").unwrap();
        insert_node(&conn, "root/b", "b", 0, 20, 2, "").unwrap();
        insert_node(&conn, "other/c", "c", 0, 5, 3, "").unwrap();

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
        insert_node(&conn, "f", "f", 0, 100, 500, json).unwrap();

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
            insert_node(conn, "x", "x", 0, 1, 2, "ok").unwrap();
            let name: String = conn
                .query_row("SELECT name FROM nodes WHERE id = 'x'", [], |r| r.get(0))
                .unwrap();
            assert_eq!(name, "x");
        }
    }

    /// The exact `nodes` table an arena written before the derived column
    /// carries: `parent_id` as an ordinary stored column, plus the index that
    /// `DROP COLUMN` will refuse to step over.
    const LEGACY_NODES_DDL: &str = "\
CREATE TABLE nodes (
    id TEXT PRIMARY KEY,
    parent_id TEXT,
    name TEXT NOT NULL,
    kind INTEGER NOT NULL,
    size INTEGER DEFAULT 0,
    mtime INTEGER NOT NULL,
    record_id TEXT,
    record TEXT,
    source_file TEXT
);
CREATE INDEX idx_parent_name ON nodes(parent_id, name);";

    fn legacy_arena() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(LEGACY_NODES_DDL).unwrap();
        conn.execute_batch(
            "INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES
               ('src',            '',        'src',    1, 0, 1, NULL),
               ('src/a.go',       'src',     'a.go',   0, 9, 2, 'package a'),
               ('src/deep',       'src',     'deep',   1, 0, 3, NULL),
               ('src/deep/b.go',  'src/deep','b.go',   0, 9, 4, 'package b');",
        )
        .unwrap();
        conn
    }

    /// `pragma_table_info` OMITS a VIRTUAL generated column. A probe built on
    /// it reports a migrated arena as having no `parent_id`, which would make
    /// the migration re-run forever and any `has_column`-style guard lie.
    #[test]
    fn parent_id_shape_sees_what_table_info_cannot() {
        let legacy = legacy_arena();
        assert_eq!(parent_id_shape(&legacy).unwrap(), ParentIdShape::Stored);

        let fresh = Connection::open_in_memory().unwrap();
        create_schema(&fresh).unwrap();
        assert_eq!(parent_id_shape(&fresh).unwrap(), ParentIdShape::Generated);

        let visible_to_table_info: i64 = fresh
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('nodes') WHERE name = 'parent_id'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            visible_to_table_info, 0,
            "pinning the blind spot: table_info does NOT list the derived \
             column, which is why parent_id_shape must use table_xinfo"
        );

        let empty = Connection::open_in_memory().unwrap();
        assert_eq!(parent_id_shape(&empty).unwrap(), ParentIdShape::Absent);
    }

    /// A legacy arena must come out the far side holding exactly the parents
    /// it held going in — and still answering `WHERE parent_id = ?` from the
    /// index, which is the query every directory listing makes.
    #[test]
    fn legacy_arena_migrates_without_moving_a_single_parent() {
        let conn = legacy_arena();
        let before: Vec<(String, String)> = conn
            .prepare("SELECT id, parent_id FROM nodes ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert!(
            migrate_parent_id_to_generated(&conn).unwrap(),
            "a stored column is exactly what this migrates"
        );
        assert_eq!(parent_id_shape(&conn).unwrap(), ParentIdShape::Generated);

        let after: Vec<(String, String)> = conn
            .prepare("SELECT id, parent_id FROM nodes ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            before, after,
            "the derived column must reproduce the stored one byte for byte; \
             a difference here is every parent edge in the graph rebinding"
        );

        let plan: String = conn
            .query_row(
                "EXPLAIN QUERY PLAN SELECT id FROM nodes WHERE parent_id = 'src'",
                [],
                |r| r.get(3),
            )
            .unwrap();
        assert!(
            plan.contains("idx_parent_name"),
            "directory listing must still be an index seek, not a scan; got: {plan}"
        );
    }

    /// The migration is destructive, so it verifies before it drops. An arena
    /// whose stored parent is not the prefix of its own id is one where the
    /// derivation would MOVE that node; refusing keeps a rebuildable
    /// projection rebuildable instead of silently rewiring it.
    #[test]
    fn migration_refuses_an_arena_whose_parents_disagree() {
        let conn = legacy_arena();
        conn.execute(
            "INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) \
             VALUES ('src/c.go', 'somewhere/else', 'c.go', 0, 1, 5, NULL)",
            [],
        )
        .unwrap();

        let err = migrate_parent_id_to_generated(&conn)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("refusing to migrate"),
            "must refuse, not silently rebind; got: {err}"
        );
        assert_eq!(
            parent_id_shape(&conn).unwrap(),
            ParentIdShape::Stored,
            "a refused migration must leave the arena untouched"
        );
    }

    /// A root row written as NULL rather than '' means the same thing and must
    /// not be read as a disagreement.
    #[test]
    fn migration_accepts_a_null_root_parent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(LEGACY_NODES_DDL).unwrap();
        conn.execute_batch(
            "INSERT INTO nodes (id, parent_id, name, kind, size, mtime) \
             VALUES ('src', NULL, 'src', 1, 0, 1);",
        )
        .unwrap();
        assert!(migrate_parent_id_to_generated(&conn).unwrap());
        let parent: String = conn
            .query_row("SELECT parent_id FROM nodes WHERE id = 'src'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(parent, "", "a root derives to '' — the stored convention");
    }

    /// `create_nodes_table` runs on every parse, including reparses of an
    /// arena that has already been migrated.
    #[test]
    fn migration_is_idempotent_and_runs_from_the_schema_entry_points() {
        let conn = legacy_arena();
        create_nodes_table(&conn).unwrap();
        assert_eq!(parent_id_shape(&conn).unwrap(), ParentIdShape::Generated);

        assert!(
            !migrate_parent_id_to_generated(&conn).unwrap(),
            "second call has nothing to do"
        );
        create_nodes_table(&conn).unwrap();
        create_schema(&conn).unwrap();
        assert_eq!(parent_id_shape(&conn).unwrap(), ParentIdShape::Generated);

        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE parent_id = 'src'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2, "rows survive repeated schema application");
    }

    /// A migrated arena and a fresh one must be the same table. If the
    /// migration's copy of the derivation drifts from the DDL's, two arenas
    /// over identical source disagree about what a node's parent is — and
    /// nothing else in the suite would notice.
    #[test]
    fn migrated_and_fresh_arenas_derive_identical_parents() {
        let migrated = legacy_arena();
        migrate_parent_id_to_generated(&migrated).unwrap();

        let fresh = Connection::open_in_memory().unwrap();
        create_schema(&fresh).unwrap();
        for (id, name) in [
            ("src", "src"),
            ("src/a.go", "a.go"),
            ("src/deep", "deep"),
            ("src/deep/b.go", "b.go"),
        ] {
            insert_node(&fresh, id, name, 0, 0, 1, "").unwrap();
        }

        let read = |c: &Connection| -> Vec<(String, String)> {
            c.prepare("SELECT id, parent_id FROM nodes ORDER BY id")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(
            read(&migrated),
            read(&fresh),
            "the migration and the DDL must share one derivation"
        );
    }

    /// The DDL is built by splicing `parent_id_derivation!()`, so this pins
    /// that the exported runtime string is the same text the table actually
    /// carries — the migration formats its ALTER from the exported one.
    #[test]
    fn exported_derivation_is_the_one_the_ddl_uses() {
        assert!(
            NODES_TABLE_DDL.contains(PARENT_ID_DERIVATION),
            "PARENT_ID_DERIVATION must be verbatim in the table DDL"
        );
        assert!(NODES_DDL.contains(PARENT_ID_DERIVATION));
    }

    /// Naming `parent_id` in an INSERT is now an error, not a silent write.
    /// This is the failure every one of the 58 rewritten call sites would
    /// otherwise have hit at runtime, invisible to `cargo check`.
    #[test]
    fn inserting_into_the_derived_column_is_rejected() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        let err = conn
            .execute(
                "INSERT INTO nodes (id, parent_id, name, kind, mtime) \
                 VALUES ('a/b', 'a', 'b', 0, 1)",
                [],
            )
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("cannot INSERT into generated column"),
            "got: {err}"
        );
    }

    fn table_sql(conn: &Connection) -> String {
        conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='nodes'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn column_order(conn: &Connection) -> Vec<String> {
        conn.prepare("SELECT name FROM pragma_table_xinfo('nodes') ORDER BY cid")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    /// A migrated arena must be indistinguishable from a fresh one.
    ///
    /// `ALTER TABLE ... ADD COLUMN` appends, so the obvious migration —
    /// drop the column, add it back generated — leaves `parent_id` LAST while
    /// a fresh table has it second. `SELECT *` returns declaration order, so
    /// that alone would hand positional consumers different columns depending
    /// on the arena's history, with nothing failing to say so. It also moves
    /// the `sqlite_master.sql` text that mache pins byte-for-byte.
    #[test]
    fn a_migrated_arena_is_indistinguishable_from_a_fresh_one() {
        let migrated = legacy_arena();
        migrate_parent_id_to_generated(&migrated).unwrap();

        let fresh = Connection::open_in_memory().unwrap();
        create_schema(&fresh).unwrap();

        assert_eq!(
            column_order(&migrated),
            column_order(&fresh),
            "column ORDER must match — `SELECT *` is positional for some consumers"
        );
        assert_eq!(
            column_order(&fresh)[1],
            "parent_id",
            "parent_id belongs in its declared position, not appended at the end"
        );
        assert_eq!(
            table_sql(&migrated),
            table_sql(&fresh),
            "the stored DDL text must match — consumers pin it byte-for-byte"
        );
    }

    /// An arena old enough to predate `record_id` / `source_file` must still
    /// migrate: the rebuild copies the columns both tables share, not a fixed
    /// list that assumes today's shape.
    #[test]
    fn an_arena_missing_later_columns_still_migrates() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE nodes (
                id TEXT PRIMARY KEY,
                parent_id TEXT,
                name TEXT NOT NULL,
                kind INTEGER NOT NULL,
                size INTEGER DEFAULT 0,
                mtime INTEGER NOT NULL,
                record TEXT
            );",
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) \
             VALUES ('src', '', 'src', 1, 0, 1, NULL), \
                    ('src/a.go', 'src', 'a.go', 0, 9, 2, 'package a');",
        )
        .unwrap();

        assert!(migrate_parent_id_to_generated(&conn).unwrap());
        assert_eq!(parent_id_shape(&conn).unwrap(), ParentIdShape::Generated);

        let parent: String = conn
            .query_row(
                "SELECT parent_id FROM nodes WHERE id = 'src/a.go'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(parent, "src");

        let record: Option<String> = conn
            .query_row("SELECT record FROM nodes WHERE id = 'src/a.go'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            record.as_deref(),
            Some("package a"),
            "shared columns must survive the rebuild"
        );
        // The columns the legacy table lacked exist now, holding NULL.
        let src: Option<String> = conn
            .query_row("SELECT source_file FROM nodes WHERE id = 'src'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(src.is_none());
    }

    /// A half-finished earlier attempt must not wedge the next one.
    #[test]
    fn migration_recovers_from_a_leftover_scratch_table() {
        let conn = legacy_arena();
        conn.execute_batch("CREATE TABLE nodes_pre_generated_parent (id TEXT);")
            .unwrap();
        assert!(migrate_parent_id_to_generated(&conn).unwrap());
        assert_eq!(parent_id_shape(&conn).unwrap(), ParentIdShape::Generated);
        let leftover: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'nodes_pre_generated_parent'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(leftover, 0, "the scratch table must not survive");
    }
}
