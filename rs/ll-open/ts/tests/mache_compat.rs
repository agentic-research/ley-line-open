//! Integration test: produce a .db that mache can consume.
//! Validates the ley-line → mache contract: nodes + _ast + _source tables.

use rusqlite::Connection;

#[test]
fn produce_go_fixture_for_mache() {
    let out = "/tmp/llo-go-fixture.db";
    let _ = std::fs::remove_file(out);
    let conn = Connection::open(out).unwrap();

    let src = b"package main\n\nimport \"fmt\"\n\nfunc Validate(x int) error {\n\tif x <= 0 {\n\t\treturn fmt.Errorf(\"invalid: %d\", x)\n\t}\n\treturn nil\n}\n\nfunc Helper() string {\n\treturn \"hello\"\n}\n\ntype Config struct {\n\tName string\n}\n";

    leyline_ts::project::project_ast_with_source(
        src,
        leyline_ts::languages::TsLanguage::Go.ts_language(),
        &conn,
        "main.go",
        "go",
    )
    .unwrap();

    let node_count: i64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0)).unwrap();
    let ast_count: i64 = conn.query_row("SELECT COUNT(*) FROM _ast", [], |r| r.get(0)).unwrap();
    let src_count: i64 = conn.query_row("SELECT COUNT(*) FROM _source", [], |r| r.get(0)).unwrap();

    println!("Produced {out}: nodes={node_count}, _ast={ast_count}, _source={src_count}");
    assert!(node_count > 10, "should have nodes for functions/types/identifiers");
    assert!(ast_count > 5, "should have AST byte-range entries");
    assert_eq!(src_count, 1, "one source file");

    // Verify node_kind values that mache's ASTWalker queries
    let fn_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM _ast WHERE node_kind = 'function_declaration'",
        [], |r| r.get(0),
    ).unwrap();
    assert!(fn_count >= 2, "should have 2+ function_declaration AST entries, got {fn_count}");

    let id_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM _ast WHERE node_kind = 'identifier'",
        [], |r| r.get(0),
    ).unwrap();
    assert!(id_count >= 2, "should have identifier AST entries, got {id_count}");

    println!("  function_declarations={fn_count}, identifiers={id_count}");
}
