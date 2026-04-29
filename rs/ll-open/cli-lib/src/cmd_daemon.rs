//! Daemon command — arena + mount + UDS socket for coordination.
//!
//! Two entry points:
//! - `cmd_daemon()` — open edition, uses `NoExt` (no private extensions).
//! - `run_daemon()` — generic entry point that accepts an `Arc<dyn DaemonExt>`,
//!   called by ley-line (private) with its own extension.

use std::path::Path;
use std::process::Child;
use std::sync::Arc;

use anyhow::{Context, Result};
#[cfg(feature = "mount")]
use leyline_fs::graph::HotSwapGraph;

use crate::cmd_serve;
use crate::daemon::{
    DaemonContext, DaemonExt, DaemonPhase, DaemonState, EventRouter, NoExt,
};

/// Open edition entry point — runs the daemon with no private extensions.
#[allow(clippy::too_many_arguments)]
pub async fn cmd_daemon(
    arena: &Path,
    arena_size_mib: u64,
    control: Option<&Path>,
    mount: Option<&Path>,
    backend: &str,
    nfs_port: u16,
    language: Option<&str>,
    timeout: Option<&str>,
    source: Option<&Path>,
    mcp_port: Option<u16>,
) -> Result<()> {
    let ext: Arc<dyn DaemonExt> = Arc::new(NoExt);
    run_daemon(
        arena,
        arena_size_mib,
        control,
        mount,
        backend,
        nfs_port,
        language,
        timeout,
        ext,
        source,
        mcp_port,
    )
    .await
}

