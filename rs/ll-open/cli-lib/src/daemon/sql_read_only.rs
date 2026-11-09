//! Read-only SQL for the daemon's `query` op (bead `ley-line-open-b7e9a3`).
//!
//! `query` takes SQL from its caller. Before this module it ran that SQL on
//! the living db's writer, so a caller could `DROP` a table, `ATTACH` a host
//! file, or `VACUUM INTO` a host path. Now the statement runs on a reader-pool
//! connection (`PRAGMA query_only=ON`) under a SQLite authorizer, and is
//! refused unless SQLite itself reports it as a read-only `SELECT`.
//!
//! Three checks, none of which inspects the SQL text:
//!
//! 1. **The authorizer, deny by default.** SQLite calls it for every action a
//!    statement takes, at prepare time and again for statements SQLite
//!    prepares while stepping (a table-valued pragma's inner `PRAGMA`, the
//!    internal statements of `VACUUM`). It allows only [`AuthAction::Select`],
//!    [`AuthAction::Read`], [`AuthAction::Recursive`], the functions in
//!    [`ALLOWED_FUNCTIONS`] and the pragmas in [`ALLOWED_PRAGMAS`]. Every
//!    other action is denied, including ones this build does not know: an
//!    `ATTACH` whose file name is an expression reaches the authorizer as
//!    `SQLITE_ATTACH` with no file name, which rusqlite reports as
//!    [`AuthAction::Unknown`]. A deny list naming `Attach` would let it
//!    through; deny by default refuses it.
//! 2. **One statement.** rusqlite refuses SQL that holds a second statement
//!    (`Error::MultipleStatement`) after preparing, never running, the second
//!    one, so a second statement is refused before anything is stepped.
//! 3. **A read-only `SELECT`.** The prepared statement must have reported a
//!    `SELECT` to the authorizer and `sqlite3_stmt_readonly` must hold. This
//!    is what refuses `VACUUM`: SQLite reports no action for a `VACUUM` at
//!    prepare time, and `VACUUM INTO` writes its file even on a `query_only`
//!    connection.
//!
//! A refusal is decided at prepare time, or by the authorizer during a step,
//! and the op returns the [`Refusal`] instead of any rows: never a partial
//! result.
//!
//! The authorizer is installed on the checked-out pool connection for the
//! statement's lifetime and removed when [`query`] returns, so the pool's
//! other users see the connection unchanged.

use std::fmt;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
use rusqlite::{Connection, Statement, ffi};

/// The functions a `query` statement may call. Every SQLite built-in that
/// computes a value from its arguments alone (core scalar, aggregate, window,
/// date and JSON functions), and the HDC functions the daemon registers on a
/// reader connection (`popcount_xor`, `bundle`, `bundle_majority`).
///
/// Left out: `load_extension` (loads code into the daemon), `sqlite_log`
/// (writes the SQLite error log), and `changes`, `total_changes` and
/// `last_insert_rowid`, which report the history of the shared pool
/// connection rather than the database. Any function an extension or a later
/// SQLite adds is denied until it is listed here;
/// `every_builtin_function_is_classified` fails until it is.
pub const ALLOWED_FUNCTIONS: &[&str] = &[
    "->",
    "->>",
    "abs",
    "avg",
    "char",
    "coalesce",
    "concat",
    "concat_ws",
    "count",
    "cume_dist",
    "current_date",
    "current_time",
    "current_timestamp",
    "date",
    "datetime",
    "dense_rank",
    "first_value",
    "format",
    "glob",
    "group_concat",
    "hex",
    "if",
    "ifnull",
    "iif",
    "instr",
    "json",
    "json_array",
    "json_array_length",
    "json_error_position",
    "json_extract",
    "json_group_array",
    "json_group_object",
    "json_insert",
    "json_object",
    "json_patch",
    "json_pretty",
    "json_quote",
    "json_remove",
    "json_replace",
    "json_set",
    "json_type",
    "json_valid",
    "jsonb",
    "jsonb_array",
    "jsonb_extract",
    "jsonb_group_array",
    "jsonb_group_object",
    "jsonb_insert",
    "jsonb_object",
    "jsonb_patch",
    "jsonb_remove",
    "jsonb_replace",
    "jsonb_set",
    "julianday",
    "lag",
    "last_value",
    "lead",
    "length",
    "like",
    "likelihood",
    "likely",
    "lower",
    "ltrim",
    "max",
    "min",
    "nth_value",
    "ntile",
    "nullif",
    "octet_length",
    "percent_rank",
    "printf",
    "quote",
    "random",
    "randomblob",
    "rank",
    "replace",
    "round",
    "row_number",
    "rtrim",
    "sign",
    "soundex",
    "sqlite_compileoption_get",
    "sqlite_compileoption_used",
    "sqlite_source_id",
    "sqlite_version",
    "strftime",
    "string_agg",
    "substr",
    "substring",
    "subtype",
    "sum",
    "time",
    "timediff",
    "total",
    "trim",
    "typeof",
    "unhex",
    "unicode",
    "unistr",
    "unistr_quote",
    "unixepoch",
    "unlikely",
    "upper",
    "zeroblob",
    // HDC (`leyline_hdc::sql_udf`): pure functions over hypervector blobs.
    "popcount_xor",
    "bundle",
    "bundle_majority",
];

