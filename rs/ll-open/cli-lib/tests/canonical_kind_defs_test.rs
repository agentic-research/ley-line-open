//! Pin: `node_defs.canonical_kind` — mache parity follow-up to bead
//! `ley-line-open-6e798d`.
//!
//! ## Load-bearing claim
//!
//! Every `node_defs` row emitted by LLO's parse pipeline carries the
//! κ canonical kind of the definition it names. That lets consumers
//! filter dead-code / god-file / any-symbol-scope rules by kind
//! (`WHERE canonical_kind IN ('function', 'method', 'type')`) without
//! joining through `node_content.kind`.
//!
//! ## Why this matters (mache regression it prevents)
//!
//! Mache observed `dead_code = 321` on the LLO projection vs the
//! tree-sitter projection's `5`. Root cause: LLO's `node_defs` emits
//! defs for every module-level named binding in Rust (functions,
//! methods, types, mods, consts, statics) while mache-schema only
//! exports "functions/methods/types" as defs. The 321-vs-5 count
//! isn't over-extraction on LLO's side — it's a mache-rule filter
//! that assumed all `node_defs` rows are "symbol-scope defs." With
//! `canonical_kind` populated on every row, the mache rule adds a
//! one-column WHERE and the count collapses back toward parity.

#![cfg(feature = "hdc")]

use leyline_cli_lib::cmd_parse;
use rusqlite::Connection;
use std::fs;
use tempfile::TempDir;

/// Parse the given source under a temp filename and return the DB.
fn parse_go(source: &str, filename: &str) -> Connection {
    let td = TempDir::new().unwrap();
    fs::write(td.path().join(filename), source).unwrap();
    let conn = Connection::open_in_memory().unwrap();
    cmd_parse::parse_into_conn(&conn, td.path(), Some("go"), None).unwrap();
    conn
}

#[test]
fn canonical_kind_populated_for_go_function_and_type() {
    // Load-bearing: a Go file with a function + a method + a type
    // produces `node_defs` rows with canonical_kind set to "function",
    // "method", and "type" respectively.
    let source = "\
package main

type Greeter struct{}

func (g *Greeter) Hello() string { return \"hi\" }

func TopLevel() {}
";
    let conn = parse_go(source, "main.go");
    let mut stmt = conn
        .prepare("SELECT token, canonical_kind FROM node_defs ORDER BY token")
        .unwrap();
    let rows: Vec<(String, Option<String>)> = stmt
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    // Sanity — the fixture emits refs/defs.
    assert!(
        !rows.is_empty(),
        "fixture must emit at least one node_defs row"
    );

    // Every row's canonical_kind is populated (no NULL) AND is a
    // recognized κ kind. If a NULL slips in, mache's rule breaks; if
    // a foreign kind slips in, mache's `IN (...)` filter misses.
    let allowed = [
        "function",
        "method",
        "type",
        "constant",
        "variable",
        "field",
        "module",
        "import",
        "parameter",
    ];
    for (token, kind) in &rows {
        let k = kind
            .as_deref()
            .unwrap_or_else(|| panic!("row {token:?} has NULL canonical_kind"));
        assert!(
            allowed.contains(&k),
            "row {token:?} has canonical_kind {k:?} — not a κ kind (expected one of {allowed:?})",
        );
    }

    // Specific tokens present with expected kinds. These are the ones
    // mache's dead_code rule cares about.
    fn find(rows: &[(String, Option<String>)], token: &str) -> Option<String> {
        rows.iter()
            .find(|(t, _)| t == token)
            .and_then(|(_, k)| k.clone())
    }
    assert_eq!(
        find(&rows, "TopLevel").as_deref(),
        Some("function"),
        "TopLevel is a top-level function → canonical_kind = function",
    );
    assert_eq!(
        find(&rows, "Greeter").as_deref(),
        Some("type"),
        "Greeter is a type → canonical_kind = type",
    );
    // Go method_declaration emits both the qualified (Greeter.Hello)
    // and bare (Hello) forms per bead `caf423`. Both should carry
    // canonical_kind = "method".
    assert_eq!(
        find(&rows, "Hello").as_deref(),
        Some("method"),
        "Hello is a method → canonical_kind = method",
    );
    assert_eq!(
        find(&rows, "Greeter.Hello").as_deref(),
        Some("method"),
        "Greeter.Hello (qualified) is also a method",
    );
}

#[test]
fn canonical_kind_filter_matches_dead_code_use_case() {
    // Mache-shape query: SELECT DISTINCT token FROM node_defs WHERE
    // canonical_kind IN ('function', 'method', 'type'). Returns only
    // symbol-scope defs — this is what mache's dead_code rule wants.
    // With this filter, the 321-vs-5 explosion collapses back toward
    // tree-sitter parity.
    let source = "\
package main

type S struct{}

func (s *S) M() {}

func TopLevel() {}

const K = 42
";
    let conn = parse_go(source, "main.go");

    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT token FROM node_defs \
             WHERE canonical_kind IN ('function', 'method', 'type') \
             ORDER BY token",
        )
        .unwrap();
    let tokens: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    // Should contain TopLevel (function), M (method), S (type).
    // K is a constant — filtered out by the WHERE clause.
    assert!(
        tokens.contains(&"TopLevel".to_string()),
        "TopLevel must survive filter; got {tokens:?}",
    );
    assert!(tokens.contains(&"M".to_string()));
    assert!(tokens.contains(&"S".to_string()));
    assert!(
        !tokens.contains(&"K".to_string()),
        "constant K must be filtered out by canonical_kind IN (function, method, type); \
         got {tokens:?}. This is the mache dead_code parity contract.",
    );
}

#[test]
fn idx_defs_canonical_kind_exists() {
    // The load-bearing index for the mache-shaped query. Pin it —
    // dropping it silently makes the filter O(n) on registry-scale
    // dbs.
    let source = "package main\nfunc F() {}\n";
    let conn = parse_go(source, "main.go");
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master \
             WHERE type='index' AND name='idx_defs_canonical_kind'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        exists,
        "idx_defs_canonical_kind must be created — mache's filter would fall back to O(n) without it",
    );
}
