//! Adversarial coverage for **bead `ley-line-open-f0239d`** — WAL adoption
//! 15b (`Mutex<Connection>` → connection pool + dedicated writer).
//!
//! Empirical target: `docs/research/2026-05-08-workerd-wal-sqlite-experiment.md`
//! measured p99 reads at 290–375 µs on file-backed WAL@N=10 readers + 1
//! writer vs 120–250 ms p99 on DELETE-journal — ~600× improvement. 15a
//! shipped file-backed WAL (`journal_mode=WAL`); 15b splits the single
//! `Mutex<Connection>` into a reader pool + dedicated writer so that
//! win is actually realized end-to-end via `DaemonContext::with_read`.
//!
//! Per bead `ley-line-open-fd07d8`'s adversarial-testing gate for
//! storage-layer changes, this file must include:
//!
//! 1. **Load-bearing: reproduce the concurrent-reader win** at N=10
//!    concurrent readers + 1 writer. Assert p99 read < 10 ms (CI-safe
//!    order-of-magnitude below the DELETE-journal baseline; full 600×
//!    reproduction is too flaky on GHA runners).
//! 2. **Reader pragma enforcement**: `CREATE TABLE` from a reader
//!    connection returns `SQLITE_READONLY`.
//! 3. **Pool exhaustion**: N+1 readers spawned concurrently — the
//!    (N+1)th blocks briefly, never fails.
//! 4. **Writer serialization**: two concurrent writers execute serially
//!    and never corrupt state.
//! 5. **Pragma consistency**: every pool connection has
//!    `journal_mode=wal` and `query_only=ON` after startup.
//!
//! Runs entirely against `LiveDb` — the container `DaemonContext.live_db`
//! now holds — without spinning up the full daemon so we can drive the
//! concurrency shape directly.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use leyline_cli_lib::daemon::db_pool::LiveDb;
use rusqlite::Connection;
use serial_test::serial;
use tempfile::TempDir;

// ── Fixture ─────────────────────────────────────────────────────────

/// Build a fresh WAL-backed `LiveDb` at `<dir>/wal_pool.db` with a
/// `nodes` table seeded with `n_rows` × 256 B payload rows. Mirrors the
/// shape of `benches/wal_concurrent_readers.rs::build_db` so this test
/// exercises the same read shape the empirical experiment measured.
fn seeded_live_db(dir: &TempDir, n_rows: usize, pool_size: u32) -> LiveDb {
    let path = dir.path().join("wal_pool.db");
    let writer = Connection::open(&path).unwrap();
    let mode: String = writer
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
    writer.pragma_update(None, "synchronous", "NORMAL").unwrap();
    writer
        .execute_batch(
            "CREATE TABLE nodes (
                id INTEGER PRIMARY KEY,
                kind TEXT NOT NULL,
                payload BLOB NOT NULL
             );",
        )
        .unwrap();
    let payload = vec![0xCDu8; 256];
    let tx = writer.unchecked_transaction().unwrap();
    {
        let mut stmt = tx
            .prepare("INSERT INTO nodes (kind, payload) VALUES (?1, ?2)")
            .unwrap();
        for i in 0..n_rows {
            stmt.execute(rusqlite::params![format!("kind_{}", i % 8), &payload])
                .unwrap();
        }
    }
    tx.commit().unwrap();
    LiveDb::new(writer, &path, pool_size).unwrap()
}

fn percentile(samples: &mut Vec<Duration>, p: f64) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }
    samples.sort();
    let idx = ((samples.len() as f64 - 1.0) * p).round() as usize;
    samples[idx]
}

// ── (1) Load-bearing: 10 concurrent readers + 1 writer ──────────────

