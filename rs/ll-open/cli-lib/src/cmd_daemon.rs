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
use leyline_core::ContentAddressed;
#[cfg(feature = "mount")]
use leyline_fs::graph::HotSwapGraph;

use crate::cmd_serve;
use crate::daemon::{DaemonContext, DaemonExt, DaemonPhase, DaemonState, EventRouter, NoExt};

// ---------------------------------------------------------------------------
// Tuning constants — extracted from the daemon orchestration so each magic
// value has one named, documented home. Resist inlining literals; if you
// need a different value at runtime, plumb a CLI flag instead.
// ---------------------------------------------------------------------------

/// Capacity of the in-memory event log behind `EventRouter`. Each emit
/// either delivers to a subscriber or lands in the log; old entries roll
/// off when the log fills. 10k is enough headroom for a session of busy
/// edits + reparses without losing recent history.
const EVENT_LOG_CAPACITY: usize = 10_000;

/// Default embedding dimension when no `DaemonExt::embedder()` is
/// provided. Matches MiniLM-L6-v2 / many small open models. Extensions
/// that ship a different model override this implicitly via
/// `Embedder::dimensions()`.
#[cfg(feature = "vec")]
const DEFAULT_EMBEDDING_DIM: usize = 384;

/// How often the periodic snapshot timer fires. Each tick takes the live
/// db lock, serializes, and writes to the arena. 500ms is the
/// crash-recovery window: at most this much data is lost if the process
/// dies between snapshots.
const SNAPSHOT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// How often `git_watch_loop` polls `git status` / `rev-parse HEAD`.
/// 2s is the change-detection window — files edited since the last tick
/// won't be reparsed until the next one.
const GIT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// When the live db outgrows the arena, auto-resize to `db_bytes *
/// ARENA_GROWTH_FACTOR + ARENA_HEADROOM_BYTES` so the next few
/// snapshots don't trigger another resize. Factor must be ≥2 (each
/// arena holds two buffers — one active, one inactive).
const ARENA_GROWTH_FACTOR: u64 = 2;

/// Slack added on top of `db_bytes * ARENA_GROWTH_FACTOR` during arena
/// auto-resize, so a slowly growing db doesn't churn through resizes.
const ARENA_HEADROOM_BYTES: u64 = 1024 * 1024; // 1 MiB

// Compile-time invariants on the tuning constants. Each fails the build
// if a future edit violates the documented constraint — cheaper than a
// runtime test and impossible to skip in CI.
//
// (Clippy correctly notes that runtime assertions on these would be
// constant-folded; const _ asserts are the idiomatic Rust answer.)
const _: () = assert!(
    ARENA_GROWTH_FACTOR >= 2,
    "arena holds 2 buffers (active + inactive); growth factor < 2 \
     can't fit both copies after a resize",
);
const _: () = assert!(
    SNAPSHOT_INTERVAL.as_millis() < GIT_POLL_INTERVAL.as_millis(),
    "crash-recovery window (SNAPSHOT_INTERVAL) must be shorter than the \
     watcher's reparse cadence (GIT_POLL_INTERVAL) so dirty edits get \
     snapshotted before the next watcher tick captures more",
);

/// Open edition entry point — runs the daemon with no private extensions.
#[allow(clippy::too_many_arguments)]
/// Configuration for the daemon lifecycle. Replaces the 14-argument
/// `run_daemon` signature (bead `ley-line-open-ba8294`). All fields are
/// owned so the struct is `'static`-friendly and can be passed through
/// async boundaries without lifetime gymnastics.
///
/// Construct via `DaemonConfig::builder()` or direct struct literal.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Path to the arena file (backing store for the substrate).
    pub arena: std::path::PathBuf,
    /// Arena size hint in MiB. Grown auto per `ARENA_GROWTH_FACTOR`.
    pub arena_size_mib: u64,
    /// Optional control socket path. Auto-derived from arena if `None`.
    pub control: Option<std::path::PathBuf>,
    /// Optional mount point for FUSE/NFS. `None` = headless mode.
    pub mount: Option<std::path::PathBuf>,
    /// Mount backend name (e.g. "fuse", "nfs"). Ignored when `mount = None`.
    pub backend: String,
    /// TCP port for the NFS backend. Ignored when `mount = None` or `backend != "nfs"`.
    pub nfs_port: u16,
    /// Optional language filter for parse (`Some("go")`, `Some("rust")`, etc.).
    pub language: Option<String>,
    /// Optional shutdown timeout (`Some("30s")`, `Some("1h")`, etc.).
    pub timeout: Option<String>,
    /// Optional source directory to parse on startup.
    pub source: Option<std::path::PathBuf>,
    /// Optional MCP HTTP port. `None` disables MCP HTTP.
    pub mcp_port: Option<u16>,
    /// Optional MCP HTTP bind address. Defaults to loopback if `None`.
    pub mcp_bind: Option<std::net::IpAddr>,
    /// Explicit opt-in for non-loopback MCP binding. See bead
    /// `ley-line-open-b7dd03` — required alongside a non-loopback
    /// `mcp_bind` or startup refuses.
    pub mcp_allow_public: bool,
    /// Disables MCP wire authentication. See ADR-0022.
    pub mcp_no_auth: bool,
}

