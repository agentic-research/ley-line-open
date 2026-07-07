//! UDS listener that dispatches ops to the event router, base ops, and extension.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use crate::daemon::DaemonContext;
use crate::daemon::events::ConnectionState;
use crate::daemon::ops;
use crate::daemon::ops::is_state_changing;

/// `rm -f` semantics: remove `path` if present, log a warning when
/// removal fails for any reason other than "file does not exist". Both
/// the stale-socket cleanup and the default.sock symlink-replacement
/// path want exactly this behavior — missing-file is the common case
/// (fresh start), and other errors (permissions, EBUSY, etc) deserve
/// to be surfaced without aborting the daemon's bring-up.
fn remove_file_best_effort(path: &std::path::Path, what: &str) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("could not remove {what} {}: {e}", path.display());
    }
}

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
    // Remove any stale socket left from a previous run.
    remove_file_best_effort(&sock_path, "stale socket");

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
            remove_file_best_effort(&symlink_path, "old default.sock symlink");
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

/// Bound on the writer-task's outbound queue. Op responses and pushed
/// events both serialise through this channel so wire bytes never
/// interleave (one `recv` ⇒ one `write_all` + newline). 64 absorbs a
/// typical subscribe-then-burst pattern; if the client falls behind,
/// the per-subscriber router channel (`SUBSCRIBER_CHANNEL_BUFFER`)
/// fills first and the bus's overflow policy decides drop vs disconnect.
const CONNECTION_WRITE_BUFFER: usize = 64;

