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
    let secs = value
        .checked_mul(multiplier)
        .with_context(|| format!("duration overflow: {value} × {multiplier} exceeds u64"))?;
    Ok(Duration::from_secs(secs))
}

/// Set up the arena and controller without mounting.
///
/// Returns the controller path (resolved) so callers can build a graph.
/// This is also the seam tested by integration tests that cannot mount.
pub fn setup_arena(arena: &Path, arena_size: u64, control: Option<&Path>) -> Result<PathBuf> {
    // Create the arena file.
    let _mmap = create_arena(arena, arena_size).context("create arena")?;

    // Derive control path: explicit or arena path with .ctrl extension.
    let ctrl_path = match control {
        Some(p) => p.to_path_buf(),
        None => arena.with_extension("ctrl"),
    };

    // Open/create the controller.
    let mut ctrl = Controller::open_or_create(&ctrl_path).context("open controller")?;

    // If no arena path is registered, register it. Detection uses
    // arena_path emptiness — bead `ley-line-open-baee26` removed the
    // V1 `generation == 0` check from the public API. A fresh
    // controller has an empty arena_path and zero-root sentinel.
    if ctrl.arena_path().is_empty() {
        let arena_str = arena
            .canonicalize()
            .unwrap_or_else(|_| arena.to_path_buf())
            .to_string_lossy()
            .to_string();
        ctrl.set_arena(&arena_str, arena_size)
            .context("set arena in controller")?;
    }

    Ok(ctrl_path)
}

/// Where the NFS export listens: loopback, on `port` (0 picks one). The export
/// is unauthenticated and writable, and [`mount_nfs_cmd`] reaches it at
/// 127.0.0.1, so nothing off this host has a reason to reach it.
pub fn nfs_listen_addr(port: u16) -> String {
    format!("127.0.0.1:{port}")
}

/// Shell out to mount_nfs (macOS) or mount.nfs (Linux) to mount `server`'s
/// export; the returned guard owns the server, and dropping it unmounts and
/// then stops the server.
#[cfg(feature = "mount")]
pub fn mount_nfs_cmd(server: leyline_fs::nfs::NfsServer, mountpoint: &Path) -> Result<NfsMount> {
    let port = server.port();
    std::fs::create_dir_all(mountpoint)
        .with_context(|| format!("create mountpoint {}", mountpoint.display()))?;

    let mount_path = mountpoint
        .canonicalize()
        .unwrap_or_else(|_| mountpoint.to_path_buf());

    let nfs_opts =
        format!("nolocks,vers=3,tcp,rsize=131072,wsize=131072,port={port},mountport={port}");
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
    Ok(NfsMount {
        path: mount_path,
        unmount: unmount_nfs,
        _server: server,
    })
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
    timeout: Option<&str>,
) -> Result<()> {
    // Calculate arena size from MiB.
    let arena_bytes = arena_size_mib * 1024 * 1024;

    // Set up arena and controller.
    let ctrl_path = setup_arena(arena, arena_bytes, control)?;

    // Build the HotSwapGraph. An edit to a parsed file goes through the one
    // write path (ADR-0040 D3); `serve` owns its arena, so the whole grant.
    let graph = HotSwapGraph::new(ctrl_path)?
        .with_source_writer(Arc::new(crate::source_write::MountWriter {
            grant: crate::source_write::Grant::Whole,
            checkout: None,
        }))?
        .with_writable();

    let graph: Arc<dyn leyline_fs::graph::Graph> = Arc::new(graph);

    let mounted = mount_backend(graph, mount, backend, nfs_port).await?;
    wait_for_shutdown(timeout).await?;
    drop(mounted);

    Ok(())
}

/// A mount this process holds while it serves. Dropping it unmounts: the
/// FUSE session's own drop, or `umount` for an NFS mount. Held for the whole
/// serve; before, the daemon dropped its FUSE session at the end of the arm
/// that created it, and nothing ever unmounted NFS, so the daemon exited
/// leaving the kernel holding a mount whose server was gone.
#[cfg(feature = "mount")]
pub enum Mounted {
    Nfs(NfsMount),
    Fuse(leyline_fs::fuse::FuseSession),
}

/// An NFS mount this process made and the server behind it. Dropping it
/// unmounts — the kernel flushes the client's last writes to the server —
/// and then drops the server, which commits them before the drop returns.
/// So the daemon's final snapshot, taken after this drop, holds every write.
#[cfg(feature = "mount")]
pub struct NfsMount {
    path: PathBuf,
    /// [`unmount_nfs`] in every real mount; a test substitutes a recorder.
    unmount: fn(&Path) -> Result<()>,
    /// Dropped after `Drop::drop` has unmounted (fields drop after it).
    _server: leyline_fs::nfs::NfsServer,
}

