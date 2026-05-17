//! Cold-parse perf regression gate (bead `ley-line-open-a3f254`).
//!
//! The cbbedf perf win (5040ms → ~1475ms median on a 766-file mache repo,
//! bead `ley-line-open-cbbedf`) is hand-validated per release. Without
//! a CI gate, a regression in `cmd_parse.rs` — un-batching the VALUES
//! inserts, removing the `BufWriter` around the capnp dual-write,
//! re-enabling the orphan sweep on cold parse — would slip silently into
//! the next tag.
//!
//! This gate runs only when `LLO_PERF_GATES=1` is set in the environment
//! (same convention as `topology_pass_test.rs`'s gates, per Copilot
//! finding 9 on PR #20). `task ci` sets the env var; bare `cargo test`
//! does not — keeps fast-iteration loops snappy while making the local
//! pre-push gate enforce perf.
//!
//! Two assertions:
//!
//! 1. **Absolute wall ceiling** — the parse must complete in under
//!    `WALL_CEILING_MS`. Set high (500ms) on a corpus that runs in
//!    ~75ms today, so the gate doesn't flicker on CI noise.
//! 2. **Per-row budget** — `wall_ms × 1000 / row_count` must stay
//!    under `PER_ROW_BUDGET_MICROS`. This is the adaptive assertion:
//!    if the corpus grows or shrinks across branches, the per-row time
//!    stays bounded. Catches regressions like un-batched inserts (which
//!    multiply per-row time by ~10×) even when wall stays within the
//!    absolute ceiling on a smaller corpus.
//!
//! The corpus is the workspace's committed Go fixtures
//! (`tests/fixtures/topology/handcrafted/go`) replicated 200× into a
//! tempdir. Choosing committed fixtures over the workspace's own
//! source tree gives full determinism — the corpus shape doesn't drift
//! as the codebase evolves, only the parse path's perf does.
//!
//! Calibration baseline (commit 4037ef6, release build, M-series mac):
//!   parse= 11–19ms, insert= 56–62ms, head/sweep= 0ms, wall= 69–76ms
//!   row count: ~19k nodes, ~19k _ast, 800 _source
//!   per-row: ~4 µs/row (well under the 25 µs budget)
//!
//! Falsifiability proof: revert the `BULK_BATCH_ROWS` constant in
//! `cmd_parse.rs` from 3000 to 1 and observe insert phase explode by
//! ~2 orders of magnitude. The per-row budget will trip first.

use std::path::{Path, PathBuf};

use tempfile::TempDir;

/// Absolute wall ceiling. Set at ~7× the calibration baseline (75ms)
/// to absorb CI-runner noise without flickering. A regression past this
/// bound is "the parse is meaningfully slower," not "the runner is hot."
const WALL_CEILING_MS: u128 = 500;

/// Per-row time budget. Calibration baseline is ~4 µs/row on a quiescent
/// laptop; ceiling is 25 µs/row (~6× headroom). Catches per-row
/// regressions (un-batched inserts, removed `BufWriter`) even on a small
/// corpus where the absolute wall ceiling has slack.
const PER_ROW_BUDGET_MICROS: u128 = 25;

/// Number of times each fixture file is duplicated into a fresh subdir.
/// 4 Go files × 200 copies = 800 files, sufficient to exercise the
/// insert-phase batching path (`BULK_BATCH_ROWS = 3000` × 9 cols means
/// the insert phase amortises per-statement overhead across ~333
/// rows per VALUES statement at this corpus size).
const REPLICATION_COUNT: usize = 200;

/// Same env-var contract as `topology_pass_test.rs` — keep both in sync
/// or callers will need two opt-in switches.
fn perf_gate_enabled() -> bool {
    std::env::var("LLO_PERF_GATES").ok().as_deref() == Some("1")
}

/// Build the synthetic corpus by copying every Go file under the
/// handcrafted fixture root into `target/pkg_<i>/` for i in 0..N.
///
/// Returns the path the parse should walk. Each pkg_<i> directory is a
/// standalone "package" — leyline's collect_files walks the tree
/// recursively and parses each .go independently, so the directory
/// structure doesn't need to look like a real Go module.
fn build_corpus(target: &Path, copies: usize) -> std::io::Result<PathBuf> {
    let seed_dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/topology/handcrafted/go");

    let seed_files: Vec<PathBuf> = std::fs::read_dir(&seed_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("go"))
        .collect();

    assert!(
        !seed_files.is_empty(),
        "seed dir {} has no .go files — fixture missing?",
        seed_dir.display()
    );

    for i in 0..copies {
        let pkg_dir = target.join(format!("pkg_{i:04}"));
        std::fs::create_dir(&pkg_dir)?;
        for src in &seed_files {
            let dst = pkg_dir.join(src.file_name().unwrap());
            std::fs::copy(src, dst)?;
        }
    }

    Ok(target.to_path_buf())
}

