//! Black-box integration test for **bead `ley-line-open-98fb67`**
//! sub-bead 15a: file-backed WAL adoption in the daemon's living db.
//!
//! Empirical basis:
//! `docs/research/2026-05-08-workerd-wal-sqlite-experiment.md` — the
//! measurement showing ~600× p99 read improvement of file-backed WAL
//! over the pre-15a `:memory:` DELETE-journal setup at N=10 readers.
//!
//! What 15a ships (this test's target):
//!   1. `.live.db` file lives alongside the arena/ctrl artifacts.
//!   2. On startup, `PRAGMA journal_mode` returns `"wal"` — not the
//!      silent `"memory"` value a `:memory:` connection returns.
//!   3. Snapshot cycle (`snapshot_to_arena`) still works — `serialize()`
//!      on a WAL-mode connection returns a complete file image that
//!      round-trips through the arena.
//!   4. Warm-restart reuses the `.live.db` file without a full
//!      re-parse — the file survives daemon shutdown and the next
//!      boot reopens it.
//!
//! What this test does NOT cover (that's sub-bead 15b's scope):
//!   - Reader-vs-writer concurrency
//!   - `Mutex<Connection>` → pool migration

use std::path::{Path, PathBuf};
use std::sync::Arc;

use leyline_cli_lib::daemon::{DaemonPhase, NoExt};
use tempfile::TempDir;

// ── Helpers ─────────────────────────────────────────────────────────

fn arena_paths(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let arena = root.join("wal.arena");
    let ctrl = root.join("wal.ctrl");
    // Same derivation as `live_db_path_for` in cmd_daemon.rs — kept
    // in sync manually because live_db_path_for is `pub(crate)` and
    // the integration test doesn't reach into private surface.
    let live_db = ctrl.with_extension("live.db");
    (arena, ctrl, live_db)
}

/// Minimal `DaemonConfig` for these tests. Short timeout so the daemon
/// exits promptly after we've observed the invariants we care about.
fn wal_test_config(arena: &Path, source: Option<&Path>, timeout_s: u64) -> DaemonConfig {
    DaemonConfig {
        arena: arena.to_path_buf(),
        arena_size_mib: 4,
        control: None,
        mount: None,
        backend: "sqlite".to_string(),
        nfs_port: 0,
        language: None,
        timeout: Some(format!("{timeout_s}s")),
        source: source.map(|p| p.to_path_buf()),
        mcp_port: None,
        mcp_bind: None,
        mcp_allow_public: false,
        mcp_no_auth: true,
    }
}

use leyline_cli_lib::cmd_daemon::{DaemonConfig, run_daemon};

/// Check `journal_mode` and row count on a file-backed live db without
/// mutating it. Uses `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_NO_MUTEX` so
/// it's safe against a running daemon holding a writer connection.
fn probe_live_db(path: &Path) -> (String, i64) {
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .expect("open live db read-only");
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    // Some test paths seed a `nodes` table via parse; others don't.
    // Return -1 when there's no such table so the caller can pin the
    // "warm-start preserves state" invariant even when parse ran.
    let count = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='sentinel'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0);
    if count > 0 {
        let n: i64 = conn
            .query_row("SELECT id FROM sentinel", [], |r| r.get(0))
            .unwrap_or(-1);
        (mode, n)
    } else {
        (mode, -1)
    }
}

// ── Tests ───────────────────────────────────────────────────────────

/// Load-bearing #1: a fresh daemon boot creates `.live.db` at the
/// derived path AND sets `journal_mode = wal`. If either fails, the
/// 600× read win from the empirical report is left on the table.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_creates_wal_live_db_on_cold_start() {
    let dir = TempDir::new().unwrap();
    let (arena, _ctrl, live_db) = arena_paths(dir.path());

    // Sanity: nothing exists yet.
    assert!(!live_db.exists());

    let config = wal_test_config(&arena, None, 1);
    let ext: Arc<dyn leyline_cli_lib::daemon::DaemonExt> = Arc::new(NoExt);
    // Timeout is enforced by config — no need to race externally.
    run_daemon(config, ext).await.expect("daemon run");

    assert!(
        live_db.exists(),
        "cold-start daemon must create the .live.db file at {}",
        live_db.display(),
    );

    let (mode, _) = probe_live_db(&live_db);
    assert_eq!(
        mode.to_lowercase(),
        "wal",
        "journal_mode must be WAL — got {mode:?}. Regression: the daemon may be back on `:memory:` or the pragma didn't stick.",
    );
}

