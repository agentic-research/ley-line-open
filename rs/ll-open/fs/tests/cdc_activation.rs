#![cfg(feature = "cdc")]

use leyline_fs::activation::{
    ActivationOptions, activate_chunked_content, activate_chunked_content_with_progress,
};
use rusqlite::{Connection, params};
use tempfile::TempDir;

fn projection() -> Connection {
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
    for (id, kind, record) in [
        ("a.rs", 0_i64, "fn a() {}\n"),
        ("empty.rs", 0_i64, ""),
        ("dir", 1_i64, ""),
    ] {
        conn.execute(
            "INSERT INTO nodes
             (id,parent_id,name,kind,size,mtime,record)
             VALUES (?1,'',?1,?2,?3,7,?4)",
            params![id, kind, record.len() as i64, record],
        )
        .unwrap();
    }
    conn
}

#[test]
fn activation_backfills_files_and_is_idempotent() {
    let conn = projection();
    let first = activate_chunked_content(&conn, ActivationOptions { batch_size: 1 }).unwrap();
    assert_eq!(first.eligible_nodes, 2);
    assert_eq!(first.populated_nodes, 2);
    assert_eq!(first.already_fresh_nodes, 0);
    assert_eq!(first.processed_source_bytes, 10);

    let second = activate_chunked_content(&conn, ActivationOptions { batch_size: 1 }).unwrap();
    assert_eq!(second.populated_nodes, 0);
    assert_eq!(second.already_fresh_nodes, 2);
    assert_eq!(second.processed_source_bytes, 0);
    assert_eq!(first.manifest_rows, second.manifest_rows);
    assert_eq!(first.unique_chunk_rows, second.unique_chunk_rows);
}

#[test]
fn activation_rejects_a_database_without_the_nodes_contract() {
    let conn = Connection::open_in_memory().unwrap();
    let error = activate_chunked_content(&conn, ActivationOptions::default()).unwrap_err();
    assert!(
        format!("{error:#}").contains("missing required nodes table"),
        "unexpected error: {error:#}"
    );
    let cdc_tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name LIKE 'content_%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(cdc_tables, 0, "rejected databases must not be mutated");
}

#[test]
fn activation_names_missing_nodes_columns_before_mutating() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE nodes (
            id TEXT PRIMARY KEY,
            kind INTEGER NOT NULL,
            size INTEGER NOT NULL,
            record TEXT
        );",
    )
    .unwrap();
    let error = activate_chunked_content(&conn, ActivationOptions::default()).unwrap_err();
    assert!(
        format!("{error:#}").contains("missing required nodes columns: mtime"),
        "unexpected error: {error:#}"
    );
    let cdc_tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name LIKE 'content_%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(cdc_tables, 0, "rejected databases must not be mutated");
}

#[test]
fn activation_rejects_an_unrepresentable_batch_size_before_mutating() {
    let conn = projection();
    let error = activate_chunked_content(
        &conn,
        ActivationOptions {
            batch_size: usize::MAX,
        },
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("batch_size exceeds SQLite i64"),
        "unexpected error: {error:#}"
    );
    let cdc_tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name LIKE 'content_%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        cdc_tables, 0,
        "invalid options must not mutate the database"
    );
}

#[test]
fn activation_resumes_after_a_per_node_failure() {
    let conn = projection();
    leyline_fs::chunked::create_chunked_content_schema(&conn).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_second BEFORE INSERT ON content_manifest_meta
         WHEN NEW.node_id = 'empty.rs'
         BEGIN SELECT RAISE(ABORT, 'injected activation failure'); END;",
    )
    .unwrap();

    let error = activate_chunked_content(&conn, ActivationOptions { batch_size: 1 }).unwrap_err();
    assert!(
        format!("{error:#}").contains("empty.rs"),
        "failing node must be named: {error:#}"
    );
    assert!(leyline_fs::chunked::has_chunked_content(&conn, "a.rs").unwrap());

    conn.execute_batch("DROP TRIGGER fail_second").unwrap();
    let resumed = activate_chunked_content(&conn, ActivationOptions { batch_size: 1 }).unwrap();
    assert_eq!(resumed.already_fresh_nodes, 1);
    assert_eq!(resumed.populated_nodes, 1);
}

#[test]
fn activation_rebuilds_a_stale_manifest_from_authoritative_record() {
    let conn = projection();
    activate_chunked_content(&conn, ActivationOptions::default()).unwrap();
    conn.execute(
        "UPDATE nodes SET record = 'fn changed() {}', size = 15, mtime = 8
         WHERE id = 'a.rs'",
        [],
    )
    .unwrap();

    let report = activate_chunked_content(&conn, ActivationOptions::default()).unwrap();
    assert_eq!(report.populated_nodes, 1);
    assert_eq!(report.already_fresh_nodes, 1);
    assert_eq!(report.processed_source_bytes, 15);
}

#[test]
fn activation_rejects_a_record_whose_size_witness_is_inconsistent() {
    let conn = projection();
    conn.execute("UPDATE nodes SET size = 999 WHERE id = 'a.rs'", [])
        .unwrap();

    let error = activate_chunked_content(&conn, ActivationOptions::default()).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("a.rs"), "error must name node: {message}");
    assert!(
        message.contains("size 999") && message.contains("10 record bytes"),
        "error must name both conflicting lengths: {message}"
    );
    let witnesses: i64 = conn
        .query_row("SELECT COUNT(*) FROM content_manifest_meta", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        witnesses, 0,
        "inconsistent authoritative metadata must fail before storing the node"
    );
}

