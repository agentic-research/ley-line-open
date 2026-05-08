//! H1 falsification: file-backed SQLite WAL with N concurrent readers + 1 writer.
//!
//! ── Hypothesis under test ─────────────────────────────────────────────
//! H1: SQLite WAL mode on a file-backed database gives concurrent readers
//!     acceptable latency (sub-millisecond p50, single-digit ms p99) without
//!     blocking writers.
//!
//! Pass: p99 reads < 5ms, write throughput ≥ 80% of solo-writer baseline.
//! Fail: WAL doesn't materially reduce contention vs DELETE journal mode.
//!
//! ── Why this matters ──────────────────────────────────────────────────
//! ley-line-open's daemon today owns a `Mutex<Connection>` over a
//! `:memory:` SQLite DB. `:memory:` SILENTLY IGNORES `PRAGMA journal_mode
//! = WAL` (verified in wal_snapshot_experiment.rs preamble). The whole
//! "WAL would help" story is only testable on a *file-backed* db. This
//! bench tests that the file-backed direction actually delivers WAL's
//! advertised concurrency.
//!
//! Each reader opens its OWN connection (WAL's whole point is that each
//! reader gets a private snapshot via the WAL header, while the writer
//! appends new pages without blocking them). A `Mutex<Connection>` would
//! defeat WAL by serializing connections at the Rust level — that's
//! daemon's CURRENT shape, and what we'd need to undo to use WAL.
//!
//! ── Modes compared ────────────────────────────────────────────────────
//! - DELETE: the SQLite default, same on disk vs `:memory:` semantics-wise
//!   (rollback journal). Whole-DB write lock blocks readers.
//! - WAL: writers append to -wal file; readers read from main DB at the
//!   header-snapshot they observed at BEGIN; one writer + N readers
//!   should not block.
//!
//! Run with:
//!     cargo bench --bench wal_concurrent_readers -p leyline-cli-lib

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use tempfile::TempDir;

// ── Fixture ────────────────────────────────────────────────────────────

/// Build a SQLite db on disk at `path` containing N rows in a `nodes`
/// table with a 256-byte payload each. Approximates the shape of the
/// daemon's `nodes` table without the LSP enrichment columns.
fn build_db(path: &Path, n_rows: usize, journal_mode: &str) -> Connection {
    let conn = Connection::open(path).unwrap();
    // Set journal mode FIRST. PRAGMA journal_mode is sticky for WAL.
    let mode: String = conn
        .query_row(&format!("PRAGMA journal_mode = {journal_mode}"), [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        mode.to_lowercase(),
        journal_mode.to_lowercase(),
        "PRAGMA journal_mode = {journal_mode} did not stick — got '{mode}'",
    );
    // synchronous = NORMAL is the recommended WAL pairing (FULL is
    // overkill for WAL durability semantics; OFF would unfairly bias
    // numbers). Both modes get this same setting so the comparison is
    // about journal-mode behavior, not fsync frequency.
    conn.pragma_update(None, "synchronous", "NORMAL").unwrap();

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS nodes (
             id INTEGER PRIMARY KEY,
             kind TEXT NOT NULL,
             payload BLOB NOT NULL
         )",
    )
    .unwrap();

    let payload = vec![0xCDu8; 256];
    let tx = conn.unchecked_transaction().unwrap();
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
    conn
}

fn open_reader(path: &Path) -> Connection {
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .unwrap();
    // Hint: query planner shouldn't need this, but cheap insurance against
    // a stale page cache making the first read look fast.
    conn.pragma_update(None, "cache_size", -2000i64).unwrap();
    conn
}

// ── Helpers ────────────────────────────────────────────────────────────

fn percentile(samples: &mut [Duration], p: f64) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }
    samples.sort();
    let idx = ((samples.len() as f64 - 1.0) * p).round() as usize;
    samples[idx]
}

fn mean(samples: &[Duration]) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }
    let sum: u128 = samples.iter().map(|d| d.as_nanos()).sum();
    Duration::from_nanos((sum / samples.len() as u128) as u64)
}

// ── Phase A: solo-writer baseline (no readers) ─────────────────────────

fn solo_writer_throughput(path: &Path, duration: Duration) -> f64 {
    let conn = Connection::open(path).unwrap();
    conn.pragma_update(None, "synchronous", "NORMAL").unwrap();
    let stop = Instant::now() + duration;
    let mut count = 0u64;
    let payload = vec![0xEFu8; 256];
    let mut stmt = conn
        .prepare("INSERT INTO nodes (kind, payload) VALUES (?1, ?2)")
        .unwrap();
    while Instant::now() < stop {
        stmt.execute(rusqlite::params!["solo", &payload]).unwrap();
        count += 1;
    }
    count as f64 / duration.as_secs_f64()
}

