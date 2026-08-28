//! F4d — review-gate 6 of the identity ladder (bead `ley-line-open-17c271`):
//! **a scoped reparse of one file leaves every other file's rows
//! bit-identical.**
//!
//! ## Claim
//!
//! Reparsing (or deleting) file X through the scoped path touches only rows
//! that belong to X. For every other file, every row in every located table —
//! `nodes`, `_ast`, `_source`, `node_refs`, `node_defs`, `_imports`,
//! `_file_index`, `_ast_blob` — is byte-for-byte unchanged: same identities,
//! same spans, same hashes, same mtimes.
//!
//! ## How this differs from F4b
//!
//! `f4b_scoped_reparse_preserves_identities_of_untouched_files` compares the
//! `(node_id, source_id)` SET of `_ast` only. A defect that rewrites another
//! file's rows with the same identities but different content — or that eats
//! rows from a table F4b doesn't watch — passes F4b. This gate compares full
//! row images across every table that locates rows by file.
//!
//! ## What breaks this gate
//!
//! - A file-scoping predicate that over-matches. The fixture places a file
//!   under `a.gore/` precisely because an UNANCHORED prefix (`LIKE ?1 || '%'`
//!   instead of `?1 || '/%'` — the live pattern at `daemon/ops.rs`
//!   `NODE_ID_FOR_FILE`) makes a scoped delete of `a.go` eat `a.gore/`'s
//!   rows. Falsified live: unanchoring `delete_file_rows` turns the deletion
//!   test red.
//! - Post projection-v5: a nid range scan with off-by-one bounds
//!   (`(file_id<<24)..=((file_id+1)<<24)` instead of `..=(file_id<<24)|0xFFFFFF`),
//!   which deletes the first row of the NEXT file_id.
//! - Any identity assignment that renumbers unscoped files on a scoped pass
//!   (a global rank, a re-run interning pass that reassigns, a rebuilt
//!   `dirs`/`names` table that renumbers existing entries).
//!
//! ## What this gate is NOT
//!
//! Content-addressed tables (`node_content`, `node_child`, `capnp_blobs`,
//! `source_blobs`) are excluded: their rows are keyed by hash, shared across
//! files, and legitimately grow on any parse. Locality for them is a
//! GC/refcount question (ADR-0026), not an identity question.

#![cfg(feature = "hdc")]

use leyline_cli_lib::cmd_parse;
use rusqlite::Connection;
use rusqlite::types::ValueRef;
use std::fs;
use tempfile::TempDir;

/// Files the scoped pass must NOT touch. `a.gore/c.go` is the unanchored-
/// prefix trap: `'a.go' || '%'` matches it, `'a.go' || '/%'` does not.
const UNTOUCHED: &[&str] = &["b.go", "a.gore/c.go"];

fn fixture_repo() -> TempDir {
    let td = TempDir::new().unwrap();
    fs::write(
        td.path().join("a.go"),
        "package main\n\nfunc Alpha(a int) int {\n\tb := a + 1\n\treturn b\n}\n",
    )
    .unwrap();
    fs::write(
        td.path().join("b.go"),
        "package main\n\nimport \"strings\"\n\nfunc Beta(s string) string {\n\treturn strings.ToUpper(s)\n}\n",
    )
    .unwrap();
    fs::create_dir_all(td.path().join("a.gore")).unwrap();
    fs::write(
        td.path().join("a.gore/c.go"),
        "package gore\n\nfunc Gamma() int {\n\treturn 3\n}\n\nfunc Delta() int {\n\treturn 4\n}\n",
    )
    .unwrap();
    td
}