#[test]
fn activation_reports_bounded_deterministic_progress() {
    let conn = projection();
    let mut progress = Vec::new();
    let report = activate_chunked_content_with_progress(
        &conn,
        ActivationOptions { batch_size: 1 },
        |update| progress.push(update),
    )
    .unwrap();

    assert_eq!(progress.len(), 2);
    assert_eq!(
        progress
            .iter()
            .map(|update| update.visited_nodes)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(progress.iter().all(|update| update.eligible_nodes == 2));
    let last = progress.last().unwrap();
    assert_eq!(last.populated_nodes, report.populated_nodes);
    assert_eq!(last.already_fresh_nodes, report.already_fresh_nodes);
    assert_eq!(last.processed_source_bytes, report.processed_source_bytes);
}

#[test]
fn activation_keyset_paging_does_not_skip_after_an_earlier_row_is_deleted() {
    let conn = projection();
    let mut pages = 0;
    let report =
        activate_chunked_content_with_progress(&conn, ActivationOptions { batch_size: 1 }, |_| {
            pages += 1;
            if pages == 1 {
                conn.execute("DELETE FROM nodes WHERE id = 'a.rs'", [])
                    .unwrap();
            }
        })
        .unwrap();

    assert_eq!(pages, 2);
    assert_eq!(report.eligible_nodes, 1);
    assert_eq!(report.populated_nodes, 2);
    assert!(
        leyline_fs::chunked::has_chunked_content(&conn, "empty.rs").unwrap(),
        "removing a processed row must not shift the next row behind an OFFSET"
    );
}

#[test]
fn activation_keyset_includes_an_empty_string_node_id() {
    let conn = projection();
    conn.execute(
        "INSERT INTO nodes
         (id,parent_id,name,kind,size,mtime,record)
         VALUES ('','','',0,5,7,'empty')",
        [],
    )
    .unwrap();

    let report = activate_chunked_content(&conn, ActivationOptions { batch_size: 1 }).unwrap();

    assert_eq!(report.eligible_nodes, 3);
    assert_eq!(report.populated_nodes, 3);
    assert!(
        leyline_fs::chunked::has_chunked_content(&conn, "").unwrap(),
        "an empty string is a valid keyset value, not an absent cursor"
    );
}

#[test]
fn activation_converges_when_a_concurrent_insert_sorts_before_the_cursor() {
    let conn = projection();
    let mut pages = 0;
    let report =
        activate_chunked_content_with_progress(&conn, ActivationOptions { batch_size: 1 }, |_| {
            pages += 1;
            if pages == 1 {
                conn.execute(
                    "INSERT INTO nodes
                     (id,parent_id,name,kind,size,mtime,record)
                     VALUES ('0.rs','','0.rs',0,11,8,'fn zero(){}')",
                    [],
                )
                .unwrap();
            }
        })
        .unwrap();

    assert_eq!(report.eligible_nodes, 3);
    assert_eq!(report.populated_nodes, 3);
    assert!(
        leyline_fs::chunked::has_chunked_content(&conn, "0.rs").unwrap(),
        "activation must not report success with an inserted eligible row stale"
    );
}

#[test]
fn stale_caller_bytes_cannot_be_paired_with_a_new_authoritative_witness() {
    let temp = TempDir::new().unwrap();
    let db = temp.path().join("projection.db");
    let reader = Connection::open(&db).unwrap();
    reader
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE nodes (
                id TEXT PRIMARY KEY,
                kind INTEGER NOT NULL,
                size INTEGER NOT NULL,
                mtime INTEGER NOT NULL,
                record TEXT
             );
             INSERT INTO nodes VALUES ('a.rs', 0, 10, 7, 'fn a() {}\n');",
        )
        .unwrap();
    leyline_fs::chunked::create_chunked_content_schema(&reader).unwrap();
    let old_bytes: Vec<u8> = reader
        .query_row(
            "SELECT CAST(record AS BLOB) FROM nodes WHERE id = 'a.rs'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    leyline_fs::chunked::store_content_chunked(&reader, "a.rs", &old_bytes).unwrap();

    let writer = Connection::open(&db).unwrap();
    writer
        .execute(
            "UPDATE nodes
                SET record = 'fn b() {}\n', mtime = 8
              WHERE id = 'a.rs'",
            [],
        )
        .unwrap();

    let error = leyline_fs::chunked::store_content_chunked(&reader, "a.rs", &old_bytes)
        .expect_err("stale caller bytes must not receive the new row witness");
    assert!(
        format!("{error:#}").contains("authoritative node changed"),
        "unexpected error: {error:#}"
    );
    assert!(
        !leyline_fs::chunked::has_chunked_content(&reader, "a.rs").unwrap(),
        "a rejected stale store must never look fresh"
    );
    let preserved_witness: i64 = reader
        .query_row(
            "SELECT source_mtime FROM content_manifest_meta WHERE node_id = 'a.rs'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        preserved_witness, 7,
        "rejection must roll back and preserve the prior manifest generation"
    );
}
