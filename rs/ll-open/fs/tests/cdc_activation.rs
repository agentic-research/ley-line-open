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

// ---------------------------------------------------------------------------
// The canonical `nodes` contract — bead `ley-line-open-b5faa9`
//
// Every test above builds its fixture from a hand-written `record TEXT`
// column. The contract every real producer writes — `leyline_schema::
// NODES_TABLE_DDL`, "mache writes it, leyline-fs reads it" — declares
// `record JSON`. SQLite's affinity rules give a declared type containing none
// of INT/CHAR/CLOB/TEXT/BLOB/REAL/FLOA/DOUB **NUMERIC** affinity, so `JSON`
// silently coerces any leaf token that parses as a number out of TEXT on
// insert. The fixture drift is why activation was never exercised against the
// shape it ships against.
//
// Measured on three real projections (`leyline parse` at 2026-07-27):
//
// | corpus | eligible leaves | typeof=integer | typeof=real | size mismatch |
// |--------|-----------------|----------------|-------------|---------------|
// | mache  |         391,556 |          9,332 |         113 |         1,143 |
// | rosary |         208,195 |          4,572 |         164 |           807 |
// | LLO    |         254,472 |          9,669 |         166 |           801 |
//
// Most coercions round-trip textually ("42" -> 42 -> "42"). The mismatch
// column is the subset whose BYTE LENGTH changed ("1. " -> 1, "007" -> 7),
// and activation's `size == record.len()` guard fails closed on the first one
// it meets — so `leyline cdc enable` cannot complete on any of the three.
// ---------------------------------------------------------------------------

/// The same fixture shape as [`projection`], built from the CANONICAL contract
/// DDL rather than a hand-written `record TEXT` column.
fn canonical_projection() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    leyline_schema::create_nodes_table(&conn).unwrap();
    conn
}

fn insert_leaf(conn: &Connection, id: &str, record: &str) {
    conn.execute(
        "INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) \
         VALUES (?1, '', ?1, 0, ?2, 7, ?3)",
        params![id, record.len() as i64, record],
    )
    .unwrap();
}

/// What the contract is supposed to guarantee, held here as executable
/// evidence that it does not.
///
/// The three records are verbatim shapes from the measured corpora: a
/// leading-zero literal, a markdown ordered-list marker, an ordinary
/// identifier. Two of the three are numbers as far as NUMERIC affinity is
/// concerned, and the marker's stored length drops from 3 to 1.
///
/// `#[ignore]` rather than deleted: the fix is a change to the shared
/// `nodes` contract (`record JSON` -> `record TEXT`, plus a producer that
/// binds bytes), which reaches mache and is therefore its own bead. Removing
/// the attribute is the activation gesture once that lands.
#[test]
#[ignore = "ley-line-open-b5faa9: `record JSON` in the shared nodes contract carries NUMERIC \
            affinity, so numeric-looking leaf tokens are coerced out of TEXT on insert and \
            activation fails closed on the length change"]
fn activation_survives_the_canonical_nodes_contract() {
    let conn = canonical_projection();
    insert_leaf(&conn, "a.rs/fn/body/int_literal", "007");
    insert_leaf(&conn, "README.md/list/list_item_0/list_marker_dot", "1. ");
    insert_leaf(&conn, "a.rs/fn/name", "main");

    let report = activate_chunked_content(&conn, ActivationOptions::default()).unwrap();

    assert_eq!(report.populated_nodes, 3);
}

/// The same defect, pinned as the behaviour that actually ships, so the
/// measurement in `examples/cdc_storage_bound.rs` can state honestly which
/// rows it had to repair before it could measure anything.
///
/// This test inverts when the contract is fixed. That is the point: it fails
/// loudly at the moment the coercion stops happening, rather than leaving the
/// ignored test above as the only signal.
#[test]
fn canonical_nodes_contract_coerces_numeric_records_and_blocks_activation() {
    let conn = canonical_projection();
    insert_leaf(&conn, "a.rs/fn/name", "main");
    insert_leaf(&conn, "a.rs/fn/body/int_literal", "007");
    insert_leaf(&conn, "README.md/list/list_item_0/list_marker_dot", "1. ");

    let coerced: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM nodes WHERE typeof(record) <> 'text'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(coerced, 2, "NUMERIC affinity takes both numeric tokens");

    let stored: Vec<u8> = conn
        .query_row(
            "SELECT CAST(record AS BLOB) FROM nodes \
              WHERE id = 'a.rs/fn/body/int_literal'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        stored, b"7",
        "the authoritative record is lossy: 007 reads back as 7"
    );

    let error = activate_chunked_content(&conn, ActivationOptions::default()).unwrap_err();
    assert!(
        format!("{error:#}").contains("does not match"),
        "unexpected error: {error:#}"
    );
}
