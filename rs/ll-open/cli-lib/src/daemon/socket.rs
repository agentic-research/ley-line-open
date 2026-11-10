//! UDS listener that dispatches ops to the event router, base ops, and extension.

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use crate::daemon::DaemonContext;
use crate::daemon::events::ConnectionState;
use crate::daemon::ops;

/// `rm -f` semantics: remove `path` if present, log a warning when
/// removal fails for any reason other than "file does not exist".
/// Missing-file is the common case (fresh start), and other errors
/// (permissions, EBUSY, etc) deserve to be surfaced without aborting the
/// daemon's bring-up.
fn remove_file_best_effort(path: &std::path::Path, what: &str) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("could not remove {what} {}: {e}", path.display());
    }
}

/// Which well-known instance under `~/.mache` a daemon may be.
///
/// Clients discover a daemon at a well-known socket: mache dials
/// `~/.mache/default.sock`, execution clients `~/.mache/execution.sock`.
/// Each name belongs to one *default instance*: the daemon on
/// `~/.mache/<name>.arena` with `~/.mache/<name>.ctrl`, whose `<ctrl>.sock`
/// IS the well-known socket. Any other daemon is an explicit instance and
/// never creates, replaces or removes a well-known name (bead
/// `ley-line-open-e7459a`): it serves only at its own `<ctrl>.sock`, and its
/// clients are handed that path (mache: `LEYLINE_SOCKET`).
///
/// Daemons once published a symlink at the well-known name pointing at
/// whatever socket they bound. Every daemon that started repointed it, so
/// an explicit instance took the default's clients and left the name
/// dangling when it exited.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SocketDiscovery {
    /// `~/.mache/default.sock` — the substrate daemon mache resolves.
    #[default]
    Substrate,
    /// `~/.mache/execution.sock` — the first-party execution/v1 surface.
    /// Consumers may override the path with `LEYLINE_EXECUTION_SOCKET`.
    Execution,
    /// No well-known instance. For tests, and for embedders that hand the
    /// socket path to their clients directly.
    Unpublished,
}

impl SocketDiscovery {
    /// The instance name this surface's default instance uses under
    /// `~/.mache` (`<name>.arena`, `<name>.ctrl`, `<name>.sock`), or `None`.
    pub fn instance_name(self) -> Option<&'static str> {
        match self {
            Self::Substrate => Some("default"),
            Self::Execution => Some("execution"),
            Self::Unpublished => None,
        }
    }
}

/// `a` and `b` name the same directory entry: equal file names in
/// directories that resolve to the same place. The entries themselves need
/// not exist, and a symlink at either is not followed.
fn same_entry(a: &std::path::Path, b: &std::path::Path) -> bool {
    let dir = |p: &std::path::Path| {
        let parent = p
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        parent
            .canonicalize()
            .unwrap_or_else(|_| parent.to_path_buf())
    };
    a.file_name() == b.file_name() && dir(a) == dir(b)
}

/// Refuse a daemon whose socket (`<ctrl>.sock`) would land on a well-known
/// name it is not the default instance for (bead `ley-line-open-e7459a`).
///
/// `home` is the directory holding `.mache`; `None` (no home) has no
/// well-known names to protect. A daemon whose socket lands on
/// `~/.mache/<name>.sock` must be `discovery`'s own surface and serve
/// `~/.mache/<name>.arena` through `~/.mache/<name>.ctrl`; anything else,
/// such as an explicit `--arena` with the default `--control`, would bind
/// the name and take its clients.
pub fn admit_well_known(
    discovery: SocketDiscovery,
    home: Option<&std::path::Path>,
    arena: &std::path::Path,
    ctrl: &std::path::Path,
) -> anyhow::Result<()> {
    let Some(home) = home else {
        return Ok(());
    };
    let mache = home.join(".mache");
    let sock = ctrl.with_extension("sock");
    for kind in [SocketDiscovery::Substrate, SocketDiscovery::Execution] {
        let name = kind.instance_name().expect("a well-known surface");
        let well_known = mache.join(format!("{name}.sock"));
        if !same_entry(&sock, &well_known) {
            continue;
        }
        let default_arena = mache.join(format!("{name}.arena"));
        let default_ctrl = mache.join(format!("{name}.ctrl"));
        // Both paths, not just the socket: `default.foo` also has
        // `default.sock` as its `.sock`, and would serve that name while
        // publishing to a control block mache never reads.
        if kind == discovery && same_entry(arena, &default_arena) && same_entry(ctrl, &default_ctrl)
        {
            return Ok(());
        }
        anyhow::bail!(
            "control block {} would put this daemon's socket at {}, the {name} instance's \
             discovery socket; only the {name} instance (--arena {} --control {}) serves \
             there. Give this daemon its own --control outside {} and hand its clients \
             that socket.",
            ctrl.display(),
            well_known.display(),
            default_arena.display(),
            default_ctrl.display(),
            mache.display()
        );
    }
    Ok(())
}

