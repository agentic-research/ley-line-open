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
        assert_ne!(
            hash,
            &vec![0u8; 32],
            "content_hash for {id} must not be zeroed"
        );
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

// ── 2. symbols ────────────────────────────────────────────────────────────

#[test]
fn symbols_kind_is_kappa_canonical_with_raw_kind_retained() {
    let src = create_go_fixture();
    let conn = cold_parse_go(src.path());

    // Every AST node became a symbol with a 32-byte content-addressed id.
    let (count, min_len, max_len): (i64, i64, i64) = conn
        .query_row(
            "SELECT count(*), min(length(symbol_id)), max(length(symbol_id)) FROM symbols",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert!(count > 0, "symbols table must be populated");
    assert_eq!(min_len, 32, "every symbol_id is a BLAKE3-32 digest");
    assert_eq!(max_len, 32, "every symbol_id is a BLAKE3-32 digest");

    // The `add` function collapses to the κ kind `function`, while `raw_kind`
    // retains the Go grammar's `function_declaration` for rules that need it.
    let (kind, raw_kind): (String, String) = conn
        .query_row(
            "SELECT kind, raw_kind FROM symbols \
             WHERE source_id = 'util.go' AND name = 'add' \
             AND raw_kind = 'function_declaration'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("util.go must contribute an `add` function symbol");
    assert_eq!(
        kind, "function",
        "κ collapses function_declaration → function"
    );
    assert_eq!(
        raw_kind, "function_declaration",
        "raw tree-sitter kind is retained"
    );
}

#[test]
fn symbol_id_excludes_path_so_identical_bytes_match() {
    // Same bytes, different filename + different dir → identical symbol_id for
    // the `add` function. Proves the path never enters the content address
    // (the be6136 fix) and that ids are stable/diffable across parse runs.
    let bytes: &[u8] = b"package main\n\nfunc add(a, b int) int {\n\treturn a + b\n}\n";

    let dir_a = TempDir::new().unwrap();
    fs::write(dir_a.path().join("util.go"), bytes).unwrap();
    let conn_a = cold_parse_go(dir_a.path());

    let dir_b = TempDir::new().unwrap();
    fs::write(dir_b.path().join("renamed.go"), bytes).unwrap();
    let conn_b = cold_parse_go(dir_b.path());

    let id_of = |conn: &rusqlite::Connection| -> Vec<u8> {
        conn.query_row(
            "SELECT symbol_id FROM symbols WHERE name = 'add' AND kind = 'function'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        id_of(&conn_a),
        id_of(&conn_b),
        "identical bytes at different paths must yield an identical symbol_id"
    );
}

// ── 3. fact_edges ─────────────────────────────────────────────────────────

#[test]
fn fact_edges_contains_defines_references_arms() {
    let src = create_go_fixture();
    let conn = cold_parse_go(src.path());

    // contains: structural parent→child edges are produced.
    let contains: i64 = conn
        .query_row(
            "SELECT count(*) FROM fact_edges WHERE kind = 'contains'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(contains > 0, "containment edges must be produced");

    // defines: the `add` definition site is recorded (single-ended, NULL dst).
    let defines_add: i64 = conn
        .query_row(
            "SELECT count(*) FROM fact_edges \
             WHERE kind = 'defines' AND token = 'add' AND dst IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(defines_add, 1, "exactly one `defines add` edge");

    // references (bound): main.go's call to add resolves cross-file onto
    // util.go's add definition symbol — the whole point of the content key.
    let add_def_sym: Vec<u8> = conn
        .query_row(
            "SELECT symbol_id FROM symbols \
             WHERE source_id = 'util.go' AND name = 'add' AND kind = 'function'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let ref_add_dst: Option<Vec<u8>> = conn
        .query_row(
            "SELECT dst FROM fact_edges WHERE kind = 'references' AND token = 'add'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        ref_add_dst.as_deref(),
        Some(add_def_sym.as_slice()),
        "the `add` reference must bind to util.go's add definition symbol"
    );

    // references (unbound): println is a builtin with no def row → NULL dst,
    // which is what feeds the head's unbound_facts ratchet.
    let println_unbound: i64 = conn
        .query_row(
            "SELECT count(*) FROM fact_edges \
             WHERE kind = 'references' AND token = 'println' AND dst IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        println_unbound, 1,
        "println is unresolvable → exactly one unbound reference edge"
    );
}

#[test]
fn fact_edges_fk_rejects_dangling_edge() {
    let src = create_go_fixture();
    let conn = cold_parse_go(src.path());

    // `foreign_keys` stays ON for the connection after the parse. Inserting an
    // edge whose src is not a real symbol_id must fail loudly (be6136 made
    // loud) rather than persist a silently-dangling row.
    let bogus = vec![0xABu8; 32];
    let res = conn.execute(
        "INSERT INTO fact_edges (src, dst, kind, fidelity, gen) \
         VALUES (?1, NULL, 'references', 'mention', 1)",
        rusqlite::params![bogus],
    );
    assert!(
        res.is_err(),
        "a dangling fact_edges.src must violate the FK, not insert silently"
    );
}

#[test]
fn head_records_unbound_facts_count() {
    use leyline_schema_capnp::head_capnp::head;

    // File-backed parse so the sibling head.capnp is written (the :memory:
    // path has no file to write next to and skips the head pass).
    let src = create_go_fixture();
    let out = TempDir::new().unwrap();
    let db_path = out.path().join("test.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    leyline_cli_lib::cmd_parse::parse_into_conn(&conn, src.path(), Some("go"), None)
        .expect("cold parse Go fixture (file-backed)");

    // Db-level truth: exactly one unbound reference (println).
    let db_unbound: i64 = conn
        .query_row(
            "SELECT count(*) FROM fact_edges WHERE dst IS NULL AND kind IN ('references','calls')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(db_unbound, 1, "exactly one unbound reference (println)");

    // The Head must record the same count so the W5 ratchet reads it without
    // re-querying the db.
    let head_path = db_path.with_extension("head.capnp");
    let bytes = fs::read(&head_path).expect("head.capnp must be written for a file-backed db");
    let mut slice: &[u8] = &bytes;
    let msg =
        capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new()).unwrap();
    let h: head::Reader = msg.get_root().unwrap();
    assert_eq!(
        h.get_unbound_facts(),
        db_unbound as u64,
        "head.capnp unboundFacts must equal the db's NULL-dst reference count"
    );
}
