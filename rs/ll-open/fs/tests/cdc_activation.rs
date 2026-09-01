#![cfg(feature = "cdc")]

use leyline_fs::activation::{
    ActivationOptions, activate_chunked_content, activate_chunked_content_with_progress,
};
use rusqlite::{Connection, params};
use tempfile::TempDir;

/// Insert an eligible file leaf at `path` (kind 0, non-NULL record) and
/// return the nid the projection assigned it.
fn leaf(conn: &Connection, path: &str, record: &str, mtime: i64) -> i64 {
    let file_id = leyline_schema::ensure_file_id(conn, path).unwrap();
    let dir_id = leyline_schema::ensure_dir_nodes(conn, path, mtime).unwrap();
    let name_id = leyline_schema::intern_name(conn, path.rsplit('/').next().unwrap()).unwrap();
    let nid = leyline_schema::file_nid(file_id, 0);
    leyline_schema::insert_node(
        conn,
        nid,
        Some(leyline_schema::dir_nid(dir_id)),
        Some(name_id),
        None,
        0,
        0,
        record.len() as i64,
        mtime,
        record,
    )
    .unwrap();
    nid
}

/// The nid of an already-inserted display path.
fn nid_of(conn: &Connection, path: &str) -> i64 {
    leyline_schema::resolve_path(conn, path)
        .unwrap()
        .unwrap_or_else(|| panic!("fixture path {path:?} must resolve"))
}

/// Two eligible leaves plus one directory, which activation must skip.
///
/// `a.rs` is interned first, so it holds `file_id` 1 and therefore the
/// LOWEST nid in the arena — the keyset paging tests below lean on that.
fn projection() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    leyline_schema::create_nodes_table(&conn).unwrap();
    leaf(&conn, "a.rs", "fn a() {}\n", 7);
    leaf(&conn, "empty.rs", "", 7);
    // A directory: negative nid, kind 1, so it fails the `kind = 0` filter.
    let dir_id = leyline_schema::intern_dir_chain(&conn, "dir").unwrap();
    let dir_name = leyline_schema::intern_name(&conn, "dir").unwrap();
    leyline_schema::insert_node(
        &conn,
        leyline_schema::dir_nid(dir_id),
        Some(leyline_schema::dir_nid(1)),
        Some(dir_name),
        None,
        1,
        0,
        0,
        7,
        "",
    )
    .unwrap();
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
            nid INTEGER PRIMARY KEY,
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
    let empty = nid_of(&conn, "empty.rs");
    leyline_fs::chunked::create_chunked_content_schema(&conn).unwrap();
    conn.execute_batch(&format!(
        "CREATE TRIGGER fail_second BEFORE INSERT ON content_manifest_meta
         WHEN NEW.nid = {empty}
         BEGIN SELECT RAISE(ABORT, 'injected activation failure'); END;"
    ))
    .unwrap();

    let error = activate_chunked_content(&conn, ActivationOptions { batch_size: 1 }).unwrap_err();
    assert!(
        format!("{error:#}").contains(&empty.to_string()),
        "failing node must be named: {error:#}"
    );
    assert!(leyline_fs::chunked::has_chunked_content(&conn, nid_of(&conn, "a.rs")).unwrap());

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
         WHERE nid = ?1",
        [nid_of(&conn, "a.rs")],
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
    let a = nid_of(&conn, "a.rs");
    conn.execute("UPDATE nodes SET size = 999 WHERE nid = ?1", [a])
        .unwrap();

    let error = activate_chunked_content(&conn, ActivationOptions::default()).unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains(&a.to_string()),
        "error must name node: {message}"
    );
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
    let a = nid_of(&conn, "a.rs");
    let empty = nid_of(&conn, "empty.rs");
    let mut pages = 0;
    let report =
        activate_chunked_content_with_progress(&conn, ActivationOptions { batch_size: 1 }, |_| {
            pages += 1;
            if pages == 1 {
                conn.execute("DELETE FROM nodes WHERE nid = ?1", [a])
                    .unwrap();
            }
        })
        .unwrap();

    assert_eq!(pages, 2);
    assert_eq!(report.eligible_nodes, 1);
    assert_eq!(report.populated_nodes, 2);
    assert!(
        leyline_fs::chunked::has_chunked_content(&conn, empty).unwrap(),
        "removing a processed row must not shift the next row behind an OFFSET"
    );
}

