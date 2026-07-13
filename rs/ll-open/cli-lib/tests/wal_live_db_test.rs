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
        reset_arena: false,
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

/// Adversarial coverage per bead `ley-line-open-fd07d8` (adversarial
/// testing sweep for storage-layer changes).
///
/// **Claim:** if `.live.db-wal` is corrupted between daemon runs, the
/// next boot either recovers cleanly OR fails loud — it never returns
/// silently with a torn view of the DB.
///
/// **Why this matters:** WAL is a separate on-disk file with its own
/// format. A power-loss, filesystem bug, or errant `truncate` could
/// leave the WAL in a valid-header-but-invalid-body state. SQLite has
/// robust WAL recovery, but the test pins that we don't accidentally
/// paper over failures with a `let _ = ` on the reopen path.
///
/// **Method:** seed a sentinel row, cleanly shut down the daemon,
/// deliberately corrupt the WAL sidecar (write garbage bytes after
/// the WAL header at offset 32 — past the magic + format-version
/// bytes at 0..8, past the checksum salts at 12..24), then boot a
/// fresh daemon and observe what happens.
///
/// **Pass criteria (either is acceptable):**
/// - Clean recovery: daemon boots, `journal_mode = wal`, sentinel
///   row is EITHER the pre-corruption value OR missing (recovery
///   dropped incomplete WAL frames — the correct behavior).
/// - Clean failure: daemon returns an Err with a message about
///   corrupt/malformed database — never a silent success.
///
/// **What must NEVER happen:** daemon boots OK with a torn sentinel
/// value (e.g., row exists but with garbage data). That would mean
/// WAL recovery silently accepted corrupted frames.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_survives_or_fails_loud_on_wal_corruption() {
    use std::io::{Seek, SeekFrom, Write};

    let dir = TempDir::new().unwrap();
    let (arena, _ctrl, live_db) = arena_paths(dir.path());
    let wal_path = live_db.with_extension("db-wal");

    // 1. First boot: seed a sentinel row + clean shutdown.
    {
        let config = wal_test_config(&arena, None, 1);
        let ext: Arc<dyn leyline_cli_lib::daemon::DaemonExt> = Arc::new(NoExt);
        run_daemon(config, ext).await.expect("first daemon boot");

        // Seed a sentinel via a fresh WAL-configured connection so
        // the write goes through the same code path the daemon uses.
        let conn = rusqlite::Connection::open(&live_db).expect("open live_db");
        conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sentinel(id INTEGER PRIMARY KEY);
             INSERT INTO sentinel(id) VALUES(1729);",
        )
        .expect("seed sentinel");
        drop(conn);
    }

    // 2. Confirm the sentinel was written and a WAL file exists.
    let (mode_before, sentinel_before) = probe_live_db(&live_db);
    assert_eq!(mode_before, "wal", "sanity: pre-corrupt WAL mode");
    assert_eq!(sentinel_before, 1729, "sanity: sentinel row seeded");

    // 3. Corrupt the WAL sidecar. If there's no WAL file, the seed
    //    write got checkpointed already — that's fine; corrupt the
    //    main db file's page tail instead. Either way the substrate
    //    has to be able to detect+react to storage-layer garbage.
    let target = if wal_path.exists() {
        wal_path.clone()
    } else {
        // No WAL file means the transaction was checkpointed to the
        // main db during shutdown. Corrupt the main db's tail.
        live_db.clone()
    };
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .expect("open target for corruption");
        // Write 128 bytes of garbage at offset 32 — past any header
        // magic in either WAL or SQLite file format.
        f.seek(SeekFrom::Start(32)).unwrap();
        f.write_all(&[0xAB; 128]).unwrap();
        f.sync_all().unwrap();
    }

    // 4. Second boot against the corrupted store. Either clean
    //    recovery or clean failure is acceptable; silent torn state
    //    is NOT.
    let config = wal_test_config(&arena, None, 1);
    let ext: Arc<dyn leyline_cli_lib::daemon::DaemonExt> = Arc::new(NoExt);
    let result = run_daemon(config, ext).await;

    match result {
        Ok(()) => {
            // Clean recovery path. WAL recovery may have dropped
            // uncheckpointed frames — that's fine per the substrate
            // contract. Check: the sentinel is either the pre-corrupt
            // value or absent, but NOT a garbage value.
            let (mode_after, sentinel_after) = probe_live_db(&live_db);
            assert_eq!(mode_after, "wal", "post-recovery must still be WAL");
            assert!(
                sentinel_after == 1729 || sentinel_after == -1,
                "sentinel after recovery must be either preserved (1729) \
                 or absent (-1 sentinel from probe_live_db); got {sentinel_after}. \
                 A torn value would mean WAL recovery accepted corrupted frames.",
            );
        }
        Err(e) => {
            // Clean failure path. The error message should name a
            // corruption/malformed condition — anything vaguely
            // "database" is acceptable. What's NOT acceptable is
            // silence, which is what Ok(()) would mask if we didn't
            // check the sentinel above.
            let msg = format!("{e:#}");
            assert!(
                msg.to_lowercase().contains("corrupt")
                    || msg.to_lowercase().contains("malformed")
                    || msg.to_lowercase().contains("database")
                    || msg.to_lowercase().contains("disk")
                    || msg.to_lowercase().contains("io")
                    || msg.to_lowercase().contains("wal"),
                "corruption error should name the failure mode; got: {msg}",
            );
        }
    }
}

