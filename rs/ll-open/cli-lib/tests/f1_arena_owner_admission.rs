//! F1_arena_owner_admission — falsifiability gate for arena-owner
//! sentinel admission control (bead `ley-line-open-c7d00f`).
//!
//! ## Claim
//!
//! `arena_owner::try_acquire` refuses to start when another live PID
//! owns the sentinel — INCLUDING the case where the peer never called
//! flock (pre-0cba88 daemon holdover). This is the correctness gate
//! `arena_lock` (flock) cannot enforce on its own: flock only sees
//! post-0cba88 daemons; the sentinel sees every daemon that wrote its
//! PID.
//!
//! ## What breaks this gate
//!
//! - Sentinel presence check is skipped or short-circuited under some
//!   flag combination.
//! - PID liveness check misclassifies a live PID as dead (would allow
//!   silent takeover).
//! - Sentinel write is non-atomic and a partial-write crash lets a
//!   concurrent daemon read "malformed → take over" while the writer
//!   is still alive.

use leyline_cli_lib::daemon::arena_owner;
use std::fs;
use tempfile::TempDir;

#[test]
fn f1_live_peer_pid_blocks_new_daemon() {
    // Simulate a live peer (use our own PID — alive for this test).
    // A new `try_acquire` MUST refuse with the peer's identity
    // named.
    let td = TempDir::new().unwrap();
    let arena = td.path().join("test.arena");
    fs::write(&arena, b"").unwrap();

    let peer = arena_owner::OwnerSentinel {
        pid: std::process::id(),
        leyline_version: "0.7.0-pre-0cba88".to_string(),
        source_root: "/repos/pre-0cba88-daemon".to_string(),
        started_at_secs: 1_000_000_000,
    };
    fs::write(
        arena_owner::sentinel_path_for(&arena),
        serde_json::to_vec(&peer).unwrap(),
    )
    .unwrap();

    let err = arena_owner::try_acquire(&arena, "/repos/new")
        .expect_err("new daemon must refuse when a live peer owns the arena");
    let msg = format!("{err:#}");
    assert!(
        msg.contains(&format!("PID {}", std::process::id())),
        "error must name the peer PID; got: {msg}",
    );
    assert!(
        msg.contains("/repos/pre-0cba88-daemon"),
        "error must name the peer's source_root so operator knows what to kill; got: {msg}",
    );
    assert!(
        msg.contains("ley-line-open-c7d00f"),
        "error must reference the bead so future readers find this discipline; got: {msg}",
    );
}

#[test]
fn f1_dead_peer_pid_permits_takeover() {
    // Companion: dead PID → sentinel is stale → new daemon takes over.
    // Uses PID 999999 which is overwhelmingly unused on any realistic
    // system. Test skips (does not fail) if the PID is alive on this
    // machine, so a rare unlucky env doesn't fail the gate.
    let td = TempDir::new().unwrap();
    let arena = td.path().join("test.arena");
    fs::write(&arena, b"").unwrap();

    let dead_pid = 999_999_u32;
    if unsafe { libc::kill(dead_pid as libc::pid_t, 0) } == 0 {
        eprintln!("skip: PID {dead_pid} is alive on this system");
        return;
    }

    let stale = arena_owner::OwnerSentinel {
        pid: dead_pid,
        leyline_version: "0.6.0".to_string(),
        source_root: "/repos/dead".to_string(),
        started_at_secs: 0,
    };
    fs::write(
        arena_owner::sentinel_path_for(&arena),
        serde_json::to_vec(&stale).unwrap(),
    )
    .unwrap();

    let guard =
        arena_owner::try_acquire(&arena, "/repos/new").expect("dead peer PID must permit takeover");
    // Confirm our PID + source_root now own the arena.
    let s = arena_owner::read_sentinel(&arena).unwrap().unwrap();
    assert_eq!(s.pid, std::process::id());
    assert_eq!(s.source_root, "/repos/new");
    drop(guard);
}
