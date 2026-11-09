//! The `query` op is read-only (bead `ley-line-open-b7e9a3`).
//!
//! `query` used to run its SQL on the living db's writer, so any caller of
//! the control socket, or of MCP with the `sql` group, could `DROP` a table,
//! `ATTACH` a host file or `VACUUM INTO` a host path. Each such statement is
//! now refused with a typed refusal naming the action, over both transports,
//! and the database, the reader pool and the host file system are checked
//! unchanged afterwards. A `SELECT` still returns its rows.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use leyline_cli_lib::daemon::DaemonContext;
use serde_json::{Value, json};

const TOKEN: &str = "c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00";

/// A daemon on both transports, over a live db holding table `t`.
struct Daemon {
    dir: tempfile::TempDir,
    ctx: Arc<DaemonContext>,
    control: PathBuf,
    mcp: PathBuf,
    _rt: tokio::runtime::Runtime,
}

impl Daemon {
    fn start() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let ctx = common::daemon_context(dir.path());
        ctx.live_db
            .writer
            .lock()
            .execute_batch(
                "CREATE TABLE t (a INTEGER, b TEXT);
                 INSERT INTO t VALUES (1, 'one'), (2, 'two');
                 CREATE INDEX t_a ON t (a);",
            )
            .unwrap();

