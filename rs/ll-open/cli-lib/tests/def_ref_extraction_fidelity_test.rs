//! Def/ref extraction fidelity gates (bead `ley-line-open-caf423`).
//!
//! Reproduces three fidelity gaps mache surfaced while investigating
//! cross-language smell false-positives (mache bead `22fecf`). LLO's own
//! tests missed each — the fixtures here MUST fail against the pre-fix
//! producer, and pass once the extractor is fixed.
//!
//! 1. **Python + JavaScript produce zero symbols.** `parse_into_conn`
//!    runs the tree-sitter pass but the language-dispatched `extract_refs`
//!    factory has no arm for `TsLanguage::Python` / `TsLanguage::JavaScript`,
//!    so every Python or JS file writes zero `node_defs` / `node_refs`
//!    rows despite parsing successfully.
//!
//! 2. **Method tokens unqualified.** Go `method_declaration` and Rust
//!    `impl` methods emit the bare method name (`Validate`, `foo`)
//!    instead of the qualified form (`Server.Validate`, `S::foo`) that
//!    mache's cross-language rules need to disambiguate methods on
//!    different receivers.
//!
//! 3. **`nodes.source_file` empty.** The parse pipeline's `INSERT INTO
//!    nodes` never populates the `source_file` column, so mache's cross-
//!    language rules join on a null column and produce false positives.
//!
//! The tests below use `parse_into_conn` end-to-end (not the pure
//! `extract_refs` factory) so a regression anywhere along the pipeline
//! — grammar wiring, language dispatch, batch insert — trips the gate.

use std::fs;
use std::path::Path;

use rusqlite::Connection;
use tempfile::TempDir;

/// Cold-parse a source directory with `parse_into_conn`, returning the
/// populated in-memory connection. `lang` filters the parse to a single
/// language when supplied (mirrors `cmd_parse --lang <foo>`).
fn cold_parse(src_dir: &Path, lang: Option<&str>) -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    leyline_cli_lib::cmd_parse::parse_into_conn(&conn, src_dir, lang, None)
        .expect("parse_into_conn must succeed");
    conn
}

fn count_rows(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get::<_, i64>(0))
        .expect("count query")
}

fn defs_tokens(conn: &Connection) -> Vec<String> {
    conn.prepare("SELECT token FROM node_defs ORDER BY token")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

// ── Bug 1a: Python def/ref rows ─────────────────────────────────────────

#[test]
fn python_files_produce_def_ref_rows() {
    // Minimal fixture that any working Python extractor MUST emit rows
    // for: two module-level function definitions, one class, one call.
    // The pre-fix producer walks the AST + writes `_ast` rows but leaves
    // both `node_defs` and `node_refs` empty because `extract_refs` has
    // no arm for `TsLanguage::Python`.
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("mod.py"),
        b"def foo():\n    return 1\n\ndef bar():\n    return foo()\n\nclass C:\n    def method(self):\n        pass\n",
    )
    .unwrap();

    let conn = cold_parse(dir.path(), Some("python"));

    // Sanity: the file did parse (else the failure below is misleading).
    let ast_rows = count_rows(
        &conn,
        "SELECT COUNT(*) FROM _ast WHERE source_id = 'mod.py'",
    );
    assert!(
        ast_rows > 0,
        "Python file must produce _ast rows (parse pipeline sanity); got {ast_rows}"
    );

    let def_rows = count_rows(
        &conn,
        "SELECT COUNT(*) FROM node_defs WHERE source_id = 'mod.py'",
    );
    assert!(
        def_rows > 0,
        "Python file must produce node_defs rows (foo, bar, C, method); got {def_rows}. \
         Symptom: extract_refs dispatcher has no Python arm."
    );

    let ref_rows = count_rows(
        &conn,
        "SELECT COUNT(*) FROM node_refs WHERE source_id = 'mod.py'",
    );
    assert!(
        ref_rows > 0,
        "Python file must produce node_refs rows (the foo() call); got {ref_rows}"
    );

    // Concrete tokens: the def set MUST include the module-level names.
    let defs = defs_tokens(&conn);
    for want in ["foo", "bar", "C"] {
        assert!(
            defs.contains(&want.to_string()),
            "missing Python def token {want:?} in {defs:?}"
        );
    }
}

// ── Bug 1b: JavaScript def/ref rows ─────────────────────────────────────

