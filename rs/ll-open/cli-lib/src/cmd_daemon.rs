//! Daemon command — arena + mount + UDS socket for coordination.
//!
//! Two entry points:
//! - `cmd_daemon()` — open edition, uses `NoExt` (no private extensions).
//! - `run_daemon()` — generic entry point that accepts an `Arc<dyn DaemonExt>`,
//!   called by ley-line (private) with its own extension.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::daemon::{
    DaemonContext, DaemonExt, DaemonPhase, DaemonState, EventRouter, NoExt,
    execution::{ExecutionDaemonExt, ExecutionHandler},
};
use crate::lifecycle;

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

/// When an image outgrows the arena's buffer, the new buffer holds the
/// image plus `1 / ARENA_HEADROOM_DIVISOR` of it (bead
/// `ley-line-open-8c8ded`), so a growing db resizes O(log n) times. The
/// arena is two such buffers (active and inactive) after its header.
/// Sizing the buffer to the image alone, as before, rewrote the whole
/// arena for every few KiB the db grew.
const ARENA_HEADROOM_DIVISOR: u64 = 4;

/// The smallest buffer a resize allocates.
const ARENA_MIN_BUFFER_BYTES: u64 = 1024 * 1024; // 1 MiB

// Compile-time invariants on the tuning constants. Each fails the build
// if a future edit violates the documented constraint — cheaper than a
// runtime test and impossible to skip in CI.
//
// (Clippy correctly notes that runtime assertions on these would be
// constant-folded; const _ asserts are the idiomatic Rust answer.)
const _: () = assert!(
    ARENA_HEADROOM_DIVISOR > 0,
    "the headroom is a fraction of the image",
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
    /// Arena size hint in MiB. Grown automatically by [`arena_size_for`].
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
    /// Optional MCP **Unix domain socket** path (bead
    /// `ley-line-open-6569de`). `None` disables the UDS transport.
    ///
    /// Independent of `mcp_port` — a daemon may serve both, one, or
    /// neither. cloister reaches bundles over UDS via notme-proxy
    /// (cloister ADR-0005); the `--control` socket carries the OPS
    /// protocol, not MCP, so this is the only MCP surface an attested
    /// caller can dial.
    pub mcp_uds: Option<std::path::PathBuf>,
    /// Bead `ley-line-open-c7d00f`: when `true`, drop any existing
    /// live-db + zero the controller before starting so a cold parse
    /// runs against `--source` regardless of prior arena state.
    /// Required opt-in when the user WANTS to reuse an arena that
    /// previously served a different `--source` (otherwise startup
    /// refuses with the source-root-mismatch error).
    pub reset_arena: bool,
}

/// Additive daemon behavior switches.
///
/// Kept separate from [`DaemonConfig`] so downstream crates that construct the
/// long-lived public config with struct literals do not break when an opt-in
/// behavior is added.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DaemonOptions {
    /// Activate/resume the private CDC index before the first arena snapshot,
    /// over the named target. `None` leaves CDC inactive.
    ///
    /// One field rather than a `bool` plus a target, so "enabled but no
    /// target" and "target but not enabled" are unrepresentable instead of
    /// merely unreachable.
    pub cdc: Option<crate::CdcTarget>,
    /// Auto-discovery symlink this daemon publishes. The execution entry
    /// points force their own name so an execution daemon cannot silently
    /// take over `~/.mache/default.sock` from a running substrate daemon.
    pub discovery: crate::daemon::socket::SocketDiscovery,
    /// What serves `--mount` (bead `ley-line-open-3e73dc`): the store's
    /// filesystem behind the host's FUSE or NFS client. `leyline-storefs`
    /// provides it; cli-lib cannot, since storefs depends on cli-lib. A
    /// daemon asked to mount with none is refused before it starts.
    pub host_mount: Option<HostMountRef>,
}

/// The guard of a host mount: dropping it unmounts, and lands what the
/// mount holds staged, before the daemon's final snapshot.
pub type MountGuard = Box<dyn Send>;

/// Mounts the daemon's store at a path (bead `ley-line-open-3e73dc`, ADR-0045
/// T8): every file, mode, symlink and directory `_tree` holds, read and
/// written as storefs reads and writes them for a guest, on the daemon's
/// one writer.
pub trait HostMount: Send + Sync {
    fn mount<'a>(
        &'a self,
        ctx: Arc<crate::daemon::DaemonContext>,
        at: &'a Path,
        backend: &'a str,
        nfs_port: u16,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<MountGuard>> + Send + 'a>>;
}

/// A [`HostMount`] held by [`DaemonOptions`]. Two are equal when they are the
/// same provider.
#[derive(Clone)]
pub struct HostMountRef(pub Arc<dyn HostMount>);

impl std::fmt::Debug for HostMountRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostMountRef")
    }
}

impl PartialEq for HostMountRef {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for HostMountRef {}

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
/// 8. Wait for shutdown (Ctrl+C or timeout)
/// 9. Cleanup (remove socket file)
pub async fn run_daemon(config: DaemonConfig, ext: Arc<dyn DaemonExt>) -> Result<()> {
    run_daemon_with_options(config, ext, DaemonOptions::default()).await
}

/// Run a daemon with the first-party execution/v1 surface enabled.
///
/// `handler` must be constructed by a trusted embedding application. In
/// particular, rootfs/CAS resolution and backend host paths are never read
/// from execution requests received over UDS. Existing callers that do not
/// need execution continue to use [`run_daemon`] with [`NoExt`].
pub async fn run_execution_daemon(
    config: DaemonConfig,
    handler: Arc<dyn ExecutionHandler>,
) -> Result<()> {
    run_execution_daemon_with_options(config, handler, DaemonOptions::default()).await
}

/// Variant of [`run_execution_daemon`] with explicit daemon options.
pub async fn run_execution_daemon_with_options(
    config: DaemonConfig,
    handler: Arc<dyn ExecutionHandler>,
    options: DaemonOptions,
) -> Result<()> {
    run_daemon_with_options(
        config,
        Arc::new(ExecutionDaemonExt::new(handler)),
        execution_daemon_options(options),
    )
    .await
}

/// [`run_execution_daemon_with_options`], publishing the daemon's context to
/// `context` once it exists: the embedder's workspace-share provider serves
/// a run's `/work` from that context's store, on the socket's one writer
/// (ADR-0044 V5).
pub async fn run_execution_daemon_with_context(
    config: DaemonConfig,
    handler: Arc<dyn ExecutionHandler>,
    options: DaemonOptions,
    context: crate::daemon::ContextSlot,
) -> Result<()> {
    run_daemon_with_options(
        config,
        Arc::new(ExecutionDaemonExt::new(handler).with_context_slot(context)),
        execution_daemon_options(options),
    )
    .await
}

/// The execution surface always publishes under its own discovery name,
/// whatever the caller passed. Sharing `default.sock` with the substrate
/// daemon means whichever starts second silently steals the other's clients:
/// every mache client that resolves the symlink then talks to the wrong
/// daemon against the wrong arena.
fn execution_daemon_options(options: DaemonOptions) -> DaemonOptions {
    DaemonOptions {
        discovery: crate::daemon::socket::SocketDiscovery::Execution,
        ..options
    }
}

/// The arena's size in bytes for `--arena-size-mib`.
fn arena_bytes(arena_size_mib: u64) -> u64 {
    arena_size_mib * 1024 * 1024
}

/// Refuse a mount nothing can provide. A daemon asked to mount that ran
/// headless instead gave its caller no mount and no failure (bead
/// `ley-line-open-2be4a7`). `provided` says whether the caller passed a
/// [`HostMount`]: the `leyline` binary does when it is built with the
/// `mount` feature.
fn refuse_unavailable_mount(mount: Option<&Path>, provided: bool) -> Result<()> {
    if let (Some(path), false) = (mount, provided) {
        anyhow::bail!(
            "cannot mount at {}: this leyline was built without the `mount` feature. \
             Build with `--features mount`, or omit --mount to run headless.",
            path.display()
        );
    }
    Ok(())
}

/// Run the daemon with explicit additive behavior options.
pub async fn run_daemon_with_options(
    config: DaemonConfig,
    ext: Arc<dyn DaemonExt>,
    options: DaemonOptions,
) -> Result<()> {
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
        mcp_uds,
        reset_arena,
    } = config;
    let arena = arena.as_path();
    let control = control.as_deref();
    let mount = mount.as_deref();
    refuse_unavailable_mount(mount, options.host_mount.is_some())?;
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

    // 0. Admission control — two gates run in sequence, belt+suspenders.
    //
    // (a) `flock(2)`-backed advisory lockfile at `<arena>.lock` (bead
    //     `ley-line-open-0cba88`). Fast path: OS auto-releases on process
    //     exit (including SIGKILL). Catches every daemon that runs code
    //     from PR #130 forward.
    //
    // (b) `<arena>.owner` JSON sentinel (bead `ley-line-open-c7d00f`).
    //     Correctness path for pre-0cba88 daemons that hold no flock —
    //     the sentinel records the owner PID + source_root + version, and
    //     new daemons check `kill(pid, 0)` for liveness. Names the
    //     specific stale daemon in the error so operators can
    //     `kill -9 <pid>` and retry.
    //
    // Both bind to locals so their guards persist for the daemon's
    // entire runtime; Drop on either releases its resource independently.
    let _arena_lock = crate::daemon::arena_lock::ArenaLock::try_acquire(arena)
        .context("arena admission control (flock)")?;
    let source_root_str = source
        .and_then(|p| p.canonicalize().ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let _arena_owner = crate::daemon::arena_owner::try_acquire(arena, &source_root_str)
        .context("arena admission control (owner sentinel)")?;

    // 1. Arena setup.
    let ctrl_path = lifecycle::setup_arena(arena, arena_bytes(arena_size_mib), control)?;

    // Lifecycle state — starts as Initializing, transitions through Parsing /
    // Enriching / Ready / Error. Shared with op_status and background tasks.
    let state = Arc::new(parking_lot::RwLock::new(DaemonState::initializing()));

    // 2. Initialize the living database.
    //
    // Bead `ley-line-open-98fb67` (WAL adoption 15a): the living db is
    // file-backed at `<ctrl>.live.db` with `PRAGMA journal_mode=WAL`.
    // Warm start: `.live.db` file already exists → reopen (WAL replay is
    // automatic). Cold start: fresh file-backed connection + parse from
    // `--source`; any stale `.live.db` from a prior run is unlinked when
    // the controller shows no prior snapshot state (root == 0 sentinel).
    //
    // Bead `ley-line-open-f0239d` (WAL adoption 15b): after 15a builds
    // the writer connection, we wrap it in a `LiveDb` container that
    // also owns an N-connection reader pool attached to the same file.
    // `with_read` closures check out a pooled reader (`query_only=ON`,
    // `busy_timeout=5000`); `with_write` closures hold the writer
    // mutex. See `daemon::db_pool` for the container semantics.
    //
    // The arena remains the cross-host Σ substrate publication surface;
    // `snapshot_to_arena` still flips it after every write burst. Only
    // the underlying live storage swaps from a single mutex-wrapped
    // connection to pool+writer.
    let live_db_path = live_db_path_for(&ctrl_path);
    let live_conn = match init_living_db(&ctrl_path, &live_db_path, source, language, reset_arena) {
        Ok(conn) => conn,
        Err(e) => {
            state.write().phase = DaemonPhase::Error(format!("init failed: {e:#}"));
            return Err(e);
        }
    };

    #[cfg(feature = "cdc")]
    {
        let report = match activate_cdc_and_snapshot(&live_conn, &ctrl_path, options.cdc) {
            Ok(report) => report,
            Err(error) => {
                state.write().phase =
                    DaemonPhase::Error(format!("initial publish failed: {error:#}"));
                return Err(error);
            }
        };
        match report {
            // `stranded_source_blobs_manifest_rows` is not padding either. A
            // switch FROM `source_blobs` TO `nodes` on the same long-lived
            // arena leaves the `source_blobs` index behind: normal GC cannot
            // reclaim it (those rows are still fresh, not dead), so silence
            // here reads as "nothing left over" when the arena is actually
            // carrying two indexes (`ley-line-open-1869d0`).
            Some(CdcActivationReport::Nodes(report)) => eprintln!(
                "CDC activation (nodes): eligible={} populated={} already_fresh={} \
                 source_bytes={} stranded_source_blobs_manifest_rows={}",
                report.eligible_nodes,
                report.populated_nodes,
                report.already_fresh_nodes,
                report.processed_source_bytes,
                report.stranded_source_blobs_manifest_rows,
            ),
            // `skipped_sub_floor` is not padding. A `source_blobs` run over a
            // corpus of small rows legitimately populates nothing, and without
            // this count that outcome is indistinguishable from a silent
            // no-op. It is the number that explains a disappointing result:
            // 395,173 of 395,173 nodes fell below the chunking floor on a real
            // mache projection (`ley-line-open-baa57f`). `stranded_nodes_manifest_rows`
            // is the mirror of the nodes-branch count above, same reasoning.
            Some(CdcActivationReport::SourceBlobs(report)) => eprintln!(
                "CDC activation (source-blobs): eligible={} populated={} \
                 already_fresh={} skipped_sub_floor={} source_bytes={} \
                 stranded_nodes_manifest_rows={}",
                report.eligible_blobs,
                report.populated_blobs,
                report.already_fresh_blobs,
                report.skipped_sub_floor_blobs,
                report.processed_source_bytes,
                report.stranded_nodes_manifest_rows,
            ),
            None => {}
        }
    }
    #[cfg(not(feature = "cdc"))]
    {
        if let Some(target) = options.cdc {
            state.write().phase =
                DaemonPhase::Error("CDC activation unavailable in this build".into());
            anyhow::bail!(
                "daemon --cdc --cdc-target {target} requires the 'cdc' feature \
                 (compile with --features cdc)"
            );
        }
        snapshot_to_arena(&live_conn, &ctrl_path)?;
    }

    // Build the LiveDb container (writer + reader pool). The writer
    // connection produced the file and applied WAL; the pool attaches
    // to the same `.live.db` path with per-connection `query_only=ON` +
    // `busy_timeout=5000` pragmas — see `LiveDb::new`. Bead
    // `ley-line-open-f0239d`.
    let live_db = match crate::daemon::db_pool::LiveDb::new(
        live_conn,
        &live_db_path,
        crate::daemon::db_pool::default_pool_size(),
    ) {
        Ok(db) => db,
        Err(e) => {
            state.write().phase = DaemonPhase::Error(format!("reader pool init failed: {e:#}"));
            return Err(e);
        }
    };

    // Capture initial HEAD if --source is set.
    if let Some(src) = source
        && let Some(sha) = git_head(src)
    {
        state.write().head_sha = Some(sha);
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
        Arc::new(parking_lot::Mutex::new(std::collections::BinaryHeap::new()));

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
    #[cfg(feature = "sheaf")]
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
        live_db,
        enrich_inflight: Arc::new(parking_lot::Mutex::new(std::collections::HashSet::new())),
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
            #[cfg(feature = "sheaf")]
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
        #[cfg(feature = "sheaf")]
        sheaf,
    });
    // Before the socket: whatever the extension serves from the context
    // shares the socket's one writer from the first request on.
    ext.on_context(&ctx);