/// Generic daemon entry point — ley-line (private) calls this with its own extension.
///
/// Lifecycle:
/// 1. Arena setup (create/open arena + controller)
/// 2. If `--source`: parse source dir → load .db into arena
/// 3. Event router created
/// 4. `ext.on_init(emitter)` — extension initializes state
/// 5. UDS socket spawned (base ops + extension ops)
/// 6. If `--mount`: mount via NFS/FUSE (omit for headless mode)
/// 7. `ext.on_post_mount(ctrl_path, router)` — extension spawns background tasks
/// 8. If mache on PATH: spawn `mache serve --control <ctrl>` as child
/// 9. Wait for shutdown (Ctrl+C or timeout)
/// 10. Cleanup (kill mache child, remove socket file)
#[allow(clippy::too_many_arguments)]
pub async fn run_daemon(
    arena: &Path,
    arena_size_mib: u64,
    control: Option<&Path>,
    mount: Option<&Path>,
    backend: &str,
    nfs_port: u16,
    language: Option<&str>,
    timeout: Option<&str>,
    ext: Arc<dyn DaemonExt>,
    source: Option<&Path>,
    mcp_port: Option<u16>,
) -> Result<()> {
    // 1. Arena setup.
    let arena_bytes = arena_size_mib * 1024 * 1024;
    let ctrl_path = cmd_serve::setup_arena(arena, arena_bytes, control)?;

    // Lifecycle state — starts as Initializing, transitions through Parsing /
    // Enriching / Ready / Error. Shared with op_status and background tasks.
    let state = Arc::new(std::sync::RwLock::new(DaemonState::initializing()));

    // 2. Initialize the living database.
    //
    // Warm start: if the arena has a valid snapshot, deserialize it into a
    // writable :memory: connection. This recovers state across crashes.
    // Cold start: fresh :memory: connection + parse from --source.
    let live_conn = match init_living_db(&ctrl_path, source, language) {
        Ok(conn) => conn,
        Err(e) => {
            state.write().unwrap().phase = DaemonPhase::Error(format!("init failed: {e:#}"));
            return Err(e);
        }
    };

    // 3. Snapshot living db into arena (initial snapshot for mache/remote consumers).
    snapshot_to_arena(&live_conn, &ctrl_path)?;

    // Capture initial HEAD if --source is set.
    if let Some(src) = source
        && let Some(sha) = git_head(src)
    {
        state.write().unwrap().head_sha = Some(sha);
    }

    // 4. Event router.
    let router = EventRouter::new(10_000);

    // 5. Extension init.
    ext.on_init(router.emitter());

    // Resolve the embedder and sidecar vector index up front: the embedder
    // is provided by the extension (or defaults to ZeroEmbedder), and the
    // index is sized to match its dimensions.
    #[cfg(feature = "vec")]
    let embedder: Arc<dyn crate::daemon::embed::Embedder> = ext
        .embedder()
        .unwrap_or_else(|| Arc::new(crate::daemon::embed::ZeroEmbedder { dim: 384 }));
    #[cfg(feature = "vec")]
    let vec_index = {
        crate::daemon::vec_index::register_vec();
        Arc::new(
            crate::daemon::vec_index::VectorIndex::new(embedder.dimensions(), None)
                .context("create sidecar VectorIndex")?,
        )
    };
    #[cfg(feature = "vec")]
    let embed_queue: crate::daemon::embed::EmbedQueue =
        Arc::new(std::sync::Mutex::new(std::collections::BinaryHeap::new()));

    // 6. Build context + spawn UDS socket.
    let ctx = Arc::new(DaemonContext {
        ctrl_path: ctrl_path.clone(),
        ext: ext.clone(),
        router: router.clone(),
        live_db: std::sync::Mutex::new(live_conn),
        source_dir: source.map(|p| p.to_path_buf()),
        lang_filter: language.map(|s| s.to_string()),
        enrichment_passes: {
            let mut passes: Vec<Box<dyn crate::daemon::enrichment::EnrichmentPass>> = vec![
                Box::new(crate::daemon::enrichment::TreeSitterPass),
            ];
            #[cfg(feature = "lsp")]
            passes.push(Box::new(crate::daemon::lsp_pass::LspEnrichmentPass));
            #[cfg(feature = "vec")]
            passes.push(Box::new(crate::daemon::embed::EmbeddingPass::new(
                vec_index.clone(),
                embedder.clone(),
            )));
            // Extension passes go last; if any have the same name as a
            // base pass, they replace it (extensions win).
            for ext_pass in ext.enrichment_passes() {
                let name = ext_pass.name().to_string();
                if let Some(idx) = passes.iter().position(|p| p.name() == name) {
                    passes[idx] = ext_pass;
                } else {
                    passes.push(ext_pass);
                }
            }
            passes
        },
        state: state.clone(),
        #[cfg(feature = "vec")]
        vec_index: vec_index.clone(),
        #[cfg(feature = "vec")]
        embedder: embedder.clone(),
        #[cfg(feature = "vec")]
        embed_queue: embed_queue.clone(),
    });

    // Spawn the embed-queue drainer: query ops promote node_ids; this loop
    // drains them in priority order and refreshes the sidecar VectorIndex.
    #[cfg(feature = "vec")]
    crate::daemon::embed::start_drain(ctx.clone());

    // Initial parse (during init_living_db) is done — daemon is ready to serve.
    state.write().unwrap().phase = DaemonPhase::Ready;

    let sock_path = ctrl_path.with_extension("sock");
    crate::daemon::socket::spawn(ctx.clone(), sock_path.clone());
    eprintln!("daemon socket at {}", sock_path.display());

    // Optional MCP HTTP transport — feeds cloister gateway / any MCP client.
    // Same dispatch table as the UDS socket; just an MCP-shaped wrapper.
    let mcp_handle = if let Some(port) = mcp_port {
        match crate::daemon::mcp::spawn(ctx.clone(), port) {
            Ok(h) => Some(h),
            Err(e) => {
                log::error!("MCP HTTP server failed to start on port {port}: {e:#}");
                None
            }
        }
    } else {
        None
    };

    // 7. Mount (optional — omit --mount for headless mode).
    #[cfg(feature = "mount")]
    if let Some(mount_path) = mount {
        let graph = HotSwapGraph::new(ctrl_path.clone())?;
        let graph = if let Some(lang_ext) = language {
            let ts_lang = leyline_fs::validate::language_for_extension(lang_ext)
                .with_context(|| format!("unsupported language: {lang_ext}"))?;
            graph.with_validation(Some(ts_lang))
        } else {
            graph.with_writable()
        };
        let graph: Arc<dyn leyline_fs::graph::Graph> = Arc::new(graph);

        std::fs::create_dir_all(mount_path)
            .with_context(|| format!("create mountpoint {}", mount_path.display()))?;

        match backend {
            "nfs" => {
                let listen_addr = format!("0.0.0.0:{nfs_port}");
                let (port, _handle) = leyline_fs::nfs::serve_nfs(graph, &listen_addr).await?;
                eprintln!("NFS server listening on port {port}");
                cmd_serve::mount_nfs_cmd(port, mount_path)?;
                eprintln!("mounted at {}", mount_path.display());
            }
            "fuse" => {
                let _session = leyline_fs::fuse::mount_fuse(graph, mount_path)?;
                eprintln!("FUSE mounted at {}", mount_path.display());
            }
            other => anyhow::bail!("unknown backend: {other} (expected 'nfs' or 'fuse')"),
        }
    } else {
        eprintln!("headless mode (no mount)");
    }
    #[cfg(not(feature = "mount"))]
    {
        let _ = (mount, backend, nfs_port);
        eprintln!("headless mode (mount features not compiled)");
    }

    // 7. Extension post-mount.
    ext.on_post_mount(&ctrl_path, &router);

    // 8. Auto-spawn mache if on PATH.
    let mut mache_child: Option<Child> = None;
    if let Ok(mache_bin) = which::which("mache") {
        let ctrl_str = ctrl_path.to_string_lossy().to_string();
        eprintln!("spawning mache: {} serve --control {}", mache_bin.display(), ctrl_str);
        match std::process::Command::new(&mache_bin)
            .args(["serve", "--control", &ctrl_str])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .spawn()
        {
            Ok(child) => {
                eprintln!("mache started (pid={})", child.id());
                mache_child = Some(child);
            }
            Err(e) => {
                eprintln!("warn: failed to start mache: {e}");
            }
        }
    } else {
        eprintln!("mache not found on PATH — skipping auto-spawn");
    }

    // 9. Git-aware file watcher — poll git status, reparse on change.
    if let Some(ref source_dir) = ctx.source_dir {
        let watch_ctx = ctx.clone();
        let watch_source = source_dir.clone();
        let watch_emitter = router.emitter();
        tokio::spawn(async move {
            git_watch_loop(watch_ctx, &watch_source, watch_emitter).await;
        });
    }

    // 10. Periodic snapshot timer — debounce-flush living db to arena.
    {
        let snap_ctx = ctx.clone();
        let snap_ctrl = ctrl_path.clone();
        let emitter = router.emitter();
        tokio::spawn(async move {
            use tokio::time::{interval, Duration};
            let mut tick = interval(Duration::from_millis(500));
            loop {
                tick.tick().await;
                let guard = snap_ctx.live_db.lock().unwrap();
                if let Err(e) = snapshot_to_arena(&guard, &snap_ctrl) {
                    log::error!("periodic snapshot failed: {e:#}");
                }
                drop(guard);
                emitter.emit("daemon.snapshot", "leyline", serde_json::json!({}));
            }
        });
    }

    eprintln!("daemon ready — press Ctrl+C to stop");

    // 11. Wait for shutdown.
    cmd_serve::wait_for_shutdown(timeout).await?;

    // 12. Graceful shutdown: final snapshot + cleanup.
    {
        let guard = ctx.live_db.lock().unwrap();
        if let Err(e) = snapshot_to_arena(&guard, &ctrl_path) {
            eprintln!("warn: final snapshot failed: {e:#}");
        } else {
            eprintln!("final snapshot saved to arena");
        }
    }
    if let Some(mut child) = mache_child {
        eprintln!("stopping mache (pid={})...", child.id());
        let _ = child.kill();
        let _ = child.wait();
    }
    if let Some(handle) = mcp_handle {
        handle.abort();
    }
    let _ = std::fs::remove_file(&sock_path);

    Ok(())
}