#[test]
fn javascript_files_produce_def_ref_rows() {
    // Minimal fixture: two function decls, one class, and a call site.
    // Pre-fix behavior: LLO had no JavaScript pipeline at all — no
    // `TsLanguage::JavaScript` variant, no `.js` → language mapping, no
    // extractor. Post-fix: js parses + produces def/ref rows just like
    // Go/Rust/Python.
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("mod.js"),
        b"function foo() { return 1; }\nfunction bar() { return foo(); }\nclass C {\n  method() { return 2; }\n}\n",
    )
    .unwrap();

    let conn = cold_parse(dir.path(), Some("javascript"));

    let ast_rows = count_rows(
        &conn,
        "SELECT COUNT(*) FROM _ast WHERE source_id = 'mod.js'",
    );
    assert!(
        ast_rows > 0,
        "JS file must produce _ast rows (parse pipeline sanity); got {ast_rows}"
    );

    let def_rows = count_rows(
        &conn,
        "SELECT COUNT(*) FROM node_defs WHERE source_id = 'mod.js'",
    );
    assert!(
        def_rows > 0,
        "JS file must produce node_defs rows (foo, bar, C); got {def_rows}"
    );

    let ref_rows = count_rows(
        &conn,
        "SELECT COUNT(*) FROM node_refs WHERE source_id = 'mod.js'",
    );
    assert!(
        ref_rows > 0,
        "JS file must produce node_refs rows (foo() call); got {ref_rows}"
    );

    let defs = defs_tokens(&conn);
    for want in ["foo", "bar", "C"] {
        assert!(
            defs.contains(&want.to_string()),
            "missing JS def token {want:?} in {defs:?}"
        );
    }
}

// ── Bug 2: qualified method tokens ──────────────────────────────────────

#[test]
fn method_tokens_are_qualified_go() {
    // Go: `func (s *Server) Validate() {}` — pre-fix emits def token
    // "Validate" (bare). Cross-file resolution can't distinguish
    // `Server.Validate` from `Client.Validate` from a stray package
    // function called `Validate`. The fix emits the qualified form
    // `Server.Validate` alongside the bare `Validate` (paired with the
    // matching call-side qualification the existing `selector_expression`
    // arm already emits).
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("main.go"),
        b"package main\n\ntype Server struct{}\n\nfunc (s *Server) Validate() {}\n",
    )
    .unwrap();

    let conn = cold_parse(dir.path(), Some("go"));

    let defs = defs_tokens(&conn);
    // Qualified form is the load-bearing add — that's what mache joins on.
    assert!(
        defs.contains(&"Server.Validate".to_string()),
        "Go method def token must be qualified as `Server.Validate`; got {defs:?}"
    );
    // Bare form remains so `Validate()` call sites still resolve.
    assert!(
        defs.contains(&"Validate".to_string()),
        "bare method name must remain in defs for call-side compatibility; got {defs:?}"
    );
}

#[test]
fn method_tokens_are_qualified_rust() {
    // Rust: `impl S { fn foo(&self) {} }` — pre-fix emits def token
    // `foo` (bare) because tree-sitter-rust doesn't distinguish
    // `function_item` inside an `impl_item` at the node level. Fix walks
    // the parent chain to reach the impl's `type` field and emits
    // `S::foo` alongside `foo`.
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("lib.rs"),
        b"pub struct S;\nimpl S {\n    pub fn foo(&self) {}\n}\n",
    )
    .unwrap();

    let conn = cold_parse(dir.path(), Some("rust"));

    let defs = defs_tokens(&conn);
    assert!(
        defs.contains(&"S::foo".to_string()),
        "Rust method def token must be qualified as `S::foo`; got {defs:?}"
    );
    assert!(
        defs.contains(&"foo".to_string()),
        "bare method name must remain in defs for call-side compatibility; got {defs:?}"
    );
}

// ── Bug 3: nodes.source_file populated ──────────────────────────────────

#[test]
fn nodes_source_file_is_populated() {
    // The `nodes` INSERT in cmd_parse never populated `source_file`, so
    // mache's cross-language rules that JOIN on `nodes.source_file`
    // silently reduced to false positives. Pin: after a parse, every
    // node that belongs to a source file (the file node itself or any
    // AST descendant of it) MUST have `source_file` populated to that
    // file's `_source.id`. Directory-only nodes (kind=1 with no
    // source_id) may still be NULL.
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("main.go"),
        b"package main\n\nfunc Add(a, b int) int {\n\treturn a + b\n}\n",
    )
    .unwrap();

    let conn = cold_parse(dir.path(), Some("go"));

    // File-owned nodes: id equals a `_source.id` OR is prefixed by one
    // (id LIKE '<source_id>/%'). Every such node MUST carry the source
    // file's path.
    let unpopulated: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM nodes n \
             WHERE EXISTS (SELECT 1 FROM _source s \
                           WHERE n.id = s.id OR n.id LIKE s.id || '/%') \
               AND (n.source_file IS NULL OR n.source_file = '')",
            [],
            |r| r.get::<_, i64>(0),
        )
        .expect("count query");

    assert_eq!(
        unpopulated, 0,
        "every node in a parsed file must have source_file populated; \
         {unpopulated} row(s) missing"
    );

    // Concrete file-level pin: the root file node must carry its own
    // path as source_file (mache uses this as the join key).
    let file_source_file: Option<String> = conn
        .query_row(
            "SELECT source_file FROM nodes WHERE id = 'main.go'",
            [],
            |r| r.get::<_, Option<String>>(0),
        )
        .expect("main.go node must exist");
    assert_eq!(
        file_source_file.as_deref(),
        Some("main.go"),
        "file node's source_file must equal its own path"
    );
}
