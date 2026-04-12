# Open Daemon Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an open daemon to `leyline-cli-lib` with a UDS socket, base ops (status, flush, load, query, reparse), event router (pub/sub), and a `DaemonExt` trait for ley-line (private) to register additional ops.

**Architecture:** The daemon reuses `cmd_serve`'s arena setup + mount logic, then layers a UDS socket on top. The socket handles line-delimited JSON. Base ops call existing functions (`load_into_arena`, `open_arena_db`). The `DaemonExt` trait lets LL register sheaf/embed/network ops without LLO knowing about them. The event router is a standalone pub/sub bus (ported from ley-line's `event_router.rs`).

**Tech Stack:** Rust (edition 2024), tokio (full), serde_json, clap 4, anyhow, leyline-core, leyline-fs

## Hard Rules

1. Every component gets tests. The event router gets unit tests (ported from ley-line). The socket gets integration tests using a real UDS connection.
2. No code duplication. Reuse `cmd_serve::setup_arena()`, `cmd_load::load_into_arena()`, `cmd_inspect::open_arena_db()`.
3. Each file has one responsibility. The daemon module is split into: ext.rs (trait), events.rs (router), ops.rs (handlers), socket.rs (listener).
4. The `DaemonExt` trait is the ONLY extension point. No other hooks, no feature flags for private ops.

## File Structure

```
rs/ll-open/cli-lib/src/
  daemon/
    mod.rs          — re-exports, DaemonContext struct
    ext.rs          — DaemonExt trait definition
    events.rs       — Event router (pub/sub bus)
    ops.rs          — Base op handlers (status, flush, load, query, reparse)
    socket.rs       — UDS listener + per-connection dispatch loop
  cmd_daemon.rs     — CLI entry point (args → setup → run)
  lib.rs            — add Daemon variant to Commands enum
```

---

### Task 1: DaemonExt trait + DaemonContext

**Files:**
- Create: `rs/ll-open/cli-lib/src/daemon/mod.rs`
- Create: `rs/ll-open/cli-lib/src/daemon/ext.rs`

- [ ] **Step 1: Create `daemon/ext.rs` — the extension trait**

```rust
//! Extension trait for ley-line (private) to register additional daemon ops.

use std::future::Future;
use std::pin::Pin;

/// Extension point for private daemon ops.
///
/// The base daemon handles: status, flush, load, query, reparse, subscribe,
/// unsubscribe, emit. Any unrecognized op is passed to the extension.
///
/// Return `Some(json_string)` if handled, `None` to fall through to "unknown op".
pub trait DaemonExt: Send + Sync {
    /// Handle a synchronous op.
    fn handle_op(&self, op: &str, req: &serde_json::Value) -> Option<String> {
        let _ = (op, req);
        None
    }

    /// Handle an async op (e.g., LSP tool invocation).
    fn handle_op_async<'a>(
        &'a self,
        op: &'a str,
        req: &'a serde_json::Value,
    ) -> Option<Pin<Box<dyn Future<Output = String> + Send + 'a>>> {
        let _ = (op, req);
        None
    }
}

/// No-op extension that rejects all ops. Used when no private extension is registered.
pub struct NoExt;

impl DaemonExt for NoExt {}
```

- [ ] **Step 2: Create `daemon/mod.rs` — re-exports and DaemonContext**

```rust
//! Open daemon: UDS socket + event router + extensible ops.

pub mod ext;
pub mod events;
pub mod ops;
pub mod socket;

use std::path::PathBuf;
use std::sync::Arc;

pub use ext::{DaemonExt, NoExt};
pub use events::EventRouter;

/// Shared state passed to all op handlers.
pub struct DaemonContext {
    pub ctrl_path: PathBuf,
    pub ext: Arc<dyn DaemonExt>,
    pub router: Arc<EventRouter>,
}
```

- [ ] **Step 3: Verify it compiles**

Run: `cd /Users/jamesgardner/remotes/art/ley-line-open/rs && cargo build -p leyline-cli-lib`

This will fail because `events`, `ops`, `socket` modules don't exist yet. Create empty placeholder files:

```rust
// daemon/events.rs
//! Event router (pub/sub bus). Implemented in Task 2.

// daemon/ops.rs
//! Base op handlers. Implemented in Task 3.

// daemon/socket.rs
//! UDS listener. Implemented in Task 4.
```