#[cfg(feature = "mount")]
impl Drop for NfsMount {
    fn drop(&mut self) {
        if let Err(e) = (self.unmount)(&self.path) {
            eprintln!("warn: {e:#}");
        }
    }
}

/// Unmount an NFS mount: `umount`, then `umount -f` if the first refuses.
#[cfg(feature = "mount")]
pub fn unmount_nfs(path: &Path) -> Result<()> {
    for args in [&[][..], &["-f"][..]] {
        let status = std::process::Command::new("umount")
            .args(args)
            .arg(path)
            .status()
            .context("spawn umount")?;
        if status.success() {
            return Ok(());
        }
    }
    anyhow::bail!("could not unmount {}", path.display())
}

/// Mount `graph` at `mount` on `backend` ("nfs" or "fuse") and return the
/// guard that unmounts it.
#[cfg(feature = "mount")]
pub async fn mount_backend(
    graph: Arc<dyn leyline_fs::graph::Graph>,
    mount: &Path,
    backend: &str,
    nfs_port: u16,
) -> Result<Mounted> {
    std::fs::create_dir_all(mount)
        .with_context(|| format!("create mountpoint {}", mount.display()))?;
    match backend {
        "nfs" => {
            let listen_addr = nfs_listen_addr(nfs_port);
            let server = leyline_fs::nfs::serve_nfs(graph, &listen_addr).await?;
            eprintln!("NFS server listening on port {}", server.port());
            let mounted = mount_nfs_cmd(server, mount)?;
            eprintln!("mounted at {}", mount.display());
            Ok(Mounted::Nfs(mounted))
        }
        "fuse" => {
            let session = leyline_fs::fuse::mount_fuse(graph, mount)?;
            eprintln!("FUSE mounted at {}", mount.display());
            Ok(Mounted::Fuse(session))
        }
        other => anyhow::bail!("unknown backend: {other} (expected 'nfs' or 'fuse')"),
    }
}

/// What ended a [`ShutdownSignals::wait`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shutdown {
    Interrupt,
    Terminate,
    Timeout,
}

/// The signals that stop a long-running command: Ctrl+C (SIGINT) and, on
/// unix, SIGTERM — what `docker stop`, launchd and systemd send. Installed
/// up front so a failure to register is an error, not a wait that can never
/// end on that signal.
pub struct ShutdownSignals {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
}

impl ShutdownSignals {
    pub fn install() -> Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                interrupt: signal(SignalKind::interrupt()).context("install the SIGINT handler")?,
                terminate: signal(SignalKind::terminate())
                    .context("install the SIGTERM handler")?,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }

    /// Wait for a signal or the optional timeout, whichever comes first.
    pub async fn wait(&mut self, timeout: Option<Duration>) -> Shutdown {
        let timer = async {
            match timeout {
                Some(dur) => tokio::time::sleep(dur).await,
                None => std::future::pending::<()>().await,
            }
        };
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.interrupt.recv() => Shutdown::Interrupt,
                _ = self.terminate.recv() => Shutdown::Terminate,
                _ = timer => Shutdown::Timeout,
            }
        }
        #[cfg(not(unix))]
        {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => Shutdown::Interrupt,
                _ = timer => Shutdown::Timeout,
            }
        }
    }
}

