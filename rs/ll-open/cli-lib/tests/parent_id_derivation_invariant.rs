//! `nodes.parent_id` is derived, so `id` must agree with `name`.
//!
//! The derived column computes a node's parent as its own `id` with the
//! trailing `/<name>` removed. That makes one property load-bearing that used
//! to be merely true: **every row's `id` is either its `name` (a root) or ends
//! with `/` followed by its `name`.**
//!
//! Every writer maintains it by construction — ids are built as
//! `parent + "/" + name` — but nothing enforced it, and a violation does not
//! fail. It silently yields the WRONG parent, usually `''`, which makes the
//! node vanish from its directory listing while every row count stays correct.
//! Exactly that happened to one fixture during this change: `id='root/tricky'`
//! with `name='line1\nline2'` listed as having no parent at all.
//!
//! Spot-checking call sites cannot cover this — `insert_node(conn, id, name,
//! ..)` takes two adjacent `&str`, so a swapped pair compiles. This asserts the
//! property over a real projection instead, across every writer the parse path
//! runs.

use rusqlite::Connection;
use std::fs;
use tempfile::TempDir;

use leyline_cli_lib::cmd_parse;

/// Every row whose `id` does not agree with its `name`, as
/// `(id, name, derived_parent)`.
///
/// Expressed as SQL over the projection rather than over the Rust that wrote
/// it, so it holds regardless of which writer produced the row.
fn rows_violating_the_invariant(conn: &Connection) -> Vec<(String, String, String)> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, parent_id FROM nodes \
             WHERE id <> name \
               AND substr(id, length(id) - length(name)) <> '/' || name \
             ORDER BY id",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

fn assert_projection_holds(conn: &Connection, what: &str) {
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
        .unwrap();
    assert!(
        total > 0,
        "{what}: fixture produced no nodes, so this asserts nothing"
    );

    let bad = rows_violating_the_invariant(conn);
    assert!(
        bad.is_empty(),
        "{what}: {} of {total} rows have a `name` that is not the last segment \
         of their own `id`, so the derived `parent_id` is not the parent they \
         belong under. First few: {:?}",
        bad.len(),
        &bad[..bad.len().min(5)]
    );
}

/// The parse path writes file nodes, directory nodes, and one node per named
/// AST child. All three build ids differently; all three must agree.
#[test]
fn every_parsed_node_agrees_with_its_own_derived_parent() {
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

    // Every node must also actually resolve to a parent that EXISTS — a
    // derivation can satisfy the string property and still point at nothing.
    let orphans: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM nodes c \
             WHERE c.parent_id <> '' \
               AND NOT EXISTS (SELECT 1 FROM nodes p WHERE p.id = c.parent_id)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        orphans, 0,
        "every non-root node's derived parent must be a row that exists"
    );
}

/// A scoped reparse rewrites one file's rows in place. If it reconstructed ids
/// or names differently from the cold path, the invariant would break for that
/// file only — and the arena would still look healthy by row count.
#[test]
fn a_scoped_reparse_preserves_the_invariant() {
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

/// A name that is not a plausible path segment is the case that broke a fixture
/// during this change. A file genuinely named with a space or a dot still has
/// `id` ending in `/` + that name, so it holds — the property is about
/// AGREEMENT between the two columns, not about what characters a name may
/// contain.
#[test]
fn names_that_are_not_tidy_identifiers_still_agree() {
    let td = TempDir::new().unwrap();
    fs::write(td.path().join("has space.go"), "package s\n\nfunc S() {}\n").unwrap();
    fs::write(
        td.path().join("dots.in.name.go"),
        "package d\n\nfunc D() {}\n",
    )
    .unwrap();

    let conn = Connection::open_in_memory().unwrap();
    cmd_parse::parse_into_conn(&conn, td.path(), Some("go"), None).unwrap();
    assert_projection_holds(&conn, "awkward file names");

    for name in ["has space.go", "dots.in.name.go"] {
        let parent: String = conn
            .query_row("SELECT parent_id FROM nodes WHERE id = ?1", [name], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            parent, "",
            "{name} is at the repo root, so its parent is ''"
        );
    }
}