/// SQLite built-ins deliberately left out of [`ALLOWED_FUNCTIONS`]; see its
/// documentation for why each is denied.
pub const DENIED_BUILTIN_FUNCTIONS: &[&str] = &[
    "changes",
    "last_insert_rowid",
    "load_extension",
    "sqlite_log",
    "total_changes",
];

/// The pragmas a `query` statement may reach, all through their table-valued
/// form (`SELECT name FROM pragma_table_info('nodes')`): schema introspection
/// with no side effect, whose argument names a table or index, never a
/// setting. The statement form (`PRAGMA table_info(nodes)`) reports no
/// `SELECT` and is refused by the read-only-`SELECT` check. Every other
/// pragma is denied, including `writable_schema`, `query_only` and
/// `database_list`, which would name the daemon's host paths.
pub const ALLOWED_PRAGMAS: &[&str] = &[
    "foreign_key_list",
    "index_info",
    "index_list",
    "index_xinfo",
    "table_info",
    "table_list",
    "table_xinfo",
];

/// Why a `query` statement was refused. On the wire it is
/// `QueryRefusal.reason` (`daemon.capnp`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalReason {
    /// The authorizer denied an action the statement takes; `action` names it.
    DeniedAction,
    /// The SQL holds more than one statement.
    MultipleStatements,
    /// The statement is not a read-only `SELECT`; `action` is its leading
    /// keyword.
    NotAQuery,
}

impl RefusalReason {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeniedAction => "denied_action",
            Self::MultipleStatements => "multiple_statements",
            Self::NotAQuery => "not_a_query",
        }
    }
}

/// A refused `query` statement. Nothing ran: the op returns this instead of
/// any rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// Why.
    pub reason: RefusalReason,
    /// The refused action: an authorizer action in snake case (`drop_table`,
    /// `attach`, `pragma`, `function`, ...) for [`RefusalReason::DeniedAction`],
    /// the statement's leading keyword (`vacuum`) for
    /// [`RefusalReason::NotAQuery`], `statement` for
    /// [`RefusalReason::MultipleStatements`].
    pub action: String,
    /// What the action names, when it names something: the table, pragma,
    /// function or attached file. Empty otherwise.
    pub subject: String,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "query refused ({}): ", self.reason.as_str())?;
        match self.reason {
            RefusalReason::DeniedAction => {
                write!(
                    f,
                    "the query op is read-only and does not allow `{}`",
                    self.action
                )?;
                if !self.subject.is_empty() {
                    write!(f, " on `{}`", self.subject)?;
                }
                Ok(())
            }
            RefusalReason::MultipleStatements => {
                f.write_str("the query op runs one statement, and the SQL holds more than one")
            }
            RefusalReason::NotAQuery => write!(
                f,
                "the query op runs only a read-only SELECT, not `{}`",
                self.action
            ),
        }
    }
}

impl std::error::Error for Refusal {}

