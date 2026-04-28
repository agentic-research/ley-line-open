//! Serve command — create arena, mount via NFS or FUSE, wait for shutdown.

use std::path::{Path, PathBuf};
#[cfg(feature = "mount")]
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use leyline_core::{Controller, create_arena};
#[cfg(feature = "mount")]
use leyline_fs::graph::HotSwapGraph;

/// Return the default filesystem backend for the current platform.
///
/// macOS: NFS (FUSE requires macfuse/osxfuse which is often unavailable).
/// Linux and others: FUSE.
pub fn default_backend() -> String {
    if cfg!(target_os = "macos") {
        "nfs".into()
    } else {
        "fuse".into()
    }
}

/// Parse a human-friendly duration string like "30s", "5m", "2h".
///
/// Supported suffixes: `s` (seconds), `m` (minutes), `h` (hours).
/// A bare number (no suffix) is treated as seconds.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("empty duration string");
    }

    let (num_part, multiplier) = if let Some(n) = s.strip_suffix('h') {
        (n, 3600)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1)
    } else {
        (s, 1)
    };

    let value: u64 = num_part
        .parse()
        .with_context(|| format!("invalid duration number: {num_part:?}"))?;
    Ok(Duration::from_secs(value * multiplier))
}

/// Set up the arena and controller without mounting.
///
/// Returns the controller path (resolved) so callers can build a graph.
/// This is also the seam tested by integration tests that cannot mount.
pub fn setup_arena(
    arena: &Path,
    arena_size: u64,
    control: Option<&Path>,
) -> Result<PathBuf> {
    // Create the arena file.
    let _mmap = create_arena(arena, arena_size).context("create arena")?;

    // Derive control path: explicit or arena path with .ctrl extension.
    let ctrl_path = match control {
        Some(p) => p.to_path_buf(),
        None => arena.with_extension("ctrl"),
    };

    // Open/create the controller.
    let mut ctrl = Controller::open_or_create(&ctrl_path).context("open controller")?;

    // If generation is 0 (fresh), register the arena path and size.
    if ctrl.generation() == 0 {
        let arena_str = arena
            .canonicalize()
            .unwrap_or_else(|_| arena.to_path_buf())
            .to_string_lossy()
            .to_string();
        ctrl.set_arena(&arena_str, arena_size, 0)
            .context("set arena in controller")?;
    }

    Ok(ctrl_path)
}

/// Shell out to mount_nfs (macOS) or mount.nfs (Linux) to mount the NFS export.
#[cfg(feature = "mount")]
pub fn mount_nfs_cmd(port: u16, mountpoint: &Path) -> Result<()> {
    std::fs::create_dir_all(mountpoint)
        .with_context(|| format!("create mountpoint {}", mountpoint.display()))?;

    let mount_path = mountpoint
        .canonicalize()
        .unwrap_or_else(|_| mountpoint.to_path_buf());

    let nfs_opts = format!(
        "nolocks,vers=3,tcp,rsize=131072,wsize=131072,port={port},mountport={port}"
    );
    let mount_str = mount_path.to_string_lossy();

    let status = if cfg!(target_os = "macos") {
        std::process::Command::new("mount_nfs")
            .args(["-o", &nfs_opts, "127.0.0.1:/", &mount_str])
            .status()
            .context("spawn mount_nfs")?
    } else {
        std::process::Command::new("mount.nfs")
            .args(["-o", &nfs_opts, "127.0.0.1:/", &mount_str])
            .status()
            .context("spawn mount.nfs")?
    };

    if !status.success() {
        anyhow::bail!("NFS mount failed with exit code {:?}", status.code());
    }
    Ok(())
}

#[cfg(feature = "mount")]
/// Main serve entry point.
#[allow(clippy::too_many_arguments)]
pub async fn cmd_serve(
    arena: &Path,
    arena_size_mib: u64,
    control: Option<&Path>,
    mount: &Path,
    backend: &str,
    nfs_port: u16,
    language: Option<&str>,
    timeout: Option<&str>,
) -> Result<()> {
    // Calculate arena size from MiB.
    let arena_bytes = arena_size_mib * 1024 * 1024;

    // Set up arena and controller.
    let ctrl_path = setup_arena(arena, arena_bytes, control)?;

    // Build the HotSwapGraph.
    let graph = HotSwapGraph::new(ctrl_path)?;

    // Optionally enable validation with the requested language.
    let graph = if let Some(lang_ext) = language {
        let ts_lang = leyline_fs::validate::language_for_extension(lang_ext)
            .with_context(|| format!("unsupported language: {lang_ext}"))?;
        graph.with_validation(Some(ts_lang))
    } else {
        graph.with_writable()
    };

    let graph: Arc<dyn leyline_fs::graph::Graph> = Arc::new(graph);

    // Mount.
    std::fs::create_dir_all(mount)
        .with_context(|| format!("create mountpoint {}", mount.display()))?;

    match backend {
        "nfs" => {
            let listen_addr = format!("0.0.0.0:{nfs_port}");
            let (port, _handle) =
                leyline_fs::nfs::serve_nfs(graph, &listen_addr).await?;
            eprintln!("NFS server listening on port {port}");

            mount_nfs_cmd(port, mount)?;
            eprintln!("mounted at {}", mount.display());

            wait_for_shutdown(timeout).await?;
        }
        "fuse" => {
            let _session = leyline_fs::fuse::mount_fuse(graph, mount)?;
            eprintln!("FUSE mounted at {}", mount.display());

            wait_for_shutdown(timeout).await?;
        }
        other => anyhow::bail!("unknown backend: {other} (expected 'nfs' or 'fuse')"),
    }

    Ok(())
}

/// Wait for Ctrl+C or an optional timeout, whichever comes first.
pub async fn wait_for_shutdown(timeout: Option<&str>) -> Result<()> {
    let timeout_dur = match timeout {
        Some(s) => Some(parse_duration(s)?),
        None => None,
    };

    eprintln!("press Ctrl+C to stop");

    match timeout_dur {
        Some(dur) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    eprintln!("\nreceived Ctrl+C, shutting down");
                }
                _ = tokio::time::sleep(dur) => {
                    eprintln!("timeout reached, shutting down");
                }
            }
        }
        None => {
            tokio::signal::ctrl_c().await?;
            eprintln!("\nreceived Ctrl+C, shutting down");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_seconds() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
    }

    #[test]
    fn parse_duration_minutes() {
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
    }

    #[test]
    fn parse_duration_hours() {
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
    }

    #[test]
    fn parse_duration_bare_number() {
        assert_eq!(parse_duration("60").unwrap(), Duration::from_secs(60));
    }

    #[test]
    fn parse_duration_empty() {
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn parse_duration_invalid() {
        assert!(parse_duration("abc").is_err());
    }

    #[test]
    fn default_backend_is_known() {
        let b = default_backend();
        assert!(b == "nfs" || b == "fuse", "unexpected backend: {b}");
    }
}
