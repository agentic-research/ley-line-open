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
fn parse_go_to_conn(source: &str, rel: &str) -> Connection {
    let td = TempDir::new().unwrap();
    fs::write(td.path().join(rel), source).unwrap();
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
    // projection-v5: node_refs has no `source_id`. A ref's file is
    // `nid >> 24`, which lands on `_source.file_id`; `_source.id` is the
    // rel path `_imports.source_id` still carries.
    let resolved: String = conn
        .query_row(
            "SELECT i.path FROM node_refs r \
             JOIN _source s ON s.file_id = r.nid >> 24 \
             JOIN _imports i ON i.alias = r.qualifier AND i.source_id = s.id \
             WHERE r.token = 'Fatalf'",
            [],
            |r| r.get(0),
        )
        .expect("bare Fatalf row must join _imports through qualifier");
    assert_eq!(resolved, "log", "alias l must resolve to package 'log'");
}

#[test]
fn qualifier_is_in_the_base_ddl_not_an_additive_alter() {
    // Replaces `legacy_arena_gains_qualifier_column_on_reparse`, whose
    // premise (a pre-qualifier arena is migrated in place by
    // `create_qualifier_column`'s additive ALTER) died with projection-v5:
    // the ALTER migrations are gone and a pre-v5 arena is REFUSED at open,
    // not patched (`cmd_parse::tests::a_pre_v5_arena_is_refused_at_open`).
    // The surviving obligation is that the column ships in the base DDL,
    // so `create_refs_tables` alone — no migration step — yields a
    // node_refs that a qualifier-writing INSERT can target.
    let conn = Connection::open_in_memory().unwrap();
    leyline_ts::schema::create_refs_tables(&conn).unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('node_refs') WHERE name = 'qualifier'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        n, 1,
        "node_refs must carry `qualifier` straight from REFS_TABLE_DDL"
    );

    // And a cold parse into that same arena populates it.
    let td = TempDir::new().unwrap();
    fs::write(
        td.path().join("main.go"),
        "package main\n\nimport \"fmt\"\n\nfunc f() {\n\tfmt.Println(1)\n}\n",
    )
    .unwrap();
    cmd_parse::parse_into_conn(&conn, td.path(), Some("go"), None).unwrap();

    let refs = refs_with_qualifier(&conn);
    assert!(
        refs.contains(&("Println".to_string(), Some("fmt".to_string()))),
        "fresh rows must populate qualifier; got {refs:?}"
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