    // Spawn the embed-queue drainer: query ops promote node_ids; this loop
    // drains them in priority order and refreshes the sidecar VectorIndex.
    #[cfg(feature = "vec")]
    crate::daemon::embed::start_drain(ctx.clone());

    // Initial parse (during init_living_db) is done — daemon is ready to serve.
    state.write().phase = DaemonPhase::Ready;

    let sock_path = ctrl_path.with_extension("sock");
    crate::daemon::socket::spawn(ctx.clone(), sock_path.clone(), options.discovery)?;
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
    // Either transport arms the token decision. Gating this on `mcp_port`
    // alone meant `--mcp-uds` on its own started nothing — and UDS-only is
    // precisely cloister's case, since notme-proxy dials AF_UNIX.
    let mcp_handle = if mcp_port.is_some() || mcp_uds.is_some() {
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
                // Same token decision governs BOTH transports. A UDS that
                // served ungated while TCP was gated would be an auth bypass
                // reachable by anyone who can open the socket file — so the
                // gate is decided once, above, and applied to each listener.
                if let Some(uds_path) = mcp_uds.clone() {
                    match crate::daemon::mcp::spawn_uds(
                        ctx.clone(),
                        uds_path.clone(),
                        token.clone(),
                    ) {
                        Ok(_h) => {}
                        Err(e) => log::error!(
                            "MCP UDS server failed to start at {}: {e:#}",
                            uds_path.display(),
                        ),
                    }
                }
                match mcp_port {
                    Some(port) => {
                        match crate::daemon::mcp::spawn(ctx.clone(), mcp_bind, port, token) {
                            Ok(h) => Some(h),
                            Err(e) => {
                                log::error!(
                                    "MCP HTTP server failed to start on port {port}: {e:#}"
                                );
                                None
                            }
                        }
                    }
                    // UDS-only: no TCP listener to hand back. The UDS task is
                    // already spawned above and owns its own lifetime.
                    None => None,
                }
            }
        }
    } else {
        None
    };

    // 7. Mount (optional — omit --mount for headless mode).
    // The guard lives until shutdown, which drops it — unmounting — before
    // the final snapshot.
    // The mount serves the store (bead `ley-line-open-3e73dc`): `live.db`
    // through the daemon's reader pool and its one writer, each landed
    // proposal written into the checkout and published to the arena, as a
    // guest's share is served. `refuse_unavailable_mount` refused a mount
    // path with no provider above.
    let mounted = match (mount, options.host_mount.as_ref()) {
        (Some(mount_path), Some(provider)) => {
            let guard = provider
                .0
                .mount(ctx.clone(), mount_path, backend, nfs_port)
                .await?;
            eprintln!("mounted at {}", mount_path.display());
            Some(guard)
        }
        _ => {
            eprintln!("headless mode (no mount)");
            None
        }
    };

    // 7. Extension post-mount.
    ext.on_post_mount(&ctrl_path, &router);

    // mache is deliberately NOT spawned here (ley-line-open-620c3f). The
    // daemon used to run `mache serve --control <ctrl>` as a child, but
    // mache's `--control` flag was removed, so the spawn could only start
    // a child that exits on an unknown flag. The replacement is NOT
    // exec-coupling in either direction: both daemons are independently
    // operational services sharing the IPC surface (the control file +
    // UDS ops this function already exposes), with process lifecycle and
    // startup ordering owned by the init system — LaunchAgent/systemd
    // user units, ley-line-open-4bd5e0 (LLO unit) and the mache-side
    // twin. launchd/systemd declare the dependency order; a daemon
    // exec'ing its peer cannot.

    // 9. Git-aware file watcher — poll git status, reparse on change.
    if let Some(ref source_dir) = ctx.source_dir {
        let watch_ctx = ctx.clone();
        let watch_source = source_dir.clone();
        let watch_emitter = router.emitter();
        tokio::spawn(async move {
            git_watch_loop(watch_ctx, &watch_source, watch_emitter, None).await;
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
        // The daemon's publisher (bead `ley-line-open-8c8ded`): it wakes on
        // a landed proposal's `request_publish` or on the tick, and
        // publishes once for every commit since its last publish. The
        // publish runs on the blocking pool, off the writer.
        tokio::spawn(async move {
            use tokio::time::interval;
            let mut tick = interval(SNAPSHOT_INTERVAL);
            let mut last = read_total_changes(&snap_ctx.live_db.writer).unwrap_or(0);
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = snap_ctx.live_db.publish_requested.notified() => {}
                }
                let ctx = Arc::clone(&snap_ctx);
                let ctrl = snap_ctrl.clone();
                let published = tokio::task::spawn_blocking(move || {
                    let mut seen = last;
                    publish_if_dirty(&ctx.live_db, &ctrl, &mut seen).map(|p| (p, seen))
                })
                .await;
                match published {
                    Ok(Ok((true, seen))) => {
                        last = seen;
                        emitter.emit("daemon.snapshot", "leyline", serde_json::json!({}));
                    }
                    Ok(Ok((false, _))) => {}
                    Ok(Err(e)) => log::error!("publish failed: {e:#}"),
                    Err(e) => log::error!("publish task failed: {e}"),
                }
            }
        });
    }

    eprintln!("daemon ready — press Ctrl+C to stop");

    // 11. Wait for shutdown.
    lifecycle::wait_for_shutdown(timeout).await?;

    // 12. Graceful shutdown: unmount, final snapshot, cleanup.
    drop(mounted);
    if let Err(e) = publish_living_db(&ctx.live_db, &ctrl_path) {
        eprintln!("warn: final snapshot failed: {e:#}");
    } else {
        eprintln!("final snapshot saved to arena");
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

/// Derive the file-backed live-db path from a controller path.
///
/// `foo.ctrl` → `foo.live.db`. Sibling of `<ctrl>.arena`,
/// `<ctrl>.sock`, `<ctrl>.arena.lock`. Same naming convention
/// as `sock_path` at line ~366 (`ctrl_path.with_extension("sock")`).
///
/// Bead `ley-line-open-98fb67` (WAL adoption 15a).
pub fn live_db_path_for(ctrl_path: &Path) -> std::path::PathBuf {
    ctrl_path.with_extension("live.db")
}

/// Configure WAL journal mode + companion pragmas on a live-db connection.
///
/// Load-bearing: WAL is the whole point of the 15a migration
/// (bead `ley-line-open-98fb67`; empirical report at
/// `docs/research/2026-05-08-workerd-wal-sqlite-experiment.md`).
/// `journal_mode = WAL` must stick — a non-file-backed db silently
/// returns "memory" and the migration is a no-op. Bail loudly if
/// the return value is not `"wal"`.
///
/// - `journal_mode = WAL` — the win. Readers and one writer make
///   progress concurrently; DELETE-journal serializes both.
/// - `synchronous = FULL` — every COMMIT syncs the WAL before it
///   returns. `live.db` is where a mount write, a splice and a confined
///   run's write live (ADR-0040), and the mount acknowledges a write at
///   the client's fsync or NFS COMMIT; under `NORMAL` that write
///   survived a process crash but not a power loss or OS crash (bead
///   `ley-line-open-0a9931`).
/// - `fullfsync = ON`, `checkpoint_fullfsync = ON` — on macOS the commit's
///   sync is `F_FULLFSYNC`, which reaches past the drive's cache; plain
///   `fsync(2)` there stops at the drive. This commit is a proposal's
///   acknowledgement: its one full flush. The checkout and the Σ head it
///   writes after are ordered with `F_BARRIERFSYNC`, not flushed
///   (`crate::durable`, bead `ley-line-open-98d0c3`). Elsewhere SQLite
///   ignores both.
/// - `wal_autocheckpoint = 1000` — checkpoint every ~1000 pages
///   (~4 MiB at default 4KB page size). Keeps the WAL from
///   growing unbounded on a write-heavy daemon; ensures the
///   main db file stays close to snapshot-consumable size for
///   the arena publish path.
fn configure_wal(conn: &rusqlite::Connection) -> Result<()> {
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .context("set PRAGMA journal_mode=WAL")?;
    if !mode.eq_ignore_ascii_case("wal") {
        anyhow::bail!(
            "PRAGMA journal_mode=WAL did not stick — got {mode:?}. \
             The live db must be file-backed for WAL to activate; \
             `:memory:` connections silently ignore the pragma."
        );
    }
    conn.pragma_update(None, "synchronous", "FULL")
        .context("set PRAGMA synchronous=FULL")?;
    conn.pragma_update(None, "fullfsync", true)
        .context("set PRAGMA fullfsync=ON")?;
    conn.pragma_update(None, "checkpoint_fullfsync", true)
        .context("set PRAGMA checkpoint_fullfsync=ON")?;
    conn.pragma_update(None, "wal_autocheckpoint", 1000i64)
        .context("set PRAGMA wal_autocheckpoint=1000")?;
    Ok(())
}

/// Best-effort unlink of `<live_db>`, `<live_db>-wal`, and `<live_db>-shm`.
///
/// SQLite's WAL mode maintains two sidecar files (`-wal` for the
/// write-ahead log itself, `-shm` for the shared-memory index).
/// Leaving them behind after unlinking the main db file makes the
/// next open pick up corrupt state — SQLite happily replays a WAL
/// against a fresh db file if the sidecar names match. Cleaning
/// all three atomically is the safe reset for the cold-start branch.
fn unlink_live_db(live_db_path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut candidate = live_db_path.as_os_str().to_owned();
        candidate.push(suffix);
        let path = std::path::PathBuf::from(candidate);
        let _ = std::fs::remove_file(&path);
    }
}

/// Return `true` if the controller has no prior snapshot state — the
/// `current_root` field is the zero sentinel `[0u8; 32]`. Any earlier
/// `.live.db` at the derived path is orphaned from a prior experiment
/// and must be unlinked before the cold-start parse writes to a fresh
/// file-backed connection.
///
/// A controller that can't be opened (fresh disk / corrupt file) is
/// treated as fresh — cold-start is the safe fallback in every case.
fn controller_is_fresh(ctrl_path: &Path) -> bool {
    match leyline_core::Controller::open_or_create(ctrl_path) {
        Ok(c) => c.current_root() == [0u8; 32],
        Err(_) => true,
    }
}

/// Initialize the living database (file-backed WAL — bead
/// `ley-line-open-98fb67` sub-bead 15a).
///
/// **Warm start**: `<ctrl>.live.db` exists → open it. SQLite replays
/// any outstanding WAL on open, so state committed before the
/// previous crash is recovered without going through the arena
/// deserialize path. `--source` triggers an incremental reparse.
///
/// **Cold start**: the controller shows no prior snapshot
/// (`current_root == 0`). Any stale `.live.db`/`-wal`/`-shm` from an
/// earlier experiment is unlinked, then a fresh file-backed WAL
/// connection is created. `--source` triggers a full parse.
///
/// The arena-deserialize warm path (`try_warm_start`) is retained
/// as a fallback for the rare case where the controller has state
/// but the `.live.db` file is missing (external delete, disk
/// reprovision, etc.). It hydrates the WAL db from the arena's
/// active buffer, giving 15a a path back to a good state in exactly
/// the same failure modes the pre-15a implementation handled.
fn init_living_db(
    ctrl_path: &Path,
    live_db_path: &Path,
    source: Option<&Path>,
    language: Option<&str>,
    // Bead `ley-line-open-c7d00f`: when `true`, drop the live-db + zero
    // the controller before any warm-start so the daemon guarantees a
    // cold parse against `--source`. Opt-in only — the default flow
    // preserves warm-start's fast path.
    reset_arena: bool,
) -> Result<rusqlite::Connection> {
    // Bead `ley-line-open-c7d00f`: honor the explicit reset. Runs
    // BEFORE the controller-freshness / live-db-exists branches so a
    // reset-requested startup NEVER warm-starts, regardless of what
    // the prior daemon left behind.
    if reset_arena {
        eprintln!(
            "cold start (--reset-arena): unlinking any live db at {} + zeroing controller",
            live_db_path.display(),
        );
        unlink_live_db(live_db_path);
        // Zero the ROOT so `controller_is_fresh` reads true and
        // downstream `try_warm_start_from_arena` returns None — "no
        // prior snapshot", not "no arena". `setup_arena` (this
        // function's caller, always run first) already stamped the
        // path and size; blanking them too (as an earlier version of
        // this fix did, `ley-line-open-e37e03`) makes the next
        // `snapshot_to_arena` open an empty path and fail with "open
        // arena file" — including on a completely fresh arena, since
        // the flag's whole job is to run regardless of prior state.
        // Reading them back from the controller we just opened is
        // exactly `setup_arena`'s own stamp, not a guess.
        if let Ok(mut c) = leyline_core::Controller::open_or_create(ctrl_path) {
            let arena_path = c.arena_path();
            let arena_size = c.arena_size();
            let _ = c.set_arena_with_root(&arena_path, arena_size, [0u8; 32]);
        }
    }

    // Truly fresh controller → discard any orphan `.live.db` before
    // opening. Skipping this leaves stale rows from a prior daemon
    // lifecycle in the freshly-parsed db.
    if controller_is_fresh(ctrl_path) && live_db_path.exists() {
        eprintln!(
            "cold start: unlinking stale live db at {} (controller has no prior snapshot)",
            live_db_path.display(),
        );
        unlink_live_db(live_db_path);
    }

    if live_db_path.exists() {
        // Warm start: reopen the file-backed WAL db. SQLite replays
        // any outstanding WAL on first open; no arena deserialize.
        let conn = rusqlite::Connection::open(live_db_path)
            .with_context(|| format!("open live db {}", live_db_path.display()))?;
        configure_wal(&conn)?;
        // Bead `ley-line-open-c7d00f`: refuse to serve if the arena's
        // prior source_root disagrees with the current `--source`.
        // Prevents cross-repo cached-parse pollution when a shared
        // arena (e.g. `~/.mache/default.arena`) gets reused across
        // repos without an explicit reset.
        if let Some(source_dir) = source {
            verify_source_root_matches(&conn, source_dir).context("warm start (live-db)")?;
        }
        // Unconditional — a serve-only daemon (`--source` unset) never
        // parses, so this is its only shot at the pre-v5 refusal gate
        // (publication-audit F1, bead `ley-line-open-17c271`).
        crate::cmd_parse::refuse_pre_v5_projection(&conn).context("warm start (live-db)")?;
        eprintln!("warm start from {}", live_db_path.display());
        if let Some(source_dir) = source {
            run_initial_parse(&conn, source_dir, language, "incremental reparse")?;
        }
        return Ok(conn);
    }

    // `.live.db` is missing. Two possibilities:
    //   (a) Controller has arena state (`current_root != 0`) — the
    //       file was deleted out from under us. Fall back to the
    //       pre-15a arena-deserialize warm path, but hydrate into a
    //       fresh file-backed WAL connection so subsequent snapshots
    //       and reads stay on the 15a substrate.
    //   (b) Controller is fresh — cold start.
    //
    // `!reset_arena` guards this whole branch, not just the live-db one
    // above (`ley-line-open-e37e03`, second half): zeroing the
    // controller's root does not touch the ARENA FILE's own embedded
    // header, which still carries whatever a prior daemon last
    // snapshotted there. Without this guard, unlinking the live db just
    // routes a reset-requested startup into recovering that same stale
    // content one layer down — the exact failure mode this function's
    // own doc comment already promised could never happen ("a
    // reset-requested startup NEVER warm-starts, regardless of what the
    // prior daemon left behind").
    if !reset_arena && let Some(conn) = try_warm_start_from_arena(ctrl_path, live_db_path)? {
        // Same source-root guard as the live-db warm-start branch
        // above. The arena might have been snapshotted by daemon A
        // against repo R1; if daemon B started against repo R2 with
        // `--source /repos/R2`, we refuse rather than layer R2's
        // parse over R1's cached rows.
        if let Some(source_dir) = source {
            verify_source_root_matches(&conn, source_dir).context("warm start (arena)")?;
        }
        // Same F1 rationale as the live-db warm-start branch above.
        crate::cmd_parse::refuse_pre_v5_projection(&conn).context("warm start (arena)")?;
        eprintln!("warm start from arena → {}", live_db_path.display());
        if let Some(source_dir) = source {
            run_initial_parse(&conn, source_dir, language, "incremental reparse")?;
        }
        return Ok(conn);
    }

    // Cold start: fresh file-backed WAL connection.
    eprintln!("cold start at {}", live_db_path.display());
    let conn = rusqlite::Connection::open(live_db_path)
        .with_context(|| format!("open live db {}", live_db_path.display()))?;
    configure_wal(&conn)?;

    if let Some(source_dir) = source {
        run_initial_parse(&conn, source_dir, language, "parsing")?;
    }

    Ok(conn)
}

