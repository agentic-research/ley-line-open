//! ADR-0026 Phase 1 dual-write — F1 round-trip integrity gate.
//!
//! Bead `ley-line-open-3e87ad`. Pins the Phase 1 contract: every parse
//! populates BOTH the row-projected `_ast` schema AND the content-addressed
//! pointer store (`capnp_blobs` + `_ast_pointer`), and any node reachable
//! via `_ast` is reachable via the pointer store with byte-identical field
//! values.
//!
//! F1 (round-trip integrity) is the load-bearing gate for Phase 1 (ADR-0026
//! §6.F1 + §9). If it ever fails, the pointer store cannot serve the same
//! queries as the row-projected schema — the design bet is broken and
//! Phase 2 must not begin.
//!
//! What this test does NOT cover (deferred to later phases):
//! - F2 wall-time win (Phase 1 doesn't measure perf)
//! - F3 sub-file edit locality (per-file blob unit — Phase 2/3 refinement)
//! - F4 cross-generation dedup at the blob level (`INSERT OR IGNORE` covers
//!   the dedup mechanic; full cross-gen suite is Phase 2)
//! - F5 cross-file dedup on identical subtrees (per-semantic-unit blob —
//!   Phase 2)
//! - F6 transport composition (ley-line ADR-014)

use std::fs;

use blake3;
use leyline_cli_lib::cmd_parse::parse_into_conn;
use leyline_schema_capnp::ast_capnp::ast_node_list;
use rusqlite::Connection;
use tempfile::TempDir;

/// A five-file Go fixture with a mix of AST node kinds. Small enough that
/// the test runs in milliseconds, big enough that a chunk-boundary bug in
/// the multi-row VALUES batch would surface (the AST rows total ~200+).
fn create_go_fixture() -> TempDir {
    let dir = TempDir::new().expect("create temp dir");
    fs::write(
        dir.path().join("main.go"),
        b"package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(add(1, 2))\n}\n",
    )
    .expect("write main.go");
    fs::write(
        dir.path().join("util.go"),
        b"package main\n\nfunc add(a, b int) int {\n\treturn a + b\n}\n\nfunc sub(a, b int) int {\n\treturn a - b\n}\n",
    )
    .expect("write util.go");
    fs::write(
        dir.path().join("types.go"),
        b"package main\n\ntype Point struct {\n\tX int\n\tY int\n}\n\ntype Vec struct {\n\tDX int\n\tDY int\n}\n",
    )
    .expect("write types.go");
    fs::write(
        dir.path().join("iface.go"),
        b"package main\n\ntype Adder interface {\n\tAdd(a, b int) int\n}\n",
    )
    .expect("write iface.go");
    fs::write(
        dir.path().join("consts.go"),
        b"package main\n\nconst Pi = 3\n\nvar Origin = Point{X: 0, Y: 0}\n",
    )
    .expect("write consts.go");
    dir
}

// ── Basic dual-write plumbing ─────────────────────────────────────────────

/// Both schemas MUST be populated after a fresh parse. Row counts are the
/// coarsest possible check (equality asserted in the F1 test below).
#[test]
fn dual_write_populates_both_schemas() {
    let src = create_go_fixture();
    let conn = Connection::open_in_memory().unwrap();
    let r = parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();
    assert_eq!(r.parsed, 5, "all five fixture files must parse");

    let ast_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM _ast", [], |row| row.get(0))
        .unwrap();
    let pointer_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM _ast_pointer", [], |row| row.get(0))
        .unwrap();
    let blob_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM capnp_blobs", [], |row| row.get(0))
        .unwrap();

    assert!(ast_rows > 0, "row-projected _ast must be populated");
    assert!(
        pointer_rows > 0,
        "pointer store _ast_pointer must be populated"
    );
    assert!(blob_rows > 0, "capnp_blobs must be populated");
}

/// Phase 1 blob unit is per-file: one `capnp_blobs` row per parsed source
/// file (deduped by content — with five distinct fixtures, expect exactly
/// five). Guards a future refactor that accidentally emits per-node blobs
/// (regression to the pre-ADR-0026 shape).
#[test]
fn one_blob_per_file() {
    let src = create_go_fixture();
    let conn = Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();

    let blob_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM capnp_blobs", [], |row| row.get(0))
        .unwrap();
    let source_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM _source", [], |row| row.get(0))
        .unwrap();
    assert_eq!(source_count, 5, "five _source rows");
    assert_eq!(
        blob_count, source_count,
        "one capnp_blobs row per source file (Phase 1 blob unit)",
    );
}

