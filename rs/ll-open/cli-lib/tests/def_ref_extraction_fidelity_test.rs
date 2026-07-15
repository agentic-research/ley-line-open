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

// ── Bug 1c: TypeScript def/ref rows ─────────────────────────────────────

#[test]
fn typescript_files_produce_def_ref_rows() {
    // Same bug class as Python + JS — TS files parse via leyline-fs's
    // validate pass but LLO's producer had no TypeScript arm, so every
    // `.ts` / `.tsx` file wrote zero def/ref rows. Fixture uses the
    // TS-specific constructs (`interface`, `type` alias) plus the
    // JS-shared ones (function, class, method, call, import) so the
    // extraction path exercises both the ported-from-JS arms and the
    // new TS-only arms.
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("mod.ts"),
        b"import { helper } from \"./util\";\n\
          interface Shape { area(): number; }\n\
          type Point = { x: number; y: number };\n\
          function foo(): number { return 1; }\n\
          function bar(): number { return foo() + helper(); }\n\
          class C {\n\
            method(): number { return 2; }\n\
          }\n",
    )
    .unwrap();

    let conn = cold_parse(dir.path(), Some("typescript"));

    let ast_rows = count_rows(
        &conn,
        "SELECT COUNT(*) FROM _ast WHERE source_id = 'mod.ts'",
    );
    assert!(
        ast_rows > 0,
        "TS file must produce _ast rows (parse pipeline sanity); got {ast_rows}"
    );

    let def_rows = count_rows(
        &conn,
        "SELECT COUNT(*) FROM node_defs WHERE source_id = 'mod.ts'",
    );
    assert!(
        def_rows > 0,
        "TS file must produce node_defs rows (foo, bar, C, method, Shape, Point); got {def_rows}. \
         Symptom: extract_refs dispatcher has no TypeScript arm."
    );

    let ref_rows = count_rows(
        &conn,
        "SELECT COUNT(*) FROM node_refs WHERE source_id = 'mod.ts'",
    );
    assert!(
        ref_rows > 0,
        "TS file must produce node_refs rows (foo() and helper() calls); got {ref_rows}"
    );

    let defs = defs_tokens(&conn);
    // JS-shared: function + class defs.
    for want in ["foo", "bar", "C"] {
        assert!(
            defs.contains(&want.to_string()),
            "missing TS def token {want:?} in {defs:?}"
        );
    }
    // TS-specific: interface + type alias defs.
    for want in ["Shape", "Point"] {
        assert!(
            defs.contains(&want.to_string()),
            "missing TS-only def token {want:?} in {defs:?} — \
             interface / type_alias arm not wired"
        );
    }
    // Qualified method form: same discipline as Python / JS / Rust —
    // methods emit both `Class.method` and bare `method` so mache's
    // cross-language rules can disambiguate methods on different
    // classes.
    assert!(
        defs.contains(&"C.method".to_string()),
        "TS method def token must be qualified as `C.method`; got {defs:?}"
    );
    assert!(
        defs.contains(&"method".to_string()),
        "bare TS method name must remain in defs for call-side compatibility; got {defs:?}"
    );
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

// ── Post-PR-165 follow-up: two silent-empty gaps the reviewer flagged ───
//
// PR #165 closed the "supported but silent-empty" bug class for Python/JS/TS
// extraction and for qualified impl-method tokens. Two more instances of
// the same class survived because they weren't in the original repro set:
//
// 1. Rust trait default methods (`trait T { fn m(&self) {} }`) did NOT
//    emit the qualified `T::m` form even though the `extract_rust`
//    docstring explicitly promised they would. `rust_impl_receiver`
//    hard-failed unless the parent was `impl_item`, so `trait_item` fell
//    through and only the bare `m` shipped.
//
// 2. JS/TS variable bindings to arrow / function expressions
//    (`const foo = () => 1;`) produced ZERO defs even though the
//    `extract_javascript` docstring promised they'd emit a Def. The
//    match had no `lexical_declaration` / `variable_declaration` arm —
//    huge modern JS/TS surface silently dropped.
//
// Both are pinned below with the same fixture-first + explicit-token
// discipline as the pre-existing tests.