/// Bead `ley-line-open-c7d00f`: refuse warm-start when the arena's
/// prior `_meta.source_root` disagrees with the current `--source`.
///
/// The check reads `_meta.source_root` (populated by
/// `cmd_parse::parse_into_conn` at the end of every parse pass) and
/// canonicalizes both sides before comparing so relative-vs-absolute
/// path noise doesn't cause spurious refusals.
///
/// Returns `Ok(())` when either:
/// - no prior `source_root` is recorded (fresh arena, or `_meta`
///   table missing → pre-cbbedf DB), or
/// - the recorded root matches the current `--source`.
///
/// Returns `Err` with an operator-actionable message naming both
/// paths + the `--reset-arena` opt-out when they disagree.
fn verify_source_root_matches(conn: &rusqlite::Connection, current_source: &Path) -> Result<()> {
    // `get_meta` propagates SQL errors (missing table etc.) as Err,
    // but for source_root the absence of the row is "no prior parse
    // ran," which is fine — we're the first. Use `.ok().flatten()`
    // to treat both "table missing" and "row missing" as "no prior."
    let prior = leyline_ts::schema::get_meta(conn, "source_root")
        .ok()
        .flatten();
    let Some(prior) = prior else {
        return Ok(());
    };
    if prior.is_empty() {
        return Ok(());
    }
    let prior_path = std::path::PathBuf::from(&prior);
    let prior_canon = prior_path.canonicalize().unwrap_or(prior_path);
    let current_canon = current_source
        .canonicalize()
        .unwrap_or_else(|_| current_source.to_path_buf());
    if prior_canon == current_canon {
        return Ok(());
    }
    bail!(
        "arena source_root={} disagrees with --source={}. Refusing to warm-start; \
         the prior daemon's cached parses would silently pollute this run. See bead \
         ley-line-open-c7d00f. Pass `--reset-arena` to invalidate and cold-start, or \
         use a different `--arena` path per project.",
        prior_canon.display(),
        current_canon.display(),
    );
}

/// Reconcile the whole checkout of `source_dir` (bead
/// `ley-line-open-60f39f`): materialize what the store accepted while the
/// directory did not receive it, ingest what changed in the directory while
/// the daemon was down, report conflicts, and parse the tree. Logs the
/// standard `N parsed, N unchanged, N deleted, N errors` summary and a
/// checkout line. Shared by the warm-start and cold-start branches of
/// `init_living_db`; the `kind` prefix labels the first log line
/// ("incremental reparse" vs "parsing").
fn run_initial_parse(
    conn: &rusqlite::Connection,
    source_dir: &Path,
    language: Option<&str>,
    kind: &str,
) -> Result<()> {
    eprintln!("{kind} {} ...", source_dir.display());
    let reconciled = crate::checkout::reconcile(conn, source_dir, language, None)?;
    if let Some(result) = &reconciled.parse {
        eprintln!(
            "{} parsed, {} unchanged, {} deleted, {} errors",
            result.parsed, result.unchanged, result.deleted, result.errors,
        );
    }
    eprintln!(
        "checkout: {} materialized, {} conflicts, {} files read to compare",
        reconciled.materialized.len(),
        reconciled.conflicts.len(),
        reconciled.hashed,
    );
    if let Some(problems) = reconciled.problems() {
        eprintln!("warn: checkout: {problems}");
    }
    Ok(())
}

/// Try to restore the living db from the arena's active buffer into a
/// **file-backed WAL connection** at `live_db_path`.
///
/// Returns `None` if the arena doesn't exist or has no valid data. In
/// the WAL adoption (bead `ley-line-open-98fb67`), this path is a
/// fallback for the rare case where the controller has state but
/// `.live.db` is missing (external delete, disk reprovision, etc.).
/// The common warm-start hits the `.live.db` file directly and never
/// reaches this function.
///
/// Hydration approach: an arena buffer IS a full SQLite file image
/// (produced by `sqlite3_serialize`). Copy the bytes into a temp file
/// alongside `live_db_path`, rename atomically, then open with WAL
/// pragma. This avoids the `sqlite3_deserialize`-into-memory pattern
/// that would leave a `:memory:` connection (defeating 15a's purpose).
fn try_warm_start_from_arena(
    ctrl_path: &Path,
    live_db_path: &Path,
) -> Result<Option<rusqlite::Connection>> {
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

    let mmap = leyline_core::mmap::mmap_read(&file)?;
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

    // Hydrate `.live.db` from the arena bytes. Write to a `.tmp` sibling
    // and rename to make the swap atomic — a crash mid-write leaves the
    // next daemon boot with a coherent state (either the old file or no
    // file at all, never a torn write).
    let mut tmp_os = live_db_path.as_os_str().to_owned();
    tmp_os.push(".hydrate.tmp");
    let tmp_path = std::path::PathBuf::from(tmp_os);
    // Clean up any leftover from a prior failed hydrate.
    let _ = std::fs::remove_file(&tmp_path);
    std::fs::write(&tmp_path, buf)
        .with_context(|| format!("write hydrated live db to {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, live_db_path).with_context(|| {
        format!(
            "rename hydrated live db {} → {}",
            tmp_path.display(),
            live_db_path.display(),
        )
    })?;

    let conn = rusqlite::Connection::open(live_db_path)
        .with_context(|| format!("open hydrated live db {}", live_db_path.display()))?;
    configure_wal(&conn)?;

    // T2.4: log the new substrate identity (current_root prefix) on
    // recovery. Generation is gone from the public API.
    let root = ctrl.current_root();
    eprintln!(
        "recovered from arena (root {:02x}{:02x}{:02x}{:02x})",
        root[0], root[1], root[2], root[3]
    );

    Ok(Some(conn))
}

/// Publish the living db into the arena: the daemon's one publish path
/// (bead `ley-line-open-8c8ded`).
///
/// Holds [`LiveDb::publish_lock`](crate::daemon::db_pool::LiveDb) from the
/// serialize to the root advance, so publishes are serialized against each
/// other, and reads the image through a pool connection: one read
/// transaction, so the image is one committed state, and the writer stays
/// free. A proposal that commits during the publish is in the next one.
pub fn publish_living_db(live_db: &crate::daemon::db_pool::LiveDb, ctrl_path: &Path) -> Result<()> {
    let _publisher = live_db.publish_lock.lock();
    let conn = live_db
        .reader_pool
        .get()
        .map_err(|e| anyhow::anyhow!("reader pool checkout for a publish failed: {e}"))?;
    snapshot_to_arena(&conn, ctrl_path)
}

/// Publish when anything committed since the last publish this caller made
/// (`last` is the writer's `total_changes()` then). Returns whether it
/// published. A contended writer reads as "unknown": nothing is published
/// and the caller's next wake retries.
pub fn publish_if_dirty(
    live_db: &crate::daemon::db_pool::LiveDb,
    ctrl_path: &Path,
    last: &mut u64,
) -> Result<bool> {
    let Some(changes) = read_total_changes(&live_db.writer) else {
        return Ok(false);
    };
    if changes == *last {
        return Ok(false);
    }
    // Every commit counted in `changes` is visible to the read transaction
    // the publish opens after this point.
    publish_living_db(live_db, ctrl_path)?;
    *last = changes;
    Ok(true)
}

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
pub fn read_total_changes(live_db: &parking_lot::Mutex<rusqlite::Connection>) -> Option<u64> {
    live_db.try_lock().map(|g| g.total_changes())
}

/// The arena size an image of `image_len` bytes is written into, given the
/// arena's current size: unchanged while the image fits a buffer, else two
/// buffers with headroom (bead `ley-line-open-8c8ded`).
pub fn arena_size_for(current: u64, image_len: u64) -> u64 {
    if image_len <= leyline_core::ArenaHeader::buffer_size(current) {
        return current;
    }
    let buffer = std::cmp::max(
        image_len + image_len / ARENA_HEADROOM_DIVISOR,
        ARENA_MIN_BUFFER_BYTES,
    );
    leyline_core::ArenaHeader::HEADER_SIZE + 2 * buffer
}

/// `current_root` of a published image: BLAKE3 over its exact bytes, the
/// substrate's `ContentAddressed` hash. The image hashes across cores;
/// BLAKE3's tree mode gives the same bytes as the serial hash, and
/// `update_rayon` hashes a small input on the calling thread.
pub fn image_root(image: &[u8]) -> [u8; 32] {
    *blake3::Hasher::new()
        .update_rayon(image)
        .finalize()
        .as_bytes()
}

/// `image` as a standalone rollback-mode database, when it is a SQLite
/// image stamped WAL. The live db is WAL-mode, and its serialized image
/// keeps the stamp in header bytes 18 and 19 (file format write and read
/// versions, 2 = WAL). `sqlite3_deserialize` of a WAL-stamped image fails
/// every query with `SQLITE_CANTOPEN`, so a published image must say 1
/// (rollback) there (bead `ley-line-open-2bd77c`). The pages are unchanged,
/// and any other bytes are returned as they are.
pub(crate) fn rollback_image(mut image: Vec<u8>) -> Vec<u8> {
    const MAGIC: &[u8] = b"SQLite format 3\0";
    if image.starts_with(MAGIC) && image.get(18..20) == Some(&[2, 2][..]) {
        image[18..20].fill(1);
    }
    image
}

/// Serialize the living db and write it into the arena.
///
/// **Ordering contract** (load-bearing — see closed-source ley-line
/// ADR-001 §5 "Demand-Paged Strategy" + ADR-012 sync sequence):
///
/// The image is written first and advertised once, by the final
/// `set_arena_with_root`. Three invariants must hold:
///
/// 1. **File grown BEFORE size advertised.** A fresh-opening reader
///    that fetches `ctrl.arena_size()` and tries to mmap that many
///    bytes must never see `arena_size > file_size`. A growing snapshot
///    renames a full-size arena into place before the new size is
///    published (ley-line-open-609d6a).
///
/// 2. **The published image is never written.** The new image goes into
///    the inactive buffer of the same arena, or, when the arena grows,
///    into a new arena renamed over the old one, so a reader holding
///    the old one keeps the bytes it verified. Growing in place moved
///    buffer B under its readers (ley-line-open-5a0f07).
///
/// 3. **`current_root` advance is the publish point (T2.4).** Polling
///    readers (HotSwapGraph) compare `current_root`; they don't refresh
///    until it changes. `set_arena_with_root` advances size and root
///    together under the seqlock. Its error aborts the snapshot and
///    reaches the caller ([`publish_living_db`]'s callers log it, and the
///    next publish repairs it).
///
/// In the daemon every call goes through [`publish_living_db`], which
/// holds the single-publisher lock: two concurrent calls would write the
/// same inactive buffer.
pub fn snapshot_to_arena(conn: &rusqlite::Connection, ctrl_path: &Path) -> Result<()> {
    let db_bytes = rollback_image(
        conn.serialize("main")
            .context("serialize living db")?
            .to_vec(),
    );

    let mut ctrl =
        leyline_core::Controller::open_or_create(ctrl_path).context("open controller")?;
    let arena_path = ctrl.arena_path();
    let arena_size = ctrl.arena_size();

    let new_size = arena_size_for(arena_size, db_bytes.len() as u64);

    let buf_capacity = leyline_core::ArenaHeader::buffer_size(new_size) as usize;
    if db_bytes.len() > buf_capacity {
        anyhow::bail!(
            "db ({} bytes) exceeds arena buffer capacity ({} bytes) at new_size {} bytes",
            db_bytes.len(),
            buf_capacity,
            new_size,
        );
    }

    if new_size == arena_size {
        // Step 1: write into the inactive buffer. ArenaHeader.active_buffer
        // is unchanged; the old buffer remains the readable one until the
        // header flips at the end of the write.
        let mut mmap = leyline_core::create_arena(Path::new(&arena_path), new_size)
            .context("open arena file")?;
        leyline_core::write_to_arena(&mut mmap, &db_bytes).context("write to arena")?;
    } else {
        // Step 1, growing: never in place. Buffer offsets derive from the
        // file length, so a longer file moves buffer B under the image a
        // reader verified (`ley-line-open-5a0f07`). The grown arena is
        // built beside the old one and renamed over it; readers that
        // opened the old arena keep its inode. The file at the path is
        // full size before the new size is advertised below (invariant 1).
        eprintln!(
            "auto-sizing arena: {}MB → {}MB (db is {}MB)",
            arena_size / (1024 * 1024),
            new_size / (1024 * 1024),
            db_bytes.len() / (1024 * 1024),
        );
        leyline_core::write_grown_arena(Path::new(&arena_path), new_size, &db_bytes)
            .context("write grown arena")?;
    }

    // Step 2: atomic publish — write current_root under Release-ordering
    // (T2.2 + T2.4). Polling readers see current_root change, refresh,
    // observe (new_size, new active_buffer, new current_root).
    // current_root = σ(serialized db bytes) via the substrate's
    // ContentAddressed impl; Σ §3.4 locks BLAKE3. Retrofitted from
    // inline `blake3::hash` per bead `ley-line-open-32201a`; `image_root`
    // computes those bytes across cores (bead `ley-line-open-8c8ded`).
    let current_root: [u8; 32] = image_root(&db_bytes);
    ctrl.set_arena_with_root(&arena_path, new_size, current_root)
        .context("publish current_root (snapshot advance)")?;

    eprintln!(
        "snapshot to arena ({} bytes, root {})",
        db_bytes.len(),
        hex_short(&current_root),
    );
    Ok(())
}

