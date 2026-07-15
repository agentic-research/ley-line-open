//! Pin: `node_refs.qualifier` is the syntactic receiver/selector text
//! of a qualified call site, carried on the BARE-token row of the
//! dual-emit pair. Bead `ley-line-open-4dde42` (the `ley-line-open-b9d1d5`
//! leftover — container_node_id shipped in v0.7.4, qualifier did not).
//!
//! Why this matters (mache parity): mache's `fatal_call` rung-1 needs
//! `qualifier JOIN _imports.alias` to resolve stdlib packages through
//! aliases (killing the custom-logger false-positive class), and
//! `fan_out_skew`'s mention arm needs qualifier-awareness. Today only
//! capnp binding rows carry a qualifier; node_refs mention rows force
//! consumers into string-splitting tokens.
//!
//! Column semantics (pinned here):
//! - bare-token row of a qualified call (`Println` of `fmt.Println(..)`)
//!   carries the qualifier text (`'fmt'`) — exactly ONE row per
//!   qualified call site holds the structural (name, qualifier) pair,
//!   so GROUP BY / COUNT rules never double-count;
//! - the qualified-token row (`fmt.Println`) carries NULL — its token
//!   already embeds the qualifier;
//! - genuinely bare calls carry NULL. NULL (not `''`) because the
//!   additive ALTER backfills NULL on every legacy row — a second `''`
//!   encoding on fresh rows would split "no qualifier" into two shapes.
//!   mache's v_refs (`TEXT NOT NULL DEFAULT ''`) wraps the column with
//!   `COALESCE(qualifier, '')`.

#![cfg(feature = "hdc")]

use leyline_cli_lib::cmd_parse;
use rusqlite::Connection;
use std::fs;
use tempfile::TempDir;

/// Parse a Go source snippet through the full `parse_into_conn`
/// pipeline (same harness as `container_node_id_test.rs`).
fn parse_go_to_conn(source: &str, source_id: &str) -> Connection {
    let td = TempDir::new().unwrap();
    fs::write(td.path().join(source_id), source).unwrap();
    let conn = Connection::open_in_memory().unwrap();
    cmd_parse::parse_into_conn(&conn, td.path(), Some("go"), None).unwrap();
    conn
}

fn refs_with_qualifier(conn: &Connection) -> Vec<(String, Option<String>)> {
    conn.prepare("SELECT token, qualifier FROM node_refs ORDER BY token")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

#[test]
fn qualifier_lands_on_bare_row_through_full_pipeline() {
    // End-to-end shape: the batched production path (cmd_parse RefBatch,
    // not just leyline-ts's insert_extracted_refs) must thread the
    // qualifier from the engine's @qualifier capture into the column.
    let source = "\
package main

import \"fmt\"

func f(x int) {
\tfmt.Println(x)
\tg()
}

func g() {}
";
    let conn = parse_go_to_conn(source, "main.go");
    let refs = refs_with_qualifier(&conn);

    assert!(
        refs.contains(&("Println".to_string(), Some("fmt".to_string()))),
        "bare row of fmt.Println must carry qualifier 'fmt'; got {refs:?}"
    );
    assert!(
        refs.contains(&("fmt.Println".to_string(), None)),
        "qualified row must carry NULL qualifier; got {refs:?}"
    );
    assert!(
        refs.contains(&("g".to_string(), None)),
        "bare call must carry NULL qualifier; got {refs:?}"
    );
}

#[test]
fn qualifier_supports_import_alias_join() {
    // The load-bearing consumer query (mache fatal_call rung-1):
    // qualifier JOIN _imports.alias resolves the package through an
    // alias without token string-surgery. `l.Fatalf` where `l` aliases
    // `log` must resolve to path 'log' via the join.
    let source = "\
package main

import l \"log\"

func f() {
\tl.Fatalf(\"boom\")
}
";
    let conn = parse_go_to_conn(source, "main.go");
    let resolved: String = conn
        .query_row(
            "SELECT i.path FROM node_refs r \
             JOIN _imports i ON i.alias = r.qualifier AND i.source_id = r.source_id \
             WHERE r.token = 'Fatalf'",
            [],
            |r| r.get(0),
        )
        .expect("bare Fatalf row must join _imports through qualifier");
    assert_eq!(resolved, "log", "alias l must resolve to package 'log'");
}

#[test]
fn legacy_arena_gains_qualifier_column_on_reparse() {
    // Upgrade path: an arena created by a pre-qualifier binary has a
    // node_refs table WITHOUT the column. The epoch bump (3→4) forces
    // fact re-derivation, and the INSERT names the qualifier column —
    // so parse_into_conn must additively ALTER the legacy shape first
    // (create_qualifier_column), not fail loudly on the old table.
    let td = TempDir::new().unwrap();
    fs::write(
        td.path().join("main.go"),
        "package main\n\nimport \"fmt\"\n\nfunc f() {\n\tfmt.Println(1)\n}\n",
    )
    .unwrap();

    let conn = Connection::open_in_memory().unwrap();
    // v0.7.8-shaped node_refs: no qualifier column.
    conn.execute_batch(
        "CREATE TABLE node_refs (
            token TEXT NOT NULL,
            node_id TEXT NOT NULL,
            source_id TEXT NOT NULL,
            container_node_id TEXT
        );",
    )
    .unwrap();

    cmd_parse::parse_into_conn(&conn, td.path(), Some("go"), None)
        .expect("parse over a legacy-shaped arena must succeed (additive ALTER)");

    let refs = refs_with_qualifier(&conn);
    assert!(
        refs.contains(&("Println".to_string(), Some("fmt".to_string()))),
        "migrated arena must populate qualifier on fresh rows; got {refs:?}"
    );
}

#[test]
fn node_defs_does_not_gain_qualifier_column() {
    // Scope pin (bead ley-line-open-4dde42 is node_refs-only): the
    // ancestor-derived qualified DEF tokens (rust_impl_receiver,
    // python_enclosing_class, js_ts_context_fixups, java_enclosing_type,
    // cpp_enclosing_class) stay dual-emitted in the token column;
    // node_defs carries no qualifier column.
    let conn = parse_go_to_conn("package main\n\nfunc f() {}\n", "main.go");
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('node_defs') WHERE name = 'qualifier'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0, "node_defs must NOT have a qualifier column");
}