// ---------------------------------------------------------------------------
// Living database helpers
// ---------------------------------------------------------------------------

/// Initialize the living database.
///
/// **Warm start**: if the arena has a valid snapshot, deserialize it into a
/// writable `:memory:` connection, then incrementally reparse if `--source`.
/// **Cold start**: fresh `:memory:` connection + full parse from `--source`.
fn init_living_db(
    ctrl_path: &Path,
    source: Option<&Path>,
    language: Option<&str>,
) -> Result<rusqlite::Connection> {
    // Try warm start from arena.
    if let Some(conn) = try_warm_start(ctrl_path)? {
        eprintln!("warm start from arena");
        if let Some(source_dir) = source {
            eprintln!("incremental reparse {} ...", source_dir.display());
            let result = crate::cmd_parse::parse_into_conn(&conn, source_dir, language, None)?;
            eprintln!(
                "{} parsed, {} unchanged, {} deleted, {} errors",
                result.parsed, result.unchanged, result.deleted, result.errors,
            );
        }
        return Ok(conn);
    }

    // Cold start: fresh :memory: connection.
    eprintln!("cold start");
    let conn = rusqlite::Connection::open_in_memory()
        .context("open :memory: connection")?;

    if let Some(source_dir) = source {
        eprintln!("parsing {} ...", source_dir.display());
        let result = crate::cmd_parse::parse_into_conn(&conn, source_dir, language, None)?;
        eprintln!(
            "{} parsed, {} unchanged, {} deleted, {} errors",
            result.parsed, result.unchanged, result.deleted, result.errors,
        );
    }

    Ok(conn)
}