#[test]
fn rust_trait_default_methods_are_qualified() {
    // `trait Greet { fn hello(&self) {} }` — pre-fix emits only bare
    // `hello` because `rust_impl_receiver` refused to walk anything but
    // `impl_item`. Fix accepts `trait_item` too and reads the trait's
    // `name` field. Post-fix defs include `Greet::hello` alongside the
    // bare `hello`, matching the `extract_rust` docstring's claim.
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("lib.rs"),
        b"pub trait Greet {\n    fn hello(&self) {}\n}\n",
    )
    .unwrap();

    let conn = cold_parse(dir.path(), Some("rust"));

    let defs = defs_tokens(&conn);
    assert!(
        defs.contains(&"Greet::hello".to_string()),
        "Rust trait default method must qualify as `Greet::hello`; got {defs:?}"
    );
    assert!(
        defs.contains(&"hello".to_string()),
        "bare trait method name must remain in defs for call-side compatibility; got {defs:?}"
    );
    // The trait itself is also a def.
    assert!(
        defs.contains(&"Greet".to_string()),
        "trait def token must be present; got {defs:?}"
    );
}

#[test]
fn javascript_arrow_and_function_expression_bindings_extract_as_defs() {
    // `const foo = () => 1; const bar = function () {};` — pre-fix emits
    // zero defs because `extract_javascript` had no
    // `lexical_declaration` / `variable_declaration` arm. Fix walks each
    // `variable_declarator` and emits a Def when the initializer is an
    // `arrow_function` or `function_expression`.
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("mod.js"),
        b"const foo = () => 1;\nconst bar = function () { return 2; };\nlet baz = async () => 3;\nvar qux = function named() {};\n",
    )
    .unwrap();

    let conn = cold_parse(dir.path(), Some("javascript"));

    let defs = defs_tokens(&conn);
    for want in ["foo", "bar", "baz", "qux"] {
        assert!(
            defs.contains(&want.to_string()),
            "JS arrow/function-expression var binding {want:?} must emit a Def; \
             got {defs:?}. Symptom: extract_javascript lacked a \
             lexical_declaration / variable_declaration arm."
        );
    }
}

// ── Tier 3 query-native languages (bead ley-line-open-5e21c2) ───────────
//
// Java, C, and C++ registered Tier 1+2 (parse + validate, bead 46ae48)
// with NO extractor: `extract_refs` had no dispatch arm and
// `canonical_kind` returned None, so every .java/.c/.h/.cpp file wrote
// zero node_defs / node_refs / _imports rows. Same "supported but
// silent-empty" bug class as Python/JS/TS pre-PR-165 — pinned with the
// same fixture-first + explicit-token discipline. These are the first
// languages whose extraction is authored as .scm query data from day
// one (no imperative extractor ever existed).

#[test]
fn java_files_produce_def_ref_rows() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("Main.java"),
        b"import java.util.List;\n\npublic class Main {\n    static int foo() { return 1; }\n    int bar() { return foo(); }\n}\n",
    )
    .unwrap();

    let conn = cold_parse(dir.path(), Some("java"));

    let ast_rows = count_rows(
        &conn,
        "SELECT COUNT(*) FROM _ast WHERE source_id = 'Main.java'",
    );
    assert!(
        ast_rows > 0,
        "Java file must produce _ast rows (parse pipeline sanity); got {ast_rows}"
    );

    let def_rows = count_rows(
        &conn,
        "SELECT COUNT(*) FROM node_defs WHERE source_id = 'Main.java'",
    );
    assert!(
        def_rows > 0,
        "Java file must produce node_defs rows (Main, foo, bar); got {def_rows}. \
         Symptom: extract_refs dispatcher has no Java arm."
    );

    let ref_rows = count_rows(
        &conn,
        "SELECT COUNT(*) FROM node_refs WHERE source_id = 'Main.java'",
    );
    assert!(
        ref_rows > 0,
        "Java file must produce node_refs rows (the foo() call); got {ref_rows}"
    );

    let defs = defs_tokens(&conn);
    for want in ["Main", "foo", "bar"] {
        assert!(
            defs.contains(&want.to_string()),
            "missing Java def token {want:?} in {defs:?}"
        );
    }
    // Same qualified-method discipline as Go / Rust / Python / JS / TS.
    assert!(
        defs.contains(&"Main.foo".to_string()),
        "Java method def token must be qualified as `Main.foo`; got {defs:?}"
    );

    let imports: Vec<(String, String)> = conn
        .prepare("SELECT alias, path FROM _imports ORDER BY path")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(
        imports.contains(&("List".to_string(), "java.util.List".to_string())),
        "Java import must land in _imports as (List, java.util.List); got {imports:?}"
    );
}