Add `pub mod daemon;` to `lib.rs` (after `pub mod cmd_splice;`).

Run: `cargo build -p leyline-cli-lib`

Expected: Compiles with no errors.

- [ ] **Step 4: Commit**

```bash
git add rs/ll-open/cli-lib/src/daemon/ rs/ll-open/cli-lib/src/lib.rs
git commit -m "feat(daemon): add DaemonExt trait + DaemonContext scaffold"
```

---

### Task 2: Event router

**Files:**
- Modify: `rs/ll-open/cli-lib/src/daemon/events.rs`

- [ ] **Step 1: Port event router from ley-line**

Port `ley-line/rs/crates/cli/src/event_router.rs` into `daemon/events.rs`. This file is self-contained — no private deps. Copy it verbatim with these adjustments:
- Change module doc to reference LLO, not ley-line
- Keep all types: `Event`, `OverflowPolicy`, `EventRouter`, `EventEmitter`, `ConnectionState`
- Keep all methods: `new()`, `emitter()`, `subscribe()`, `unsubscribe_topics()`, `remove_subscriber()`, `emit_external()`, `head_seq()`
- Keep `ConnectionState` with `handle_subscribe()`, `handle_unsubscribe()`, `handle_emit()`, `take_event_rx()`, `cleanup()`
- Keep all internal types: `TopicPattern`, `PatternSegment`, `EventLog`, `Subscriber`
- Keep all tests (10 tests: 7 unit + 3 async)

The full source is ~743 lines including tests. Port it as-is — it's well-tested and has no private deps.

Required imports at the top:

```rust
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{RwLock, mpsc};
```

Add `serde` to cli-lib's Cargo.toml dependencies:

```toml
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

(`serde_json` is already there, `serde` with `derive` needs to be added.)

- [ ] **Step 2: Update `daemon/mod.rs` to use real EventRouter**

The `pub use events::EventRouter;` already exists from Task 1. Verify `DaemonContext` references it correctly.

- [ ] **Step 3: Run tests**

Run: `cd /Users/jamesgardner/remotes/art/ley-line-open/rs && cargo test -p leyline-cli-lib`

Expected: All existing tests pass + 10 new event router tests pass.

- [ ] **Step 4: Commit**

```bash
git add rs/ll-open/cli-lib/src/daemon/events.rs rs/ll-open/cli-lib/Cargo.toml
git commit -m "feat(daemon): port event router — pub/sub bus with topic patterns"
```

---

### Task 3: Base op handlers

**Files:**
- Modify: `rs/ll-open/cli-lib/src/daemon/ops.rs`

- [ ] **Step 1: Implement base ops**

```rust
//! Base op handlers for the daemon's UDS protocol.
//!
//! Each handler takes a JSON request and returns a JSON response string.
//! These ops use only open crates (leyline-core, rusqlite).

use std::path::Path;

use crate::cmd_load::load_into_arena;
use crate::daemon::DaemonContext;

fn ok(v: serde_json::Value) -> String {
    serde_json::to_string(&v).unwrap()
}

fn err(msg: &str) -> String {
    serde_json::to_string(&serde_json::json!({"error": msg})).unwrap()
}

/// Dispatch a base op. Returns `Some(response)` if handled, `None` if unrecognized.
pub fn handle_base_op(ctx: &DaemonContext, op: &str, req: &serde_json::Value) -> Option<String> {
    match op {
        "status" => Some(op_status(&ctx.ctrl_path)),
        "flush" => Some(op_flush(&ctx.ctrl_path)),
        "load" => Some(op_load(&ctx.ctrl_path, req)),
        "query" => Some(op_query(&ctx.ctrl_path, req)),
        "reparse" => Some(op_reparse(&ctx.ctrl_path, req)),
        _ => None,
    }
}

fn op_status(ctrl_path: &Path) -> String {
    match leyline_core::Controller::open_or_create(ctrl_path) {
        Ok(ctrl) => ok(serde_json::json!({
            "generation": ctrl.generation(),
            "arena": ctrl.arena_path(),
            "arena_size": ctrl.arena_size(),
        })),
        Err(e) => err(&format!("controller: {e}")),
    }
}