/// Try to restore the living db from the arena's active buffer.
/// Returns `None` if the arena doesn't exist or has no valid data.
fn try_warm_start(ctrl_path: &Path) -> Result<Option<rusqlite::Connection>> {
    use std::io::Cursor;

    let ctrl = match leyline_core::Controller::open_or_create(ctrl_path) {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };

    let arena_path = ctrl.arena_path();
    if arena_path.is_empty() {
        return Ok(None);
    }

    let file = match std::fs::File::open(&arena_path) {
        Ok(f) => f,
        Err(_) => return Ok(None),
    };

    let mmap = unsafe { memmap2::Mmap::map(&file)? };
    if mmap.len() < std::mem::size_of::<leyline_core::ArenaHeader>() {
        return Ok(None);
    }

    let header: &leyline_core::ArenaHeader =
        bytemuck::from_bytes(&mmap[..std::mem::size_of::<leyline_core::ArenaHeader>()]);

    let file_size = mmap.len() as u64;
    let offset = match header.active_buffer_offset(file_size) {
        Some(o) => o,
        None => return Ok(None),
    };
    let buf_size = leyline_core::ArenaHeader::buffer_size(file_size);
    let buf = &mmap[offset as usize..(offset + buf_size) as usize];

    // Check if the buffer has any data (not all zeros).
    if buf.iter().take(16).all(|&b| b == 0) {
        return Ok(None);
    }

    // Deserialize as writable :memory: connection.
    let mut conn = rusqlite::Connection::open_in_memory()?;
    conn.deserialize_read_exact(
        rusqlite::DatabaseName::Main,
        Cursor::new(buf),
        buf.len(),
        false, // writable
    )
    .context("warm start: sqlite3_deserialize failed")?;

    let generation = ctrl.generation();
    eprintln!("recovered from arena (generation {generation})");

    Ok(Some(conn))
}

/// Serialize the living db and write it into the arena.
pub fn snapshot_to_arena(
    conn: &rusqlite::Connection,
    ctrl_path: &Path,
) -> Result<()> {
    let db_bytes = conn.serialize(rusqlite::DatabaseName::Main)
        .context("serialize living db")?;

    // Ensure arena is large enough.
    let mut ctrl = leyline_core::Controller::open_or_create(ctrl_path)
        .context("open controller")?;
    let arena_path = ctrl.arena_path();
    let arena_size = ctrl.arena_size();

    let min_arena = leyline_core::ArenaHeader::HEADER_SIZE + db_bytes.len() as u64 * 2 + (1024 * 1024);
    let arena_size = if min_arena > arena_size {
        eprintln!(
            "auto-sizing arena: {}MB → {}MB (db is {}MB)",
            arena_size / (1024 * 1024),
            min_arena / (1024 * 1024),
            db_bytes.len() / (1024 * 1024),
        );
        let _ = ctrl.set_arena(&arena_path, min_arena, ctrl.generation());
        min_arena
    } else {
        arena_size
    };

    let buf_capacity = leyline_core::ArenaHeader::buffer_size(arena_size) as usize;
    if db_bytes.len() > buf_capacity {
        anyhow::bail!(
            "db ({} bytes) exceeds arena buffer capacity ({} bytes)",
            db_bytes.len(),
            buf_capacity,
        );
    }

    let mut mmap = leyline_core::create_arena(Path::new(&arena_path), arena_size)
        .context("open arena file")?;

    leyline_core::write_to_arena(&mut mmap, &db_bytes)
        .context("write to arena")?;

    let new_gen = ctrl.generation() + 1;
    ctrl.set_arena(&arena_path, arena_size, new_gen)
        .context("bump generation")?;

    eprintln!("snapshot to arena (generation {new_gen}, {} bytes)", db_bytes.len());
    Ok(())
}

