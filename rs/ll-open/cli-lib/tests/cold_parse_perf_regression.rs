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
//!    `WALL_CEILING_MS`. The measured value is the MIN of three cold
//!    runs: shared-runner scheduler noise is strictly additive, so the
//!    minimum approximates true capability, while a real regression
//!    lifts all three runs and still trips the 500ms ceiling.
//!
//!    This ceiling is a RELEASE number and is only ever asserted in a
//!    release build — `task test:perf` passes `--release`, and the gate
//!    hard-skips under `debug_assertions`. A debug build runs this path
//!    ~an order of magnitude slower, so comparing it to a release
//!    ceiling measures the build profile, not the insert path. That
//!    mismatch, not runner noise, is what failed `task ci` on main at
//!    3752ce6 (580ms) and, on the same signature, PR #304 (635ms); the
//!    min-of-three was added for that misdiagnosis and is kept because
//!    it is still the right shape for a wall-clock gate.
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

/// The one spelling of the opt-in env var. Referenced by both the reader
/// below and the skip message, so the name exists in exactly one place.
const GATE_ENV: &str = "LLO_PERF_GATES";

/// Pure predicate over the raw env value.
///
/// Split out from `perf_gate_enabled` so the sanity test below can pin the
/// "exactly the literal 1" contract WITHOUT writing to the process
/// environment. That is not a stylistic preference — see the test for the
/// CI failure the previous env-mutating version caused.
fn gate_enabled_from(raw: Option<&str>) -> bool {
    raw == Some("1")
}

/// Same env-var contract as `topology_pass_test.rs` — keep both in sync
/// or callers will need two opt-in switches.
fn perf_gate_enabled() -> bool {
    gate_enabled_from(std::env::var(GATE_ENV).ok().as_deref())
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
    // rusqlite 0.39 dropped `FromSql for u64`; read via `i64` then cast.
    // `COUNT(*)` is non-negative so the cast is total.
    let conn = rusqlite::Connection::open(db_path)?;
    let nodes: i64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))?;
    let ast: i64 = conn.query_row("SELECT COUNT(*) FROM _ast", [], |r| r.get(0))?;
    Ok((nodes + ast) as u64)
}