/// Every `blob_hash` in `capnp_blobs` must equal BLAKE3 of its `blob_bytes`.
/// Content-addressing is the whole point of the store; a producer that lets
/// the two drift silently corrupts the F4 cross-generation dedup claim.
#[test]
fn blob_hash_matches_blake3_of_bytes() {
    let src = create_go_fixture();
    let conn = Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();

    let mut stmt = conn
        .prepare("SELECT blob_hash, blob_bytes FROM capnp_blobs")
        .unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut checked = 0;
    while let Some(row) = rows.next().unwrap() {
        let stored_hash: Vec<u8> = row.get(0).unwrap();
        let bytes: Vec<u8> = row.get(1).unwrap();
        assert_eq!(stored_hash.len(), 32, "blob_hash must be a 32-byte BLAKE3");
        let recomputed = *blake3::hash(&bytes).as_bytes();
        assert_eq!(
            stored_hash.as_slice(),
            &recomputed[..],
            "blob_hash MUST equal BLAKE3(blob_bytes) — content-address invariant",
        );
        checked += 1;
    }
    assert!(checked > 0, "at least one blob must exist to verify");
}

/// Each blob's bytes must decode as a valid `AstNodeList` capnp message.
/// A truncated / mangled blob would silently break Phase 2 consumers; pin
/// the shape here so drift surfaces at Phase 1 CI, not at cutover.
#[test]
fn blobs_decode_as_ast_node_list() {
    let src = create_go_fixture();
    let conn = Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();

    let mut stmt = conn.prepare("SELECT blob_bytes FROM capnp_blobs").unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut checked = 0;
    while let Some(row) = rows.next().unwrap() {
        let bytes: Vec<u8> = row.get(0).unwrap();
        let mut slice: &[u8] = &bytes;
        let msg = capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
            .expect("blob must decode as a capnp message");
        let list: ast_node_list::Reader = msg.get_root().expect("root must be AstNodeList");
        let nodes = list.get_nodes().unwrap();
        assert!(
            nodes.len() > 0,
            "AstNodeList.nodes must be non-empty (every fixture file has ≥1 AST node)",
        );
        // Every node has a stable, non-empty nodeId + sourceId.
        for i in 0..nodes.len() {
            let n = nodes.get(i);
            assert!(
                !n.get_node_id().unwrap().to_str().unwrap().is_empty(),
                "AstNode.nodeId must be populated",
            );
            assert!(
                !n.get_source_id().unwrap().to_str().unwrap().is_empty(),
                "AstNode.sourceId must be populated",
            );
        }
        checked += 1;
    }
    assert_eq!(checked, 5, "must have decoded all five per-file blobs");
}

// ── F1: round-trip integrity ──────────────────────────────────────────────