// ── Phase B: 1 writer + N readers ──────────────────────────────────────

#[derive(Debug)]
struct ConcurrencyResult {
    journal_mode: &'static str,
    n_readers: usize,
    duration: Duration,
    write_throughput: f64,
    total_reads: u64,
    read_throughput: f64,
    read_p50: Duration,
    read_p99: Duration,
    read_max: Duration,
    write_p50: Duration,
    write_p99: Duration,
}

fn run_concurrency(
    path: &Path,
    journal_mode: &'static str,
    n_readers: usize,
    duration: Duration,
    n_rows: usize,
) -> ConcurrencyResult {
    let stop = Arc::new(AtomicBool::new(false));
    let writes = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();

    // ── Writer thread ──────────────────────────────────────────────────
    let writer_path = path.to_path_buf();
    let stop_w = stop.clone();
    let writes_w = writes.clone();
    let writer_handle = std::thread::spawn(move || {
        let conn = Connection::open(&writer_path).unwrap();
        conn.pragma_update(None, "synchronous", "NORMAL").unwrap();
        let mut stmt = conn
            .prepare("INSERT INTO nodes (kind, payload) VALUES (?1, ?2)")
            .unwrap();
        let payload = vec![0x77u8; 256];
        let mut latencies: Vec<Duration> = Vec::with_capacity(50_000);
        while !stop_w.load(Ordering::Relaxed) {
            let t0 = Instant::now();
            // BUSY: WAL allows concurrent readers but only one writer.
            // With 1 writer we never see SQLITE_BUSY from readers; with
            // DELETE we MAY see BUSY because readers acquire SHARED lock.
            // Loop on BUSY is fair to both modes — that's the actual cost.
            loop {
                match stmt.execute(rusqlite::params!["w", &payload]) {
                    Ok(_) => break,
                    Err(rusqlite::Error::SqliteFailure(e, _))
                        if e.code == rusqlite::ErrorCode::DatabaseBusy =>
                    {
                        std::thread::yield_now();
                    }
                    Err(e) => panic!("writer error: {e}"),
                }
            }
            latencies.push(t0.elapsed());
            writes_w.fetch_add(1, Ordering::Relaxed);
        }
        latencies
    });

    // ── Reader threads ─────────────────────────────────────────────────
    for r in 0..n_readers {
        let reader_path = path.to_path_buf();
        let stop_r = stop.clone();
        handles.push(std::thread::spawn(move || {
            let conn = open_reader(&reader_path);
            // Each reader picks pseudo-random ids in the existing range so
            // we hit a representative cache mix.
            let mut stmt = conn
                .prepare("SELECT kind, payload FROM nodes WHERE id = ?1")
                .unwrap();
            let mut latencies: Vec<Duration> = Vec::with_capacity(200_000);
            let mut rng_state = (r as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xDEAD_BEEF;
            while !stop_r.load(Ordering::Relaxed) {
                rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
                let id = ((rng_state >> 33) as usize % n_rows) as i64 + 1;
                let t0 = Instant::now();
                loop {
                    match stmt.query_row(rusqlite::params![id], |row| {
                        let _kind: String = row.get(0)?;
                        let _payload: Vec<u8> = row.get(1)?;
                        Ok(())
                    }) {
                        Ok(_) => break,
                        Err(rusqlite::Error::QueryReturnedNoRows) => break,
                        Err(rusqlite::Error::SqliteFailure(e, _))
                            if e.code == rusqlite::ErrorCode::DatabaseBusy =>
                        {
                            std::thread::yield_now();
                        }
                        Err(e) => panic!("reader error: {e}"),
                    }
                }
                latencies.push(t0.elapsed());
            }
            latencies
        }));
    }

    std::thread::sleep(duration);
    stop.store(true, Ordering::Relaxed);

    let mut write_lat = writer_handle.join().unwrap();
    let mut read_lat: Vec<Duration> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();

    let total_writes = writes.load(Ordering::Relaxed);
    let total_reads = read_lat.len() as u64;
    let secs = duration.as_secs_f64();

    let write_p50 = percentile(&mut write_lat, 0.50);
    let write_p99 = percentile(&mut write_lat, 0.99);
    let read_p50 = percentile(&mut read_lat, 0.50);
    let read_p99 = percentile(&mut read_lat, 0.99);
    let read_max = read_lat.iter().copied().max().unwrap_or(Duration::ZERO);

    ConcurrencyResult {
        journal_mode,
        n_readers,
        duration,
        write_throughput: total_writes as f64 / secs,
        total_reads,
        read_throughput: total_reads as f64 / secs,
        read_p50,
        read_p99,
        read_max,
        write_p50,
        write_p99,
    }
}

fn print_table(rows: &[ConcurrencyResult]) {
    println!(
        "\n  {:<8} {:>4} {:>10} {:>11} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "mode",
        "N_r",
        "writes/s",
        "reads/s",
        "r p50",
        "r p99",
        "r max",
        "w p50",
        "w p99",
    );
    println!("  {}", "─".repeat(90));
    for r in rows {
        println!(
            "  {:<8} {:>4} {:>10.0} {:>11.0} {:>9.2?} {:>9.2?} {:>9.2?} {:>9.2?} {:>9.2?}",
            r.journal_mode,
            r.n_readers,
            r.write_throughput,
            r.read_throughput,
            r.read_p50,
            r.read_p99,
            r.read_max,
            r.write_p50,
            r.write_p99,
        );
    }
}

// ── Driver ─────────────────────────────────────────────────────────────

fn run_for_mode(
    journal_mode: &'static str,
    n_rows: usize,
    duration: Duration,
) -> (f64, Vec<ConcurrencyResult>) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join(format!("h1_{journal_mode}.db"));
    {
        let _conn = build_db(&path, n_rows, journal_mode);
    } // close so writer can re-open

    eprintln!(
        "fixture: {} mode, {} rows, db size = {} KiB",
        journal_mode,
        n_rows,
        std::fs::metadata(&path).unwrap().len() / 1024,
    );

    let solo = solo_writer_throughput(&path, duration);
    eprintln!("  solo writer baseline: {:.0} writes/sec", solo);

    let mut rows = Vec::new();
    for &n in &[1usize, 4, 10] {
        let r = run_concurrency(&path, journal_mode, n, duration, n_rows);
        rows.push(r);
    }
    (solo, rows)
}