/// One activation result, tagged by the target that produced it.
///
/// The two targets report genuinely different quantities — `nodes` counts
/// rows it populated, `source_blobs` additionally counts rows it declined to
/// chunk — so they cannot share a struct without one target's fields being
/// dead weight in the other's reports.
#[cfg(feature = "cdc")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CdcActivationReport {
    /// Construct-granular `nodes` activation.
    Nodes(leyline_fs::activation::ActivationReport),
    /// Whole-file `source_blobs` activation.
    SourceBlobs(leyline_fs::activation::BlobActivationReport),
}

/// Optionally activate CDC over the requested target, then atomically publish
/// the exact resulting database generation to the arena.
///
/// `target` is `None` when CDC is disabled. Carrying the target here rather
/// than a bare `enabled: bool` is the whole point of `ley-line-open-c3d746`:
/// the daemon previously hardcoded `activate_chunked_content`, so the
/// `--target source-blobs` selector existed only on the standalone
/// `leyline cdc enable` CLI and was unreachable from the daemon path that
/// mache actually spawns. A bool has nowhere to put the answer, so the
/// selector could not be plumbed without changing this signature.
///
/// Activation failure leaves the previously published root intact.
/// Successful activation and the disabled path both publish exactly once.
#[cfg(feature = "cdc")]
pub fn activate_cdc_and_snapshot(
    conn: &rusqlite::Connection,
    ctrl_path: &Path,
    target: Option<crate::CdcTarget>,
) -> Result<Option<CdcActivationReport>> {
    let options = leyline_fs::activation::ActivationOptions::default();
    let report = match target {
        Some(crate::CdcTarget::Nodes) => Some(CdcActivationReport::Nodes(
            leyline_fs::activation::activate_chunked_content(conn, options)
                .context("activate CDC `nodes` target in daemon living database")?,
        )),
        Some(crate::CdcTarget::SourceBlobs) => Some(CdcActivationReport::SourceBlobs(
            leyline_fs::activation::activate_chunked_source_blobs(conn, options)
                .context("activate CDC `source_blobs` target in daemon living database")?,
        )),
        None => None,
    };
    snapshot_to_arena(conn, ctrl_path)?;
    Ok(report)
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
    mut baseline_ready: Option<tokio::sync::oneshot::Sender<()>>,
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

        // The first successful HEAD + dirty-set sample is the watcher's
        // synchronization boundary. Production does not need a notification;
        // tests use it instead of guessing readiness with a fixed sleep.
        if let Some(ready) = baseline_ready.take() {
            let _ = ready.send(());
        }

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
            ctx.state.write().head_sha = Some(current_head.clone());
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

        // 3. Reconcile the checkout (bead `ley-line-open-60f39f`), scoped to
        //    the paths whose dirty state moved so we don't re-stat the
        //    entire source tree on every tick.
        let scope = watch_scope(head_changed, &last_dirty, &current_dirty);

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

        ctx.state.write().phase = DaemonPhase::Parsing;
        let lang = ctx.lang_filter.as_deref();
        let guard = ctx.live_db.writer.lock();
        // A materialized file changes no projected row, only the checkout
        // index, which the periodic snapshot publishes.
        let reconciled = crate::checkout::reconcile(&guard, source_dir, lang, scope.as_deref())
            .map(|reconciled| {
                if let Some(problems) = reconciled.problems() {
                    log::warn!("watch: checkout: {problems}");
                }
                reconciled.parse
            });
        match reconciled {
            Ok(None) => {
                drop(guard);
                ctx.state.write().phase = DaemonPhase::Ready;
            }
            Ok(Some(result)) => {
                if result.parsed > 0 || result.deleted > 0 {
                    eprintln!(
                        "watch: {} parsed, {} unchanged, {} deleted",
                        result.parsed, result.unchanged, result.deleted,
                    );
                    drop(guard);

                    // 4. Publish before the events below: a consumer that
                    // reads the arena on them sees the reparse.
                    if let Err(e) = publish_living_db(&ctx.live_db, &ctx.ctrl_path) {
                        log::error!("watch snapshot failed: {e:#}");
                    }

                    // 5. Update state + emit events.
                    {
                        let mut s = ctx.state.write();
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
                    ctx.state.write().phase = DaemonPhase::Ready;
                }
            }
            Err(e) => {
                log::error!("watch reparse failed: {e:#}");
                ctx.state.write().phase =
                    DaemonPhase::Error(format!("watch reparse failed: {e:#}"));
            }
        }
    }
}

/// The paths one watcher tick reconciles: the whole checkout when HEAD
/// moved, else every path whose git dirty state moved — dirty now, or dirty
/// at the last tick and clean now, since a revert changes the file too.
fn watch_scope(
    head_changed: bool,
    last: &std::collections::HashSet<String>,
    current: &std::collections::HashSet<String>,
) -> Option<Vec<String>> {
    (!head_changed).then(|| {
        last.union(current)
            .cloned()
            .collect::<std::collections::BTreeSet<String>>()
            .into_iter()
            .collect()
    })
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
    ctx.state.write().phase = DaemonPhase::Enriching;

    let scope: Option<&[String]> = if changed_files.is_empty() {
        None
    } else {
        Some(changed_files)
    };

    let started_ms = crate::daemon::now_ms();
    let guard = ctx.live_db.writer.lock();
    let outcome = crate::daemon::enrichment::run_all(
        &ctx.enrichment_passes,
        &guard,
        source_dir,
        scope,
        Some(&ctx.state),
    );
    drop(guard);

    ctx.state.write().phase = DaemonPhase::Ready;

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

            // Sheaf gap 3 (bead `ley-line-open-3b3476`) + follow-up
            // (bead `ley-line-open-e40566`): after the enrichment
            // cycle succeeds, emit the watcher-driven
            // `daemon.sheaf.invalidate` so consumers (mache's
            // `SheafSubscriber`) can evict stale region entries
            // without having to observe file changes themselves. Prior
            // to this wire the loop from source edit → region-precise
            // cascade was closed only from the consumer side
            // (`op_sheaf_invalidate` self-called). See
            // `docs/audits/sheaf-invalidation-trace.md` § Gap 3.
            //
            // Ordering: emitted AFTER `.complete` so subscribers to
            // both topics see enrichment finish before the cache
            // invalidate lands — matches the state-consistency
            // invariant "invalidate signals a fresh writable state".
            //
            // Payload mode: `scope: "changed-only"` when the daemon's
            // ComplexBuildPass has installed a region-label map (the
            // production path), otherwise `scope: "all-known"` — see
            // `emit_watcher_sheaf_invalidate` docs for the full
            // fallback contract.
            #[cfg(feature = "sheaf")]
            emit_watcher_sheaf_invalidate(ctx, changed_files, emitter);
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
            // Deliberately do NOT emit `daemon.sheaf.invalidate` on
            // the failure path — the sheaf cache state is unchanged
            // from the pre-enrichment view, so telling consumers to
            // evict would be a false signal. Next successful tick
            // will fire the invalidate.
        }
    }
}

