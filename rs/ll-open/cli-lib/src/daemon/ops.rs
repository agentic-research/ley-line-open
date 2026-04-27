//! Base op handlers for the daemon's UDS protocol.
//!
//! Each op queries the arena's active SQLite buffer via `sqlite3_deserialize`.
//! A cached connection is reused across requests, invalidated on generation change.

use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result};
use leyline_core::{ArenaHeader, Controller};
use memmap2::Mmap;
use rusqlite::{Connection, DatabaseName};
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
        "reparse" => Some(op_reparse(&ctx.ctrl_path, req)),
        // Structured query ops — zero-copy from arena via cached connection.
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
// Connection cache
// ---------------------------------------------------------------------------

/// Get the cached arena connection, refreshing if the generation changed.
fn with_arena_conn<F, T>(ctx: &DaemonContext, f: F) -> Result<T>
where
    F: FnOnce(&Connection) -> Result<T>,
{
    let ctrl = Controller::open_or_create(&ctx.ctrl_path).context("open controller")?;
    let current_gen = ctrl.generation();

    let mut guard = ctx.arena_conn.lock().unwrap();

    let needs_refresh = match &*guard {
        Some((cached_gen, _)) => *cached_gen != current_gen,
        None => true,
    };

    if needs_refresh {
        let conn = open_arena_db(&ctx.ctrl_path)?;
        *guard = Some((current_gen, conn));
    }

    let (_, conn) = guard.as_ref().unwrap();
    f(conn)
}

// ---------------------------------------------------------------------------
// Control ops (don't need arena connection)
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

fn op_reparse(ctrl_path: &Path, req: &serde_json::Value) -> Result<String> {
    let source = req
        .get("source")
        .and_then(|v| v.as_str())
        .context("missing \"source\" field")?;
    let lang = req.get("lang").and_then(|v| v.as_str());

    let tmp = tempfile::NamedTempFile::new().context("create temp .db")?;
    let db_path = tmp.path().to_path_buf();
    crate::cmd_parse::cmd_parse(Path::new(source), &db_path, lang)?;
    let db_bytes = std::fs::read(&db_path).context("read temp .db")?;
    crate::cmd_load::load_into_arena(ctrl_path, &db_bytes)?;

    let ctrl = Controller::open_or_create(ctrl_path).context("open controller")?;
    Ok(json!({"ok": true, "generation": ctrl.generation()}).to_string())
}

// ---------------------------------------------------------------------------
// Query ops (use cached arena connection)
// ---------------------------------------------------------------------------

/// Raw SQL query — for ad-hoc inspection.
fn op_query(ctx: &DaemonContext, req: &serde_json::Value) -> Result<String> {
    let sql = req
        .get("sql")
        .and_then(|v| v.as_str())
        .context("missing \"sql\" field")?;

    with_arena_conn(ctx, |conn| {
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

    with_arena_conn(ctx, |conn| {
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

    with_arena_conn(ctx, |conn| {
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

    with_arena_conn(ctx, |conn| {
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

    with_arena_conn(ctx, |conn| {
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

    with_arena_conn(ctx, |conn| {
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
// Helpers
// ---------------------------------------------------------------------------

/// Open the arena's active buffer as a read-only in-memory SQLite connection.
fn open_arena_db(ctrl_path: &Path) -> Result<Connection> {
    let ctrl = Controller::open_or_create(ctrl_path).context("open controller")?;
    let arena_path = ctrl.arena_path();
    anyhow::ensure!(!arena_path.is_empty(), "controller has no arena path");

    let file = std::fs::File::open(&arena_path)
        .with_context(|| format!("open arena file: {arena_path}"))?;
    let mmap = unsafe { Mmap::map(&file)? };

    let header: &ArenaHeader =
        bytemuck::from_bytes(&mmap[..std::mem::size_of::<ArenaHeader>()]);

    let file_size = mmap.len() as u64;
    let offset = header
        .active_buffer_offset(file_size)
        .context("invalid arena header")?;
    let buf_size = ArenaHeader::buffer_size(file_size);

    let buf = &mmap[offset as usize..(offset + buf_size) as usize];

    let mut conn = Connection::open_in_memory()?;
    conn.deserialize_read_exact(DatabaseName::Main, Cursor::new(buf), buf.len(), true)
        .context("sqlite3_deserialize failed")?;

    Ok(conn)
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

        let ctx = DaemonContext {
            ctrl_path,
            ext: Arc::new(crate::daemon::NoExt),
            router: crate::daemon::EventRouter::new(16),
            arena_conn: Mutex::new(None),
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
