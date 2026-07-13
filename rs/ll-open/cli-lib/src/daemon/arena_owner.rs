//! Arena-owner sentinel — the belt-and-suspenders companion to
//! `arena_lock`'s `flock(2)`.
//!
//! Bead `ley-line-open-c7d00f`. `flock` alone only protects against
//! post-0cba88 daemons: a daemon binary built before that fix landed
//! never called `flock`, so a new daemon's `try_acquire` succeeds
//! against an unlocked lockfile even though a pre-0cba88 daemon is
//! actively mmap'ing the arena. Mache observed exactly this pattern
//! 2026-07-13 — three stale daemons coexisting on
//! `~/.mache/default.arena`.
//!
//! ## Sentinel semantics
//!
//! At daemon startup we write a JSON sentinel to `<arena>.owner`:
//!
//! ```json
//! {
//!   "pid": 12345,
//!   "leyline_version": "0.7.5",
//!   "source_root": "/repos/foo",
//!   "started_at_secs": 1720900000
//! }
//! ```
//!
//! A new daemon reads the sentinel BEFORE deciding whether to acquire
//! the arena:
//!
//! - Sentinel absent → free; proceed (flock still enforces the fast
//!   path).
//! - Sentinel present + PID is alive (`kill(pid, 0)` succeeds) →
//!   refuse. Error names the holder PID + its source_root + version.
//! - Sentinel present + PID is dead → stale; take over (unlink + write
//!   fresh).
//!
//! This gate catches the pre-0cba88 coexistence case as long as the
//! long-running old daemon's PID is still alive (which it is — that's
//! the whole point). Both mechanisms fire together at startup: flock is
//! the fast bug-report ("another daemon holds the lock"), sentinel is
//! the correctness gate ("another daemon owns the arena — flock or no").
//!
//! ## Source-root mismatch guard
//!
//! When a daemon's sentinel is stale (PID dead) but the warm-start
//! flow finds `_meta.source_root` in the live-db disagreeing with the
//! current `--source`, we refuse rather than silently serve the prior
//! daemon's cached parses. See `cmd_daemon::verify_source_root_matches`.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Content of the `<arena>.owner` sentinel file. JSON — chosen over
/// binary so a human debugging a stuck daemon can `cat` it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnerSentinel {
    /// PID of the process that wrote this sentinel.
    pub pid: u32,
    /// `leyline` version string of the daemon that wrote it.
    /// Consumers use this to distinguish "same arena, older binary"
    /// from "same arena, same binary, stale process".
    pub leyline_version: String,
    /// Absolute path to the source directory the daemon parses. Empty
    /// string when the daemon was started without `--source` (rare).
    pub source_root: String,
    /// UNIX seconds when the sentinel was created. Debugging aid;
    /// not consulted for staleness decisions (we use PID liveness).
    pub started_at_secs: u64,
}

/// Guard held for the daemon's runtime. Drop unlinks the sentinel so
/// the arena becomes claimable again (belt+suspenders with `flock`'s
/// OS-managed lock release).
#[derive(Debug)]
pub struct OwnerGuard {
    path: PathBuf,
}

/// Where to write the sentinel for `arena`. Placed alongside the
/// arena — same discipline as `arena_lock`'s `.lock` file. `.owner`
/// suffix so it doesn't collide with anything else in the arena
/// namespace.
pub fn sentinel_path_for(arena: &Path) -> PathBuf {
    let mut p = arena.as_os_str().to_owned();
    p.push(".owner");
    PathBuf::from(p)
}

/// Read the sentinel at `<arena>.owner` if present. `Ok(None)` when
/// the file doesn't exist. `Err` when the file exists but is
/// unreadable / malformed — the caller decides whether to treat that
/// as "someone else owns this" or "take over".
pub fn read_sentinel(arena: &Path) -> Result<Option<OwnerSentinel>> {
    let path = sentinel_path_for(arena);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow::Error::from(e).context(format!("read {}", path.display()))),
    };
    let sentinel: OwnerSentinel = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse sentinel {}", path.display()))?;
    Ok(Some(sentinel))
}

