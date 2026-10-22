//! What a long-running command sets up and waits on: the arena and its
//! controller, the mount backend a platform defaults to, and the signals
//! and timeout that end it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use leyline_core::{Controller, create_arena};

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

    /// Tests that install the process-wide signal handlers run one at a time:
    /// the SIGTERM test signals the whole process, which would end another
    /// test's wait early.
    static SIGNAL_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

        // Trim handles surrounding whitespace (`s.trim()`).
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