fn op_flush(ctrl_path: &Path) -> String {
    match leyline_core::Controller::open_or_create(ctrl_path) {
        Ok(ctrl) => {
            let generation = ctrl.generation();
            ok(serde_json::json!({"ok": true, "generation": generation}))
        }
        Err(e) => err(&format!("flush: {e}")),
    }
}

fn op_load(ctrl_path: &Path, req: &serde_json::Value) -> String {
    let b64 = match req.get("db").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return err("load requires 'db' field (base64-encoded)"),
    };
    let db_bytes = match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64) {
        Ok(b) => b,
        Err(e) => return err(&format!("base64 decode: {e}")),
    };
    match load_into_arena(ctrl_path, &db_bytes) {
        Ok(()) => {
            let generation = leyline_core::Controller::open_or_create(ctrl_path)
                .map(|c| c.generation())
                .unwrap_or(0);
            ok(serde_json::json!({"ok": true, "generation": generation}))
        }
        Err(e) => err(&format!("load: {e}")),
    }
}

fn op_query(ctrl_path: &Path, req: &serde_json::Value) -> String {
    let sql = match req.get("sql").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return err("query requires 'sql' field"),
    };
    match crate::cmd_inspect::query_arena(ctrl_path, sql) {
        Ok(rows) => ok(serde_json::json!({"rows": rows})),
        Err(e) => err(&format!("query: {e}")),
    }
}

fn op_reparse(ctrl_path: &Path, req: &serde_json::Value) -> String {
    let source = match req.get("path").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return err("reparse requires 'path' field (source directory)"),
    };
    let output = match req.get("output").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return err("reparse requires 'output' field (.db path)"),
    };
    let source_path = std::path::Path::new(source);
    let output_path = std::path::Path::new(output);

    match crate::cmd_parse::cmd_parse(source_path, output_path, None) {
        Ok(()) => {
            // Auto-load into arena if controller exists
            if let Ok(db_bytes) = std::fs::read(output_path) {
                if load_into_arena(ctrl_path, &db_bytes).is_ok() {
                    let gen = leyline_core::Controller::open_or_create(ctrl_path)
                        .map(|c| c.generation())
                        .unwrap_or(0);
                    return ok(serde_json::json!({"ok": true, "reparsed": source, "generation": gen}));
                }
            }
            ok(serde_json::json!({"ok": true, "reparsed": source}))
        }
        Err(e) => err(&format!("reparse: {e}")),
    }
}
```

Note: `op_load` needs `base64` crate. Add to cli-lib Cargo.toml: `base64 = "0.22"`.

Also, `cmd_inspect::query_arena` may need to be made `pub`. Check and update if needed — it should be a pub function that takes `(ctrl_path, sql) -> Result<Vec<Vec<Value>>>`. If it's currently private or inline, extract it.

- [ ] **Step 2: Add unit tests for ops**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::{DaemonContext, NoExt, EventRouter};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn test_ctx() -> (DaemonContext, TempDir) {
        let dir = TempDir::new().unwrap();
        let arena_path = dir.path().join("test.arena");
        let ctrl_path = dir.path().join("test.ctrl");

        // Create a minimal arena + controller
        let _mmap = leyline_core::create_arena(&arena_path, 2 * 1024 * 1024).unwrap();
        let mut ctrl = leyline_core::Controller::open_or_create(&ctrl_path).unwrap();
        let arena_str = arena_path.to_string_lossy().to_string();
        ctrl.set_arena(&arena_str, 2 * 1024 * 1024, 0).unwrap();

        let router = EventRouter::new(100);
        let ctx = DaemonContext {
            ctrl_path,
            ext: Arc::new(NoExt),
            router,
        };
        (ctx, dir)
    }

    #[test]
    fn op_status_returns_generation() {
        let (ctx, _dir) = test_ctx();
        let resp = op_status(&ctx.ctrl_path);
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["generation"], 0);
    }

    #[test]
    fn op_flush_returns_generation() {
        let (ctx, _dir) = test_ctx();
        let resp = op_flush(&ctx.ctrl_path);
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert!(v["ok"].as_bool().unwrap());
    }

    #[test]
    fn unknown_op_returns_none() {
        let (ctx, _dir) = test_ctx();
        let req = serde_json::json!({});
        assert!(handle_base_op(&ctx, "nonexistent", &req).is_none());
    }
}
```