/// Heartbeat interval for pushed keepalive events on subscribed UDS
/// connections. Mache's Subscribe goroutine sets a 60s read deadline
/// to detect a SIGKILLed daemon (see
/// `mache/internal/leyline/socket.go::runSubscribeLoop`). An idle
/// daemon that never emits real events was tripping that deadline and
/// mache treated the connection as dead. 30s gives 2× headroom against
/// the 60s deadline. Emit shape: `{"type":"keepalive","ts":<ms>}`.
const SUBSCRIBE_KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Handle a single UDS connection: read line-delimited JSON, dispatch, respond.
///
/// Two background tasks back this loop:
///
///   * **Writer task** — sole owner of the `OwnedWriteHalf`. Drains a
///     bounded `mpsc::channel<String>` and writes one line per `recv`.
///     Routing every outbound line through a single task is what keeps
///     op responses and pushed events from interleaving mid-byte.
///
///   * **Event relay task** — spawned after a successful `subscribe` op.
///     Pulls events from the per-subscriber `mpsc::Receiver` stashed on
///     `ConnectionState`, serialises each as a single JSON line, and
///     forwards it through the writer task's channel. A resubscribe
///     drops the prior `Subscriber.tx` in the router (see
///     `handle_subscribe`'s `remove_subscriber` path), which closes the
///     old relay's `recv` and lets the relay exit before the new one
///     starts pumping.
///
/// Without the relay, pushed events accumulated in the per-subscriber
/// channel and never reached the wire — see ley-line-open-5caa59 and
/// `tests/event_push_blackbox_test.rs` for the regression guard.
async fn handle_connection(ctx: Arc<DaemonContext>, stream: tokio::net::UnixStream) {
    let (reader, writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let mut conn_state = ConnectionState::new(ctx.router.clone());

    let (write_tx, mut write_rx) = tokio::sync::mpsc::channel::<String>(CONNECTION_WRITE_BUFFER);

    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(body) = write_rx.recv().await {
            if let Err(e) = writer.write_all(body.as_bytes()).await {
                log::trace!("UDS client gone (mid-body): {e}");
                break;
            }
            if let Err(e) = writer.write_all(b"\n").await {
                log::trace!("UDS client gone (newline): {e}");
                break;
            }
        }
    });

    // `send` failures here only happen if the writer task has already
    // exited (client gone). Surface that to the caller so the read loop
    // can break instead of looping while the channel back-pressures.
    async fn enqueue(tx: &tokio::sync::mpsc::Sender<String>, body: String) -> bool {
        tx.send(body).await.is_ok()
    }

    'read: while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let req: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let err = json!({"error": format!("invalid JSON: {e}")}).to_string();
                if !enqueue(&write_tx, err).await {
                    break 'read;
                }
                continue;
            }
        };

        let op = match req.get("op").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => {
                let err = json!({"error": "missing 'op' field"}).to_string();
                if !enqueue(&write_tx, err).await {
                    break 'read;
                }
                continue;
            }
        };

        let response = dispatch(&ctx, &mut conn_state, &op, &req).await;
        if !enqueue(&write_tx, response).await {
            break 'read;
        }

        // A successful `subscribe` stashes a fresh `event_rx` on
        // `conn_state`. Move it into a dedicated relay task so live
        // pushed events make it through the writer task. Spawning AFTER
        // the response is enqueued preserves "subscribe-ack before
        // first event" ordering — though the underlying mpsc is FIFO so
        // the contract holds either way.
        //
        // The relay also emits a small `{"type":"keepalive","ts":<ms>}`
        // event every `SUBSCRIBE_KEEPALIVE_INTERVAL` when no real event
        // is pending. Bead surfaced by a mache log: mache's Subscribe
        // goroutine sets a 60s read deadline on the UDS connection to
        // detect a SIGKILLed daemon; an idle daemon that never emits
        // real events was tripping that deadline and mache treated the
        // connection as dead. 30s heartbeat gives 2× headroom against
        // the 60s deadline. Consumers that don't want keepalive events
        // filter by `type == "keepalive"` — mache does this in
        // `runSubscribeLoop` post-v0.5.7.
        if let Some(mut event_rx) = conn_state.take_event_rx() {
            let relay_tx = write_tx.clone();
            tokio::spawn(async move {
                let mut keepalive = tokio::time::interval(SUBSCRIBE_KEEPALIVE_INTERVAL);
                // Skip the initial immediate tick — `tick()` fires
                // once on first call before the interval elapses. We
                // want the first keepalive at t+30s, not t+0s.
                keepalive.tick().await;
                loop {
                    tokio::select! {
                        maybe_event = event_rx.recv() => {
                            let Some(event) = maybe_event else {
                                // Sender dropped (resubscribe or router
                                // shutdown) — relay exits.
                                return;
                            };
                            let body = match serde_json::to_string(&event) {
                                Ok(s) => s,
                                Err(e) => {
                                    log::warn!("event serialize failed (dropped): {e}");
                                    continue;
                                }
                            };
                            if relay_tx.send(body).await.is_err() {
                                return;
                            }
                        }
                        _ = keepalive.tick() => {
                            let ts = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis() as u64)
                                .unwrap_or(0);
                            let body = format!(
                                r#"{{"type":"keepalive","ts":{ts}}}"#,
                            );
                            if relay_tx.send(body).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    }

    // Read side hit EOF or fatal error. Unregister the subscriber so
    // the router stops queueing events into a dead channel, then drop
    // our `write_tx` clone and wait for the writer task to drain.
    conn_state.cleanup().await;
    drop(write_tx);
    let _ = writer_task.await;
}

/// Dispatch an op through the chain:
/// 1. Event ops (subscribe, unsubscribe, emit) -> ConnectionState
/// 2. Base ops (status, flush, load, query, reparse) -> ops::handle_base_op()
/// 3. Extension async -> ctx.ext.handle_op_async()
/// 4. Extension sync -> ctx.ext.handle_op()
/// 5. Unknown -> error
async fn dispatch(
    ctx: &Arc<DaemonContext>,
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

    // 2. Base ops — re-fuse (op, req) into the canonical envelope shape
    // expected by the typed `BaseRequest` decoder. The dispatch chain
    // splits the incoming line into (op, args) for routing; we pass the
    // already-parsed Value straight to `handle_base_op_value` so serde
    // can `from_value` it without re-stringifying (a perf concern raised
    // by Copilot on PR #8). The BaseRequest enum is the single source
    // of truth for accepted shapes after A-3 (b69606).
    let combined = {
        let mut v = req.clone();
        if let serde_json::Value::Object(ref mut m) = v {
            m.insert("op".into(), json!(op));
        } else {
            v = json!({"op": op});
        }
        v
    };
    if let Some(response) = ops::handle_base_op_value(ctx, combined) {
        // Emit event for state-changing ops.
        if is_state_changing(op) {
            let emitter = conn_state.emitter();
            emitter.emit(&format!("daemon.{op}"), "leyline", json!({"op": op}));
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn remove_file_best_effort_succeeds_on_existing_file() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("victim");
        std::fs::write(&p, b"x").unwrap();
        assert!(p.exists());
        remove_file_best_effort(&p, "test");
        assert!(!p.exists(), "file should be removed");
    }

    #[test]
    fn remove_file_best_effort_silent_on_missing() {
        // The whole point: ENOENT is the common case (fresh start) and
        // must not produce a warning. We can't easily assert "no log
        // output" in unit tests, but we can assert the function returns
        // without panicking and leaves no side effects.
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("does-not-exist");
        assert!(!p.exists());
        remove_file_best_effort(&p, "test");
        assert!(!p.exists());
    }

    #[test]
    fn remove_file_best_effort_swallows_other_errors() {
        // If the path is a non-empty directory, std::fs::remove_file
        // returns an error other than NotFound. The helper must NOT
        // propagate or panic — it logs and returns. Verify no panic.
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("sub");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("file"), b"keep").unwrap();
        // remove_file on a non-empty directory fails on most platforms;
        // the helper must swallow that without panicking.
        remove_file_best_effort(&nested, "non-empty dir");
        // The dir is still there because the call was a no-op (errored
        // and was swallowed).
        assert!(nested.exists());
    }
}
