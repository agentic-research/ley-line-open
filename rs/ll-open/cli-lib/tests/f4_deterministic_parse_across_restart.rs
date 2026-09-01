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
            "SELECT COUNT(*) FROM node_refs WHERE container_nid IS NOT NULL",
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

    let query = "SELECT container_nid, token, COUNT(*) \
                 FROM node_refs \
                 WHERE container_nid IS NOT NULL \
                 GROUP BY container_nid, token \
                 ORDER BY container_nid, token";
    let read = |c: &Connection| -> Vec<(i64, String, i64)> {
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

// ── F4b: identity stability under scoped reparse ───────────────────────────

/// Every node identity in the arena, as a sorted set.
///
/// Deliberately the IDENTITIES, not their count. F4's existing gates compare
/// row counts, which is the exact blind spot this closes: an identity scheme
/// that renumbers every node still produces identical counts, so those gates
/// stay green while every stored reference silently rebinds to a different
/// node.
fn identity_snapshot(conn: &Connection) -> Vec<i64> {
    // projection-v5: the nid IS the identity, and it encodes the file in
    // its high bits — one integer carries what (node_id, source_id) did.
    let mut stmt = conn.prepare("SELECT nid FROM _ast ORDER BY nid").unwrap();
    let out: Vec<i64> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    out
}

/// The nid range a file owns, for scoping a snapshot to one file.
fn file_range(conn: &Connection, rel: &str) -> (i64, i64) {
    let file_id = leyline_ts::schema::lookup_file_id(conn, rel)
        .unwrap()
        .unwrap_or_else(|| panic!("{rel} must be interned"));
    leyline_ts::schema::file_nid_range(file_id)
}

/// **A scoped reparse of one file must not disturb the identity of any other
/// file's nodes.**
///
/// This is the gate that a future identity change has to pass, and it does not
/// exist anywhere else. The node-key work tracked in `ley-line-open-17c271`
/// replaces the path-shaped `node_id` with an integer; the obvious way to
/// assign one — a dense rank over the sorted work-list, or a plain rowid — is a
/// pure function of the WHOLE FILE SET, not of one file's parse. Under
/// `parse_into_conn(.., scope)` the git watcher reparses only the dirty files,
/// so a global assignment renumbers everything ordered after the edited file:
/// the same integer then denotes a different node, and anything that stored one
/// now points somewhere else.
///
/// Nothing catches that today. `f4_two_cold_parses_produce_identical_row_counts`
/// compares counts across seven tables, and a total renumbering preserves every
/// one of them.
///
/// SCOPE, precisely — this gate is the WEAKER of the pair, and it is worth
/// being exact about what it can and cannot see. Under a global rank, a scoped
/// reparse of `a.go` rewrites only `a.go`'s rows, so `b.go` keeps whatever it
/// was assigned and THIS assertion still passes. What it does catch is a scoped
/// pass that rewrites rows outside its scope — a real and separate defect, and
/// the one `delete_file_rows` plus the scope filter exist to prevent.
/// `f4b_scoped_reparse_agrees_with_a_full_cold_parse` below is the gate that
/// catches global ranking; treat that one as load-bearing here.
///
/// The invariant holds trivially for today's path-shaped ids — a path is a pure
/// function of one file's own parse. That is the point: this pins the property
/// while it is free, so the change that would break it fails here instead of in
/// a consumer's cache months later.
#[test]
fn f4b_scoped_reparse_preserves_identities_of_untouched_files() {
    let td = TempDir::new().unwrap();
    // Two files. `b.go` sorts after `a.go`, so any work-list-ordered scheme
    // assigns its identities AFTER a.go's — which is what makes a.go's edit
    // able to shift them.
    fs::write(
        td.path().join("a.go"),
        "package a\n\nfunc Alpha() string { return \"one\" }\n",
    )
    .unwrap();
    fs::write(
        td.path().join("b.go"),
        "package b\n\nfunc Beta() string { return \"two\" }\n\nfunc Gamma() {}\n",
    )
    .unwrap();

    let conn = Connection::open_in_memory().unwrap();
    cmd_parse::parse_into_conn(&conn, td.path(), Some("go"), None).unwrap();

    let (b_lo, b_hi) = file_range(&conn, "b.go");
    let before: Vec<i64> = identity_snapshot(&conn)
        .into_iter()
        .filter(|nid| (b_lo..=b_hi).contains(nid))
        .collect();
    assert!(
        !before.is_empty(),
        "fixture must produce _ast rows for b.go, or this gate asserts nothing"
    );

    // Edit a.go so its node COUNT changes — the case that shifts any
    // offset-based or rank-based assignment for everything after it.
    fs::write(
        td.path().join("a.go"),
        "package a\n\nfunc Alpha() string { return \"one\" }\n\nfunc Delta() {}\n\nfunc Epsilon() {}\n",
    )
    .unwrap();

    // Scoped reparse: only a.go, exactly as the git watcher drives it.
    cmd_parse::parse_into_conn(&conn, td.path(), Some("go"), Some(&["a.go".to_string()])).unwrap();

    let after: Vec<i64> = identity_snapshot(&conn)
        .into_iter()
        .filter(|nid| (b_lo..=b_hi).contains(nid))
        .collect();

    assert_eq!(
        before, after,
        "b.go was not reparsed, so every identity it owns must be byte-identical. \
         A difference here means node identity is a function of the file SET rather \
         than of one file's parse, so an edit to an unrelated file silently rebinds \
         references — the failure mode `ley-line-open-17c271` has to avoid, and the \
         one row-count gates cannot see."
    );
}

/// **A scoped reparse must land the same identities a full cold parse would.**
///
/// **This is the load-bearing half of the pair**, and the one that actually
/// catches a globally-ranked identity scheme.
///
/// Walk it through: with a dense rank over the sorted work-list, the first cold
/// parse gives `a.go` ranks 1..K and `b.go` K+1..L. Editing `a.go` so it gains
/// nodes and reparsing it SCOPED rewrites only `a.go`, now 1..N with N > K —
/// while `b.go` still holds K+1..L. A cold parse of that same final source
/// instead puts `b.go` at N+1..M. The two arenas describe identical trees and
/// disagree about what every node in `b.go` is called.
///
/// Not only must untouched files keep their identities, an arena's contents
/// must not depend on the sequence of edits that produced it.
#[test]
fn f4b_scoped_reparse_agrees_with_a_full_cold_parse() {
    let td = TempDir::new().unwrap();
    fs::write(
        td.path().join("a.go"),
        "package a\n\nfunc Alpha() string { return \"one\" }\n",
    )
    .unwrap();
    fs::write(td.path().join("b.go"), "package b\n\nfunc Beta() {}\n").unwrap();

    // Arena 1: cold parse, then edit a.go, then scoped reparse of a.go.
    let incremental = Connection::open_in_memory().unwrap();
    cmd_parse::parse_into_conn(&incremental, td.path(), Some("go"), None).unwrap();
    let edited = "package a\n\nfunc Alpha() string { return \"one\" }\n\nfunc Delta() {}\n";
    fs::write(td.path().join("a.go"), edited).unwrap();
    cmd_parse::parse_into_conn(
        &incremental,
        td.path(),
        Some("go"),
        Some(&["a.go".to_string()]),
    )
    .unwrap();

    // Arena 2: one cold parse of the SAME final source.
    let cold = Connection::open_in_memory().unwrap();
    cmd_parse::parse_into_conn(&cold, td.path(), Some("go"), None).unwrap();

    assert_eq!(
        identity_snapshot(&incremental),
        identity_snapshot(&cold),
        "an arena built incrementally must hold the same identities as one built \
         cold from the same source — otherwise identity depends on edit history, \
         and two arenas over identical trees disagree about what a node is called"
    );
}
