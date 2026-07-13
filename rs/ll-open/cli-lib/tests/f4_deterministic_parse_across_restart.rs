//! F4_deterministic_parse_across_restart — falsifiability gate for
//! parse determinism across daemon restart (bead `ley-line-open-c7d00f`).
//!
//! ## Claim
//!
//! Two cold parses of the same source directory produce byte-identical
//! `_ast` / `node_defs` / `node_refs` / `node_content` row counts and
//! `nodes` counts. This is the invariant mache's find-smells baseline
//! depends on — if parse output jitters, smell counts jitter, and
//! consumers can't tell "real drift" from "cache pollution artifact."
//!
//! ## What breaks this gate
//!
//! - Non-deterministic ordering in the parse pipeline (rayon race
//!   surface, unsorted HashMap iteration order affecting insert
//!   order, etc.).
//! - `_meta` row order variance (should be irrelevant to counts, but
//!   pins the shape).
//! - Any lazy-init pass that reads env / clock / random for structural
//!   decisions.
//!
//! ## What this gate is NOT
//!
//! - Not a determinism gate for `_hdc` (hyperdimensional
//!   fingerprints). Those are content-addressed and stable, but their
//!   population is opt-in and out of scope for the smell baseline.
//! - Not a determinism gate for `_lsp*` (LSP enrichment). LSP is
//!   external-process; determinism there is bounded by the language
//!   server's own guarantees.

#![cfg(feature = "hdc")]

use leyline_cli_lib::cmd_parse;
use rusqlite::Connection;
use std::fs;
use tempfile::TempDir;

/// Set up a repo fixture with a small but structurally interesting
/// Go corpus — enough to exercise func/method/type/import extraction
/// and the container_node_id ancestor walk. The corpus is
/// intentionally small so this test stays fast; it's about
/// determinism, not scale.
fn fixture_repo() -> TempDir {
    let td = TempDir::new().unwrap();
    fs::write(
        td.path().join("main.go"),
        "\
package main

import (
\t\"fmt\"
\t\"strings\"
)

type Greeter struct {
\tName string
}

func (g *Greeter) Hello() string {
\treturn fmt.Sprintf(\"Hello, %s!\", g.Name)
}

func normalizeName(name string) string {
\treturn strings.TrimSpace(name)
}

func main() {
\tg := &Greeter{Name: \"world\"}
\tfmt.Println(g.Hello())
\t_ = normalizeName(\"  hi  \")
}
",
    )
    .unwrap();
    fs::write(
        td.path().join("util.go"),
        "\
package main

import \"strings\"

func upper(s string) string {
\treturn strings.ToUpper(s)
}

func lower(s string) string {
\treturn strings.ToLower(s)
}
",
    )
    .unwrap();
    td
}

fn parse_repo_and_count(td: &TempDir) -> RowCounts {
    let conn = Connection::open_in_memory().unwrap();
    cmd_parse::parse_into_conn(&conn, td.path(), Some("go"), None).unwrap();
    RowCounts {
        nodes: count(&conn, "SELECT COUNT(*) FROM nodes"),
        ast: count(&conn, "SELECT COUNT(*) FROM _ast"),
        source: count(&conn, "SELECT COUNT(*) FROM _source"),
        refs: count(&conn, "SELECT COUNT(*) FROM node_refs"),
        defs: count(&conn, "SELECT COUNT(*) FROM node_defs"),
        content: count(&conn, "SELECT COUNT(*) FROM node_content"),
        refs_with_container: count(
            &conn,
            "SELECT COUNT(*) FROM node_refs WHERE container_node_id IS NOT NULL",
        ),
    }
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

#[derive(Debug, PartialEq, Eq)]
struct RowCounts {
    nodes: i64,
    ast: i64,
    source: i64,
    refs: i64,
    defs: i64,
    content: i64,
    refs_with_container: i64,
}

#[test]
fn f4_two_cold_parses_produce_identical_row_counts() {
    // The load-bearing invariant: cold-parse repo R twice; assert
    // every count matches. Any jitter here means downstream smell
    // gates jitter — which is the exact class of non-determinism
    // mache observed (dead_code changing between identical builds).
    let td = fixture_repo();
    let a = parse_repo_and_count(&td);
    let b = parse_repo_and_count(&td);
    assert_eq!(
        a, b,
        "cold-parse of the same repo must produce identical row counts on every table; \
         any drift here means downstream smell gates will jitter (bead ley-line-open-c7d00f)",
    );
    // Sanity — the fixture actually produces meaningful counts, so
    // "0 == 0" doesn't accidentally pass this gate.
    assert!(a.nodes > 5, "fixture must produce >5 nodes; got {a:?}");
    assert!(a.refs > 3, "fixture must produce >3 refs; got {a:?}");
    assert!(a.defs > 3, "fixture must produce >3 defs; got {a:?}");
    // Container column populated for refs inside functions.
    assert!(
        a.refs_with_container > 0,
        "fixture must produce refs with container_node_id set (v0.7.4 shape); got {a:?}",
    );
}

#[test]
fn f4_deterministic_container_node_ids_across_parses() {
    // Companion invariant: not just row counts, but the actual
    // container_node_id values must match across parses. Sorts both
    // sides so the assertion doesn't care about insertion order (SQL
    // GROUP BY is order-agnostic; smell rules are too).
    let td = fixture_repo();
    let conn_a = Connection::open_in_memory().unwrap();
    cmd_parse::parse_into_conn(&conn_a, td.path(), Some("go"), None).unwrap();
    let conn_b = Connection::open_in_memory().unwrap();
    cmd_parse::parse_into_conn(&conn_b, td.path(), Some("go"), None).unwrap();

    let query = "SELECT container_node_id, token, COUNT(*) \
                 FROM node_refs \
                 WHERE container_node_id IS NOT NULL \
                 GROUP BY container_node_id, token \
                 ORDER BY container_node_id, token";
    let read = |c: &Connection| -> Vec<(String, String, i64)> {
        let mut stmt = c.prepare(query).unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    let a = read(&conn_a);
    let b = read(&conn_b);
    assert_eq!(
        a, b,
        "per-(container, token) ref counts must match across cold parses; \
         drift here breaks mache's fan_out_skew reproducibility",
    );
}