fn main() {
    eprintln!("=== H1: file-backed SQLite WAL vs DELETE concurrency ===");
    eprintln!("Hypothesis: WAL gives p99 reads < 5ms with N=10 concurrent readers");
    eprintln!("            AND write throughput ≥ 80% of solo baseline.");
    eprintln!();

    let n_rows = 50_000;
    let duration = Duration::from_secs(3);

    let (delete_solo, delete_rows) = run_for_mode("DELETE", n_rows, duration);
    let (wal_solo, wal_rows) = run_for_mode("WAL", n_rows, duration);

    println!("\n══ DELETE (rollback journal) ══");
    println!("  solo writer: {:.0} writes/sec", delete_solo);
    print_table(&delete_rows);

    println!("\n══ WAL ══");
    println!("  solo writer: {:.0} writes/sec", wal_solo);
    print_table(&wal_rows);

    // ── Verdict ────────────────────────────────────────────────────────
    let wal_n10 = wal_rows.iter().find(|r| r.n_readers == 10).unwrap();
    let delete_n10 = delete_rows.iter().find(|r| r.n_readers == 10).unwrap();

    println!("\n══ H1 verdict ══");
    println!(
        "  WAL @ N=10: read p99 = {:?}, write throughput = {:.0}/s ({:.1}% of solo)",
        wal_n10.read_p99,
        wal_n10.write_throughput,
        100.0 * wal_n10.write_throughput / wal_solo,
    );
    println!(
        "  DELETE @ N=10: read p99 = {:?}, write throughput = {:.0}/s ({:.1}% of solo)",
        delete_n10.read_p99,
        delete_n10.write_throughput,
        100.0 * delete_n10.write_throughput / delete_solo,
    );

    let read_pass = wal_n10.read_p99 < Duration::from_millis(5);
    let write_pass = wal_n10.write_throughput >= 0.8 * wal_solo;
    let wal_better_p99 = wal_n10.read_p99 < delete_n10.read_p99;

    println!("\n  Pass criteria:");
    println!(
        "    WAL p99 reads < 5ms             : {}",
        if read_pass { "PASS" } else { "FAIL" }
    );
    println!(
        "    WAL writes ≥ 80% of solo        : {}",
        if write_pass { "PASS" } else { "FAIL" }
    );
    println!(
        "    WAL p99 < DELETE p99 (sanity)   : {}",
        if wal_better_p99 { "PASS" } else { "FAIL" }
    );

    let overall = read_pass && write_pass && wal_better_p99;
    println!(
        "\n  H1 overall: {}",
        if overall { "PASS" } else { "FAIL" }
    );
}
