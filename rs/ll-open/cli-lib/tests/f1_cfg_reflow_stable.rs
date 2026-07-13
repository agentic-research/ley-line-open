//! F1_cfg_reflow_stable — falsifiability gate for the analysis-substrate
//! decade's T1 thread (bead `ley-line-open-46f7d1`).
//!
//! ## Claim (falsifiable)
//!
//! `_cfg` rows produced by the CFG builder are keyed on `node_hash`
//! (ADR-0027 merkle-AST content address), so:
//!
//! - **Reflow-invariance (row count + node_hash + block_kinds).** Two
//!   parses of the same function body, regardless of source formatting
//!   (indentation, blank lines, tabs vs spaces), produce the SAME
//!   count of `_cfg` rows, SAME `node_hash`, and SAME block_kind
//!   sequence. `entry_offset` / `exit_offset` are relative to the
//!   function body but still shift when interior whitespace changes —
//!   they're first-writer-wins convenience for consumers, not part of
//!   the reflow-invariant contract.
//! - **Cross-file dedup on _cfg.** Two files with byte-identical
//!   function bodies produce ONE `_cfg` row set for the shared body
//!   via the `(node_hash, block_id)` PRIMARY KEY + `INSERT OR IGNORE`.
//!   This is the load-bearing dedup story T3's differential-dataflow
//!   `arrange` operator hinges on.
//!
//! ## What breaks this gate
//!
//! - CFG builder's `node_hash` computation leaks whitespace or comment
//!   content (breaks reflow-invariance).
//! - `_cfg` PRIMARY KEY drops `node_hash` and keys on file+line/col
//!   (breaks cross-file dedup).
//! - Walker starts emitting extra spurious blocks under reflow (breaks
//!   row-count identity).
//!
//! Any red = ADR-0024 producer-side thesis is refuted for T3's
//! differential-dataflow `arrange` operator and we regroup before
//! shipping T3.

#![cfg(feature = "hdc")]
// Feature-gate here picks up leyline-ts's `go` + `rust` grammars via
// cli-lib's own feature graph; without a gate this test won't compile
// under the workspace's default feature set.

use leyline_ts::cfg::emit_cfg_for_source;
use leyline_ts::languages::TsLanguage;
use leyline_ts::schema::{
    create_ast_schema, create_cfg_schema, create_ir_tables, create_refs_schema,
};
use rusqlite::Connection;

fn open_prepared_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    create_ast_schema(&conn).unwrap();
    create_refs_schema(&conn).unwrap();
    create_ir_tables(&conn).unwrap();
    create_cfg_schema(&conn).unwrap();
    conn
}

fn cfg_row_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM _cfg", [], |r| r.get(0))
        .unwrap()
}

/// Read (node_hash, block_id, block_kind) tuples in row order. These are
/// the reflow-invariant columns per the F1 contract. Offsets are
/// deliberately excluded — they're first-writer-wins convenience data,
/// not part of the invariant.
fn read_cfg_identity_rows(conn: &Connection) -> Vec<(Vec<u8>, i64, String)> {
    let mut stmt = conn
        .prepare(
            "SELECT node_hash, block_id, block_kind FROM _cfg \
             ORDER BY node_hash, block_id",
        )
        .unwrap();
    stmt.query_map([], |r| {
        Ok((
            r.get::<_, Vec<u8>>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
        ))
    })
    .unwrap()
    .map(|r| r.unwrap())
    .collect()
}

