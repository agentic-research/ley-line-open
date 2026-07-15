//! Living-database container — reader pool + dedicated writer connection.
//!
//! Bead `ley-line-open-f0239d` (WAL adoption 15b). Pairs with 15a
//! (bead `ley-line-open-98fb67`): 15a made the live db file-backed with
//! `journal_mode=WAL`; 15b splits the single `Mutex<Connection>` into
//! N pooled reader connections + 1 dedicated writer so the WAL win
//! (concurrent readers making progress against a single-writer db) is
//! actually realized end-to-end.
//!
//! Empirical target: `docs/research/2026-05-08-workerd-wal-sqlite-experiment.md`
//! measured p99 reads at 290–375 µs on file-backed WAL@N=10 readers +
//! 1 writer vs 120–250 ms p99 on DELETE-journal — ~600× improvement.
//! The single-`Mutex<Connection>` daemon shape prior to this change
//! would have serialized all reads at the Rust level, hiding the win.
//!
//! Design:
//! - **Reader pool**: `r2d2::Pool<SqliteConnectionManager>` with
//!   `PRAGMA query_only=ON` and `PRAGMA busy_timeout=5000` applied to
//!   every checkout via `with_init`. Writes on a reader connection
//!   fail-loud with `SQLITE_READONLY`.
//! - **Writer**: single `parking_lot::Mutex<rusqlite::Connection>`
//!   mirroring SQLite WAL's own writer-serialization at the Rust
//!   level. This is not a limitation — it *matches* SQLite's
//!   guarantee.
//!
//! Pool size defaults to `min(10, available_parallelism())`. The bench
//! showed N=10 as the sweet spot; higher hits diminishing returns as
//! per-connection overhead outweighs concurrency gain.

use parking_lot::Mutex;
use std::path::Path;

use anyhow::{Context, Result};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;

/// The daemon's living-database container. Replaces the pre-15b
/// `Mutex<Connection>` field on `DaemonContext`.
///
/// Field access is `pub` so helper functions in `cmd_daemon.rs` that
/// need the raw writer mutex (`snapshot_or_log`, `try_snapshot_or_log`,
/// `read_total_changes`) can pass `&live_db.writer` without going
/// through `with_read` / `with_write`. Those helpers pre-date the
/// method API and their signatures were left as `&Mutex<Connection>`
/// on purpose so the migration is a mechanical rename, not a rewrite.
pub struct LiveDb {
    /// Shared read-only connection pool. `with_read` checks out one
    /// connection per closure invocation; connections return to the
    /// pool on Drop. Each pool connection has `query_only=ON` so a
    /// misclassified write path fails with `SQLITE_READONLY` instead
    /// of silently mutating state.
    pub reader_pool: r2d2::Pool<SqliteConnectionManager>,
    /// Single dedicated writer connection. Held exclusively by
    /// `with_write` — SQLite WAL serializes writers at the file
    /// level, and this `Mutex` mirrors that constraint at the Rust
    /// level. There is no path to run two write transactions
    /// concurrently through this API; a caller that tries will block
    /// on the mutex, matching what SQLite would do at the file lock.
    pub writer: Mutex<Connection>,
}

/// Compute the default reader pool size.
///
/// `min(10, available_parallelism())` with a floor of 2. The bench in
/// `docs/research/2026-05-08-workerd-wal-sqlite-experiment.md` showed
/// N=10 as the sweet spot on Apple Silicon; smaller machines cap at
/// their core count. Floor of 2 so a wonky `available_parallelism()`
/// (single-thread runners, containers with 1 CPU) can't collapse the
/// pool to a single connection — that would defeat the whole point.
pub fn default_pool_size() -> u32 {
    const MAX_POOL_SIZE: u32 = 10;
    const MIN_POOL_SIZE: u32 = 2;

    let cores = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4);
    cores.clamp(MIN_POOL_SIZE, MAX_POOL_SIZE)
}