/// Load-bearing #2: the file-backed `.live.db` survives daemon
/// shutdown, and a second boot reuses it (warm start) instead of
/// blowing it away for a fresh cold start. Verified by seeding a
/// `sentinel` row between boots — if the second boot cold-started,
/// the row wouldn't be there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_warm_restart_reuses_live_db() {
    let dir = TempDir::new().unwrap();
    let (arena, ctrl, live_db) = arena_paths(dir.path());

    // Boot 1: cold start → creates .live.db.
    let ext1: Arc<dyn leyline_cli_lib::daemon::DaemonExt> = Arc::new(NoExt);
    run_daemon(wal_test_config(&arena, None, 1), ext1)
        .await
        .expect("boot 1");
    assert!(live_db.exists(), "boot 1 must create live.db");
    let inode1 = std::fs::metadata(&live_db).unwrap().len();

    // Seed a sentinel row directly. Between boots we take the live
    // db over — that models "the daemon wrote state, then exited."
    {
        let conn = rusqlite::Connection::open(&live_db).unwrap();
        // Reactivate WAL on this connection — sticky per-file, but
        // being explicit costs nothing.
        let _: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sentinel (id INTEGER PRIMARY KEY);
             INSERT INTO sentinel (id) VALUES (7);",
        )
        .unwrap();
    }

    // Boot 2: warm start → must reopen .live.db and preserve sentinel.
    let ext2: Arc<dyn leyline_cli_lib::daemon::DaemonExt> = Arc::new(NoExt);
    run_daemon(wal_test_config(&arena, None, 1), ext2)
        .await
        .expect("boot 2");
    assert!(live_db.exists(), "boot 2 must retain live.db");

    let (mode, sentinel) = probe_live_db(&live_db);
    assert_eq!(mode.to_lowercase(), "wal", "boot 2 must be WAL");
    assert_eq!(
        sentinel,
        7,
        "warm-restart must preserve the sentinel row — {} does not. \
         Regression: boot 2 cold-started and wiped the previous state.",
        live_db.display(),
    );

    // The file grew (or at least didn't shrink to empty) — sanity
    // check that we're really talking about the same file.
    let inode2 = std::fs::metadata(&live_db).unwrap().len();
    assert!(
        inode2 >= inode1,
        "live db shrunk unexpectedly between boots ({inode1} → {inode2})",
    );

    // Ctrl-path is derived correctly from arena.
    assert!(ctrl.exists(), "ctrl file must exist after both boots");
}

/// Load-bearing #3: snapshot cycle works after WAL adoption. This is
/// the invariant flagged in the bead: `serialize()` may require an
/// exclusive write txn, and if WAL breaks that, the arena publish
/// path silently regresses. We drive one snapshot end-to-end and
/// check that the arena's `current_root` advances off the zero
/// sentinel — that's the substrate publish signal (T2.4).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_snapshot_publishes_root_after_wal_adoption() {
    let dir = TempDir::new().unwrap();
    let (arena, ctrl, _live_db) = arena_paths(dir.path());

    let config = wal_test_config(&arena, None, 1);
    let ext: Arc<dyn leyline_cli_lib::daemon::DaemonExt> = Arc::new(NoExt);
    run_daemon(config, ext).await.expect("daemon run");

    // The daemon's startup path calls `snapshot_to_arena` once
    // (cmd_daemon.rs line ~221) as its initial publish. On WAL
    // regression, `serialize()` would either fail or return
    // incomplete bytes — either way, the published root wouldn't
    // match the substrate contract (non-zero == "state exists").
    let c = leyline_core::Controller::open_or_create(&ctrl).unwrap();
    let root = c.current_root();
    assert_ne!(
        root, [0u8; 32],
        "daemon startup must publish a non-zero root via snapshot_to_arena. \
         Zero sentinel means the WAL snapshot path failed silently.",
    );
}

// Silence unused-import warnings when the compiler can't tell we use
// DaemonPhase behind cfg-gated paths in a future extension.
#[allow(dead_code)]
fn _unused_phase() -> DaemonPhase {
    DaemonPhase::Ready
}