/// Try to acquire the arena via the sentinel discipline.
///
/// - Sentinel absent → write ours, return guard.
/// - Sentinel present + PID alive → refuse. Error message names the
///   holder PID + source_root + version.
/// - Sentinel present + PID dead (or unparseable) → take over.
///
/// Uses atomic-write via temp+rename so a crash mid-write can't leave
/// a truncated sentinel that reads as "unparseable → take over" and
/// races.
///
/// `source_root` is the absolute path the daemon will parse (or empty
/// when `--source` was omitted). Written into the sentinel so peers
/// can distinguish "same arena, matching source" from "same arena,
/// wrong source."
pub fn try_acquire(arena: &Path, source_root: &str) -> Result<OwnerGuard> {
    let path = sentinel_path_for(arena);

    // Check for an existing owner.
    if let Some(existing) = read_sentinel(arena).ok().flatten()
        && is_pid_alive(existing.pid)
    {
        bail!(
            "arena {} is owned by PID {} (leyline {}), source_root={:?}. \
             Refusing to start a second daemon. See bead ley-line-open-c7d00f. \
             If the holding PID is actually dead (stale mmap, external kill), \
             remove {} manually and retry.",
            arena.display(),
            existing.pid,
            existing.leyline_version,
            existing.source_root,
            path.display(),
        );
    }

    // Either absent, malformed, or the holder is dead. Take over.
    let sentinel = OwnerSentinel {
        pid: std::process::id(),
        leyline_version: env!("CARGO_PKG_VERSION").to_string(),
        source_root: source_root.to_string(),
        started_at_secs: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    let bytes = serde_json::to_vec_pretty(&sentinel).context("serialize sentinel")?;

    // Atomic-write via temp+rename so a partial-write crash doesn't
    // leave a truncated sentinel that a peer would misread as
    // "malformed → dead → take over".
    let tmp_path = path.with_extension("owner.tmp");
    fs::write(&tmp_path, &bytes).with_context(|| format!("write {}", tmp_path.display()))?;
    fs::rename(&tmp_path, &path)
        .with_context(|| format!("rename {} → {}", tmp_path.display(), path.display()))?;

    Ok(OwnerGuard { path })
}

impl Drop for OwnerGuard {
    fn drop(&mut self) {
        // Best-effort unlink. If we crash without running Drop, the
        // next daemon's `is_pid_alive` check on our stale sentinel
        // will see the PID is dead and take over.
        let _ = fs::remove_file(&self.path);
    }
}

/// True when `pid` names an alive process this user can signal. On
/// Unix, `kill(pid, 0)` returns 0 for alive processes without
/// delivering a signal, and ESRCH for dead ones. On other platforms,
/// conservatively return true (assume alive) — the flock fast-path
/// is the correctness gate there.
#[cfg(unix)]
fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: signal 0 delivers nothing; kernel just validates the pid
    // and permission. No side effects.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(not(unix))]
fn is_pid_alive(_pid: u32) -> bool {
    // Windows fallback: assume alive so the sentinel refuses on
    // presence alone. Correctness for the sentinel gate degrades to
    // "no stale-takeover on Windows" — acceptable since Windows has
    // no arena_lock either (see arena_lock.rs).
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn acquire_write_and_read_roundtrip() {
        let td = TempDir::new().unwrap();
        let arena = td.path().join("test.arena");
        // Create the arena file so tests behave like a real daemon.
        fs::write(&arena, b"").unwrap();

        let guard = try_acquire(&arena, "/repos/foo").unwrap();
        let sentinel = read_sentinel(&arena).unwrap().unwrap();
        assert_eq!(sentinel.pid, std::process::id());
        assert_eq!(sentinel.source_root, "/repos/foo");
        assert_eq!(sentinel.leyline_version, env!("CARGO_PKG_VERSION"));

        drop(guard);
        // Guard drop unlinks the sentinel.
        assert!(read_sentinel(&arena).unwrap().is_none());
    }

    #[test]
    fn acquire_refuses_when_live_pid_owns_arena() {
        // Simulate a live peer holding the sentinel. Use our own PID —
        // it's guaranteed alive for the duration of this test.
        let td = TempDir::new().unwrap();
        let arena = td.path().join("test.arena");
        fs::write(&arena, b"").unwrap();

        let peer = OwnerSentinel {
            pid: std::process::id(),
            leyline_version: "0.7.0".to_string(),
            source_root: "/repos/other".to_string(),
            started_at_secs: 0,
        };
        fs::write(
            sentinel_path_for(&arena),
            serde_json::to_vec(&peer).unwrap(),
        )
        .unwrap();

        let err = try_acquire(&arena, "/repos/mine")
            .expect_err("must refuse when a live peer owns the arena");
        let msg = format!("{err}");
        assert!(
            msg.contains("owned by PID") && msg.contains("/repos/other"),
            "error must name the holder's PID and source_root, got: {msg}",
        );
    }

    #[test]
    fn acquire_takes_over_stale_sentinel() {
        // Simulate a dead peer. PID 999999 has an overwhelmingly high
        // chance of being unused on any realistic system.
        let td = TempDir::new().unwrap();
        let arena = td.path().join("test.arena");
        fs::write(&arena, b"").unwrap();

        // Confirm the PID we're using is actually dead — else the
        // test would spuriously fail on this system.
        let dead_pid = 999_999_u32;
        if is_pid_alive(dead_pid) {
            eprintln!("test skipped: PID {dead_pid} is alive on this system");
            return;
        }
        let stale = OwnerSentinel {
            pid: dead_pid,
            leyline_version: "0.6.5".to_string(),
            source_root: "/repos/old".to_string(),
            started_at_secs: 0,
        };
        fs::write(
            sentinel_path_for(&arena),
            serde_json::to_vec(&stale).unwrap(),
        )
        .unwrap();

        let guard = try_acquire(&arena, "/repos/new").expect("must take over stale sentinel");
        let sentinel = read_sentinel(&arena).unwrap().unwrap();
        assert_eq!(
            sentinel.pid,
            std::process::id(),
            "our PID must overwrite the stale one",
        );
        assert_eq!(sentinel.source_root, "/repos/new");
        drop(guard);
    }

    #[test]
    fn acquire_takes_over_malformed_sentinel() {
        // A truncated / malformed sentinel is treated as stale.
        // This is the "our predecessor crashed mid-write" case;
        // `read_sentinel` returns Err inside try_acquire's `.ok()`
        // chain, so it becomes "no sentinel" and we take over.
        let td = TempDir::new().unwrap();
        let arena = td.path().join("test.arena");
        fs::write(&arena, b"").unwrap();
        fs::write(sentinel_path_for(&arena), b"not-valid-json").unwrap();

        let guard = try_acquire(&arena, "/repos/mine").expect("malformed sentinel → take over");
        let sentinel = read_sentinel(&arena).unwrap().unwrap();
        assert_eq!(sentinel.pid, std::process::id());
        drop(guard);
    }
}