#[test]
fn c_files_produce_def_ref_rows() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("mod.c"),
        b"#include <stdio.h>\n\nstruct point { int x; int y; };\n\nint foo(void) { return 1; }\n\nint bar(void) { return foo(); }\n",
    )
    .unwrap();

    let conn = cold_parse(dir.path(), Some("c"));

    let ast_rows = count_rows(&conn, "SELECT COUNT(*) FROM _ast WHERE source_id = 'mod.c'");
    assert!(
        ast_rows > 0,
        "C file must produce _ast rows (parse pipeline sanity); got {ast_rows}"
    );

    let def_rows = count_rows(
        &conn,
        "SELECT COUNT(*) FROM node_defs WHERE source_id = 'mod.c'",
    );
    assert!(
        def_rows > 0,
        "C file must produce node_defs rows (foo, bar, point); got {def_rows}. \
         Symptom: extract_refs dispatcher has no C arm."
    );

    let ref_rows = count_rows(
        &conn,
        "SELECT COUNT(*) FROM node_refs WHERE source_id = 'mod.c'",
    );
    assert!(
        ref_rows > 0,
        "C file must produce node_refs rows (the foo() call); got {ref_rows}"
    );

    let defs = defs_tokens(&conn);
    for want in ["foo", "bar", "point"] {
        assert!(
            defs.contains(&want.to_string()),
            "missing C def token {want:?} in {defs:?}"
        );
    }

    // The include lands with the angle brackets STRIPPED — `<stdio.h>`
    // is the node text; the emitted path is `stdio.h`.
    let imports: Vec<(String, String)> = conn
        .prepare("SELECT alias, path FROM _imports ORDER BY path")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(
        imports.contains(&("stdio.h".to_string(), "stdio.h".to_string())),
        "C system include must land bracket-stripped as (stdio.h, stdio.h); got {imports:?}"
    );
}

#[test]
fn cpp_files_produce_def_ref_rows() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("mod.cpp"),
        b"#include <vector>\n\nclass Shape {\npublic:\n    double area();\n    void draw() {}\n};\n\ndouble Shape::area() { return 0; }\n\nvoid render() { Shape s; s.draw(); }\n",
    )
    .unwrap();

    let conn = cold_parse(dir.path(), Some("cpp"));

    let ast_rows = count_rows(
        &conn,
        "SELECT COUNT(*) FROM _ast WHERE source_id = 'mod.cpp'",
    );
    assert!(
        ast_rows > 0,
        "C++ file must produce _ast rows (parse pipeline sanity); got {ast_rows}"
    );

    let def_rows = count_rows(
        &conn,
        "SELECT COUNT(*) FROM node_defs WHERE source_id = 'mod.cpp'",
    );
    assert!(
        def_rows > 0,
        "C++ file must produce node_defs rows (Shape, area, draw, render); got {def_rows}. \
         Symptom: extract_refs dispatcher has no Cpp arm."
    );

    let ref_rows = count_rows(
        &conn,
        "SELECT COUNT(*) FROM node_refs WHERE source_id = 'mod.cpp'",
    );
    assert!(
        ref_rows > 0,
        "C++ file must produce node_refs rows (the s.draw() call); got {ref_rows}"
    );

    let defs = defs_tokens(&conn);
    for want in ["Shape", "render", "draw", "area"] {
        assert!(
            defs.contains(&want.to_string()),
            "missing C++ def token {want:?} in {defs:?}"
        );
    }
    // Qualified forms: the out-of-line definition qualifies via query
    // dual-emit; the in-class ones via the ancestor fixup. Same `::`
    // spelling as Rust impl methods.
    assert!(
        defs.contains(&"Shape::area".to_string()),
        "C++ out-of-line method def must be qualified as `Shape::area`; got {defs:?}"
    );
    assert!(
        defs.contains(&"Shape::draw".to_string()),
        "C++ in-class method def must be qualified as `Shape::draw`; got {defs:?}"
    );
}

#[test]
fn typescript_arrow_and_function_expression_bindings_extract_as_defs() {
    // Same shape as the JS test; TypeScript grammar shares the node
    // kinds so the shared helper covers both. Fixture uses typed
    // annotations that pre-fix would still drop entirely.
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("mod.ts"),
        b"const foo = (): number => 1;\nconst bar = function (): number { return 2; };\nlet baz: () => Promise<number> = async () => 3;\n",
    )
    .unwrap();

    let conn = cold_parse(dir.path(), Some("typescript"));

    let defs = defs_tokens(&conn);
    for want in ["foo", "bar", "baz"] {
        assert!(
            defs.contains(&want.to_string()),
            "TS arrow/function-expression var binding {want:?} must emit a Def; \
             got {defs:?}. Symptom: extract_typescript lacked a \
             lexical_declaration / variable_declaration arm."
        );
    }
}