- [ ] **Step 3: Build and test**

Run: `cd /Users/jamesgardner/remotes/art/ley-line-open/rs && cargo test -p leyline-cli-lib`

Expected: All tests pass.

- [ ] **Step 4: Commit**

```bash
git add rs/ll-open/cli-lib/src/daemon/ops.rs rs/ll-open/cli-lib/Cargo.toml
git commit -m "feat(daemon): base op handlers — status, flush, load, query, reparse"
```

---

### Task 4: UDS socket listener

**Files:**
- Modify: `rs/ll-open/cli-lib/src/daemon/socket.rs`

- [ ] **Step 1: Implement the UDS listener**

```rust
//! UDS socket listener for the daemon's line-delimited JSON protocol.
//!
//! Each connection gets its own task. Requests are dispatched:
//! 1. Event ops (subscribe, unsubscribe, emit) → ConnectionState
//! 2. Base ops (status, flush, load, query, reparse) → ops::handle_base_op
//! 3. Extension ops → DaemonExt::handle_op / handle_op_async
//! 4. Unknown → {"error": "unknown op: ..."}

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use crate::daemon::events::ConnectionState;
use crate::daemon::ops;
use crate::daemon::DaemonContext;

/// Spawn the UDS listener. Returns the socket path.
///
/// The listener runs until the task is dropped/cancelled.
pub fn spawn(ctx: Arc<DaemonContext>, sock_path: PathBuf) -> PathBuf {
    let _ = std::fs::remove_file(&sock_path);

    tokio::spawn(listener_loop(ctx, sock_path.clone()));

    // Symlink to ~/.mache/default.sock for auto-discovery
    if let Some(home) = dirs::home_dir() {
        let mache_dir = home.join(".mache");
        let well_known = mache_dir.join("default.sock");
        let _ = std::fs::create_dir_all(&mache_dir);
        let _ = std::fs::remove_file(&well_known);
        if std::os::unix::fs::symlink(&sock_path, &well_known).is_ok() {
            log::info!("symlinked {} → {}", well_known.display(), sock_path.display());
        }
    }

    sock_path
}

async fn listener_loop(ctx: Arc<DaemonContext>, sock_path: PathBuf) {
    let listener = match UnixListener::bind(&sock_path) {
        Ok(l) => l,
        Err(e) => {
            log::error!("UDS bind failed on {}: {e}", sock_path.display());
            return;
        }
    };
    log::info!("UDS control socket listening on {}", sock_path.display());

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(_) => continue,
        };
        let ctx = ctx.clone();
        tokio::spawn(handle_connection(ctx, stream));
    }
}

async fn handle_connection(ctx: Arc<DaemonContext>, stream: tokio::net::UnixStream) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let mut conn_state = ConnectionState::new(ctx.router.clone());

    while let Ok(Some(line)) = lines.next_line().await {
        let resp = dispatch_op(&ctx, &mut conn_state, &line).await;
        let mut out = resp;
        out.push('\n');
        if writer.write_all(out.as_bytes()).await.is_err() {
            break;
        }
    }

    conn_state.cleanup().await;
}

async fn dispatch_op(
    ctx: &DaemonContext,
    conn: &mut ConnectionState,
    line: &str,
) -> String {
    let req: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return format!(r#"{{"error":"invalid JSON: {e}"}}"#),
    };

    let op = match req.get("op").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return r#"{"error":"missing 'op' field"}"#.to_string(),
    };

    // 1. Event ops
    match op {
        "subscribe" => return conn.handle_subscribe(&req).await,
        "unsubscribe" => return conn.handle_unsubscribe(&req).await,
        "emit" => return conn.handle_emit(&req).await,
        _ => {}
    }

    // 2. Base ops
    if let Some(resp) = ops::handle_base_op(ctx, op, &req) {
        // Emit event for state-changing ops
        if matches!(op, "load" | "reparse" | "flush") {
            let gen = leyline_core::Controller::open_or_create(&ctx.ctrl_path)
                .map(|c| c.generation())
                .unwrap_or(0);
            ctx.router.emitter().emit(
                &format!("daemon.{op}"),
                "leyline",
                serde_json::json!({"generation": gen}),
            );
        }
        return resp;
    }

    // 3. Extension: async first, then sync
    if let Some(fut) = ctx.ext.handle_op_async(op, &req) {
        return fut.await;
    }
    if let Some(resp) = ctx.ext.handle_op(op, &req) {
        return resp;
    }

    // 4. Unknown
    format!(r#"{{"error":"unknown op: {op}"}}"#)
}
```