        let control = dir.path().join("control.sock");
        leyline_cli_lib::daemon::socket::spawn(
            Arc::clone(&ctx),
            control.clone(),
            leyline_cli_lib::daemon::socket::SocketDiscovery::Unpublished,
        )
        .expect("spawn control socket");
        let mcp = dir.path().join("mcp.sock");
        leyline_cli_lib::daemon::mcp::spawn_uds(
            Arc::clone(&ctx),
            mcp.clone(),
            Some(Arc::new(TOKEN.to_string())),
            leyline_cli_lib::daemon::mcp::McpGrant::all(),
        )
        .expect("spawn MCP over uds");
        drop(common::wait_for_uds(&control, 10_000));
        drop(common::wait_for_uds(&mcp, 10_000));
        Self {
            dir,
            ctx,
            control,
            mcp,
            _rt: rt,
        }
    }

    /// The `query` op's response over the control socket.
    fn over_socket(&self, sql: &str) -> Value {
        let mut stream = UnixStream::connect(&self.control).expect("connect control socket");
        let line = json!({"op": "query", "sql": sql}).to_string();
        stream.write_all(line.as_bytes()).unwrap();
        stream.write_all(b"\n").unwrap();
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).unwrap();
        serde_json::from_str(&reply).unwrap_or_else(|e| panic!("{e}: {reply}"))
    }

    /// The `query` tool's response over MCP: the op's JSON, which MCP carries
    /// as the text content, and whether MCP flagged it an error.
    fn over_mcp(&self, sql: &str) -> (Value, bool) {
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "query", "arguments": {"sql": sql}},
        })
        .to_string();
        let (code, reply) = common::post_mcp_over_uds(&self.mcp, &body, Some(TOKEN));
        assert_eq!(code, 200, "{reply}");
        let reply: Value = serde_json::from_str(&reply).unwrap();
        let text = reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no tool result: {reply}"));
        (
            serde_json::from_str(text).unwrap(),
            reply["result"]["isError"] == true,
        )
    }

    /// Everything a refused statement could have changed: the schema and
    /// rows of the living db, and every reader-pool connection's attached
    /// databases and settings.
    fn state(&self) -> Value {
        let writer = self.ctx.live_db.writer.lock();
        let schema: Vec<Value> = writer
            .prepare("SELECT type, name, sql FROM sqlite_master ORDER BY name")
            .unwrap()
            .query_map([], |r| {
                Ok(json!([
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?
                ]))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let rows: Vec<Value> = writer
            .prepare("SELECT a, b FROM t ORDER BY rowid")
            .unwrap()
            .query_map([], |r| {
                Ok(json!([r.get::<_, i64>(0)?, r.get::<_, String>(1)?]))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        drop(writer);

        // Hold every pool connection at once so each is inspected.
        let pool = &self.ctx.live_db.reader_pool;
        let readers: Vec<_> = (0..pool.max_size()).map(|_| pool.get().unwrap()).collect();
        let readers: Vec<Value> = readers
            .iter()
            .map(|c| {
                let attached: i64 = c
                    .query_row("SELECT count(*) FROM pragma_database_list", [], |r| {
                        r.get(0)
                    })
                    .unwrap();
                let query_only: i64 = c.query_row("PRAGMA query_only", [], |r| r.get(0)).unwrap();
                let writable: i64 = c
                    .query_row("PRAGMA writable_schema", [], |r| r.get(0))
                    .unwrap();
                json!({"attached": attached, "query_only": query_only, "writable_schema": writable})
            })
            .collect();
        json!({"schema": schema, "rows": rows, "readers": readers})
    }

    fn host_path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

/// The refusal `query` must return for `sql`: `ok` false, no rows, and a
/// refusal with this reason and action, the same over both transports.
fn assert_refused(d: &Daemon, sql: &str, reason: &str, action: &str) {
    let before = d.state();
    let socket = d.over_socket(sql);
    let (mcp, is_error) = d.over_mcp(sql);
    for (transport, reply) in [("socket", &socket), ("mcp", &mcp)] {
        assert_eq!(reply["ok"], false, "{transport}: {sql}: {reply}");
        assert!(
            reply.get("rows").is_none(),
            "{transport}: {sql}: rows on a refusal: {reply}"
        );
        assert_eq!(
            reply["refusal"]["reason"], reason,
            "{transport}: {sql}: {reply}"
        );
        assert_eq!(
            reply["refusal"]["action"], action,
            "{transport}: {sql}: {reply}"
        );
        let error = reply["error"].as_str().unwrap_or_default();
        assert!(error.contains(action), "{transport}: {sql}: {reply}");
    }
    assert!(is_error, "MCP marks the refusal an error: {mcp}");
    assert_eq!(
        socket, mcp,
        "both transports return the same refusal for {sql}"
    );
    assert_eq!(d.state(), before, "{sql} changed the daemon");
}

fn assert_no_host_file(path: &Path) {
    assert!(!path.exists(), "{} was written", path.display());
}

/// SQLite authorizes a DROP's write to the schema table before the drop
/// itself, and the first denial ends the statement: the refusal names that
/// write.
#[test]
fn drop_is_refused() {
    let d = Daemon::start();
    assert_refused(&d, "DROP TABLE t", "denied_action", "delete");
    assert_refused(&d, "DROP INDEX t_a", "denied_action", "delete");
    assert_eq!(
        d.over_socket("DROP TABLE t")["refusal"]["subject"],
        "sqlite_master"
    );
}

#[test]
fn attach_is_refused() {
    let d = Daemon::start();
    let host = d.host_path("attached.db");
    let p = host.display();
    assert_refused(
        &d,
        &format!("ATTACH DATABASE '{p}' AS e"),
        "denied_action",
        "attach",
    );
    assert_refused(&d, "DETACH DATABASE main", "denied_action", "detach");
    assert_no_host_file(&host);
}

/// A file name given as an expression reaches the authorizer as an action
/// rusqlite does not name, and SQLite reports the statement read-only and
/// as holding a SELECT: only the authorizer's deny-by-default refuses it.
/// Without it the reader attaches, and creates, the host file.
#[test]
fn an_attach_by_expression_is_refused() {
    let d = Daemon::start();
    let host = d.host_path("attached-by-expression.db");
    assert_refused(
        &d,
        &format!("ATTACH (SELECT '{}') AS e", host.display()),
        "denied_action",
        "attach",
    );
    assert_no_host_file(&host);
}

#[test]
fn vacuum_into_is_refused() {
    let d = Daemon::start();
    let host = d.host_path("exfil.db");
    assert_refused(
        &d,
        &format!("VACUUM INTO '{}'", host.display()),
        "not_a_query",
        "vacuum",
    );
    assert_refused(&d, "VACUUM", "not_a_query", "vacuum");
    // A refusal that names nothing carries no `subject`.
    let refusal = &d.over_socket("VACUUM")["refusal"];
    assert!(refusal.get("subject").is_none(), "{refusal}");
    assert_no_host_file(&host);
}

#[test]
fn insert_update_and_delete_are_refused() {
    let d = Daemon::start();
    assert_refused(
        &d,
        "INSERT INTO t VALUES (3, 'three')",
        "denied_action",
        "insert",
    );
    assert_refused(&d, "UPDATE t SET b = 'x'", "denied_action", "update");
    assert_refused(&d, "DELETE FROM t", "denied_action", "delete");
}

#[test]
fn pragma_writable_schema_is_refused() {
    let d = Daemon::start();
    assert_refused(&d, "PRAGMA writable_schema = ON", "denied_action", "pragma");
    assert_refused(&d, "PRAGMA query_only = OFF", "denied_action", "pragma");
}

#[test]
fn a_write_inside_a_cte_is_refused() {
    let d = Daemon::start();
    assert_refused(
        &d,
        "WITH doomed AS (SELECT a FROM t) DELETE FROM t WHERE a IN (SELECT a FROM doomed)",
        "denied_action",
        "delete",
    );
    assert_refused(
        &d,
        "WITH extra(a, b) AS (VALUES (9, 'nine')) INSERT INTO t SELECT * FROM extra",
        "denied_action",
        "insert",
    );
}

#[test]
fn a_trigger_cannot_be_created_to_write_later() {
    let d = Daemon::start();
    assert_refused(
        &d,
        "CREATE TRIGGER wipe AFTER UPDATE ON t BEGIN DELETE FROM t; END",
        "denied_action",
        "create_trigger",
    );
    // A temp table writes the temp schema first.
    assert_refused(
        &d,
        "CREATE TEMP TABLE scratch (x)",
        "denied_action",
        "insert",
    );
    assert_eq!(
        d.over_socket("CREATE TEMP TABLE scratch (x)")["refusal"]["subject"],
        "sqlite_temp_master"
    );
}

#[test]
fn stacked_statements_are_refused() {
    let d = Daemon::start();
    assert_refused(
        &d,
        "SELECT a FROM t; SELECT 2",
        "multiple_statements",
        "statement",
    );
    // The second statement is prepared, never run, and its action is denied.
    assert_refused(
        &d,
        "SELECT a FROM t; DROP TABLE t",
        "denied_action",
        "delete",
    );
    assert_refused(
        &d,
        "SELECT a FROM t; DELETE FROM t; SELECT 1",
        "denied_action",
        "delete",
    );
}

#[test]
fn a_function_that_reaches_outside_the_db_is_refused() {
    let d = Daemon::start();
    assert_refused(
        &d,
        "SELECT load_extension('x')",
        "denied_action",
        "function",
    );
}

#[test]
fn a_select_still_returns_its_rows() {
    let d = Daemon::start();
    let want = json!({
        "ok": true,
        "columns": ["a", "b"],
        "rows": [{"a": "1", "b": "one"}, {"a": "2", "b": "two"}],
    });
    let sql = "SELECT a, b FROM t ORDER BY a";
    assert_eq!(d.over_socket(sql), want);
    let (mcp, is_error) = d.over_mcp(sql);
    assert_eq!(mcp, want);
    assert!(!is_error);

    // Joins, CTEs, functions and table-valued schema introspection are reads.
    let (cte, _) = d.over_mcp(
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 3) \
         SELECT count(*) AS n, upper(group_concat(b)) AS bs FROM n JOIN t ON t.a = n.x",
    );
    assert_eq!(cte["rows"], json!([{"n": "2", "bs": "ONE,TWO"}]), "{cte}");
    let cols = d.over_socket("SELECT name FROM pragma_table_info('t') ORDER BY cid");
    assert_eq!(
        cols["rows"],
        json!([{"name": "a"}, {"name": "b"}]),
        "{cols}"
    );
}
