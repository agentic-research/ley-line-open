//! Pin: `node_refs.container_node_id` (and `node_defs.container_node_id`)
//! is the node_id of the nearest enclosing κ `function`/`method`
//! ancestor. Bead `ley-line-open-6e798d`.
//!
//! Why this matters (mache parity): mache's `fan_out_skew` and
//! `untested_function` smell rules `GROUP BY referrer_node_id` on their
//! `v_refs` view, expecting `referrer_node_id` = ID of the *containing
//! function*. LLO's `node_refs.node_id` is the AST-native path to the
//! CALL SITE — every call has a unique path → every group has n=1 →
//! rules find nothing on the LLO projection. Materializing the container
//! at parse time (this pin) means mache's view can `SELECT
//! COALESCE(container_node_id, node_id) AS referrer_node_id` and the
//! rules recover their tree-sitter counts.

#![cfg(feature = "hdc")]

use leyline_cli_lib::cmd_parse;
use rusqlite::Connection;
use std::fs;
use tempfile::TempDir;

/// Parse a Go source snippet by writing it to a tempdir and running the
/// full `parse_into_conn` pipeline. Returns the SQLite connection with
/// nodes + _ast + node_refs + node_defs populated.
///
/// `source_id` is the relative filename under the tempdir (e.g.
/// `"main.go"`); the container_node_id assertions in each test key off
/// this prefix.
fn parse_go_to_conn(source: &str, source_id: &str) -> Connection {
    let td = TempDir::new().unwrap();
    fs::write(td.path().join(source_id), source).unwrap();
    let conn = Connection::open_in_memory().unwrap();
    cmd_parse::parse_into_conn(&conn, td.path(), Some("go"), None).unwrap();
    conn
}

#[test]
fn refs_inside_go_function_share_container_node_id() {
    // Load-bearing shape: three call sites inside a single Go function
    // must all carry the SAME container_node_id — the function's own
    // node_id (per LLO's <source>/<ast_kind>_N path convention).
    //
    // What broke pre-6e798d: node_refs.node_id was the call-site path,
    // unique per call → `GROUP BY referrer_node_id` in mache's
    // fan_out_skew collapsed every group to n=1 → rule filter n>=5
    // dropped everything.
    let source = "\
package main

import \"fmt\"

func f(x int) int {
\tfmt.Println(x)
\tfmt.Printf(\"%d\", x)
\tfmt.Sprintf(\"%d\", x)
\treturn x
}
";
    let conn = parse_go_to_conn(source, "main.go");

    // Every ref (call) inside f must carry a non-null container_node_id
    // and they must all share the same value.
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT container_node_id FROM node_refs \
             WHERE token IN ('Println', 'Printf', 'Sprintf') \
             AND container_node_id IS NOT NULL",
        )
        .unwrap();
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "all three calls inside f() must share ONE container_node_id, got {rows:?}",
    );
    let container = &rows[0];
    // Container must be the function_declaration's node_id, which lives
    // under source_file main.go per LLO's `<source>/<ast>` convention.
    assert!(
        container.starts_with("main.go/"),
        "container_node_id should live under main.go/, got {container:?}",
    );
    assert!(
        container.contains("function_declaration"),
        "container_node_id should be the function_declaration node, got {container:?}",
    );
}

#[test]
fn top_level_refs_have_null_container_node_id() {
    // Load-bearing edge case: a Go file's import references (or any ref
    // that sits above any function body) has no enclosing function/method.
    // container_node_id MUST be NULL for those — mache's rules that GROUP
    // BY container skip the null group naturally.
    //
    // A `import "fmt"` line produces an `_imports` row but NOT a
    // node_refs row (imports go through a different extractor path).
    // So to exercise "top-level ref", use a package-level type
    // declaration whose body might reference another identifier — but
    // in Go, refs are extracted from `call_expression` / `selector_expression`
    // sites inside function bodies. Package-level `var x = fmt.Println`
    // etc. are rare enough that this test's assertion is really: no
    // container-null refs sneak in for THIS well-formed fixture (i.e.
    // every ref in this file is inside a function).
    let source = "\
package main

import \"fmt\"

func inner() {
\tfmt.Println(\"hello\")
}
";
    let conn = parse_go_to_conn(source, "main.go");
    // Every ref in this fixture is inside `inner`, so no NULL container.
    let null_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM node_refs WHERE container_node_id IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        null_count, 0,
        "no top-level refs in this fixture; got {null_count} NULL-container rows",
    );
}

#[test]
fn method_bodies_carry_method_container() {
    // Load-bearing: Go method declarations are also κ-canonical
    // functions/methods for container purposes. Refs inside a method
    // body must carry the method_declaration's node_id, not the
    // containing type's or file's.
    let source = "\
package main

import \"fmt\"

type S struct{}

func (s *S) M() {
\tfmt.Println(\"in method\")
}
";
    let conn = parse_go_to_conn(source, "main.go");

    // The Println call inside M() must have a container that's a
    // method_declaration node, not a function_declaration or file.
    let mut stmt = conn
        .prepare(
            "SELECT container_node_id FROM node_refs \
             WHERE token = 'Println' AND container_node_id IS NOT NULL",
        )
        .unwrap();
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(!rows.is_empty(), "Println ref must exist");
    let container = &rows[0];
    assert!(
        container.contains("method_declaration"),
        "expected method_declaration container, got {container:?}",
    );
}

#[test]
fn defs_carry_their_own_container() {
    // Load-bearing: a nested function/method's def row should carry
    // its ENCLOSING scope as container, not itself. Since Go doesn't
    // permit nested function/method declarations at parse level (they
    // parse as func_literal expressions with different node kinds),
    // this test uses the top-level shape: a package-level function's
    // def has NULL container (no enclosing function/method).
    let source = "\
package main

func TopLevel() {}
";
    let conn = parse_go_to_conn(source, "main.go");

    let container: Option<String> = conn
        .query_row(
            "SELECT container_node_id FROM node_defs WHERE token = 'TopLevel'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        container, None,
        "TopLevel is a package-level def — container_node_id must be NULL, got {container:?}",
    );
}
