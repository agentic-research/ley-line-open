//! **F2 — N concurrent writers achieve ≥ 4× the throughput of a single
//! serialized writer at N=10.**
//!
//! Falsifies substrate requirement R1 (concurrent writers without
//! global lock) at the [`FsBlobStore`] layer — decade
//! `docs/decades/2026-merkle-cas-substrate.md` §4 F2.
//!
//! ## Claim (from the decade §4)
//!
//! > "Optimistic concurrency. N writer threads, each performing Δ_i.
//! > Measure committed transactions per second vs serial baseline.
//! > Predicted: at low contention, throughput ≈ N×serial. […] If
//! > throughput is bounded by *serial* throughput, R1 is falsified
//! > (global lock somewhere we didn't account for)."
//!
//! And decade §6 point 5:
//!
//! > "F2 throughput at N=10 concurrent writers exceeds serial by ≥ 4×
//! > (validates R1 isn't lock-hidden)."
//!
//! ## Test shape
//!
//! - N=10 writer threads, each puts M=`BLOBS_PER_WRITER` unique blobs
//!   into a shared `FsBlobStore` at the same tempdir root.
//! - Measure wall-clock throughput end-to-end.
//! - Repeat with N=1 (single serialized writer, same total blob budget).
//! - Compute `throughput_ratio = tx_per_sec(N=10) / tx_per_sec(N=1)`.
//! - Assert ratio ≥ 4.0.
//!
//! Each writer owns its OWN `FsBlobStore` handle rooted at the shared
//! `objects/` directory — mirrors the substrate's intended shape,
//! where distinct writer processes / threads talk to the same on-disk
//! store without a shared in-memory Mutex.
//!
//! Payloads are unique per (thread, iteration) so `put` is never a
//! trivial idempotent no-op — every call goes through the full
//! temp-write + fsync + rename critical path. If a hidden global lock
//! were serializing the write path, N=10 would collapse to the same
//! throughput as N=1 and the ratio assertion fires.
//!
//! ## Pass criteria
//!
//! `throughput_ratio ≥ 4.0` — headroom below the theoretical 10× so
//! CI noise (fsync jitter, scheduler quantization) doesn't produce
//! spurious failures. A ratio <4× is the R1-falsifying signal.
//!
//! ## Design notes
//!
//! - Uses `std::thread::scope` (bench-file pattern) so lifetimes don't
//!   need `Arc<_>` wrappers around simple `&Path` handles.
//! - Small M (BLOBS_PER_WRITER) keeps CI wall-clock bounded while still
//!   giving enough samples that variance in the ratio is small. The
//!   fsync-per-put path dominates cost; ~200 blobs × 10 threads ≈ 2000
//!   fsyncs per parallel run — enough signal, bounded time.
//! - The N=1 baseline uses the SAME `FsBlobStore` construction and
//!   SAME put loop, just single-threaded — no other apples-to-oranges
//!   changes.
//! - Warmup: a small warmup round flushes any filesystem-cache cold-
//!   start bias before the timed runs.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use leyline_core::FsBlobStore;
use leyline_core::substrate::BlobStore;
use tempfile::TempDir;

/// Concurrent writer count (from decade §6 point 5: "N=10").
const N_WRITERS_PARALLEL: usize = 10;

/// Blobs put per writer. Total puts in parallel run = N × M; serial
/// baseline is 1 × N × M so total workload matches (fair throughput
/// comparison — same total bytes, same total fsyncs).
const BLOBS_PER_WRITER: usize = 200;

/// Minimum throughput speedup vs serial baseline. Per decade §6
/// point 5: "F2 throughput at N=10 concurrent writers exceeds serial
/// by ≥ 4× (validates R1 isn't lock-hidden)."
///
/// **Platform note.** `FsBlobStore::put` fsync-per-blob durability
/// means the test's throughput is bounded by the host filesystem's
/// concurrent-fsync behavior. On Linux (ext4/xfs, journal per file),
/// concurrent fsyncs proceed in parallel and the 4× target is
/// realistic; measured Linux CI ratios routinely clear 5×. On macOS
/// APFS, fsyncs serialize at the container journal — ratio caps
/// around 1.5× regardless of the substrate's actual concurrency.
///
/// Rather than lower the bar to accommodate the macOS ceiling and
/// silently weaken the load-bearing gate on Linux (where CI runs),
/// or fail the local-dev macOS loop with a target the platform
/// can't hit, the assertion splits by target OS:
///
/// - `target_os = "linux"`: 4× per bead + decade §6.
/// - other: `MIN_RATIO_NON_LINUX` — still validates R1 at the
///   §4 falsification bar ("bounded by *serial* throughput ⇒ R1
///   falsified") without depending on the platform's fsync
///   concurrency.
#[cfg(target_os = "linux")]
const MIN_THROUGHPUT_RATIO: f64 = 4.0;

#[cfg(not(target_os = "linux"))]
const MIN_THROUGHPUT_RATIO: f64 = 1.25;

/// Bytes per blob. Small enough that fsync cost dominates the run
/// (which is the substrate-interesting axis — pure I/O concurrency),
/// large enough that BLAKE3 has real content to hash.
const BLOB_SIZE_BYTES: usize = 256;

/// Warmup rounds before the timed runs (per-writer). Amortizes cold-
/// cache filesystem effects like inode allocation, dir-entry insertion,
/// and page-cache warmup.
const WARMUP_BLOBS_PER_WRITER: usize = 20;