#[test]
fn f1_cfg_rows_survive_go_reflow() {
    // Bead ley-line-open-46f7d1 / decade dataflow-substrate T1.b3.
    //
    // Two source variants of the same function body:
    // - `raw`: cramped, mixed tabs+spaces, extra blank lines
    // - `reflowed`: gofmt-idiomatic single-tab indentation
    //
    // Tree-sitter parses both to the SAME named-node structure (extras
    // like whitespace are absorbed into leaf spans). Therefore the
    // merkle-AST hash of the body is identical, and the SAME
    // (node_hash, block_id, block_kind) triples appear in both parses.
    // Row COUNT is byte-identical (Fable F1 claim); byte offsets shift
    // because interior whitespace changes position, but those are not
    // part of the reflow-invariant contract.
    let raw = concat!(
        "package a\n",
        "\n",
        "func f(x int) int {\n",
        "  if   x > 0   {\n",
        "\n",
        "\t\treturn   x\n",
        "  }\n",
        "\n",
        "     return -x\n",
        "}\n",
    );
    let reflowed = concat!(
        "package a\n",
        "\n",
        "func f(x int) int {\n",
        "\tif x > 0 {\n",
        "\t\treturn x\n",
        "\t}\n",
        "\treturn -x\n",
        "}\n",
    );

    let conn_raw = open_prepared_conn();
    let conn_reflow = open_prepared_conn();
    emit_cfg_for_source(raw.as_bytes(), TsLanguage::Go, "raw.go", &conn_raw).unwrap();
    emit_cfg_for_source(
        reflowed.as_bytes(),
        TsLanguage::Go,
        "reflow.go",
        &conn_reflow,
    )
    .unwrap();

    let rows_raw = read_cfg_identity_rows(&conn_raw);
    let rows_reflow = read_cfg_identity_rows(&conn_reflow);
    assert!(!rows_raw.is_empty(), "raw variant must emit >=1 _cfg row");
    assert_eq!(
        rows_raw.len(),
        rows_reflow.len(),
        "row COUNT must be byte-identical across reflow (raw={} vs reflow={})",
        rows_raw.len(),
        rows_reflow.len(),
    );
    assert_eq!(
        rows_raw, rows_reflow,
        "reflow perturbed the (node_hash, block_id, block_kind) identity — \
         CFG builder's hash or walk order leaks non-structural content."
    );
}

#[test]
fn f1_cfg_rows_dedupe_cross_file_for_identical_bodies() {
    // Bead ley-line-open-46f7d1. Two Go files with byte-identical
    // function bodies — only the surrounding package name and function
    // name differ. Tree-sitter produces the SAME body subtree
    // structure; the merkle-AST hash is identical; the
    // (node_hash, block_id) PRIMARY KEY on `_cfg` collapses them to
    // ONE row set.
    //
    // This is the win T3's differential-dataflow `arrange` operator
    // hinges on: identical bodies → identical tuples → one `arrange`
    // row, not N.
    let file_a = concat!(
        "package a\n",
        "\n",
        "func f(x int) int {\n",
        "\tif x > 0 {\n",
        "\t\treturn x\n",
        "\t}\n",
        "\treturn -x\n",
        "}\n",
    );
    let file_b = concat!(
        "package b\n",
        "\n",
        "// A totally different file with the same shape.\n",
        "\n",
        "func g(x int) int {\n",
        "\tif x > 0 {\n",
        "\t\treturn x\n",
        "\t}\n",
        "\treturn -x\n",
        "}\n",
    );

    let conn = open_prepared_conn();
    // File A first, file B second, into the SAME connection — the
    // (node_hash, block_id) PK must dedup B's identical body rows.
    emit_cfg_for_source(file_a.as_bytes(), TsLanguage::Go, "a.go", &conn).unwrap();
    let after_a = cfg_row_count(&conn);
    emit_cfg_for_source(file_b.as_bytes(), TsLanguage::Go, "b.go", &conn).unwrap();
    let after_b = cfg_row_count(&conn);

    assert!(after_a >= 1, "file A must emit >=1 _cfg row");
    assert_eq!(
        after_a, after_b,
        "byte-identical function bodies MUST dedupe to ONE _cfg row set \
         (after_a={after_a}, after_b={after_b}); PRIMARY KEY (node_hash, block_id) \
         is not doing its job."
    );

    // The first-writer's source_id wins under INSERT OR IGNORE. Pin
    // this — a refactor that switches to INSERT OR REPLACE would
    // silently flip the semantics and would not be caught by
    // count-based tests.
    let first_source_id: String = conn
        .query_row(
            "SELECT source_id FROM _cfg ORDER BY block_id LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        first_source_id, "a.go",
        "first-writer's source_id must win under INSERT OR IGNORE",
    );
}