/// Time a workload that reads through a shared `Mutex<Connection>`
/// with `n_threads` readers + 1 writer for `duration`. All ops
/// serialize on the SAME `Mutex<Connection>` — the true pre-15b
/// shape. This is what 15b replaces.
fn measure_mutex_shape(
    db_path: &std::path::Path,
    n_threads: usize,
    duration: Duration,
) -> (u64, Vec<Duration>) {
    // The pre-15b daemon had ONE Mutex<Connection> for readers AND
    // writers. Both compete for the same lock; readers see writer-
    // held lock windows as p99 spikes. Model that shape exactly.
    let shared_conn = Connection::open(db_path).unwrap();
    shared_conn
        .pragma_update(None, "busy_timeout", 5000i64)
        .unwrap();
    let live = Arc::new(Mutex::new(shared_conn));

    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer_stop = stop_flag.clone();
    let writer_live = live.clone();
    let writer_handle = thread::spawn(move || {
        while !writer_stop.load(Ordering::Relaxed) {
            let guard = writer_live.lock().unwrap();
            let _ = guard.execute(
                "INSERT INTO nodes (kind, payload) VALUES ('bg', ?1)",
                [&vec![0xEFu8; 256]],
            );
        }
    });

    let deadline = Instant::now() + duration;
    let mut handles = Vec::with_capacity(n_threads);
    for tid in 0..n_threads {
        let live = live.clone();
        handles.push(thread::spawn(move || {
            let mut samples: Vec<Duration> = Vec::with_capacity(2048);
            let mut lcg = (tid as u64)
                .wrapping_mul(2_862_933_555_777_941_757)
                .wrapping_add(1);
            while Instant::now() < deadline {
                lcg = lcg
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let id = ((lcg >> 33) as i64 % 10_000).abs() + 1;
                let t0 = Instant::now();
                let conn = live.lock().unwrap();
                let _: String = conn
                    .query_row("SELECT kind FROM nodes WHERE id = ?1", [id], |r| r.get(0))
                    .unwrap();
                drop(conn);
                samples.push(t0.elapsed());
            }
            samples
        }));
    }
    let mut all_samples: Vec<Duration> = Vec::new();
    for h in handles {
        let mut s = h.join().unwrap();
        all_samples.append(&mut s);
    }
    stop_flag.store(true, Ordering::Relaxed);
    let _ = writer_handle.join();
    let total = all_samples.len() as u64;
    (total, all_samples)
}

/// Time the pool shape: `n_readers` readers + 1 writer against a
/// `LiveDb` for `duration`. Returns total reads + per-read latency
/// samples.
fn measure_pool_shape(
    live_db: Arc<LiveDb>,
    n_readers: usize,
    duration: Duration,
) -> (u64, Vec<Duration>) {
    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let writer_stop = stop_flag.clone();
    let writer_db = live_db.clone();
    let writer_handle = thread::spawn(move || {
        while !writer_stop.load(Ordering::Relaxed) {
            let guard = writer_db.writer.lock().unwrap();
            let _ = guard.execute(
                "INSERT INTO nodes (kind, payload) VALUES ('bg', ?1)",
                [&vec![0xEFu8; 256]],
            );
        }
    });

    let deadline = Instant::now() + duration;
    let mut handles = Vec::with_capacity(n_readers);
    for tid in 0..n_readers {
        let live = live_db.clone();
        handles.push(thread::spawn(move || {
            let mut samples: Vec<Duration> = Vec::with_capacity(4096);
            let mut lcg = (tid as u64)
                .wrapping_mul(2_862_933_555_777_941_757)
                .wrapping_add(1);
            while Instant::now() < deadline {
                lcg = lcg
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let id = ((lcg >> 33) as i64 % 10_000).abs() + 1;
                let t0 = Instant::now();
                let _: String = live
                    .reader_pool
                    .get()
                    .unwrap()
                    .query_row("SELECT kind FROM nodes WHERE id = ?1", [id], |r| r.get(0))
                    .unwrap();
                samples.push(t0.elapsed());
            }
            samples
        }));
    }
    let mut all_samples: Vec<Duration> = Vec::new();
    for h in handles {
        let mut s = h.join().unwrap();
        all_samples.append(&mut s);
    }
    stop_flag.store(true, Ordering::Relaxed);
    let _ = writer_handle.join();
    let total = all_samples.len() as u64;
    (total, all_samples)
}

