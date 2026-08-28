//! Injections MVP acceptance gates (bead `ley-line-open-c822a6`, EXP2
//! from the queries-as-data analysis on bead `ley-line-open-e5addb`).
//!
//! ## Claims
//!
//! 1. **Embedded-language facts land on the host file.** A SQL
//!    `CREATE TABLE` inside a Go string literal — marked by
//!    `queries/go/injections.scm` (`@injection.content` +
//!    `(#set! injection.language "sql")`, upstream tree-sitter
//!    conventions) — produces a `node_defs` row whose nid lives in the
//!    HOST `.go` file's range and whose `container_nid` is the host's
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
//! 3. **Injected node identity is pinned.** projection-v5 replaced the
//!    pre-v5 `{host_literal_node_id}#inj{k}` string id: an injected
//!    fact row carries a real nid in the host file's range whose
//!    ordinal sits PAST the host's `_ast` count, in fold order, with no
//!    `_ast` and no `nodes` row of its own. "Has no `_ast` row" IS the
//!    v5 test for injectedness — the property the `#inj` infix used to
//!    spell, now structural instead of lexical.
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
/// container_nid assertion has a target.
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

/// One `node_defs` row, projection-v5 shaped.
struct DefRow {
    token: String,
    /// The def site's nid. Injected rows have no `_ast`/`nodes` row, so
    /// this is the only handle on them.
    nid: i64,
    /// Rel path of the host file (`_source.id` via `nid >> 24`).
    file: String,
    /// Container rendered as its display path, `None` for a NULL container.
    container: Option<String>,
    canonical_kind: Option<String>,
    node_hash: Option<String>,
}

/// Every node_defs row, token-ordered.
///
/// projection-v5: `source_id` is gone (the file is `nid >> 24`, joined
/// through `_source.file_id`) and `container_node_id` is the integer
/// `container_nid`, rendered here through `v_node_path` so the
/// assertions stay path-shaped. The container LEFT JOIN is deliberate —
/// a container can be NULL, and an injected container would have no
/// path row at all.
fn defs_rows(db_path: &Path) -> Vec<DefRow> {
    let conn = Connection::open(db_path).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT d.token, d.nid, s.id, p.path, d.canonical_kind, \
             lower(hex(d.node_hash)) \
             FROM node_defs d \
             JOIN _source s ON s.file_id = d.nid >> 24 \
             LEFT JOIN v_node_path p ON p.nid = d.container_nid \
             ORDER BY d.token, d.nid",
        )
        .unwrap();
    stmt.query_map([], |r| {
        Ok(DefRow {
            token: r.get(0)?,
            nid: r.get(1)?,
            file: r.get(2)?,
            container: r.get(3)?,
            canonical_kind: r.get(4)?,
            node_hash: r.get(5)?,
        })
    })
    .unwrap()
    .map(|r| r.unwrap())
    .collect()
}