impl LiveDb {
    /// Build a `LiveDb` from a writer connection + a file-backed db path.
    ///
    /// The writer connection is passed in already-configured — 15a's
    /// `configure_wal` runs on it in `init_living_db` before this
    /// constructor sees it. The reader pool is built fresh here,
    /// attaching to the same `.live.db` file with `query_only=ON` +
    /// `busy_timeout=5000` applied per-connection.
    ///
    /// # Errors
    ///
    /// - Pool build fails when the file cannot be opened for read (missing
    ///   file, permission error, corrupt header). The writer connection
    ///   must have been created against `path` before this call so the
    ///   file exists.
    /// - Any `with_init` pragma failure aborts pool build.
    pub fn new(writer: Connection, path: &Path, pool_size: u32) -> Result<Self> {
        // Reader pragmas applied on every pool checkout:
        // - `query_only=ON` — writes fail SQLITE_READONLY. Belt-and-braces
        //   against a misclassified `with_read` path that tries to mutate.
        // - `busy_timeout=5000` — wait 5s rather than fail immediately on
        //   transient WAL contention. WAL rarely blocks readers, but a
        //   long checkpoint under load can briefly acquire the writer
        //   lock; we'd rather wait than surface the transient failure.
        //
        // We deliberately do NOT set `journal_mode=WAL` on readers —
        // journal_mode is per-database-file, not per-connection, and once
        // the writer sets it, every subsequent connection sees WAL.
        // Setting it on a read-only connection also fails silently in
        // some SQLite versions, which would mask real WAL regressions.
        let manager = SqliteConnectionManager::file(path).with_init(|conn| {
            conn.pragma_update(None, "query_only", "ON")?;
            conn.pragma_update(None, "busy_timeout", 5000i64)?;
            Ok(())
        });

        let reader_pool = r2d2::Pool::builder()
            .max_size(pool_size)
            // Eager min_idle so the pragma-consistency invariant holds
            // at startup — the wal_pool_concurrent_readers_daemon_test
            // suite asserts every pool connection has journal_mode=wal
            // + query_only=ON, and lazy fill would let that pass by
            // never having opened a connection. Also warms the pool
            // for the p99-read-latency assertion; a cold pool would
            // pay a first-open cost inside the measured window.
            .min_idle(Some(pool_size))
            .build(manager)
            .with_context(|| {
                format!(
                    "build reader connection pool for live_db at {}",
                    path.display()
                )
            })?;

        Ok(Self {
            reader_pool,
            writer: Mutex::new(writer),
        })
    }

    /// Open a fresh file-backed WAL LiveDb at `path`. Convenience for
    /// integration tests + fixtures that need a working LiveDb without
    /// running the full `init_living_db` recovery pipeline.
    ///
    /// Applies the same WAL pragma set the daemon uses in production
    /// (`journal_mode=WAL`, `synchronous=NORMAL`) so tests reflect
    /// real behavior. Panics on failure — meant for tests, not
    /// production.
    ///
    /// Pool size defaults to 4 for test workloads — enough to
    /// exercise concurrency without opening more file handles than
    /// a typical test needs.
    pub fn open_fresh_for_test(path: &Path) -> Self {
        let writer = Connection::open(path).expect("open test live db");
        let mode: String = writer
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .expect("set journal_mode=WAL");
        assert_eq!(
            mode.to_lowercase(),
            "wal",
            "test live db must accept WAL pragma",
        );
        writer
            .pragma_update(None, "synchronous", "NORMAL")
            .expect("set synchronous=NORMAL");
        Self::new(writer, path, 4).expect("build test LiveDb")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Build a fresh WAL-configured writer connection at `path`. Mirrors
    /// what `init_living_db` produces so tests exercise the same shape
    /// as production.
    fn wal_writer(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        conn.pragma_update(None, "synchronous", "NORMAL").unwrap();
        conn
    }

    #[test]
    fn default_pool_size_respects_bounds() {
        let size = default_pool_size();
        assert!(size >= 2, "pool size floor is 2, got {size}");
        assert!(size <= 10, "pool size cap is 10, got {size}");
    }

    #[test]
    fn new_populates_reader_pool_with_query_only_readers() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("pool_test.db");
        let writer = wal_writer(&path);
        let live = LiveDb::new(writer, &path, 4).expect("build LiveDb");

        // Every checkout must report `query_only = ON` (returned as
        // "1"/"0" or "on"/"off" depending on SQLite version; both are
        // acceptable when the numeric-1 case is truthy).
        for _ in 0..4 {
            let conn = live.reader_pool.get().expect("checkout reader");
            let query_only: i64 = conn
                .query_row("PRAGMA query_only", [], |r| r.get(0))
                .unwrap();
            assert_eq!(query_only, 1, "every reader must have query_only=ON");
        }
    }

    #[test]
    fn reader_pool_write_fails_readonly() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("readonly_test.db");
        let writer = wal_writer(&path);
        // Seed a table via the writer so the reader has something to
        // legitimately query — the point is that a WRITE from a reader
        // fails, not that reads fail on an empty db.
        writer
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);")
            .unwrap();
        let live = LiveDb::new(writer, &path, 2).unwrap();

        let reader = live.reader_pool.get().unwrap();
        let err = reader
            .execute("INSERT INTO t (v) VALUES ('nope')", [])
            .expect_err("write through reader must fail");
        // rusqlite renders `SQLITE_READONLY` errors with the string
        // "readonly" in the message; assert on that rather than
        // exact enum variant (which changed shape across rusqlite
        // versions).
        let msg = format!("{err}");
        assert!(
            msg.to_lowercase().contains("readonly"),
            "expected SQLITE_READONLY error, got: {msg}",
        );
    }
}