// Silence unused-import warnings when the compiler can't tell we use
// DaemonPhase behind cfg-gated paths in a future extension.
#[allow(dead_code)]
fn _unused_phase() -> DaemonPhase {
    DaemonPhase::Ready
}

// ── Adversarial coverage (bead `ley-line-open-0cdf2d`) ──────────────
//
// Two failure-mode tests deferred from the WAL 15a adversarial gate
// (bead `fd07d8`, closed 2026-07-08). Split off because they need
// test-only infrastructure (RLIMIT_FSIZE for ENOSPC + a WAL-bloat
// scenario that would slow the standard suite).

// ── ENOSPC coverage (bead `ley-line-open-fb3f49`) ──────────────────
//
// Subprocess-isolated ENOSPC / RLIMIT_FSIZE test. RLIMIT_FSIZE is
// process-scoped; an in-process cap leaks to sibling tokio-test tasks
// that boot daemons concurrently under the multi-thread runtime. We
// side-step that by making the test spawn a fresh copy of THIS test
// binary running only the `enospc_child` helper (marked `#[ignore]`
// so it never fires in the normal suite). The child caps its own
// RLIMIT_FSIZE — the cap can't leak because the child exits after
// the assertion runs.
//
// Wire protocol:
//   ENV VAR `LEYLINE_ENOSPC_CHILD_DIR` = temp dir for the child's
//       arena. Presence signals "you are the child; don't spawn."
//   Exit code 42 = child saw the expected Err (test passes).
//   Exit code 43 = child saw Ok (test fails — daemon didn't fail loud).
//   Any other code / signal = test fails (child panicked / crashed).