/// The keyset cursor is `Option<i64>`, and `None` means "no cursor yet" — NOT
/// "cursor at the smallest key". The pre-v5 shape of this trap was an empty
/// STRING id, indistinguishable from an absent cursor to anything that tested
/// emptiness instead of `Option`; the v5 shape is the arena's LOWEST nid,
/// which a cursor conflating the two would either skip or re-visit forever.
///
/// `a.rs` holds `file_id` 1 and ordinal 0, so its nid is the smallest any
/// node in this arena can have.
#[test]
fn activation_keyset_visits_the_lowest_nid_exactly_once() {
    let conn = projection();
    let lowest = nid_of(&conn, "a.rs");
    assert_eq!(
        lowest,
        leyline_schema::file_nid(1, 0),
        "fixture must put a.rs at the arena's minimum nid"
    );

    let mut visited = Vec::new();
    let report =
        activate_chunked_content_with_progress(&conn, ActivationOptions { batch_size: 1 }, |u| {
            visited.push(u.visited_nodes)
        })
        .unwrap();

    assert_eq!(report.eligible_nodes, 2);
    assert_eq!(report.populated_nodes, 2);
    assert_eq!(visited, vec![1, 2], "each node visited exactly once");
    assert!(
        leyline_fs::chunked::has_chunked_content(&conn, lowest).unwrap(),
        "the minimum nid is a valid keyset value, not an absent cursor"
    );
}

/// A row that appears BEHIND the keyset cursor is invisible to the paging
/// loop, so only the convergence pass can catch it — and it must, or
/// activation reports success over a stale manifest.
///
/// Under v5 that row is an AST node of an ALREADY-PAGED file: nids are
/// file-scoped, so `a.rs`'s ordinal 5 sits inside file 1's range and
/// therefore below the cursor once paging has moved on to file 2. A newly
/// interned FILE could not reproduce this — `files` is append-only, so its
/// `file_id` and hence its nid always sort after everything already there.
#[test]
fn activation_converges_when_a_concurrent_insert_sorts_before_the_cursor() {
    let conn = projection();
    let latecomer = leyline_schema::file_nid(1, 5);
    assert!(
        latecomer < nid_of(&conn, "empty.rs"),
        "the injected row must sort behind the final cursor"
    );

    let mut pages = 0;
    let report =
        activate_chunked_content_with_progress(&conn, ActivationOptions { batch_size: 1 }, |_| {
            pages += 1;
            if pages == 2 {
                let body = "fn zero(){}";
                leyline_schema::insert_node(
                    &conn,
                    latecomer,
                    Some(nid_of(&conn, "a.rs")),
                    None,
                    None,
                    0,
                    5,
                    body.len() as i64,
                    8,
                    body,
                )
                .unwrap();
            }
        })
        .unwrap();

    assert_eq!(report.eligible_nodes, 3);
    assert_eq!(report.populated_nodes, 3);
    assert!(
        leyline_fs::chunked::has_chunked_content(&conn, latecomer).unwrap(),
        "activation must not report success with an inserted eligible row stale"
    );
}

