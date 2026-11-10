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
//!    [`AuthAction::Recursive`], the functions in [`ALLOWED_FUNCTIONS`], the
//!    pragmas in [`ALLOWED_PRAGMAS`], and an [`AuthAction::Read`] of a plain
//!    table or view of `main`, of a module in [`ALLOWED_MODULES`] or of an
//!    allowed pragma's `pragma_` table (bead `ley-line-open-a9be1f`). A read
//!    of any other virtual-table module, a table-valued function a later
//!    extension adds, or a virtual table in the schema, is refused when the
//!    statement is prepared; `Catalog` says how a module is told from a
//!    table. Every other action is denied, including ones this build does
//!    not know: an
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
    // SQLite 3.54 (rusqlite 0.41, bead `ley-line-open-6ef210`): inserts into
    // a JSON array value and returns the result, like `json_insert`.
    "json_array_insert",
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
    "jsonb_array_insert",
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

/// The virtual-table modules a `query` statement may read as a table-valued
/// function or an eponymous table (bead `ley-line-open-a9be1f`): read-only
/// views of their arguments or of the database's own pages.
///
/// - `json_each`, `json_tree`: the rows of a JSON value.
/// - `dbstat`: the database's b-tree pages.
/// - `vec_each`: the elements of a vector, when sqlite-vec is registered.
///
/// The table-valued pragmas in [`ALLOWED_PRAGMAS`] (`pragma_table_info`,
/// ...) are allowed by that list. Every other module is refused, including
/// ones a later SQLite, sqlite-vec or another `sqlite3_auto_extension`
/// adds; `every_module_is_classified` fails until such a module is listed
/// here or in [`DENIED_MODULES`].
pub const ALLOWED_MODULES: &[&str] = &["dbstat", "json_each", "json_tree", "vec_each"];

/// The modules of this build left out of [`ALLOWED_MODULES`]. The list
/// classifies them; the authorizer refuses every module not allowed, listed
/// here or not.
///
/// - The full-text and R-tree modules (`fts3`, `fts4`, `fts5`, `rtree`, ...)
///   back virtual tables created in the schema, and their auxiliary modules
///   (`fts3tokenize`, `fts4aux`, `fts5vocab`) read those tables' internals.
/// - `jsonb_each` and `jsonb_tree`: not needed beside their JSON-text forms.
/// - `pragma_module_list`: a pragma outside [`ALLOWED_PRAGMAS`].
/// - sqlite-vec's `vec0`: backs vector tables created in a schema.
pub const DENIED_MODULES: &[&str] = &[
    "fts3",
    "fts3tokenize",
    "fts4",
    "fts4aux",
    "fts5",
    "fts5vocab",
    "jsonb_each",
    "jsonb_tree",
    "pragma_module_list",
    "rtree",
    "rtree_i32",
    "vec0",
];