pub async fn cmd_daemon(config: DaemonConfig) -> Result<()> {
    let ext: Arc<dyn DaemonExt> = Arc::new(NoExt);
    run_daemon(config, ext).await
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
pub async fn run_daemon(config: DaemonConfig, ext: Arc<dyn DaemonExt>) -> Result<()> {
    let DaemonConfig {
        arena,
        arena_size_mib,
        control,
        mount,
        backend,
        nfs_port,
        language,
        timeout,
        source,
        mcp_port,
        mcp_bind,
        mcp_allow_public,
        mcp_no_auth,
    } = config;
    let arena = arena.as_path();
    let control = control.as_deref();
    let mount = mount.as_deref();
    let source = source.as_deref();
    let backend = backend.as_str();
    let language = language.as_deref();
    let timeout = timeout.as_deref();
    // Bead `ley-line-open-b7dd03`: gate non-loopback `--mcp-bind` behind
    // an explicit `--mcp-allow-public` flag. The MCP wire now has a
    // shared-secret token gate (ADR-0022 / bead `ley-line-open-b885d1`);
    // making the public bind a deliberate two-flag opt-in still applies
    // because the token alone doesn't defend against off-LAN attackers
    // hammering the listener. Container deployments pass both flags in
    // the image CMD; outside containers, only pass them when you
    // control the firewall.
    //
    // We check BEFORE any arena/db work to fail fast — no point spinning
    // the daemon up if it's about to be rejected.
    if mcp_port.is_some()
        && let Some(addr) = mcp_bind
        && !addr.is_loopback()
        && !mcp_allow_public
    {
        let auth_note = if mcp_no_auth {
            "With `--mcp-no-auth` the listener is unauthenticated — off-loopback bind \
             is immediately exploitable."
        } else {
            "Even with the token gate (ADR-0022) active, off-loopback bind makes the \
             daemon discoverable to every interface on this machine — any probe on \
             LAN/Internet reaches the listener."
        };
        anyhow::bail!(
            "refusing to bind MCP HTTP to non-loopback address {addr} without \
             `--mcp-allow-public`. {auth_note} \
             Pass `--mcp-allow-public` if you mean to do this (containers do; \
             see image.Dockerfile)."
        );
    }

    // 0. Admission control — refuse to start if another daemon already
    // holds this arena. flock-backed advisory lockfile at `<arena>.lock`;
    // OS releases the lock automatically on process exit even if we
    // crash without running Drop. Bind to a local so the lock persists
    // for the daemon's entire runtime. Bead `ley-line-open-0cba88`.
    let _arena_lock = crate::daemon::arena_lock::ArenaLock::try_acquire(arena)
        .context("arena admission control")?;

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
    let router = EventRouter::new(EVENT_LOG_CAPACITY);

    // 5. Extension init.
    ext.on_init(router.emitter());

    // Resolve the embedder and sidecar vector index up front: the embedder
    // is provided by the extension (or defaults to FastEmbedder under
    // `vec`). Set `LLO_EMBEDDER=zero` to force the deterministic
    // ZeroEmbedder for tests / offline use — FastEmbedder downloads a
    // ~22MB ONNX model on first init.
    #[cfg(feature = "vec")]
    let embedder: Arc<dyn crate::daemon::embed::Embedder> = ext.embedder().unwrap_or_else(|| {
        use crate::daemon::embed::{FastEmbedModel, FastEmbedder, ZeroEmbedder};
        let force_zero = std::env::var("LLO_EMBEDDER")
            .map(|v| v.eq_ignore_ascii_case("zero"))
            .unwrap_or(false);
        if force_zero {
            log::info!("LLO_EMBEDDER=zero — using ZeroEmbedder (deterministic, no model download)");
            return Arc::new(ZeroEmbedder {
                dim: DEFAULT_EMBEDDING_DIM,
            });
        }
        match FastEmbedder::new(FastEmbedModel::default()) {
            Ok(fe) => Arc::new(fe),
            Err(e) => {
                log::warn!(
                    "FastEmbedder init failed ({e}); falling back to ZeroEmbedder. \
                     Set LLO_EMBEDDER=zero to silence this and skip the model probe."
                );
                Arc::new(ZeroEmbedder {
                    dim: DEFAULT_EMBEDDING_DIM,
                })
            }
        }
    });
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

    // Text-search engine: extension provides one (real Witchcraft engine,
    // etc.), or fall back to NullEngine — the daemon op surface is wired
    // either way; clients get a structured "no backend" error when no
    // engine is installed, never "unknown op".
    #[cfg(feature = "text-search")]
    let text_search: Arc<dyn leyline_text_search::TextSearchEngine> = ext
        .text_search_engine()
        .unwrap_or_else(|| Arc::new(leyline_text_search::null::NullEngine::new()));

    // 6. Build context + spawn UDS socket.
    //
    // Hoist SheafState creation before the enrichment_passes list so
    // ComplexBuildPass can hold an `Arc<SheafState>` and install its
    // built CellComplex + CoChangeTracker into the shared cache at end
    // of run. Closes bead `ley-line-open-3af437` (sheaf gap 2):
    // previously the pass dropped the derived complex on return,
    // forcing every `op_sheaf_*` consumer query to trigger a rebuild.
    let sheaf = {
        let s = Arc::new(crate::daemon::sheaf_ops::SheafState::new());
        // Wire the event bus so `sheaf_set_topology` / `sheaf_invalidate`
        // emit `sheaf.topology` / `sheaf.invalidate` on subscribers.
        s.set_emitter(router.emitter());
        s
    };

    let ctx = Arc::new(DaemonContext {
        ctrl_path: ctrl_path.clone(),
        ext: ext.clone(),
        router: router.clone(),
        live_db: std::sync::Mutex::new(live_conn),
        enrich_inflight: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        source_dir: source.map(|p| p.to_path_buf()),
        lang_filter: language.map(|s| s.to_string()),
        enrichment_passes: {
            let mut passes: Vec<Box<dyn crate::daemon::enrichment::EnrichmentPass>> =
                vec![Box::new(crate::daemon::enrichment::TreeSitterPass)];
            #[cfg(feature = "lsp")]
            passes.push(Box::new(crate::daemon::lsp_pass::LspEnrichmentPass::new()));
            #[cfg(feature = "vec")]
            passes.push(Box::new(crate::daemon::embed::EmbeddingPass::new(
                vec_index.clone(),
                embedder.clone(),
            )));
            #[cfg(feature = "hdc")]
            passes.push(Box::new(crate::daemon::hdc_enrich::HdcEnrichmentPass));
            // Session observation pass — ADR-0020 §1 Gate 1. Writes `observation` rows.
            passes.push(Box::new(
                crate::daemon::session_observation_pass::SessionObservationPass::new(),
            ));
            // ADR-0020 Gate 2: ComplexBuildPass reads `observation` rows
            // (written by SessionObservationPass) and builds a CellComplex
            // + drives CoChangeTracker. Installs the built complex +
            // tracker into `sheaf.cache` at end of run (bead `3af437`).
            passes.push(Box::new(
                crate::daemon::complex_build_pass::ComplexBuildPass::new(sheaf.clone()),
            ));
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
        #[cfg(feature = "text-search")]
        text_search,
        sheaf,
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
    //
    // ADR-0022 / bead `ley-line-open-b885d1`: gate /mcp behind a
    // shared-secret token. Token is auto-generated at the platform's
    // data dir (Linux: `~/.local/share/leyline/daemon.token`; macOS:
    // `~/Library/Application Support/leyline/daemon.token`) on first
    // boot. `--mcp-no-auth` skips the gate entirely — required for
    // pre-provisioned containers where the token file isn't present at
    // startup, and logged as a warning.
    //
    // Token-load failure is fail-CLOSED: we skip the spawn rather than
    // start an unauthenticated listener. The previous draft logged
    // "refusing to serve /mcp" but still called spawn with `token=None`,
    // which silently opened the gate (Copilot finding on PR #66).
    let mcp_handle = if let Some(port) = mcp_port {
        enum TokenDecision {
            Gated(Arc<String>),
            NoAuth,
            Bail,
        }

        let decision = if mcp_no_auth {
            let exposure_hint = match mcp_bind {
                Some(addr) if !addr.is_loopback() => "off-loopback / potentially network-reachable",
                _ => "any local caller on this machine",
            };
            log::warn!("MCP HTTP auth disabled by --mcp-no-auth — /mcp is open to {exposure_hint}",);
            TokenDecision::NoAuth
        } else {
            match crate::daemon::auth::default_token_path()
                .and_then(|p| crate::daemon::auth::load_or_generate(&p).map(|t| (p, t)))
            {
                Ok((path, tok)) => {
                    eprintln!("MCP HTTP token at {}", path.display());
                    TokenDecision::Gated(Arc::new(tok))
                }
                Err(e) => {
                    log::error!(
                        "failed to load/generate MCP token: {e:#}; refusing to serve /mcp \
                         (pass --mcp-no-auth to opt out of the token gate)",
                    );
                    TokenDecision::Bail
                }
            }
        };

        match decision {
            TokenDecision::Bail => None,
            TokenDecision::Gated(_) | TokenDecision::NoAuth => {
                let token = match &decision {
                    TokenDecision::Gated(t) => Some(t.clone()),
                    _ => None,
                };
                match crate::daemon::mcp::spawn(ctx.clone(), mcp_bind, port, token) {
                    Ok(h) => Some(h),
                    Err(e) => {
                        log::error!("MCP HTTP server failed to start on port {port}: {e:#}");
                        None
                    }
                }
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
        eprintln!(
            "spawning mache: {} serve --control {}",
            mache_bin.display(),
            ctrl_str
        );
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
    //
    // Idle-CPU fix (bead ley-line-open-1a0a2a): skip the snapshot when
    // the living db hasn't changed since the previous successful snapshot.
    // `sqlite3_total_changes()` is O(1) and covers all INSERT/UPDATE/DELETE
    // rows. Fresh daemons idle near zero CPU because the msync-a-big-arena
    // path only fires when writes actually happened. Under load the timer
    // still fires every SNAPSHOT_INTERVAL because writes advance the
    // counter each tick. Long-idle daemons (mache-scale DBs sitting idle
    // for minutes/hours) now stay quiet instead of spending CPU on
    // no-op serialize+msync cycles.
    //
    // The initial snapshot at line 207 (mache/remote-consumer visibility)
    // already fired before the timer loop starts, so a fresh daemon has
    // exactly one snapshot in the arena regardless of whether the timer
    // ever fires.
    {
        let snap_ctx = ctx.clone();
        let snap_ctrl = ctrl_path.clone();
        let emitter = router.emitter();
        tokio::spawn(async move {
            use tokio::time::interval;
            let mut tick = interval(SNAPSHOT_INTERVAL);
            let mut last_snapshot_changes: u64 = read_total_changes(&snap_ctx.live_db).unwrap_or(0);
            loop {
                tick.tick().await;
                // Cheap dirty-check: read `total_changes()` under a
                // brief try_lock. If the counter hasn't advanced since
                // the last successful snapshot, skip the whole
                // serialize/msync path — nothing to persist.
                let current_changes = match read_total_changes(&snap_ctx.live_db) {
                    Some(n) => n,
                    None => continue, // contended; try again next tick
                };
                if current_changes == last_snapshot_changes {
                    continue; // db is clean; no work to do
                }
                // Non-blocking: skip this tick if op_reparse / op_enrich
                // is mid-flight rather than queuing snapshots behind it.
                // 5fea4e — block-the-tokio-worker prevention.
                if try_snapshot_or_log(&snap_ctx.live_db, &snap_ctrl, "periodic snapshot failed") {
                    last_snapshot_changes = current_changes;
                    emitter.emit("daemon.snapshot", "leyline", serde_json::json!({}));
                }
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
            run_initial_parse(&conn, source_dir, language, "incremental reparse")?;
        }
        return Ok(conn);
    }

    // Cold start: fresh :memory: connection.
    eprintln!("cold start");
    let conn = rusqlite::Connection::open_in_memory().context("open :memory: connection")?;

    if let Some(source_dir) = source {
        run_initial_parse(&conn, source_dir, language, "parsing")?;
    }

    Ok(conn)
}

/// Run a full-tree parse + log the standard `N parsed, N unchanged, N
/// deleted, N errors` summary. Shared by the warm-start and cold-start
/// branches of `init_living_db`. The `kind` prefix lets each branch
/// label its log line ("incremental reparse" vs "parsing") while
/// keeping the summary format identical so log scrapers see one shape.
fn run_initial_parse(
    conn: &rusqlite::Connection,
    source_dir: &Path,
    language: Option<&str>,
    kind: &str,
) -> Result<()> {
    eprintln!("{kind} {} ...", source_dir.display());
    let result = crate::cmd_parse::parse_into_conn(conn, source_dir, language, None)?;
    eprintln!(
        "{} parsed, {} unchanged, {} deleted, {} errors",
        result.parsed, result.unchanged, result.deleted, result.errors,
    );
    Ok(())
}

/// Try to restore the living db from the arena's active buffer.
/// Returns `None` if the arena doesn't exist or has no valid data.
fn try_warm_start(ctrl_path: &Path) -> Result<Option<rusqlite::Connection>> {
    use std::io::Cursor;

    // Two classes of "fall through to cold start":
    //   - FRESH state (no warm data exists yet): return Ok(None) silently.
    //     These are normal first-launch / fresh-ctrl conditions.
    //   - REAL ERROR (data exists but is malformed/inaccessible): return
    //     Ok(None) but log::warn so the failure is visible to operators.
    //     Cold start still works, but the warm-start path produced
    //     unexpected output that's worth investigating.
    let ctrl = match leyline_core::Controller::open_or_create(ctrl_path) {
        Ok(c) => c,
        Err(e) => {
            log::warn!(
                "warm start: open Controller at {}: {e:#} — falling through to cold start",
                ctrl_path.display(),
            );
            return Ok(None);
        }
    };

    let arena_path = ctrl.arena_path();
    if arena_path.is_empty() {
        // FRESH: ctrl exists but no arena registered yet (set_arena hasn't run).
        return Ok(None);
    }

    let file = match std::fs::File::open(&arena_path) {
        Ok(f) => f,
        Err(e) => {
            log::warn!(
                "warm start: arena file {arena_path} unreadable: {e:#} — \
                 falling through to cold start",
            );
            return Ok(None);
        }
    };

    let mmap = unsafe { memmap2::Mmap::map(&file)? };
    if mmap.len() < std::mem::size_of::<leyline_core::ArenaHeader>() {
        log::warn!(
            "warm start: arena {arena_path} too small ({} bytes) for header — \
             falling through to cold start",
            mmap.len(),
        );
        return Ok(None);
    }

    let header: &leyline_core::ArenaHeader =
        bytemuck::from_bytes(&mmap[..std::mem::size_of::<leyline_core::ArenaHeader>()]);

    let file_size = mmap.len() as u64;
    let offset = match header.validate_header(file_size) {
        Ok(o) => o,
        Err(e) => {
            // Typed reason → operator can distinguish a stale-VERSION
            // arena needing the cutover from on-disk corruption or a
            // truncated file. Falls through to cold start in every
            // case (warm restart is best-effort), but the log line
            // is now actionable.
            log::warn!(
                "warm start: arena {arena_path} rejected — {e} \
                 — falling through to cold start",
            );
            return Ok(None);
        }
    };
    let buf_size = leyline_core::ArenaHeader::buffer_size(file_size);
    let buf = &mmap[offset as usize..(offset + buf_size) as usize];

    // FRESH: arena exists but the active buffer hasn't been written to yet
    // (no snapshot has flipped the header). Not an error.
    if buf.iter().take(16).all(|&b| b == 0) {
        return Ok(None);
    }

    // Deserialize as writable :memory: connection.
    let mut conn = rusqlite::Connection::open_in_memory()?;
    conn.deserialize_read_exact(
        "main",
        Cursor::new(buf),
        buf.len(),
        false, // writable
    )
    .context("warm start: sqlite3_deserialize failed")?;

    // T2.4: log the new substrate identity (current_root prefix) on
    // recovery. Generation is gone from the public API.
    let root = ctrl.current_root();
    eprintln!(
        "recovered from arena (root {:02x}{:02x}{:02x}{:02x})",
        root[0], root[1], root[2], root[3]
    );

    Ok(Some(conn))
}

/// Take the live-db lock, snapshot to arena, and log any failure under
/// `label`. Used by the background timers (periodic snapshot, post-watch
/// snapshot) where a snapshot failure is recoverable — the next tick
/// retries — but should still surface in logs. The graceful-shutdown
/// path uses `eprintln!` directly because it has a distinct success
/// message; this helper is for the fire-and-forget periodic shape.
pub fn snapshot_or_log(
    live_db: &std::sync::Mutex<rusqlite::Connection>,
    ctrl_path: &Path,
    label: &str,
) {
    let guard = live_db.lock().unwrap();
    if let Err(e) = snapshot_to_arena(&guard, ctrl_path) {
        log::error!("{label}: {e:#}");
    }
}

/// Non-blocking variant of `snapshot_or_log` for the periodic snapshot
/// timer (5fea4e). Uses `try_lock`: if the live_db lock is contended
/// (e.g. `op_reparse` holds it across `parse_into_conn`), this tick is
/// skipped with a debug log instead of blocking the tokio worker for
/// the duration of the long-running operation.
///
/// Without this guard, a 500ms+ reparse would queue every subsequent
/// snapshot tick behind it; on completion the queue drains in a burst,
/// and any concurrent UDS query (which only borrows the db read-only)
/// blocks behind the burst. `WouldBlock` and `Poisoned` are both
/// recoverable — the next tick retries the lock.
/// Best-effort read of `sqlite3_total_changes()` under a brief try_lock.
///
/// Returns `None` if the mutex is contended (some other task is holding
/// the live db) or poisoned. Callers should treat contention as
/// "unknown, try again next tick" rather than "clean" — snapshotting
/// slightly more often is safer than skipping a real write.
///
/// Used by the periodic snapshot timer (bead `ley-line-open-1a0a2a`) as
/// the dirty-check: if the counter hasn't advanced since the last
/// successful snapshot, the timer skips the whole
/// serialize-and-msync path. Idle daemons with mache-scale DBs used to
/// pay the O(DB_size) msync cost every 500ms even when no writes
/// happened; now they stay near zero CPU.
///
/// `total_changes()` is O(1) — SQLite maintains it as an internal
/// counter that increments on INSERT/UPDATE/DELETE row events. It does
/// NOT increment on schema-only changes (CREATE TABLE, CREATE INDEX,
/// etc.) which is fine for our case: the daemon initializes schema at
/// startup then serves data writes; we snapshot after the initial
/// setup (line 207).
pub fn read_total_changes(live_db: &std::sync::Mutex<rusqlite::Connection>) -> Option<u64> {
    use std::sync::TryLockError;
    match live_db.try_lock() {
        Ok(guard) => Some(guard.total_changes()),
        Err(TryLockError::WouldBlock) => None,
        Err(TryLockError::Poisoned(_)) => None,
    }
}

pub fn try_snapshot_or_log(
    live_db: &std::sync::Mutex<rusqlite::Connection>,
    ctrl_path: &Path,
    label: &str,
) -> bool {
    use std::sync::TryLockError;
    match live_db.try_lock() {
        Ok(guard) => {
            if let Err(e) = snapshot_to_arena(&guard, ctrl_path) {
                log::error!("{label}: {e:#}");
            }
            true
        }
        Err(TryLockError::WouldBlock) => {
            log::debug!("{label}: live_db contended, skipping this tick");
            false
        }
        Err(TryLockError::Poisoned(poisoned)) => {
            // A previous writer panicked. Recover the inner state and
            // retry once — better to take a single hit than wedge the
            // snapshot timer permanently. Same recovery strategy as the
            // embed drainer (294fd6b).
            log::error!("{label}: live_db mutex poisoned; recovering inner state",);
            let guard = poisoned.into_inner();
            if let Err(e) = snapshot_to_arena(&guard, ctrl_path) {
                log::error!("{label}: post-recovery snapshot failed: {e:#}");
            }
            true
        }
    }
}

/// Serialize the living db and write it into the arena.
///
/// **Ordering contract** (load-bearing — see closed-source ley-line
/// ADR-001 §5 "Demand-Paged Strategy" + ADR-012 sync sequence):
///
/// The dual `set_arena` calls below mirror the network manifest-
/// then-data wire protocol. The first call is the local single-host
/// analog of the QUIC reliable-stream manifest publish; the second is
/// the atomic CAS commit point. Three invariants must hold:
///
/// 1. **File grown BEFORE size advertised.** A fresh-opening reader
///    that fetches `ctrl.arena_size()` and tries to mmap that many
///    bytes must never see `arena_size > file_size`. Therefore
///    `create_arena` (which calls `set_len`) precedes the early
///    `set_arena`. Reversing this order produces torn reads in
///    cross-process consumers (ADR-fixed bug ley-line-open-609d6a).
///
/// 2. **`current_root` advance is the publish point (T2.4).** Polling
///    readers (HotSwapGraph) compare `current_root`; they don't refresh
///    until it changes. The early `set_arena` preserves `current_root`
///    so readers stay on the old buffer until data is fully written.
///    The final `set_arena_with_root` advances the root, making the
///    new buffer visible. (Pre-T2.4 this rotated on `generation`; the
///    sync counter is now a private fence atom only.)
///
/// 3. **Advertisement errors abort the snapshot.** If the early
///    `set_arena` fails, a partially-grown file may be on disk with
///    the controller state half-published. Propagate the error;
///    callers (snapshot_or_log) move the daemon into Error phase so
///    operators see it. Silently continuing is the original bug
///    described in ley-line-open-609d6a.
pub fn snapshot_to_arena(conn: &rusqlite::Connection, ctrl_path: &Path) -> Result<()> {
    let db_bytes = conn.serialize("main").context("serialize living db")?;

    let mut ctrl =
        leyline_core::Controller::open_or_create(ctrl_path).context("open controller")?;
    let arena_path = ctrl.arena_path();
    let arena_size = ctrl.arena_size();

    let min_arena = leyline_core::ArenaHeader::HEADER_SIZE
        + db_bytes.len() as u64 * ARENA_GROWTH_FACTOR
        + ARENA_HEADROOM_BYTES;
    let new_size = std::cmp::max(arena_size, min_arena);

    let buf_capacity = leyline_core::ArenaHeader::buffer_size(new_size) as usize;
    if db_bytes.len() > buf_capacity {
        anyhow::bail!(
            "db ({} bytes) exceeds arena buffer capacity ({} bytes) at new_size {} bytes",
            db_bytes.len(),
            buf_capacity,
            new_size,
        );
    }

    // Step 1: grow the file (no-op if new_size == arena_size). Must
    // precede advertisement — see ordering invariant 1.
    let mut mmap =
        leyline_core::create_arena(Path::new(&arena_path), new_size).context("open arena file")?;

    // Step 2: advertise new size to controller via set_arena (no root
    // advance). Fresh-opening readers see arena_size = new_size; file
    // is already at new_size from step 1 so the advertised size is
    // safe to mmap. Polling readers don't refresh because current_root
    // is preserved (set_arena bumps the sync atom but keeps the root
    // bytes unchanged — readers compare roots, not the atom). Failure
    // here MUST abort the snapshot — see invariant 3.
    if new_size != arena_size {
        eprintln!(
            "auto-sizing arena: {}MB → {}MB (db is {}MB)",
            arena_size / (1024 * 1024),
            new_size / (1024 * 1024),
            db_bytes.len() / (1024 * 1024),
        );
        // T2.4: re-advertise (size grow) preserves current_root.
        // No publish here — readers don't see "change."
        ctrl.set_arena(&arena_path, new_size)
            .context("advertise new arena size before write")?;
    }

    // Step 3: write into the inactive buffer. ArenaHeader.active_buffer
    // is unchanged; old buffer remains the readable one until step 4.
    leyline_core::write_to_arena(&mut mmap, &db_bytes).context("write to arena")?;

    // Step 4: atomic publish — write current_root under Release-ordering
    // (T2.2 + T2.4). Polling readers see current_root change, refresh,
    // observe (new_size, new active_buffer, new current_root).
    // current_root = σ(serialized db bytes) via the substrate's
    // ContentAddressed impl; Σ §3.4 locks BLAKE3. Retrofitted from
    // inline `blake3::hash` per bead `ley-line-open-32201a`.
    let current_root: [u8; 32] = *db_bytes.as_ref().hash().as_bytes();
    ctrl.set_arena_with_root(&arena_path, new_size, current_root)
        .context("publish current_root (snapshot advance)")?;

    eprintln!(
        "snapshot to arena ({} bytes, root {})",
        db_bytes.len(),
        hex_short(&current_root),
    );
    Ok(())
}

/// Compact hex prefix for log lines (first 8 hex chars of a 32-byte hash).
fn hex_short(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(8);
    for b in &bytes[..4] {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
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
    use tokio::time::interval;

    let mut tick = interval(GIT_POLL_INTERVAL);
    let mut last_dirty: HashSet<String> = HashSet::new();
    let mut last_head: String = git_head(source_dir).unwrap_or_default();
    let mut git_failure_streak: u32 = 0;

    eprintln!(
        "git watcher started (polling every {}s)",
        GIT_POLL_INTERVAL.as_secs(),
    );

    loop {
        tick.tick().await;

        // 1. Check HEAD for branch switches.
        let current_head = git_head(source_dir).unwrap_or_default();
        let head_changed = !current_head.is_empty() && current_head != last_head;

        // 2. Check dirty files. On a non-repo --source, every poll fails.
        // Log the first failure at WARN so the user can act, then dedupe
        // to DEBUG to avoid 30-line/minute spam. The streak resets on the
        // next success — if the user runs `git init` mid-session we
        // resume normally and emit a fresh WARN if it fails again.
        let current_dirty = match git_dirty_files(source_dir) {
            Ok(files) => {
                git_failure_streak = 0;
                files
            }
            Err(e) => {
                if git_failure_streak == 0 {
                    log::warn!(
                        "git status failed at {}: {e:#} — watcher will keep \
                         polling at debug level. Hint: --source must point \
                         at a git repo for branch/HEAD tracking.",
                        source_dir.display(),
                    );
                } else {
                    log::debug!("git status failed (streak {}): {e:#}", git_failure_streak);
                }
                git_failure_streak = git_failure_streak.saturating_add(1);
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
        let scope: Option<&[String]> = if dirty_vec.is_empty() {
            None
        } else {
            Some(dirty_vec.as_slice())
        };
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
                    snapshot_or_log(&ctx.live_db, &ctx.ctrl_path, "watch snapshot failed");

                    // 5. Update state + emit events.
                    {
                        let mut s = ctx.state.write().unwrap();
                        s.last_reparse_at_ms = Some(crate::daemon::now_ms());
                        s.phase = DaemonPhase::Ready;
                    }
                    // u64 fields render as JSON strings to match capnp_json's
                    // op-response convention — see `op_sheaf_invalidate` in
                    // daemon/sheaf_ops.rs for the rationale.
                    emitter.emit(
                        "daemon.reparse.complete",
                        "leyline",
                        serde_json::json!({
                            "parsed": result.parsed.to_string(),
                            "deleted": result.deleted.to_string(),
                            "changed_files": result.changed_files.clone(),
                        }),
                    );

                    // Sheaf gap 1 (bead ley-line-open-3ab7db): drive
                    // enrichment from the watcher path so a source edit
                    // triggers HDC re-encode + complex-build + LSP
                    // scoped to the just-parsed files. Prior to this
                    // wiring, `git_watch_loop` stopped after emitting
                    // `daemon.reparse.complete`; enrichment only ran
                    // when a consumer explicitly called `op_enrich`,
                    // which left the "sheaf-driven region-precise
                    // invalidation" moat claim only reachable via
                    // consumer-driven polls. See
                    // `docs/audits/sheaf-invalidation-trace.md` § Gap 1.
                    //
                    // Contract: enrichment is best-effort. Reparse
                    // already succeeded and its writes are in the
                    // living db + arena. If a pass errors we log +
                    // emit `daemon.enrichment.failed` and continue —
                    // NEVER crash the watcher on an enrichment fault.
                    //
                    // Scope: use `result.changed_files` (files that
                    // actually parsed) rather than the raw git-dirty
                    // set — untracked files ignored by mtime or
                    // language filter shouldn't drag enrichment work.
                    run_watcher_enrichment(&ctx, source_dir, &result.changed_files, &emitter);
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

/// Run the enrichment pipeline scoped to `changed_files` and emit
/// `daemon.enrichment.complete` / `daemon.enrichment.failed` for the
/// watcher-driven cascade.
///
/// Bead `ley-line-open-3ab7db` (sheaf gap 1). This closes the wire from
/// scoped reparse → HDC re-encode + complex-build inside the daemon
/// itself; without it, enrichment only ran on consumer-invoked
/// `op_enrich`, which meant the sheaf-driven cascade was consumer-driven,
/// not source-change-driven. Region-precise `sheaf.invalidate` emit
/// (Gap 3, bead `ley-line-open-3b3476`) hangs off the completion event
/// this function emits.
///
/// Contract: best-effort. Reparse writes are already durable in the
/// living db + arena by the time we enter here. If a pass errors we
/// log and emit `daemon.enrichment.failed`, then continue; the next
/// tick will try again on whatever the next dirty set turns out to be.
/// The watcher task NEVER dies on an enrichment fault.
///
/// Locking: takes `ctx.live_db` for the pipeline duration. Callers MUST
/// have dropped any prior guard before invoking (matches the call site
/// in `git_watch_loop` where the reparse guard is dropped at line 1055
/// before this helper runs).
///
/// Exposed to integration tests so the watcher→enrichment wire has a
/// direct entry point that doesn't require spinning up a full git-poll
/// loop. Production code path is `git_watch_loop` calling this after a
/// successful scoped reparse.
pub fn run_watcher_enrichment(
    ctx: &Arc<crate::daemon::DaemonContext>,
    source_dir: &Path,
    changed_files: &[String],
    emitter: &crate::daemon::events::EventEmitter,
) {
    ctx.state.write().unwrap().phase = DaemonPhase::Enriching;

    let scope: Option<&[String]> = if changed_files.is_empty() {
        None
    } else {
        Some(changed_files)
    };

    let started_ms = crate::daemon::now_ms();
    let guard = ctx.live_db.lock().unwrap();
    let outcome = crate::daemon::enrichment::run_all(
        &ctx.enrichment_passes,
        &guard,
        source_dir,
        scope,
        Some(&ctx.state),
    );
    drop(guard);

    ctx.state.write().unwrap().phase = DaemonPhase::Ready;

    match outcome {
        Ok(stats) => {
            let duration_ms = crate::daemon::now_ms().saturating_sub(started_ms);
            let passes_json: Vec<serde_json::Value> = stats
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "pass_name": s.pass_name,
                        "files_processed": s.files_processed.to_string(),
                        "items_added": s.items_added.to_string(),
                        "duration_ms": s.duration_ms.to_string(),
                    })
                })
                .collect();
            emitter.emit(
                "daemon.enrichment.complete",
                "leyline",
                serde_json::json!({
                    "changed_files": changed_files,
                    "passes": passes_json,
                    "duration_ms": duration_ms.to_string(),
                }),
            );
        }
        Err(e) => {
            log::warn!("watcher-driven enrichment failed: {e:#}");
            emitter.emit(
                "daemon.enrichment.failed",
                "leyline",
                serde_json::json!({
                    "changed_files": changed_files,
                    "error": format!("{e:#}"),
                }),
            );
        }
    }
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
        anyhow::bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // --porcelain -z: NUL-separated entries, each starts with 2-char
    // status + space + path. For renames + copies (status code R/C),
    // the rename produces TWO consecutive entries:
    //   entry N:   "RX newpath\0"  (status, space, NEW path)
    //   entry N+1: "oldpath\0"     (BARE old path, no status prefix)
    // We want the new path in the dirty set; the old path's removal
    // is already handled by the _file_index diff downstream.
    //
    // The state machine: walk entries in order. If we see a status
    // entry with code R or C, the NEXT non-empty entry is the rename
    // source and must be skipped, not stripped of 3 chars.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut files = std::collections::HashSet::new();
    let mut skip_next_as_rename_source = false;
    for entry in stdout.split('\0') {
        if entry.is_empty() {
            continue;
        }
        if skip_next_as_rename_source {
            skip_next_as_rename_source = false;
            continue;
        }
        if entry.len() < 4 {
            // Not a status-prefixed entry; defensive skip.
            continue;
        }
        // Status byte at index 0 (X = index status). R = renamed, C = copied.
        let xy_first = entry.as_bytes()[0];
        if xy_first == b'R' || xy_first == b'C' {
            skip_next_as_rename_source = true;
        }
        files.insert(entry[3..].to_string());
    }

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    // ── try_warm_start: error-handling pins ────────────────────────────
    //
    // try_warm_start distinguishes two classes of "fall through to cold
    // start":
    //   - FRESH state (no warm data exists yet): silent Ok(None). Normal
    //     first-launch / fresh-ctrl conditions.
    //   - REAL ERROR (data exists but is malformed/inaccessible): log::warn
    //     before Ok(None). Cold start still works, but the warm-start path
    //     produced unexpected output that's worth investigating.
    //
    // The tests below assert Ok(None) is returned in both cases (cold
    // start always remains the safe fallback) and exercise both fresh
    // and error code paths.

    // ── read_total_changes: idle-CPU dirty-check pins ─────────────────
    //
    // The periodic snapshot timer (line 500-550) uses `read_total_changes`
    // to skip serialize+msync when nothing has been written since the
    // last snapshot. Pin the semantics so a refactor that broke the
    // dirty-check would surface here (regressing bead
    // `ley-line-open-1a0a2a`).

    #[test]
    fn read_total_changes_returns_zero_on_fresh_connection_with_only_schema() {
        // Schema DDL doesn't advance total_changes — only row events
        // (INSERT/UPDATE/DELETE) do. A fresh in-memory connection with
        // only CREATE TABLE run against it should read as zero, so the
        // snapshot timer skips ticks on daemons that never received a
        // write.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (x INTEGER)").unwrap();
        let live = StdMutex::new(conn);
        assert_eq!(read_total_changes(&live), Some(0));
    }

    #[test]
    fn read_total_changes_advances_on_row_writes() {
        // total_changes() must increment on real writes so the timer
        // fires a snapshot when it should.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (x INTEGER)").unwrap();
        let live = StdMutex::new(conn);
        let before = read_total_changes(&live).unwrap();
        live.lock()
            .unwrap()
            .execute("INSERT INTO t VALUES (1)", [])
            .unwrap();
        let after = read_total_changes(&live).unwrap();
        assert!(
            after > before,
            "total_changes must advance after INSERT — got before={before}, after={after}",
        );
    }

    #[test]
    fn read_total_changes_returns_none_when_contended() {
        // If another holder has the lock (e.g. op_reparse is running),
        // the dirty-check returns None. Timer treats None as "try
        // again next tick" — skipping is safer than snapshotting on
        // stale data, and safer than blocking the tokio worker.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let live = StdMutex::new(conn);
        let _held_guard = live.lock().unwrap();
        assert_eq!(read_total_changes(&live), None);
    }

    #[test]
    fn read_total_changes_returns_none_when_poisoned() {
        // Poison recovery is deliberately NOT auto-attempted here.
        // Poisoned mutex means some earlier writer panicked; the
        // regular try_snapshot_or_log path already has recovery
        // logic (line 738-748). The dirty-check just returns None so
        // the timer treats it as "unknown" — will retry next tick,
        // and if the snapshot itself is attempted it'll go through
        // the poisoning recovery path.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let live = std::sync::Arc::new(StdMutex::new(conn));
        let live_thread = live.clone();
        let handle = std::thread::spawn(move || {
            let _guard = live_thread.lock().unwrap();
            panic!("intentionally poison the lock");
        });
        let _ = handle.join();
        assert_eq!(read_total_changes(&live), None);
    }

    #[test]
    fn try_snapshot_or_log_skips_when_lock_contended() {
        // 5fea4e contract: when the live_db mutex is contended (e.g.
        // op_reparse holds it across parse_into_conn), the periodic
        // snapshot timer must skip the tick — NOT block the tokio
        // worker thread waiting for the lock to release. Pin so a
        // refactor that swapped try_lock for blocking lock would
        // surface here as a hung test.
        let dir = TempDir::new().unwrap();
        let ctrl_path = dir.path().join("contended.ctrl");
        // Need a real arena for snapshot_to_arena to find — it'll
        // bail when called, but the lock-skip path returns BEFORE we
        // get there, so we don't need a working snapshot.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let live_db = StdMutex::new(conn);

        // Hold the lock from this thread.
        let _held_guard = live_db.lock().unwrap();

        // Now invoke try_snapshot_or_log — it must observe WouldBlock
        // and return false WITHOUT trying to take the lock again
        // (which would deadlock since we hold it on the same thread).
        let snapshotted = try_snapshot_or_log(&live_db, &ctrl_path, "contention test");
        assert!(
            !snapshotted,
            "try_snapshot_or_log must return false when lock is held by another holder",
        );
    }

    #[test]
    fn try_snapshot_or_log_recovers_from_poisoned_lock() {
        // Sister contract: when the lock is poisoned (a previous
        // writer panicked), the timer recovers via into_inner() and
        // attempts the snapshot anyway. Same recovery strategy as
        // the embed drainer (294fd6b). Without recovery, one panic
        // would wedge the snapshot timer permanently — silent
        // freshness regression.
        let dir = TempDir::new().unwrap();
        let ctrl_path = dir.path().join("poisoned.ctrl");
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let live_db = std::sync::Arc::new(StdMutex::new(conn));

        // Poison the lock by panicking inside a write guard.
        let live_db_p = live_db.clone();
        let join = std::thread::spawn(move || {
            let _guard = live_db_p.lock().unwrap();
            panic!("deliberate panic to poison the lock");
        });
        let _ = join.join(); // expect panic; ignore result

        // Sanity: lock is poisoned.
        assert!(
            live_db.lock().is_err(),
            "pre-condition: lock should be poisoned"
        );

        // try_snapshot_or_log MUST recover and attempt the snapshot.
        // The snapshot itself will fail (no arena registered in this
        // test), but `try_snapshot_or_log` returns true to indicate
        // the lock was acquired (via into_inner) and the snapshot was
        // attempted (and logged as a recoverable failure).
        let attempted = try_snapshot_or_log(&live_db, &ctrl_path, "poison-recovery test");
        assert!(
            attempted,
            "try_snapshot_or_log must recover from poisoned lock and report attempt",
        );
    }

    #[test]
    fn warm_start_returns_none_on_missing_ctrl() {
        let dir = TempDir::new().unwrap();
        let ctrl = dir.path().join("nonexistent.ctrl");
        // No file exists yet — Controller::open_or_create will create one,
        // but it'll be empty (no arena_path). Should fall through cleanly.
        let result = try_warm_start(&ctrl).unwrap();
        assert!(result.is_none(), "missing-ctrl path should return None");
    }

    #[test]
    fn warm_start_returns_none_on_corrupted_ctrl() {
        // Garbage ctrl: try_warm_start MUST NOT panic and MUST return
        // None so cold start remains a safe fallback. The fix (5f7100-6)
        // logs a warn alongside the None — captured behavior is unchanged
        // from the caller's perspective; the new visibility is in the log.
        let dir = TempDir::new().unwrap();
        let ctrl = dir.path().join("corrupt.ctrl");
        std::fs::write(&ctrl, b"\x00\x01\x02 not a valid controller \xff\xfe").unwrap();

        let result = try_warm_start(&ctrl);
        match result {
            Ok(None) => {}
            Ok(Some(_)) => panic!("garbage ctrl should not produce a usable connection"),
            Err(e) => {
                // leyline_core may surface the corruption directly — also
                // acceptable. The contract is "no panic, no usable conn."
                eprintln!("warm_start surfaced error (acceptable): {e:#}");
            }
        }
    }

    // ── git helpers: behavior on a non-repo directory ──────────────────
    //
    // git_watch_loop calls git_dirty_files / git_head every 2s. On a
    // non-repo --source, the watcher logs the FIRST failure at WARN
    // (with a hint) and dedupes subsequent identical failures to DEBUG.
    // The streak resets on the next success, so a mid-session `git init`
    // recovers cleanly. These tests pin the underlying helper behavior;
    // streak/dedup is exercised end-to-end via integration tests.

    #[test]
    fn git_dirty_files_handles_renames_correctly() {
        // `git status --porcelain -z` emits a rename as TWO nul-
        // separated entries:
        //   entry1: "R  newpath\0"  (status + space + new path)
        //   entry2: "oldpath\0"     (bare old path, NO status prefix)
        // The parser detects status code R/C in entry1 and skips
        // entry2 as the rename-source. The dirty set should contain
        // ONLY the new path. (Old-path removal is handled downstream
        // by the _file_index diff during reparse.)
        let dir = TempDir::new().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        std::fs::write(dir.path().join("old.go"), b"package m").unwrap();
        std::process::Command::new("git")
            .args(["-c", "user.email=t@t", "-c", "user.name=t", "add", "old.go"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "init",
                "-q",
            ])
            .current_dir(dir.path())
            .status()
            .unwrap();
        // Rename via `git mv` to ensure git tracks it as a rename.
        std::process::Command::new("git")
            .args(["mv", "old.go", "new.go"])
            .current_dir(dir.path())
            .status()
            .unwrap();

        let dirty = git_dirty_files(dir.path()).unwrap();
        assert!(
            dirty.contains("new.go"),
            "new path must be in dirty set, got {dirty:?}"
        );
        // The old path's removal is handled by _file_index diff
        // during reparse; the dirty set should NOT carry it (and
        // certainly not the truncated ".go" phantom).
        assert!(
            !dirty.contains("old.go") && !dirty.contains(".go"),
            "rename source must not appear in dirty set; got {dirty:?}",
        );
        assert_eq!(dirty.len(), 1, "exactly one entry expected: {dirty:?}");
    }

    #[test]
    fn git_dirty_files_extracts_paths_correctly() {
        // Pin the porcelain -z parsing: each entry starts with 2-char
        // status + space; path is at offset 3. Pinning here matters
        // because the reparse path keys on this set — a parsing bug
        // would either reparse the wrong files or skip changed ones.
        // Sister to git_dirty_files_on_clean_repo_returns_empty_set.
        let dir = TempDir::new().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        // Write files in distinct states: untracked, modified, added.
        std::fs::write(dir.path().join("untracked.txt"), b"hello").unwrap();
        std::fs::write(dir.path().join("tracked.txt"), b"v1").unwrap();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "add",
                "tracked.txt",
            ])
            .current_dir(dir.path())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "init",
            ])
            .current_dir(dir.path())
            .status()
            .unwrap();
        std::fs::write(dir.path().join("tracked.txt"), b"v2").unwrap();

        let dirty = git_dirty_files(dir.path()).unwrap();
        assert!(
            dirty.contains("untracked.txt"),
            "untracked file must be in dirty set"
        );
        assert!(
            dirty.contains("tracked.txt"),
            "modified tracked file must be in dirty set"
        );
        // No phantom paths.
        assert_eq!(dirty.len(), 2, "exactly 2 dirty paths, got {dirty:?}");
    }

    #[test]
    fn git_dirty_files_on_clean_repo_returns_empty_set() {
        // Scale-problem pin. Most working repos are mostly-clean, so
        // this is the most-common code path. With clean repo,
        // `git status --porcelain -z` produces empty stdout → split
        // by NUL yields one empty entry → filter `entry.len() > 3`
        // excludes it. Pin: empty HashSet, no Err. A refactor that
        // off-by-one'd the filter (e.g. `> 0` instead of `> 3`) or
        // failed to handle empty-stdout cleanly would surface here.
        let dir = TempDir::new().unwrap();
        // git init + initial commit produces a clean repo with no
        // dirty files.
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "init",
            ])
            .current_dir(dir.path())
            .status()
            .unwrap();

        let dirty = git_dirty_files(dir.path()).expect("clean repo must succeed");
        assert!(
            dirty.is_empty(),
            "clean repo dirty set must be empty, got {dirty:?}"
        );
    }

    #[test]
    fn git_dirty_files_on_non_repo_returns_err() {
        // A bare temp dir is not a git repo. `git status` exits non-zero
        // and writes to stderr; git_dirty_files must surface that as a
        // Result::Err with the stderr message included so the watcher
        // can log something useful (today it just log::debug!s).
        let dir = TempDir::new().unwrap();
        let result = git_dirty_files(dir.path());
        assert!(result.is_err(), "non-repo should return Err");
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(
            err_msg.contains("git status failed") || err_msg.contains("not a git"),
            "error should describe the failure mode; got: {err_msg}",
        );
    }

    #[test]
    fn git_head_on_non_repo_returns_none() {
        // git_head returns Option (not Result) — non-repo silently returns
        // None. Pin: this is the watcher's current "skip HEAD-change
        // detection on non-repo" signal.
        let dir = TempDir::new().unwrap();
        assert!(git_head(dir.path()).is_none());
    }

    #[test]
    fn warm_start_returns_none_on_missing_arena_file() {
        // ctrl exists and is valid, but the arena_path it points at does
        // not exist on disk. Should fall through to cold start (today
        // silently — pin for 5f7100-6).
        let dir = TempDir::new().unwrap();
        let ctrl = dir.path().join("orphan.ctrl");
        let mut c = leyline_core::Controller::open_or_create(&ctrl).unwrap();
        c.set_arena("/tmp/cloister-no-such-arena-xyzzy", 1024 * 1024)
            .unwrap();
        drop(c);

        let result = try_warm_start(&ctrl).unwrap();
        assert!(result.is_none(), "missing-arena path should return None");
    }

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
        sh(
            dir.path(),
            &["git", "config", "user.email", "test@example.com"],
        );
        sh(dir.path(), &["git", "config", "user.name", "test"]);
        sh(dir.path(), &["git", "config", "commit.gpgsign", "false"]);
        std::fs::write(dir.path().join("a.go"), "package m\n\nfunc A() {}\n").unwrap();
        sh(dir.path(), &["git", "add", "."]);
        sh(dir.path(), &["git", "commit", "-q", "-m", "init"]);
        dir
    }

    /// Build a DaemonContext suitable for git_watch_loop testing.
    fn test_ctx(ctrl_path: &Path, ext: Arc<dyn DaemonExt>, source: &Path) -> Arc<DaemonContext> {
        let _ = leyline_core::create_arena(&ctrl_path.with_extension("arena"), 2 * 1024 * 1024)
            .unwrap();
        let mut ctrl = leyline_core::Controller::open_or_create(ctrl_path).unwrap();
        ctrl.set_arena(
            ctrl_path.with_extension("arena").to_string_lossy().as_ref(),
            2 * 1024 * 1024,
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
            enrich_inflight: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
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
            #[cfg(feature = "text-search")]
            text_search: Arc::new(leyline_text_search::null::NullEngine::new()),
            sheaf: Arc::new(crate::daemon::sheaf_ops::SheafState::new()),
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

    /// Bundle for `start_watcher_test` — keeps the fixture's owned values
    /// alive (TempDir auto-cleanup at drop; abort the task at test end).
    /// `_ctx` and `_dir` are stored only for their RAII lifetime — neither
    /// test reads them through the bundle, but dropping them early would
    /// kill the daemon context / TempDir mid-test.
    struct WatcherTestBed {
        repo: TempDir,
        _dir: TempDir,
        _ctx: Arc<DaemonContext>,
        ext: Arc<RecordingExt>,
        events_rx: tokio::sync::mpsc::Receiver<crate::daemon::events::Event>,
        task: tokio::task::JoinHandle<()>,
    }

    /// Spin up a fresh git fixture + DaemonContext + RecordingExt, subscribe
    /// to `topic`, and spawn `git_watch_loop`. Sleeps 2500ms so the watcher
    /// has ticked once before the test makes its first observable change —
    /// otherwise the first tick's "establish baseline" call could race with
    /// the test's modification and miss the event. Returns a bundle the
    /// caller can drive with arbitrary file/HEAD changes.
    async fn start_watcher_test(topic: &str) -> WatcherTestBed {
        let repo = fixture_repo();
        let dir = TempDir::new().unwrap();
        let ctrl_path = dir.path().join("test.ctrl");
        let ext = Arc::new(RecordingExt::new());
        let ctx = test_ctx(&ctrl_path, ext.clone(), repo.path());
        let events_rx = subscribe_to(&ctx.router, topic).await;
        let emitter = ctx.router.emitter();

        let watch_ctx = ctx.clone();
        let watch_source = repo.path().to_path_buf();
        let task = tokio::spawn(async move {
            git_watch_loop(watch_ctx, &watch_source, emitter).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;

        WatcherTestBed {
            repo,
            _dir: dir,
            _ctx: ctx,
            ext,
            events_rx,
            task,
        }
    }

    /// Wait up to 5s for the next event from a watcher subscription.
    /// Returns `true` if an event arrived within the deadline. Used by
    /// watcher tests where 5s covers two full git_watch_loop ticks at
    /// 2s each plus slack. Centralizes the timeout-to-bool chain so a
    /// future change to the deadline (or to the receiver type) is one
    /// site, not N.
    async fn recv_event_within_5s(
        rx: &mut tokio::sync::mpsc::Receiver<crate::daemon::events::Event>,
    ) -> bool {
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .ok()
            .flatten()
            .is_some()
    }

    /// `git_watch_loop` ticks every 2s. Modify a file, wait one full tick + a
    /// generous slack, and verify both the typed hook and the event fire.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_invokes_files_changed_hook_and_event() {
        let mut tb = start_watcher_test("daemon.files.changed").await;

        // Make the file dirty after the baseline tick.
        std::fs::write(
            tb.repo.path().join("a.go"),
            "package m\n\nfunc A() { /* edit */ }\n",
        )
        .unwrap();

        let saw_files_event = recv_event_within_5s(&mut tb.events_rx).await;
        tb.task.abort();

        assert!(saw_files_event, "expected daemon.files.changed event");
        let recorded = tb.ext.file_changes.lock().unwrap();
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
        let mut tb = start_watcher_test("daemon.head.changed").await;
        let initial_head = git_head(tb.repo.path()).unwrap();

        // New commit — bumps HEAD.
        std::fs::write(tb.repo.path().join("b.go"), "package m\n\nfunc B() {}\n").unwrap();
        sh(tb.repo.path(), &["git", "add", "."]);
        sh(tb.repo.path(), &["git", "commit", "-q", "-m", "add b"]);
        let new_head = git_head(tb.repo.path()).unwrap();
        assert_ne!(initial_head, new_head);

        let saw_head_event = recv_event_within_5s(&mut tb.events_rx).await;
        tb.task.abort();

        assert!(saw_head_event, "expected daemon.head.changed event");
        let head_calls = tb.ext.head_changes.lock().unwrap();
        assert!(!head_calls.is_empty(), "expected on_head_changed to fire");
        let (_old, new) = head_calls.last().unwrap();
        assert_eq!(new, &new_head, "hook should report the new HEAD sha");
    }

    // -----------------------------------------------------------------
    // Bead `ley-line-open-b7dd03`: --mcp-bind public-address gate.
    //
    // The fail-fast check sits at the top of `run_daemon`, before any
    // arena setup, so the tests below can pass throwaway paths and
    // expect the bail to fire before anything is touched on disk.
    // -----------------------------------------------------------------

    use std::net::{IpAddr, Ipv4Addr};

    /// Helper: minimal DaemonConfig for MCP-gate tests. Sets a
    /// throwaway arena path and immediate timeout so tests exit before
    /// any real setup. Callers override the MCP fields relevant to
    /// their specific assertion.
    fn test_mcp_gate_config(arena: &Path, nfs_port: u16) -> DaemonConfig {
        DaemonConfig {
            arena: arena.to_path_buf(),
            arena_size_mib: 64,
            control: None,
            mount: None,
            backend: "sqlite".to_string(),
            nfs_port,
            language: None,
            timeout: Some("0s".to_string()),
            source: None,
            mcp_port: None,
            mcp_bind: None,
            mcp_allow_public: false,
            mcp_no_auth: false,
        }
    }

    #[tokio::test]
    async fn mcp_public_bind_without_allow_flag_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = test_mcp_gate_config(&tmp.path().join("arena"), 12345);
        config.mcp_port = Some(8384);
        config.mcp_bind = Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        config.mcp_no_auth = true; // skip token gate; test exits on public-bind gate first
        let res = run_daemon(config, std::sync::Arc::new(NoExt)).await;
        let err = res.expect_err("non-loopback bind without --mcp-allow-public must bail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("--mcp-allow-public"),
            "error message must name the required flag; got: {msg}"
        );
        assert!(
            msg.contains("0.0.0.0"),
            "error message must echo the offending address; got: {msg}"
        );
    }

    #[tokio::test]
    async fn mcp_loopback_bind_without_allow_flag_proceeds_past_gate() {
        // Loopback bind never trips the gate regardless of the flag.
        // We expect the daemon to proceed past the gate and fail (or
        // succeed) on something else — what matters is the error does
        // NOT mention --mcp-allow-public.
        let tmp = tempfile::tempdir().unwrap();
        let mut config = test_mcp_gate_config(&tmp.path().join("arena"), 12346);
        config.mcp_port = Some(8384);
        config.mcp_bind = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
        config.mcp_no_auth = true;
        let res = run_daemon(config, std::sync::Arc::new(NoExt)).await;
        if let Err(e) = res {
            let msg = format!("{e:#}");
            assert!(
                !msg.contains("--mcp-allow-public"),
                "loopback bind must not trip the public-bind gate; got: {msg}"
            );
        }
    }

    #[tokio::test]
    async fn mcp_public_bind_with_allow_flag_proceeds_past_gate() {
        // When the operator explicitly opts in, the gate must not fire.
        let tmp = tempfile::tempdir().unwrap();
        let mut config = test_mcp_gate_config(&tmp.path().join("arena"), 12347);
        config.mcp_port = Some(8384);
        config.mcp_bind = Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        config.mcp_allow_public = true;
        config.mcp_no_auth = true;
        let res = run_daemon(config, std::sync::Arc::new(NoExt)).await;
        if let Err(e) = res {
            let msg = format!("{e:#}");
            assert!(
                !msg.contains("--mcp-allow-public"),
                "public bind WITH --mcp-allow-public must not trip the gate; got: {msg}"
            );
        }
    }

    #[tokio::test]
    async fn mcp_public_bind_gate_only_fires_when_mcp_port_is_set() {
        // The gate predicate is `mcp_port.is_some() && bind.is_public()
        // && !allow_public`. Drop mcp_port and the rest of the
        // condition shouldn't matter — pin that the daemon does NOT
        // surface the public-bind error when MCP HTTP is disabled.
        // We deliberately pass the most-likely-to-trip combination
        // (public bind + no allow flag) so a future short-circuit
        // bug that ignores mcp_port would be caught.
        let tmp = tempfile::tempdir().unwrap();
        let mut config = test_mcp_gate_config(&tmp.path().join("arena"), 12348);
        // no mcp_port — the gate's first conjunct
        config.mcp_bind = Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        config.mcp_no_auth = true;
        let res = run_daemon(config, std::sync::Arc::new(NoExt)).await;
        if let Err(e) = res {
            let msg = format!("{e:#}");
            assert!(
                !msg.contains("--mcp-allow-public"),
                "gate must NOT fire when mcp_port is None; got: {msg}"
            );
        }
    }
}
