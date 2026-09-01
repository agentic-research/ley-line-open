//! Structural invariants of the projection-v5 nid scheme, asserted over a
//! real projection (bead `ley-line-open-17c271`).
//!
//! The pre-v5 sibling of this file (`parent_id_derivation_invariant.rs`)
//! pinned "every row's `id` agrees with its `name`" — the property the
//! derived `parent_id` leaned on. v5 stores `parent_nid` outright, so the
//! load-bearing properties move:
//!
//! 1. **Range discipline.** Every non-negative nid's high bits name a file
//!    that exists in `files`; every negative nid names a dir in `dirs`.
//! 2. **Dense pre-order ordinals.** A file's `_ast` nids are exactly
//!    `base..base+n-1` with no gaps — the ordinal doubles as the pointer
//!    store's blob index, so a gap or duplicate silently mis-addresses
//!    capnp records.
//! 3. **Parent closure.** Every stored `parent_nid` is a row that exists,
//!    and lives either in the same file's range or is the file's directory.
//! 4. **Display round-trip.** `node_path` renders every `nodes` row and
//!    `resolve_path` inverts it — the D6 locator demotion only works if the
//!    derived path is faithful.
//!
//! Violations here don't fail loudly in production — they silently rebind
//! blob lookups or drop nodes from listings — which is exactly why the
//! properties are asserted over a real parse across every writer.

use rusqlite::Connection;
use std::fs;
use tempfile::TempDir;

use leyline_cli_lib::cmd_parse;
use leyline_ts::schema::{self as ts_schema};

fn assert_projection_holds(conn: &Connection, what: &str) {
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
        .unwrap();
    assert!(
        total > 0,
        "{what}: fixture produced no nodes, so this asserts nothing"
    );

    // 1. Range discipline.
    let bad_file: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM nodes n \
             WHERE n.nid >= 0 \
               AND NOT EXISTS (SELECT 1 FROM files f WHERE f.file_id = n.nid >> 24)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        bad_file, 0,
        "{what}: every non-negative nid's high bits must name an interned file"
    );
    let bad_dir: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM nodes n \
             WHERE n.nid < 0 \
               AND NOT EXISTS (SELECT 1 FROM dirs d WHERE d.dir_id = -n.nid)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        bad_dir, 0,
        "{what}: every negative nid must name an interned dir"
    );

    // 2. Dense pre-order ordinals per file: max ordinal + 1 == row count.
    let sparse_files: Vec<i64> = {
        let mut stmt = conn
            .prepare(
                "SELECT nid >> 24 FROM _ast \
                 GROUP BY nid >> 24 \
                 HAVING MAX(nid & 16777215) + 1 <> COUNT(*)",
            )
            .unwrap();
        let v = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        v
    };
    assert!(
        sparse_files.is_empty(),
        "{what}: `_ast` ordinals must be dense 0..n-1 per file (they are the \
         pointer store's blob indexes); sparse file_ids: {sparse_files:?}"
    );

    // 3. Parent closure.
    let orphans: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM nodes c \
             WHERE c.parent_nid IS NOT NULL \
               AND NOT EXISTS (SELECT 1 FROM nodes p WHERE p.nid = c.parent_nid)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        orphans, 0,
        "{what}: every stored parent_nid must be a row that exists"
    );
    let cross_file: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM nodes c \
             WHERE c.nid >= 0 AND (c.nid & 16777215) <> 0 \
               AND c.parent_nid IS NOT NULL \
               AND (c.parent_nid < 0 OR (c.parent_nid >> 24) <> (c.nid >> 24))",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        cross_file, 0,
        "{what}: an AST node's parent must live in its own file's range"
    );

    // 4. Display round-trip over every row.
    let nids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT nid FROM nodes").unwrap();
        let v = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        v
    };
    for nid in nids {
        let path = ts_schema::node_path(conn, nid)
            .unwrap()
            .unwrap_or_else(|| panic!("{what}: nid {nid} must render a display path"));
        let back = ts_schema::resolve_path(conn, &path).unwrap();
        assert_eq!(
            back,
            Some(nid),
            "{what}: resolve_path must invert node_path for {path:?}"
        );
    }
}

/// The parse path writes file nodes, directory nodes, and one node per named
/// AST child. All three mint nids differently; all three must agree on the
/// scheme.
#[test]
fn every_parsed_node_satisfies_the_nid_invariants() {
    let td = TempDir::new().unwrap();
    fs::create_dir_all(td.path().join("deep/nested/dir")).unwrap();
    fs::write(
        td.path().join("top.go"),
        "package top\n\nfunc Alpha() string { return \"a\" }\n\ntype T struct{ X int }\n",
    )
    .unwrap();
    fs::write(
        td.path().join("deep/mid.go"),
        "package deep\n\nfunc Beta() {}\n",
    )
    .unwrap();
    fs::write(
        td.path().join("deep/nested/dir/leaf.go"),
        "package leaf\n\nfunc Gamma(a int, b string) error { return nil }\n\nvar V = 1\n",
    )
    .unwrap();

    let conn = Connection::open_in_memory().unwrap();
    cmd_parse::parse_into_conn(&conn, td.path(), Some("go"), None).unwrap();
    assert_projection_holds(&conn, "cold parse");
}

/// A scoped reparse rewrites one file's rows in place. If it minted ordinals
/// or parents differently from the cold path, the invariants would break for
/// that file only — and the arena would still look healthy by row count.
#[test]
fn a_scoped_reparse_preserves_the_nid_invariants() {
    let td = TempDir::new().unwrap();
    fs::write(td.path().join("a.go"), "package a\n\nfunc Alpha() {}\n").unwrap();
    fs::write(td.path().join("b.go"), "package b\n\nfunc Beta() {}\n").unwrap();

    let conn = Connection::open_in_memory().unwrap();
    cmd_parse::parse_into_conn(&conn, td.path(), Some("go"), None).unwrap();

    fs::write(
        td.path().join("a.go"),
        "package a\n\nfunc Alpha() {}\n\nfunc Delta(x int) {}\n\ntype S struct{}\n",
    )
    .unwrap();
    cmd_parse::parse_into_conn(&conn, td.path(), Some("go"), Some(&["a.go".to_string()])).unwrap();

    assert_projection_holds(&conn, "after scoped reparse");
}
