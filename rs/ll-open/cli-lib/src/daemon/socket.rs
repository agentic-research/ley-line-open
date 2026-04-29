//! UDS listener that dispatches ops to the event router, base ops, and extension.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use crate::daemon::events::ConnectionState;
use crate::daemon::ops;
use crate::daemon::DaemonContext;
use crate::daemon::ops::is_state_changing;

/// Spawn the UDS socket listener as a background tokio task.
///
/// 1. Removes any stale socket file at `sock_path`.
/// 2. Binds a `UnixListener`.
/// 3. Symlinks to `~/.mache/default.sock` for auto-discovery.
/// 4. Spawns a tokio task per connection.
/// 5. Each connection reads line-delimited JSON, dispatches, writes response.
///
/// Returns the socket path. The listener runs in the background.
pub fn spawn(ctx: Arc<DaemonContext>, sock_path: PathBuf) -> PathBuf {
    // Remove any stale socket left from a previous run. Missing-file is the
    // common case (fresh start), so swallow ENOENT silently and only warn on
    // unexpected errors (permissions, EBUSY, etc).
    if let Err(e) = std::fs::remove_file(&sock_path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!(
            "could not remove stale socket {}: {e}",
            sock_path.display(),
        );
    }

    // Bind the listener synchronously so the path is ready on return.
    let listener = UnixListener::bind(&sock_path).expect("bind UDS socket");

    // Symlink to ~/.mache/default.sock for auto-discovery.
    // Skip if sock_path is already at the default location (avoids self-referencing symlink).
    if let Some(home) = dirs::home_dir() {
        let mache_dir = home.join(".mache");
        let symlink_path = mache_dir.join("default.sock");
        if sock_path != symlink_path {
            // Each step is best-effort — a daemon that can't auto-symlink
            // is still functional, just not discoverable via the default
            // path. Log so a broken mache discovery is debuggable.
            if let Err(e) = std::fs::create_dir_all(&mache_dir) {
                log::warn!(
                    "could not create {} for socket auto-discovery: {e}",
                    mache_dir.display(),
                );
            }
            if let Err(e) = std::fs::remove_file(&symlink_path)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                log::warn!(
                    "could not remove old default.sock symlink: {e}",
                );
            }
            if let Err(e) = std::os::unix::fs::symlink(&sock_path, &symlink_path) {
                log::warn!(
                    "could not create default.sock symlink at {}: {e}",
                    symlink_path.display(),
                );
            }
        }
    }

    let path = sock_path.clone();

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let ctx = ctx.clone();
                    tokio::spawn(async move {
                        handle_connection(ctx, stream).await;
                    });
                }
                Err(e) => {
                    log::error!("UDS accept error: {e}");
                }
            }
        }
    });

    path
}

/// Handle a single UDS connection: read line-delimited JSON, dispatch, respond.
async fn handle_connection(ctx: Arc<DaemonContext>, stream: tokio::net::UnixStream) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let mut conn_state = ConnectionState::new(ctx.router.clone());

    /// Write a line back to the client, log::trace! on disconnect. Returns
    /// false if the write failed so the caller can break out of the loop.
    async fn write_line(
        writer: &mut tokio::net::unix::OwnedWriteHalf,
        body: &str,
    ) -> bool {
        if let Err(e) = writer.write_all(body.as_bytes()).await {
            log::trace!("UDS client gone (mid-body): {e}");
            return false;
        }
        if let Err(e) = writer.write_all(b"\n").await {
            log::trace!("UDS client gone (newline): {e}");
            return false;
        }
        true
    }

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let req: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let err = json!({"error": format!("invalid JSON: {e}")}).to_string();
                if !write_line(&mut writer, &err).await { break; }
                continue;
            }
        };

        let op = match req.get("op").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => {
                let err = json!({"error": "missing 'op' field"}).to_string();
                if !write_line(&mut writer, &err).await { break; }
                continue;
            }
        };

        let response = dispatch(&ctx, &mut conn_state, &op, &req).await;
        if !write_line(&mut writer, &response).await { break; }
    }

    // Clean up subscriptions on disconnect.
    conn_state.cleanup().await;
}

/// Dispatch an op through the chain:
/// 1. Event ops (subscribe, unsubscribe, emit) -> ConnectionState
/// 2. Base ops (status, flush, load, query, reparse) -> ops::handle_base_op()
/// 3. Extension async -> ctx.ext.handle_op_async()
/// 4. Extension sync -> ctx.ext.handle_op()
/// 5. Unknown -> error
async fn dispatch(
    ctx: &DaemonContext,
    conn_state: &mut ConnectionState,
    op: &str,
    req: &serde_json::Value,
) -> String {
    // 1. Event ops
    match op {
        "subscribe" => return conn_state.handle_subscribe(req).await,
        "unsubscribe" => return conn_state.handle_unsubscribe(req).await,
        "emit" => return conn_state.handle_emit(req).await,
        _ => {}
    }

    // 2. Base ops
    if let Some(response) = ops::handle_base_op(ctx, op, req) {
        // Emit event for state-changing ops.
        if is_state_changing(op) {
            let emitter = conn_state.emitter();
            emitter.emit(
                &format!("daemon.{op}"),
                "leyline",
                json!({"op": op}),
            );
        }
        return response;
    }

    // 3. Extension async
    if let Some(fut) = ctx.ext.handle_op_async(op, req) {
        return fut.await;
    }

    // 4. Extension sync
    if let Some(response) = ctx.ext.handle_op(op, req) {
        return response;
    }

    // 5. Unknown op
    json!({"error": format!("unknown op: {op}")}).to_string()
}