/// **F1 (ADR-0026 §6.F1) — the load-bearing Phase 1 gate.**
///
/// For every row in `_ast`, look up the same node_id in `_ast_pointer`,
/// resolve to (blob_hash, offset_in_blob), decode the blob, index into the
/// list, assert every field byte-identical to the row-projected schema.
///
/// This is the falsifier that ADR-0026 §9 says must run continuously
/// during Phase 1. If it fails, the pointer store cannot serve the same
/// queries as the row-projected schema and Phase 2 is off the table until
/// the divergence is fixed.
#[test]
fn f1_round_trip_integrity() {
    let src = create_go_fixture();
    let conn = Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();

    // 1. Row count parity: every `_ast` row has exactly one `_ast_pointer`.
    let ast_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM _ast", [], |r| r.get(0))
        .unwrap();
    let pointer_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM _ast_pointer", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        ast_count, pointer_count,
        "F1: every _ast row MUST have exactly one _ast_pointer row (parity broken)",
    );
    // Every `_ast.node_id` MUST be present in `_ast_pointer`.
    let unmatched: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM _ast a \
             LEFT JOIN _ast_pointer p ON p.node_id = a.node_id \
             WHERE p.node_id IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        unmatched, 0,
        "F1: every _ast.node_id MUST resolve in _ast_pointer",
    );

    // 2. Cache all blobs in memory keyed by blob_hash, decoded as
    //    AstNodeList Readers. The Reader borrows from the message, so we
    //    decode once per blob and reuse.
    let mut blob_bytes_by_hash: std::collections::HashMap<Vec<u8>, Vec<u8>> =
        std::collections::HashMap::new();
    {
        let mut stmt = conn
            .prepare("SELECT blob_hash, blob_bytes FROM capnp_blobs")
            .unwrap();
        let mut rows = stmt.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            let h: Vec<u8> = row.get(0).unwrap();
            let b: Vec<u8> = row.get(1).unwrap();
            blob_bytes_by_hash.insert(h, b);
        }
    }

    // 3. For every `_ast` row, decode the corresponding pointer's blob at
    //    `offset_in_blob` and compare field-by-field.
    let mut stmt = conn
        .prepare(
            "SELECT a.node_id, a.source_id, a.node_kind, \
                    a.start_byte, a.end_byte, a.start_row, a.start_col, a.end_row, a.end_col, \
                    p.blob_hash, p.offset_in_blob \
             FROM _ast a JOIN _ast_pointer p ON p.node_id = a.node_id",
        )
        .unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut checked = 0usize;
    while let Some(row) = rows.next().unwrap() {
        let ast_node_id: String = row.get(0).unwrap();
        let ast_source_id: String = row.get(1).unwrap();
        let ast_node_kind: String = row.get(2).unwrap();
        let ast_start_byte: i64 = row.get(3).unwrap();
        let ast_end_byte: i64 = row.get(4).unwrap();
        let ast_start_row: i64 = row.get(5).unwrap();
        let ast_start_col: i64 = row.get(6).unwrap();
        let ast_end_row: i64 = row.get(7).unwrap();
        let ast_end_col: i64 = row.get(8).unwrap();
        let blob_hash: Vec<u8> = row.get(9).unwrap();
        let offset: i64 = row.get(10).unwrap();

        let blob_bytes = blob_bytes_by_hash
            .get(&blob_hash)
            .expect("F1: _ast_pointer.blob_hash MUST resolve in capnp_blobs");
        let mut slice: &[u8] = blob_bytes;
        let msg = capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
            .expect("F1: blob MUST decode as capnp message");
        let list: ast_node_list::Reader = msg.get_root().unwrap();
        let nodes = list.get_nodes().unwrap();
        assert!(
            (offset as u32) < nodes.len(),
            "F1: offset_in_blob ({offset}) MUST be < AstNodeList.nodes.len ({})",
            nodes.len(),
        );
        let n = nodes.get(offset as u32);

        // Byte-identical field-level comparison.
        assert_eq!(
            n.get_node_id().unwrap().to_str().unwrap(),
            ast_node_id,
            "F1: nodeId mismatch at node_id={ast_node_id}",
        );
        assert_eq!(
            n.get_source_id().unwrap().to_str().unwrap(),
            ast_source_id,
            "F1: sourceId mismatch at node_id={ast_node_id}",
        );
        assert_eq!(
            n.get_node_kind().unwrap().to_str().unwrap(),
            ast_node_kind,
            "F1: nodeKind mismatch at node_id={ast_node_id}",
        );
        let range = n.get_range().unwrap();
        let start = range.get_start().unwrap();
        let end = range.get_end().unwrap();
        assert_eq!(
            start.get_byte() as i64,
            ast_start_byte,
            "F1: start.byte mismatch at node_id={ast_node_id}",
        );
        assert_eq!(
            end.get_byte() as i64,
            ast_end_byte,
            "F1: end.byte mismatch at node_id={ast_node_id}",
        );
        assert_eq!(
            start.get_line() as i64,
            ast_start_row,
            "F1: start.line mismatch at node_id={ast_node_id}",
        );
        assert_eq!(
            start.get_column() as i64,
            ast_start_col,
            "F1: start.column mismatch at node_id={ast_node_id}",
        );
        assert_eq!(
            end.get_line() as i64,
            ast_end_row,
            "F1: end.line mismatch at node_id={ast_node_id}",
        );
        assert_eq!(
            end.get_column() as i64,
            ast_end_col,
            "F1: end.column mismatch at node_id={ast_node_id}",
        );

        checked += 1;
    }
    assert!(
        checked >= 100,
        "F1 must have checked at least 100 rows; got {checked} \
         (fixture may need to grow if this drops)",
    );
}

// ── Kind classification pin ───────────────────────────────────────────────

/// The Phase 1 semantic-kind allowlist covers function / method / type /
/// import per ADR-0026 §2.1. Pin at least the function-kind mapping so a
/// future refactor that silently rewrites the enum shows up here.
#[test]
fn semantic_kind_tags_functions_as_nonzero() {
    let src = create_go_fixture();
    let conn = Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();

    // add/sub/main/Println — every function_declaration must have a
    // non-zero (semantic) `kind` in _ast_pointer.
    let fn_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM _ast a JOIN _ast_pointer p ON p.node_id = a.node_id \
             WHERE a.node_kind = 'function_declaration' AND p.kind > 0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let fn_total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM _ast WHERE node_kind = 'function_declaration'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        fn_total >= 3,
        "fixture should contain ≥3 function_declaration rows"
    );
    assert_eq!(
        fn_count, fn_total,
        "every function_declaration MUST get a non-zero semantic kind tag",
    );
}
