//! Empirical experiment: measure the lock-hold-time of `snapshot_to_arena`
//! and the contention it produces on writer/reader threads.
//!
//! Background — premise under test:
//!     "Mutex<SqliteGraph> is held during conn.serialize() inside
//!      snapshot_to_arena. WAL mode + a separate read-connection for
//!      serialize would shorten the critical section from seconds to ms,
//!      possibly eliminating the need for a lock-free protocol (T6)."
//!
//! Empirical reality (verified before writing this bench):
//!     `:memory:` SQLite databases SILENTLY IGNORE `PRAGMA journal_mode = WAL`.
//!     The pragma returns "memory" no matter what is requested (SQLite docs:
//!     "WAL mode is not supported for in-memory databases."). The current
//!     daemon (`cmd_daemon.rs:200`) opens the live_db as `:memory:`, so the
//!     premise as written is *not configurable*. Re-architecting the live_db
//!     to be file-backed is a much larger change with portability + crash
//!     semantics implications.
//!
//! This bench therefore tests the most charitable interpretation of the
//! premise — the part that IS achievable under the current architecture:
//!
//!   ── lock-hold-time decomposition ──
//!   How much of `snapshot_to_arena`'s lock-held window is dominated by
//!   `conn.serialize()` itself versus the arena tail (resize +
//!   create_arena + write_to_arena + set_arena)? If serialize dominates,
//!   no lock-shortening trick on the current architecture matters. If the
//!   tail dominates, releasing the writer lock immediately after
//!   `serialize()` returns the bytes is a free win.
//!
//!   ── contention measurement ──
//!   With concurrent writers and snapshots, what is the writer p50/p99
//!   wait-for-lock latency, and what is total write throughput, for two
//!   shapes of `snapshot_to_arena`:
//!     (a) baseline:  hold lock through serialize + arena tail   (current)
//!     (b) shortened: hold lock only for serialize; arena tail unlocked
//!
//! Run with:
//!     cargo run --release --bin wal_snapshot_experiment \
//!         --features bench-bin -p leyline-cli-lib

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use leyline_core::{ArenaHeader, Controller, create_arena, write_to_arena};
use rusqlite::{Connection, DatabaseName};
use tempfile::TempDir;

const ARENA_GROWTH_FACTOR: u64 = 2;
const ARENA_HEADROOM_BYTES: u64 = 4 * 1024 * 1024;

// ── Fixture builder ────────────────────────────────────────────────────

/// Build a synthetic source tree of `n_files` Python files, each
/// approximately `bytes_per_file` bytes of parseable Python.
fn build_source_tree(root: &Path, n_files: usize, bytes_per_file: usize) {
    // ~80 chars per line of plausible-looking python; repeat to size.
    let line = "def fn_{i}_{j}(x, y, z=None):\n    return (x + y) * (z or 1) + {j}\n";
    let lines_needed = bytes_per_file / line.len() + 1;

    for i in 0..n_files {
        // Spread across a few subdirs so we exercise nested paths.
        let subdir = root.join(format!("pkg{}", i / 100));
        std::fs::create_dir_all(&subdir).unwrap();
        let mut content = String::with_capacity(bytes_per_file + 256);
        content.push_str("# auto-generated\n");
        for j in 0..lines_needed {
            content.push_str(
                &line
                    .replace("{i}", &i.to_string())
                    .replace("{j}", &j.to_string()),
            );
        }
        std::fs::write(subdir.join(format!("mod_{i}.py")), content).unwrap();
    }
}

// ── Two snapshot shapes under test ─────────────────────────────────────