/// Reproduces the concurrent-reader win from
/// `docs/research/2026-05-08-workerd-wal-sqlite-experiment.md` by
/// comparing:
///
/// - **Baseline (pre-15b shape)**: 10 readers sharing a single
///   `Mutex<Connection>` — the pre-15b daemon shape.
/// - **New shape (15b)**: 10 readers checking out from an r2d2 reader
///   pool — the shape this bead ships.
///
/// Both variants use file-backed WAL SQLite (15a), so the ONLY
/// difference is the reader-side concurrency primitive. The pool
/// shape must:
///
///   1. Keep p99 read latency < 10 ms (order-of-magnitude below the
///      DELETE-journal baseline the empirical report measured;
///      CI-safe margin above the observed 290–375 µs).
///   2. Aggregate ≥ 3× more reads than the mutex shape in the same
///      wall time — this is what the pool actually buys: unblocked
///      concurrent readers. Under a Mutex, all reads serialize; under
///      the pool, they parallelize up to `min(N, pool_size)`.
///
/// GHA runners are noisy; the p99 sanity ceiling has ~30× headroom
/// over the observed number, and the 3× throughput floor is well
/// below the ~10× the pool typically delivers. The bead documents an
/// escape hatch to 5× → 2× if these prove flaky in CI.
///
/// `#[serial]` (bead `ley-line-open-14b7a2`): under `task ci`
/// workspace-parallel scheduling, the mutex-baseline reader phase
/// picked up co-tenant CPU noise that made its throughput swing 40k
/// → 90k reads/1.5s while the pool phase stayed steady at ~124k. The
/// ratio-against-a-wobbly-baseline flipped pass/fail on scheduling
/// noise, not on any real pool regression. Serial execution restores
/// the invariant this perf test actually measures.
#[test]
#[serial]
fn concurrent_readers_beat_pre_15b_mutex_shape() {
    const N_ROWS: usize = 10_000;
    const N_READERS: usize = 10;
    const DURATION: Duration = Duration::from_millis(1500);

    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("wal_pool.db");
    // Seed the file — shared between both variants.
    {
        let writer = Connection::open(&db_path).unwrap();
        let mode: String = writer
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        writer.pragma_update(None, "synchronous", "NORMAL").unwrap();
        writer
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS nodes (
                    id INTEGER PRIMARY KEY,
                    kind TEXT NOT NULL,
                    payload BLOB NOT NULL
                 );",
            )
            .unwrap();
        let payload = vec![0xCDu8; 256];
        let tx = writer.unchecked_transaction().unwrap();
        {
            let mut stmt = tx
                .prepare("INSERT INTO nodes (kind, payload) VALUES (?1, ?2)")
                .unwrap();
            for i in 0..N_ROWS {
                stmt.execute(rusqlite::params![format!("kind_{}", i % 8), &payload])
                    .unwrap();
            }
        }
        tx.commit().unwrap();
    }

    // Baseline: pre-15b Mutex<Connection> shape.
    let (mutex_reads, mut mutex_samples) = measure_mutex_shape(&db_path, N_READERS, DURATION);
    let mutex_p99 = percentile(&mut mutex_samples, 0.99);

    // New shape: LiveDb (writer + pool).
    let writer = Connection::open(&db_path).unwrap();
    writer.pragma_update(None, "synchronous", "NORMAL").unwrap();
    let live_db = Arc::new(LiveDb::new(writer, &db_path, N_READERS as u32).unwrap());
    let (pool_reads, pool_samples) = measure_pool_shape(live_db.clone(), N_READERS, DURATION);
    let pool_p50 = percentile(&mut pool_samples.clone(), 0.50);
    let pool_p99 = percentile(&mut pool_samples.clone(), 0.99);
    let pool_max = *pool_samples.iter().max().unwrap_or(&Duration::ZERO);

    let scale = pool_reads as f64 / mutex_reads.max(1) as f64;
    eprintln!(
        "\nWAL 15b concurrent readers (N={N_READERS} readers + 1 writer, {}s each):",
        DURATION.as_secs_f32(),
    );
    eprintln!("  Mutex<Connection> baseline: {mutex_reads} reads, p99={mutex_p99:?}");
    eprintln!(
        "  LiveDb pool + writer:       {pool_reads} reads, p99={pool_p99:?}, p50={pool_p50:?}, max={pool_max:?}"
    );
    eprintln!("  pool-vs-mutex throughput scale: {scale:.2}×");

    // Assertion 1: p99 read latency < 10 ms.
    //
    // The empirical bench measured 290–375 µs on Apple Silicon. GHA
    // runners are noisier; 10 ms leaves ~30× headroom while still
    // catching a regression to DELETE-journal levels (120–250 ms p99
    // baseline). If this flakes in CI, the bead documents relaxing
    // to 50 ms — still order-of-magnitude below the baseline.
    assert!(
        pool_p99 < Duration::from_millis(10),
        "p99 read latency exceeded 10ms — WAL 15b's concurrent-reader win \
         has regressed. Got pool_p99={pool_p99:?}, max={pool_max:?}. \
         Empirical baseline (docs/research/2026-05-08-workerd-wal-sqlite-experiment.md): \
         WAL@N=10 p99 = 290–375 µs; DELETE-journal p99 = 120–250 ms.",
    );

    // Assertion 2: pool aggregate throughput ≥ 3× mutex shape.
    //
    // Under Mutex<Connection>, N readers serialize on a single lock;
    // aggregate throughput is bounded by one-reader-at-a-time. Under
    // the pool, N readers parallelize; aggregate should scale with
    // the pool size up to CPU count. 3× is a CI-safe floor: local
    // runs typically see 8–10× on Apple Silicon.
    assert!(
        scale >= 3.0,
        "pool shape did not deliver ≥3× the concurrent-reader throughput \
         of the pre-15b Mutex<Connection> baseline. Got scale={scale:.2}× \
         (mutex={mutex_reads}, pool={pool_reads}). Regression: readers may \
         be serializing on a hidden shared lock.",
    );

    // Assertion 3 (belt-and-suspenders per bead `ley-line-open-14b7a2`):
    // absolute pool floor. If the pool shape regresses to serialized
    // behavior, aggregate collapses toward the mutex baseline (~40–60k
    // reads/1.5s). 50k is above that failure mode but well below the
    // stable ~124k the pool delivers — a real regression fails here
    // even if the ratio-vs-mutex assertion happens to survive a lucky
    // co-tenant scheduling window.
    assert!(
        pool_reads >= 50_000,
        "pool aggregate throughput collapsed to serialized levels: got \
         {pool_reads} reads in {DURATION:?}, floor is 50000. This is a \
         hard regression indicator independent of the mutex baseline.",
    );
}