// ---------------------------------------------------------------------------
// Git-aware file watcher
// ---------------------------------------------------------------------------

/// Poll `git status` to detect source changes, trigger incremental reparse.
///
/// Runs as a background tokio task. On each tick:
/// 1. Run `git status --porcelain` in the source directory
/// 2. Run `git rev-parse HEAD` to detect branch switches
/// 3. If the dirty set or HEAD changed since last check, reparse
/// 4. Snapshot to arena + emit events
async fn git_watch_loop(
    ctx: Arc<DaemonContext>,
    source_dir: &Path,
    emitter: crate::daemon::events::EventEmitter,
) {
    use std::collections::HashSet;
    use tokio::time::{interval, Duration};

    let mut tick = interval(Duration::from_secs(2));
    let mut last_dirty: HashSet<String> = HashSet::new();
    let mut last_head: String = git_head(source_dir).unwrap_or_default();

    eprintln!("git watcher started (polling every 2s)");

    loop {
        tick.tick().await;

        // 1. Check HEAD for branch switches.
        let current_head = git_head(source_dir).unwrap_or_default();
        let head_changed = !current_head.is_empty() && current_head != last_head;

        // 2. Check dirty files.
        let current_dirty = match git_dirty_files(source_dir) {
            Ok(files) => files,
            Err(e) => {
                log::debug!("git status failed: {e:#}");
                continue;
            }
        };

        let dirty_changed = current_dirty != last_dirty;

        if !head_changed && !dirty_changed {
            continue;
        }

        if head_changed {
            eprintln!(
                "HEAD changed: {} → {}",
                &last_head[..7.min(last_head.len())],
                &current_head[..7.min(current_head.len())],
            );
            ctx.state.write().unwrap().head_sha = Some(current_head.clone());
            emitter.emit(
                "daemon.head.changed",
                "leyline",
                serde_json::json!({
                    "old_sha": last_head,
                    "new_sha": current_head,
                }),
            );
            ctx.ext.on_head_changed(&last_head, &current_head);
            last_head = current_head;
        }

        if dirty_changed {
            let new_files: Vec<&String> = current_dirty.difference(&last_dirty).collect();
            if !new_files.is_empty() {
                eprintln!("git: {} file(s) changed", new_files.len());
            }
            let dirty_paths: Vec<String> = current_dirty.iter().cloned().collect();
            emitter.emit(
                "daemon.files.changed",
                "leyline",
                serde_json::json!({ "paths": dirty_paths.clone() }),
            );
            ctx.ext.on_files_changed(&dirty_paths);
            last_dirty = current_dirty;
        }

        // 3. Incremental reparse, scoped to the dirty set so we don't re-stat
        //    the entire source tree on every tick.
        ctx.state.write().unwrap().phase = DaemonPhase::Parsing;
        let lang = ctx.lang_filter.as_deref();
        let dirty_vec: Vec<String> = last_dirty.iter().cloned().collect();
        let scope: Option<&[String]> =
            if dirty_vec.is_empty() { None } else { Some(dirty_vec.as_slice()) };
        let guard = ctx.live_db.lock().unwrap();
        match crate::cmd_parse::parse_into_conn(&guard, source_dir, lang, scope) {
            Ok(result) => {
                if result.parsed > 0 || result.deleted > 0 {
                    eprintln!(
                        "watch: {} parsed, {} unchanged, {} deleted",
                        result.parsed, result.unchanged, result.deleted,
                    );
                    drop(guard);

                    // 4. Snapshot to arena.
                    let guard = ctx.live_db.lock().unwrap();
                    if let Err(e) = snapshot_to_arena(&guard, &ctx.ctrl_path) {
                        log::error!("watch snapshot failed: {e:#}");
                    }
                    drop(guard);

                    // 5. Update state + emit events.
                    {
                        let mut s = ctx.state.write().unwrap();
                        s.last_reparse_at_ms = Some(now_ms());
                        s.phase = DaemonPhase::Ready;
                    }
                    emitter.emit(
                        "daemon.reparse.complete",
                        "leyline",
                        serde_json::json!({
                            "parsed": result.parsed,
                            "deleted": result.deleted,
                            "changed_files": result.changed_files,
                        }),
                    );
                } else {
                    drop(guard);
                    ctx.state.write().unwrap().phase = DaemonPhase::Ready;
                }
            }
            Err(e) => {
                log::error!("watch reparse failed: {e:#}");
                ctx.state.write().unwrap().phase =
                    DaemonPhase::Error(format!("watch reparse failed: {e:#}"));
            }
        }
    }
}

