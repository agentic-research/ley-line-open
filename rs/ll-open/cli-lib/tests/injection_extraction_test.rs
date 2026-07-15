//! Injections MVP acceptance gates (bead `ley-line-open-c822a6`, EXP2
//! from the queries-as-data analysis on bead `ley-line-open-e5addb`).
//!
//! ## Claims
//!
//! 1. **Embedded-language facts land on the host file.** A SQL
//!    `CREATE TABLE` inside a Go string literal — marked by
//!    `queries/go/injections.scm` (`@injection.content` +
//!    `(#set! injection.language "sql")`, upstream tree-sitter
//!    conventions) — produces a `node_defs` row whose `source_id` is
//!    the HOST `.go` file and whose `container_node_id` is the host's
//!    enclosing Go function. mache reads facts per-file; the injected
//!    subtree has no file of its own.
//!
//! 2. **Host-hash independence.** The host file's structural
//!    `node_hash` values are byte-identical with the injection pass on
//!    vs off. Injected subtrees get their OWN content-addressed root —
//!    bumping the injected grammar (tree-sitter-sequel) must never
//!    re-hash a Go file that contains SQL. The off-switch is the
//!    `LLO_DISABLE_INJECTIONS=1` falsification seam.
//!
//! 3. **Injected node identity is pinned.** Injected root node_id =
//!    `{host_literal_node_id}#inj{k}`; descendants follow the host
//!    fold's `{parent}/{kind}[_{idx}]` naming. `#` cannot occur in
//!    host node_ids (they are path + grammar-kind derived), so the
//!    scheme cannot collide.
//!
//! 4. **Own-CA-root dedup.** The injected `create_table` subtree's
//!    `node_hash` equals the hash the SAME statement bytes produce in
//!    a standalone `.sql` file (the merkle fold is span-free), and the
//!    hash resolves in `node_content` with `lang = 'sql'`.

use leyline_cli_lib::cmd_parse;
use rusqlite::Connection;
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use tempfile::TempDir;

/// Serializes tests that mutate `LLO_DISABLE_INJECTIONS` — env vars are
/// process-global and the harness runs tests on parallel threads. Same
/// pattern as f6's `ENV_LOCK`.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Restores the prior `LLO_DISABLE_INJECTIONS` on drop.
struct DisableInjectionsOverride {
    prev: Option<String>,
}

impl DisableInjectionsOverride {
    const KEY: &'static str = "LLO_DISABLE_INJECTIONS";

    fn set(value: &str) -> Self {
        let prev = std::env::var(Self::KEY).ok();
        // SAFETY: callers hold ENV_LOCK, so no other thread in this
        // test binary touches the env concurrently.
        unsafe { std::env::set_var(Self::KEY, value) };
        Self { prev }
    }
}

impl Drop for DisableInjectionsOverride {
    fn drop(&mut self) {
        // SAFETY: same ENV_LOCK scope as `set`.
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var(Self::KEY, v),
                None => std::env::remove_var(Self::KEY),
            }
        }
    }
}

/// The SQL statement embedded in the Go fixture AND written verbatim to
/// the standalone `.sql` fixture — the own-CA-root test asserts both
/// derivations produce the same `create_table` subtree hash.
const SQL_STMT: &str = "CREATE TABLE users (id INTEGER, name TEXT)";

/// Go fixture: one SQL def site (raw string) + one SQL ref site
/// (interpreted string), both inside named functions so the
/// container_node_id assertion has a target.
fn go_fixture() -> TempDir {
    let td = TempDir::new().unwrap();
    fs::write(
        td.path().join("app.go"),
        format!(
            "\
package main

import \"database/sql\"

func setup(db *sql.DB) {{
\tdb.Exec(`{SQL_STMT}`)
}}

func fetch(db *sql.DB) {{
\tdb.Query(\"SELECT name FROM users\")
}}
"
        ),
    )
    .unwrap();
    td
}

fn parse_pass(db_path: &Path, repo: &Path, lang: &str) -> cmd_parse::ParseResult {
    let conn = Connection::open(db_path).unwrap();
    cmd_parse::parse_into_conn(&conn, repo, Some(lang), None).unwrap()
}

/// (token, node_id, source_id, container_node_id, canonical_kind,
/// hex(node_hash)) for every node_defs row, token-ordered.
#[allow(clippy::type_complexity)]
fn defs_rows(
    db_path: &Path,
) -> Vec<(
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
)> {
    let conn = Connection::open(db_path).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT token, node_id, source_id, container_node_id, canonical_kind, \
             lower(hex(node_hash)) FROM node_defs ORDER BY token, node_id",
        )
        .unwrap();
    stmt.query_map([], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
        ))
    })
    .unwrap()
    .map(|r| r.unwrap())
    .collect()
}