Add `dirs = "5"` to cli-lib Cargo.toml for `dirs::home_dir()`.

- [ ] **Step 2: Add integration test**

Add to `rs/ll-open/cli-lib/tests/integration.rs`:

```rust
#[tokio::test]
async fn test_daemon_socket_status_op() {
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;
    use leyline_cli_lib::daemon::{DaemonContext, EventRouter, NoExt};

    let dir = tempfile::TempDir::new().unwrap();
    let arena_path = dir.path().join("test.arena");
    let ctrl_path = dir.path().join("test.ctrl");
    let sock_path = dir.path().join("test.sock");

    // Set up arena + controller
    let _mmap = leyline_core::create_arena(&arena_path, 2 * 1024 * 1024).unwrap();
    let mut ctrl = leyline_core::Controller::open_or_create(&ctrl_path).unwrap();
    ctrl.set_arena(
        &arena_path.to_string_lossy(),
        2 * 1024 * 1024,
        0,
    ).unwrap();

    let router = leyline_cli_lib::daemon::EventRouter::new(100);
    let ctx = Arc::new(DaemonContext {
        ctrl_path: ctrl_path.clone(),
        ext: Arc::new(NoExt),
        router,
    });

    // Spawn the socket
    leyline_cli_lib::daemon::socket::spawn(ctx, sock_path.clone());

    // Give the listener time to bind
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Connect and send status op
    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    writer.write_all(b"{\"op\":\"status\"}\n").await.unwrap();

    let resp = lines.next_line().await.unwrap().unwrap();
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["generation"], 0);
    assert_eq!(v["arena_size"], 2 * 1024 * 1024);
}

#[tokio::test]
async fn test_daemon_ext_dispatches_to_extension() {
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;
    use leyline_cli_lib::daemon::{DaemonContext, DaemonExt, EventRouter};

    struct TestExt;
    impl DaemonExt for TestExt {
        fn handle_op(&self, op: &str, _req: &serde_json::Value) -> Option<String> {
            if op == "custom_op" {
                Some(r#"{"ok":true,"custom":true}"#.to_string())
            } else {
                None
            }
        }
    }

    let dir = tempfile::TempDir::new().unwrap();
    let arena_path = dir.path().join("test.arena");
    let ctrl_path = dir.path().join("test.ctrl");
    let sock_path = dir.path().join("test.sock");

    let _mmap = leyline_core::create_arena(&arena_path, 2 * 1024 * 1024).unwrap();
    let mut ctrl = leyline_core::Controller::open_or_create(&ctrl_path).unwrap();
    ctrl.set_arena(&arena_path.to_string_lossy(), 2 * 1024 * 1024, 0).unwrap();

    let router = leyline_cli_lib::daemon::EventRouter::new(100);
    let ctx = Arc::new(DaemonContext {
        ctrl_path: ctrl_path.clone(),
        ext: Arc::new(TestExt),
        router,
    });

    leyline_cli_lib::daemon::socket::spawn(ctx, sock_path.clone());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    // Custom op dispatches to extension
    writer.write_all(b"{\"op\":\"custom_op\"}\n").await.unwrap();
    let resp = lines.next_line().await.unwrap().unwrap();
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["custom"].as_bool().unwrap());

    // Unknown op returns error
    writer.write_all(b"{\"op\":\"nonexistent\"}\n").await.unwrap();
    let resp = lines.next_line().await.unwrap().unwrap();
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["error"].as_str().unwrap().contains("unknown op"));
}
```

- [ ] **Step 3: Build and test**

Run: `cd /Users/jamesgardner/remotes/art/ley-line-open/rs && cargo test -p leyline-cli-lib`

Expected: All tests pass including the two new socket tests.

- [ ] **Step 4: Commit**

