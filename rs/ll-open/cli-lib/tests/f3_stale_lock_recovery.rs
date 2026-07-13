//! F3_stale_lock_recovery — falsifiability gate for stale-lock
//! recovery on daemon crash (bead `ley-line-open-c7d00f`).
//!
//! ## Claim
//!
//! When a daemon dies without running Drop (SIGKILL, segfault, kernel
//! OOM), a new daemon MUST be able to acquire the arena. The kernel
//! auto-releases `flock` on process exit; the arena-owner sentinel's
//! PID-liveness check auto-detects the stale sentinel. Both survive
//! ungraceful termination.
//!
//! ## What breaks this gate
//!
//! - Sentinel PID-liveness check misses stale PID (would leave a
//!   permanently unclaimable arena after any crash).
//! - Sentinel-write is not atomic — a crash mid-write leaves a
//!   truncated sentinel that a peer might interpret as "malformed →
//!   take over" (fine) OR "unreadable → don't touch" (would strand
//!   the arena).
//! - `Drop` cleanup is the only path that releases the sentinel
//!   (would strand on crash — Drop doesn't run on SIGKILL).

use leyline_cli_lib::daemon::arena_owner;
use std::fs;
use tempfile::TempDir;

#[test]
fn f3_stale_dead_pid_yields_to_new_daemon() {
    let td = TempDir::new().unwrap();
    let arena = td.path().join("test.arena");
    fs::write(&arena, b"").unwrap();

    let dead_pid = 999_998_u32;
    // Sanity: verify the PID we're using is actually dead.
    if unsafe { libc::kill(dead_pid as libc::pid_t, 0) } == 0 {
        eprintln!("skip: PID {dead_pid} is alive on this system");
        return;
    }

    let stale = arena_owner::OwnerSentinel {
        pid: dead_pid,
        leyline_version: "0.6.9".to_string(),
        source_root: "/repos/dead".to_string(),
        started_at_secs: 0,
    };
    fs::write(
        arena_owner::sentinel_path_for(&arena),
        serde_json::to_vec(&stale).unwrap(),
    )
    .unwrap();

    let guard = arena_owner::try_acquire(&arena, "/repos/new").expect(
        "stale sentinel (dead PID) must permit new daemon to acquire; \
             this is the recovery path from SIGKILL / segfault / OOM",
    );
    let s = arena_owner::read_sentinel(&arena).unwrap().unwrap();
    assert_eq!(s.pid, std::process::id());
    drop(guard);
}

#[test]
fn f3_malformed_sentinel_yields_to_new_daemon() {
    // Companion: partial write (crashed peer that got the create+open
    // but not the write done) leaves a malformed sentinel. New
    // daemon parses it, sees garbage, treats as stale → takes over.
    // Prevents the "permanent poison-arena" scenario.
    let td = TempDir::new().unwrap();
    let arena = td.path().join("test.arena");
    fs::write(&arena, b"").unwrap();
    fs::write(arena_owner::sentinel_path_for(&arena), b"partial-{").unwrap();

    let guard = arena_owner::try_acquire(&arena, "/repos/new")
        .expect("malformed sentinel must NOT strand the arena — new daemon takes over");
    let s = arena_owner::read_sentinel(&arena).unwrap().unwrap();
    assert_eq!(s.pid, std::process::id());
    drop(guard);
}