/// A bound control socket, removed on drop only while its path still names
/// the socket this daemon bound.
///
/// Compare-then-remove: by the time a daemon exits, another may have bound
/// the same path (mache's `DiscoverOrStart` clears a socket it finds dead and
/// spawns a fresh daemon there). Removing by name would then take the newer
/// daemon's socket away from its clients. The check compares the entry's
/// device and inode with the ones bound. It and the unlink are two
/// syscalls, so a rebind landing between them is still removed; for the
/// default instance that window is closed in practice by the arena lock,
/// which this daemon holds until after the socket is dropped, so no second
/// default instance can be serving yet.
#[derive(Debug)]
pub struct BoundSocket {
    path: PathBuf,
    id: Option<(u64, u64)>,
}

impl BoundSocket {
    /// The socket's path.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Give up removal on drop and return the path.
    pub fn into_path(mut self) -> PathBuf {
        self.id = None;
        std::mem::take(&mut self.path)
    }
}

impl Drop for BoundSocket {
    fn drop(&mut self) {
        let Some(bound) = self.id else { return };
        use std::os::unix::fs::MetadataExt;
        match std::fs::symlink_metadata(&self.path) {
            Ok(meta) if (meta.dev(), meta.ino()) == bound => {
                remove_file_best_effort(&self.path, "daemon socket");
            }
            Ok(_) => log::info!(
                "{} now names another socket; leaving it in place",
                self.path.display()
            ),
            Err(_) => {}
        }
    }
}

/// Bind a listener that is never reachable under umask-derived permissions.
///
/// `bind` applies the process umask to the new socket inode, so a `chmod`
/// afterwards is one syscall too late: for that interval any local uid can
/// connect, and a connection established in the window survives the later
/// tightening.
///
/// Narrowing the umask around the `bind` would fix the socket and break
/// everything else — umask is process-global, so every concurrent file and
/// directory creation in the process inherits it. `0o177` in particular
/// strips owner-execute, so a directory created by another thread during the
/// window comes out untraversable. That is a worse bug than the one being
/// fixed, and the test suite catches it immediately.
///
/// Instead the socket is bound inside a private `0o700` directory, where no
/// other uid can traverse to it at any point, then atomically renamed into
/// place. Renaming a bound unix socket keeps the listener valid: the binding
/// belongs to the inode, not to the directory entry that names it.
///
/// The staged path must fit `sun_path` too, not just the final one: on macOS
/// `sun_path` is 104 bytes and a `HOME` under `TMPDIR` (`/var/folders/…`)
/// leaves little room. The staging name is therefore as short as the final
/// socket's own name allows (`.s<pid>/s`), and a staged path that still does
/// not fit is an error naming both paths, never a panic. Every failure is
/// returned so the daemon's caller sees the reason instead of exit 101, and
/// the staging directory is removed on every path out.
///
/// Returns the listener and the socket's `(device, inode)`, read while it is
/// still in the private directory, so it is certainly the inode bound.
fn bind_owner_only(sock_path: &std::path::Path) -> anyhow::Result<(UnixListener, (u64, u64))> {
    use anyhow::Context;
    let parent = sock_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let staging = parent.join(format!(".s{}", std::process::id()));
    let staged_path = staging.join("s");
    let staged_len = staged_path.as_os_str().len();
    if staged_len >= SUN_PATH_MAX {
        anyhow::bail!(
            "cannot bind the daemon socket: the staging path {} is {staged_len} bytes, \
             over the {}-byte unix socket path limit (final socket {}). Use a shorter \
             --control path or HOME.",
            staged_path.display(),
            SUN_PATH_MAX - 1,
            sock_path.display()
        );
    }
    // A staging directory left by a previous process with this pid would
    // otherwise fail the create below.
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&staging)
        .with_context(|| format!("create private staging directory {}", staging.display()))?;

    let bound = (|| -> anyhow::Result<(UnixListener, (u64, u64))> {
        use std::os::unix::fs::MetadataExt;
        let listener = UnixListener::bind(&staged_path)
            .with_context(|| format!("bind UDS socket at {}", staged_path.display()))?;
        std::fs::set_permissions(&staged_path, std::fs::Permissions::from_mode(0o600))
            .context("restrict UDS socket to owner-only permissions")?;
        let meta = std::fs::symlink_metadata(&staged_path).context("stat the bound UDS socket")?;
        std::fs::rename(&staged_path, sock_path)
            .with_context(|| format!("publish UDS socket at {}", sock_path.display()))?;
        Ok((listener, (meta.dev(), meta.ino())))
    })();
    let _ = std::fs::remove_dir_all(&staging);
    bound
}