```bash
git add rs/ll-open/cli-lib/src/daemon/socket.rs rs/ll-open/cli-lib/tests/integration.rs rs/ll-open/cli-lib/Cargo.toml
git commit -m "feat(daemon): UDS socket listener with base + extension dispatch"
```

---

### Task 5: Daemon CLI subcommand

**Files:**
- Create: `rs/ll-open/cli-lib/src/cmd_daemon.rs`
- Modify: `rs/ll-open/cli-lib/src/lib.rs`

- [ ] **Step 1: Create `cmd_daemon.rs`**

```rust
//! Daemon command — arena + mount + UDS socket + event router.
//!
//! This is the open edition daemon. ley-line (private) extends it by
//! passing a `DaemonExt` implementation to `run_daemon()`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use crate::cmd_serve;
use crate::daemon::{DaemonContext, DaemonExt, EventRouter, NoExt};

/// Run the daemon with the default (no-op) extension.
///
/// Called by the open `leyline daemon` subcommand.
pub async fn cmd_daemon(
    arena: &Path,
    arena_size_mib: u64,
    control: Option<&Path>,
    mount: &Path,
    backend: &str,
    nfs_port: u16,
    language: Option<&str>,
    timeout: Option<&str>,
) -> Result<()> {
    run_daemon(
        arena,
        arena_size_mib,
        control,
        mount,
        backend,
        nfs_port,
        language,
        timeout,
        Arc::new(NoExt),
    )
    .await
}

/// Run the daemon with a custom extension.
///
/// This is the entry point for ley-line (private) to pass its DaemonExt impl.
pub async fn run_daemon(
    arena: &Path,
    arena_size_mib: u64,
    control: Option<&Path>,
    mount: &Path,
    backend: &str,
    nfs_port: u16,
    language: Option<&str>,
    timeout: Option<&str>,
    ext: Arc<dyn DaemonExt>,
) -> Result<()> {
    let arena_bytes = arena_size_mib * 1024 * 1024;
    let ctrl_path = cmd_serve::setup_arena(arena, arena_bytes, control)?;

    // Start event router
    let router = EventRouter::new(10_000);

    // Build daemon context
    let ctx = Arc::new(DaemonContext {
        ctrl_path: ctrl_path.clone(),
        ext,
        router: router.clone(),
    });

    // Start UDS socket
    let sock_path = ctrl_path.with_extension("sock");
    crate::daemon::socket::spawn(ctx.clone(), sock_path.clone());
    eprintln!("UDS socket: {}", sock_path.display());

    // Mount (reuse serve's mount logic)
    let graph = leyline_fs::graph::HotSwapGraph::new(ctrl_path)?;
    let graph = if let Some(lang_ext) = language {
        let ts_lang = leyline_fs::validate::language_for_extension(lang_ext)
            .with_context(|| format!("unsupported language: {lang_ext}"))?;
        graph.with_validation(Some(ts_lang))
    } else {
        graph.with_writable()
    };
    let graph: Arc<dyn leyline_fs::graph::Graph> = Arc::new(graph);

    std::fs::create_dir_all(mount)?;
    match backend {
        "nfs" => {
            let listen_addr = format!("0.0.0.0:{nfs_port}");
            let (port, _handle) = leyline_fs::nfs::serve_nfs(graph, &listen_addr).await?;
            eprintln!("NFS server on port {port}");
            cmd_serve::mount_nfs_cmd(port, mount)?;
            eprintln!("mounted at {}", mount.display());
        }
        "fuse" => {
            let _session = leyline_fs::fuse::mount_fuse(graph, mount)?;
            eprintln!("FUSE mounted at {}", mount.display());
        }
        other => anyhow::bail!("unknown backend: {other}"),
    }

    eprintln!("daemon ready — press Ctrl+C to stop");
    cmd_serve::wait_for_shutdown(timeout).await?;

    // Cleanup socket
    let _ = std::fs::remove_file(&sock_path);
    Ok(())
}
```

Note: `cmd_serve::mount_nfs_cmd` and `cmd_serve::wait_for_shutdown` need to be made `pub`. Update their visibility in `cmd_serve.rs`:

```rust
pub fn mount_nfs_cmd(port: u16, mountpoint: &Path) -> Result<()> { ... }
pub async fn wait_for_shutdown(timeout: Option<&str>) -> Result<()> { ... }
```

