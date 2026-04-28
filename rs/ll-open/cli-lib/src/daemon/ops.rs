//! Base op handlers for the daemon's UDS protocol.
//!
//! Each op queries the living in-memory SQLite database directly.
//! The arena is used only for periodic snapshots (crash recovery + mache).

use std::path::Path;

use anyhow::{Context, Result};
use leyline_core::Controller;
use rusqlite::Connection;
use serde_json::json;

use super::{DaemonContext, DaemonPhase};

// ---------------------------------------------------------------------------
// Public dispatch
// ---------------------------------------------------------------------------

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

/// Execute a closure with the living database connection.
fn with_live_db<F, T>(ctx: &DaemonContext, f: F) -> Result<T>
where
    F: FnOnce(&Connection) -> Result<T>,
{
    let guard = ctx.live_db.lock().unwrap();
    f(&guard)
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
    let ctrl = Controller::open_or_create(ctrl_path).context("open controller")?;
    Ok(json!({
        "ok": true,
        "generation": ctrl.generation(),
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
    let ctrl = Controller::open_or_create(ctrl_path).context("open controller")?;
    Ok(json!({"ok": true, "generation": ctrl.generation()}).to_string())
}

// ---------------------------------------------------------------------------
// Reparse + snapshot ops
// ---------------------------------------------------------------------------

fn op_reparse(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    let source = req
        .get("source")
        .and_then(|v| v.as_str())
        .or_else(|| ctx.source_dir.as_ref().map(|p| p.to_str().unwrap_or("")))
        .context("missing \"source\" field and no --source configured")?
        .to_string();
    let lang = req
        .get("lang")
        .and_then(|v| v.as_str())
        .or(ctx.lang_filter.as_deref());

    // Parse directly into the living db.
    ctx.state.write().unwrap().phase = DaemonPhase::Parsing;
    let guard = ctx.live_db.lock().unwrap();
    let result = match crate::cmd_parse::parse_into_conn(&guard, Path::new(&source), lang, None) {
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
    crate::cmd_daemon::snapshot_to_arena(
        &ctx.live_db.lock().unwrap(),
        &ctx.ctrl_path,
    )?;

    {
        let mut s = ctx.state.write().unwrap();
        s.phase = DaemonPhase::Ready;
        s.last_reparse_at_ms = Some(now_ms());
    }

    let ctrl = Controller::open_or_create(&ctx.ctrl_path).context("open controller")?;
    Ok(json!({
        "ok": true,
        "generation": ctrl.generation(),
        "parsed": result.parsed,
        "unchanged": result.unchanged,
        "deleted": result.deleted,
        "errors": result.errors,
        "changed_files": result.changed_files,
    })
    .to_string())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn op_enrich(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    let pass_name = req
        .get("pass")
        .and_then(|v| v.as_str())
        .context("missing \"pass\" field")?;
    let files: Option<Vec<String>> = req
        .get("files")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect());

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
    crate::cmd_daemon::snapshot_to_arena(
        &ctx.live_db.lock().unwrap(),
        &ctx.ctrl_path,
    )?;

    let ctrl = Controller::open_or_create(&ctx.ctrl_path).context("open controller")?;
    Ok(json!({
        "ok": true,
        "generation": ctrl.generation(),
        "passes": stats,
    })
    .to_string())
}

fn op_snapshot(ctx: &DaemonContext) -> Result<String> {
    crate::cmd_daemon::snapshot_to_arena(
        &ctx.live_db.lock().unwrap(),
        &ctx.ctrl_path,
    )?;
    let ctrl = Controller::open_or_create(&ctx.ctrl_path).context("open controller")?;
    Ok(json!({"ok": true, "generation": ctrl.generation()}).to_string())
}

// ---------------------------------------------------------------------------
// Query ops (use living db directly)
// ---------------------------------------------------------------------------

/// Raw SQL query — for ad-hoc inspection.
fn op_query(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    let sql = req
        .get("sql")
        .and_then(|v| v.as_str())
        .context("missing \"sql\" field")?;

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

    with_live_db(ctx, |conn| {
        let mut stmt = conn.prepare_cached(
            "SELECT id, name, kind, size FROM nodes WHERE parent_id = ?1 ORDER BY name",
        )?;
        let rows: Vec<serde_json::Value> = stmt
            .query_map([id], |row| {
                Ok(json!({
                    "id": row.get::<_, String>(0)?,
                    "name": row.get::<_, String>(1)?,
                    "kind": row.get::<_, i32>(2)?,
                    "size": row.get::<_, i64>(3)?,
                }))
            })?
            .collect::<Result<_, _>>()?;

        Ok(json!({"ok": true, "children": rows}).to_string())
    })
}

/// Read a node's content (the `record` column).
fn op_read_content(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    let id = req
        .get("id")
        .and_then(|v| v.as_str())
        .context("missing \"id\" field")?;

    with_live_db(ctx, |conn| {
        let result = conn.query_row(
            "SELECT record FROM nodes WHERE id = ?1",
            [id],
            |row| row.get::<_, String>(0),
        );
        match result {
            Ok(content) => Ok(json!({"ok": true, "content": content}).to_string()),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                Ok(json!({"ok": false, "error": format!("node '{id}' not found")}).to_string())
            }
            Err(e) => Err(e.into()),
        }
    })
}

/// Find callers of a token (queries node_refs).
fn op_find_callers(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    let token = req
        .get("token")
        .and_then(|v| v.as_str())
        .context("missing \"token\" field")?;

    with_live_db(ctx, |conn| {
        let mut stmt = conn.prepare_cached(
            "SELECT node_id, source_id FROM node_refs WHERE token = ?1",
        )?;
        let rows: Vec<serde_json::Value> = stmt
            .query_map([token], |row| {
                Ok(json!({
                    "node_id": row.get::<_, String>(0)?,
                    "source_id": row.get::<_, String>(1)?,
                }))
            })?
            .collect::<Result<_, _>>()?;

        Ok(json!({"ok": true, "callers": rows}).to_string())
    })
}

/// Find definitions of a token (queries node_defs).
fn op_find_defs(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    let token = req
        .get("token")
        .and_then(|v| v.as_str())
        .context("missing \"token\" field")?;

    with_live_db(ctx, |conn| {
        let mut stmt = conn.prepare_cached(
            "SELECT node_id, source_id FROM node_defs WHERE token = ?1",
        )?;
        let rows: Vec<serde_json::Value> = stmt
            .query_map([token], |row| {
                Ok(json!({
                    "node_id": row.get::<_, String>(0)?,
                    "source_id": row.get::<_, String>(1)?,
                }))
            })?
            .collect::<Result<_, _>>()?;

        Ok(json!({"ok": true, "defs": rows}).to_string())
    })
}

/// Get a single node by ID.
fn op_get_node(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    let id = req
        .get("id")
        .and_then(|v| v.as_str())
        .context("missing \"id\" field")?;

    with_live_db(ctx, |conn| {
        let result = conn.query_row(
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
        );
        match result {
            Ok(node) => Ok(json!({"ok": true, "node": node}).to_string()),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                Ok(json!({"ok": false, "error": format!("node '{id}' not found")}).to_string())
            }
            Err(e) => Err(e.into()),
        }
    })
}

// ---------------------------------------------------------------------------
// Position-based LSP query ops
// ---------------------------------------------------------------------------

/// Find the node_id at a given (file, line, col) position via the _ast table.
fn find_node_at_position(conn: &Connection, file: &str, line: u32, col: u32) -> Result<Option<String>> {
    // Find the most specific (smallest range) AST node containing this position.
    let result = conn.query_row(
        "SELECT node_id FROM _ast \
         WHERE source_id = ?1 \
           AND start_row <= ?2 AND end_row >= ?2 \
           AND (start_row < ?2 OR start_col <= ?3) \
           AND (end_row > ?2 OR end_col >= ?3) \
         ORDER BY (end_byte - start_byte) ASC \
         LIMIT 1",
        rusqlite::params![file, line, col],
        |row| row.get::<_, String>(0),
    );
    match result {
        Ok(id) => Ok(Some(id)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Parse file + line + col from request. File can be a path or file:// URI.
fn parse_position(req: &serde_json::Value) -> Result<(String, u32, u32)> {
    let file = req
        .get("file")
        .and_then(|v| v.as_str())
        .context("missing \"file\" field")?;
    // Strip file:// prefix if present.
    let file = file.strip_prefix("file://").unwrap_or(file);
    let file = file.to_string();

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
            "SELECT COUNT(*) > 0 FROM _lsp WHERE node_id LIKE ?1 || '%'",
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
    let hover = conn.query_row(
        "SELECT hover_text FROM _lsp_hover WHERE node_id = ?1",
        [&node_id],
        |row| row.get::<_, String>(0),
    );
    match hover {
        Ok(text) => Ok(Some((text, node_id))),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Go-to-definition at a position. Auto-enriches if no data exists.
fn op_lsp_defs(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    let (file, line, col) = parse_position(req)?;

    let result = with_live_db(ctx, |conn| lsp_defs_query(conn, &file, line, col))?;

    if result.is_empty() {
        let enriched = with_live_db(ctx, |conn| Ok(maybe_enrich(ctx, conn, &file)))?;
        if enriched {
            let result = with_live_db(ctx, |conn| lsp_defs_query(conn, &file, line, col))?;
            return Ok(json!({"ok": true, "definitions": result, "enriched": true}).to_string());
        }
    }

    Ok(json!({"ok": true, "definitions": result}).to_string())
}

fn lsp_defs_query(conn: &Connection, file: &str, line: u32, col: u32) -> Result<Vec<serde_json::Value>> {
    let node_id = match find_node_at_position(conn, file, line, col)? {
        Some(id) => id,
        None => return Ok(vec![]),
    };

    // Check if table exists.
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='_lsp_defs'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(false);
    if !exists {
        return Ok(vec![]);
    }

    let mut stmt = conn.prepare_cached(
        "SELECT def_uri, def_start_line, def_start_col, def_end_line, def_end_col \
         FROM _lsp_defs WHERE node_id = ?1",
    )?;
    let rows: Vec<serde_json::Value> = stmt
        .query_map([&node_id], |row| {
            Ok(json!({
                "uri": row.get::<_, String>(0)?,
                "start_line": row.get::<_, i32>(1)?,
                "start_col": row.get::<_, i32>(2)?,
                "end_line": row.get::<_, i32>(3)?,
                "end_col": row.get::<_, i32>(4)?,
            }))
        })?
        .collect::<Result<_, _>>()?;

    Ok(rows)
}

/// Find references at a position. Auto-enriches if no data exists.
fn op_lsp_refs(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    let (file, line, col) = parse_position(req)?;

    let result = with_live_db(ctx, |conn| lsp_refs_query(conn, &file, line, col))?;

    if result.is_empty() {
        let enriched = with_live_db(ctx, |conn| Ok(maybe_enrich(ctx, conn, &file)))?;
        if enriched {
            let result = with_live_db(ctx, |conn| lsp_refs_query(conn, &file, line, col))?;
            return Ok(json!({"ok": true, "references": result, "enriched": true}).to_string());
        }
    }

    Ok(json!({"ok": true, "references": result}).to_string())
}

fn lsp_refs_query(conn: &Connection, file: &str, line: u32, col: u32) -> Result<Vec<serde_json::Value>> {
    let node_id = match find_node_at_position(conn, file, line, col)? {
        Some(id) => id,
        None => return Ok(vec![]),
    };

    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='_lsp_refs'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(false);
    if !exists {
        return Ok(vec![]);
    }

    let mut stmt = conn.prepare_cached(
        "SELECT ref_uri, ref_start_line, ref_start_col, ref_end_line, ref_end_col \
         FROM _lsp_refs WHERE node_id = ?1",
    )?;
    let rows: Vec<serde_json::Value> = stmt
        .query_map([&node_id], |row| {
            Ok(json!({
                "uri": row.get::<_, String>(0)?,
                "start_line": row.get::<_, i32>(1)?,
                "start_col": row.get::<_, i32>(2)?,
                "end_line": row.get::<_, i32>(3)?,
                "end_col": row.get::<_, i32>(4)?,
            }))
        })?
        .collect::<Result<_, _>>()?;

    Ok(rows)
}

/// Document symbols for a file.
fn op_lsp_symbols(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    let file = req
        .get("file")
        .and_then(|v| v.as_str())
        .context("missing \"file\" field")?;
    let file = file.strip_prefix("file://").unwrap_or(file);

    with_live_db(ctx, |conn| {
        let mut stmt = conn.prepare_cached(
            "SELECT node_id, symbol_kind, detail, start_line, start_col, end_line, end_col \
             FROM _lsp WHERE node_id LIKE ?1 || '%'",
        )?;
        let rows: Vec<serde_json::Value> = stmt
            .query_map([file], |row| {
                Ok(json!({
                    "node_id": row.get::<_, String>(0)?,
                    "kind": row.get::<_, String>(1)?,
                    "detail": row.get::<_, String>(2)?,
                    "start_line": row.get::<_, i32>(3)?,
                    "start_col": row.get::<_, i32>(4)?,
                    "end_line": row.get::<_, i32>(5)?,
                    "end_col": row.get::<_, i32>(6)?,
                }))
            })?
            .collect::<Result<_, _>>()?;

        Ok(json!({"ok": true, "symbols": rows}).to_string())
    })
}

/// Diagnostics for a file.
fn op_lsp_diagnostics(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    let file = req
        .get("file")
        .and_then(|v| v.as_str())
        .context("missing \"file\" field")?;
    let file = file.strip_prefix("file://").unwrap_or(file);

    with_live_db(ctx, |conn| {
        let mut stmt = conn.prepare_cached(
            "SELECT node_id, diagnostics, start_line, start_col, end_line, end_col \
             FROM _lsp WHERE node_id LIKE ?1 || '%' AND diagnostics IS NOT NULL AND diagnostics != ''",
        )?;
        let rows: Vec<serde_json::Value> = stmt
            .query_map([file], |row| {
                Ok(json!({
                    "node_id": row.get::<_, String>(0)?,
                    "diagnostics": row.get::<_, String>(1)?,
                    "start_line": row.get::<_, i32>(2)?,
                    "start_col": row.get::<_, i32>(3)?,
                    "end_line": row.get::<_, i32>(4)?,
                    "end_col": row.get::<_, i32>(5)?,
                }))
            })?
            .collect::<Result<_, _>>()?;

        Ok(json!({"ok": true, "diagnostics": rows}).to_string())
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
    let query = req
        .get("query")
        .and_then(|v| v.as_str())
        .context("missing \"query\" field")?;
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
}