/// Parent: spawn the ENOSPC child helper via `cargo test --exact ...`
/// and assert exit code == 42 (child observed the required Err).
#[cfg(target_family = "unix")]
#[test]
fn daemon_fails_loud_under_fsize_quota() {
    use std::process::Command;

    // Guard: if the parent env already has LEYLINE_ENOSPC_CHILD_DIR
    // set, we ARE the child and shouldn't recurse. Should never
    // happen (this test isn't marked #[ignore] so it wouldn't be
    // selected by the child's cargo invocation), but belt-and-braces.
    if std::env::var("LEYLINE_ENOSPC_CHILD_DIR").is_ok() {
        return;
    }

    let dir = TempDir::new().expect("parent temp dir");
    let child_arena_dir = dir.path().to_str().expect("child arena dir utf-8");

    // env::current_exe() returns the test binary path itself; running
    // it with `--test enospc_child --nocapture --ignored` invokes just
    // the child test below.
    let test_binary = std::env::current_exe().expect("test binary path");

    let status = Command::new(&test_binary)
        .env("LEYLINE_ENOSPC_CHILD_DIR", child_arena_dir)
        // `--exact enospc_child` = run only the child test.
        // `--ignored` = required because the child test is #[ignore].
        // `--nocapture` = surface child panics if the assertion trips.
        .args([
            "--exact",
            "enospc_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .status()
        .expect("spawn child test binary");

    // Exit code map:
    //   42 = expected Err path fired (test passes)
    //   43 = daemon returned Ok under quota (invariant broken)
    //   * anything else (including signal death, panic, timeout) = fail
    let code = status.code().unwrap_or(-1);
    assert_eq!(
        code, 42,
        "child ENOSPC test exit code {code} — expected 42 (child saw \
         daemon return Err under RLIMIT_FSIZE cap). Exit 43 means the \
         daemon returned Ok despite the quota (invariant broken); any \
         other code means the child panicked/crashed.",
    );
}

/// **Child helper** — never runs in the normal suite (`#[ignore]`),
/// only invoked by the parent test above.
///
/// Caps its own RLIMIT_FSIZE and attempts a daemon boot. Exits with
/// 42 if the boot returned Err (expected), 43 if it returned Ok
/// (invariant broken). Any panic in this function propagates out and
/// the parent sees a non-42/43 exit code.
#[cfg(target_family = "unix")]
#[test]
#[ignore = "subprocess helper — parent spawns via LEYLINE_ENOSPC_CHILD_DIR env var"]
fn enospc_child() {
    // Refuse to run in a non-subprocess context.
    let arena_dir = match std::env::var("LEYLINE_ENOSPC_CHILD_DIR") {
        Ok(d) => d,
        Err(_) => {
            // Not invoked by the parent — silently no-op so
            // running `cargo test --ignored` accidentally doesn't
            // cap RLIMIT_FSIZE on the developer's shell.
            eprintln!(
                "enospc_child: no LEYLINE_ENOSPC_CHILD_DIR set; \
                 this helper only runs under the parent's spawn."
            );
            return;
        }
    };

    // Also block SIGXFSZ so a size-limit hit returns EFBIG rather
    // than killing the child.
    unsafe {
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
    }

    // Cap RLIMIT_FSIZE at 4 KiB — way below any daemon startup file.
    // No RAII Drop guard needed: the child exits after this test.
    let mut current = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `current` points to valid rlimit storage.
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, &mut current) };
    assert_eq!(rc, 0, "child getrlimit failed");
    let new = libc::rlimit {
        rlim_cur: 4096,
        rlim_max: current.rlim_max,
    };
    // SAFETY: `new` is a valid rlimit; rlim_cur ≤ rlim_max.
    let rc = unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &new) };
    assert_eq!(rc, 0, "child setrlimit failed");

    // Attempt daemon boot under cap.
    let arena_path = std::path::Path::new(&arena_dir).join("wal.arena");
    let config = wal_test_config(&arena_path, None, 1);
    let ext: Arc<dyn leyline_cli_lib::daemon::DaemonExt> = Arc::new(NoExt);

    // Build a minimal runtime for the async run_daemon. Can't use
    // #[tokio::test] because we need to exit with a specific code.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("child tokio runtime");

    let result = rt.block_on(async move { run_daemon(config, ext).await });

    // Exit code protocol.
    if result.is_err() {
        // Expected: daemon reported failure rather than silently
        // ignoring the quota.
        std::process::exit(42);
    } else {
        // Invariant broken: substrate write path succeeded even
        // though the quota should have blocked file creation.
        std::process::exit(43);
    }
}