// ── (2) Reader pragma enforcement ───────────────────────────────────

/// Load-bearing invariant for the reader pool: `query_only=ON` must be
/// applied to every checkout so a misclassified write path fails-loud
/// with `SQLITE_READONLY`. Without this pragma, a `with_read` closure
/// that accidentally issues `CREATE TABLE` would silently mutate state
/// through the reader connection — SQLite would happily let it happen
/// on a read-write file. The bead's fail-loud contract requires that
/// writes fail with an actionable error, not silently succeed.
#[test]
fn reader_connection_rejects_writes_with_readonly_error() {
    let dir = TempDir::new().unwrap();
    let live_db = seeded_live_db(&dir, 100, 2);

    let reader = live_db.reader_pool.get().unwrap();
    let err = reader
        .execute("CREATE TABLE forbidden (x INTEGER)", [])
        .expect_err("CREATE TABLE from a reader must error");
    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("readonly"),
        "reader-side DDL should return SQLITE_READONLY; got: {msg}",
    );

    // Also verify INSERT fails-loud — a write into an existing table.
    let err = reader
        .execute(
            "INSERT INTO nodes (kind, payload) VALUES ('nope', ?1)",
            [&vec![0u8; 8]],
        )
        .expect_err("INSERT from a reader must error");
    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("readonly"),
        "reader-side INSERT should return SQLITE_READONLY; got: {msg}",
    );
}

// ── (3) Pool exhaustion behavior ────────────────────────────────────

/// Load-bearing invariant: an (N+1)th reader checkout blocks briefly
/// (until an in-flight reader returns to the pool via Drop), never
/// fails. r2d2's default `connection_timeout` is 30 s — long enough
/// that any correct checkout ordering succeeds, short enough that a
/// bug (permanently exhausted pool) would timeout instead of hanging
/// forever. This test spawns N+1 short-lived checkouts and confirms
/// the (N+1)th observes non-zero wait but eventually succeeds.
#[test]
fn pool_exhaustion_blocks_not_fails() {
    const POOL_SIZE: u32 = 3;
    let dir = TempDir::new().unwrap();
    let live_db = Arc::new(seeded_live_db(&dir, 100, POOL_SIZE));

    // Hold POOL_SIZE checkouts on the main thread. Spawned checkout
    // must wait — the pool has no free connections until we drop.
    let held: Vec<_> = (0..POOL_SIZE)
        .map(|_| live_db.reader_pool.get().unwrap())
        .collect();

    let live_bg = live_db.clone();
    let waited = Arc::new(AtomicU64::new(0));
    let waited_bg = waited.clone();
    let bg = thread::spawn(move || {
        let t0 = Instant::now();
        let _c = live_bg
            .reader_pool
            .get()
            .expect("(N+1)th checkout must eventually succeed");
        waited_bg.store(t0.elapsed().as_millis() as u64, Ordering::Relaxed);
    });

    // Let the background thread block for ~50 ms, then release. This
    // window would be zero if the pool weren't actually enforcing
    // capacity (e.g. someone bumped max_size at build time to hide the
    // constraint).
    thread::sleep(Duration::from_millis(50));
    drop(held);
    bg.join().expect("bg thread must not panic");
    let waited_ms = waited.load(Ordering::Relaxed);
    assert!(
        waited_ms >= 40,
        "(N+1)th checkout should have blocked ~50 ms; observed {waited_ms} ms. \
         Regression: pool capacity is not being enforced.",
    );
    assert!(
        waited_ms < 5000,
        "(N+1)th checkout blocked too long ({waited_ms} ms) — expected \
         handoff within ~50 ms of the held connections being dropped.",
    );
}