#[test]
fn cold_parse_wall_within_budget_on_synthetic_go_corpus() {
    if !perf_gate_enabled() {
        eprintln!(
            "skipping cold-parse perf gate: {GATE_ENV} not set to '1'. \
             Run `LLO_PERF_GATES=1 cargo test --release` or `task ci`."
        );
        return;
    }

    // Both ceilings below are calibrated against a RELEASE build (see the
    // module header's baseline: wall 69-76ms). A debug build runs this
    // path roughly an order of magnitude slower, so asserting either
    // ceiling here measures the profile, not a regression — 580ms of
    // debug-build wall against a 500ms release ceiling is exactly how run
    // 31848026749 failed on main.
    //
    // `task test:perf` passes `--release`, so the real gate is unaffected.
    // This only catches an armed gate reaching a debug binary: a developer
    // who follows the header's instructions but drops `--release`, or any
    // future path that re-arms the gate where it shouldn't.
    if cfg!(debug_assertions) {
        eprintln!(
            "skipping cold-parse perf gate: {GATE_ENV}=1 but this is a debug \
             build, and WALL_CEILING_MS/PER_ROW_BUDGET_MICROS are calibrated \
             for --release. Run `task test:perf`."
        );
        return;
    }

    let corpus_root = TempDir::new().expect("tempdir");
    let db_dir = TempDir::new().expect("db tempdir");
    let corpus = build_corpus(corpus_root.path(), REPLICATION_COUNT).expect("build corpus");

    // MIN of three cold runs, not one shot — the sampling rule lives in
    // `leyline-perf-sample` (rule 1 of its crate docs): scheduler noise on
    // a shared runner is strictly ADDITIVE, so the minimum approximates
    // true capability while a real regression (un-batched inserts, dropped
    // BufWriter) lifts every run by multiples and still trips. Each run
    // parses into its own fresh db so all three stay cold; the corpus
    // build is shared and unmeasured.
    //
    // This does NOT defend against the debug-vs-release mismatch that
    // actually failed CI — three debug runs are all ~10× over. The
    // `debug_assertions` skip above is what covers that.
    //
    // The runtime is built once, outside the timed region, and mirrors
    // `#[tokio::test]`'s current-thread default (this fn stopped being
    // `#[tokio::test]` so the sync sampling helper can own the rep loop),
    // keeping the measurement comparable with the module header's
    // calibration baseline.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let runs = leyline_perf_sample::best_of(3, |i| {
        let db_path = db_dir.path().join(format!("perf-bench-{i}.db"));
        let cmd = leyline_cli_lib::Commands::Parse {
            source: corpus.clone(),
            output: db_path.clone(),
            lang: None,
        };

        // Wall measurement wraps the entire parse call. The parse path
        // emits per-phase timings to stderr; we use the outer wall here
        // because it's what consumers actually observe (binary startup
        // time is measured separately in the bench script — see
        // CHANGELOG v0.4.1). The row count is derived AFTER the timed
        // region, so the COUNT queries never pollute the wall.
        leyline_perf_sample::timed(|| {
            rt.block_on(leyline_cli_lib::run(cmd))
                .expect("parse must succeed")
        })
        .map(|()| count_rows(&db_path).expect("count rows"))
    });

    let wall_ms = runs.wall().as_millis();
    let walls_ms: Vec<u128> = runs.walls().iter().map(|w| w.as_millis()).collect();
    let row_counts: Vec<u64> = runs.samples().iter().map(|s| s.value).collect();
    let row_count = row_counts[0];
    assert!(
        row_counts.iter().all(|&c| c == row_count),
        "cold runs disagreed on row count ({row_counts:?}) — the corpus \
         or parse is nondeterministic and the per-row budget below would \
         be measuring different work per run"
    );
    assert!(
        row_count > 0,
        "perf gate produced 0 rows — corpus likely wasn't parsed; \
         check that the Go fixtures are present and tree-sitter-go is \
         enabled in the binary under test"
    );

    let per_row_micros = (wall_ms * 1_000) / row_count as u128;

    eprintln!(
        "[perf-gate] wall={wall_ms}ms (min of {walls_ms:?}) rows={row_count} \
         per_row={per_row_micros}us \
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

/// Falsifiability sanity test for the gate mechanism: the gate arms on
/// exactly the literal `"1"` and on nothing else, so neither a loosened
/// comparison nor a stray value can flip it.
///
/// This exercises the PURE predicate and never touches `std::env`, which
/// is load-bearing rather than tidy. The previous version set
/// `LLO_PERF_GATES=1` on the process environment and restored it
/// afterwards, justified as:
///
/// > tests in this file run in serial wrt this env var because
/// > only this test mutates it; the real gate test only reads it.
///
/// That is not what serial means. Cargo runs both tests in this binary on
/// parallel threads and the environment is process-global, so "only one
/// writer" still leaves a concurrent reader racing the write window. When
/// the gate test's `perf_gate_enabled()` read landed inside that window it
/// ARMED under `cargo test -p leyline-cli-lib --features vec` — a DEBUG
/// build with the env var unset — and asserted the release-calibrated
/// 500ms ceiling against a debug measurement.
///
/// That is what broke `task ci` on main at 3752ce6 (`cli-lib:test:vec`,
/// run 31848026749, wall=580ms with `per_row=15us` comfortably inside its
/// own 25us budget). PR #304's 635ms failure has the same signature and
/// was read as runner noise; the min-of-three mitigation above was added
/// for that misdiagnosis.
///
/// Keep this test free of `std::env` mutation. The sibling case in
/// `daemon/sheaf_ablation.rs` (bead `ley-line-open-d71cf6`, PR #184) is
/// the same defect and took the `#[serial(...)]` route because production
/// code there genuinely reads the env; here the predicate is separable, so
/// deleting the shared mutable state beats arbitrating access to it.
#[test]
fn perf_gate_arms_on_exactly_one() {
    assert!(gate_enabled_from(Some("1")), "gate arms on \"1\"");
    assert!(!gate_enabled_from(Some("0")), "gate stays off on \"0\"");
    assert!(!gate_enabled_from(None), "gate stays off when unset");
    assert!(!gate_enabled_from(Some("")), "gate stays off on empty");
    assert!(
        !gate_enabled_from(Some("true")),
        "gate stays off on \"true\" — the contract is the literal \"1\""
    );
}