/// **Adversarial #2**: WAL recovers cleanly after growing large + crash.
///
/// **Claim**: if the WAL sidecar grows to non-trivial size and the
/// process is killed without a checkpoint, the next daemon boot
/// still recovers via WAL replay.
///
/// **Method**: after 15a's file-backed live_db is created, we open a
/// direct rusqlite connection to `.live.db`, disable auto-checkpoint,
/// insert enough rows to grow the WAL to >1 MiB, then drop the
/// connection WITHOUT explicit checkpoint (simulates crash from a
/// WAL-replay perspective — the writer never got to flush the WAL
/// into the main db). Restart the daemon and verify:
///   - the daemon boots
///   - `journal_mode` returns `"wal"` (recovery preserved WAL mode)
///   - a sentinel row inserted before the "crash" is queryable
///     (proves WAL replay recovered committed transactions)
///
/// **Why 1 MiB, not 50 MB from the bead**: 1 MiB is enough to prove
/// the WAL replay actually did work (empty WAL trivially recovers).
/// 50 MB would slow the test suite. If a future regression shows
/// large-WAL replay diverging from small-WAL replay, bump the size.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_recovers_from_bloated_wal_after_crash() {
    let dir = TempDir::new().unwrap();
    let (arena, _ctrl, live_db) = arena_paths(dir.path());

    // First: normal boot so the .live.db file exists in WAL mode.
    {
        let config = wal_test_config(&arena, None, 1);
        let ext: Arc<dyn leyline_cli_lib::daemon::DaemonExt> = Arc::new(NoExt);
        run_daemon(config, ext).await.expect("first boot");
    }
    assert!(live_db.exists(), "15a must have created the live_db file");

    // Direct rusqlite access — disable auto-checkpoint, seed a
    // sentinel, then grow the WAL until it's > 1 MiB. Dropping the
    // connection without an explicit checkpoint leaves the writes
    // ONLY in the WAL sidecar; the main db file doesn't have them.
    let wal_path = live_db.with_extension("db-wal");
    {
        let conn = rusqlite::Connection::open(&live_db).expect("open live_db");
        // Reassert WAL mode + disable autocheckpoint for THIS
        // connection. Deliberately do NOT restore autocheckpoint —
        // the goal is to leave a bloated WAL.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA wal_autocheckpoint=0;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS sentinel(id INTEGER PRIMARY KEY, blob BLOB);",
        )
        .expect("configure connection");

        // Seed the sentinel — this is the row we assert is
        // recoverable post-restart.
        conn.execute(
            "INSERT OR REPLACE INTO sentinel(id, blob) VALUES(?1, ?2)",
            rusqlite::params![1729i64, vec![0xABu8; 64]],
        )
        .expect("seed sentinel");

        // Grow the WAL by inserting rows in separate small
        // transactions so each commit appends WAL frames without
        // triggering a checkpoint (autocheckpoint=0).
        for i in 2..=200 {
            conn.execute(
                "INSERT OR REPLACE INTO sentinel(id, blob) VALUES(?1, ?2)",
                rusqlite::params![i as i64, vec![i as u8; 8192]],
            )
            .expect("grow wal");
        }
        // Explicit sync so the WAL is actually on disk before
        // "crash" (dropping the connection without checkpoint).
        conn.execute_batch("PRAGMA synchronous=FULL")
            .expect("bump sync");
        drop(conn);
    }
    // Assert WAL actually bloated (test would be meaningless if not).
    let wal_len = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
    assert!(
        wal_len > 512 * 1024,
        "WAL didn't bloat to >512 KiB (got {wal_len} bytes) — \
         autocheckpoint may not be disabled correctly, test is not \
         actually exercising the bloat path",
    );

    // Second boot against the bloated WAL. Must recover cleanly.
    {
        let config = wal_test_config(&arena, None, 1);
        let ext: Arc<dyn leyline_cli_lib::daemon::DaemonExt> = Arc::new(NoExt);
        run_daemon(config, ext).await.expect(
            "daemon must boot cleanly against a bloated WAL — \
             recovery invariant broken",
        );
    }

    // Verify recovery: journal_mode still WAL + sentinel row queryable.
    let conn =
        rusqlite::Connection::open_with_flags(&live_db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("post-recovery open");
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .expect("post-recovery journal_mode");
    assert_eq!(
        mode, "wal",
        "post-recovery journal_mode must remain WAL; got {mode:?}",
    );
    let sentinel_blob_len: i64 = conn
        .query_row("SELECT length(blob) FROM sentinel WHERE id=1729", [], |r| {
            r.get(0)
        })
        .expect(
            "sentinel row must survive WAL replay; if this errors, \
             recovery dropped committed transactions",
        );
    assert_eq!(
        sentinel_blob_len, 64,
        "sentinel blob length lost after WAL replay — recovery corrupted committed data",
    );
}