/// Full row images for one file across every located table, rendered to
/// strings so the comparison is type- and schema-agnostic (the projection-v5
/// re-key changes column types under this gate; the property must survive).
fn file_rows(conn: &Connection, rel: &str) -> Vec<String> {
    // (table, WHERE clause) — each clause is the file-scoping predicate the
    // production delete path uses for that table today.
    const LOCATED: &[(&str, &str)] = &[
        ("nodes", "id = ?1 OR id LIKE ?1 || '/%'"),
        ("_ast", "source_id = ?1"),
        ("_source", "id = ?1"),
        ("node_refs", "source_id = ?1"),
        ("node_defs", "source_id = ?1"),
        ("_imports", "source_id = ?1"),
        ("_file_index", "path = ?1"),
        ("_ast_blob", "source_id = ?1"),
    ];
    let mut out = Vec::new();
    for (table, clause) in LOCATED {
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |r| r.get(0),
            )
            .unwrap();
        if !exists {
            continue;
        }
        let sql = format!("SELECT * FROM {table} WHERE {clause}");
        let mut stmt = conn.prepare(&sql).unwrap();
        let ncols = stmt.column_count();
        let mut rows = stmt.query([rel]).unwrap();
        let mut table_rows: Vec<String> = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            let mut cells: Vec<String> = Vec::with_capacity(ncols + 1);
            cells.push(table.to_string());
            for i in 0..ncols {
                cells.push(match row.get_ref(i).unwrap() {
                    ValueRef::Null => "∅".to_string(),
                    ValueRef::Integer(v) => v.to_string(),
                    ValueRef::Real(v) => v.to_string(),
                    ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
                    ValueRef::Blob(b) => hex::encode(b),
                });
            }
            table_rows.push(cells.join("|"));
        }
        table_rows.sort();
        out.extend(table_rows);
    }
    out
}

fn snapshot_untouched(conn: &Connection) -> Vec<(String, Vec<String>)> {
    UNTOUCHED
        .iter()
        .map(|rel| (rel.to_string(), file_rows(conn, rel)))
        .collect()
}

fn assert_fixture_is_not_vacuous(snap: &[(String, Vec<String>)]) {
    for (rel, rows) in snap {
        assert!(
            rows.iter().any(|r| r.starts_with("nodes|")),
            "{rel} must own nodes rows, or this gate asserts nothing"
        );
        assert!(
            rows.iter().any(|r| r.starts_with("_ast|")),
            "{rel} must own _ast rows, or this gate asserts nothing"
        );
        assert!(
            rows.len() > 5,
            "{rel} must own >5 located rows total; got {}",
            rows.len()
        );
    }
}

#[test]
fn f4d_scoped_reparse_of_an_edited_file_leaves_other_files_rows_bit_identical() {
    let td = fixture_repo();
    let conn = Connection::open_in_memory().unwrap();
    cmd_parse::parse_into_conn(&conn, td.path(), Some("go"), None).unwrap();

    let before = snapshot_untouched(&conn);
    assert_fixture_is_not_vacuous(&before);

    // Edit a.go so its node count and spans change — the shape that shifts
    // any assignment ordered after it.
    fs::write(
        td.path().join("a.go"),
        "package main\n\nfunc Alpha(a int) int {\n\tb := a + 1\n\treturn b\n}\n\n\
         func Zed() {}\n\nfunc Yankee() {}\n",
    )
    .unwrap();
    cmd_parse::parse_into_conn(&conn, td.path(), Some("go"), Some(&["a.go".to_string()])).unwrap();

    let after = snapshot_untouched(&conn);
    assert_eq!(
        before, after,
        "a scoped reparse of a.go must leave every row of every other file \
         byte-identical in every located table (review-gate 6, \
         ley-line-open-17c271)"
    );
}

#[test]
fn f4d_scoped_delete_of_a_file_leaves_other_files_rows_bit_identical() {
    let td = fixture_repo();
    let conn = Connection::open_in_memory().unwrap();
    cmd_parse::parse_into_conn(&conn, td.path(), Some("go"), None).unwrap();

    let before = snapshot_untouched(&conn);
    assert_fixture_is_not_vacuous(&before);

    // Delete a.go and drive the deletion through the scoped path, exactly as
    // the git watcher reports a removed file.
    fs::remove_file(td.path().join("a.go")).unwrap();
    cmd_parse::parse_into_conn(&conn, td.path(), Some("go"), Some(&["a.go".to_string()])).unwrap();

    // a.go's own rows must actually be gone — the deletion has to have
    // happened for the locality claim below to mean anything.
    assert!(
        file_rows(&conn, "a.go").is_empty(),
        "a.go's rows must be deleted by the scoped pass"
    );

    let after = snapshot_untouched(&conn);
    assert_eq!(
        before, after,
        "deleting a.go through the scoped path must not touch any other \
         file's rows — `a.gore/c.go` in particular is one unanchored LIKE \
         away from being collateral (review-gate 6, ley-line-open-17c271)"
    );
}