/// The schema table of `main`, which `main`'s schema does not list, under
/// the name the authorizer reports for it (`sqlite_schema` is reported as
/// `sqlite_master`). `sqlite_temp_master` is left out: reading it opens the
/// connection's temp database.
const SCHEMA_TABLES: &[&str] = &["sqlite_master"];

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
    conn: &mut Connection,
    sql: &str,
    run: impl FnOnce(&mut Statement<'_>) -> Result<T>,
) -> Result<T> {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let catalog = Catalog::read(conn)?;
    // Every statement borrows the connection through the guard, so each one
    // is dropped before the guard removes the authorizer.
    let authorized = Authorizer::install(conn, catalog, Arc::clone(&seen))?;

    let mut stmt = match authorized.conn().prepare(sql) {
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

/// The authorizer, installed on a connection until dropped. It holds the
/// connection exclusively: rusqlite (0.41+) installs and removes hooks
/// through `&mut Connection`, and statements reach the connection only
/// through [`Authorizer::conn`], so none outlives the hook.
struct Authorizer<'c>(&'c mut Connection);

impl<'c> Authorizer<'c> {
    fn install(conn: &'c mut Connection, catalog: Catalog, seen: Arc<Mutex<Seen>>) -> Result<Self> {
        conn.authorizer(Some(move |ctx: AuthContext<'_>| {
            let mut seen = lock(&seen);
            match judge(&ctx.action, &catalog) {
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

    /// The connection, with the authorizer installed.
    fn conn(&self) -> &Connection {
        self.0
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

/// What a `Read` may name, read from the connection before the authorizer
/// is installed: which names are plain schema tables and which are
/// virtual-table modules.
///
/// SQLite reports a read of a table, a view, a virtual table and a
/// table-valued function alike, as [`AuthAction::Read`] naming the table
/// (with an empty column when no column is used), so the name is the only
/// thing the authorizer can judge. SQLite resolves a name to a schema table
/// first and to an eponymous module only when no schema table has it.
///
/// - `tables`: every table and view in the `main` and `temp` schemas
///   (`sqlite_schema`), less the virtual tables (type `table` with
///   `rootpage` 0, which runs its module's code on every read).
/// - `modules`: every module registered on the connection
///   (`pragma_module_list`). SQLite registers the JSON and pragma modules
///   only when a statement first names them, so a name missing here can
///   still be a module.
///
/// Neither list is decided by reading the SQL text of the statement.
#[derive(Debug, Default)]
struct Catalog {
    tables: Vec<String>,
    modules: Vec<String>,
}

impl Catalog {
    fn read(conn: &Connection) -> Result<Self> {
        let names = |sql: &str| -> Result<Vec<String>> {
            let mut stmt = conn.prepare(sql)?;
            let names = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(names)
        };
        // Only `main`: naming `temp` would open the connection's temp
        // database, and `query` can create nothing there.
        let mut tables = names(
            "SELECT name FROM main.sqlite_schema \
             WHERE type IN ('table', 'view') AND NOT (type = 'table' AND rootpage = 0)",
        )?;
        tables.extend(SCHEMA_TABLES.iter().map(|t| t.to_string()));
        let modules = names("SELECT name FROM pragma_module_list")?;
        Ok(Self { tables, modules })
    }

    /// `name` is read as a plain schema table: it is one, and no module has
    /// its name. A schema table that shares an unlisted module's name is
    /// refused, because a writer that drops the table between this
    /// catalog's read and the prepare would turn the name into the module.
    fn is_plain_table(&self, name: &str) -> bool {
        listed_owned(&self.tables, name) && !listed_owned(&self.modules, name)
    }
}

fn listed_owned(list: &[String], name: &str) -> bool {
    list.iter().any(|item| item.eq_ignore_ascii_case(name))
}

/// The authorizer's decision for one action: deny unless allowed.
fn judge(action: &AuthAction<'_>, catalog: &Catalog) -> Judgement {
    match action {
        AuthAction::Select => Judgement::Select,
        AuthAction::Recursive => Judgement::Allow,
        AuthAction::Read { table_name, .. } => judge_read(table_name, catalog),
        AuthAction::Function { function_name } if listed(ALLOWED_FUNCTIONS, function_name) => {
            Judgement::Allow
        }
        AuthAction::Pragma { pragma_name, .. } if listed(ALLOWED_PRAGMAS, pragma_name) => {
            Judgement::Allow
        }
        denied => {
            let (action, subject) = describe(denied);
            deny(action, subject)
        }
    }
}

/// A read of `table`: a plain schema table, an allowed module, or a
/// `pragma_` table whose pragma is allowed. Anything else is refused when
/// the statement is prepared: an unlisted `pragma_` table as its pragma,
/// whatever else as a `read` of that name.
fn judge_read(table: &str, catalog: &Catalog) -> Judgement {
    if let Some(pragma) = strip_prefix_ignore_ascii_case(table, "pragma_") {
        return if listed(ALLOWED_PRAGMAS, pragma) {
            Judgement::Allow
        } else {
            deny("pragma", pragma.to_string())
        };
    }
    if listed(ALLOWED_MODULES, table) || catalog.is_plain_table(table) {
        Judgement::Allow
    } else {
        deny("read", table.to_string())
    }
}

fn deny(action: &str, subject: String) -> Judgement {
    Judgement::Deny(Refusal {
        reason: RefusalReason::DeniedAction,
        action: action.into(),
        subject,
    })
}

fn strip_prefix_ignore_ascii_case<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
    let head = name.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &name[prefix.len()..])
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

/// The first keyword of `sql`, lower case, past white space, comments, NUL
/// and the empty statements of a leading `;`, all of which SQLite skips:
/// what a [`RefusalReason::NotAQuery`] refusal names. It labels the refusal
/// and decides nothing.
fn leading_keyword(sql: &str) -> String {
    let mut rest = sql;
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == ';' || c == '\0');
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

    fn rows(conn: &mut Connection, sql: &str) -> Result<Vec<String>> {
        query(conn, sql, |stmt| {
            let mut out = Vec::new();
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                out.push(format!("{:?}", row.get_ref(0)?));
            }
            Ok(out)
        })
    }

    fn refusal(conn: &mut Connection, sql: &str) -> Refusal {
        let err = rows(conn, sql).expect_err(sql);
        err.downcast_ref::<Refusal>()
            .unwrap_or_else(|| panic!("{sql}: not a refusal: {err:#}"))
            .clone()
    }

    #[test]
    fn a_select_returns_its_rows() {
        let (_dir, _w, mut r) = reader();
        assert_eq!(rows(&mut r, "SELECT a FROM t ORDER BY a").unwrap().len(), 2);
        assert_eq!(
            rows(&mut r,"WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 3) SELECT upper('a') || x FROM n")
                .unwrap()
                .len(),
            3,
        );
        assert_eq!(
            rows(&mut r, "SELECT name FROM pragma_table_info('t')")
                .unwrap()
                .len(),
            1,
            "table-valued schema introspection is a read",
        );
    }

    #[test]
    fn a_non_refusal_error_passes_through() {
        let (_dir, _w, mut r) = reader();
        let err = rows(&mut r, "SELECT nope FROM missing").unwrap_err();
        assert!(err.downcast_ref::<Refusal>().is_none(), "{err:#}");
        assert!(format!("{err:#}").contains("no such table"), "{err:#}");
    }

    #[test]
    fn statements_that_write_or_reach_the_host_are_refused() {
        let (dir, _w, mut r) = reader();
        let host = dir.path().join("exfil.db");
        let host = host.to_str().unwrap();
        let mut denied = |sql: &str, action: &str, subject: &str| {
            let got = refusal(&mut r, sql);
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

        let vacuum = refusal(&mut r, &format!("VACUUM INTO '{host}'"));
        assert_eq!(
            (vacuum.reason, vacuum.action.as_str()),
            (RefusalReason::NotAQuery, "vacuum"),
        );
        let stacked = refusal(&mut r, "SELECT 1; SELECT 2");
        assert_eq!(
            (stacked.reason, stacked.action.as_str()),
            (RefusalReason::MultipleStatements, "statement"),
        );
        assert_eq!(refusal(&mut r, "").reason, RefusalReason::NotAQuery);
        assert_eq!(
            refusal(&mut r, "PRAGMA table_info(t)").reason,
            RefusalReason::NotAQuery
        );

        assert!(
            !std::path::Path::new(host).exists(),
            "nothing reached the host path"
        );
        assert_eq!(
            rows(&mut r, "SELECT a FROM t").unwrap().len(),
            2,
            "t is whole"
        );
    }

    /// A step the authorizer denied is a refusal even when `run` swallows
    /// the step's error: the caller never gets a truncated result as success.
    #[test]
    fn a_denial_during_a_step_is_a_refusal_even_if_run_swallows_it() {
        let (_dir, _w, mut r) = reader();
        deny_on_two(&r);
        let err = query(&mut r, "SELECT abs(a) FROM t ORDER BY rowid", |stmt| {
            Ok(stmt.query_map([], |_| Ok(()))?.count())
        })
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
        let (_dir, _w, mut r) = reader();
        refusal(&mut r, "DROP TABLE t");
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
                A::Read {
                    table_name: "JSON_EACH",
                    column_name: "",
                },
                Judgement::Allow,
            ),
            (
                A::Read {
                    table_name: "Pragma_Table_Info",
                    column_name: "name",
                },
                Judgement::Allow,
            ),
            (
                A::Read {
                    table_name: "pragma_database_list",
                    column_name: "file",
                },
                deny("pragma", "database_list"),
            ),
            (
                A::Read {
                    table_name: "generate_series",
                    column_name: "value",
                },
                deny("read", "generate_series"),
            ),
            (
                A::Read {
                    table_name: "series",
                    column_name: "",
                },
                deny("read", "series"),
            ),
            (
                A::Read {
                    table_name: "pragma",
                    column_name: "",
                },
                deny("read", "pragma"),
            ),
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
        // `T` is a plain table; `series` is a table and also a module, so
        // a read of it is refused.
        let catalog = Catalog {
            tables: vec!["T".into(), "series".into()],
            modules: vec!["series".into(), "generate_series".into()],
        };
        for (action, want) in table {
            assert_eq!(judge(&action, &catalog), want, "{action:?}");
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

    /// SQLite skips empty statements, so `;` and NUL before the keyword do
    /// not change what runs, and the label names the statement past them
    /// (bead `ley-line-open-a9d50a`, N1).
    #[test]
    fn the_leading_keyword_skips_empty_statements_and_nul() {
        assert_eq!(leading_keyword("; VACUUM INTO 'x'"), "vacuum");
        assert_eq!(leading_keyword(";;\n ; -- c\n;Vacuum"), "vacuum");
        assert_eq!(leading_keyword("\0VACUUM"), "vacuum");
        assert_eq!(leading_keyword("\0 ;/* c */\0 begin"), "begin");
        assert_eq!(leading_keyword(";"), "empty");
        let (_dir, _w, mut r) = reader();
        let got = refusal(&mut r, "; VACUUM");
        assert_eq!(
            (got.reason, got.action.as_str()),
            (RefusalReason::NotAQuery, "vacuum")
        );
    }

    /// A reader as the daemon's reader pool hands it out, with sqlite-vec
    /// registered first when the build has it, as `cmd_daemon` does.
    fn pool() -> (tempfile::TempDir, crate::daemon::db_pool::LiveDb) {
        #[cfg(feature = "vec")]
        crate::daemon::vec_index::register_vec();
        let dir = tempfile::TempDir::new().unwrap();
        let db = crate::daemon::db_pool::LiveDb::open_fresh_for_test(&dir.path().join("live.db"));
        (dir, db)
    }

    /// Every virtual-table module registered on `conn`, read on the raw
    /// connection (no authorizer).
    fn modules(conn: &Connection) -> Vec<String> {
        let mut stmt = conn.prepare("SELECT name FROM pragma_module_list").unwrap();
        let mut names: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|n| n.unwrap())
            .collect();
        names.sort();
        names
    }

    /// The modules SQLite registers only when a statement first names them
    /// (`sqlite3LocateTable`): the JSON ones, and `carray` when it is
    /// compiled in. `pragma_` modules register the same way and are judged
    /// by [`ALLOWED_PRAGMAS`].
    const LAZY_BUILTIN_MODULES: &[&str] = &[
        "carray",
        "json_each",
        "json_tree",
        "jsonb_each",
        "jsonb_tree",
    ];

    /// The modules of `conn`, the lazily registered built-ins included,
    /// that are neither allowed nor denied by name. The listing registers
    /// `pragma_module_list` itself; any other `pragma_` module on a fresh
    /// connection is an extension's.
    fn unclassified(conn: &Connection) -> Vec<String> {
        for name in LAZY_BUILTIN_MODULES {
            // Naming the module registers it; one this build lacks fails
            // to prepare.
            let _ = conn.prepare(&format!("SELECT 1 FROM {name}"));
        }
        modules(conn)
            .into_iter()
            .filter(|m| !listed(ALLOWED_MODULES, m) && !listed(DENIED_MODULES, m))
            .collect()
    }

    /// The modules sqlite-vec registers, present only in a `vec` build.
    const VEC_MODULES: &[&str] = &["vec0", "vec_each"];

    /// Like `every_builtin_function_is_classified`, for modules: a module a
    /// later SQLite, sqlite-vec or another auto-extension adds is reachable
    /// from `query` only once someone lists it (bead
    /// `ley-line-open-a9be1f`).
    #[test]
    fn every_module_is_classified() {
        let (_dir, db) = pool();
        let conn = db.reader_pool.get().unwrap();
        assert_eq!(
            unclassified(&conn),
            Vec::<String>::new(),
            "modules neither allowed nor denied"
        );
        let present = modules(&conn);
        for name in ALLOWED_MODULES.iter().chain(DENIED_MODULES) {
            assert!(
                !(listed(ALLOWED_MODULES, name) && listed(DENIED_MODULES, name)),
                "{name} is both allowed and denied"
            );
            if cfg!(feature = "vec") || !listed(VEC_MODULES, name) {
                assert!(
                    listed(
                        &present.iter().map(String::as_str).collect::<Vec<_>>(),
                        name
                    ),
                    "{name} is not a module of this build: {present:?}"
                );
            }
        }
    }

    /// The guard above bites: a module the build did not have is
    /// unclassified, and `query` refuses to read it.
    #[test]
    fn a_new_module_is_unclassified_and_refused() {
        let (_dir, db) = pool();
        let mut conn = db.reader_pool.get().unwrap();
        rusqlite::vtab::series::load_module(&conn).unwrap();
        assert_eq!(unclassified(&conn), vec!["generate_series".to_string()]);
        for sql in [
            "SELECT value FROM generate_series(1, 3)",
            "SELECT count(*) FROM generate_series(1, 3)",
            "SELECT 1 WHERE EXISTS (SELECT 1 FROM Generate_Series(1, 3))",
        ] {
            let got = refusal(&mut conn, sql);
            assert_eq!(
                (got.reason, got.action.as_str()),
                (RefusalReason::DeniedAction, "read"),
                "{sql}"
            );
            assert!(
                got.subject.eq_ignore_ascii_case("generate_series"),
                "{sql}: {got:?}"
            );
        }
    }

    /// The allowed modules and pragmas still return rows; every other
    /// `pragma_` table is refused as its pragma when the statement is
    /// prepared.
    #[test]
    fn allowed_modules_return_rows() {
        let (_dir, _w, mut r) = reader();
        for (sql, n) in [
            ("SELECT value FROM json_each('[1, 2, 3]')", 3),
            ("SELECT count(*) FROM Json_Each('[1, 2]')", 1),
            ("SELECT key FROM json_tree('{\"a\": 1}')", 2),
            ("SELECT name FROM dbstat", 2),
            ("SELECT name FROM pragma_table_info('t')", 1),
            ("SELECT name FROM sqlite_master", 1),
            ("SELECT name FROM sqlite_schema", 1),
        ] {
            assert_eq!(rows(&mut r, sql).unwrap().len(), n, "{sql}");
        }
        // Only `main` is read; a temp table cannot be created, and naming
        // the temp schema would open the temp database.
        let got = refusal(&mut r, "SELECT name FROM sqlite_temp_master");
        assert_eq!(
            (got.action.as_str(), got.subject.as_str()),
            ("read", "sqlite_temp_master")
        );
        for (sql, pragma) in [
            ("SELECT name FROM pragma_module_list", "module_list"),
            ("SELECT count(*) FROM pragma_database_list", "database_list"),
        ] {
            let got = refusal(&mut r, sql);
            assert_eq!(
                (got.reason, got.action.as_str(), got.subject.as_str()),
                (RefusalReason::DeniedAction, "pragma", pragma),
                "{sql}"
            );
        }
    }

    #[cfg(feature = "vec")]
    #[test]
    fn vec_each_returns_rows() {
        let (_dir, db) = pool();
        let mut conn = db.reader_pool.get().unwrap();
        assert_eq!(
            rows(&mut conn, "SELECT value FROM vec_each('[1.0, 2.0]')")
                .unwrap()
                .len(),
            2
        );
    }

    /// Plain schema objects are read as before: tables, views, `WITHOUT
    /// ROWID` tables, the shadow tables of a virtual table, and
    /// `sqlite_sequence`.
    #[test]
    fn schema_tables_and_views_are_read() {
        let (_dir, w, mut r) = reader();
        w.execute_batch(
            "CREATE VIEW v AS SELECT a FROM t;
             CREATE TABLE k (id INTEGER PRIMARY KEY, x) WITHOUT ROWID;
             CREATE TABLE s (id INTEGER PRIMARY KEY AUTOINCREMENT);
             INSERT INTO s DEFAULT VALUES;
             CREATE VIRTUAL TABLE docs USING fts5(body);",
        )
        .unwrap();
        for (sql, n) in [
            ("SELECT a FROM v", 2),
            ("SELECT count(*) FROM k", 1),
            ("SELECT name FROM sqlite_sequence", 1),
            ("SELECT id FROM docs_data", 2),
            ("SELECT 1 FROM t AS json_tree", 2),
        ] {
            assert_eq!(rows(&mut r, sql).unwrap().len(), n, "{sql}");
        }
    }

    /// A virtual table in the schema runs its module's code on every read,
    /// so it is refused like an unlisted module, whatever the reader's
    /// history (bead `ley-line-open-07237c`): the same refusal on a reader
    /// that never connected the table and on one that did.
    #[test]
    fn a_schema_virtual_table_is_refused_on_any_reader() {
        let (_dir, w, _r) = reader();
        w.execute_batch(
            "CREATE VIRTUAL TABLE docs USING fts5(body);
             INSERT INTO docs VALUES ('alpha');
             CREATE VIRTUAL TABLE boxes USING rtree(id, x0, x1);
             INSERT INTO boxes VALUES (1, 0, 1);",
        )
        .unwrap();
        let path = w.path().unwrap().to_string();
        let mut mismatches = Vec::new();
        // (statement, table, the refused action and subject on a fresh
        // reader, the same on a reader that connected the table). The
        // refusal names the first denied action. On a fresh reader the
        // R-tree module's constructor prepares its own `INSERT` into the
        // node table before SQLite reports the read of `boxes`, so that
        // insert is named; the decision is the same.
        for (sql, table, fresh_label, connected_label) in [
            (
                "SELECT body FROM docs",
                "docs",
                ("read", "docs"),
                ("read", "docs"),
            ),
            (
                "SELECT count(*) FROM docs WHERE docs MATCH 'alpha'",
                "docs",
                ("function", "match"),
                ("function", "match"),
            ),
            (
                "SELECT id FROM boxes",
                "boxes",
                ("insert", "boxes_node"),
                ("read", "boxes"),
            ),
            (
                "SELECT count(*) FROM boxes WHERE x0 >= 0",
                "boxes",
                ("insert", "boxes_node"),
                ("read", "boxes"),
            ),
        ] {
            let mut outcomes = Vec::new();
            for connected in [false, true] {
                let mut fresh = Connection::open(&path).unwrap();
                fresh.pragma_update(None, "query_only", "ON").unwrap();
                if connected {
                    // Connect the table without the authorizer, as another
                    // op on the same pooled reader would.
                    let n: i64 = fresh
                        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                            row.get(0)
                        })
                        .unwrap();
                    assert_eq!(n, 1);
                }
                let got = refusal(&mut fresh, sql);
                assert_eq!(got.reason, RefusalReason::DeniedAction, "{sql}");
                outcomes.push(got);
            }
            let label = |r: &Refusal| (r.action.clone(), r.subject.clone());
            let got = (label(&outcomes[0]), label(&outcomes[1]));
            let want = (
                (fresh_label.0.to_string(), fresh_label.1.to_string()),
                (connected_label.0.to_string(), connected_label.1.to_string()),
            );
            if got != want {
                mismatches.push(format!("{sql}: fresh, connected: {got:?}"));
            }
        }
        assert!(mismatches.is_empty(), "{mismatches:#?}");
    }

    /// A schema table whose name is also an unlisted module's is refused:
    /// which of the two a name resolves to can change between the
    /// classification and the prepare (a writer drops the table), so the
    /// collision fails closed.
    #[test]
    fn a_table_named_like_an_unlisted_module_is_refused() {
        let (_dir, w, mut r) = reader();
        w.execute_batch("CREATE TABLE fts5vocab (x); INSERT INTO fts5vocab VALUES (1);")
            .unwrap();
        let got = refusal(&mut r, "SELECT x FROM fts5vocab");
        assert_eq!(
            (got.action.as_str(), got.subject.as_str()),
            ("read", "fts5vocab")
        );
    }

    /// A function that prepares a statement on its own connection while
    /// the outer statement steps: what an action denied during a step looks
    /// like (a table-valued pragma's inner `PRAGMA` was one before reads of
    /// unlisted `pragma_` tables were refused at prepare). It overrides
    /// `abs`, an allowed name, and denies only on its second row.
    fn deny_on_two(conn: &Connection) {
        use rusqlite::functions::FunctionFlags;
        conn.create_scalar_function("abs", 1, FunctionFlags::SQLITE_UTF8, |ctx| {
            let n: i64 = ctx.get(0)?;
            if n == 2 {
                // SAFETY: the connection outlives the call it is running.
                let inner = unsafe { ctx.get_connection()? };
                inner.prepare("SELECT name FROM pragma_database_list")?;
            }
            Ok(n)
        })
        .unwrap();
    }

    /// Row 1 is produced before row 2's step is denied, and the caller
    /// still gets the refusal and no rows (bead `ley-line-open-a9d50a`,
    /// N3).
    #[test]
    fn a_denial_after_rows_returns_no_rows() {
        let (_dir, _w, mut r) = reader();
        deny_on_two(&r);
        let mut produced = 0;
        let err = query(&mut r, "SELECT abs(a) FROM t ORDER BY rowid", |stmt| {
            let mut rows = stmt.query([])?;
            while rows.next()?.is_some() {
                produced += 1;
            }
            Ok(produced)
        })
        .expect_err("a denial on row 2 refuses the statement");
        assert_eq!(produced, 1, "row 1 was stepped before the denial");
        let got = err.downcast_ref::<Refusal>().expect("a refusal");
        assert_eq!(
            (got.action.as_str(), got.subject.as_str()),
            ("pragma", "database_list")
        );
    }
}
