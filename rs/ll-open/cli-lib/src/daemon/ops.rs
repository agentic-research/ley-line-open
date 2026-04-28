//! Base op handlers for the daemon's UDS protocol.
//!
//! Each op queries the living in-memory SQLite database directly.
//! The arena is used only for periodic snapshots (crash recovery + mache).

use std::path::Path;

use anyhow::{Context, Result};
use leyline_core::Controller;
use rusqlite::Connection;
use serde_json::json;

use super::DaemonContext;

// ---------------------------------------------------------------------------
// Public dispatch
// ---------------------------------------------------------------------------

/// Dispatch a base op. Returns `Some(json_string)` if handled, `None` if unrecognized.
pub fn handle_base_op(ctx: &DaemonContext, op: &str, req: &serde_json::Value) -> Option<String> {
    let result = match op {
        "status" => Some(op_status(&ctx.ctrl_path)),
        "flush" => Some(op_flush(&ctx.ctrl_path)),
        "load" => Some(op_load(&ctx.ctrl_path, req)),
        "query" => Some(op_query(ctx, req)),
        "reparse" => Some(op_reparse(ctx, req)),
        "snapshot" => Some(op_snapshot(ctx)),
        // Structured query ops — direct from living db.
        "list_roots" => Some(op_list_children(ctx, &json!({"id": ""}))),
        "list_children" => Some(op_list_children(ctx, req)),
        "read_content" => Some(op_read_content(ctx, req)),
        "find_callers" => Some(op_find_callers(ctx, req)),
        "find_defs" => Some(op_find_defs(ctx, req)),
        "get_node" => Some(op_get_node(ctx, req)),
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

fn op_status(ctrl_path: &Path) -> Result<String> {
    let ctrl = Controller::open_or_create(ctrl_path).context("open controller")?;
    Ok(json!({
        "ok": true,
        "generation": ctrl.generation(),
        "arena_path": ctrl.arena_path(),
        "arena_size": ctrl.arena_size(),
    })
    .to_string())
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
    let guard = ctx.live_db.lock().unwrap();
    let result = crate::cmd_parse::parse_into_conn(&guard, Path::new(&source), lang)?;
    drop(guard);

    // Snapshot to arena for mache/remote consumers.
    crate::cmd_daemon::snapshot_to_arena(
        &ctx.live_db.lock().unwrap(),
        &ctx.ctrl_path,
    )?;

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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
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

        let ctx = DaemonContext {
            ctrl_path,
            ext: Arc::new(crate::daemon::NoExt),
            router: crate::daemon::EventRouter::new(16),
            live_db: Mutex::new(conn),
            source_dir: None,
            lang_filter: None,
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
