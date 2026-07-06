//! Tests for the unified code-fact IR producer (ADR-0026 / mache ADR-0023).
//!
//! Covers, in the order the feature was built:
//!   1. `_source.content_hash` — BLAKE3 of file bytes, populated + stable.
//!   2. `symbols` — one row per AST node, content-addressed `symbol_id`
//!      that is byte-identical across parse runs for unchanged files and
//!      excludes the path (the be6136 fix), with κ collapsing kinds.
//!   3. `fact_edges` — contains/defines/references arms, FK fail-loud on a
//!      dangling edge, and the `unbound_facts` counter in head.capnp.

use std::fs;
use std::path::Path;

use tempfile::TempDir;

/// Parse a Go source dir into a fresh in-memory db. Mirrors the
/// `cold_parse_go` idiom in integration.rs.
fn cold_parse_go(src_dir: &Path) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    leyline_cli_lib::cmd_parse::parse_into_conn(&conn, src_dir, Some("go"), None)
        .expect("cold parse Go fixture");
    conn
}

/// A minimal two-file Go fixture. `add` is defined in util.go and called
/// from main.go, giving us a resolvable cross-file reference plus at
/// least one unresolvable one (`println`, a builtin with no def row).
fn create_go_fixture() -> TempDir {
    let dir = TempDir::new().expect("create temp dir");
    fs::write(
        dir.path().join("main.go"),
        b"package main\n\nfunc main() {\n\tprintln(add(1, 2))\n}\n",
    )
    .expect("write main.go");
    fs::write(
        dir.path().join("util.go"),
        b"package main\n\nfunc add(a, b int) int {\n\treturn a + b\n}\n",
    )
    .expect("write util.go");
    dir
}

// ── 1. contentHash ────────────────────────────────────────────────────────

#[test]
fn content_hash_is_populated_and_correct_length() {
    let src = create_go_fixture();
    let conn = cold_parse_go(src.path());

    let mut stmt = conn
        .prepare("SELECT id, content_hash FROM _source ORDER BY id")
        .unwrap();
    let rows: Vec<(String, Vec<u8>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(rows.len(), 2, "two .go files → two _source rows");
    for (id, hash) in &rows {
        assert_eq!(
            hash.len(),
            32,
            "content_hash for {id} must be a BLAKE3-32 digest, got {} bytes",
            hash.len()
        );
        assert_ne!(hash, &vec![0u8; 32], "content_hash for {id} must not be zeroed");
    }
    assert_ne!(
        rows[0].1, rows[1].1,
        "distinct file contents must hash to distinct content_hash values"
    );
}

#[test]
fn content_hash_matches_blake3_of_file_bytes() {
    let src = create_go_fixture();
    let conn = cold_parse_go(src.path());

    let stored: Vec<u8> = conn
        .query_row(
            "SELECT content_hash FROM _source WHERE id = 'util.go'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    let bytes = fs::read(src.path().join("util.go")).unwrap();
    let expected = blake3::hash(&bytes);
    assert_eq!(
        stored,
        expected.as_bytes(),
        "stored content_hash must equal BLAKE3(file bytes)"
    );
}

#[test]
fn content_hash_is_stable_across_reparse_of_unchanged_file() {
    let src = create_go_fixture();

    let h1: Vec<u8> = {
        let conn = cold_parse_go(src.path());
        conn.query_row(
            "SELECT content_hash FROM _source WHERE id = 'main.go'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    // Second cold parse of the same, unchanged bytes.
    let h2: Vec<u8> = {
        let conn = cold_parse_go(src.path());
        conn.query_row(
            "SELECT content_hash FROM _source WHERE id = 'main.go'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };

    assert_eq!(
        h1, h2,
        "content_hash must be byte-identical across parse runs for unchanged bytes"
    );
}
