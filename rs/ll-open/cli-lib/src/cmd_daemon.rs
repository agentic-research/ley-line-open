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
use leyline_fs::graph::HotSwapGraph;

use crate::cmd_serve;
use crate::daemon::{DaemonContext, DaemonExt, EventRouter, NoExt};

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
) -> Result<()> {
    // 1. Arena setup.
    let arena_bytes = arena_size_mib * 1024 * 1024;
    let ctrl_path = cmd_serve::setup_arena(arena, arena_bytes, control)?;

    // 2. If --source: parse and load into arena.
    //    Uses the language filter if provided, otherwise parses all recognized languages.
    //    The parse is incremental — unchanged files are skipped on re-runs.
    if let Some(source_dir) = source {
        eprintln!("parsing {} ...", source_dir.display());
        let db_path = ctrl_path.with_extension("db");
        crate::cmd_parse::cmd_parse(source_dir, &db_path, language)?;

        let db_bytes = std::fs::read(&db_path)
            .with_context(|| format!("read parsed db: {}", db_path.display()))?;
        crate::cmd_load::load_into_arena(&ctrl_path, &db_bytes)?;

        let generation = leyline_core::Controller::open_or_create(&ctrl_path)
            .map(|c| c.generation())
            .unwrap_or(0);
        eprintln!("loaded into arena (generation {generation})");
    }

    // 3. Event router.
    let router = EventRouter::new(10_000);

    // 4. Extension init.
    ext.on_init(router.emitter());

    // 5. Build context + spawn UDS socket.
    let ctx = Arc::new(DaemonContext {
        ctrl_path: ctrl_path.clone(),
        ext: ext.clone(),
        router: router.clone(),
    });

    let sock_path = ctrl_path.with_extension("sock");
    crate::daemon::socket::spawn(ctx, sock_path.clone());
    eprintln!("daemon socket at {}", sock_path.display());

    // 6. Mount (optional — omit --mount for headless mode).
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

    eprintln!("daemon ready — press Ctrl+C to stop");

    // 9. Wait for shutdown.
    cmd_serve::wait_for_shutdown(timeout).await?;

    // 10. Cleanup: kill mache child, remove socket.
    if let Some(mut child) = mache_child {
        eprintln!("stopping mache (pid={})...", child.id());
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = std::fs::remove_file(&sock_path);

    Ok(())
}