/// Prepare `sql` as a read-only `SELECT` on `conn` and hand the statement to
/// `run`. A statement that is not one comes back as an error holding a
/// [`Refusal`] (`downcast_ref::<Refusal>()`), before `run` sees it; an action
/// the authorizer denies while `run` steps the statement comes back the same
/// way. Other errors (a syntax error, a missing table) pass through as
/// before.
pub fn query<T>(
    conn: &Connection,
    sql: &str,
    run: impl FnOnce(&mut Statement<'_>) -> Result<T>,
) -> Result<T> {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let _authorizer = Authorizer::install(conn, Arc::clone(&seen))?;

    let mut stmt = match conn.prepare(sql) {
        Ok(stmt) => stmt,
        Err(rusqlite::Error::MultipleStatement) => {
            return Err(Refusal {
                reason: RefusalReason::MultipleStatements,
                action: "statement".into(),
                subject: String::new(),
            }
            .into());
        }
        Err(e) => {
            return Err(denial_or(
                &seen,
                anyhow::Error::new(e).context("prepare SQL"),
            ));
        }
    };
    let saw_select = lock(&seen).saw_select;
    if !saw_select || !stmt.readonly() {
        return Err(Refusal {
            reason: RefusalReason::NotAQuery,
            action: leading_keyword(sql),
            subject: String::new(),
        }
        .into());
    }
    let out = run(&mut stmt).map_err(|e| denial_or(&seen, e))?;
    // A denial `run` swallowed (an iterator that drops a step's error) is
    // still a refusal: the result it built may be truncated.
    match lock(&seen).denied.take() {
        Some(refusal) => Err(refusal.into()),
        None => Ok(out),
    }
}

/// What the authorizer saw while a statement was prepared and stepped.
#[derive(Default)]
struct Seen {
    saw_select: bool,
    denied: Option<Refusal>,
}

fn lock(seen: &Mutex<Seen>) -> std::sync::MutexGuard<'_, Seen> {
    // The authorizer only records into `Seen`; a panic while it held the
    // lock leaves a value that is still whole.
    seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The first action the authorizer denied, as the error; `err` when it
/// denied none.
fn denial_or(seen: &Mutex<Seen>, err: anyhow::Error) -> anyhow::Error {
    match lock(seen).denied.take() {
        Some(refusal) => refusal.into(),
        None => err,
    }
}

/// The authorizer, installed on a connection until dropped.
struct Authorizer<'c>(&'c Connection);

impl<'c> Authorizer<'c> {
    fn install(conn: &'c Connection, seen: Arc<Mutex<Seen>>) -> Result<Self> {
        conn.authorizer(Some(move |ctx: AuthContext<'_>| {
            let mut seen = lock(&seen);
            match judge(&ctx.action) {
                Judgement::Select => {
                    seen.saw_select = true;
                    Authorization::Allow
                }
                Judgement::Allow => Authorization::Allow,
                Judgement::Deny(refusal) => {
                    seen.denied.get_or_insert(refusal);
                    Authorization::Deny
                }
            }
        }))?;
        Ok(Self(conn))
    }
}

impl Drop for Authorizer<'_> {
    fn drop(&mut self) {
        // Removing a hook cannot fail on a connection that accepted one.
        let _ = self
            .0
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Judgement {
    Select,
    Allow,
    Deny(Refusal),
}

/// The authorizer's decision for one action: deny unless allowed.
fn judge(action: &AuthAction<'_>) -> Judgement {
    match action {
        AuthAction::Select => Judgement::Select,
        AuthAction::Read { .. } | AuthAction::Recursive => Judgement::Allow,
        AuthAction::Function { function_name } if listed(ALLOWED_FUNCTIONS, function_name) => {
            Judgement::Allow
        }
        AuthAction::Pragma { pragma_name, .. } if listed(ALLOWED_PRAGMAS, pragma_name) => {
            Judgement::Allow
        }
        denied => {
            let (action, subject) = describe(denied);
            Judgement::Deny(Refusal {
                reason: RefusalReason::DeniedAction,
                action: action.into(),
                subject,
            })
        }
    }
}

/// SQLite matches function and pragma names without regard to ASCII case.
fn listed(list: &[&str], name: &str) -> bool {
    list.iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(name))
}

