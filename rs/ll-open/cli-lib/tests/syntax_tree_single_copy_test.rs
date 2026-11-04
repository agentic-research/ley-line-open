//! ADR-0040 D5 — one stored copy of the syntax tree (bead
//! `ley-line-open-f30fdf`, projection-v8).
//!
//! The ADR-0026 pointer store (`capnp_blobs` + `_ast_blob`) held a second
//! copy of every file's syntax tree as one canonical `AstNodeList` blob per
//! file. Nothing read it, and its blob hash bound the tree to the file's
//! path (`AstNode.sourceId`), so two byte-identical files at different paths
//! held two blobs. It is retired. What remains:
//!
//! - the SQL rows (`_ast`, `node_content`, `node_child`), keyed by the
//!   location-free `node_hash` — the copy every consumer reads;
//! - the `.ast.capnp` segment log, which `Head.rootHash` folds. Its bytes
//!   never depended on the blob, so the run receipt's definition and value
//!   are untouched by the retirement.
//!
//! Each test names the claim it falsifies.

use std::fs;
use std::path::Path;

use leyline_cli_lib::cmd_parse::{self, parse_into_conn};
use leyline_schema_capnp::ast_capnp::ast_node;
use rusqlite::Connection;
use tempfile::TempDir;

const SHARED: &str = "package shared\n\nfunc Add(a, b int) int {\n\treturn a + b\n}\n\n\
                      type Point struct {\n\tX int\n\tY int\n}\n";
const OTHER: &str = "package other\n\nfunc Sub(a, b int) int {\n\treturn a - b\n}\n";

fn write_tree(files: &[(&str, &str)]) -> TempDir {
    let td = TempDir::new().unwrap();
    for (rel, body) in files {
        let p = td.path().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }
    td
}

fn parse(files: &[(&str, &str)]) -> (TempDir, Connection) {
    let td = write_tree(files);
    let conn = Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, td.path(), Some("go"), None).unwrap();
    (td, conn)
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE name = ?1",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        > 0
}

/// The file's `_ast.node_hash` multiset, in pre-order.
fn node_hashes(conn: &Connection, rel: &str) -> Vec<Vec<u8>> {
    let fid = leyline_ts::schema::lookup_file_id(conn, rel)
        .unwrap()
        .unwrap_or_else(|| panic!("{rel} was not projected"));
    let (lo, hi) = leyline_ts::schema::file_nid_range(fid);
    let mut stmt = conn
        .prepare("SELECT node_hash FROM _ast WHERE nid BETWEEN ?1 AND ?2 ORDER BY nid")
        .unwrap();
    stmt.query_map([lo, hi], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// Falsifier: re-adding either pointer-store table to the parse.
#[test]
fn a_parse_stores_no_pointer_store_tables() {
    let (_td, conn) = parse(&[("a.go", SHARED), ("b.go", OTHER)]);
    for table in ["capnp_blobs", "_ast_blob"] {
        assert!(
            !table_exists(&conn, table),
            "{table} is retired at projection-v8 and must not be created"
        );
    }
    // Vacuity guard: the parse did project the tree, into the one copy.
    for table in ["_ast", "node_content", "node_child"] {
        let n: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert!(n > 0, "{table} must hold the projected tree");
    }
}

/// ADR-0040 F4: content keys are location-free. A byte-identical file at two
/// different paths — in one arena, and across two arenas — has one
/// `node_hash` sequence, and no per-file blob keyed by its path exists.
/// Falsifier: folding the path (or `file_id`) into `node_hash`.
#[test]
fn a_byte_identical_file_at_two_paths_shares_its_node_hash_set() {
    let (_a, one) = parse(&[("x/a.go", SHARED), ("y/z/copy.go", SHARED), ("o.go", OTHER)]);
    let (_b, two) = parse(&[("elsewhere/b.go", SHARED)]);

    let a = node_hashes(&one, "x/a.go");
    assert!(
        a.len() > 10,
        "fixture must produce a real tree; got {}",
        a.len()
    );
    assert!(
        a.iter().all(|h| h.len() == 32),
        "every _ast row carries node_hash"
    );
    assert_eq!(
        a,
        node_hashes(&one, "y/z/copy.go"),
        "same bytes, two paths, one arena"
    );
    assert_eq!(
        a,
        node_hashes(&two, "elsewhere/b.go"),
        "same bytes, two arenas"
    );
    assert_ne!(
        a,
        node_hashes(&one, "o.go"),
        "different bytes must not collide"
    );

    // The second path adds no stored tree: the arena holding both copies has
    // exactly the node_content and node_child rows of one copy plus o.go.
    let (_c, single) = parse(&[("x/a.go", SHARED), ("o.go", OTHER)]);
    for table in ["node_content", "node_child"] {
        let count = |c: &Connection| -> i64 {
            c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(
            count(&one),
            count(&single),
            "{table}: a byte-identical second path must store nothing new"
        );
    }
}

/// `Head.rootHash` folds the `.ast.capnp` segment; retiring the blob must not
/// retire the segment's content. The segment still carries one canonical
/// `AstNode` record per `_ast` row, with the node's kind and byte span.
/// Falsifier: dropping `serialize_ast_node_record` from the parse along with
/// the blob serializer, which would silently change what rootHash commits to.
#[test]
fn the_segment_root_still_folds_one_ast_node_per_projected_node() {
    let src = write_tree(&[("a.go", SHARED), ("b.go", OTHER)]);
    let out = TempDir::new().unwrap();
    let db = out.path().join("t.db");
    cmd_parse::cmd_parse(src.path(), &db, Some("go")).unwrap();

    let segment = fs::read(out.path().join("t.ast.capnp")).expect("ast segment written");
    let mut slice: &[u8] = &segment;
    let mut records: Vec<(String, u64, u64)> = Vec::new();
    while !slice.is_empty() {
        let msg = capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
            .expect("each segment record decodes");
        let n: ast_node::Reader = msg.get_root().unwrap();
        let r = n.get_range().unwrap();
        records.push((
            n.get_node_kind().unwrap().to_str().unwrap().to_string(),
            r.get_start().unwrap().get_byte(),
            r.get_end().unwrap().get_byte(),
        ));
    }

    let conn = Connection::open(&db).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT k.raw_kind, a.start_byte, a.end_byte FROM _ast a \
             JOIN kinds k ON k.kind_id = a.kind_id ORDER BY a.nid",
        )
        .unwrap();
    let rows: Vec<(String, u64, u64)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)? as u64,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(!rows.is_empty());
    assert_eq!(
        records, rows,
        "the segment is the projected tree, record for record"
    );
    assert!(head_root(&db) != [0u8; 32], "the head names the segments");
}

fn head_root(db: &Path) -> [u8; 32] {
    use leyline_schema_capnp::head_capnp::head;
    let bytes = fs::read(cmd_parse::head_path_for(db)).unwrap();
    let mut slice: &[u8] = &bytes;
    let msg =
        capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new()).unwrap();
    let h: head::Reader = msg.get_root().unwrap();
    h.get_root_hash()
        .unwrap()
        .get_bytes()
        .unwrap()
        .try_into()
        .unwrap()
}