/// Shape A: BASELINE.
///
/// Mirrors `cmd_daemon::snapshot_or_log` exactly — lock held through
/// `serialize()` AND through the arena tail (resize + create + write +
/// set_arena). `serialize()` returns SQLITE_SERIALIZE_NOCOPY on `:memory:`
/// connections, so the bytes are borrowed from the live connection — the
/// lock MUST be held while `db_bytes` is in scope.
fn snapshot_baseline(
    live_db: &Mutex<Connection>,
    ctrl_path: &Path,
) -> (Duration, Duration, usize) {
    let lock_acquired = Instant::now();
    let guard = live_db.lock().unwrap();

    let serialize_start = Instant::now();
    let db_bytes = guard
        .serialize(DatabaseName::Main)
        .expect("serialize");
    let serialize_dur = serialize_start.elapsed();

    let mut ctrl = Controller::open_or_create(ctrl_path).expect("ctrl");
    let arena_path = ctrl.arena_path();
    let arena_size = ctrl.arena_size();

    let min_arena = ArenaHeader::HEADER_SIZE
        + db_bytes.len() as u64 * ARENA_GROWTH_FACTOR
        + ARENA_HEADROOM_BYTES;
    let arena_size = if min_arena > arena_size {
        let _ = ctrl.set_arena(&arena_path, min_arena, ctrl.generation());
        min_arena
    } else {
        arena_size
    };

    let mut mmap =
        create_arena(Path::new(&arena_path), arena_size).expect("create_arena");
    write_to_arena(&mut mmap, &db_bytes).expect("write_to_arena");
    let new_gen = ctrl.generation() + 1;
    ctrl.set_arena(&arena_path, arena_size, new_gen)
        .expect("set_arena");

    let n_bytes = db_bytes.len();
    let total = lock_acquired.elapsed();
    drop(db_bytes);
    drop(guard);
    (total, serialize_dur, n_bytes)
}

/// Shape B: SHORTENED.
///
/// Lock is held only for `serialize()` + a `to_vec()` copy out of the
/// SQLITE_SERIALIZE_NOCOPY shared buffer. After the lock is released the
/// arena tail (resize + create_arena + write_to_arena + set_arena) runs
/// without contending with writers/readers.
///
/// The cost added by this shape, compared to BASELINE, is one full memcpy
/// of the serialized DB (under the lock). The cost removed is the entire
/// arena tail (under the lock). The bench measures whether the trade is
/// favorable.
fn snapshot_shortened(
    live_db: &Mutex<Connection>,
    ctrl_path: &Path,
) -> (Duration, Duration, usize) {
    let lock_acquired = Instant::now();
    let guard = live_db.lock().unwrap();

    let serialize_start = Instant::now();
    let db_bytes_owned: Vec<u8> = {
        let data = guard.serialize(DatabaseName::Main).expect("serialize");
        data.to_vec()
    };
    let serialize_dur = serialize_start.elapsed();

    // Critical section ENDS here.
    let lock_held = lock_acquired.elapsed();
    drop(guard);

    // Arena tail — runs without any lock held.
    let mut ctrl = Controller::open_or_create(ctrl_path).expect("ctrl");
    let arena_path = ctrl.arena_path();
    let arena_size = ctrl.arena_size();

    let min_arena = ArenaHeader::HEADER_SIZE
        + db_bytes_owned.len() as u64 * ARENA_GROWTH_FACTOR
        + ARENA_HEADROOM_BYTES;
    let arena_size = if min_arena > arena_size {
        let _ = ctrl.set_arena(&arena_path, min_arena, ctrl.generation());
        min_arena
    } else {
        arena_size
    };

    let mut mmap =
        create_arena(Path::new(&arena_path), arena_size).expect("create_arena");
    write_to_arena(&mut mmap, &db_bytes_owned).expect("write_to_arena");
    let new_gen = ctrl.generation() + 1;
    ctrl.set_arena(&arena_path, arena_size, new_gen)
        .expect("set_arena");

    (lock_held, serialize_dur, db_bytes_owned.len())
}

// ── Helpers ────────────────────────────────────────────────────────────