/// A denied action's wire name and the thing it names.
fn describe(action: &AuthAction<'_>) -> (&'static str, String) {
    use AuthAction as A;
    let (name, subject): (&'static str, &str) = match *action {
        A::CreateIndex { index_name, .. } => ("create_index", index_name),
        A::CreateTable { table_name } => ("create_table", table_name),
        A::CreateTempIndex { index_name, .. } => ("create_temp_index", index_name),
        A::CreateTempTable { table_name } => ("create_temp_table", table_name),
        A::CreateTempTrigger { trigger_name, .. } => ("create_temp_trigger", trigger_name),
        A::CreateTempView { view_name } => ("create_temp_view", view_name),
        A::CreateTrigger { trigger_name, .. } => ("create_trigger", trigger_name),
        A::CreateView { view_name } => ("create_view", view_name),
        A::Delete { table_name } => ("delete", table_name),
        A::DropIndex { index_name, .. } => ("drop_index", index_name),
        A::DropTable { table_name } => ("drop_table", table_name),
        A::DropTempIndex { index_name, .. } => ("drop_temp_index", index_name),
        A::DropTempTable { table_name } => ("drop_temp_table", table_name),
        A::DropTempTrigger { trigger_name, .. } => ("drop_temp_trigger", trigger_name),
        A::DropTempView { view_name } => ("drop_temp_view", view_name),
        A::DropTrigger { trigger_name, .. } => ("drop_trigger", trigger_name),
        A::DropView { view_name } => ("drop_view", view_name),
        A::Insert { table_name } => ("insert", table_name),
        A::Pragma { pragma_name, .. } => ("pragma", pragma_name),
        A::Read { table_name, .. } => ("read", table_name),
        A::Select => ("select", ""),
        A::Transaction { operation } => ("transaction", transaction_name(operation)),
        A::Update { table_name, .. } => ("update", table_name),
        A::Attach { filename } => ("attach", filename),
        A::Detach { database_name } => ("detach", database_name),
        A::AlterTable { table_name, .. } => ("alter_table", table_name),
        A::Reindex { index_name } => ("reindex", index_name),
        A::Analyze { table_name } => ("analyze", table_name),
        A::CreateVtable { table_name, .. } => ("create_vtable", table_name),
        A::DropVtable { table_name, .. } => ("drop_vtable", table_name),
        A::Function { function_name } => ("function", function_name),
        A::Savepoint { savepoint_name, .. } => ("savepoint", savepoint_name),
        A::Recursive => ("recursive", ""),
        // rusqlite reports an `ATTACH` or `DETACH` whose operand is an
        // expression, not a literal, as an unknown action.
        A::Unknown { code, arg1, .. } => match code {
            ffi::SQLITE_ATTACH => ("attach", arg1.unwrap_or("")),
            ffi::SQLITE_DETACH => ("detach", arg1.unwrap_or("")),
            _ => ("unknown", arg1.unwrap_or("")),
        },
        _ => ("unknown", ""),
    };
    (name, subject.to_string())
}

fn transaction_name(operation: TransactionOperation) -> &'static str {
    match operation {
        TransactionOperation::Begin => "begin",
        TransactionOperation::Release => "release",
        TransactionOperation::Rollback => "rollback",
        _ => "unknown",
    }
}

