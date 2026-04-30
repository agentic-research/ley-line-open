//! Base op handlers for the daemon's UDS protocol.
//!
//! Each op queries the living in-memory SQLite database directly.
//! The arena is used only for periodic snapshots (crash recovery + mache).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use leyline_core::Controller;
use rusqlite::Connection;
use serde_json::json;

use super::events::json_string_array_opt;
use super::{DaemonContext, DaemonPhase};

// ---------------------------------------------------------------------------
// Public dispatch
// ---------------------------------------------------------------------------

/// Names of ops that mutate the daemon's state and should emit a
/// `daemon.<op>` event after completion. Lives next to the dispatch
/// table so a new mutating op gets a one-stop checklist (add a match
/// arm in `handle_base_op` AND list the name here).
///
/// Single source of truth — `daemon::socket` reads this via
/// `is_state_changing()` rather than maintaining a parallel list.
pub(crate) const STATE_CHANGING_OPS: &[&str] = &[
    "load",
    "reparse",
    "flush",
    "snapshot",
    "enrich",
];

/// Whether an op name belongs to `STATE_CHANGING_OPS`. Used by the UDS
/// dispatch loop to decide if the op deserves a follow-up event.
pub(crate) fn is_state_changing(op: &str) -> bool {
    STATE_CHANGING_OPS.contains(&op)
}

/// Canonical list of op names that `handle_base_op` dispatches.
///
/// Single source of truth shared with `daemon::mcp::tool_registry` — every
/// MCP tool name must be in this list. A drift test in `mcp::tests`
/// catches mismatches; another in this module's tests verifies that
/// `handle_base_op` recognizes every name here.
///
/// If you add a new op:
///   1. Add a match arm in `handle_base_op`.
///   2. Add the name here (and in `STATE_CHANGING_OPS` if mutating).
///   3. Optionally expose it via `tool_registry()` in `daemon::mcp`.
#[cfg(test)]
pub(crate) fn base_op_names() -> Vec<&'static str> {
    #[cfg_attr(not(feature = "vec"), allow(unused_mut))]
    let mut v = vec![
        "status",
        "flush",
        "load",
        "query",
        "reparse",
        "snapshot",
        "enrich",
        "list_roots",
        "list_children",
        "read_content",
        "find_callers",
        "find_defs",
        "get_node",
        "lsp_hover",
        "lsp_defs",
        "lsp_refs",
        "lsp_symbols",
        "lsp_diagnostics",
    ];
    #[cfg(feature = "vec")]
    v.push("vec_search");
    v
}

/// Dispatch a base op. Returns `Some(json_string)` if handled, `None` if unrecognized.
pub fn handle_base_op(ctx: &DaemonContext, op: &str, req: &serde_json::Value) -> Option<String> {
    let result = match op {
        "status" => Some(op_status(ctx)),
        "flush" => Some(op_flush(&ctx.ctrl_path)),
        "load" => Some(op_load(&ctx.ctrl_path, req)),
        "query" => Some(op_query(ctx, req)),
        "reparse" => Some(op_reparse(ctx, req)),
        "snapshot" => Some(op_snapshot(ctx)),
        "enrich" => Some(op_enrich(ctx, req)),
        // Structured query ops — direct from living db.
        "list_roots" => Some(op_list_children(ctx, &json!({"id": ""}))),
        "list_children" => Some(op_list_children(ctx, req)),
        "read_content" => Some(op_read_content(ctx, req)),
        "find_callers" => Some(op_find_callers(ctx, req)),
        "find_defs" => Some(op_find_defs(ctx, req)),
        "get_node" => Some(op_get_node(ctx, req)),
        // Position-based LSP queries — translate (file, line, col) to node lookups.
        "lsp_hover" => Some(op_lsp_hover(ctx, req)),
        "lsp_defs" => Some(op_lsp_defs(ctx, req)),
        "lsp_refs" => Some(op_lsp_refs(ctx, req)),
        "lsp_symbols" => Some(op_lsp_symbols(ctx, req)),
        "lsp_diagnostics" => Some(op_lsp_diagnostics(ctx, req)),
        #[cfg(feature = "vec")]
        "vec_search" => Some(op_vec_search(ctx, req)),
        _ => None,
    };
    result.map(|r| match r {
        Ok(v) => v,
        Err(e) => json!({"ok": false, "error": format!("{e:#}")}).to_string(),
    })
}

// ---------------------------------------------------------------------------
// Living db access
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Small shared helpers — keep these one-liner-trivial so callers stay readable.
// ---------------------------------------------------------------------------

/// SQL fragment for "the row's `node_id` belongs to file ?1". Use as the WHERE
/// clause of any per-file `_lsp` query. Bind the file path as the first param.
///
/// Convention: node ids look like `"<file>/<ast-path>"`, so the LIKE prefix
/// scopes a query to all nodes in a single file.
const NODE_ID_FOR_FILE: &str = "node_id LIKE ?1 || '%'";