// ── (4) Writer serialization ────────────────────────────────────────

/// Load-bearing invariant: two concurrent `with_write` invocations
/// execute serially and never corrupt state. SQLite WAL serializes
/// writers at the file level; the `Mutex<Connection>` on the writer
/// mirrors that constraint at the Rust level. We verify by driving
/// two writer threads that each increment a counter row 100 times.
/// If serialization holds, the final counter equals 200 exactly. If
/// the mutex leaks or gets bypassed, we'd see lost updates or
/// SQLITE_BUSY errors.
#[test]
fn concurrent_writers_serialize_never_corrupt_state() {
    let dir = TempDir::new().unwrap();
    let live_db = Arc::new(seeded_live_db(&dir, 0, 4));

    // Seed a counter row via the writer.
    {
        let guard = live_db.writer.lock().unwrap();
        guard
            .execute_batch(
                "CREATE TABLE counter (id INTEGER PRIMARY KEY, value INTEGER);
                 INSERT INTO counter (id, value) VALUES (1, 0);",
            )
            .unwrap();
    }

    let n_writers = 2;
    let n_incs = 100;
    let mut handles = Vec::with_capacity(n_writers);
    for _ in 0..n_writers {
        let db = live_db.clone();
        handles.push(thread::spawn(move || {
            for _ in 0..n_incs {
                let guard = db.writer.lock().unwrap();
                guard
                    .execute("UPDATE counter SET value = value + 1 WHERE id = 1", [])
                    .expect("writer UPDATE must succeed under serialization");
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let final_value: i64 = live_db
        .writer
        .lock()
        .unwrap()
        .query_row("SELECT value FROM counter WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        final_value,
        (n_writers * n_incs) as i64,
        "writer serialization lost updates — got {final_value}, expected {}",
        n_writers * n_incs,
    );
}

// ── (5) Pragma consistency across the pool ──────────────────────────

/// Load-bearing invariant: every reader connection in the pool has
/// both `journal_mode=wal` and `query_only=ON` after startup. If the
/// `with_init` closure silently drops one pragma (e.g. a refactor that
/// swaps the pragma set for a shorter list), reads could either lose
/// WAL's concurrency (falling back to DELETE-mode locking) or the
/// fail-loud write-rejection. Either regression is silent otherwise.
///
/// r2d2 fills to `min_idle` eagerly on `build()`, so we can iterate
/// the pool size and observe every connection's pragmas without
/// racing with lazy initialization.
#[test]
fn every_pool_connection_has_wal_and_query_only_pragmas() {
    const POOL_SIZE: u32 = 6;
    let dir = TempDir::new().unwrap();
    let live_db = seeded_live_db(&dir, 100, POOL_SIZE);

    // Hold all POOL_SIZE checkouts simultaneously so each observation
    // hits a distinct connection.
    let checkouts: Vec<_> = (0..POOL_SIZE)
        .map(|_| live_db.reader_pool.get().unwrap())
        .collect();

    for (i, conn) in checkouts.iter().enumerate() {
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            mode.to_lowercase(),
            "wal",
            "reader connection #{i} must observe journal_mode=WAL (per-DB \
             property set by the writer), got {mode:?}",
        );
        let query_only: i64 = conn
            .query_row("PRAGMA query_only", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            query_only, 1,
            "reader connection #{i} must have query_only=ON, got {query_only}",
        );
        // Confirm busy_timeout also stuck — this is the safety net
        // for transient checkpoint contention.
        let busy_timeout: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert!(
            busy_timeout >= 5000,
            "reader connection #{i} must have busy_timeout ≥ 5000ms, \
             got {busy_timeout}ms",
        );
    }
}