/// Emit `daemon.sheaf.invalidate` from the watcher path after a
/// successful enrichment cycle. Closes sheaf gap 3
/// (bead `ley-line-open-3b3476`): the daemon drives region-precise
/// cache invalidation off its own file-change observations instead of
/// requiring the consumer to poll and call `op_sheaf_invalidate`.
///
/// # Region-set strategy — two modes
///
/// The `scope` field on the emitted payload tells the consumer which
/// mode this emit ran in:
///
/// **`scope: "changed-only"` — fine-grained diff.** When
/// [`crate::daemon::complex_build_pass::ComplexBuildPass`] has installed
/// a `region_id → token label` map on
/// [`crate::daemon::sheaf_ops::SheafState`], the emit computes the
/// subset of region IDs whose labels match `changed_files` — either as
/// a bare path token (`foo/bar.rs`) or as a `<path>:sym:<NAME>`
/// citation. Sheaf gap 3 follow-up (bead `ley-line-open-e40566`):
/// this replaces the coarse "invalidate every known region" fanout
/// with a diff that only names regions actually touched by the reparse.
/// The label map is derived from the daemon's own `observation` table
/// (see the pass's `build_complex` — token → id assignment) so no
/// consumer-side registration protocol is required (ADR-0026 Path A).
///
/// **`scope: "all-known"` — coarse-v1 fallback.** When no label map is
/// installed the daemon cannot compute a file→region diff, and emits
/// every currently-known region ID (from the persisted [`CellComplex`]).
/// Consumers treat this as "reparse touched something structural; play
/// it safe and invalidate all cached region entries." This is the
/// path when the daemon is fresh (no enrichment run yet) or when a
/// consumer pushed its own topology via `op_sheaf_set_topology` — mache
/// computes Louvain regions from Go-side signals the daemon can't
/// label, so the daemon can't diff them.
///
/// # Payload shape (JSON)
///
/// - `region_ids`: array of region IDs (u32). In `changed-only` mode:
///   only those regions whose labels match `changed_files`. In
///   `all-known` mode: every region ID in the current complex. Empty
///   when no complex has been installed yet — the event still fires
///   as a "state advanced" continuity signal.
/// - `count`: `region_ids.len()` — mirrors the existing consumer-
///   driven `sheaf.invalidate` payload's `count` field so subscribers
///   can use the same parse path.
/// - `scope`: `"changed-only"` (fine-grained diff computed) or
///   `"all-known"` (coarse fallback). See above.
/// - `changed_files`: files whose reparse + enrichment triggered this
///   invalidate.
/// - `current_root`: 64-char hex root from the substrate controller
///   (matches the wire format of every state-changing op response,
///   paired with mache's `mache-36d961` epic). Empty string if the
///   controller read fails — best-effort, degrades honestly.
/// - `generation`: quoted u64 — the cache generation after the emit.
///   Bumped via [`leyline_sheaf::SheafCache::bump_generation`] so
///   consumers see strict monotonicity across watcher-driven +
///   consumer-driven invalidates. Bumps ALWAYS fire, regardless of
///   scope — an empty region set in `changed-only` mode still moves
///   the counter forward so mache's freshness cursor stays monotonic.
/// - `prior_generation`: quoted u64 — the generation before the bump.
/// - `timestamp_ms`: quoted i64, `now_ms()` at emit time.
///
/// # Locking
///
/// Takes `ctx.sheaf.cache()` briefly to snapshot region IDs and bump
/// generation, plus (in `changed-only` mode) `ctx.sheaf` label state
/// via [`crate::daemon::sheaf_ops::SheafState::regions_touching_files`].
/// Both guards are dropped before opening the controller so slow disk
/// I/O on the ctrl path doesn't hold sheaf-side locks.
///
/// # Best-effort
///
/// Any failure inside this helper (currently only `read_root_hex` can
/// fail) logs at warn and degrades the payload field rather than
/// aborting the emit. The consumer still gets the region set +
/// generation advance.
///
/// Exposed to integration tests so the wire has a direct entry point
/// that doesn't require spinning up the full git-poll loop.
#[cfg(feature = "sheaf")]
pub fn emit_watcher_sheaf_invalidate(
    ctx: &Arc<crate::daemon::DaemonContext>,
    changed_files: &[String],
    _emitter: &crate::daemon::events::EventEmitter,
) {
    // Bead `ley-line-open-2191e1`: route watcher-driven emit through
    // the shared `SheafState::emit_invalidate` helper so watcher and
    // consumer paths share ONE payload contract. The `_emitter`
    // parameter is retained for API compatibility (integration tests
    // may want to observe emits without a live SheafState); the
    // actual emit goes through `state.emit` which uses the emitter
    // installed via `SheafState::set_emitter`.
    //
    // Step 1: try the fine-grained diff. Reads the label map first
    // (no cache lock held) so the cache critical section stays short.
    // Returns `Some(_)` when labels are installed and a complex is
    // present; `None` in either the coarse-fallback case (no labels)
    // or the degenerate "labels but no complex" case.
    let fine_grained: Option<Vec<u32>> = ctx.sheaf.regions_touching_files(changed_files);

    // Snapshot the full currently-known region set alongside the
    // fine-grained diff so ablation logging (bead `ley-line-open-2775a3`)
    // can compare `sheaf_count` vs `naive_count` for the sheaf value
    // study. `all_known` is what a naive "invalidate everything on any
    // file change" baseline would emit; we compute it under the same
    // cache lock so both sides are consistent to the moment the payload
    // is decided. Never sent on the wire — measurement only.
    let (invalidated, scope, prior_generation, generation, all_known) = {
        let mut cache = ctx.sheaf.cache().lock();
        let all_known: Vec<u32> = cache
            .complex()
            .map(|cx| cx.nodes.clone())
            .unwrap_or_default();
        let (regions, scope_tag) = match fine_grained {
            Some(diff) => (diff, "changed-only"),
            None => {
                // Coarse-v1 fallback: emit every currently-known region ID
                // from the installed complex. Empty when no complex has been
                // installed yet (fresh daemon) — the event still fires as a
                // "state advanced" continuity signal.
                (all_known.clone(), "all-known")
            }
        };
        let prior = cache.generation();
        // `gen` is a reserved keyword in Rust 2024 edition — use a
        // spelled-out binding to avoid the r#gen escape hatch.
        let bumped = cache.bump_generation();
        (regions, scope_tag, prior, bumped, all_known)
    };

    // Ablation instrumentation (opt-in via `LEYLINE_SHEAF_ABLATION_LOG`).
    // Zero-cost when the env var is unset. See
    // `daemon::sheaf_ablation` for the wire format + rationale.
    crate::daemon::sheaf_ablation::log_event(&invalidated, &all_known, changed_files, scope);

    let current_root = match crate::daemon::ops::read_root_hex(&ctx.ctrl_path) {
        Ok(hex) => hex,
        Err(e) => {
            log::warn!("emit_watcher_sheaf_invalidate: read_root_hex failed: {e:#}");
            String::new()
        }
    };

    // Route through the shared `SheafState::emit_invalidate` helper
    // (bead ley-line-open-2191e1) — consumer + watcher paths share
    // ONE payload contract; drift is impossible by construction.
    ctx.sheaf.emit_invalidate(
        invalidated,
        scope,
        changed_files,
        current_root,
        generation,
        prior_generation,
    );
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
    // is already handled by the file-stat diff downstream.
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
    use parking_lot::Mutex as StdMutex;
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
        live.lock().execute("INSERT INTO t VALUES (1)", []).unwrap();
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
        let _held_guard = live.lock();
        assert_eq!(read_total_changes(&live), None);
    }

    // The pre-parking_lot poison-return test lived here — deleted
    // when `live_db` swapped `std::sync::Mutex` → `parking_lot::Mutex`.
    // parking_lot doesn't poison, so `read_total_changes` returning
    // None specifically on poison is not expressible. The remaining
    // contract — `read_total_changes` returns None when the lock is
    // contended (uncontended → Some(counter)) — is covered by
    // `the_publisher_skips_a_wake_while_the_writer_is_busy` below.

    #[test]
    fn the_publisher_skips_a_wake_while_the_writer_is_busy() {
        // 5fea4e contract: while the writer is held (op_reparse across
        // parse_into_conn, a proposal mid-commit), the publisher's dirty
        // check cannot read the counter; it returns at once without
        // publishing, and its next wake retries. A blocking lock here
        // would hang this test, which holds the writer on its own thread.
        let dir = TempDir::new().unwrap();
        let (live, ctrl_path) = publishing_live_db(&dir);
        let _held_guard = live.writer.lock();
        let mut last = 0;
        assert!(!publish_if_dirty(&live, &ctrl_path, &mut last).unwrap());
        assert_eq!(last, 0, "nothing recorded as published");
    }

    // The pre-parking_lot poison-recovery test lived here — deleted
    // when the daemon swapped `std::sync::Mutex` → `parking_lot::Mutex`
    // (which never poisons by design). The recovery contract it was
    // pinning is no longer expressible: `parking_lot::Mutex::lock()`
    // returns `MutexGuard` directly, so a panic-during-lock leaves no
    // Err state to recover from — the next lock just proceeds. The
    // "acquire when uncontended, skip when contended" contract that
    // matters is still pinned by
    // `the_publisher_skips_a_wake_while_the_writer_is_busy` above.

    #[test]
    fn warm_start_returns_none_on_missing_ctrl() {
        let dir = TempDir::new().unwrap();
        let ctrl = dir.path().join("nonexistent.ctrl");
        let live_db = live_db_path_for(&ctrl);
        // No file exists yet — Controller::open_or_create will create one,
        // but it'll be empty (no arena_path). Should fall through cleanly.
        let result = try_warm_start_from_arena(&ctrl, &live_db).unwrap();
        assert!(result.is_none(), "missing-ctrl path should return None");
    }

    #[test]
    fn warm_start_returns_none_on_corrupted_ctrl() {
        // Garbage ctrl: try_warm_start_from_arena MUST NOT panic and
        // MUST return None so cold start remains a safe fallback. The
        // fix (5f7100-6) logs a warn alongside the None — captured
        // behavior is unchanged from the caller's perspective; the new
        // visibility is in the log.
        let dir = TempDir::new().unwrap();
        let ctrl = dir.path().join("corrupt.ctrl");
        let live_db = live_db_path_for(&ctrl);
        std::fs::write(&ctrl, b"\x00\x01\x02 not a valid controller \xff\xfe").unwrap();

        let result = try_warm_start_from_arena(&ctrl, &live_db);
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
        // by the file-stat diff during reparse.)
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
                "-c",
                "core.hooksPath=/dev/null",
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
        // The old path's removal is handled by the file-stat diff
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
                "-c",
                "core.hooksPath=/dev/null",
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
                "-c",
                "core.hooksPath=/dev/null",
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
        let live_db = live_db_path_for(&ctrl);
        let mut c = leyline_core::Controller::open_or_create(&ctrl).unwrap();
        c.set_arena("/tmp/cloister-no-such-arena-xyzzy", 1024 * 1024)
            .unwrap();
        drop(c);

        let result = try_warm_start_from_arena(&ctrl, &live_db).unwrap();
        assert!(result.is_none(), "missing-arena path should return None");
    }

    // ── 15a: file-backed WAL live_db invariants ─────────────────────────
    //
    // Bead `ley-line-open-98fb67` sub-bead 15a: the daemon's living db
    // must be file-backed with WAL journaling. These tests pin the
    // load-bearing invariants at the helper level; the integration
    // suite (tests/wal_live_db.rs) exercises them via a full daemon
    // spawn.

    #[test]
    fn live_db_path_derives_from_ctrl_path() {
        // Sibling-of-ctrl naming keeps arena / ctrl / sock / lock /
        // live.db co-located so `--arena` alone locates every artifact.
        let path = Path::new("/tmp/foo.ctrl");
        assert_eq!(
            live_db_path_for(path),
            std::path::PathBuf::from("/tmp/foo.live.db"),
        );
    }

    /// Bead `ley-line-open-98d0c3`: the cost of one accepted proposal and
    /// its write-through to the checkout, on the daemon's `live.db`
    /// configuration, without a VM or a socket in the way. Prints the
    /// median and mean over `LEYLINE_BENCH_PROPOSALS` (default 100) one-file
    /// proposals into a `LEYLINE_BENCH_FILES` (default 2,000) file tree.
    #[test]
    #[ignore = "a benchmark: run with --ignored --nocapture"]
    fn bench_one_proposal_and_its_checkout_write() {
        use crate::source_write::{Base, Grant, Proposed, propose};
        let files: usize = std::env::var("LEYLINE_BENCH_FILES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2_000);
        let proposals: usize = std::env::var("LEYLINE_BENCH_PROPOSALS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100);
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src");
        for i in 0..files {
            let path = src.join(format!("d{}/f{i}.go", i % 50));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                path,
                format!("package d\n\nfunc F{i}() int {{ return {i} }}\n"),
            )
            .unwrap();
        }
        let conn = rusqlite::Connection::open(dir.path().join("bench.live.db")).unwrap();
        configure_wal(&conn).unwrap();
        crate::checkout::reconcile(&conn, &src, None, None).unwrap();
        let mut took = Vec::with_capacity(proposals);
        for n in 0..proposals {
            let rel = format!("d{}/f{n}.go", n % 50);
            let bytes = format!("package d\n\nfunc F{n}() int {{ return -{n} }}\n").into_bytes();
            let start = std::time::Instant::now();
            propose(
                &conn,
                &Grant::Whole,
                vec![Proposed::write(&rel, Base::Any, bytes)],
            )
            .unwrap();
            let r = crate::checkout::reconcile(&conn, &src, None, Some(&[rel])).unwrap();
            took.push(start.elapsed());
            assert_eq!(r.materialized.len(), 1);
        }
        took.sort();
        let mean = took.iter().sum::<std::time::Duration>() / took.len() as u32;
        println!(
            "proposal+checkout over {files} files, {proposals} proposals: median {:.2} ms, mean {:.2} ms",
            took[took.len() / 2].as_secs_f64() * 1e3,
            mean.as_secs_f64() * 1e3
        );
    }

    #[test]
    fn configure_wal_activates_wal_on_file_backed_connection() {
        // The 15a load-bearing check: PRAGMA journal_mode=WAL must
        // return "wal" on a file-backed connection. A regression here
        // (`:memory:` sneaking back in, forgotten pragma, etc.) is
        // exactly the failure mode the empirical report warned about.
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("wal_test.live.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        configure_wal(&conn).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal", "journal_mode must stick as WAL");
        let synchronous: i64 = conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap();
        // synchronous=FULL is the numeric value 2: every COMMIT syncs the
        // WAL before it returns, so a write acknowledged to a mount client
        // at fsync or NFS COMMIT is on disk, not only in the page cache
        // (bead ley-line-open-0a9931).
        assert_eq!(synchronous, 2, "synchronous must be FULL (=2)");
        // The commit is a proposal's acknowledgement and its one full
        // flush: F_FULLFSYNC on macOS (bead ley-line-open-98d0c3).
        for pragma in ["fullfsync", "checkpoint_fullfsync"] {
            let on: i64 = conn
                .query_row(&format!("PRAGMA {pragma}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(on, 1, "{pragma} must be ON");
        }
        let autocheckpoint: i64 = conn
            .query_row("PRAGMA wal_autocheckpoint", [], |r| r.get(0))
            .unwrap();
        assert_eq!(autocheckpoint, 1000, "wal_autocheckpoint must be 1000");
    }

    #[test]
    fn configure_wal_bails_on_in_memory_connection() {
        // `:memory:` silently ignores journal_mode=WAL (returns "memory").
        // The empirical report documented this footgun at length; the
        // helper MUST refuse to proceed rather than silently no-op.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let err = configure_wal(&conn).expect_err(":memory: must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("did not stick"),
            "error must name the failure mode; got: {msg}",
        );
    }

    #[test]
    fn unlink_live_db_removes_sidecars() {
        // WAL leaves `-wal` and `-shm` sidecars next to the main db.
        // The cold-start reset must clear all three; leaving `-wal`
        // behind lets SQLite replay a stale write-ahead log against
        // the next fresh db file at the same path.
        let dir = TempDir::new().unwrap();
        let live_db = dir.path().join("cleanup.live.db");
        // Seed all three files.
        std::fs::write(&live_db, b"stub main db").unwrap();
        std::fs::write(dir.path().join("cleanup.live.db-wal"), b"stub wal").unwrap();
        std::fs::write(dir.path().join("cleanup.live.db-shm"), b"stub shm").unwrap();

        unlink_live_db(&live_db);

        assert!(!live_db.exists(), "main db must be gone");
        assert!(
            !dir.path().join("cleanup.live.db-wal").exists(),
            "-wal sidecar must be gone (stale WAL replay = corruption)",
        );
        assert!(
            !dir.path().join("cleanup.live.db-shm").exists(),
            "-shm sidecar must be gone",
        );
    }

    #[test]
    fn init_living_db_cold_start_creates_wal_file() {
        // Pure cold start: no ctrl file, no live.db, no source. Should
        // create the live.db and activate WAL.
        let dir = TempDir::new().unwrap();
        let ctrl = dir.path().join("cold.ctrl");
        let live_db = live_db_path_for(&ctrl);
        assert!(!live_db.exists(), "pre-condition: live.db must not exist");

        let conn = init_living_db(&ctrl, &live_db, None, None, false).unwrap();
        assert!(live_db.exists(), "cold start must create live.db");
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            mode.to_lowercase(),
            "wal",
            "cold-start connection must be in WAL mode",
        );
    }

    #[test]
    fn init_living_db_cold_start_unlinks_stale_live_db_when_ctrl_is_fresh() {
        // Fresh controller + stale live.db (from a prior experiment):
        // the file must be unlinked so cold-start writes to a clean db.
        let dir = TempDir::new().unwrap();
        let ctrl = dir.path().join("stale.ctrl");
        let live_db = live_db_path_for(&ctrl);
        // Seed a stale file with sentinel content.
        std::fs::write(&live_db, b"stale content from an earlier run").unwrap();
        let stale_len_before = std::fs::metadata(&live_db).unwrap().len();

        let _conn = init_living_db(&ctrl, &live_db, None, None, false).unwrap();
        let fresh_len = std::fs::metadata(&live_db).unwrap().len();
        assert_ne!(
            fresh_len, stale_len_before,
            "stale file must be replaced (bytes should differ from the stub)",
        );
        // The new file must be a valid SQLite db — sqlite header magic
        // string starts with "SQLite format 3\0".
        let bytes = std::fs::read(&live_db).unwrap();
        assert!(
            bytes.starts_with(b"SQLite format 3\0"),
            "cold-start live.db must be a valid SQLite database file",
        );
    }

    #[test]
    fn init_living_db_warm_start_reopens_existing_live_db() {
        // Set up: cold-start the daemon once, insert a sentinel row,
        // close. Then re-init — must NOT unlink (controller is fresh
        // when there's no snapshot, so we simulate the "warm" path by
        // running with a controller that has a non-zero root, per the
        // controller_is_fresh check). Approximation: skip snapshot,
        // just verify the reopen preserves state under the fresh
        // controller check-and-unlink path. This test uses the actual
        // conditional: if the file exists AND controller is not fresh,
        // reopen; otherwise unlink first.
        //
        // To simulate a non-fresh controller, we set an arena_root
        // via test-only Controller helper. If none exists (they do —
        // set_arena_with_root), we skip. The whole flow is exercised
        // end-to-end in the integration test.
        let dir = TempDir::new().unwrap();
        let ctrl_path = dir.path().join("warm.ctrl");
        let live_db = live_db_path_for(&ctrl_path);

        // Cold start creates live.db with WAL.
        {
            let conn = init_living_db(&ctrl_path, &live_db, None, None, false).unwrap();
            conn.execute_batch(
                "CREATE TABLE sentinel (id INTEGER PRIMARY KEY);
                 INSERT INTO sentinel VALUES (42);",
            )
            .unwrap();
            drop(conn);
        }

        // Fake "warm restart" by publishing a non-zero root into ctrl,
        // then re-init. The published root makes controller_is_fresh()
        // return false, so the stale-file unlink is skipped and the
        // existing live.db is reopened.
        {
            let mut c = leyline_core::Controller::open_or_create(&ctrl_path).unwrap();
            // Register a dummy arena path so set_arena_with_root has a
            // valid target; then advance the root off zero.
            let fake_arena = dir.path().join("warm.arena");
            let _ = leyline_core::create_arena(&fake_arena, 64 * 1024).unwrap();
            let mut root = [0u8; 32];
            root[0] = 0xAB;
            c.set_arena_with_root(&fake_arena.to_string_lossy(), 64 * 1024, root)
                .unwrap();
            drop(c);
        }

        let conn = init_living_db(&ctrl_path, &live_db, None, None, false).unwrap();
        let sentinel: i64 = conn
            .query_row("SELECT id FROM sentinel", [], |r| r.get(0))
            .expect("warm start must preserve sentinel row");
        assert_eq!(sentinel, 42, "warm start must reopen existing live.db");
    }

    #[test]
    fn a_serve_only_warm_start_refuses_a_pre_v5_live_db() {
        // Publication-audit F1 (bead `ley-line-open-17c271`): the pre-v5
        // refusal gate lived only inside the parse path, and
        // run_initial_parse is gated on `--source`. A serve-only daemon
        // (source = None) warm-starting over a v4 live db therefore never
        // hit the gate — it published the v4 image to the arena and served
        // ops that each died with per-op "no such column: nid" noise
        // instead of the one actionable refusal at open.
        let dir = TempDir::new().unwrap();
        let ctrl_path = dir.path().join("v4warm.ctrl");
        let live_db = live_db_path_for(&ctrl_path);

        // Cold start, then shape the live db as a v4 projection: the
        // version label, the `_source` probe target, and the
        // TEXT-keyed `nodes` (no `nid` column).
        {
            let conn = init_living_db(&ctrl_path, &live_db, None, None, false).unwrap();
            conn.execute_batch(
                "CREATE TABLE _meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO _meta VALUES ('projection_schema_version', 'projection-v4');
                 CREATE TABLE _source (id TEXT PRIMARY KEY, language TEXT, path TEXT);
                 CREATE TABLE nodes (id TEXT PRIMARY KEY, name TEXT, kind INTEGER);",
            )
            .unwrap();
            drop(conn);
        }

        // Make the controller non-fresh so re-init takes the warm path.
        {
            let mut c = leyline_core::Controller::open_or_create(&ctrl_path).unwrap();
            let fake_arena = dir.path().join("v4warm.arena");
            let _ = leyline_core::create_arena(&fake_arena, 64 * 1024).unwrap();
            let mut root = [0u8; 32];
            root[0] = 0xCD;
            c.set_arena_with_root(&fake_arena.to_string_lossy(), 64 * 1024, root)
                .unwrap();
            drop(c);
        }

        // Serve-only warm start (source = None) must refuse at open.
        let err = init_living_db(&ctrl_path, &live_db, None, None, false)
            .expect_err("a serve-only warm start over a v4 live db must refuse at open");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("projection-v4") || msg.contains("nid"),
            "the refusal must name the version mismatch, got: {msg}",
        );
    }

    #[test]
    fn a_mount_this_build_cannot_provide_is_refused() {
        let at = Some(Path::new("/mnt/x"));
        assert!(refuse_unavailable_mount(None, false).is_ok(), "headless");
        assert!(refuse_unavailable_mount(None, true).is_ok(), "headless");
        assert!(refuse_unavailable_mount(at, true).is_ok(), "a mount build");
        let err = refuse_unavailable_mount(at, false).unwrap_err().to_string();
        assert!(err.contains("`mount` feature"), "{err}");
        assert!(err.contains("/mnt/x"), "{err}");
    }

    /// `--arena-size-mib` is mebibytes.
    #[test]
    fn the_arena_size_is_mebibytes() {
        assert_eq!(arena_bytes(1), 1_048_576);
        assert_eq!(arena_bytes(64), 67_108_864);
    }

    /// Two [`HostMountRef`]s are equal when they hold the same provider, and
    /// a provider prints by name, so `DaemonOptions` stays `Debug` and `Eq`.
    #[test]
    fn host_mount_refs_compare_by_provider() {
        struct Never;
        impl HostMount for Never {
            fn mount<'a>(
                &'a self,
                _ctx: Arc<crate::daemon::DaemonContext>,
                _at: &'a Path,
                _backend: &'a str,
                _nfs_port: u16,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<MountGuard>> + Send + 'a>>
            {
                Box::pin(async { anyhow::bail!("never mounts") })
            }
        }
        let one = HostMountRef(Arc::new(Never));
        let same = one.clone();
        let other = HostMountRef(Arc::new(Never));
        assert_eq!(one, same);
        assert_ne!(one, other);
        assert_eq!(format!("{one:?}"), "HostMountRef");
    }

    #[test]
    fn only_a_wal_stamped_sqlite_image_is_restamped() {
        let header = |w: u8, r: u8| {
            let mut h = b"SQLite format 3\0".to_vec();
            h.extend_from_slice(&[0x10, 0x00, w, r, 0, 64, 32, 32]);
            h
        };
        assert_eq!(rollback_image(header(2, 2))[18..20], [1, 1]);
        assert_eq!(rollback_image(header(1, 1)), header(1, 1));
        assert_eq!(rollback_image(header(2, 1)), header(2, 1), "a mixed stamp");
        let raw = b"not a database, though byte 18 is \x02\x02 here".to_vec();
        assert_eq!(rollback_image(raw.clone()), raw, "other bytes as they are");
        let mut lookalike = header(2, 2);
        lookalike[0] = b'X';
        assert_eq!(rollback_image(lookalike.clone()), lookalike);
    }

    #[test]
    fn a_published_image_opens_in_an_in_memory_reader() {
        // Bead ley-line-open-2bd77c: the live db is WAL-mode, and its
        // serialized image carried the WAL stamp (header bytes 18/19 = 2).
        // sqlite3_deserialize of a WAL-stamped image fails every query with
        // SQLITE_CANTOPEN, so no in-memory reader could open a published
        // arena. The published image is a standalone rollback-mode database.
        let dir = TempDir::new().unwrap();
        let arena_path = dir.path().join("img.arena");
        let ctrl_path = dir.path().join("img.ctrl");
        let live_db_path = live_db_path_for(&ctrl_path);
        let _mmap = leyline_core::create_arena(&arena_path, 2 * 1024 * 1024).unwrap();
        leyline_core::Controller::open_or_create(&ctrl_path)
            .unwrap()
            .set_arena(&arena_path.to_string_lossy(), 2 * 1024 * 1024)
            .unwrap();
        let conn = init_living_db(&ctrl_path, &live_db_path, None, None, false).unwrap();
        conn.execute_batch(
            "CREATE TABLE img_rows (id INTEGER PRIMARY KEY, payload TEXT);
             INSERT INTO img_rows (payload) VALUES ('one'), ('two');",
        )
        .unwrap();

        snapshot_to_arena(&conn, &ctrl_path).unwrap();

        let graph = leyline_fs::SqliteGraph::from_arena(&ctrl_path).unwrap();
        let rows: i64 = graph
            .conn()
            .query_row("SELECT count(*) FROM img_rows", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 2);
        let live_mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(live_mode.to_lowercase(), "wal", "the live db stays WAL");
    }

    #[test]
    fn snapshot_to_arena_works_with_wal_backed_connection() {
        // Load-bearing 15a invariant (per bead ley-line-open-98fb67):
        // `snapshot_to_arena` calls `conn.serialize("main")` on the
        // live db. The empirical report warned that `serialize()` may
        // require an exclusive write txn — if WAL breaks that,
        // snapshots break, and the whole substrate publish path
        // silently regresses.
        //
        // Verify: create a WAL-mode file-backed connection, insert
        // rows, snapshot to arena, verify the arena current_root
        // advances to a non-zero fingerprint of the serialized bytes.
        let dir = TempDir::new().unwrap();
        let arena_path = dir.path().join("snap.arena");
        let ctrl_path = dir.path().join("snap.ctrl");
        let live_db_path = live_db_path_for(&ctrl_path);

        // Fresh arena + controller registration (mirrors setup_arena).
        let _mmap = leyline_core::create_arena(&arena_path, 2 * 1024 * 1024).unwrap();
        {
            let mut c = leyline_core::Controller::open_or_create(&ctrl_path).unwrap();
            c.set_arena(&arena_path.to_string_lossy(), 2 * 1024 * 1024)
                .unwrap();
        }

        // File-backed WAL live db + rows to make the snapshot non-trivial.
        let conn = init_living_db(&ctrl_path, &live_db_path, None, None, false).unwrap();
        conn.execute_batch(
            "CREATE TABLE snap_rows (id INTEGER PRIMARY KEY, payload TEXT);
             INSERT INTO snap_rows (payload) VALUES ('one'), ('two'), ('three');",
        )
        .unwrap();

        // Pre: current_root is the zero sentinel.
        let root_before = {
            let c = leyline_core::Controller::open_or_create(&ctrl_path).unwrap();
            c.current_root()
        };
        assert_eq!(root_before, [0u8; 32], "pre-snapshot root must be zero");

        // The load-bearing call — this is what would blow up if
        // serialize() + WAL didn't compose.
        snapshot_to_arena(&conn, &ctrl_path).expect("snapshot_to_arena on WAL'd db");

        // Post: current_root must have advanced off zero.
        let root_after = {
            let c = leyline_core::Controller::open_or_create(&ctrl_path).unwrap();
            c.current_root()
        };
        assert_ne!(
            root_after, [0u8; 32],
            "snapshot must publish a non-zero root"
        );

        // And the live db is still queryable — snapshot didn't close/poison it.
        let count: i64 = conn
            .query_row("SELECT count(*) FROM snap_rows", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3, "live db must remain queryable after snapshot");
    }

    /// `ley-line-open-5a0f07`: growing the arena must not touch the
    /// published image. Buffer offsets derive from the file length
    /// (`ArenaHeader::buffer_size`), so growing the file in place while
    /// buffer B is active moved B's offset and the pending write into A
    /// overwrote the image a reader had already verified, before the new
    /// root published. A reader holding the arena open across a growing
    /// snapshot must still read the bytes it verified.
    #[test]
    fn a_growing_snapshot_leaves_the_published_image_intact() {
        use std::os::unix::fs::FileExt;

        let dir = TempDir::new().unwrap();
        let arena_path = dir.path().join("grow.arena");
        let ctrl_path = dir.path().join("grow.ctrl");
        let live_db_path = live_db_path_for(&ctrl_path);
        let small = 2 * 1024 * 1024;
        let _mmap = leyline_core::create_arena(&arena_path, small).unwrap();
        leyline_core::Controller::open_or_create(&ctrl_path)
            .unwrap()
            .set_arena(&arena_path.to_string_lossy(), small)
            .unwrap();

        let conn = init_living_db(&ctrl_path, &live_db_path, None, None, false).unwrap();
        conn.execute_batch(
            "CREATE TABLE grow_rows (id INTEGER PRIMARY KEY, payload BLOB);
             INSERT INTO grow_rows (payload) VALUES (x'01');",
        )
        .unwrap();
        snapshot_to_arena(&conn, &ctrl_path).unwrap();

        // The first snapshot lands in buffer B: the case that moves.
        let published_root = leyline_core::Controller::open_or_create(&ctrl_path)
            .unwrap()
            .current_root();
        let reader = std::fs::File::open(&arena_path).unwrap();
        let mut header_bytes = [0u8; std::mem::size_of::<leyline_core::ArenaHeader>()];
        reader.read_exact_at(&mut header_bytes, 0).unwrap();
        let header: leyline_core::ArenaHeader = *bytemuck::from_bytes(&header_bytes);
        assert_eq!(
            header.active_buffer, 1,
            "the published image must sit in buffer B"
        );
        let offset = header.validate_header(small).unwrap();

        // Enough rows that the next snapshot must grow the arena.
        conn.execute(
            "INSERT INTO grow_rows (payload) SELECT zeroblob(4096) FROM
             (WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 600)
              SELECT i FROM n)",
            [],
        )
        .unwrap();
        snapshot_to_arena(&conn, &ctrl_path).unwrap();
        let grown = leyline_core::Controller::open_or_create(&ctrl_path).unwrap();
        assert!(
            grown.arena_size() > small,
            "the second snapshot must grow the arena"
        );
        // The grown arena is sized from the image it holds: two buffers of
        // the image plus a quarter, and the file is exactly that long (bead
        // `ley-line-open-8c8ded`: a sizing bug must fail here, not fill the
        // disk).
        let grown_file = std::fs::File::open(&arena_path).unwrap();
        let mut grown_header = [0u8; std::mem::size_of::<leyline_core::ArenaHeader>()];
        grown_file.read_exact_at(&mut grown_header, 0).unwrap();
        let grown_header: leyline_core::ArenaHeader = *bytemuck::from_bytes(&grown_header);
        let image_len = grown_header.data_size;
        assert_eq!(
            grown.arena_size(),
            leyline_core::ArenaHeader::HEADER_SIZE
                + 2 * std::cmp::max(image_len + image_len / 4, 1024 * 1024),
            "the grown arena is the header and two buffers of the image plus a quarter"
        );
        assert_eq!(grown_file.metadata().unwrap().len(), grown.arena_size());
        assert_ne!(grown.current_root(), published_root);

        let mut image = vec![0u8; header.data_size as usize];
        reader.read_exact_at(&mut image, offset).unwrap();
        assert_eq!(
            *blake3::hash(&image).as_bytes(),
            published_root,
            "a reader that opened the arena before the growth must still read the image it verified",
        );

        // And a fresh reader opens the grown arena under the new root.
        leyline_fs::SqliteGraph::from_arena(&ctrl_path).expect("open the grown arena");
    }

    /// `ley-line-open-e37e03`, filed by mache: `--reset-arena` ALWAYS
    /// failed, including on a completely fresh arena with no prior
    /// state, because the reset zeroed the arena PATH along with the
    /// root — but `lifecycle::setup_arena` (this function's real
    /// caller, always run first) had already stamped the path, and
    /// nothing re-stamps it. The next `snapshot_to_arena` then opens
    /// an empty path and fails with "open arena file".
    ///
    /// This is case (a) of the bead's acceptance criteria — reproduces
    /// the exact reported repro (real `setup_arena`, `--reset-arena`,
    /// through to a completed snapshot) rather than a paraphrase of
    /// it, since the bug is specifically about what THAT function
    /// stamps and THIS one blanks.
    #[test]
    fn reset_arena_reaches_a_completed_snapshot_on_a_fresh_arena() {
        let dir = TempDir::new().unwrap();
        let arena_path = dir.path().join("reset.arena");
        let ctrl_path = dir.path().join("reset.ctrl");
        let live_db_path = live_db_path_for(&ctrl_path);

        // The real caller, not a hand-rolled mimic — the bug is specifically
        // about what THIS stamps and the reset then blanks.
        let ctrl_path_returned =
            lifecycle::setup_arena(&arena_path, 2 * 1024 * 1024, Some(&ctrl_path))
                .expect("setup_arena on a completely fresh arena");
        assert_eq!(ctrl_path_returned, ctrl_path);

        let conn = init_living_db(&ctrl_path, &live_db_path, None, None, true)
            .expect("--reset-arena must succeed on a fresh arena — this is the reported failure");
        conn.execute_batch(
            "CREATE TABLE reset_rows (id INTEGER PRIMARY KEY, payload TEXT);
             INSERT INTO reset_rows (payload) VALUES ('after-reset');",
        )
        .unwrap();

        // The exact failure site: `snapshot_to_arena` reading back
        // whatever path the reset left behind.
        snapshot_to_arena(&conn, &ctrl_path)
            .expect("snapshot_to_arena after --reset-arena on a fresh arena");

        let c = leyline_core::Controller::open_or_create(&ctrl_path).unwrap();
        assert_ne!(
            c.current_root(),
            [0u8; 32],
            "snapshot must publish a non-zero root after reset"
        );
        assert!(
            !c.arena_path().is_empty(),
            "the arena path must survive the reset — this is the actual bug"
        );
    }

    /// Case (b) of `ley-line-open-e37e03`'s acceptance criteria: the
    /// PRESCRIBED remedy for a cross-source refusal must itself work.
    /// `verify_source_root_matches` tells an operator verbatim to pass
    /// `--reset-arena` when a shared arena was previously parsed
    /// against a different `--source` — so a daemon that starts
    /// against repo B with `--reset-arena` after a prior run against
    /// repo A must reach ready, not hit the same "open arena file"
    /// this bead is about.
    #[test]
    fn reset_arena_reaches_a_completed_snapshot_across_a_source_switch() {
        let dir = TempDir::new().unwrap();
        let arena_path = dir.path().join("switch.arena");
        let ctrl_path = dir.path().join("switch.ctrl");
        let live_db_path = live_db_path_for(&ctrl_path);

        let repo_a = dir.path().join("repo_a");
        std::fs::create_dir(&repo_a).unwrap();
        std::fs::write(repo_a.join("a.json"), "{\"k\":\"a\"}\n").unwrap();
        let repo_b = dir.path().join("repo_b");
        std::fs::create_dir(&repo_b).unwrap();
        std::fs::write(repo_b.join("b.json"), "{\"k\":\"b\"}\n").unwrap();

        // First daemon: cold start against repo A, snapshot it — this is
        // the prior state a shared arena (e.g. `~/.mache/default.arena`)
        // carries into the next run.
        lifecycle::setup_arena(&arena_path, 2 * 1024 * 1024, Some(&ctrl_path))
            .expect("setup_arena for repo A");
        let conn_a = init_living_db(
            &ctrl_path,
            &live_db_path,
            Some(&repo_a),
            Some("json"),
            false,
        )
        .expect("cold start against repo A");
        snapshot_to_arena(&conn_a, &ctrl_path).expect("snapshot repo A's parse");
        drop(conn_a);

        // Confirm the refusal this bead's remedy exists for actually
        // fires, so the test proves the REMEDY works — not that the
        // refusal never triggered in the first place.
        let refused = init_living_db(
            &ctrl_path,
            &live_db_path,
            Some(&repo_b),
            Some("json"),
            false,
        )
        .expect_err("warm start against a different --source must be refused");
        assert!(
            format!("{refused:#}").contains("warm start"),
            "must fail via verify_source_root_matches, not some other error: {refused:#}"
        );

        // The prescribed remedy: --reset-arena, now against repo B.
        let conn_b = init_living_db(&ctrl_path, &live_db_path, Some(&repo_b), Some("json"), true)
            .expect("--reset-arena is the documented remedy for a source switch — it must work");
        snapshot_to_arena(&conn_b, &ctrl_path)
            .expect("snapshot_to_arena after reset across a source switch");

        let row_count: i64 = conn_b
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert!(row_count > 0, "repo B must have actually been parsed");
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
                .push((old_sha.to_string(), new_sha.to_string()));
        }
        fn on_files_changed(&self, paths: &[String]) {
            self.file_changes.lock().push(paths.to_vec());
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
        // The fixture's git runs no developer hooks (bead ley-line-open-d1697b).
        sh(
            dir.path(),
            &["git", "config", "core.hooksPath", "/dev/null"],
        );
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

        // Test fixture uses a file-backed live db so the reader pool
        // can attach to it (bead `ley-line-open-f0239d`). `:memory:`
        // would work for the writer alone but can't be shared with
        // pool-owned reader connections.
        let live_db_path = live_db_path_for(ctrl_path);
        let writer = rusqlite::Connection::open(&live_db_path).unwrap();
        configure_wal(&writer).unwrap();
        context_over(ctrl_path, ext, source, writer)
    }

    /// An execution handler with nothing behind it.
    struct NoExecution;

    impl crate::daemon::execution::ExecutionHandler for NoExecution {
        fn capabilities(&self) -> Result<String> {
            Ok(String::new())
        }
        fn provision(&self, _: &serde_json::Value) -> Result<String> {
            Ok(String::new())
        }
        fn start(&self, _: &serde_json::Value) -> Result<String> {
            Ok(String::new())
        }
        fn status(&self, _: &serde_json::Value) -> Result<String> {
            Ok(String::new())
        }
        fn inspect(&self, _: &serde_json::Value) -> Result<String> {
            Ok(String::new())
        }
        fn collect(&self, _: &serde_json::Value) -> Result<String> {
            Ok(String::new())
        }
        fn cleanup(&self, _: &serde_json::Value) -> Result<String> {
            Ok(String::new())
        }
        fn cancel(&self, _: &serde_json::Value) -> Result<String> {
            Ok(String::new())
        }
    }

    /// A run's workspace share is served from the daemon's own context: the
    /// execution extension hands the context it is given to the slot its
    /// embedder holds, once.
    #[tokio::test]
    async fn the_execution_extension_publishes_the_daemon_context_to_its_slot() {
        let dir = TempDir::new().unwrap();
        let slot = crate::daemon::ContextSlot::default();
        let ext = crate::daemon::ExecutionDaemonExt::new(Arc::new(NoExecution))
            .with_context_slot(Arc::clone(&slot));
        let ctx = test_ctx(&dir.path().join("slot.ctrl"), Arc::new(NoExt), dir.path());
        assert!(slot.get().is_none());
        ext.on_context(&ctx);
        assert!(Arc::ptr_eq(slot.get().expect("published"), &ctx));

        // Without a slot the hook does nothing (and the trait default too).
        crate::daemon::ExecutionDaemonExt::new(Arc::new(NoExecution)).on_context(&ctx);
        NoExt.on_context(&ctx);
    }

    /// A daemon context over `writer`, the live db's writer connection at
    /// `live_db_path_for(ctrl_path)`, whose checkout is `source`.
    fn context_over(
        ctrl_path: &Path,
        ext: Arc<dyn DaemonExt>,
        source: &Path,
        writer: rusqlite::Connection,
    ) -> Arc<DaemonContext> {
        let live_db_path = live_db_path_for(ctrl_path);
        let live_db = crate::daemon::db_pool::LiveDb::new(writer, &live_db_path, 4).unwrap();
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
            live_db,
            enrich_inflight: Arc::new(parking_lot::Mutex::new(std::collections::HashSet::new())),
            source_dir: Some(source.to_path_buf()),
            lang_filter: Some("go".to_string()),
            enrichment_passes: vec![Box::new(crate::daemon::enrichment::TreeSitterPass)],
            state: Arc::new(parking_lot::RwLock::new(DaemonState::initializing())),
            #[cfg(feature = "vec")]
            vec_index,
            #[cfg(feature = "vec")]
            embedder,
            #[cfg(feature = "vec")]
            embed_queue: Arc::new(parking_lot::Mutex::new(std::collections::BinaryHeap::new())),
            #[cfg(feature = "text-search")]
            text_search: Arc::new(leyline_text_search::null::NullEngine::new()),
            sheaf: Arc::new(crate::daemon::sheaf_ops::SheafState::new()),
        })
    }

    // ── The checkout across restarts (bead `ley-line-open-60f39f`) ─────
    //
    // Each test runs daemon lifetimes through the startup path a real
    // daemon runs: `setup_arena`, `init_living_db` (cold, then warm), and
    // the first publish. Dropping a lifetime's context is a kill: nothing
    // runs at exit, so what the next start sees is what was committed.

    const CHECKOUT_A: &str = "package a\n\nfunc A() int { return 1 }\n";
    const CHECKOUT_PROPOSED: &str = "package a\n\nfunc A() int { return 2 }\n";
    const CHECKOUT_EDITED: &str = "package a\n\nfunc A() int { return 3 }\n";
    const CHECKOUT_B: &str = "package a\n\nfunc B() string { return \"b\" }\n";

    /// A source directory holding `a.go` and `b.go`, and a state directory
    /// for the arena, controller and live db.
    fn checkout_fixture() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(src.join("a.go"), CHECKOUT_A).unwrap();
        std::fs::write(src.join("b.go"), CHECKOUT_B).unwrap();
        (dir, src, state)
    }

    /// One daemon lifetime over `src`, started the way `run_daemon` starts.
    fn boot(state: &Path, src: &Path) -> Arc<DaemonContext> {
        let ctrl = lifecycle::setup_arena(
            &state.join("arena"),
            8 * 1024 * 1024,
            Some(&state.join("arena.ctrl")),
        )
        .unwrap();
        let live = live_db_path_for(&ctrl);
        let conn = init_living_db(&ctrl, &live, Some(src), Some("go"), false).unwrap();
        snapshot_to_arena(&conn, &ctrl).unwrap();
        context_over(&ctrl, Arc::new(NoExt), src, conn)
    }

    fn op(ctx: &Arc<DaemonContext>, request: serde_json::Value) -> serde_json::Value {
        let out = crate::daemon::ops::handle_base_op_value(ctx, request).expect("a base op");
        serde_json::from_str(&out).unwrap()
    }

    fn read_source(ctx: &Arc<DaemonContext>, path: &str) -> serde_json::Value {
        op(ctx, serde_json::json!({"op": "read_source", "path": path}))
    }

    /// Propose `content` for `path` against what `path` holds now.
    fn propose_over(ctx: &Arc<DaemonContext>, path: &str, content: &str) -> serde_json::Value {
        let base = read_source(ctx, path)["blob_hash"]
            .as_str()
            .unwrap()
            .to_string();
        op(
            ctx,
            serde_json::json!({"op": "propose", "writes": [
                {"path": path, "base": base, "content": content},
            ]}),
        )
    }

    /// What the checkout's conflict record holds for `path`, read through
    /// the daemon's `query` op.
    fn conflict_rows(ctx: &Arc<DaemonContext>, path: &str) -> Vec<serde_json::Value> {
        let r = op(
            ctx,
            serde_json::json!({"op": "query", "sql": format!(
                "SELECT path, hex(disk_hash) AS disk, hex(stored_hash) AS stored \
                   FROM _checkout_conflict WHERE path = '{path}'"
            )}),
        );
        assert_eq!(r["ok"], true, "{r}");
        r["rows"].as_array().cloned().unwrap_or_default()
    }

    fn upper_hex(text: &str) -> String {
        use leyline_core::ContentAddressed;
        text.as_bytes().hash().to_string().to_uppercase()
    }

    #[tokio::test]
    async fn a_restart_keeps_an_accepted_proposal_and_the_directory_holds_it() {
        let (_dir, src, state) = checkout_fixture();
        let ctx = boot(&state, &src);
        let accepted = propose_over(&ctx, "a.go", CHECKOUT_PROPOSED);
        assert_eq!(accepted["ok"], true, "{accepted}");
        assert!(accepted.get("post_commit_error").is_none(), "{accepted}");
        assert_eq!(
            std::fs::read_to_string(src.join("a.go")).unwrap(),
            CHECKOUT_PROPOSED,
            "an accepted proposal is materialized into the source directory",
        );
        drop(ctx);

        let ctx = boot(&state, &src);
        assert_eq!(read_source(&ctx, "a.go")["content"], CHECKOUT_PROPOSED);
        assert_eq!(read_source(&ctx, "b.go")["content"], CHECKOUT_B);
        assert_eq!(
            std::fs::read_to_string(src.join("a.go")).unwrap(),
            CHECKOUT_PROPOSED
        );
    }

    #[tokio::test]
    async fn a_crash_between_commit_and_materialization_is_materialized_at_restart() {
        let (_dir, src, state) = checkout_fixture();
        let ctx = boot(&state, &src);
        // The file cannot be staged: a directory sits where its temp file goes.
        let staging = crate::checkout::staging_path(&src.join("a.go"));
        std::fs::create_dir(&staging).unwrap();
        let accepted = propose_over(&ctx, "a.go", CHECKOUT_PROPOSED);
        assert_eq!(accepted["ok"], true, "the proposal commits: {accepted}");
        assert!(
            accepted["post_commit_error"]
                .as_str()
                .is_some_and(|e| e.contains("a.go")),
            "the failed materialization is reported: {accepted}",
        );
        assert_eq!(
            std::fs::read_to_string(src.join("a.go")).unwrap(),
            CHECKOUT_A
        );
        drop(ctx);
        std::fs::remove_dir(&staging).unwrap();

        let ctx = boot(&state, &src);
        assert_eq!(read_source(&ctx, "a.go")["content"], CHECKOUT_PROPOSED);
        assert_eq!(
            std::fs::read_to_string(src.join("a.go")).unwrap(),
            CHECKOUT_PROPOSED,
            "startup materializes the proposal instead of reverting it",
        );
        assert!(conflict_rows(&ctx, "a.go").is_empty());
    }

    #[tokio::test]
    async fn an_edit_made_while_the_daemon_was_down_is_ingested() {
        let (_dir, src, state) = checkout_fixture();
        drop(boot(&state, &src));
        std::fs::write(src.join("a.go"), CHECKOUT_EDITED).unwrap();
        std::fs::remove_file(src.join("b.go")).unwrap();
        std::fs::write(src.join("c.go"), CHECKOUT_B).unwrap();

        let ctx = boot(&state, &src);
        assert_eq!(read_source(&ctx, "a.go")["content"], CHECKOUT_EDITED);
        assert_eq!(
            read_source(&ctx, "b.go")["ok"],
            false,
            "a deleted file leaves"
        );
        assert_eq!(read_source(&ctx, "c.go")["content"], CHECKOUT_B);
        assert!(conflict_rows(&ctx, "a.go").is_empty());
        drop(ctx);

        // The ingest was recorded: the next start changes nothing.
        let ctx = boot(&state, &src);
        assert_eq!(read_source(&ctx, "a.go")["content"], CHECKOUT_EDITED);
        assert_eq!(
            std::fs::read_to_string(src.join("a.go")).unwrap(),
            CHECKOUT_EDITED
        );
        assert!(!src.join("b.go").exists(), "a deleted file is not restored");
    }

    #[tokio::test]
    async fn both_sides_changed_is_a_conflict_that_keeps_both() {
        let (_dir, src, state) = checkout_fixture();
        let ctx = boot(&state, &src);
        let staging = crate::checkout::staging_path(&src.join("a.go"));
        std::fs::create_dir(&staging).unwrap();
        assert_eq!(propose_over(&ctx, "a.go", CHECKOUT_PROPOSED)["ok"], true);
        drop(ctx);
        std::fs::remove_dir(&staging).unwrap();
        // While the daemon is down, a person edits the file it never wrote.
        std::fs::write(src.join("a.go"), CHECKOUT_EDITED).unwrap();

        let ctx = boot(&state, &src);
        assert_eq!(
            read_source(&ctx, "a.go")["content"],
            CHECKOUT_PROPOSED,
            "the store keeps the proposal",
        );
        assert_eq!(
            std::fs::read_to_string(src.join("a.go")).unwrap(),
            CHECKOUT_EDITED,
            "the directory keeps the edit",
        );
        assert_eq!(
            conflict_rows(&ctx, "a.go"),
            vec![serde_json::json!({
                "path": "a.go",
                "disk": upper_hex(CHECKOUT_EDITED),
                "stored": upper_hex(CHECKOUT_PROPOSED),
            })],
        );
        drop(ctx);

        // Proposing the directory's bytes resolves it.
        let ctx = boot(&state, &src);
        assert_eq!(propose_over(&ctx, "a.go", CHECKOUT_EDITED)["ok"], true);
        assert!(conflict_rows(&ctx, "a.go").is_empty());
        drop(ctx);
        let ctx = boot(&state, &src);
        assert_eq!(read_source(&ctx, "a.go")["content"], CHECKOUT_EDITED);
        assert!(conflict_rows(&ctx, "a.go").is_empty());
    }

    /// `reparse` over the daemon's own checkout reconciles it: a proposal
    /// the directory never received is written out, not replaced by the
    /// directory's older bytes. A reparse of another directory parses it.
    #[tokio::test]
    async fn a_reparse_of_the_checkout_reconciles_it() {
        let (dir, src, state) = checkout_fixture();
        let ctx = boot(&state, &src);
        let staging = crate::checkout::staging_path(&src.join("a.go"));
        std::fs::create_dir(&staging).unwrap();
        assert_eq!(propose_over(&ctx, "a.go", CHECKOUT_PROPOSED)["ok"], true);
        std::fs::remove_dir(&staging).unwrap();

        let r = op(&ctx, serde_json::json!({"op": "reparse"}));
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(read_source(&ctx, "a.go")["content"], CHECKOUT_PROPOSED);
        assert_eq!(
            std::fs::read_to_string(src.join("a.go")).unwrap(),
            CHECKOUT_PROPOSED
        );

        // Scoped to a file with nothing to ingest: no parse ran.
        let quiet = op(
            &ctx,
            serde_json::json!({"op": "reparse", "files": ["a.go"]}),
        );
        assert_eq!(quiet["ok"], true, "{quiet}");
        assert_eq!(quiet["parsed"], "0", "{quiet}");

        let other = dir.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("c.go"), CHECKOUT_B).unwrap();
        let r = op(
            &ctx,
            serde_json::json!({"op": "reparse", "source": other.to_string_lossy()}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["parsed"], "1", "{r}");
    }

    #[test]
    fn the_watcher_reconciles_every_path_whose_dirty_state_moved() {
        let set = |paths: &[&str]| -> std::collections::HashSet<String> {
            paths.iter().map(|p| p.to_string()).collect()
        };
        assert_eq!(
            watch_scope(false, &set(&["a.go", "b.go"]), &set(&["b.go", "c.go"])),
            Some(vec![
                "a.go".to_string(),
                "b.go".to_string(),
                "c.go".to_string()
            ]),
        );
        assert_eq!(watch_scope(true, &set(&["a.go"]), &set(&[])), None);
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
    /// to `topic`, and spawn `git_watch_loop`. The watcher explicitly signals
    /// when its first HEAD + dirty-set sample is complete, so callers cannot
    /// race the baseline and no timing delay is needed.
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
        let (baseline_tx, baseline_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            git_watch_loop(watch_ctx, &watch_source, emitter, Some(baseline_tx)).await;
        });
        baseline_rx
            .await
            .expect("git watcher must publish its initial baseline");

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
        let recorded = tb.ext.file_changes.lock();
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
        let head_calls = tb.ext.head_changes.lock();
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
            mcp_uds: None,
            reset_arena: false,
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

    #[derive(Default)]
    struct RejectingExecutionHandler;

    impl ExecutionHandler for RejectingExecutionHandler {
        fn capabilities(&self) -> Result<String> {
            anyhow::bail!("not called")
        }

        fn provision(&self, _input: &serde_json::Value) -> Result<String> {
            anyhow::bail!("not called")
        }

        fn start(&self, _input: &serde_json::Value) -> Result<String> {
            anyhow::bail!("not called")
        }

        fn status(&self, _input: &serde_json::Value) -> Result<String> {
            anyhow::bail!("not called")
        }

        fn inspect(&self, _input: &serde_json::Value) -> Result<String> {
            anyhow::bail!("not called")
        }

        fn collect(&self, _input: &serde_json::Value) -> Result<String> {
            anyhow::bail!("not called")
        }

        fn cleanup(&self, _input: &serde_json::Value) -> Result<String> {
            anyhow::bail!("not called")
        }

        fn cancel(&self, _input: &serde_json::Value) -> Result<String> {
            anyhow::bail!("not called")
        }
    }

    fn rejected_execution_config(arena: &Path) -> DaemonConfig {
        let mut config = test_mcp_gate_config(arena, 12348);
        config.mcp_port = Some(8384);
        config.mcp_bind = Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        config.mcp_no_auth = true;
        config
    }

    #[tokio::test]
    async fn execution_daemon_entry_points_preserve_fail_fast_admission() {
        let tmp = tempfile::tempdir().unwrap();
        let handler: Arc<dyn ExecutionHandler> = Arc::new(RejectingExecutionHandler);

        let error = run_execution_daemon(
            rejected_execution_config(&tmp.path().join("execution-default.arena")),
            Arc::clone(&handler),
        )
        .await
        .expect_err("execution daemon must preserve public-bind admission");
        assert!(format!("{error:#}").contains("--mcp-allow-public"));

        let error = run_execution_daemon_with_options(
            rejected_execution_config(&tmp.path().join("execution-options.arena")),
            Arc::clone(&handler),
            DaemonOptions {
                cdc: Some(crate::CdcTarget::Nodes),
                ..DaemonOptions::default()
            },
        )
        .await
        .expect_err("execution daemon options entry point must preserve admission");
        assert!(format!("{error:#}").contains("--mcp-allow-public"));

        let error = run_execution_daemon_with_context(
            rejected_execution_config(&tmp.path().join("execution-context.arena")),
            handler,
            DaemonOptions::default(),
            crate::daemon::ContextSlot::default(),
        )
        .await
        .expect_err("execution daemon context entry point must preserve admission");
        assert!(format!("{error:#}").contains("--mcp-allow-public"));
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

    // --- bead ley-line-open-8c8ded: a publish costs one image, off the writer

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn the_arena_keeps_its_size_while_the_image_fits_its_buffer() {
        let header = leyline_core::ArenaHeader::HEADER_SIZE;
        let arena = header + 2 * (100 * MIB);
        // An image that fits the inactive buffer exactly, or with room to
        // spare, is written in place: no new arena.
        assert_eq!(arena_size_for(arena, 100 * MIB), arena);
        assert_eq!(arena_size_for(arena, 1), arena);
        assert_eq!(arena_size_for(arena, 0), arena);
    }

    #[test]
    fn a_grown_arena_leaves_room_for_the_image_to_grow() {
        let header = leyline_core::ArenaHeader::HEADER_SIZE;
        let image = 1_312_063_488u64;
        let grown = arena_size_for(64 * MIB, image);
        let buffer = leyline_core::ArenaHeader::buffer_size(grown);
        assert!(buffer >= image, "the image fits the new buffer");
        assert_eq!(
            buffer,
            image + image / 4,
            "the new buffer leaves exactly a quarter of the image free, no more"
        );
        assert_eq!(
            grown,
            header + 2 * buffer,
            "the arena is the header and two buffers"
        );
        // The case the share benchmark hit: a few KiB of growth after a
        // resize rewrote the whole 2.6 GB arena. It now stays in place.
        assert_eq!(arena_size_for(grown, image + 64 * 1024), grown);
        // A small image still gets a usable minimum.
        let small = arena_size_for(header, 10);
        assert_eq!(leyline_core::ArenaHeader::buffer_size(small), MIB);
    }

    #[test]
    fn the_image_root_is_the_substrate_hash_of_the_image() {
        use leyline_core::ContentAddressed;
        // Sizes across BLAKE3's chunk (1 KiB) and its multi-chunk tree.
        for len in [
            0usize,
            1,
            1023,
            1024,
            1025,
            64 * 1024 + 3,
            3 * 1024 * 1024 + 7,
        ] {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
            let expected = *bytes.as_slice().hash().as_bytes();
            assert_eq!(image_root(&bytes), expected, "len {len}");
        }
    }

    /// A living db file-backed beside `ctrl_path`, wrapped as the daemon
    /// wraps it, with a small arena registered.
    fn publishing_live_db(dir: &TempDir) -> (crate::daemon::db_pool::LiveDb, std::path::PathBuf) {
        let arena_path = dir.path().join("pub.arena");
        let ctrl_path = dir.path().join("pub.ctrl");
        let live_db_path = live_db_path_for(&ctrl_path);
        let _mmap = leyline_core::create_arena(&arena_path, 2 * MIB).unwrap();
        leyline_core::Controller::open_or_create(&ctrl_path)
            .unwrap()
            .set_arena(&arena_path.to_string_lossy(), 2 * MIB)
            .unwrap();
        let conn = init_living_db(&ctrl_path, &live_db_path, None, None, false).unwrap();
        conn.execute_batch("CREATE TABLE landed (id INTEGER PRIMARY KEY, name TEXT);")
            .unwrap();
        let live = crate::daemon::db_pool::LiveDb::new(conn, &live_db_path, 2).unwrap();
        (live, ctrl_path)
    }

    fn published_names(ctrl_path: &Path) -> Vec<String> {
        let graph = leyline_fs::SqliteGraph::from_arena(ctrl_path).unwrap();
        let mut stmt = graph
            .conn()
            .prepare("SELECT name FROM landed ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn every_commit_before_a_publish_is_in_the_image_it_publishes() {
        let dir = TempDir::new().unwrap();
        let (live, ctrl_path) = publishing_live_db(&dir);
        for name in ["a", "b", "c", "d"] {
            live.writer
                .lock()
                .execute("INSERT INTO landed (name) VALUES (?1)", [name])
                .unwrap();
        }
        publish_living_db(&live, &ctrl_path).unwrap();
        assert_eq!(published_names(&ctrl_path), ["a", "b", "c", "d"]);
        let root = leyline_core::Controller::open_or_create(&ctrl_path)
            .unwrap()
            .current_root();
        assert_ne!(root, [0u8; 32], "the publish advanced current_root");
    }

    #[test]
    fn a_publish_does_not_wait_for_the_writer() {
        // The writer is held (a proposal mid-commit) for the whole publish:
        // the publish reads a committed snapshot through the pool and
        // finishes. Publishing on the writer deadlocked here.
        let dir = TempDir::new().unwrap();
        let (live, ctrl_path) = publishing_live_db(&dir);
        live.writer
            .lock()
            .execute("INSERT INTO landed (name) VALUES ('committed')", [])
            .unwrap();
        let live = Arc::new(live);
        let held = live.writer.lock();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let publisher = {
            let live = Arc::clone(&live);
            let ctrl_path = ctrl_path.clone();
            std::thread::spawn(move || {
                done_tx
                    .send(publish_living_db(&live, &ctrl_path).is_ok())
                    .unwrap();
            })
        };
        let published = done_rx.recv_timeout(std::time::Duration::from_secs(60));
        drop(held);
        publisher.join().unwrap();
        assert_eq!(
            published,
            Ok(true),
            "the publish finished while the writer was held"
        );
        assert_eq!(published_names(&ctrl_path), ["committed"]);
    }

    #[test]
    fn the_publisher_publishes_only_when_something_committed() {
        let dir = TempDir::new().unwrap();
        let (live, ctrl_path) = publishing_live_db(&dir);
        let mut last = read_total_changes(&live.writer).unwrap();
        assert!(
            !publish_if_dirty(&live, &ctrl_path, &mut last).unwrap(),
            "clean: no publish"
        );
        live.writer
            .lock()
            .execute("INSERT INTO landed (name) VALUES ('x')", [])
            .unwrap();
        assert!(
            publish_if_dirty(&live, &ctrl_path, &mut last).unwrap(),
            "dirty: one publish"
        );
        assert_eq!(published_names(&ctrl_path), ["x"]);
        assert!(
            !publish_if_dirty(&live, &ctrl_path, &mut last).unwrap(),
            "nothing since: no second publish of the same image"
        );
    }
}

#[cfg(test)]
mod execution_discovery_tests {
    use super::{DaemonOptions, execution_daemon_options};
    use crate::daemon::socket::SocketDiscovery;

    #[test]
    fn execution_daemon_cannot_be_configured_onto_the_substrate_socket_name() {
        let caller = DaemonOptions {
            cdc: Some(crate::CdcTarget::SourceBlobs),
            discovery: SocketDiscovery::Substrate,
            host_mount: None,
        };
        let effective = execution_daemon_options(caller);
        assert_eq!(
            effective.discovery,
            SocketDiscovery::Execution,
            "the execution surface must never publish default.sock"
        );
        // Deliberately the non-default target: a `bool` field would have
        // survived this assertion even if the override dropped the caller's
        // CHOICE of target, which is exactly the bug c3d746 fixed.
        assert_eq!(
            effective.cdc,
            Some(crate::CdcTarget::SourceBlobs),
            "unrelated caller options must survive, target included"
        );
    }
}