/// `sizeof(sockaddr_un.sun_path)`: 104 on macOS and the BSDs, 108 on Linux.
/// A path must be strictly shorter (room for the NUL).
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd"
))]
const SUN_PATH_MAX: usize = 104;
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd"
)))]
const SUN_PATH_MAX: usize = 108;

/// Spawn the UDS socket listener as a background tokio task, and return its
/// path. The socket is left in place when the daemon exits; a daemon that
/// removes its socket on exit uses [`bind`].
///
/// `discovery` names the surface the caller serves. Nothing here publishes
/// under it: a daemon is reachable at a well-known name only by binding that
/// name as its default instance, which [`admit_well_known`] decides before
/// any bind.
pub fn spawn(
    ctx: Arc<DaemonContext>,
    sock_path: PathBuf,
    discovery: SocketDiscovery,
) -> anyhow::Result<PathBuf> {
    let _ = discovery;
    bind(ctx, sock_path).map(BoundSocket::into_path)
}

/// Bind the UDS socket and serve it as a background tokio task.
///
/// 1. Removes any stale socket file at `sock_path`.
/// 2. Binds a `UnixListener` reachable only by its owner.
/// 3. Spawns a tokio task per connection.
/// 4. Each connection reads line-delimited JSON, dispatches, writes response.
///
/// Returns the bound socket, which removes the path on drop while it still
/// names this socket (see [`BoundSocket`]). Fails, rather than panicking,
/// when the socket cannot be bound (see [`bind_owner_only`]).
pub fn bind(ctx: Arc<DaemonContext>, sock_path: PathBuf) -> anyhow::Result<BoundSocket> {
    // Remove any stale socket left from a previous run.
    remove_file_best_effort(&sock_path, "stale socket");

    // Bind synchronously so the path is ready on return. A control socket is
    // a filesystem capability: the path itself grants access to every daemon
    // operation, so it is never reachable under inherited permissions, even
    // briefly. MCP's UDS transport applies the same owner-only boundary.
    let (listener, id) = bind_owner_only(&sock_path)?;

    let bound = BoundSocket {
        path: sock_path,
        id: Some(id),
    };

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

    Ok(bound)
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
/// 2. Base ops (status, load, query, reparse, ...) -> ops::handle_base_op()
/// 3. Extension async -> ctx.ext.handle_op_async()
/// 4. Extension sync -> ctx.ext.handle_op()
/// 5. Unknown -> error
pub(crate) async fn dispatch(
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
    // `handle_base_op_value` emits the op's `daemon.<op>` event itself, when
    // the op changed state, for every transport (bead `ley-line-open-851f00`).
    if let Some(response) = ops::handle_base_op_value(ctx, combined) {
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
pub(crate) mod tests {
    use super::*;
    use parking_lot::{Mutex, RwLock};
    use std::sync::Arc;
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

    /// A daemon context over a fresh arena and WAL live db in `dir`. Needs a
    /// tokio runtime (the event router spawns its dispatch task). Shared with
    /// other daemon modules' tests.
    pub(crate) fn test_context(dir: &std::path::Path) -> Arc<DaemonContext> {
        let arena_path = dir.join("socket.arena");
        let ctrl_path = dir.join("socket.ctrl");
        let _mmap = leyline_core::create_arena(&arena_path, 2 * 1024 * 1024).unwrap();
        let mut ctrl = leyline_core::Controller::open_or_create(&ctrl_path).unwrap();
        ctrl.set_arena(&arena_path.to_string_lossy(), 2 * 1024 * 1024)
            .unwrap();
        let live_db = crate::daemon::db_pool::LiveDb::open_fresh_for_test(
            &ctrl_path.with_extension("live.db"),
        );

        Arc::new(DaemonContext {
            ctrl_path,
            ext: Arc::new(crate::daemon::NoExt),
            router: crate::daemon::EventRouter::new(16),
            live_db,
            enrich_inflight: Arc::new(Mutex::new(std::collections::HashSet::new())),
            source_dir: None,
            lang_filter: None,
            enrichment_passes: vec![],
            state: Arc::new(RwLock::new(crate::daemon::DaemonState::initializing())),
            #[cfg(feature = "vec")]
            vec_index: {
                crate::daemon::vec_index::register_vec();
                Arc::new(crate::daemon::vec_index::VectorIndex::new(4, None).unwrap())
            },
            #[cfg(feature = "vec")]
            embedder: Arc::new(crate::daemon::embed::ZeroEmbedder { dim: 4 }),
            #[cfg(feature = "vec")]
            embed_queue: Arc::new(Mutex::new(std::collections::BinaryHeap::new())),
            #[cfg(feature = "text-search")]
            text_search: Arc::new(leyline_text_search::null::NullEngine::new()),
            sheaf: Arc::new(crate::daemon::sheaf_ops::SheafState::new()),
        })
    }

    #[test]
    fn each_daemon_surface_owns_a_distinct_instance_name() {
        // Two surfaces sharing one name is the defect: the second to bind
        // takes the first one's clients.
        assert_eq!(SocketDiscovery::Substrate.instance_name(), Some("default"));
        assert_eq!(
            SocketDiscovery::Execution.instance_name(),
            Some("execution")
        );
        assert_eq!(SocketDiscovery::Unpublished.instance_name(), None);
    }

    /// The refusal is scoped to `$HOME/.mache`, not to a file name: a
    /// `default.ctrl`/`default.sock` (or `execution.*`) anywhere else is an
    /// ordinary explicit instance. mache's test spawners run daemons on
    /// private temp dirs (`d.ctrl`, `a.ctrl`, `test.ctrl`) and rely on this.
    #[test]
    fn well_known_names_outside_home_mache_are_admitted() {
        let home = TempDir::new().unwrap();
        std::fs::create_dir_all(home.path().join(".mache")).unwrap();
        let spawn_dir = TempDir::new().unwrap();
        let tmp = spawn_dir.path();
        // `HOME` itself, not `HOME/.mache`, is also outside the scope.
        let dirs = [tmp.to_path_buf(), home.path().to_path_buf()];
        for d in [
            SocketDiscovery::Substrate,
            SocketDiscovery::Execution,
            SocketDiscovery::Unpublished,
        ] {
            for dir in &dirs {
                for (arena, ctrl) in [
                    ("x.arena", "default.ctrl"),
                    ("default.arena", "default.ctrl"),
                    ("x.arena", "execution.ctrl"),
                    ("x.arena", "default.sock"),
                    ("d.arena", "d.ctrl"),
                    ("a.arena", "a.ctrl"),
                    ("test.arena", "test.ctrl"),
                ] {
                    admit_well_known(d, Some(home.path()), &dir.join(arena), &dir.join(ctrl))
                        .unwrap_or_else(|e| panic!("{d:?} {arena} {ctrl}: {e:#}"));
                }
            }
        }
    }

    /// Bead `ley-line-open-e7459a`: only a surface's default instance may
    /// bind its well-known socket. Paths are compared as directory entries,
    /// so a HOME reached through a symlink still matches.
    #[test]
    fn only_the_default_instance_is_admitted_to_a_well_known_socket() {
        let base = TempDir::new().unwrap();
        let real = base.path().join("real");
        std::fs::create_dir_all(real.join(".mache")).unwrap();
        let home = base.path().join("home");
        std::os::unix::fs::symlink(&real, &home).unwrap();
        let mache = real.join(".mache");
        let elsewhere = base.path().join("x");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let admit = |d, arena: &std::path::Path, ctrl: &std::path::Path| {
            admit_well_known(d, Some(&home), arena, ctrl)
        };
        use SocketDiscovery::*;

        // Each default instance, its paths spelled through the real dir.
        for (d, name) in [(Substrate, "default"), (Execution, "execution")] {
            admit(
                d,
                &mache.join(format!("{name}.arena")),
                &mache.join(format!("{name}.ctrl")),
            )
            .unwrap_or_else(|e| panic!("{name}: {e:#}"));
        }
        // An explicit instance off the well-known names.
        for d in [Substrate, Execution, Unpublished] {
            admit(d, &elsewhere.join("a.arena"), &elsewhere.join("a.ctrl")).unwrap();
            // The default arena with its own control block binds elsewhere.
            admit(d, &mache.join("default.arena"), &elsewhere.join("a.ctrl")).unwrap();
            // The well-known file names in another directory are not the
            // well-known entries.
            admit(
                d,
                &elsewhere.join("default.arena"),
                &elsewhere.join("default.ctrl"),
            )
            .unwrap();
        }
        // An explicit arena on a well-known control block, and a surface on
        // the other surface's name, are refused.
        let refused = [
            (
                Substrate,
                elsewhere.join("a.arena"),
                mache.join("default.ctrl"),
            ),
            (
                Execution,
                mache.join("default.arena"),
                mache.join("default.ctrl"),
            ),
            (
                Unpublished,
                mache.join("default.arena"),
                mache.join("default.ctrl"),
            ),
            (
                Substrate,
                mache.join("execution.arena"),
                mache.join("execution.ctrl"),
            ),
            // A control block whose `.sock` lands on the name all the same.
            (
                Substrate,
                elsewhere.join("a.arena"),
                mache.join("default.other"),
            ),
            // Even with the default arena (bead `ley-line-open-1d58bc`).
            (
                Substrate,
                mache.join("default.arena"),
                mache.join("default.other"),
            ),
        ];
        for (d, arena, ctrl) in refused {
            let err = admit(d, &arena, &ctrl).unwrap_err();
            assert!(
                format!("{err:#}").contains("discovery socket"),
                "{d:?} {} {}: {err:#}",
                arena.display(),
                ctrl.display()
            );
        }
        // No home: no well-known names to protect.
        admit_well_known(
            Substrate,
            None,
            &elsewhere.join("a"),
            &mache.join("default.ctrl"),
        )
        .unwrap();
    }

    /// Compare-then-remove: a bound socket removes its path on drop only
    /// while the path still names the socket it bound.
    #[tokio::test]
    async fn a_bound_socket_removes_only_its_own_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("c.sock");

        let bound = bind(test_context(dir.path()), path.clone()).unwrap();
        assert_eq!(bound.path(), path);
        drop(bound);
        assert!(std::fs::symlink_metadata(&path).is_err(), "removed on drop");

        let bound = bind(test_context(dir.path()), path.clone()).unwrap();
        std::fs::remove_file(&path).unwrap();
        let newer = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(bound);
        std::os::unix::net::UnixStream::connect(&path).expect("the newer socket stays");
        drop(newer);

        let kept = spawn(
            test_context(dir.path()),
            dir.path().join("k.sock"),
            SocketDiscovery::Unpublished,
        )
        .unwrap();
        assert!(kept.exists(), "spawn leaves its socket in place");
    }

    #[tokio::test]
    async fn spawn_binds_requested_owner_only_socket_before_returning() {
        let dir = TempDir::new().unwrap();
        let socket = dir.path().join("control.sock");
        let returned = spawn(
            test_context(dir.path()),
            socket.clone(),
            SocketDiscovery::Unpublished,
        )
        .unwrap();

        assert_eq!(returned, socket);
        assert!(returned.exists(), "spawn must bind synchronously");
        #[cfg(unix)]
        {
            let mode = std::fs::metadata(&returned).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "control socket must be owner-only");
        }
        tokio::net::UnixStream::connect(&returned)
            .await
            .expect("bound listener must accept immediately");
    }

    /// A directory nested under `base` so that `<dir>/<name>` is exactly
    /// `total` bytes.
    fn dir_for_socket_len(base: &std::path::Path, name: &str, total: usize) -> PathBuf {
        let fixed = base.as_os_str().len() + 1 + 1 + name.len(); // base / pad / name
        assert!(total > fixed, "temp dir too long for this test");
        let dir = base.join("p".repeat(total - fixed));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Reported by mache's release parity run: with HOME under macOS TMPDIR
    /// the final socket fit `sun_path` but the old staging path
    /// (`.leyline-sock-stage-<pid>/socket`, 27 bytes longer) did not, and the
    /// daemon panicked with exit 101. A socket whose final path fits must
    /// bind.
    #[tokio::test]
    async fn spawn_binds_when_the_final_path_fits_near_the_limit() {
        let base = TempDir::new().unwrap();
        let name = "control.sock";
        let dir = dir_for_socket_len(base.path(), name, SUN_PATH_MAX - 4);
        let socket = dir.join(name);
        let returned = spawn(
            test_context(base.path()),
            socket.clone(),
            SocketDiscovery::Unpublished,
        )
        .expect("a socket path under the limit must bind");
        assert_eq!(returned, socket);
        tokio::net::UnixStream::connect(&returned).await.unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".s"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "the staging directory must be removed"
        );
    }

    /// A path that cannot fit is an error naming the limit, not a panic, and
    /// leaves no staging directory behind.
    #[tokio::test]
    async fn spawn_reports_an_over_long_socket_path_instead_of_panicking() {
        let base = TempDir::new().unwrap();
        let name = "control.sock";
        let dir = dir_for_socket_len(base.path(), name, SUN_PATH_MAX + 20);
        let err = spawn(
            test_context(base.path()),
            dir.join(name),
            SocketDiscovery::Unpublished,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unix socket path limit"), "{err}");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "nothing left behind"
        );
    }
}