/// Full host occurrence map: (node_id, hex(node_hash)) for every `_ast`
/// row, node_id-ordered. THE host-structural-identity snapshot.
fn ast_hash_map(db_path: &Path) -> Vec<(String, String)> {
    let conn = Connection::open(db_path).unwrap();
    let mut stmt = conn
        .prepare("SELECT node_id, lower(hex(node_hash)) FROM _ast ORDER BY node_id")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

/// Host node_id of the fixture's raw-string SQL literal, derived from
/// the fold's path naming (every hop is the only named child of its
/// kind, so no `_{idx}` suffixes appear).
const LITERAL_NODE_ID: &str = "app.go/function_declaration_0/block/statement_list\
/expression_statement/call_expression/argument_list/raw_string_literal";

#[test]
fn inj_sql_create_table_def_lands_on_host_file() {
    // Acceptance (a): the SQL CREATE TABLE name inside a Go string
    // literal is a node_defs row for the HOST Go file, contained by
    // the host's enclosing function, with SQL's κ kind for
    // create_table ("type").
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = go_fixture();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");
    parse_pass(&db_path, repo.path(), "go");

    let defs = defs_rows(&db_path);
    let users: Vec<_> = defs.iter().filter(|d| d.0 == "users").collect();
    assert_eq!(
        users.len(),
        1,
        "exactly one injected `users` def expected; got {users:?}"
    );
    let (_, node_id, source_id, container, kind, node_hash) = users[0];
    assert_eq!(
        source_id, "app.go",
        "injected def must carry the HOST file as source_id (mache reads per-file)"
    );
    assert_eq!(
        container.as_deref(),
        Some("app.go/function_declaration_0"),
        "injected def must be contained by the host's enclosing Go function"
    );
    assert_eq!(
        kind.as_deref(),
        Some("type"),
        "create_table maps to κ `type` (languages.rs SQL arm)"
    );
    assert!(
        node_id.starts_with(&format!("{LITERAL_NODE_ID}#inj0/")),
        "injected node_id must be rooted at the host literal's node_id + #inj0; got {node_id}"
    );
    assert!(
        node_hash.is_some(),
        "injected def must carry its own content-addressed node_hash"
    );
}

#[test]
fn inj_sql_ref_site_lands_on_host_file() {
    // The SELECT ... FROM users interpreted string emits a `users` ref
    // joining the def above — the dead_code join works across the
    // injection boundary.
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = go_fixture();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");
    parse_pass(&db_path, repo.path(), "go");

    let conn = Connection::open(&db_path).unwrap();
    let (source_id, container): (String, Option<String>) = conn
        .query_row(
            "SELECT source_id, container_node_id FROM node_refs WHERE token = 'users'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("SELECT ... FROM users inside a Go string must emit a `users` ref");
    assert_eq!(source_id, "app.go");
    assert_eq!(
        container.as_deref(),
        Some("app.go/function_declaration_1"),
        "ref site sits inside the second fixture function (fetch)"
    );
}

#[test]
fn inj_injected_node_id_scheme_pinned() {
    // Pin the exact injected node_id: root = host literal + `#inj0`,
    // descendants follow the host fold's `{parent}/{kind}` naming over
    // the INJECTED tree (program → statement → create_table; single
    // named children, no `_{idx}` suffixes).
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = go_fixture();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");
    parse_pass(&db_path, repo.path(), "go");

    let defs = defs_rows(&db_path);
    let users: Vec<_> = defs.iter().filter(|d| d.0 == "users").collect();
    assert_eq!(users.len(), 1, "expected one `users` def; got {users:?}");
    assert_eq!(
        users[0].1,
        format!("{LITERAL_NODE_ID}#inj0/statement/create_table"),
        "injected node_id scheme is pinned: change it only with a documented migration"
    );
}

#[test]
fn inj_host_node_hashes_independent_of_injection_pass() {
    // Acceptance (b): the host file's structural node_hash values are
    // byte-identical with the injection pass on vs off. This is the
    // executable form of "bumping tree-sitter-sql must not re-hash Go
    // files containing SQL": injected subtrees hash into their OWN
    // content-addressed roots, never into host preimages.
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = go_fixture();
    let db_dir = TempDir::new().unwrap();

    let db_on = db_dir.path().join("on.db");
    parse_pass(&db_on, repo.path(), "go");

    let db_off = db_dir.path().join("off.db");
    {
        let _g = DisableInjectionsOverride::set("1");
        parse_pass(&db_off, repo.path(), "go");
    }

    // The toggle must actually toggle — otherwise the equality below
    // is vacuous.
    assert!(
        defs_rows(&db_on).iter().any(|d| d.0 == "users"),
        "injection pass ON must produce the injected `users` def"
    );
    assert!(
        !defs_rows(&db_off).iter().any(|d| d.0 == "users"),
        "LLO_DISABLE_INJECTIONS=1 must suppress injected facts"
    );

    let on = ast_hash_map(&db_on);
    let off = ast_hash_map(&db_off);
    assert!(!on.is_empty(), "host _ast must not be empty");
    assert_eq!(
        on, off,
        "host (node_id → node_hash) map must be byte-identical with the injection pass on vs off"
    );
}

#[test]
fn inj_own_ca_root_dedups_with_standalone_sql() {
    // Own-CA-root pin: the injected create_table subtree's node_hash
    // equals the node_hash the SAME statement bytes produce in a
    // standalone .sql file — the merkle fold is span-free and
    // grammar-scoped, so content addressing crosses the host boundary.
    // The hash also resolves in node_content with lang='sql' (the
    // node_defs.node_hash → node_content FK made loud).
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let go_repo = go_fixture();
    let db_dir = TempDir::new().unwrap();
    let host_db = db_dir.path().join("host.db");
    parse_pass(&host_db, go_repo.path(), "go");

    let sql_repo = TempDir::new().unwrap();
    fs::write(sql_repo.path().join("schema.sql"), SQL_STMT).unwrap();
    let sql_db = db_dir.path().join("standalone.db");
    parse_pass(&sql_db, sql_repo.path(), "sql");

    let host_hash = defs_rows(&host_db)
        .into_iter()
        .find(|d| d.0 == "users")
        .expect("injected `users` def in host db")
        .5
        .expect("injected def must carry node_hash");
    let standalone_hash = defs_rows(&sql_db)
        .into_iter()
        .find(|d| d.0 == "users")
        .expect("standalone `users` def in sql db")
        .5
        .expect("standalone def must carry node_hash");
    assert_eq!(
        host_hash, standalone_hash,
        "identical SQL bytes must content-address identically whether injected or standalone"
    );

    let conn = Connection::open(&host_db).unwrap();
    let (lang, raw_kind): (String, String) = conn
        .query_row(
            "SELECT lang, raw_kind FROM node_content WHERE lower(hex(node_hash)) = ?1",
            [&host_hash],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("injected def's node_hash must resolve in node_content (own CA root)");
    assert_eq!((lang.as_str(), raw_kind.as_str()), ("sql", "create_table"));
}

#[test]
fn inj_prose_strings_do_not_inject() {
    // The injections.scm heuristic requires a statement-shaped leading
    // keyword. Prose like "update the docs" / "delete this file" must
    // not produce SQL facts on the host file.
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let td = TempDir::new().unwrap();
    fs::write(
        td.path().join("prose.go"),
        "\
package main

func notes() (string, string) {
\ta := \"update the docs\"
\tb := \"delete this file\"
\treturn a, b
}
",
    )
    .unwrap();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");
    parse_pass(&db_path, td.path(), "go");

    let conn = Connection::open(&db_path).unwrap();
    let injected: i64 = conn
        .query_row(
            "SELECT count(*) FROM node_refs WHERE node_id LIKE '%#inj%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let injected_defs: i64 = conn
        .query_row(
            "SELECT count(*) FROM node_defs WHERE node_id LIKE '%#inj%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        (injected, injected_defs),
        (0, 0),
        "prose strings must not pass the injection heuristic"
    );
}