/// Full host occurrence map: (display path, hex(node_hash)) for every
/// `_ast` row, path-ordered. THE host-structural-identity snapshot.
///
/// Rendered through `v_node_path` rather than compared as raw nids: the
/// on/off arenas are separate databases, so the pre-v5 path-shaped key
/// is what makes the cross-db comparison meaningful.
fn ast_hash_map(db_path: &Path) -> Vec<(String, String)> {
    let conn = Connection::open(db_path).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT p.path, lower(hex(a.node_hash)) FROM _ast a \
             JOIN v_node_path p ON p.nid = a.nid \
             ORDER BY p.path",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

/// Display path of the fixture's raw-string SQL literal — the host node
/// the injection is anchored at. Every hop is the only named child of
/// its kind, so no `_{idx}` suffixes appear. Under projection-v5 this is
/// a path to `resolve_path`, not a stored id.
const LITERAL_NODE_PATH: &str = "app.go/function_declaration_0/block/statement_list\
/expression_statement/call_expression/argument_list/raw_string_literal";

/// The count of `_ast` rows in `file_id`'s range — the first ordinal an
/// injected node can occupy.
fn ast_count(conn: &Connection, file_id: i64) -> i64 {
    let (lo, hi) = leyline_ts::schema::file_nid_range(file_id);
    conn.query_row(
        "SELECT COUNT(*) FROM _ast WHERE nid BETWEEN ?1 AND ?2",
        [lo, hi],
        |r| r.get(0),
    )
    .unwrap()
}

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
    let users: Vec<_> = defs.iter().filter(|d| d.token == "users").collect();
    assert_eq!(
        users.len(),
        1,
        "exactly one injected `users` def expected; got {} rows",
        users.len(),
    );
    let users = users[0];
    assert_eq!(
        users.file, "app.go",
        "injected def's nid must live in the HOST file's range (mache reads per-file)"
    );
    assert_eq!(
        users.container.as_deref(),
        Some("app.go/function_declaration_0"),
        "injected def must be contained by the host's enclosing Go function"
    );
    assert_eq!(
        users.canonical_kind.as_deref(),
        Some("type"),
        "create_table maps to κ `type` (languages.rs SQL arm)"
    );
    assert!(
        users.node_hash.is_some(),
        "injected def must carry its own content-addressed node_hash"
    );

    // The injection's anchor is still a real host node — the host
    // literal resolves, and the injected fact takes an ordinal past the
    // whole host AST rather than displacing any of it.
    let conn = Connection::open(&db_path).unwrap();
    assert!(
        leyline_ts::schema::resolve_path(&conn, LITERAL_NODE_PATH)
            .unwrap()
            .is_some(),
        "the host literal the injection anchors at must be a real `_ast` node"
    );
    let file_id = leyline_ts::schema::lookup_file_id(&conn, "app.go")
        .unwrap()
        .expect("app.go must be interned");
    assert_eq!(
        users.nid >> 24,
        file_id,
        "injected def's nid must be in app.go's range"
    );
    assert!(
        leyline_ts::schema::nid_ordinal(users.nid).unwrap() >= ast_count(&conn, file_id),
        "injected ordinals come PAST the host's `_ast` count",
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
    let (file, container): (String, Option<String>) = conn
        .query_row(
            "SELECT s.id, p.path FROM node_refs r \
             JOIN _source s ON s.file_id = r.nid >> 24 \
             LEFT JOIN v_node_path p ON p.nid = r.container_nid \
             WHERE r.token = 'users'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("SELECT ... FROM users inside a Go string must emit a `users` ref");
    assert_eq!(file, "app.go");
    assert_eq!(
        container.as_deref(),
        Some("app.go/function_declaration_1"),
        "ref site sits inside the second fixture function (fetch)"
    );
}

#[test]
fn inj_injected_nid_scheme_pinned() {
    // projection-v5 replacement for the pre-v5
    // `{host_literal}#inj0/statement/create_table` string pin: that id
    // is not stored anywhere any more. What IS observable — and what the
    // `#inj` infix was standing in for — is the nid's shape:
    //
    //   * it is in the host file's range (no file of its own),
    //   * its ordinal is past the host's `_ast` count, in fold order,
    //   * it has NO `_ast` row and NO `nodes` row, so it renders no path.
    //
    // The last one is the v5 test for "is this fact injected?", which
    // consumers previously spelled `node_id LIKE '%#inj%'`.
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = go_fixture();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");
    parse_pass(&db_path, repo.path(), "go");

    let defs = defs_rows(&db_path);
    let users: Vec<_> = defs.iter().filter(|d| d.token == "users").collect();
    assert_eq!(
        users.len(),
        1,
        "expected one `users` def; got {}",
        users.len()
    );
    let nid = users[0].nid;

    let conn = Connection::open(&db_path).unwrap();
    let file_id = leyline_ts::schema::lookup_file_id(&conn, "app.go")
        .unwrap()
        .expect("app.go must be interned");
    let (lo, hi) = leyline_ts::schema::file_nid_range(file_id);
    assert!(
        (lo..=hi).contains(&nid),
        "injected nid {nid} must live in app.go's range [{lo}, {hi}]"
    );

    let host_ast = ast_count(&conn, file_id);
    assert!(
        leyline_ts::schema::nid_ordinal(nid).unwrap() >= host_ast,
        "injected ordinal must come past the host's {host_ast} `_ast` nodes"
    );

    let has_ast: i64 = conn
        .query_row("SELECT COUNT(*) FROM _ast WHERE nid = ?1", [nid], |r| {
            r.get(0)
        })
        .unwrap();
    let has_node: i64 = conn
        .query_row("SELECT COUNT(*) FROM nodes WHERE nid = ?1", [nid], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        (has_ast, has_node),
        (0, 0),
        "an injected node has neither an `_ast` nor a `nodes` row — that absence IS its identity"
    );
    assert_eq!(
        leyline_ts::schema::node_path(&conn, nid).unwrap(),
        None,
        "an injected nid renders no display path"
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
        defs_rows(&db_on).iter().any(|d| d.token == "users"),
        "injection pass ON must produce the injected `users` def"
    );
    assert!(
        !defs_rows(&db_off).iter().any(|d| d.token == "users"),
        "LLO_DISABLE_INJECTIONS=1 must suppress injected facts"
    );

    let on = ast_hash_map(&db_on);
    let off = ast_hash_map(&db_off);
    assert!(!on.is_empty(), "host _ast must not be empty");
    assert_eq!(
        on, off,
        "host (path → node_hash) map must be byte-identical with the injection pass on vs off"
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
        .find(|d| d.token == "users")
        .expect("injected `users` def in host db")
        .node_hash
        .expect("injected def must carry node_hash");
    let standalone_hash = defs_rows(&sql_db)
        .into_iter()
        .find(|d| d.token == "users")
        .expect("standalone `users` def in sql db")
        .node_hash
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

    // projection-v5: an injected fact row is one whose nid has no `_ast`
    // row — the structural form of the pre-v5 `node_id LIKE '%#inj%'`.
    let conn = Connection::open(&db_path).unwrap();
    let injected: i64 = conn
        .query_row(
            "SELECT count(*) FROM node_refs r \
             WHERE NOT EXISTS (SELECT 1 FROM _ast a WHERE a.nid = r.nid)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let injected_defs: i64 = conn
        .query_row(
            "SELECT count(*) FROM node_defs d \
             WHERE NOT EXISTS (SELECT 1 FROM _ast a WHERE a.nid = d.nid)",
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