/// Wait for Ctrl+C, SIGTERM or an optional timeout, whichever comes first.
pub async fn wait_for_shutdown(timeout: Option<&str>) -> Result<()> {
    let timeout_dur = match timeout {
        Some(s) => Some(parse_duration(s)?),
        None => None,
    };
    let mut signals = ShutdownSignals::install()?;

    eprintln!("press Ctrl+C to stop");

    match signals.wait(timeout_dur).await {
        Shutdown::Interrupt => eprintln!("\nreceived Ctrl+C, shutting down"),
        Shutdown::Terminate => eprintln!("received SIGTERM, shutting down"),
        Shutdown::Timeout => eprintln!("timeout reached, shutting down"),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything `serve` does before it mounts — arena, control block,
    /// graph with its source writer, mountpoint — runs, and an unknown
    /// backend is then refused by name rather than served.
    #[cfg(feature = "mount")]
    #[tokio::test]
    async fn serve_builds_its_graph_then_refuses_an_unknown_backend() {
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("mnt");
        let err = cmd_serve(
            &dir.path().join("a.arena"),
            1,
            None,
            &mount,
            "carrier-pigeon",
            0,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("unknown backend: carrier-pigeon"),
            "{err:#}"
        );
        assert!(
            mount.is_dir(),
            "the mountpoint is created before the backend is chosen"
        );
    }

    /// Unmounting a directory nothing is mounted on fails, naming it, after
    /// both `umount` and `umount -f` refuse — it does not report success.
    /// Tests that install the process-wide signal handlers run one at a time:
    /// the SIGTERM test signals the whole process, which would end another
    /// test's wait early.
    static SIGNAL_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Dropping an NFS mount unmounts its own path — the daemon and `serve`
    /// rely on the drop alone to leave no mount behind.
    #[cfg(feature = "mount")]
    #[tokio::test]
    async fn dropping_an_nfs_mount_unmounts_its_path() {
        use std::sync::Mutex;
        static UNMOUNTED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
        fn record(path: &Path) -> Result<()> {
            UNMOUNTED.lock().unwrap().push(path.to_path_buf());
            Ok(())
        }
        let graph = Arc::new(leyline_fs::graph::MemoryGraph::new());
        let server = leyline_fs::nfs::serve_nfs(graph, &nfs_listen_addr(0))
            .await
            .unwrap();
        let path = PathBuf::from("/nonexistent/leyline-test-mount");
        drop(NfsMount {
            path: path.clone(),
            unmount: record,
            _server: server,
        });
        assert_eq!(*UNMOUNTED.lock().unwrap(), vec![path]);
    }

    /// A timeout that does not parse is refused before anything waits, and a
    /// valid one is waited out rather than returning at once.
    #[tokio::test]
    async fn wait_for_shutdown_refuses_a_bad_timeout_and_waits_out_a_good_one() {
        let _serial = SIGNAL_TESTS.lock().await;
        assert!(wait_for_shutdown(Some("soon")).await.is_err());
        let started = std::time::Instant::now();
        wait_for_shutdown(Some("1")).await.unwrap();
        assert!(started.elapsed() >= Duration::from_secs(1));
    }

    #[cfg(feature = "mount")]
    #[test]
    fn unmounting_what_is_not_mounted_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = unmount_nfs(dir.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains(&dir.path().display().to_string()),
            "{err:#}"
        );
    }

    /// The NFS export is unauthenticated and writable, and the mount reaches
    /// it at 127.0.0.1, so it listens on loopback only. It listened on
    /// 0.0.0.0, serving the arena — and since ADR-0040 D3 writes into it —
    /// to every host that could reach the port.
    #[test]
    fn the_nfs_export_listens_on_loopback_only() {
        let addr: std::net::SocketAddr = nfs_listen_addr(2049).parse().unwrap();
        assert!(addr.ip().is_loopback(), "{addr}");
        assert_eq!(addr.port(), 2049);
    }

    /// The timeout ends the wait, and SIGTERM — what `docker stop`, launchd
    /// and systemd send — ends it the way Ctrl+C does, so the caller's
    /// graceful shutdown runs. Before, only Ctrl+C was handled and SIGTERM
    /// killed the process outright, leaving the socket, the arena lock and
    /// any mount behind. One test, in this order: a process-wide SIGTERM
    /// reaches every installed listener, so a separate timeout test running
    /// alongside would see it.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_timeout_or_sigterm_ends_the_wait() {
        let _serial = SIGNAL_TESTS.lock().await;
        let mut signals = ShutdownSignals::install().expect("install shutdown signals");
        let which = tokio::time::timeout(
            Duration::from_secs(5),
            signals.wait(Some(Duration::from_millis(10))),
        )
        .await
        .expect("a 10ms timeout must end the wait");
        assert_eq!(which, Shutdown::Timeout);

        // SAFETY: kill(2) on our own pid; the handler installed above
        // replaces SIGTERM's default action, so the process survives.
        assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGTERM) }, 0);
        let which = tokio::time::timeout(Duration::from_secs(5), signals.wait(None))
            .await
            .expect("SIGTERM must end the wait");
        assert_eq!(which, Shutdown::Terminate);
    }

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
    fn parse_duration_boundary_inputs() {
        // Valid: zero. Some users will type "0s" to mean immediate.
        assert_eq!(parse_duration("0s").unwrap(), Duration::from_secs(0));
        assert_eq!(parse_duration("0").unwrap(), Duration::from_secs(0));

        // Trim handles surrounding whitespace (line 30 `s.trim()`).
        assert_eq!(parse_duration("  5m  ").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("\t30s\n").unwrap(), Duration::from_secs(30));

        // Suffix without number → Err (parse() of "" fails).
        assert!(parse_duration("h").is_err(), "h alone must error");
        assert!(parse_duration("m").is_err());
        assert!(parse_duration("s").is_err());

        // Unsupported suffix `d` for days. The current impl falls
        // through to bare-number path, where "5d".parse::<u64>()
        // fails. Error message says "invalid duration number" not
        // "unsupported suffix". Documented limitation.
        assert!(parse_duration("5d").is_err(), "d (days) is not supported");
        assert!(parse_duration("5w").is_err(), "w (weeks) is not supported");
    }

    #[test]
    fn setup_arena_derives_ctrl_path_from_arena_extension() {
        // Convention pin: when control is None, ctrl_path defaults to
        // arena.with_extension("ctrl"). Daemons / tools rely on this
        // to discover the controller from the arena alone (e.g.
        // /foo/bar.arena → /foo/bar.ctrl). A refactor that changed
        // the extension or moved to a sibling directory would silently
        // break the discovery convention used by mache + cli tools.
        use tempfile::TempDir;
        let td = TempDir::new().unwrap();
        let arena_path = td.path().join("test.arena");
        let ctrl_path = setup_arena(&arena_path, 4096, None).expect("setup_arena");
        assert_eq!(
            ctrl_path,
            arena_path.with_extension("ctrl"),
            "ctrl path must derive as arena.with_extension(ctrl)",
        );
        assert!(ctrl_path.exists(), "controller file must be created");
    }

    #[test]
    fn setup_arena_respects_explicit_control_path() {
        // Sister pin: when explicit ctrl path is supplied, it's used
        // verbatim — no .ctrl appending, no parent-dir mucking.
        use tempfile::TempDir;
        let td = TempDir::new().unwrap();
        let arena_path = td.path().join("test.arena");
        let explicit_ctrl = td.path().join("custom_name.weird");
        let returned = setup_arena(&arena_path, 4096, Some(&explicit_ctrl))
            .expect("setup_arena with explicit ctrl");
        assert_eq!(
            returned, explicit_ctrl,
            "explicit ctrl path must be used verbatim"
        );
    }

    #[test]
    fn parse_duration_rejects_overflow_on_either_step() {
        // Two overflow paths, both must Err cleanly:
        //   (a) input too large for u64 — fails at parse()
        //   (b) input fits in u64 but overflows when multiplied by
        //       the suffix multiplier — fails at checked_mul
        // Path (a):
        let err_a = parse_duration("99999999999999999999999999h")
            .expect_err("26-digit prefix must Err on parse");
        assert!(
            format!("{err_a:#}").contains("invalid duration number"),
            "expected parse error; got: {err_a:#}",
        );
        // Path (b): u64::MAX / 3600 + 1 fits in u64 but overflows × 3600.
        let huge_h = format!("{}h", u64::MAX / 3600 + 1);
        let err_b = parse_duration(&huge_h).expect_err("× 3600 must Err");
        assert!(
            format!("{err_b:#}").contains("overflow"),
            "expected overflow error; got: {err_b:#}",
        );
        // Path (b) for minutes: u64::MAX / 60 + 1 fits in u64.
        let huge_m = format!("{}m", u64::MAX / 60 + 1);
        assert!(parse_duration(&huge_m).is_err());
    }

    #[test]
    fn default_backend_is_known() {
        let b = default_backend();
        assert!(b == "nfs" || b == "fuse", "unexpected backend: {b}");
    }

    #[test]
    fn default_backend_per_platform() {
        // Tightening pin. The platform-specific choice is load-bearing
        // for daemon DX: on macOS, FUSE requires macFUSE/osxfuse which
        // is third-party and often unavailable, so the daemon defaults
        // to NFS via the `nessie-nfsserve` crate. On Linux/BSD, FUSE is the
        // standard. A refactor that flipped the cfg or removed the
        // platform branch would silently break daemon mount UX. Pin
        // platform-specific defaults so a typo (e.g. swapping the
        // arms) surfaces as a test failure.
        let b = default_backend();
        if cfg!(target_os = "macos") {
            assert_eq!(b, "nfs", "macOS must default to NFS (FUSE needs macFUSE)");
        } else {
            assert_eq!(b, "fuse", "non-macOS must default to FUSE");
        }
    }
}