Also add `use anyhow::Context;` to the import block.

- [ ] **Step 2: Add `Daemon` variant to `Commands` enum**

Add to `lib.rs`:

```rust
pub mod cmd_daemon;
```

Add variant:

```rust
    /// Run the daemon: arena + mount + UDS socket for coordination.
    Daemon {
        /// Path to the arena file.
        #[arg(long, default_value = "./leyline.arena")]
        arena: PathBuf,

        /// Arena size in MiB.
        #[arg(long, default_value_t = 64)]
        arena_size_mib: u64,

        /// Path to the controller (.ctrl) file.
        #[arg(long)]
        control: Option<PathBuf>,

        /// Directory to mount the filesystem at.
        #[arg(long)]
        mount: PathBuf,

        /// Filesystem backend: "nfs" or "fuse".
        #[arg(long, default_value_t = cmd_serve::default_backend())]
        backend: String,

        /// NFS listen port (0 = auto-assign).
        #[arg(long, default_value_t = 0)]
        nfs_port: u16,

        /// Default language for validation.
        #[arg(long)]
        language: Option<String>,

        /// Timeout before automatic shutdown.
        #[arg(long)]
        timeout: Option<String>,
    },
```

Add dispatch in `run()`:

```rust
        Commands::Daemon {
            arena,
            arena_size_mib,
            control,
            mount,
            backend,
            nfs_port,
            language,
            timeout,
        } => cmd_daemon::cmd_daemon(
            &arena,
            arena_size_mib,
            control.as_deref(),
            &mount,
            &backend,
            nfs_port,
            language.as_deref(),
            timeout.as_deref(),
        ).await,
```

- [ ] **Step 3: Add integration test**

Add to `rs/ll-open/cli-lib/tests/integration.rs`:

```rust
#[test]
fn test_daemon_variant_in_help() {
    use clap::CommandFactory;
    // Verify the daemon subcommand exists in the CLI
    let cmd = leyline_cli_lib::Commands::augment_subcommands(clap::Command::new("test"));
    let sub_names: Vec<&str> = cmd
        .get_subcommands()
        .map(|s| s.get_name())
        .collect();
    assert!(sub_names.contains(&"daemon"), "daemon subcommand should exist: {sub_names:?}");
}
```

- [ ] **Step 4: Build and test**

Run: `cd /Users/jamesgardner/remotes/art/ley-line-open/rs && cargo build -p leyline-cli-lib && cargo test -p leyline-cli-lib`

Expected: Compiles, all tests pass.

- [ ] **Step 5: Verify help output**

Run: `cargo run -p leyline-cli -- --help`

Expected: Shows `daemon` alongside parse, splice, serve, load, inspect, lsp.

- [ ] **Step 6: Commit**

```bash
git add rs/ll-open/cli-lib/src/cmd_daemon.rs rs/ll-open/cli-lib/src/cmd_serve.rs rs/ll-open/cli-lib/src/lib.rs
git commit -m "feat(cli): add daemon subcommand — arena + mount + UDS socket"
```

---

### Task 6: Final verification and cleanup

**Files:**
- Modify: `rs/Cargo.lock`

- [ ] **Step 1: Full workspace build and test**

Run: `cd /Users/jamesgardner/remotes/art/ley-line-open/rs && cargo build --workspace && cargo test --workspace`

Expected: All crates compile, all tests pass.

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace -- -D warnings`

Expected: No warnings.

- [ ] **Step 3: Verify help output**

Run: `cargo run -p leyline-cli -- --help`

Expected:
```
Commands:
  parse    ...
  load     ...
  inspect  ...
  splice   ...
  lsp      ...
  serve    ...
  daemon   ...
  help     ...
```

- [ ] **Step 4: Verify daemon flattenable**

The existing `test_commands_flattenable` test should already cover this since `Daemon` is a new variant in `Commands`. Run: `cargo test -p leyline-cli-lib test_commands_flattenable`

Expected: PASS.

- [ ] **Step 5: Commit Cargo.lock**

```bash
git add rs/Cargo.lock
git commit -m "chore: update Cargo.lock for daemon deps (dirs, base64, serde)"
```

- [ ] **Step 6: Push**

```bash
git push origin main
```
