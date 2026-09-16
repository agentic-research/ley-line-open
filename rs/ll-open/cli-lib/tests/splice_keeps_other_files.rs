//! A splice re-projects ONE file and leaves every other file's rows alone
//! (bead ley-line-open-2b6444).
//!
//! `reproject` used to run `DELETE FROM nodes`, `DELETE FROM _ast` and
//! `DELETE FROM _source` for the whole arena before re-projecting the one
//! spliced file, so `leyline splice` on a daemon-parsed arena destroyed every
//! other file's projection. Every splice test seeded a single file, which is
//! why nothing saw it. This one parses TWO Go files through the real parser,
//! splices a node of one, and checks the other's rows across every
//! file-keyed table — and that the spliced file's `_source` row keeps the
//! daemon's shape (its `path` and a `content_hash` that is BLAKE3 of the
//! bytes it now holds).

use std::fs;

use tempfile::TempDir;

fn count_in_range(conn: &rusqlite::Connection, table: &str, rel: &str) -> i64 {
    let file_id = leyline_schema::lookup_file_id(conn, rel)
        .expect("lookup")
        .unwrap_or_else(|| panic!("{rel} must be interned"));
    let (lo, hi) = leyline_schema::file_nid_range(file_id);
    conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE nid BETWEEN ?1 AND ?2"),
        rusqlite::params![lo, hi],
        |r| r.get(0),
    )
    .expect("count")
}

#[test]
fn splicing_one_file_leaves_the_other_file_untouched() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("a.go"),
        b"package main\n\nfunc alpha() int {\n\treturn 1\n}\n",
    )
    .unwrap();
    // b.go both defines and references, so every file-keyed table has rows
    // for it and an accidental wipe of any one of them is visible.
    fs::write(
        dir.path().join("b.go"),
        b"package main\n\nfunc beta() int {\n\treturn helper()\n}\n\nfunc helper() int {\n\treturn 2\n}\n",
    )
    .unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    leyline_cli_lib::cmd_parse::parse_into_conn(&conn, dir.path(), Some("go"), None)
        .expect("cold parse");

    let b_before: Vec<(&str, i64)> = ["nodes", "_ast", "node_refs", "node_defs"]
        .into_iter()
        .map(|t| (t, count_in_range(&conn, t, "b.go")))
        .collect();
    assert!(
        b_before.iter().all(|(_, n)| *n > 0),
        "fixture must project rows for b.go in every table: {b_before:?}"
    );
    let a_file_id = leyline_schema::lookup_file_id(&conn, "a.go")
        .unwrap()
        .unwrap();
    let a_path_before: Option<String> = conn
        .query_row("SELECT path FROM _source WHERE id = 'a.go'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(
        a_path_before.is_some(),
        "the parser records the absolute path"
    );

    // Splice the whole of a.go (its file node spans the entire source).
    let a_root = leyline_schema::file_nid(a_file_id, 0);
    let new_source = b"package main\n\nfunc alpha() int {\n\treturn 42\n}\n";
    let spliced = leyline_ts::splice::splice_and_reproject(
        &conn,
        a_root,
        std::str::from_utf8(new_source).unwrap(),
    )
    .expect("splice a.go");
    assert_eq!(spliced, new_source);

    for (table, before) in &b_before {
        assert_eq!(
            count_in_range(&conn, table, "b.go"),
            *before,
            "{table}: b.go's rows must survive a splice of a.go"
        );
    }
    let b_source: i64 = conn
        .query_row("SELECT COUNT(*) FROM _source WHERE id = 'b.go'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(b_source, 1, "b.go's _source row must survive");

    // a.go: same file_id (so the same nid range), re-projected, and its
    // _source row still carries the daemon's columns.
    assert_eq!(
        leyline_schema::lookup_file_id(&conn, "a.go").unwrap(),
        Some(a_file_id)
    );
    assert!(count_in_range(&conn, "_ast", "a.go") > 0);
    let (path, hash): (Option<String>, Option<Vec<u8>>) = conn
        .query_row(
            "SELECT path, content_hash FROM _source WHERE id = 'a.go'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        path, a_path_before,
        "the absolute path must be carried across"
    );
    let expected_hash = {
        use leyline_core::substrate::ContentAddressed;
        *new_source.as_slice().hash().as_bytes()
    };
    assert_eq!(
        hash.as_deref(),
        Some(expected_hash.as_slice()),
        "content_hash must be BLAKE3 of the bytes the file now holds"
    );
    assert!(
        leyline_schema::resolve_path(&conn, "b.go")
            .unwrap()
            .is_some(),
        "b.go still resolves through the tree"
    );
}
