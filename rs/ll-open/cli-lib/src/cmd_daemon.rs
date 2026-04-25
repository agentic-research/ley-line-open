//! Daemon command — arena + mount + UDS socket for coordination.
//!
//! Two entry points:
//! - `cmd_daemon()` — open edition, uses `NoExt` (no private extensions).
//! - `run_daemon()` — generic entry point that accepts an `Arc<dyn DaemonExt>`,
//!   called by ley-line (private) with its own extension.

use std::path::Path;
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
    mount: &Path,
    backend: &str,
    nfs_port: u16,
    language: Option<&str>,
    timeout: Option<&str>,
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
    )
    .await
}

/// Generic daemon entry point — ley-line (private) calls this with its own extension.
///
/// Lifecycle:
/// 1. Arena setup (create/open arena + controller)
/// 2. Event router created
/// 3. `ext.on_init(emitter)` — extension initializes state
/// 4. UDS socket spawned (base ops + extension ops)
/// 5. Mount via NFS/FUSE
/// 6. `ext.on_post_mount(ctrl_path, router)` — extension spawns background tasks
/// 7. Wait for shutdown (Ctrl+C or timeout)
/// 8. Cleanup (remove socket file)
#[allow(clippy::too_many_arguments)]
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
    // 1. Arena setup.
    let arena_bytes = arena_size_mib * 1024 * 1024;
    let ctrl_path = cmd_serve::setup_arena(arena, arena_bytes, control)?;

    // 2. Event router.
    let router = EventRouter::new(10_000);

    // 3. Extension init — pass emitter for background event emission.
    ext.on_init(router.emitter());

    // 4. Build context + spawn UDS socket.
    let ctx = Arc::new(DaemonContext {
        ctrl_path: ctrl_path.clone(),
        ext: ext.clone(),
        router: router.clone(),
    });

    let sock_path = ctrl_path.with_extension("sock");
    crate::daemon::socket::spawn(ctx, sock_path.clone());
    eprintln!("daemon socket at {}", sock_path.display());

    // 5. Mount.
    let graph = HotSwapGraph::new(ctrl_path.clone())?;
    let graph = if let Some(lang_ext) = language {
        let ts_lang = leyline_fs::validate::language_for_extension(lang_ext)
            .with_context(|| format!("unsupported language: {lang_ext}"))?;
        graph.with_validation(Some(ts_lang))
    } else {
        graph.with_writable()
    };
    let graph: Arc<dyn leyline_fs::graph::Graph> = Arc::new(graph);

    std::fs::create_dir_all(mount)
        .with_context(|| format!("create mountpoint {}", mount.display()))?;

    match backend {
        "nfs" => {
            let listen_addr = format!("0.0.0.0:{nfs_port}");
            let (port, _handle) = leyline_fs::nfs::serve_nfs(graph, &listen_addr).await?;
            eprintln!("NFS server listening on port {port}");
            cmd_serve::mount_nfs_cmd(port, mount)?;
            eprintln!("mounted at {}", mount.display());
        }
        "fuse" => {
            let _session = leyline_fs::fuse::mount_fuse(graph, mount)?;
            eprintln!("FUSE mounted at {}", mount.display());
        }
        other => anyhow::bail!("unknown backend: {other} (expected 'nfs' or 'fuse')"),
    }

    // 6. Extension post-mount — spawn background tasks (receiver, TCP control, etc.).
    ext.on_post_mount(&ctrl_path, &router);

    eprintln!("daemon ready — press Ctrl+C to stop");

    // 7. Wait for shutdown.
    cmd_serve::wait_for_shutdown(timeout).await?;

    // 8. Cleanup.
    let _ = std::fs::remove_file(&sock_path);

    Ok(())
}
