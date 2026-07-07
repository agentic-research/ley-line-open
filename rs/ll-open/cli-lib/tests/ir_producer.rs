//! Tests for the merkle-AST content-addressed IR producer (ADR-0026 /
//! mache ADR-0023).
//!
//! Covers, in the order the feature was built:
//!   1. `_source.content_hash` — BLAKE3 of file bytes, populated + stable
//!      (retained, e251083 — complementary byte-level address).
//!   2. `_ast.node_hash` — the merkle-AST content address: byte-identical
//!      across files for identical subtrees, distinct for `a+b` vs `a-b`
//!      (anonymous operators are folded), unchanged under a whitespace or
//!      comment-only edit, and changed under an identifier rename.
//!   3. `node_content` / `node_child` — the deduped content layer + git-tree
//!      object, with FK integrity under `PRAGMA foreign_keys = ON`.
//!   4. `node_defs` / `node_refs` — occurrence rows now carrying `node_hash`,
//!      and the `unbound_facts` counter in head.capnp (parity with the old
//!      `fact_edges WHERE dst IS NULL` count).

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

/// The `node_hash` of the (single) function_declaration in `source_id`.
fn fn_hash(conn: &rusqlite::Connection, source_id: &str) -> Vec<u8> {
    conn.query_row(
        "SELECT node_hash FROM _ast \
         WHERE source_id = ?1 AND node_kind = 'function_declaration'",
        [source_id],
        |r| r.get(0),
    )
    .expect("function_declaration node must exist")
}

// ── 1. contentHash (retained) ─────────────────────────────────────────────

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

// ── 2. node_hash (merkle-AST content address) ─────────────────────────────