fn percentile(samples: &mut [Duration], p: f64) -> Duration {
    samples.sort();
    if samples.is_empty() {
        return Duration::ZERO;
    }
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

// ── Fixture: prepare a populated live_db + arena ───────────────────────

fn build_fixture(
    n_files: usize,
    bytes_per_file: usize,
) -> (TempDir, Connection, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    build_source_tree(&src, n_files, bytes_per_file);

    // Arena + controller.
    let arena_path = dir.path().join("test.arena");
    let ctrl_path = dir.path().join("test.ctrl");
    let _mmap = create_arena(&arena_path, 2 * 1024 * 1024).unwrap();
    {
        let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
        ctrl.set_arena(&arena_path.to_string_lossy(), 2 * 1024 * 1024, 0)
            .unwrap();
    }

    // Build the in-memory live_db and parse the tree once.
    let conn = Connection::open_in_memory().unwrap();
    let parse_start = Instant::now();
    let result = leyline_cli_lib::cmd_parse::parse_into_conn(
        &conn,
        &src,
        Some("python"),
        None,
    )
    .expect("parse_into_conn");
    eprintln!(
        "fixture: parsed {} files ({} parsed, {} unchanged) in {:.2?}",
        n_files,
        result.parsed,
        result.unchanged,
        parse_start.elapsed()
    );

    (dir, conn, ctrl_path)
}

// ── Phase 1: lock-hold decomposition (no contention) ───────────────────

fn phase1_decomposition(n_files: usize, bytes_per_file: usize, iters: usize) {
    println!("\n══ Phase 1: lock-hold decomposition (single-thread) ══");
    println!(
        "fixture: {} files × {} bytes/file (target ~{} MB raw)",
        n_files,
        bytes_per_file,
        (n_files * bytes_per_file) / (1024 * 1024)
    );

    let (_dir, conn, ctrl_path) = build_fixture(n_files, bytes_per_file);
    let live_db = Mutex::new(conn);

    let mut baseline_total = Vec::with_capacity(iters);
    let mut baseline_serialize = Vec::with_capacity(iters);
    let mut shortened_locked = Vec::with_capacity(iters);
    let mut shortened_serialize = Vec::with_capacity(iters);
    let mut db_size = 0usize;

    // Warmup.
    let _ = snapshot_baseline(&live_db, &ctrl_path);

    for _ in 0..iters {
        let (total, serialize, n) = snapshot_baseline(&live_db, &ctrl_path);
        baseline_total.push(total);
        baseline_serialize.push(serialize);
        db_size = n;
    }
    for _ in 0..iters {
        let (locked, serialize, _) = snapshot_shortened(&live_db, &ctrl_path);
        shortened_locked.push(locked);
        shortened_serialize.push(serialize);
    }

    let bs_total = mean(&baseline_total);
    let bs_serialize = mean(&baseline_serialize);
    let sh_locked = mean(&shortened_locked);
    let sh_serialize = mean(&shortened_serialize);

    println!("\n  serialized DB size:                {} KiB", db_size / 1024);
    println!(
        "  BASELINE  total lock-held (mean):  {:>9.2?}",
        bs_total
    );
    println!(
        "  BASELINE  ├ serialize() share:     {:>9.2?}  ({:.1}%)",
        bs_serialize,
        100.0 * bs_serialize.as_secs_f64() / bs_total.as_secs_f64()
    );
    println!(
        "  BASELINE  └ arena-tail share:      {:>9.2?}  ({:.1}%)",
        bs_total - bs_serialize,
        100.0 * (bs_total - bs_serialize).as_secs_f64() / bs_total.as_secs_f64()
    );
    println!(
        "  SHORTENED lock-held (mean):        {:>9.2?}",
        sh_locked
    );
    println!(
        "  SHORTENED ├ serialize() share:     {:>9.2?}  ({:.1}%)",
        sh_serialize,
        100.0 * sh_serialize.as_secs_f64() / sh_locked.as_secs_f64()
    );
    let lock_hold_ratio = bs_total.as_secs_f64() / sh_locked.as_secs_f64();
    println!(
        "\n  lock-hold ratio (baseline / shortened):   {:.2}×",
        lock_hold_ratio
    );
    println!(
        "  premise threshold ≥ 5×:                   {}",
        if lock_hold_ratio >= 5.0 {
            "MET"
        } else {
            "NOT MET"
        }
    );
}

// ── Phase 2: contention with N concurrent writers ──────────────────────

fn phase2_contention(
    n_files: usize,
    bytes_per_file: usize,
    n_writers: usize,
    duration: Duration,
    snapshot_period: Duration,
    shape: &str,
) -> ContentionResult {
    let (_dir, conn, ctrl_path) = build_fixture(n_files, bytes_per_file);
    let live_db = Arc::new(Mutex::new(conn));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Writers loop: take lock, do a tiny INSERT/UPDATE on `nodes`, release.
    // We use a no-op UPDATE to simulate the "edit happened" flag, plus
    // a small INSERT into a scratch table to make the writer non-trivial.
    {
        let conn = live_db.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS bench_writes (i INTEGER PRIMARY KEY AUTOINCREMENT, payload BLOB)"
        ).unwrap();
    }

    // Per-writer wait-for-lock latency samples.
    let writer_stats: Arc<Mutex<Vec<Vec<Duration>>>> =
        Arc::new(Mutex::new(vec![Vec::new(); n_writers]));
    let writer_counts: Arc<Mutex<Vec<u64>>> =
        Arc::new(Mutex::new(vec![0u64; n_writers]));

    let mut handles = Vec::new();

    // Spawn N writer threads.
    for w in 0..n_writers {
        let live_db = live_db.clone();
        let stop = stop.clone();
        let writer_stats = writer_stats.clone();
        let writer_counts = writer_counts.clone();
        handles.push(std::thread::spawn(move || {
            let mut local_waits = Vec::new();
            let mut local_count = 0u64;
            let payload = vec![0xABu8; 256];
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let wait_start = Instant::now();
                let conn = live_db.lock().unwrap();
                let wait = wait_start.elapsed();
                local_waits.push(wait);
                conn.execute(
                    "INSERT INTO bench_writes (payload) VALUES (?1)",
                    rusqlite::params![&payload],
                )
                .unwrap();
                drop(conn);
                local_count += 1;
                // Tiny pause so we don't completely starve the snapshot thread.
                if w % 2 == 0 {
                    std::hint::spin_loop();
                }
            }
            writer_stats.lock().unwrap()[w] = local_waits;
            writer_counts.lock().unwrap()[w] = local_count;
        }));
    }

    // Snapshot thread.
    let snap_stop = stop.clone();
    let snap_db = live_db.clone();
    let snap_ctrl = ctrl_path.clone();
    let snap_shape = shape.to_string();
    let snap_lock_holds: Arc<Mutex<Vec<Duration>>> =
        Arc::new(Mutex::new(Vec::new()));
    let snap_lock_holds_t = snap_lock_holds.clone();
    let snap_handle = std::thread::spawn(move || {
        let mut next = Instant::now() + snapshot_period;
        while !snap_stop.load(std::sync::atomic::Ordering::Relaxed) {
            let now = Instant::now();
            if now < next {
                std::thread::sleep(next - now);
            }
            if snap_stop.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            let (held, _ser, _bytes) = match snap_shape.as_str() {
                "baseline" => snapshot_baseline(&snap_db, &snap_ctrl),
                "shortened" => snapshot_shortened(&snap_db, &snap_ctrl),
                _ => unreachable!(),
            };
            snap_lock_holds_t.lock().unwrap().push(held);
            next = Instant::now() + snapshot_period;
        }
    });

    std::thread::sleep(duration);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for h in handles {
        h.join().unwrap();
    }
    snap_handle.join().unwrap();

    let mut all_waits: Vec<Duration> = writer_stats
        .lock()
        .unwrap()
        .iter()
        .flat_map(|v| v.iter().copied())
        .collect();
    let total_writes: u64 = writer_counts.lock().unwrap().iter().sum();
    let snapshots = snap_lock_holds.lock().unwrap();
    let mut snap_holds: Vec<Duration> = snapshots.iter().copied().collect();

    ContentionResult {
        n_writers,
        shape: shape.to_string(),
        duration,
        total_writes,
        write_throughput: total_writes as f64 / duration.as_secs_f64(),
        wait_p50: percentile(&mut all_waits, 0.5),
        wait_p99: percentile(&mut all_waits, 0.99),
        wait_max: all_waits.iter().copied().max().unwrap_or(Duration::ZERO),
        snap_count: snap_holds.len(),
        snap_lock_held_mean: mean(&snap_holds),
        snap_lock_held_p99: percentile(&mut snap_holds, 0.99),
    }
}