/// The first keyword of `sql`, lower case, past white space and comments:
/// what a [`RefusalReason::NotAQuery`] refusal names. It labels the refusal
/// and decides nothing.
fn leading_keyword(sql: &str) -> String {
    let mut rest = sql;
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, tail)| tail);
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, tail)| tail);
        } else {
            break;
        }
    }
    let word: String = rest
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if word.is_empty() {
        "empty".into()
    } else {
        word
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file-backed db with one table, and a `query_only` reader on it,
    /// as the daemon's reader pool opens them.
    fn reader() -> (tempfile::TempDir, Connection, Connection) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("live.db");
        let writer = Connection::open(&path).unwrap();
        writer
            .execute_batch("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1), (2);")
            .unwrap();
        let reader = Connection::open(&path).unwrap();
        reader.pragma_update(None, "query_only", "ON").unwrap();
        (dir, writer, reader)
    }

    fn rows(conn: &Connection, sql: &str) -> Result<Vec<String>> {
        query(conn, sql, |stmt| {
            let mut out = Vec::new();
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                out.push(format!("{:?}", row.get_ref(0)?));
            }
            Ok(out)
        })
    }

    fn refusal(conn: &Connection, sql: &str) -> Refusal {
        let err = rows(conn, sql).expect_err(sql);
        err.downcast_ref::<Refusal>()
            .unwrap_or_else(|| panic!("{sql}: not a refusal: {err:#}"))
            .clone()
    }

    #[test]
    fn a_select_returns_its_rows() {
        let (_dir, _w, r) = reader();
        assert_eq!(rows(&r, "SELECT a FROM t ORDER BY a").unwrap().len(), 2);
        assert_eq!(
            rows(&r, "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 3) SELECT upper('a') || x FROM n")
                .unwrap()
                .len(),
            3,
        );
        assert_eq!(
            rows(&r, "SELECT name FROM pragma_table_info('t')")
                .unwrap()
                .len(),
            1,
            "table-valued schema introspection is a read",
        );
    }

    #[test]
    fn a_non_refusal_error_passes_through() {
        let (_dir, _w, r) = reader();
        let err = rows(&r, "SELECT nope FROM missing").unwrap_err();
        assert!(err.downcast_ref::<Refusal>().is_none(), "{err:#}");
        assert!(format!("{err:#}").contains("no such table"), "{err:#}");
    }

    #[test]
    fn statements_that_write_or_reach_the_host_are_refused() {
        let (dir, _w, r) = reader();
        let host = dir.path().join("exfil.db");
        let host = host.to_str().unwrap();
        let denied = |sql: &str, action: &str, subject: &str| {
            let got = refusal(&r, sql);
            assert_eq!(
                (got.reason, got.action.as_str(), got.subject.as_str()),
                (RefusalReason::DeniedAction, action, subject),
                "{sql}",
            );
        };
        // A DROP or a CREATE asks first to write the schema table, and the
        // first denial ends the statement.
        denied("DROP TABLE t", "delete", "sqlite_master");
        denied("INSERT INTO t VALUES (3)", "insert", "t");
        denied("UPDATE t SET a = 0", "update", "t");
        denied("DELETE FROM t", "delete", "t");
        denied("WITH x AS (SELECT 1) DELETE FROM t", "delete", "t");
        denied(
            "WITH x AS (SELECT 9) INSERT INTO t SELECT * FROM x",
            "insert",
            "t",
        );
        denied(&format!("ATTACH '{host}' AS e"), "attach", host);
        denied(&format!("ATTACH (SELECT '{host}') AS e"), "attach", "");
        denied("DETACH main", "detach", "main");
        denied("PRAGMA writable_schema = ON", "pragma", "writable_schema");
        denied(
            "SELECT * FROM pragma_database_list",
            "pragma",
            "database_list",
        );
        denied("SELECT load_extension('x')", "function", "load_extension");
        denied("SELECT changes()", "function", "changes");
        denied("BEGIN", "transaction", "begin");
        denied("CREATE TEMP TABLE s (x)", "insert", "sqlite_temp_master");
        denied(
            "CREATE TRIGGER g AFTER INSERT ON t BEGIN DELETE FROM t; END",
            "create_trigger",
            "g",
        );
        denied("SELECT 1; DROP TABLE t", "delete", "sqlite_master");

        let vacuum = refusal(&r, &format!("VACUUM INTO '{host}'"));
        assert_eq!(
            (vacuum.reason, vacuum.action.as_str()),
            (RefusalReason::NotAQuery, "vacuum"),
        );
        let stacked = refusal(&r, "SELECT 1; SELECT 2");
        assert_eq!(
            (stacked.reason, stacked.action.as_str()),
            (RefusalReason::MultipleStatements, "statement"),
        );
        assert_eq!(refusal(&r, "").reason, RefusalReason::NotAQuery);
        assert_eq!(
            refusal(&r, "PRAGMA table_info(t)").reason,
            RefusalReason::NotAQuery
        );

        assert!(
            !std::path::Path::new(host).exists(),
            "nothing reached the host path"
        );
        assert_eq!(rows(&r, "SELECT a FROM t").unwrap().len(), 2, "t is whole");
    }

    /// A step the authorizer denied is a refusal even when `run` swallows
    /// the step's error: the caller never gets a truncated result as success.
    #[test]
    fn a_denial_during_a_step_is_a_refusal_even_if_run_swallows_it() {
        let (_dir, _w, r) = reader();
        let err = query(
            &r,
            "SELECT a FROM t UNION ALL SELECT name FROM pragma_database_list",
            |stmt| Ok(stmt.query_map([], |_| Ok(()))?.count()),
        )
        .expect_err("a swallowed denial must still refuse");
        let refusal = err.downcast_ref::<Refusal>().expect("a refusal");
        assert_eq!(
            (refusal.action.as_str(), refusal.subject.as_str()),
            ("pragma", "database_list")
        );
    }

    /// The authorizer is gone once `query` returns: the pool connection
    /// serves the daemon's other read ops as before.
    #[test]
    fn the_authorizer_is_removed_after_the_statement() {
        let (_dir, _w, r) = reader();
        refusal(&r, "DROP TABLE t");
        let n: i64 = r
            .query_row("SELECT changes()", [], |row| row.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn every_builtin_function_is_classified() {
        let (_dir, _w, r) = reader();
        let mut stmt = r
            .prepare("SELECT DISTINCT name FROM pragma_function_list WHERE builtin = 1")
            .unwrap();
        let builtins: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|n| n.unwrap())
            .collect();
        let unclassified: Vec<&String> = builtins
            .iter()
            .filter(|n| !listed(ALLOWED_FUNCTIONS, n) && !listed(DENIED_BUILTIN_FUNCTIONS, n))
            .collect();
        assert!(
            unclassified.is_empty(),
            "SQLite built-ins neither allowed nor denied: {unclassified:?}"
        );
        for name in DENIED_BUILTIN_FUNCTIONS {
            assert!(
                builtins.contains(&name.to_string()),
                "{name} is not a built-in"
            );
            assert!(
                !listed(ALLOWED_FUNCTIONS, name),
                "{name} is both allowed and denied"
            );
        }
    }

    #[test]
    fn names_compare_without_ascii_case() {
        assert!(listed(ALLOWED_FUNCTIONS, "BUNDLE"));
        assert!(listed(ALLOWED_PRAGMAS, "Table_Info"));
        assert!(!listed(ALLOWED_PRAGMAS, "table_infox"));
    }

    /// Every action the authorizer can report has its own wire name, and
    /// only the allowed ones are allowed.
    #[test]
    fn every_action_is_judged_and_named() {
        use AuthAction as A;
        let deny = |action: &str, subject: &str| {
            Judgement::Deny(Refusal {
                reason: RefusalReason::DeniedAction,
                action: action.into(),
                subject: subject.into(),
            })
        };
        let (i, t, g, v) = ("i", "t", "g", "v");
        let table: Vec<(A<'_>, Judgement)> = vec![
            (A::Select, Judgement::Select),
            (
                A::Read {
                    table_name: t,
                    column_name: "c",
                },
                Judgement::Allow,
            ),
            (A::Recursive, Judgement::Allow),
            (
                A::Function {
                    function_name: "abs",
                },
                Judgement::Allow,
            ),
            (
                A::Pragma {
                    pragma_name: "table_info",
                    pragma_value: Some(t),
                },
                Judgement::Allow,
            ),
            (A::Function { function_name: "f" }, deny("function", "f")),
            (
                A::Pragma {
                    pragma_name: "query_only",
                    pragma_value: None,
                },
                deny("pragma", "query_only"),
            ),
            (
                A::CreateIndex {
                    index_name: i,
                    table_name: t,
                },
                deny("create_index", i),
            ),
            (A::CreateTable { table_name: t }, deny("create_table", t)),
            (
                A::CreateTempIndex {
                    index_name: i,
                    table_name: t,
                },
                deny("create_temp_index", i),
            ),
            (
                A::CreateTempTable { table_name: t },
                deny("create_temp_table", t),
            ),
            (
                A::CreateTempTrigger {
                    trigger_name: g,
                    table_name: t,
                },
                deny("create_temp_trigger", g),
            ),
            (
                A::CreateTempView { view_name: v },
                deny("create_temp_view", v),
            ),
            (
                A::CreateTrigger {
                    trigger_name: g,
                    table_name: t,
                },
                deny("create_trigger", g),
            ),
            (A::CreateView { view_name: v }, deny("create_view", v)),
            (A::Delete { table_name: t }, deny("delete", t)),
            (
                A::DropIndex {
                    index_name: i,
                    table_name: t,
                },
                deny("drop_index", i),
            ),
            (A::DropTable { table_name: t }, deny("drop_table", t)),
            (
                A::DropTempIndex {
                    index_name: i,
                    table_name: t,
                },
                deny("drop_temp_index", i),
            ),
            (
                A::DropTempTable { table_name: t },
                deny("drop_temp_table", t),
            ),
            (
                A::DropTempTrigger {
                    trigger_name: g,
                    table_name: t,
                },
                deny("drop_temp_trigger", g),
            ),
            (A::DropTempView { view_name: v }, deny("drop_temp_view", v)),
            (
                A::DropTrigger {
                    trigger_name: g,
                    table_name: t,
                },
                deny("drop_trigger", g),
            ),
            (A::DropView { view_name: v }, deny("drop_view", v)),
            (A::Insert { table_name: t }, deny("insert", t)),
            (
                A::Transaction {
                    operation: TransactionOperation::Begin,
                },
                deny("transaction", "begin"),
            ),
            (
                A::Transaction {
                    operation: TransactionOperation::Release,
                },
                deny("transaction", "release"),
            ),
            (
                A::Transaction {
                    operation: TransactionOperation::Rollback,
                },
                deny("transaction", "rollback"),
            ),
            (
                A::Transaction {
                    operation: TransactionOperation::Unknown,
                },
                deny("transaction", "unknown"),
            ),
            (
                A::Update {
                    table_name: t,
                    column_name: "c",
                },
                deny("update", t),
            ),
            (A::Attach { filename: "/f" }, deny("attach", "/f")),
            (A::Detach { database_name: "d" }, deny("detach", "d")),
            (
                A::AlterTable {
                    database_name: "main",
                    table_name: t,
                },
                deny("alter_table", t),
            ),
            (A::Reindex { index_name: i }, deny("reindex", i)),
            (A::Analyze { table_name: t }, deny("analyze", t)),
            (
                A::CreateVtable {
                    table_name: t,
                    module_name: "m",
                },
                deny("create_vtable", t),
            ),
            (
                A::DropVtable {
                    table_name: t,
                    module_name: "m",
                },
                deny("drop_vtable", t),
            ),
            (
                A::Savepoint {
                    operation: TransactionOperation::Begin,
                    savepoint_name: "s",
                },
                deny("savepoint", "s"),
            ),
            (
                A::Unknown {
                    code: ffi::SQLITE_ATTACH,
                    arg1: None,
                    arg2: None,
                },
                deny("attach", ""),
            ),
            (
                A::Unknown {
                    code: ffi::SQLITE_DETACH,
                    arg1: Some("d"),
                    arg2: None,
                },
                deny("detach", "d"),
            ),
            (
                A::Unknown {
                    code: 999,
                    arg1: Some("x"),
                    arg2: None,
                },
                deny("unknown", "x"),
            ),
        ];
        for (action, want) in table {
            assert_eq!(judge(&action), want, "{action:?}");
        }
        // Allowed actions still have names, for completeness of `describe`.
        assert_eq!(describe(&A::Select), ("select", String::new()));
        assert_eq!(describe(&A::Recursive), ("recursive", String::new()));
        assert_eq!(
            describe(&A::Read {
                table_name: t,
                column_name: "c"
            }),
            ("read", t.to_string())
        );
    }

    #[test]
    fn a_refusal_names_its_reason_action_and_subject() {
        let r = Refusal {
            reason: RefusalReason::DeniedAction,
            action: "drop_table".into(),
            subject: "nodes".into(),
        };
        assert_eq!(
            r.to_string(),
            "query refused (denied_action): the query op is read-only and does not allow `drop_table` on `nodes`"
        );
        let r = Refusal {
            reason: RefusalReason::NotAQuery,
            action: "vacuum".into(),
            subject: String::new(),
        };
        assert_eq!(
            r.to_string(),
            "query refused (not_a_query): the query op runs only a read-only SELECT, not `vacuum`"
        );
        let r = Refusal {
            reason: RefusalReason::MultipleStatements,
            action: "statement".into(),
            subject: String::new(),
        };
        assert_eq!(
            r.to_string(),
            "query refused (multiple_statements): the query op runs one statement, and the SQL holds more than one"
        );
        let r = Refusal {
            reason: RefusalReason::DeniedAction,
            action: "transaction".into(),
            subject: String::new(),
        };
        assert_eq!(
            r.to_string(),
            "query refused (denied_action): the query op is read-only and does not allow `transaction`"
        );
    }

    #[test]
    fn the_leading_keyword_skips_space_and_comments() {
        assert_eq!(leading_keyword("  VACUUM INTO 'x'"), "vacuum");
        assert_eq!(leading_keyword("-- note\n/* c */ Begin"), "begin");
        assert_eq!(leading_keyword("-- only a comment"), "empty");
        assert_eq!(leading_keyword("/* open"), "empty");
        assert_eq!(leading_keyword(""), "empty");
    }
}