#[test]
fn stale_caller_bytes_cannot_be_paired_with_a_new_authoritative_witness() {
    let temp = TempDir::new().unwrap();
    let db = temp.path().join("projection.db");
    let reader = Connection::open(&db).unwrap();
    reader.execute_batch("PRAGMA journal_mode = WAL;").unwrap();
    leyline_schema::create_nodes_table(&reader).unwrap();
    let a = leaf(&reader, "a.rs", "fn a() {}\n", 7);
    leyline_fs::chunked::create_chunked_content_schema(&reader).unwrap();
    let old_bytes: Vec<u8> = reader
        .query_row(
            "SELECT CAST(record AS BLOB) FROM nodes WHERE nid = ?1",
            [a],
            |row| row.get(0),
        )
        .unwrap();
    leyline_fs::chunked::store_content_chunked(&reader, a, &old_bytes).unwrap();

    let writer = Connection::open(&db).unwrap();
    writer
        .execute(
            "UPDATE nodes
                SET record = 'fn b() {}\n', mtime = 8
              WHERE nid = ?1",
            [a],
        )
        .unwrap();

    let error = leyline_fs::chunked::store_content_chunked(&reader, a, &old_bytes)
        .expect_err("stale caller bytes must not receive the new row witness");
    assert!(
        format!("{error:#}").contains("authoritative node changed"),
        "unexpected error: {error:#}"
    );
    assert!(
        !leyline_fs::chunked::has_chunked_content(&reader, a).unwrap(),
        "a rejected stale store must never look fresh"
    );
    let preserved_witness: i64 = reader
        .query_row(
            "SELECT source_mtime FROM content_manifest_meta WHERE nid = ?1",
            [a],
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

/// An AST leaf under `file`, at `ordinal` within that file's nid range —
/// the shape a real parse writes, and the shape these records come from.
fn insert_leaf(conn: &Connection, file: &str, ordinal: i64, record: &str) -> i64 {
    let file_id = leyline_schema::ensure_file_id(conn, file).unwrap();
    leyline_schema::ensure_dir_nodes(conn, file, 7).unwrap();
    let nid = leyline_schema::file_nid(file_id, ordinal);
    leyline_schema::insert_node(
        conn,
        nid,
        Some(leyline_schema::file_nid(file_id, 0)),
        None,
        None,
        0,
        ordinal,
        record.len() as i64,
        7,
        record,
    )
    .unwrap();
    nid
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
fn activation_survives_the_canonical_nodes_contract() {
    let conn = canonical_projection();
    insert_leaf(&conn, "a.rs", 1, "007");
    insert_leaf(&conn, "README.md", 1, "1. ");
    insert_leaf(&conn, "a.rs", 2, "main");

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
fn canonical_nodes_contract_stores_numeric_tokens_verbatim() {
    let conn = canonical_projection();
    let name = insert_leaf(&conn, "a.rs", 1, "main");
    let int_literal = insert_leaf(&conn, "a.rs", 2, "007");
    let marker = insert_leaf(&conn, "README.md", 1, "1. ");

    let coerced: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM nodes WHERE typeof(record) <> 'text'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        coerced, 0,
        "every record must read back as text; a non-text typeof means the \
         column's declared type has drifted back to a NUMERIC-affinity name"
    );

    for (nid, expected) in [
        (int_literal, &b"007"[..]),
        (marker, &b"1. "[..]),
        (name, &b"main"[..]),
    ] {
        let stored: Vec<u8> = conn
            .query_row(
                "SELECT CAST(record AS BLOB) FROM nodes WHERE nid = ?1",
                params![nid],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stored, expected,
            "record for nid {nid} must round-trip verbatim"
        );
    }

    // And the size witness now agrees, so activation completes rather than
    // failing closed on damage the DDL caused upstream.
    let report = activate_chunked_content(&conn, ActivationOptions::default()).unwrap();
    assert_eq!(report.populated_nodes, 3);
}

/// Activation opened one IMMEDIATE transaction PER NODE and committed it, so a
/// projection with 391,556 leaves paid 391,556 commits — measured at ~10 KiB/s
/// against a chunker that benchmarks in the hundreds of MiB/s. The work per
/// node is trivial; the commit is not.
///
/// Counting commits rather than timing them keeps this exact: a wall-clock
/// assertion would be flaky on a loaded machine and would not say *why* it
/// regressed. Resumability is preserved at page granularity — a crash re-does
/// at most one page, and re-activation is idempotent through `AlreadyFresh`.
///
/// Bead `ley-line-open-b5faa9`.
#[test]
fn activation_commits_per_page_not_per_node() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    leyline_fs::chunked::create_chunked_content_schema(&conn).unwrap();
    leyline_schema::create_nodes_table(&conn).unwrap();
    const NODES: usize = 64;
    for i in 0..NODES {
        let body = format!("fn n{i}() {{}}");
        leaf(&conn, &format!("n{i}.rs"), &body, 1);
    }

    let commits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&commits);
    conn.commit_hook(Some(move || {
        counter.fetch_add(1, Ordering::SeqCst);
        false
    }))
    .expect("commit hook must install, or this test counts nothing");

    let report = leyline_fs::activation::activate_chunked_content(
        &conn,
        leyline_fs::activation::ActivationOptions { batch_size: 256 },
    )
    .unwrap();
    assert_eq!(
        report.populated_nodes as usize, NODES,
        "all nodes activated"
    );

    let observed = commits.load(Ordering::SeqCst);
    assert!(
        observed < NODES,
        "activation must batch commits per page, not per node: \
         {NODES} nodes produced {observed} commits"
    );
}