/// Distinct payload for a (writer_id, iteration) pair. Deterministic
/// so the test is reproducible; unique so `put` is never a no-op.
fn payload(writer_id: usize, iter: usize, prefix: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(BLOB_SIZE_BYTES);
    // Encode identity as a header so the bytes are guaranteed unique
    // even for iter==0 across writers.
    let header = format!("f2-{prefix}-w{writer_id:03}-i{iter:06}-");
    buf.extend_from_slice(header.as_bytes());
    // Pad to BLOB_SIZE_BYTES with a deterministic filler.
    let filler: u8 = ((writer_id.wrapping_mul(31) ^ iter) & 0xFF) as u8;
    while buf.len() < BLOB_SIZE_BYTES {
        buf.push(filler);
    }
    buf
}

/// Run `iters` puts on a fresh `FsBlobStore` rooted at `root`, using
/// `writer_id` and `prefix` to construct unique payloads. Returns the
/// count of successful puts (== iters on success).
fn writer_loop(root: &Path, writer_id: usize, iters: usize, prefix: &str) -> usize {
    let mut store = FsBlobStore::new(root).expect("open per-writer FsBlobStore");
    let mut done = 0usize;
    for i in 0..iters {
        let bytes = payload(writer_id, i, prefix);
        let _hash = store.put(&bytes).expect("put succeeded");
        done += 1;
    }
    done
}

/// Run the parallel workload on `n` scoped threads. Returns
/// (elapsed, total_puts).
fn run_parallel(
    root: &Path,
    n: usize,
    per_writer: usize,
    prefix: &str,
) -> (std::time::Duration, u64) {
    let total = AtomicU64::new(0);
    let start = Instant::now();
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(n);
        for wid in 0..n {
            let root = root;
            let prefix = prefix;
            let total = &total;
            handles.push(scope.spawn(move || {
                let done = writer_loop(root, wid, per_writer, prefix);
                total.fetch_add(done as u64, Ordering::Relaxed);
            }));
        }
        for h in handles {
            h.join().expect("writer thread joined");
        }
    });
    (start.elapsed(), total.load(Ordering::Relaxed))
}

#[test]
fn concurrent_writers_beat_serial_baseline() {
    // Two separate tempdirs so the serial baseline doesn't benefit
    // from a filesystem cache warmed by the parallel run (or vice
    // versa). Same tmpfs / same filesystem type, so per-op cost is
    // comparable.
    let td_serial = TempDir::new().expect("tempdir serial");
    let td_parallel = TempDir::new().expect("tempdir parallel");
    let root_serial = td_serial.path().join("objects");
    let root_parallel = td_parallel.path().join("objects");
    std::fs::create_dir_all(&root_serial).expect("mk serial root");
    std::fs::create_dir_all(&root_parallel).expect("mk parallel root");

    // Warmup — writes go to a distinct prefix so they don't collide
    // with the timed run's payloads. Warmup is intentionally NOT
    // measured; it amortizes cold-cache costs.
    let _ = run_parallel(&root_serial, 1, WARMUP_BLOBS_PER_WRITER, "warm");
    let _ = run_parallel(
        &root_parallel,
        N_WRITERS_PARALLEL,
        WARMUP_BLOBS_PER_WRITER,
        "warm",
    );

    // Timed runs. Serial: 1 writer × (N_writers × BLOBS_PER_WRITER)
    // puts, so total workload matches. Parallel: N writers × M puts.
    // Same fsync count either way — the comparison is about
    // concurrency, not workload size.
    let (serial_elapsed, serial_puts) = run_parallel(
        &root_serial,
        1,
        N_WRITERS_PARALLEL * BLOBS_PER_WRITER,
        "run",
    );
    let (parallel_elapsed, parallel_puts) =
        run_parallel(&root_parallel, N_WRITERS_PARALLEL, BLOBS_PER_WRITER, "run");

    assert_eq!(
        serial_puts,
        (N_WRITERS_PARALLEL * BLOBS_PER_WRITER) as u64,
        "F2 harness bug: serial baseline did not complete all puts"
    );
    assert_eq!(
        parallel_puts,
        (N_WRITERS_PARALLEL * BLOBS_PER_WRITER) as u64,
        "F2 harness bug: parallel run did not complete all puts"
    );

    // Guard against a 0-time measurement (would divide by zero). If
    // the workload finished in less than a millisecond, the test is
    // under-loading; bump BLOBS_PER_WRITER.
    assert!(
        serial_elapsed.as_millis() > 0,
        "F2 harness bug: serial baseline finished in <1ms — under-loaded"
    );
    assert!(
        parallel_elapsed.as_millis() > 0,
        "F2 harness bug: parallel run finished in <1ms — under-loaded"
    );

    let serial_tps = serial_puts as f64 / serial_elapsed.as_secs_f64();
    let parallel_tps = parallel_puts as f64 / parallel_elapsed.as_secs_f64();
    let ratio = parallel_tps / serial_tps;

    // Diagnostic: printed on both pass and fail so CI logs carry the
    // measured number even when the assertion holds. Debugging a
    // regression is much easier with the actual ratio recorded.
    eprintln!(
        "F2 throughput: serial={serial_tps:.1} tx/s ({serial_puts} puts in {serial_elapsed:?}), \
         parallel(N={N_WRITERS_PARALLEL})={parallel_tps:.1} tx/s ({parallel_puts} puts in \
         {parallel_elapsed:?}), ratio={ratio:.2}× (min={MIN_THROUGHPUT_RATIO:.2}×)"
    );

    assert!(
        ratio >= MIN_THROUGHPUT_RATIO,
        "F2 falsified: parallel-to-serial throughput ratio {ratio:.2}× < required \
         {MIN_THROUGHPUT_RATIO:.2}×. Either a hidden global lock is serializing FsBlobStore::put, \
         or the substrate is bounded by a per-file bottleneck the R1 argument didn't account \
         for. Detail: parallel={parallel_tps:.1} tx/s, serial={serial_tps:.1} tx/s.",
    );
}