/// Wall-clock millis since UNIX_EPOCH. Used for `last_*_ms` state fields.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Get the current HEAD commit hash.
fn git_head(dir: &Path) -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
}

/// Get the set of dirty files (modified, added, untracked) via git status.
fn git_dirty_files(dir: &Path) -> Result<std::collections::HashSet<String>> {
    let output = std::process::Command::new("git")
        .args(["status", "--porcelain", "-z"])
        .current_dir(dir)
        .output()
        .context("run git status")?;

    if !output.status.success() {
        anyhow::bail!("git status failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    // --porcelain -z: NUL-separated entries, each starts with 2-char status + space + path
    let stdout = String::from_utf8_lossy(&output.stdout);
    let files: std::collections::HashSet<String> = stdout
        .split('\0')
        .filter(|entry| entry.len() > 3)
        .map(|entry| entry[3..].to_string())
        .collect();

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    /// DaemonExt that records every VCS hook invocation.
    struct RecordingExt {
        head_changes: StdMutex<Vec<(String, String)>>,
        file_changes: StdMutex<Vec<Vec<String>>>,
    }

    impl RecordingExt {
        fn new() -> Self {
            Self {
                head_changes: StdMutex::new(Vec::new()),
                file_changes: StdMutex::new(Vec::new()),
            }
        }
    }

    impl DaemonExt for RecordingExt {
        fn on_head_changed(&self, old_sha: &str, new_sha: &str) {
            self.head_changes
                .lock()
                .unwrap()
                .push((old_sha.to_string(), new_sha.to_string()));
        }
        fn on_files_changed(&self, paths: &[String]) {
            self.file_changes.lock().unwrap().push(paths.to_vec());
        }
    }

    /// Run a shell command in a directory, panicking on failure.
    fn sh(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new(args[0])
            .args(&args[1..])
            .current_dir(dir)
            .status()
            .expect("spawn");
        assert!(status.success(), "command failed: {args:?}");
    }

    /// Set up a temp git repo with one committed file, return the path.
    fn fixture_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        sh(dir.path(), &["git", "init", "-q"]);
        sh(dir.path(), &["git", "config", "user.email", "test@example.com"]);
        sh(dir.path(), &["git", "config", "user.name", "test"]);
        sh(dir.path(), &["git", "config", "commit.gpgsign", "false"]);
        std::fs::write(dir.path().join("a.go"), "package m\n\nfunc A() {}\n").unwrap();
        sh(dir.path(), &["git", "add", "."]);
        sh(dir.path(), &["git", "commit", "-q", "-m", "init"]);
        dir
    }

    /// Build a DaemonContext suitable for git_watch_loop testing.
    fn test_ctx(
        ctrl_path: &Path,
        ext: Arc<dyn DaemonExt>,
        source: &Path,
    ) -> Arc<DaemonContext> {
        let _ = leyline_core::create_arena(
            &ctrl_path.with_extension("arena"),
            2 * 1024 * 1024,
        )
        .unwrap();
        let mut ctrl = leyline_core::Controller::open_or_create(ctrl_path).unwrap();
        ctrl.set_arena(
            ctrl_path.with_extension("arena").to_string_lossy().as_ref(),
            2 * 1024 * 1024,
            0,
        )
        .unwrap();
        drop(ctrl);

        let conn = rusqlite::Connection::open_in_memory().unwrap();
        #[cfg(feature = "vec")]
        let vec_index = {
            crate::daemon::vec_index::register_vec();
            Arc::new(crate::daemon::vec_index::VectorIndex::new(4, None).unwrap())
        };
        #[cfg(feature = "vec")]
        let embedder: Arc<dyn crate::daemon::embed::Embedder> =
            Arc::new(crate::daemon::embed::ZeroEmbedder { dim: 4 });
        Arc::new(DaemonContext {
            ctrl_path: ctrl_path.to_path_buf(),
            ext,
            router: crate::daemon::EventRouter::new(64),
            live_db: std::sync::Mutex::new(conn),
            source_dir: Some(source.to_path_buf()),
            lang_filter: Some("go".to_string()),
            enrichment_passes: vec![Box::new(crate::daemon::enrichment::TreeSitterPass)],
            state: Arc::new(std::sync::RwLock::new(DaemonState::initializing())),
            #[cfg(feature = "vec")]
            vec_index,
            #[cfg(feature = "vec")]
            embedder,
            #[cfg(feature = "vec")]
            embed_queue: Arc::new(std::sync::Mutex::new(std::collections::BinaryHeap::new())),
        })
    }

    /// Subscribe to one topic, return the event receiver.
    async fn subscribe_to(
        router: &Arc<crate::daemon::EventRouter>,
        topic: &str,
    ) -> tokio::sync::mpsc::Receiver<crate::daemon::events::Event> {
        let (_id, rx, _replay, _gap) = router
            .subscribe(
                &[topic.to_string()],
                None,
                u64::MAX, // skip replay
                crate::daemon::events::OverflowPolicy::DropOldest,
                64,
            )
            .await;
        rx
    }

    /// `git_watch_loop` ticks every 2s. Modify a file, wait one full tick + a
    /// generous slack, and verify both the typed hook and the event fire.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_invokes_files_changed_hook_and_event() {
        let repo = fixture_repo();
        let dir = TempDir::new().unwrap();
        let ctrl_path = dir.path().join("test.ctrl");

        let ext = Arc::new(RecordingExt::new());
        let ctx = test_ctx(&ctrl_path, ext.clone(), repo.path());

        let mut events_rx = subscribe_to(&ctx.router, "daemon.files.changed").await;
        let emitter = ctx.router.emitter();

        let watch_ctx = ctx.clone();
        let watch_source = repo.path().to_path_buf();
        let task = tokio::spawn(async move {
            git_watch_loop(watch_ctx, &watch_source, emitter).await;
        });

        // Let the watcher tick once with no changes (establishes baseline).
        tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;

        // Now make the file dirty.
        std::fs::write(repo.path().join("a.go"), "package m\n\nfunc A() { /* edit */ }\n")
            .unwrap();

        // Wait up to 5s for the next tick to detect the change.
        let saw_files_event = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            events_rx.recv(),
        )
        .await
        .ok()
        .flatten()
        .is_some();

        task.abort();

        assert!(saw_files_event, "expected daemon.files.changed event");
        let recorded = ext.file_changes.lock().unwrap();
        assert!(
            !recorded.is_empty(),
            "expected on_files_changed to be invoked at least once",
        );
        let last_set = recorded.last().unwrap();
        assert!(
            last_set.iter().any(|p| p == "a.go"),
            "expected dirty set to include a.go, got {last_set:?}",
        );
    }

    /// HEAD changes (e.g. new commit) should fire both the event and the hook.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_invokes_head_changed_hook_and_event() {
        let repo = fixture_repo();
        let initial_head = git_head(repo.path()).unwrap();
        let dir = TempDir::new().unwrap();
        let ctrl_path = dir.path().join("test.ctrl");

        let ext = Arc::new(RecordingExt::new());
        let ctx = test_ctx(&ctrl_path, ext.clone(), repo.path());

        let mut events_rx = subscribe_to(&ctx.router, "daemon.head.changed").await;
        let emitter = ctx.router.emitter();

        let watch_ctx = ctx.clone();
        let watch_source = repo.path().to_path_buf();
        let task = tokio::spawn(async move {
            git_watch_loop(watch_ctx, &watch_source, emitter).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;

        // New commit — bumps HEAD.
        std::fs::write(repo.path().join("b.go"), "package m\n\nfunc B() {}\n").unwrap();
        sh(repo.path(), &["git", "add", "."]);
        sh(repo.path(), &["git", "commit", "-q", "-m", "add b"]);
        let new_head = git_head(repo.path()).unwrap();
        assert_ne!(initial_head, new_head);

        let saw_head_event = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            events_rx.recv(),
        )
        .await
        .ok()
        .flatten()
        .is_some();

        task.abort();

        assert!(saw_head_event, "expected daemon.head.changed event");
        let head_calls = ext.head_changes.lock().unwrap();
        assert!(!head_calls.is_empty(), "expected on_head_changed to fire");
        let (_old, new) = head_calls.last().unwrap();
        assert_eq!(new, &new_head, "hook should report the new HEAD sha");
    }
}