#[derive(Debug)]
struct ContentionResult {
    n_writers: usize,
    shape: String,
    duration: Duration,
    total_writes: u64,
    write_throughput: f64,
    wait_p50: Duration,
    wait_p99: Duration,
    wait_max: Duration,
    snap_count: usize,
    snap_lock_held_mean: Duration,
    snap_lock_held_p99: Duration,
}

fn print_contention_table(rows: &[ContentionResult]) {
    println!(
        "\n  {:<10} {:>4} {:>12} {:>10} {:>10} {:>10} {:>8} {:>11} {:>11}",
        "shape", "N", "writes/sec", "wait p50", "wait p99", "wait max", "snaps",
        "lock_hold μ", "lock_hold p99",
    );
    println!("  {}", "─".repeat(10 + 4 + 12 + 10 + 10 + 10 + 8 + 11 + 11 + 16));
    for r in rows {
        println!(
            "  {:<10} {:>4} {:>12.0} {:>10.2?} {:>10.2?} {:>10.2?} {:>8} {:>11.2?} {:>11.2?}",
            r.shape,
            r.n_writers,
            r.write_throughput,
            r.wait_p50,
            r.wait_p99,
            r.wait_max,
            r.snap_count,
            r.snap_lock_held_mean,
            r.snap_lock_held_p99,
        );
    }
}