/// Count rows in the tables `cmd_parse` populates. The per-row budget
/// divides wall time by this number, so it adapts as the parser's
/// node-emission strategy evolves (e.g. if we ever start storing
/// additional per-token rows).
fn count_rows(db_path: &Path) -> rusqlite::Result<u64> {
    let conn = rusqlite::Connection::open(db_path)?;
    let nodes: u64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))?;
    let ast: u64 = conn.query_row("SELECT COUNT(*) FROM _ast", [], |r| r.get(0))?;
    Ok(nodes + ast)
}

#[tokio::test]
async fn cold_parse_wall_within_budget_on_synthetic_go_corpus() {
    if !perf_gate_enabled() {
        eprintln!(
            "skipping cold-parse perf gate: LLO_PERF_GATES not set to '1'. \
             Run `LLO_PERF_GATES=1 cargo test --release` or `task ci`."
        );
        return;
    }

    let corpus_root = TempDir::new().expect("tempdir");
    let db_dir = TempDir::new().expect("db tempdir");
    let corpus = build_corpus(corpus_root.path(), REPLICATION_COUNT).expect("build corpus");
    let db_path = db_dir.path().join("perf-bench.db");

    let cmd = leyline_cli_lib::Commands::Parse {
        source: corpus,
        output: db_path.clone(),
        lang: None,
    };

    // Wall measurement wraps the entire parse call. The parse path emits
    // per-phase timings to stderr; we use the outer wall here because
    // it's what consumers actually observe (binary startup time is
    // measured separately in the bench script — see CHANGELOG v0.4.1).
    let start = std::time::Instant::now();
    leyline_cli_lib::run(cmd).await.expect("parse must succeed");
    let wall_ms = start.elapsed().as_millis();

    let row_count = count_rows(&db_path).expect("count rows");
    assert!(
        row_count > 0,
        "perf gate produced 0 rows — corpus likely wasn't parsed; \
         check that the Go fixtures are present and tree-sitter-go is \
         enabled in the binary under test"
    );

    let per_row_micros = (wall_ms * 1_000) / row_count as u128;

    eprintln!(
        "[perf-gate] wall={wall_ms}ms rows={row_count} per_row={per_row_micros}us \
         (ceiling: wall<{WALL_CEILING_MS}ms, per_row<{PER_ROW_BUDGET_MICROS}us)"
    );

    assert!(
        wall_ms < WALL_CEILING_MS,
        "cold-parse perf REGRESSION: wall {wall_ms}ms exceeded ceiling {WALL_CEILING_MS}ms \
         on a {REPLICATION_COUNT}-copy Go corpus ({row_count} rows). \
         The cbbedf insert-phase optimisations may have regressed — \
         check that BULK_BATCH_ROWS is still 3000, BufWriter wraps the \
         capnp dual-write, indexes are deferred until after COMMIT, and \
         the orphan sweep is skipped on cold parse."
    );

    assert!(
        per_row_micros < PER_ROW_BUDGET_MICROS,
        "cold-parse per-row REGRESSION: {per_row_micros}us/row exceeded budget \
         {PER_ROW_BUDGET_MICROS}us/row (wall={wall_ms}ms, rows={row_count}). \
         This is the adaptive assertion — the absolute wall ceiling may \
         still pass on a smaller corpus, but per-row time has degraded. \
         Most likely cause: VALUES batching was disabled or BULK_BATCH_ROWS \
         was lowered. See cmd_parse.rs:BULK_BATCH_ROWS comment for context."
    );
}

/// Falsifiability sanity test for the gate mechanism. Confirms the gate
/// env-var read works correctly (we don't want a silent skip on the
/// real test due to env-var typo).
#[test]
fn perf_gate_env_var_read_is_correct() {
    let prior = std::env::var("LLO_PERF_GATES").ok();
    // Safety: tests in this file run in serial wrt this env var because
    // only this test mutates it; the real gate test only reads it.
    unsafe {
        std::env::set_var("LLO_PERF_GATES", "1");
    }
    assert!(perf_gate_enabled(), "gate should be enabled when env=1");
    unsafe {
        std::env::set_var("LLO_PERF_GATES", "0");
    }
    assert!(!perf_gate_enabled(), "gate should be disabled when env=0");
    unsafe {
        std::env::remove_var("LLO_PERF_GATES");
    }
    assert!(
        !perf_gate_enabled(),
        "gate should be disabled when env unset"
    );
    // Restore prior state so subsequent tests in this binary see the
    // original env.
    unsafe {
        match prior {
            Some(v) => std::env::set_var("LLO_PERF_GATES", v),
            None => std::env::remove_var("LLO_PERF_GATES"),
        }
    }
}
