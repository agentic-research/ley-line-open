//! Base op handlers for the daemon's UDS protocol.
//!
//! Each op reads from / writes to the ley-line arena via the Controller.
//! `handle_base_op` returns `Some(json)` if the op is recognized, `None` otherwise.

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
        "query" => Some(op_query(&ctx.ctrl_path, req)),
        "reparse" => Some(op_reparse(&ctx.ctrl_path, req)),
        _ => None,
    };
    result.map(|r| match r {
        Ok(v) => v,
        Err(e) => json!({"ok": false, "error": format!("{e:#}")}).to_string(),
    })
}

// ---------------------------------------------------------------------------
// Individual ops
// ---------------------------------------------------------------------------

/// `status` — returns generation, arena path, and arena size.
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

/// `flush` — returns the current generation (actual flush is a no-op for now).
fn op_flush(ctrl_path: &Path) -> Result<String> {
    let ctrl = Controller::open_or_create(ctrl_path).context("open controller")?;
    Ok(json!({
        "ok": true,
        "generation": ctrl.generation(),
    })
    .to_string())
}

/// `load` — decodes base64 .db payload, loads into arena, returns new generation.
///
/// Expected request fields:
///   `"db"`: base64-encoded SQLite bytes
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
    Ok(json!({
        "ok": true,
        "generation": ctrl.generation(),
    })
    .to_string())
}

/// `query` — runs SQL against the arena's active buffer, returns rows as JSON.
///
/// Expected request fields:
///   `"sql"`: SQL string to execute
fn op_query(ctrl_path: &Path, req: &serde_json::Value) -> Result<String> {
    let sql = req
        .get("sql")
        .and_then(|v| v.as_str())
        .context("missing \"sql\" field")?;

    let conn = open_arena_db(ctrl_path)?;
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

    Ok(json!({
        "ok": true,
        "columns": headers,
        "rows": rows_out,
    })
    .to_string())
}

/// `reparse` — re-parses source directory, writes .db, loads into arena.
///
/// Expected request fields:
///   `"source"`: path to source directory
///   `"lang"`:   (optional) language filter
fn op_reparse(ctrl_path: &Path, req: &serde_json::Value) -> Result<String> {
    let source = req
        .get("source")
        .and_then(|v| v.as_str())
        .context("missing \"source\" field")?;

    let lang = req.get("lang").and_then(|v| v.as_str());

    // Write to a temp .db, then load into arena.
    let tmp = tempfile::NamedTempFile::new().context("create temp .db")?;
    let db_path = tmp.path().to_path_buf();

    crate::cmd_parse::cmd_parse(Path::new(source), &db_path, lang)?;

    let db_bytes = std::fs::read(&db_path).context("read temp .db")?;
    crate::cmd_load::load_into_arena(ctrl_path, &db_bytes)?;

    let ctrl = Controller::open_or_create(ctrl_path).context("open controller")?;
    Ok(json!({
        "ok": true,
        "generation": ctrl.generation(),
    })
    .to_string())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Open the arena's active buffer as a read-only in-memory SQLite connection.
///
/// Mirrors the logic in `cmd_inspect::open_arena_db` but takes only a ctrl path.
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
    use std::sync::Arc;
    use tempfile::TempDir;

    /// Create a temporary arena + controller for testing.
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
        };
        (dir, ctx)
    }

    #[tokio::test]
    async fn test_op_status_returns_generation() {
        let (_dir, ctx) = setup();
        let req = json!({});
        let result = handle_base_op(&ctx, "status", &req);
        assert!(result.is_some());
        let parsed: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["generation"], 0);
    }

    #[tokio::test]
    async fn test_op_flush_returns_ok() {
        let (_dir, ctx) = setup();
        let req = json!({});
        let result = handle_base_op(&ctx, "flush", &req);
        assert!(result.is_some());
        let parsed: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(parsed["ok"], true);
    }

    #[tokio::test]
    async fn test_unknown_op_returns_none() {
        let (_dir, ctx) = setup();
        let req = json!({});
        let result = handle_base_op(&ctx, "nonexistent", &req);
        assert!(result.is_none());
    }
}
