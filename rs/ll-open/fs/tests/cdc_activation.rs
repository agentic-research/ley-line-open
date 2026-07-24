#![cfg(feature = "cdc")]

use leyline_fs::activation::{
    ActivationOptions, activate_chunked_content, activate_chunked_content_with_progress,
};
use rusqlite::{Connection, params};

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