#[test]
fn node_hash_is_populated_on_every_ast_row() {
    let src = create_go_fixture();
    let conn = cold_parse_go(src.path());

    // Every _ast occurrence carries a 32-byte, non-null node_hash pointing at
    // a real node_content row.
    let (total, with_hash, min_len, max_len): (i64, i64, i64, i64) = conn
        .query_row(
            "SELECT count(*), \
                    count(node_hash), \
                    min(length(node_hash)), \
                    max(length(node_hash)) \
             FROM _ast",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert!(total > 0, "_ast must be populated");
    assert_eq!(with_hash, total, "every _ast row must carry a node_hash");
    assert_eq!(min_len, 32, "every node_hash is a 32-byte digest");
    assert_eq!(max_len, 32, "every node_hash is a 32-byte digest");

    // Referential integrity: no _ast.node_hash dangles off node_content.
    let dangling: i64 = conn
        .query_row(
            "SELECT count(*) FROM _ast a \
             LEFT JOIN node_content c ON a.node_hash = c.node_hash \
             WHERE c.node_hash IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        dangling, 0,
        "every _ast.node_hash must resolve to node_content"
    );
}

#[test]
fn node_hash_identical_bytes_different_path_are_equal() {
    // B1(a): two byte-identical functions in different files (different
    // filename + dir) must share a node_hash. Proves spans/paths never enter
    // the merkle preimage — a unique subtree is stored once.
    let bytes: &[u8] = b"package main\n\nfunc add(a, b int) int {\n\treturn a + b\n}\n";

    let dir_a = TempDir::new().unwrap();
    fs::write(dir_a.path().join("util.go"), bytes).unwrap();
    let conn_a = cold_parse_go(dir_a.path());

    let dir_b = TempDir::new().unwrap();
    fs::write(dir_b.path().join("renamed.go"), bytes).unwrap();
    let conn_b = cold_parse_go(dir_b.path());

    assert_eq!(
        fn_hash(&conn_a, "util.go"),
        fn_hash(&conn_b, "renamed.go"),
        "identical function bytes at different paths must share a node_hash"
    );
}

#[test]
fn node_hash_folds_anonymous_operators_add_vs_sub_differ() {
    // B1(b): `a + b` and `a - b` must hash DISTINCTLY. The operator is an
    // ANONYMOUS token; folding all non-extra children (not just named ones)
    // is what fixes the historical `a+b == a-b` collision.
    let plus = TempDir::new().unwrap();
    fs::write(plus.path().join("m.go"), b"package main\n\nvar c = a + b\n").unwrap();
    let conn_plus = cold_parse_go(plus.path());

    let minus = TempDir::new().unwrap();
    fs::write(
        minus.path().join("m.go"),
        b"package main\n\nvar c = a - b\n",
    )
    .unwrap();
    let conn_minus = cold_parse_go(minus.path());

    let hash_of = |conn: &rusqlite::Connection| -> Vec<u8> {
        conn.query_row(
            "SELECT node_hash FROM _ast WHERE node_kind = 'binary_expression'",
            [],
            |r| r.get(0),
        )
        .expect("a binary_expression node must exist")
    };
    assert_ne!(
        hash_of(&conn_plus),
        hash_of(&conn_minus),
        "`a + b` and `a - b` must have distinct node_hash (anonymous operator folded)"
    );
}

#[test]
fn node_hash_unchanged_under_whitespace_and_comment_edit() {
    // B1(c): reformatting whitespace and inserting a comment must NOT change
    // the enclosing function's node_hash. Whitespace produces no nodes;
    // comments are `extra` and excluded from the fold.
    let base = TempDir::new().unwrap();
    fs::write(
        base.path().join("m.go"),
        b"package main\n\nfunc add(a, b int) int {\n\treturn a + b\n}\n",
    )
    .unwrap();
    let conn_base = cold_parse_go(base.path());

    let edited = TempDir::new().unwrap();
    fs::write(
        edited.path().join("m.go"),
        b"package main\n\nfunc  add(a,   b   int)   int   {\n\t// an added comment\n\treturn a + b\n}\n",
    )
    .unwrap();
    let conn_edited = cold_parse_go(edited.path());

    assert_eq!(
        fn_hash(&conn_base, "m.go"),
        fn_hash(&conn_edited, "m.go"),
        "whitespace/comment-only edit must leave the function node_hash unchanged"
    );
}

#[test]
fn node_hash_changes_under_identifier_rename() {
    // B1(d): renaming the function identifier (`add` → `sub`) must change the
    // function's node_hash. Identifiers are hashed verbatim — the whole point
    // of not alpha-normalizing (find_definition resolves on the name string).
    let before = TempDir::new().unwrap();
    fs::write(
        before.path().join("m.go"),
        b"package main\n\nfunc add(a, b int) int {\n\treturn a + b\n}\n",
    )
    .unwrap();
    let conn_before = cold_parse_go(before.path());

    let after = TempDir::new().unwrap();
    fs::write(
        after.path().join("m.go"),
        b"package main\n\nfunc sub(a, b int) int {\n\treturn a + b\n}\n",
    )
    .unwrap();
    let conn_after = cold_parse_go(after.path());

    assert_ne!(
        fn_hash(&conn_before, "m.go"),
        fn_hash(&conn_after, "m.go"),
        "renaming the function identifier must change its node_hash"
    );
}

// ── 3. node_content / node_child (deduped content layer) ──────────────────

#[test]
fn node_content_dedups_below_ast_row_count() {
    // B2: the content layer dedups identical subtrees. With a
    // repetition-heavy fixture (the same `println(1)` statement 40×) the many
    // identical call subtrees collapse to a handful of unique node_content
    // rows, so node_content lands far below the un-deduped _ast occurrence
    // layer — dedup observed.
    let dir = TempDir::new().unwrap();
    let mut src = String::from("package main\n\nfunc main() {\n");
    for _ in 0..40 {
        src.push_str("\tprintln(1)\n");
    }
    src.push_str("}\n");
    fs::write(dir.path().join("m.go"), src.as_bytes()).unwrap();
    let conn = cold_parse_go(dir.path());

    let ast_count: i64 = conn
        .query_row("SELECT count(*) FROM _ast", [], |r| r.get(0))
        .unwrap();
    let content_count: i64 = conn
        .query_row("SELECT count(*) FROM node_content", [], |r| r.get(0))
        .unwrap();

    assert!(ast_count > 0 && content_count > 0, "both layers populated");
    assert!(
        content_count < ast_count,
        "node_content ({content_count}) must dedup below _ast ({ast_count})"
    );

    // node_content PRIMARY KEY is node_hash → every row is a distinct subtree.
    let distinct: i64 = conn
        .query_row(
            "SELECT count(DISTINCT node_hash) FROM node_content",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(distinct, content_count, "node_content.node_hash is unique");

    // node_child edges resolve to real content on both endpoints.
    let dangling_children: i64 = conn
        .query_row(
            "SELECT count(*) FROM node_child ch \
             LEFT JOIN node_content p ON ch.parent_hash = p.node_hash \
             LEFT JOIN node_content c ON ch.child_hash  = c.node_hash \
             WHERE p.node_hash IS NULL OR c.node_hash IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(dangling_children, 0, "node_child endpoints must resolve");
}

#[test]
fn node_content_fk_rejects_dangling_child_edge() {
    // B2: `PRAGMA foreign_keys` stays ON for the connection after the parse.
    // Inserting a node_child whose endpoints are not real content hashes must
    // fail loudly (be6136 made loud) rather than persist a dangling edge.
    let src = create_go_fixture();
    let conn = cold_parse_go(src.path());

    let bogus = vec![0xABu8; 32];
    let res = conn.execute(
        "INSERT INTO node_child (parent_hash, ordinal, child_hash, field) \
         VALUES (?1, 0, ?1, NULL)",
        rusqlite::params![bogus],
    );
    assert!(
        res.is_err(),
        "a node_child with no matching node_content must violate the FK"
    );
}

#[test]
fn node_content_leaf_and_internal_shape() {
    // B2: leaves carry a token + arity 0 + node_tag 0; internal nodes carry a
    // NULL token + arity > 0 + node_tag 1. Pin both arms.
    let src = create_go_fixture();
    let conn = cold_parse_go(src.path());

    // The `add` identifier leaf: token = 'add', node_tag 0, arity 0.
    let (tag, token, arity): (i64, Option<String>, i64) = conn
        .query_row(
            "SELECT node_tag, token, arity FROM node_content \
             WHERE kind = 'identifier' AND token = 'add'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("an `add` identifier leaf must exist");
    assert_eq!(tag, 0, "identifier leaf → node_tag 0");
    assert_eq!(token.as_deref(), Some("add"));
    assert_eq!(arity, 0, "leaf arity is 0");

    // Every internal row has a NULL token and arity > 0; every leaf has a
    // non-null token and arity 0 — the tag partitions the two cleanly.
    let bad: i64 = conn
        .query_row(
            "SELECT count(*) FROM node_content \
             WHERE (node_tag = 1 AND (token IS NOT NULL OR arity = 0)) \
                OR (node_tag = 0 AND arity <> 0)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bad, 0, "node_tag must partition leaf/internal invariants");
}

// ── 4. node_defs / node_refs + unbound_facts ──────────────────────────────

#[test]
fn defs_and_refs_carry_node_hash() {
    let src = create_go_fixture();
    let conn = cold_parse_go(src.path());

    // The `add` definition occurrence carries a node_hash pointing at real
    // content — the additive pointer B3 stamps on the occurrence tables.
    let dangling_defs: i64 = conn
        .query_row(
            "SELECT count(*) FROM node_defs d \
             LEFT JOIN node_content c ON d.node_hash = c.node_hash \
             WHERE d.node_hash IS NOT NULL AND c.node_hash IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        dangling_defs, 0,
        "node_defs.node_hash must resolve to content"
    );

    let dangling_refs: i64 = conn
        .query_row(
            "SELECT count(*) FROM node_refs r \
             LEFT JOIN node_content c ON r.node_hash = c.node_hash \
             WHERE r.node_hash IS NOT NULL AND c.node_hash IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        dangling_refs, 0,
        "node_refs.node_hash must resolve to content"
    );

    // The `add` def occurrence exists and is populated (non-null hash).
    let add_def_hash: Option<Vec<u8>> = conn
        .query_row(
            "SELECT node_hash FROM node_defs WHERE token = 'add'",
            [],
            |r| r.get(0),
        )
        .expect("an `add` def occurrence must exist");
    assert!(
        add_def_hash.is_some(),
        "the `add` def must carry a node_hash"
    );
}

#[test]
fn head_records_unbound_facts_parity() {
    use leyline_schema_capnp::head_capnp::head;

    // File-backed parse so the sibling head.capnp is written (the :memory:
    // path has no file to write next to and skips the head pass).
    let src = create_go_fixture();
    let out = TempDir::new().unwrap();
    let db_path = out.path().join("test.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    leyline_cli_lib::cmd_parse::parse_into_conn(&conn, src.path(), Some("go"), None)
        .expect("cold parse Go fixture (file-backed)");

    // New unbound-facts truth: node_refs whose token has no matching def.
    // This is the exact parity image of the retired
    // `fact_edges WHERE dst IS NULL AND kind IN ('references','calls')` count.
    let db_unbound: i64 = conn
        .query_row(
            "SELECT count(*) FROM node_refs WHERE token NOT IN (SELECT token FROM node_defs)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        db_unbound, 1,
        "exactly one unbound reference (println, a builtin with no def)"
    );

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
        "head.capnp unboundFacts must equal the db's unresolved node_refs count"
    );
}

#[test]
fn retired_symbols_and_fact_edges_tables_are_gone() {
    // B3: the eager symbols/fact_edges tables are deleted. Neither may be
    // produced by the parse pass anymore.
    let src = create_go_fixture();
    let conn = cold_parse_go(src.path());

    let leftover: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master \
             WHERE type = 'table' AND name IN ('symbols', 'fact_edges')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leftover, 0, "symbols/fact_edges must no longer be produced");
}

// ── 5. cross-language coverage (Rust) ─────────────────────────────────────
//
// The fold is language-agnostic — it runs over whatever tree tree-sitter
// produces. Go is covered above; this pins that Rust's function_item /
// call_expression extraction flows through the same merkle machinery,
// including a cross-file bound ref and an unbound builtin.

/// Parse a Rust source dir into a fresh in-memory db.
fn cold_parse_rust(src_dir: &Path) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    leyline_cli_lib::cmd_parse::parse_into_conn(&conn, src_dir, Some("rust"), None)
        .expect("cold parse Rust fixture");
    conn
}

#[test]
fn rust_defs_refs_and_unbound_flow_through_merkle() {
    let dir = TempDir::new().unwrap();
    // `add` defined in lib.rs, called from main.rs → cross-file bound ref.
    // The undefined `missing` call → an unbound reference.
    fs::write(
        dir.path().join("lib.rs"),
        b"pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("main.rs"),
        b"fn main() {\n    let _ = add(1, 2);\n    let _ = missing();\n}\n",
    )
    .unwrap();
    let conn = cold_parse_rust(dir.path());

    // κ collapses Rust's function_item → function on the content row; raw_kind
    // is retained.
    let (kind, raw_kind): (String, String) = conn
        .query_row(
            "SELECT c.kind, c.raw_kind FROM _ast a \
             JOIN node_content c ON a.node_hash = c.node_hash \
             WHERE a.source_id = 'lib.rs' AND a.node_kind = 'function_item'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("lib.rs must contribute an `add` function_item");
    assert_eq!(kind, "function", "κ collapses function_item → function");
    assert_eq!(
        raw_kind, "function_item",
        "raw tree-sitter kind is retained"
    );

    // defines: `add` recorded with a resolving node_hash.
    let defines_add: i64 = conn
        .query_row(
            "SELECT count(*) FROM node_defs WHERE token = 'add' AND node_hash IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(defines_add, 1, "one `add` def from lib.rs");

    // references: the `add` call resolves (its token has a def); `missing`
    // does not → it is the unbound reference.
    let add_bound: i64 = conn
        .query_row(
            "SELECT count(*) FROM node_refs \
             WHERE token = 'add' AND token IN (SELECT token FROM node_defs)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        add_bound >= 1,
        "the `add` call must be bound (token has a def)"
    );

    let missing_unbound: i64 = conn
        .query_row(
            "SELECT count(*) FROM node_refs \
             WHERE token = 'missing' AND token NOT IN (SELECT token FROM node_defs)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        missing_unbound, 1,
        "the undefined `missing` call is an unbound reference"
    );
}