/// Query a `(token) → (node_id, source_id)` table, used by find_callers
/// (node_refs) and find_defs (node_defs). The two ops differ only in
/// table name + the JSON output key, so this helper handles the SQL +
/// row decoding once.
fn query_token_in_table(
    conn: &Connection,
    token: &str,
    table: &str,
) -> Result<Vec<serde_json::Value>> {
    let sql = format!("SELECT node_id, source_id FROM {table} WHERE token = ?1");
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt
        .query_map([token], |row| {
            Ok(json!({
                "node_id":   row.get::<_, String>(0)?,
                "source_id": row.get::<_, String>(1)?,
            }))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Helper for `_lsp_defs` / `_lsp_refs` queries. Both share the
/// `(uri, start_line, start_col, end_line, end_col)` shape with a
/// table-specific column prefix (`def_` / `ref_`). Returns an empty
/// vec if the table doesn't exist yet — that's the "not enriched"
/// signal callers use to trigger lazy enrichment.
/// Whether a table exists in the connection. Used by every LSP-rows
/// helper so a query against a not-yet-enriched table returns an
/// empty Vec (the "needs enrichment" signal callers act on) instead
/// of bubbling up a `no such table` SQL error.
fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |r| r.get(0),
    )
    .unwrap_or(false)
}

fn lsp_5col_position_rows(
    conn: &Connection,
    node_id: &str,
    table: &str,
    col_prefix: &str,
) -> Result<Vec<serde_json::Value>> {
    if !table_exists(conn, table) {
        return Ok(vec![]);
    }
    let sql = format!(
        "SELECT {col_prefix}_uri, {col_prefix}_start_line, {col_prefix}_start_col, \
                {col_prefix}_end_line, {col_prefix}_end_col \
         FROM {table} WHERE node_id = ?1"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt
        .query_map([node_id], |row| {
            Ok(json!({
                "uri":        row.get::<_, String>(0)?,
                "start_line": row.get::<_, i32>(1)?,
                "start_col":  row.get::<_, i32>(2)?,
                "end_line":   row.get::<_, i32>(3)?,
                "end_col":    row.get::<_, i32>(4)?,
            }))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Strip a leading `file://` from an LSP-style URI. Returns the input
/// unchanged if no prefix is present. Centralized so the rule for what
/// counts as "the file path" stays in one spot.
#[inline]
fn normalize_file_uri(s: &str) -> &str {
    s.strip_prefix("file://").unwrap_or(s)
}

/// Promote one or more node ids to the embed queue (no-op without `vec`).
///
/// Called from query ops so the touched nodes' embeddings get refreshed soon
/// by the background drainer.
#[inline]
#[allow(unused_variables)]
fn promote_touched(ctx: &DaemonContext, ids: &[&str]) {
    #[cfg(feature = "vec")]
    {
        for id in ids {
            crate::daemon::embed::promote(&ctx.embed_queue, id);
        }
    }
}

/// Execute a closure with the living database connection.
fn with_live_db<F, T>(ctx: &DaemonContext, f: F) -> Result<T>
where
    F: FnOnce(&Connection) -> Result<T>,
{
    let guard = ctx.live_db.lock().unwrap();
    f(&guard)
}

/// Open the controller and return its current generation. Centralizes the
/// "open ctrl + read generation" pair that every op which mutates state
/// needs to include in its JSON response. The `"open controller"` context
/// string is part of the wire-error contract — clients see this when the
/// controller path is broken.
fn read_generation(ctrl_path: &Path) -> Result<u64> {
    Controller::open_or_create(ctrl_path)
        .context("open controller")
        .map(|c| c.generation())
}

/// Acquire the living-db lock and snapshot to the arena. Used by every
/// state-changing op (reparse, enrich, snapshot) to publish the latest
/// db image to mache/remote consumers.
///
/// **Lock window:** the write lock is held for the *full* duration of
/// `snapshot_to_arena`, which serializes the entire SQLite database to
/// the on-disk arena (a disk write proportional to db size). This is
/// not a cheap window. Concurrent readers and writers block until the
/// snapshot completes. Don't add work inside this function expecting
/// the lock to be held briefly — it isn't. If we ever want concurrent
/// reads during snapshot, the path is `serialize_with_flags(NO_COPY)`
/// followed by an out-of-lock disk write, but that's a deliberate
/// refactor, not something to assume here. Doc rewritten after iter-35
/// adversarial review caught the previous false minimal-lock claim.
fn snapshot_living_db(ctx: &DaemonContext) -> Result<()> {
    crate::cmd_daemon::snapshot_to_arena(
        &ctx.live_db.lock().unwrap(),
        &ctx.ctrl_path,
    )
}

// ---------------------------------------------------------------------------
// Control ops (don't need the living db)
// ---------------------------------------------------------------------------

fn op_status(ctx: &DaemonContext) -> Result<String> {
    let ctrl = Controller::open_or_create(&ctx.ctrl_path).context("open controller")?;
    let state = ctx.state.read().unwrap();

    let mut enrichment = serde_json::Map::new();
    for (name, status) in &state.enrichment {
        let mut s = serde_json::Map::new();
        if let Some(t) = status.last_run_at_ms {
            s.insert("last_run_at_ms".into(), json!(t));
        }
        if let Some(b) = status.basis {
            s.insert("basis".into(), json!(b));
        }
        if let Some(e) = &status.error {
            s.insert("error".into(), json!(e));
        }
        enrichment.insert(name.clone(), serde_json::Value::Object(s));
    }

    let mut out = json!({
        "ok": true,
        "phase": state.phase.as_str(),
        "generation": ctrl.generation(),
        "arena_path": ctrl.arena_path(),
        "arena_size": ctrl.arena_size(),
        "enrichment": enrichment,
    });
    if let serde_json::Value::Object(ref mut map) = out {
        if let Some(sha) = &state.head_sha {
            map.insert("head_sha".into(), json!(sha));
        }
        if let Some(ts) = state.last_reparse_at_ms {
            map.insert("last_reparse_at_ms".into(), json!(ts));
        }
        if let DaemonPhase::Error(msg) = &state.phase {
            map.insert("error".into(), json!(msg));
        }
    }
    Ok(out.to_string())
}

fn op_flush(ctrl_path: &Path) -> Result<String> {
    Ok(json!({
        "ok": true,
        "generation": read_generation(ctrl_path)?,
    })
    .to_string())
}

fn op_load(ctrl_path: &Path, req: &serde_json::Value) -> Result<String> {
    use base64::Engine;
    let b64 = req
        .get("db")
        .and_then(|v| v.as_str())
        .context("missing \"db\" field (base64-encoded .db)")?;
    let db_bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("invalid base64 in \"db\" field")?;
    crate::cmd_load::load_into_arena(ctrl_path, &db_bytes)?;
    Ok(json!({"ok": true, "generation": read_generation(ctrl_path)?}).to_string())
}

// ---------------------------------------------------------------------------
// Reparse + snapshot ops
// ---------------------------------------------------------------------------

fn op_reparse(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    // Inputs:
    //   `source` — directory or single file. If omitted, falls back to ctx.source_dir.
    //   `files`  — optional explicit scope (relative paths under source). When set,
    //              only those files are parsed; unscoped files are untouched.
    //   `lang`   — optional language filter.
    //
    // For Claude Code's PostToolUse hook the natural shape is
    // `{source: "<file>"}`. We accept that and auto-rewrite to
    // `(parent, scope=[basename])` so existing hook callers don't need to
    // know about the directory invariant.
    let source_arg = req
        .get("source")
        .and_then(|v| v.as_str())
        .or_else(|| ctx.source_dir.as_ref().map(|p| p.to_str().unwrap_or("")))
        .context("missing \"source\" field and no --source configured")?
        .to_string();
    let lang = req
        .get("lang")
        .and_then(|v| v.as_str())
        .or(ctx.lang_filter.as_deref());

    // Explicit `files: [...]` always takes precedence as the scope.
    let mut explicit_files: Option<Vec<String>> = json_string_array_opt(req, "files");

    // If the caller passed a single-file `source`, reinterpret as parent +
    // scope so we satisfy parse_into_conn's directory invariant. This lets
    // hooks blindly forward `tool_input.file_path` without knowing the
    // project root.
    let source_path = Path::new(&source_arg);
    let (source_dir, derived_scope): (PathBuf, Option<Vec<String>>) =
        if source_path.is_dir() {
            (source_path.to_path_buf(), None)
        } else if source_path.is_file() {
            // Fall back to ctx.source_dir as the project root if available
            // (lets the relative path stay short); otherwise use the file's
            // own parent directory.
            let project_root = ctx
                .source_dir
                .as_ref()
                .filter(|root| source_path.starts_with(root))
                .cloned();
            match project_root {
                Some(root) => {
                    let rel = source_path
                        .strip_prefix(&root)
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|_| {
                            source_path
                                .file_name()
                                .map(|f| f.to_string_lossy().to_string())
                                .unwrap_or_default()
                        });
                    (root, Some(vec![rel]))
                }
                None => {
                    let parent = source_path
                        .parent()
                        .map(|p| p.to_path_buf())
                        .unwrap_or_else(|| PathBuf::from("."));
                    let basename = source_path
                        .file_name()
                        .map(|f| f.to_string_lossy().to_string())
                        .unwrap_or_default();
                    (parent, Some(vec![basename]))
                }
            }
        } else {
            // Path doesn't exist — bubble up the error from parse_into_conn
            // so the caller sees a helpful message.
            (source_path.to_path_buf(), None)
        };

    if explicit_files.is_none() {
        explicit_files = derived_scope;
    }
    let scope: Option<&[String]> = explicit_files.as_deref();

    // Parse directly into the living db.
    ctx.state.write().unwrap().phase = DaemonPhase::Parsing;
    let guard = ctx.live_db.lock().unwrap();
    let result = match crate::cmd_parse::parse_into_conn(&guard, &source_dir, lang, scope) {
        Ok(r) => r,
        Err(e) => {
            drop(guard);
            ctx.state.write().unwrap().phase =
                DaemonPhase::Error(format!("reparse failed: {e:#}"));
            return Err(e);
        }
    };
    drop(guard);

    // Snapshot to arena for mache/remote consumers.
    snapshot_living_db(ctx)?;

    {
        let mut s = ctx.state.write().unwrap();
        s.phase = DaemonPhase::Ready;
        s.last_reparse_at_ms = Some(super::now_ms());
    }

    Ok(json!({
        "ok": true,
        "generation": read_generation(&ctx.ctrl_path)?,
        "parsed": result.parsed,
        "unchanged": result.unchanged,
        "deleted": result.deleted,
        "errors": result.errors,
        "changed_files": result.changed_files,
    })
    .to_string())
}


fn op_enrich(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    let pass_name = required_str_field(req, "pass")?;
    let files: Option<Vec<String>> = json_string_array_opt(req, "files");

    let source_dir = ctx
        .source_dir
        .as_deref()
        .context("no --source configured; cannot run enrichment")?;

    ctx.state.write().unwrap().phase = DaemonPhase::Enriching;
    let guard = ctx.live_db.lock().unwrap();
    let stats = crate::daemon::enrichment::run_pass(
        &ctx.enrichment_passes,
        pass_name,
        &guard,
        source_dir,
        files.as_deref(),
        Some(&ctx.state),
    )?;
    drop(guard);
    ctx.state.write().unwrap().phase = DaemonPhase::Ready;

    // Snapshot to arena after enrichment.
    snapshot_living_db(ctx)?;

    Ok(json!({
        "ok": true,
        "generation": read_generation(&ctx.ctrl_path)?,
        "passes": stats,
    })
    .to_string())
}

fn op_snapshot(ctx: &DaemonContext) -> Result<String> {
    snapshot_living_db(ctx)?;
    Ok(json!({"ok": true, "generation": read_generation(&ctx.ctrl_path)?}).to_string())
}

// ---------------------------------------------------------------------------
// Query ops (use living db directly)
// ---------------------------------------------------------------------------

/// Raw SQL query — for ad-hoc inspection.
fn op_query(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    let sql = required_str_field(req, "sql")?;

    with_live_db(ctx, |conn| {
        let mut stmt = conn.prepare(sql).context("prepare SQL")?;
        let col_count = stmt.column_count();
        let headers: Vec<String> = (0..col_count)
            .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
            .collect();

        let mut rows_out: Vec<serde_json::Value> = Vec::new();
        let mut rows = stmt.query([]).context("execute SQL")?;
        while let Some(row) = rows.next()? {
            let mut obj = serde_json::Map::new();
            for (i, col) in headers.iter().enumerate() {
                let val: String = row.get::<_, String>(i).unwrap_or_default();
                obj.insert(col.clone(), serde_json::Value::String(val));
            }
            rows_out.push(serde_json::Value::Object(obj));
        }

        Ok(json!({"ok": true, "columns": headers, "rows": rows_out}).to_string())
    })
}

/// List children of a node (or roots if id="").
fn op_list_children(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    let id = req.get("id").and_then(|v| v.as_str()).unwrap_or("");

    let response = with_live_db(ctx, |conn| {
        let mut stmt = conn.prepare_cached(
            "SELECT id, name, kind, size FROM nodes WHERE parent_id = ?1 ORDER BY name",
        )?;
        let raw: Vec<(String, String, i32, i64)> = stmt
            .query_map([id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i32>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?
            .collect::<Result<_, _>>()?;
        let rows: Vec<serde_json::Value> = raw
            .iter()
            .map(|(id, name, kind, size)| {
                json!({"id": id, "name": name, "kind": kind, "size": size})
            })
            .collect();
        let touched: Vec<&str> = raw.iter().map(|(id, ..)| id.as_str()).collect();
        Ok((json!({"ok": true, "children": rows}).to_string(), touched.into_iter().map(String::from).collect::<Vec<_>>()))
    })?;
    let touched_refs: Vec<&str> = response.1.iter().map(String::as_str).collect();
    promote_touched(ctx, &touched_refs);
    Ok(response.0)
}

/// Read a node's content (the `record` column).
fn op_read_content(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    let id = required_str_field(req, "id")?;
    promote_touched(ctx, &[id]);

    with_live_db(ctx, |conn| {
        let content = query_row_opt(
            conn,
            "SELECT record FROM nodes WHERE id = ?1",
            [id],
            |row| row.get::<_, String>(0),
        )?;
        match content {
            Some(c) => Ok(json!({"ok": true, "content": c}).to_string()),
            None => Ok(node_not_found_response(id)),
        }
    })
}

/// Find callers of a token (queries node_refs).
fn op_find_callers(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    op_find_token(ctx, req, "node_refs", "callers")
}

/// Find definitions of a token (queries node_defs).
fn op_find_defs(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    op_find_token(ctx, req, "node_defs", "defs")
}

/// Shared body for `find_callers` and `find_defs`. Both ops differ only
/// in which `(token → node_id)` table they consult and the JSON key
/// under which results are returned.
fn op_find_token(
    ctx: &DaemonContext,
    req: &serde_json::Value,
    table: &str,
    json_key: &str,
) -> Result<String> {
    let token = required_str_field(req, "token")?;
    with_live_db(ctx, |conn| {
        let rows = query_token_in_table(conn, token, table)?;
        let mut obj = serde_json::Map::new();
        obj.insert("ok".to_string(), json!(true));
        obj.insert(json_key.to_string(), json!(rows));
        Ok(serde_json::Value::Object(obj).to_string())
    })
}

/// Get a single node by ID.
fn op_get_node(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    let id = required_str_field(req, "id")?;
    promote_touched(ctx, &[id]);

    with_live_db(ctx, |conn| {
        let node = query_row_opt(
            conn,
            "SELECT id, parent_id, name, kind, size, record FROM nodes WHERE id = ?1",
            [id],
            |row| {
                Ok(json!({
                    "id": row.get::<_, String>(0)?,
                    "parent_id": row.get::<_, String>(1)?,
                    "name": row.get::<_, String>(2)?,
                    "kind": row.get::<_, i32>(3)?,
                    "size": row.get::<_, i64>(4)?,
                    "record": row.get::<_, String>(5)?,
                }))
            },
        )?;
        match node {
            Some(n) => Ok(json!({"ok": true, "node": n}).to_string()),
            None => Ok(node_not_found_response(id)),
        }
    })
}

// ---------------------------------------------------------------------------
// Position-based LSP query ops
// ---------------------------------------------------------------------------

/// Find the node_id at a given (file, line, col) position via the _ast table.
fn find_node_at_position(conn: &Connection, file: &str, line: u32, col: u32) -> Result<Option<String>> {
    // Find the most specific (smallest range) AST node containing this position.
    query_row_opt(
        conn,
        "SELECT node_id FROM _ast \
         WHERE source_id = ?1 \
           AND start_row <= ?2 AND end_row >= ?2 \
           AND (start_row < ?2 OR start_col <= ?3) \
           AND (end_row > ?2 OR end_col >= ?3) \
         ORDER BY (end_byte - start_byte) ASC \
         LIMIT 1",
        rusqlite::params![file, line, col],
        |row| row.get::<_, String>(0),
    )
}

/// Extract the `file` field from a request, normalizing any leading
/// `file://` prefix. Returns the borrowed slice on success so callers
/// can decide whether to copy or pass through. Single source of truth
/// for the "missing \"file\" field" error message — every op that takes
/// a file argument routes through here.
fn parse_file_arg(req: &serde_json::Value) -> Result<&str> {
    let file = required_str_field(req, "file")?;
    Ok(normalize_file_uri(file))
}

/// Required-string-field extractor with a uniform error message shape.
///
/// Centralizes the "missing \"<field>\" field" wording — clients see this
/// directly when they make a malformed request, so it's part of the wire
/// contract. Drift would silently change error strings under callers.
fn required_str_field<'a>(req: &'a serde_json::Value, field: &'static str) -> Result<&'a str> {
    req.get(field)
        .and_then(|v| v.as_str())
        .with_context(|| format!("missing \"{field}\" field"))
}

/// Run a `query_row`, mapping `QueryReturnedNoRows` to `Ok(None)`. Other
/// errors propagate. Replaces the four-arm match (`Ok→Some / NoRows→None /
/// Err→Err`) that several "id-or-position lookup" ops were carrying inline.
fn query_row_opt<T, P, F>(
    conn: &Connection,
    sql: &str,
    params: P,
    mapper: F,
) -> Result<Option<T>>
where
    P: rusqlite::Params,
    F: FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
{
    match conn.query_row(sql, params, mapper) {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Wire-contract error response when a node id doesn't resolve. Used by
/// `op_read_content` and `op_get_node` — both must return the same shape
/// and message so clients can detect "no such node" without brittle string
/// matching.
fn node_not_found_response(id: &str) -> String {
    json!({"ok": false, "error": format!("node '{id}' not found")}).to_string()
}

/// Parse file + line + col from request. File can be a path or file:// URI.
fn parse_position(req: &serde_json::Value) -> Result<(String, u32, u32)> {
    let file = parse_file_arg(req)?.to_string();

    let line = req
        .get("line")
        .and_then(|v| v.as_u64())
        .context("missing \"line\" field")? as u32;
    let col = req
        .get("col")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;

    Ok((file, line, col))
}

/// Check if a file has ANY _lsp enrichment data. If not, auto-trigger
/// enrichment and return true (caller should retry the query).
fn maybe_enrich(ctx: &DaemonContext, conn: &Connection, file: &str) -> bool {
    // Check if _lsp table exists at all.
    let table_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='_lsp'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(false);

    if !table_exists {
        return try_enrich_file(ctx, file);
    }

    // Check if this file has any _lsp rows.
    let has_data: bool = conn
        .query_row(
            &format!("SELECT COUNT(*) > 0 FROM _lsp WHERE {NODE_ID_FOR_FILE}"),
            [file],
            |r| r.get(0),
        )
        .unwrap_or(false);

    if has_data {
        return false; // already enriched, don't retry
    }

    try_enrich_file(ctx, file)
}

/// Trigger LSP enrichment for a single file. Returns true if enrichment ran.
fn try_enrich_file(ctx: &DaemonContext, file: &str) -> bool {
    let source_dir = match &ctx.source_dir {
        Some(d) => d.clone(),
        None => return false,
    };

    eprintln!("lazy enrich: triggering LSP for {file}");

    let guard = ctx.live_db.lock().unwrap();
    let result = crate::daemon::enrichment::run_pass(
        &ctx.enrichment_passes,
        "lsp",
        &guard,
        &source_dir,
        Some(&[file.to_string()]),
        Some(&ctx.state),
    );
    drop(guard);

    match result {
        Ok(stats) => {
            if let Some(s) = stats.last() {
                eprintln!("lazy enrich: {} items for {file}", s.items_added);
            }
            true
        }
        Err(e) => {
            eprintln!("lazy enrich failed for {file}: {e:#}");
            false
        }
    }
}

/// Hover info at a position. Auto-enriches if no data exists.
fn op_lsp_hover(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    let (file, line, col) = parse_position(req)?;

    // First attempt.
    let result = with_live_db(ctx, |conn| {
        lsp_hover_query(conn, &file, line, col)
    })?;

    // If no data and file not enriched yet, trigger enrichment and retry.
    if result.is_none() {
        let enriched = with_live_db(ctx, |conn| Ok(maybe_enrich(ctx, conn, &file)))?;
        if enriched {
            let result = with_live_db(ctx, |conn| lsp_hover_query(conn, &file, line, col))?;
            if let Some((hover, node_id)) = result {
                return Ok(json!({"ok": true, "hover": hover, "node_id": node_id, "enriched": true}).to_string());
            }
        }
    }

    match result {
        Some((hover, node_id)) => Ok(json!({"ok": true, "hover": hover, "node_id": node_id}).to_string()),
        None => Ok(json!({"ok": true, "hover": null}).to_string()),
    }
}

fn lsp_hover_query(conn: &Connection, file: &str, line: u32, col: u32) -> Result<Option<(String, String)>> {
    let node_id = match find_node_at_position(conn, file, line, col)? {
        Some(id) => id,
        None => return Ok(None),
    };
    let hover = query_row_opt(
        conn,
        "SELECT hover_text FROM _lsp_hover WHERE node_id = ?1",
        [&node_id],
        |row| row.get::<_, String>(0),
    )?;
    Ok(hover.map(|text| (text, node_id)))
}

/// Go-to-definition at a position. Auto-enriches if no data exists.
fn op_lsp_defs(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    op_lsp_position(ctx, req, "_lsp_defs", "def", "definitions")
}

/// Find references at a position. Auto-enriches if no data exists.
fn op_lsp_refs(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    op_lsp_position(ctx, req, "_lsp_refs", "ref", "references")
}

/// Shared body for `lsp_defs` and `lsp_refs`. They differ only in the
/// `_lsp_*` table queried, the column prefix in that table, and the JSON
/// key under which results are returned. Both follow the same shape:
/// resolve the node at (file, line, col), pull rows from the 5-column
/// position table, retry once after lazy enrichment if the first attempt
/// is empty.
fn op_lsp_position(
    ctx: &DaemonContext,
    req: &serde_json::Value,
    table: &str,
    col_prefix: &str,
    json_key: &str,
) -> Result<String> {
    let (file, line, col) = parse_position(req)?;

    let do_query = |conn: &Connection| -> Result<Vec<serde_json::Value>> {
        match find_node_at_position(conn, &file, line, col)? {
            Some(id) => lsp_5col_position_rows(conn, &id, table, col_prefix),
            None => Ok(vec![]),
        }
    };

    let result = with_live_db(ctx, |conn| do_query(conn))?;
    if result.is_empty() {
        let enriched = with_live_db(ctx, |conn| Ok(maybe_enrich(ctx, conn, &file)))?;
        if enriched {
            let result = with_live_db(ctx, |conn| do_query(conn))?;
            return Ok(lsp_rows_response(json_key, result, true));
        }
    }
    Ok(lsp_rows_response(json_key, result, false))
}

/// Build the JSON response for an LSP query. Inserts the
/// `enriched: true` marker only when a lazy refresh just ran — the
/// shape clients see on a fresh-cache hit must be identical to the
/// shape on a warm hit (same fields, just no `enriched` key).
///
/// Used by both position queries (defs/refs/hover) where `enriched`
/// reflects whether a retry happened, and by file-level queries
/// (symbols/diagnostics) which always pass `enriched=false`.
fn lsp_rows_response(json_key: &str, rows: Vec<serde_json::Value>, enriched: bool) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("ok".to_string(), json!(true));
    obj.insert(json_key.to_string(), json!(rows));
    if enriched {
        obj.insert("enriched".to_string(), json!(true));
    }
    serde_json::Value::Object(obj).to_string()
}

/// Run a single-file LSP rows query: `prepare_cached` the supplied SQL,
/// bind `file` as `?1`, decode each row via `mapper`, collect into a
/// `Vec<Value>`. Used by `op_lsp_symbols` and `op_lsp_diagnostics`,
/// which differ only in the SELECTed columns and how each row is
/// decoded — both share this pipeline.
///
/// Returns `Ok(vec![])` when `table` doesn't exist yet (the pre-enrichment
/// state). This matches `lsp_5col_position_rows`'s contract — without
/// the guard, queries against a not-yet-enriched `_lsp` raise a SQL
/// error which clients have to special-case. `op_lsp_symbols` and
/// `op_lsp_defs`/`refs` were behaviorally divergent in this respect
/// before the guard was added (caught by adversarial review).
fn query_lsp_rows_for_file<F>(
    conn: &Connection,
    file: &str,
    table: &str,
    sql: &str,
    mapper: F,
) -> Result<Vec<serde_json::Value>>
where
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<serde_json::Value>,
{
    if !table_exists(conn, table) {
        return Ok(vec![]);
    }
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt
        .query_map([file], mapper)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Document symbols for a file.
fn op_lsp_symbols(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    let file = parse_file_arg(req)?;
    let sql = format!(
        "SELECT node_id, symbol_kind, detail, start_line, start_col, end_line, end_col \
         FROM _lsp WHERE {NODE_ID_FOR_FILE}"
    );
    with_live_db(ctx, |conn| {
        let rows = query_lsp_rows_for_file(conn, file, "_lsp", &sql, |row| {
            Ok(json!({
                "node_id": row.get::<_, String>(0)?,
                "kind": row.get::<_, String>(1)?,
                "detail": row.get::<_, String>(2)?,
                "start_line": row.get::<_, i32>(3)?,
                "start_col": row.get::<_, i32>(4)?,
                "end_line": row.get::<_, i32>(5)?,
                "end_col": row.get::<_, i32>(6)?,
            }))
        })?;
        Ok(lsp_rows_response("symbols", rows, false))
    })
}

/// Diagnostics for a file.
fn op_lsp_diagnostics(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    let file = parse_file_arg(req)?;
    let sql = format!(
        "SELECT node_id, diagnostics, start_line, start_col, end_line, end_col \
         FROM _lsp WHERE {NODE_ID_FOR_FILE} \
         AND diagnostics IS NOT NULL AND diagnostics != ''"
    );
    with_live_db(ctx, |conn| {
        let rows = query_lsp_rows_for_file(conn, file, "_lsp", &sql, |row| {
            Ok(json!({
                "node_id": row.get::<_, String>(0)?,
                "diagnostics": row.get::<_, String>(1)?,
                "start_line": row.get::<_, i32>(2)?,
                "start_col": row.get::<_, i32>(3)?,
                "end_line": row.get::<_, i32>(4)?,
                "end_col": row.get::<_, i32>(5)?,
            }))
        })?;
        Ok(lsp_rows_response("diagnostics", rows, false))
    })
}

// ---------------------------------------------------------------------------
// vec_search — KNN over the sidecar VectorIndex
// ---------------------------------------------------------------------------

/// `{"op":"vec_search", "query":"text", "k":10}` — embed the query via the
/// active embedder and KNN-search the sidecar VectorIndex. Returns
/// `{ok, results: [{node_id, distance}]}`.
#[cfg(feature = "vec")]
fn op_vec_search(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    let query = required_str_field(req, "query")?;
    let k = req.get("k").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

    let qvec = ctx.embedder.embed(query).context("embed query")?;
    let results = ctx.vec_index.search(&qvec, k).context("vec search")?;
    let rows: Vec<serde_json::Value> = results
        .into_iter()
        .map(|(id, d)| json!({"node_id": id, "distance": d}))
        .collect();
    Ok(json!({"ok": true, "results": rows}).to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex, RwLock};
    use tempfile::TempDir;

    // ── Helper unit tests ───────────────────────────────────────────────

    #[test]
    fn normalize_file_uri_strips_prefix() {
        assert_eq!(normalize_file_uri("file:///abs/foo.rs"), "/abs/foo.rs");
    }

    #[test]
    fn normalize_file_uri_passes_through_plain_path() {
        // Already-relative paths and bare paths come through untouched.
        assert_eq!(normalize_file_uri("src/foo.rs"), "src/foo.rs");
        assert_eq!(normalize_file_uri("/abs/foo.rs"), "/abs/foo.rs");
        assert_eq!(normalize_file_uri(""), "");
    }

    #[test]
    fn normalize_file_uri_only_strips_one_prefix() {
        // Defensive: avoid eating extra slashes if the caller has already
        // stripped once. The strip is exact, not greedy.
        assert_eq!(normalize_file_uri("file://file:///x"), "file:///x");
    }

    #[test]
    fn node_id_for_file_clause_shape() {
        // Sanity-check the SQL fragment hasn't drifted from the bind index.
        // If this fragment ever needs ?2 or a different column the call sites
        // must be updated in lockstep.
        assert!(NODE_ID_FOR_FILE.contains("?1"));
        assert!(NODE_ID_FOR_FILE.starts_with("node_id"));
    }

    #[test]
    fn state_changing_ops_pin_known_set() {
        // Bidirectional drift guard: the canonical set must be exactly
        // the hardcoded list, in some order. Iterating the hardcoded
        // list (the previous form) only caught *removal* — adding a
        // new op to STATE_CHANGING_OPS without updating the test
        // would silently pass and a state-changing op would silently
        // emit no event. Equality assertion fails in both directions.
        // (Caught by iter-35 adversarial review.)
        let mut actual: Vec<&str> = STATE_CHANGING_OPS.to_vec();
        actual.sort();
        let expected: Vec<&str> = {
            let mut v = vec!["load", "reparse", "flush", "snapshot", "enrich"];
            v.sort();
            v
        };
        assert_eq!(
            actual, expected,
            "STATE_CHANGING_OPS drift detected — update the hardcoded list \
             (and the matching test) when adding/removing mutating ops",
        );
    }

    #[test]
    fn state_changing_ops_excludes_pure_reads() {
        // The query/observation ops must NOT trigger an event emission.
        for op in [
            "status", "query", "list_children", "read_content",
            "find_callers", "find_defs", "get_node",
            "lsp_hover", "lsp_defs", "lsp_refs", "lsp_symbols", "lsp_diagnostics",
        ] {
            assert!(
                !is_state_changing(op),
                "read-only op `{op}` should not be state-changing",
            );
        }
    }

    #[test]
    fn state_changing_ops_unknown_returns_false() {
        // Defensive: an op that doesn't exist in the dispatch table must
        // not be called state-changing (avoids spurious events).
        assert!(!is_state_changing("nonexistent_op"));
        assert!(!is_state_changing(""));
    }

    #[tokio::test]
    async fn op_find_token_preserves_caller_supplied_json_key() {
        // Wire contract: op_find_callers must return rows under "callers",
        // op_find_defs under "defs". Clients (mache, hooks) parse the
        // specific key — a refactor that swapped them silently would
        // break clients without any test failing. Pin the dispatch
        // direction explicitly. (Caught by iter-35 adversarial review.)
        // setup() already creates the node_refs/node_defs tables via
        // create_refs_schema; we just need empty tables for the shape
        // test. handle_base_op routes find_callers → node_refs and
        // find_defs → node_defs.
        let (_dir, ctx) = setup();
        let callers = handle_base_op(&ctx, "find_callers", &json!({"token": "x"}))
            .unwrap();
        let defs = handle_base_op(&ctx, "find_defs", &json!({"token": "x"}))
            .unwrap();
        let callers_v: serde_json::Value = serde_json::from_str(&callers).unwrap();
        let defs_v: serde_json::Value = serde_json::from_str(&defs).unwrap();
        assert!(
            callers_v.get("callers").is_some(),
            "find_callers must use \"callers\" key; got {callers_v}",
        );
        assert!(
            defs_v.get("defs").is_some(),
            "find_defs must use \"defs\" key; got {defs_v}",
        );
        assert!(
            callers_v.get("defs").is_none() && defs_v.get("callers").is_none(),
            "keys must not cross-pollinate",
        );
    }

    #[test]
    fn query_token_in_table_returns_matching_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE node_refs (token TEXT, node_id TEXT, source_id TEXT);
             INSERT INTO node_refs VALUES
               ('foo', 'a/x', 'a.go'),
               ('foo', 'b/y', 'b.go'),
               ('bar', 'c/z', 'c.go');",
        )
        .unwrap();

        let rows = query_token_in_table(&conn, "foo", "node_refs").unwrap();
        assert_eq!(rows.len(), 2);
        let ids: std::collections::HashSet<&str> = rows
            .iter()
            .map(|r| r["node_id"].as_str().unwrap())
            .collect();
        assert!(ids.contains("a/x"));
        assert!(ids.contains("b/y"));

        let none = query_token_in_table(&conn, "missing", "node_refs").unwrap();
        assert!(none.is_empty());
    }

    /// Build the `CREATE TABLE` statement for an LSP 5-col position
    /// table (`_lsp_defs` with `def_*` columns, or `_lsp_refs` with
    /// `ref_*`). Replaces two byte-similar CREATE statements that
    /// only differed in their column-prefix substring.
    fn lsp_5col_create_sql(table: &str, prefix: &str) -> String {
        format!(
            "CREATE TABLE {table} (
                node_id TEXT,
                {prefix}_uri TEXT,
                {prefix}_start_line INTEGER,
                {prefix}_start_col INTEGER,
                {prefix}_end_line INTEGER,
                {prefix}_end_col INTEGER
            );"
        )
    }

    #[test]
    fn lsp_5col_position_rows_returns_empty_when_table_missing() {
        // Pre-enrichment state: callers must get an empty vec, not an
        // error. This is the signal that lazy enrichment should fire.
        let conn = Connection::open_in_memory().unwrap();
        let rows = lsp_5col_position_rows(&conn, "any/node", "_lsp_defs", "def").unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn lsp_5col_position_rows_decodes_def_shape() {
        let conn = Connection::open_in_memory().unwrap();
        let create = lsp_5col_create_sql("_lsp_defs", "def");
        conn.execute_batch(&format!(
            "{create}
             INSERT INTO _lsp_defs VALUES
               ('foo/main', 'file:///foo.rs', 10, 4, 12, 0),
               ('bar/baz', 'file:///bar.rs', 1, 0, 1, 8);"
        ))
        .unwrap();

        let rows = lsp_5col_position_rows(&conn, "foo/main", "_lsp_defs", "def").unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r["uri"], "file:///foo.rs");
        assert_eq!(r["start_line"], 10);
        assert_eq!(r["start_col"], 4);
        assert_eq!(r["end_line"], 12);
        assert_eq!(r["end_col"], 0);
    }

    #[test]
    fn lsp_5col_position_rows_handles_ref_prefix() {
        // The same helper services _lsp_refs with a different col prefix.
        let conn = Connection::open_in_memory().unwrap();
        let create = lsp_5col_create_sql("_lsp_refs", "ref");
        conn.execute_batch(&format!(
            "{create}
             INSERT INTO _lsp_refs VALUES ('x/y', 'file:///z.rs', 5, 2, 5, 7);"
        ))
        .unwrap();

        let rows = lsp_5col_position_rows(&conn, "x/y", "_lsp_refs", "ref").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["uri"], "file:///z.rs");
        assert_eq!(rows[0]["start_line"], 5);
    }


    /// Test-helper: dispatch `op` with `req` and assert the response
    /// is a JSON object containing an `error` field. Used by the
    /// input-validation pin triplet (op_load, op_query, op_reparse)
    /// which all share the same expected error-shape contract.
    fn assert_op_errors(ctx: &DaemonContext, op: &str, req: serde_json::Value, why: &str) {
        let resp = handle_base_op(ctx, op, &req)
            .unwrap_or_else(|| panic!("op {op} returned None for {why}"));
        let parsed: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert!(
            parsed.get("error").is_some(),
            "{op}: {why} should error; got {parsed}",
        );
    }

    fn setup() -> (TempDir, DaemonContext) {
        let dir = TempDir::new().unwrap();
        let arena_path = dir.path().join("test.arena");
        let ctrl_path = dir.path().join("test.ctrl");
        let _mmap = leyline_core::create_arena(&arena_path, 2 * 1024 * 1024).unwrap();
        let mut ctrl = leyline_core::Controller::open_or_create(&ctrl_path).unwrap();
        ctrl.set_arena(&arena_path.to_string_lossy(), 2 * 1024 * 1024, 0)
            .unwrap();

        // Create a living db with the nodes schema.
        let conn = Connection::open_in_memory().unwrap();
        leyline_ts::schema::create_ast_schema(&conn).unwrap();
        leyline_ts::schema::create_refs_schema(&conn).unwrap();

        #[cfg(feature = "vec")]
        let vec_index = {
            crate::daemon::vec_index::register_vec();
            Arc::new(crate::daemon::vec_index::VectorIndex::new(4, None).unwrap())
        };
        #[cfg(feature = "vec")]
        let embedder: Arc<dyn crate::daemon::embed::Embedder> =
            Arc::new(crate::daemon::embed::ZeroEmbedder { dim: 4 });
        let ctx = DaemonContext {
            ctrl_path,
            ext: Arc::new(crate::daemon::NoExt),
            router: crate::daemon::EventRouter::new(16),
            live_db: Mutex::new(conn),
            source_dir: None,
            lang_filter: None,
            enrichment_passes: vec![],
            state: Arc::new(RwLock::new(crate::daemon::DaemonState::initializing())),
            #[cfg(feature = "vec")]
            vec_index,
            #[cfg(feature = "vec")]
            embedder,
            #[cfg(feature = "vec")]
            embed_queue: Arc::new(std::sync::Mutex::new(std::collections::BinaryHeap::new())),
        };
        (dir, ctx)
    }

    #[tokio::test]
    async fn test_op_status_returns_generation() {
        let (_dir, ctx) = setup();
        let result = handle_base_op(&ctx, "status", &json!({}));
        assert!(result.is_some());
        let parsed: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["generation"], 0);
    }

    #[tokio::test]
    async fn op_reparse_errors_when_source_neither_field_nor_ctx() {
        // op_reparse pulls `source` from req or falls back to
        // ctx.source_dir. When neither is set, it must surface an
        // actionable error. setup() builds ctx with source_dir: None
        // so this test exercises the missing-everything fallthrough.
        let (_dir, ctx) = setup();
        assert_op_errors(&ctx, "reparse", json!({}), "missing source + no ctx fallback");
    }

    #[tokio::test]
    async fn op_query_errors_on_missing_or_invalid_sql() {
        // Input-validation triplet: op_query is the ad-hoc inspection
        // escape hatch. At scale a misconfigured client would
        // otherwise see panic / hang on a large registry db. Pin all
        // three failure modes via the shared assert_op_errors helper.
        let (_dir, ctx) = setup();
        assert_op_errors(&ctx, "query", json!({}), "missing sql");
        assert_op_errors(&ctx, "query", json!({"sql": 42}), "non-string sql");
        assert_op_errors(
            &ctx,
            "query",
            json!({"sql": "SELECT garbage FROM nowhere WHERE x SYNTAX_ERROR"}),
            "invalid sql",
        );
    }

    #[tokio::test]
    async fn required_string_ops_all_error_on_missing_field() {
        // Sweep every op that uses required_str_field. Each must
        // surface an actionable error when its required string field
        // is missing — otherwise a misconfigured client (or a future
        // typo in the MCP tool schema) could silently no-op or panic.
        // Uses the shared assert_op_errors helper to keep the sweep
        // compact. If a new op lands that takes a required string
        // field via required_str_field, add it here.
        let (_dir, ctx) = setup();
        let cases: &[(&str, &str)] = &[
            ("enrich", "pass"),
            ("get_node", "id"),
            ("read_content", "id"),
            ("find_callers", "token"),
            ("find_defs", "token"),
            ("lsp_symbols", "file"),
            ("lsp_diagnostics", "file"),
        ];
        for (op, _missing_field) in cases {
            assert_op_errors(&ctx, op, json!({}), &format!("{op} with no required field"));
        }
    }

    #[tokio::test]
    async fn op_load_errors_on_missing_or_invalid_db_field() {
        // Input-validation triplet: op_load takes a base64-encoded .db
        // payload. At scale a misconfigured client sending raw bytes,
        // forgetting the field, or with the wrong type would otherwise
        // see daemon hang or panic. Pin all three via the shared
        // assert_op_errors helper.
        let (_dir, ctx) = setup();
        assert_op_errors(&ctx, "load", json!({}), "missing db field");
        assert_op_errors(&ctx, "load", json!({"db": 42}), "non-string db");
        assert_op_errors(&ctx, "load", json!({"db": "!@#not-base64$%^"}), "invalid base64");
    }

    #[tokio::test]
    async fn op_status_wire_format_pins_required_fields() {
        // Wire-format pin parallel to enrichment_stats_serialize_to_
        // expected_json_shape and event_serialize_to_expected_json_
        // shape. op_status response is consumed by mache + cli
        // status checks; clients dispatch on every field name. The
        // existing test_op_status_returns_generation only checks ok
        // and generation. A refactor that renamed any required field
        // (or dropped one) would silently break every status-check
        // path. Pin all 6 always-emitted fields (ok, phase,
        // generation, arena_path, arena_size, enrichment).
        let (_dir, ctx) = setup();
        let result = handle_base_op(&ctx, "status", &json!({})).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let obj = parsed.as_object().expect("op_status returns an object");

        for required_field in ["ok", "phase", "generation", "arena_path", "arena_size", "enrichment"] {
            assert!(
                obj.contains_key(required_field),
                "op_status JSON must include `{required_field}`; got keys {:?}",
                obj.keys().collect::<Vec<_>>(),
            );
        }
        // phase from a fresh setup is "initializing".
        assert_eq!(parsed["phase"], "initializing");
        // enrichment is an object (possibly empty).
        assert!(parsed["enrichment"].is_object());
    }

    #[tokio::test]
    async fn test_op_flush_returns_ok() {
        let (_dir, ctx) = setup();
        let result = handle_base_op(&ctx, "flush", &json!({}));
        assert!(result.is_some());
        let parsed: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(parsed["ok"], true);
    }

    #[tokio::test]
    async fn test_unknown_op_returns_none() {
        let (_dir, ctx) = setup();
        assert!(handle_base_op(&ctx, "nonexistent", &json!({})).is_none());
    }

    #[tokio::test]
    async fn handle_base_op_dispatches_every_canonical_name() {
        // Drift guard: if a name is added to `base_op_names()` but not to
        // the `handle_base_op` match table, this test fails. We don't care
        // that some ops return errors with empty bodies — we only care that
        // dispatch returns `Some(...)`.
        let (_dir, ctx) = setup();
        for name in base_op_names() {
            assert!(
                handle_base_op(&ctx, name, &json!({})).is_some(),
                "handle_base_op did not recognize canonical op `{name}`",
            );
        }
    }

    #[test]
    fn read_generation_starts_at_zero_for_fresh_controller() {
        // Wire-contract: a brand-new controller reports generation 0.
        // This is what op_status / op_flush / op_load / op_reparse /
        // op_enrich / op_snapshot all surface to clients in the
        // `generation` field. If a future Controller bump changes the
        // initial value silently, integration tests that pin
        // `parsed["generation"] == 0` would fail mysteriously without
        // this unit test.
        let dir = TempDir::new().unwrap();
        let arena_path = dir.path().join("g.arena");
        let ctrl_path = dir.path().join("g.ctrl");
        let _mmap = leyline_core::create_arena(&arena_path, 1024 * 1024).unwrap();
        let mut ctrl = leyline_core::Controller::open_or_create(&ctrl_path).unwrap();
        ctrl.set_arena(&arena_path.to_string_lossy(), 1024 * 1024, 0)
            .unwrap();
        drop(ctrl);

        assert_eq!(read_generation(&ctrl_path).unwrap(), 0);
    }

    #[test]
    fn read_generation_propagates_open_failure() {
        // The "open controller" context string is part of the wire-error
        // contract. Pin its presence by triggering a path-doesn't-resolve
        // error. The test asserts the message reaches the caller through
        // anyhow's chain — without it, debugging a broken controller path
        // would be much harder.
        let bad_path = std::path::Path::new("/dev/null/definitely_not_a_directory/ctrl");
        let err = read_generation(bad_path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("open controller"),
            "expected error chain to mention 'open controller', got: {msg}",
        );
    }

    #[test]
    fn query_row_opt_returns_none_for_no_rows() {
        // The whole point of the helper: NoRows must not propagate as an
        // error. If a future rusqlite bump changed that, callers (read_content,
        // get_node, find_node_at_position, lsp_hover_query) would all start
        // returning Err for legitimate "id not found" lookups.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER, name TEXT);").unwrap();
        let r: Option<String> = query_row_opt(
            &conn,
            "SELECT name FROM t WHERE id = ?1",
            [42],
            |row| row.get(0),
        )
        .unwrap();
        assert_eq!(r, None);
    }

    #[test]
    fn query_row_opt_returns_some_for_match() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t (id INTEGER, name TEXT);
             INSERT INTO t VALUES (1, 'alpha'), (2, 'beta');",
        )
        .unwrap();
        let r: Option<String> = query_row_opt(
            &conn,
            "SELECT name FROM t WHERE id = ?1",
            [2],
            |row| row.get(0),
        )
        .unwrap();
        assert_eq!(r.as_deref(), Some("beta"));
    }

    #[test]
    fn query_row_opt_propagates_prepare_phase_errors() {
        // SQL errors at the prepare/query phase (bad table, bad column,
        // syntax) must NOT be swallowed as None — that would hide bugs
        // at runtime. Only the QueryReturnedNoRows variant collapses
        // to None.
        let conn = Connection::open_in_memory().unwrap();
        let r = query_row_opt(
            &conn,
            "SELECT * FROM definitely_not_a_table",
            [],
            |row| row.get::<_, String>(0),
        );
        assert!(r.is_err(), "expected error for missing table, got Ok");
    }

    #[test]
    fn query_row_opt_propagates_mapper_phase_errors() {
        // Distinct from the prepare-phase test: the mapper closure can
        // also fail (type-mismatch, missing column index). Those errors
        // also must propagate, not collapse to None. The previous test
        // only exercised the prepare path — this one exercises the
        // path through the mapper, where rusqlite returns the error
        // from inside `query_row` itself. (Caught by iter-35 adversarial
        // review.)
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t (id INTEGER, val TEXT); INSERT INTO t VALUES (1, 'hi');",
        )
        .unwrap();
        // Mapper asks for column 1 as i32 but it's TEXT — runtime type
        // mismatch surfaces from inside query_row. Must not be swallowed
        // as None (that would silently hide real type bugs).
        let r: Result<Option<i32>> = query_row_opt(
            &conn,
            "SELECT val FROM t WHERE id = ?1",
            [1],
            |row| row.get::<_, i32>(0),
        );
        assert!(
            r.is_err(),
            "type-mismatch in mapper must propagate as Err, got: {r:?}",
        );
    }

    #[test]
    fn node_not_found_response_pins_wire_contract() {
        // Clients (mache, hooks, etc) parse this error message. Pin the
        // exact shape so a refactor doesn't silently break their detection.
        let body = node_not_found_response("a/b/c");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "node 'a/b/c' not found");
    }

    #[test]
    fn node_not_found_response_quotes_id_for_disambiguation() {
        // The ID is wrapped in single quotes so an empty or whitespace
        // ID still produces a parseable message. If the quoting were
        // dropped, an empty id would yield "node  not found" which
        // looks like a different bug class.
        for id in ["", " ", "a/b", "weird id with spaces"] {
            let body = node_not_found_response(id);
            assert!(
                body.contains(&format!("'{id}'")),
                "expected single-quoted id `{id}` in response, got: {body}",
            );
        }
    }

    #[test]
    fn required_str_field_returns_borrowed_value() {
        let req = json!({"token": "Foo", "id": "abc"});
        assert_eq!(required_str_field(&req, "token").unwrap(), "Foo");
        assert_eq!(required_str_field(&req, "id").unwrap(), "abc");
    }

    #[test]
    fn required_str_field_error_includes_field_name() {
        // Wire contract: clients see this error string and key off the
        // field name. If we drop the field name from the message,
        // user-facing errors get worse without anyone noticing.
        let req = json!({});
        for field in ["token", "id", "sql", "pass", "query"] {
            let err = required_str_field(&req, field).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains(&format!("\"{field}\"")),
                "expected error to name field `{field}`, got: {msg}",
            );
        }
    }

    #[test]
    fn required_str_field_rejects_non_string_values() {
        // Numbers, nulls, arrays, objects all fail the same way. None
        // of these should slip past `as_str()` and hit downstream code.
        for bad in [json!(42), json!(null), json!([]), json!({"x": 1})] {
            let req = json!({"k": bad});
            assert!(required_str_field(&req, "k").is_err());
        }
    }

    #[test]
    fn parse_file_arg_strips_file_uri_prefix() {
        // The helper centralizes `file://` stripping. If a caller passes
        // an LSP-style URI, we must hand back the bare path so the SQL
        // `node_id LIKE ?1 || '%'` clause matches our node-id convention.
        let req = json!({"file": "file:///tmp/foo.rs"});
        assert_eq!(parse_file_arg(&req).unwrap(), "/tmp/foo.rs");
    }

    #[test]
    fn parse_file_arg_passes_plain_path_through() {
        // Plain paths must round-trip unchanged.
        let req = json!({"file": "src/lib.rs"});
        assert_eq!(parse_file_arg(&req).unwrap(), "src/lib.rs");
    }

    #[test]
    fn parse_file_arg_errors_on_missing_field() {
        // The error message is part of the wire contract — clients show
        // it directly. Pin its shape so a refactor doesn't silently
        // change what users see.
        let req = json!({});
        let err = parse_file_arg(&req).unwrap_err();
        assert!(
            format!("{err:#}").contains("missing \"file\""),
            "unexpected error: {err:#}",
        );
    }

    #[test]
    fn parse_file_arg_errors_on_non_string_field() {
        // A non-string `file` (e.g. number, object) must hit the same
        // error path as a missing key — both are equally broken from
        // the caller's perspective.
        for bad in [json!({"file": 42}), json!({"file": null}), json!({"file": []})] {
            assert!(parse_file_arg(&bad).is_err(), "expected error for {bad}");
        }
    }

    #[test]
    fn query_lsp_rows_for_file_returns_empty_when_table_missing() {
        // Pre-enrichment: the `_lsp` table doesn't exist yet. Helper
        // must return Ok(empty), NOT propagate "no such table" SQL
        // error. Mirrors the behavior of `lsp_5col_position_rows`
        // for defs/refs — both op families behave identically when
        // the underlying enrichment hasn't run. This pins the fix
        // for the asymmetry caught by the iter-35 adversarial review.
        let conn = Connection::open_in_memory().unwrap();
        let sql = format!("SELECT node_id FROM _lsp WHERE {NODE_ID_FOR_FILE}");
        let rows = query_lsp_rows_for_file(&conn, "src/lib.rs", "_lsp", &sql, |row| {
            Ok(json!({"node_id": row.get::<_, String>(0)?}))
        })
        .unwrap();
        assert!(rows.is_empty(), "missing table must yield empty, not error");
    }

    /// Open an in-memory conn with a minimal `_lsp(node_id, foo)`
    /// table — the fixture shape used by the
    /// `query_lsp_rows_for_file_*` tests below. Centralizes the
    /// CREATE so the table shape stays consistent.
    fn lsp_test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE _lsp (node_id TEXT, foo TEXT);")
            .unwrap();
        conn
    }

    #[test]
    fn query_lsp_rows_for_file_returns_empty_when_no_match() {
        // Pre-enrichment / no-rows-for-file is the common pre-LSP case;
        // callers expect an empty Vec, not an error.
        let conn = lsp_test_conn();
        conn.execute_batch("INSERT INTO _lsp VALUES ('other.rs/x', 'bar');")
            .unwrap();
        let sql = format!("SELECT node_id, foo FROM _lsp WHERE {NODE_ID_FOR_FILE}");
        let rows = query_lsp_rows_for_file(&conn, "src/lib.rs", "_lsp", &sql, |row| {
            Ok(json!({"node_id": row.get::<_, String>(0)?, "foo": row.get::<_, String>(1)?}))
        })
        .unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn query_lsp_rows_for_file_collects_matching_rows() {
        // The helper must hand back exactly the rows whose node_id starts
        // with `<file>/` — the LIKE prefix from NODE_ID_FOR_FILE is the
        // boundary between scoped queries (used by symbols/diagnostics)
        // and global queries (used by find_callers/find_defs).
        let conn = lsp_test_conn();
        conn.execute_batch(
            "INSERT INTO _lsp VALUES
                ('src/lib.rs/a', 'one'),
                ('src/lib.rs/b', 'two'),
                ('src/other.rs/c', 'three');",
        )
        .unwrap();
        let sql = format!("SELECT node_id, foo FROM _lsp WHERE {NODE_ID_FOR_FILE}");
        let rows = query_lsp_rows_for_file(&conn, "src/lib.rs", "_lsp", &sql, |row| {
            Ok(json!({"node_id": row.get::<_, String>(0)?, "foo": row.get::<_, String>(1)?}))
        })
        .unwrap();
        assert_eq!(rows.len(), 2, "expected 2 scoped rows, got {rows:?}");
        let foos: std::collections::HashSet<&str> =
            rows.iter().map(|r| r["foo"].as_str().unwrap()).collect();
        assert!(foos.contains("one"));
        assert!(foos.contains("two"));
    }

    #[test]
    fn lsp_rows_response_omits_enriched_when_false() {
        // Pinned shape: `enriched: true` must NOT appear when rows came
        // from the warm cache. Clients distinguish "served from cache"
        // vs "served after a lazy refresh" by the *presence* of the
        // key — adding it always would silently break that signal.
        let body = lsp_rows_response("definitions", vec![], false);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert!(v["definitions"].is_array());
        assert!(
            v.get("enriched").is_none(),
            "warm hit should not include `enriched` key, got {body}",
        );
    }

    #[test]
    fn lsp_rows_response_includes_enriched_when_true() {
        // Symmetric guard: when the helper is told to mark the response
        // as enriched (i.e. second attempt succeeded), the marker must
        // be present and equal to `true`.
        let body = lsp_rows_response("references", vec![json!({"x": 1})], true);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["enriched"], true);
        assert_eq!(v["references"][0]["x"], 1);
    }

    #[test]
    fn lsp_rows_response_uses_caller_supplied_key() {
        // Drift guard: the helper must use whatever `json_key` the caller
        // passes — `definitions` for op_lsp_defs, `references` for op_lsp_refs.
        // If a future caller picks a new key (e.g. `decls`), the helper must
        // honor it without modification.
        for key in ["definitions", "references", "decls"] {
            let body = lsp_rows_response(key, vec![], false);
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert!(
                v.get(key).is_some(),
                "expected key `{key}` in {body}",
            );
        }
    }

    #[test]
    fn state_changing_ops_subset_of_canonical_names() {
        // Mutating ops must be a subset of the canonical dispatch list.
        // Catches the case where someone retires an op from `handle_base_op`
        // but forgets to remove it from `STATE_CHANGING_OPS`.
        let canonical: std::collections::HashSet<&str> =
            base_op_names().into_iter().collect();
        for name in STATE_CHANGING_OPS {
            assert!(
                canonical.contains(name),
                "STATE_CHANGING_OPS contains `{name}` but base_op_names() does not",
            );
        }
    }
}