// ── Driver ─────────────────────────────────────────────────────────────

fn main() {
    eprintln!("=== WAL snapshot lock-hold experiment ===");
    eprintln!(
        "VERIFIED PRECONDITION: `:memory:` SQLite ignores `PRAGMA journal_mode = WAL`."
    );
    eprintln!(
        "Premise tested: 'shorten lock-hold by releasing writer mutex after serialize().'"
    );

    // Scale: aim for a serialized DB of a few hundred MiB to imitate
    // registry-relevant lock-hold-time ranges. 5_000 files × 4 KiB of
    // python lands in that ballpark after AST is materialized into the
    // `nodes` table. We sweep a small + larger fixture so the trend is
    // visible.
    let scales = [(2_000usize, 2_048usize), (5_000usize, 4_096usize)];

    for (nf, bpf) in scales {
        println!(
            "\n────────────────────────────────────────────────────────────"
        );
        println!("FIXTURE: {} files × {} bytes/file", nf, bpf);
        println!(
            "────────────────────────────────────────────────────────────"
        );

        // Phase 1: lock-hold decomposition.
        phase1_decomposition(nf, bpf, 5);

        // Phase 2: contention.
        println!("\n══ Phase 2: contention (writers vs snapshot) ══");
        let mut rows = Vec::new();
        for &n_writers in &[1usize, 4, 10] {
            for shape in ["baseline", "shortened"] {
                let r = phase2_contention(
                    nf,
                    bpf,
                    n_writers,
                    Duration::from_secs(3),
                    Duration::from_millis(500),
                    shape,
                );
                rows.push(r);
            }
        }
        print_contention_table(&rows);

        // Pairwise ratios.
        println!("\n  ── ratio summary (shortened / baseline at same N) ──");
        for n in [1usize, 4, 10] {
            let b = rows
                .iter()
                .find(|r| r.n_writers == n && r.shape == "baseline")
                .unwrap();
            let s = rows
                .iter()
                .find(|r| r.n_writers == n && r.shape == "shortened")
                .unwrap();
            let throughput_ratio = s.write_throughput / b.write_throughput;
            let lock_hold_ratio = b.snap_lock_held_mean.as_secs_f64()
                / s.snap_lock_held_mean.as_secs_f64();
            println!(
                "    N={:>2}: throughput ×{:.2}, lock-hold ×{:.2}",
                n, throughput_ratio, lock_hold_ratio
            );
        }
    }

    println!("\n=== done ===");
}
