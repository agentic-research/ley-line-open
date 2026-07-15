//! Sheaf ablation harness — bead `ley-line-open-2775a3`.
//!
//! Empirically proves (or falsifies) the load-bearing v0.7.0 claim:
//! sheaf-driven `daemon.sheaf.invalidate` (with fine-grained diff from
//! PR #146 / bead `ley-line-open-e40566`) touches ≤ 30% of regions per
//! changed-file event on average, versus the naive "invalidate every
//! known region on file change" baseline.
//!
//! # What this harness does
//!
//! 1. Enumerates real source files from the LLO-self checkout at the
//!    repo root (~500 files, real code, real git history).
//! 2. Synthesizes observation rows that make each file a labelled region
//!    (bare `path` label) plus one `<path>:sym:<NAME>` per file so
//!    `ComplexBuildPass` builds a realistic labelled complex.
//! 3. Points `LEYLINE_SHEAF_ABLATION_LOG` at a temp file.
//! 4. Drives four workload shapes through `emit_watcher_sheaf_invalidate`:
//!    - **Single-file edits**: 100 random single-file changes
//!    - **Multi-file commits**: replay 30 recent git commits' file sets
//!    - **Rename-heavy**: 30 rename-family file sets from `git log --diff-filter=R`
//!    - **Directory-scoped**: 10 events each touching 10 files in one dir
//! 5. Analyzes the log (per-workload avg ratio + histogram + failure
//!    modes) and prints the study report to stderr in markdown form.
//!
//! # Why `#[ignore]`
//!
//! The harness scans the real repo and spins up a full parse +
//! enrichment pipeline, taking on the order of tens of seconds even on
//! a fast disk. `task ci` skips it via `#[ignore]`; run explicitly:
//!
//! ```bash
//! cargo test -p leyline-cli-lib --test sheaf_ablation_test \
//!     -- --ignored --nocapture
//! ```
//!
//! The `--nocapture` is load-bearing: the markdown report is printed to
//! stderr for hand-copy into `docs/research/sheaf-ablation-study.md`.
//!
//! # Adversarial re-run
//!
//! The harness is a single test that runs the study TWICE against the
//! same corpus and asserts the aggregate ratios are within 10% delta.
//! If they diverge more than that, the assertion fails — the ablation
//! itself is unreliable, and the verdict cannot be trusted.
//!
//! # What this harness does NOT do
//!
//! - **No wall-time measurement.** Precision (regions touched) only;
//!   speed is ADR-0026 Phase 2's F2 gate scope.
//! - **No production-behaviour change.** The ablation log is opt-in via
//!   env var; the wire emit is unchanged.
//! - **No topology retune.** If the study falsifies the claim, follow-up
//!   beads investigate; this harness never mutates sheaf machinery to
//!   make the numbers look better.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use leyline_cli_lib::daemon::complex_build_pass::ComplexBuildPass;
use leyline_cli_lib::daemon::enrichment::{EnrichmentPass, TreeSitterPass};
use leyline_cli_lib::daemon::observation_schema::create_observation_schema;
use leyline_cli_lib::daemon::sheaf_ablation;
use leyline_cli_lib::daemon::sheaf_ops::SheafState;
use leyline_cli_lib::daemon::{DaemonContext, DaemonState, EventRouter, NoExt};
use rusqlite::Connection;
use tempfile::TempDir;

// ─── Corpus discovery ──────────────────────────────────────────────────────

/// Locate the LLO-self repo root — the directory that contains this
/// test file. Fall back to walking up from `CARGO_MANIFEST_DIR` until
/// we find a `.git` directory or a workspace `Cargo.toml`.
///
/// The corpus is the real LLO-self repo (~500 files, real Rust source).
/// The bead permits mache-self at `~/remotes/art/mache/` as an
/// alternative but explicitly notes "LLO-self ... is good enough" and
/// asks the harness to prefer it when mache-self isn't reachable. We
/// always use LLO-self here for reproducibility — every developer running
/// this test operates on the same corpus (their own checkout).
fn find_repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points at .../rs/ll-open/cli-lib during test
    // compilation. Walk up until we hit the repo root (contains
    // `Taskfile.yml` — a stable marker for the LLO-self repo layout).
    let mut cur = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        if cur.join("Taskfile.yml").exists() {
            return cur;
        }
        if !cur.pop() {
            panic!("could not locate repo root from CARGO_MANIFEST_DIR");
        }
    }
}

/// Walk the repo's `rs/` subtree and collect all `.rs` files, returning
/// their paths RELATIVE to `repo_root`. Skips `target/` (build output)
/// and `.git/` (already excluded because we walk from `rs/`).
///
/// Returns paths in stable sorted order so the ablation report is
/// reproducible across runs (the "adversarial re-run" gate would break
/// if walking produced different orderings on different runs).
fn walk_source_files(repo_root: &Path) -> Vec<String> {
    let rs_dir = repo_root.join("rs");
    let mut out: Vec<String> = Vec::new();
    walk_recursive(&rs_dir, repo_root, &mut out);
    out.sort();
    out
}

fn walk_recursive(dir: &Path, repo_root: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if path.is_dir() {
            if name_str == "target" || name_str.starts_with('.') {
                continue;
            }
            walk_recursive(&path, repo_root, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs")
            && let Ok(rel) = path.strip_prefix(repo_root)
        {
            out.push(rel.to_string_lossy().into_owned());
        }
    }
}

// ─── Fixture setup ─────────────────────────────────────────────────────────

/// Fresh arena + controller so `read_root_hex` in the emit path
/// returns a well-formed hex string. Mirrors
/// `sheaf_gap3_invalidate_emit_test.rs::fresh_arena`.
fn fresh_arena(dir: &Path) -> PathBuf {
    use leyline_core::{Controller, create_arena};
    let arena_path = dir.join("test.arena");
    let ctrl_path = dir.join("test.ctrl");
    let _mmap = create_arena(&arena_path, 4 * 1024 * 1024).expect("create arena");
    let mut ctrl = Controller::open_or_create(&ctrl_path).expect("open ctrl");
    ctrl.set_arena(&arena_path.to_string_lossy(), 4 * 1024 * 1024)
        .expect("set arena");
    drop(ctrl);
    ctrl_path
}

/// Build a `DaemonContext` wired the same way `sheaf_loop_end_to_end_test`
/// does — TreeSitterPass + ComplexBuildPass — so the region labels are
/// derived from real observation rows via the production pipeline.
fn build_ctx(
    dir: &Path,
    source_dir: PathBuf,
) -> (Arc<DaemonContext>, Arc<EventRouter>, Arc<SheafState>) {
    use parking_lot::{Mutex, RwLock};

    let ctrl_path = fresh_arena(dir);
    let router = EventRouter::new(1024);
    let sheaf = Arc::new(SheafState::new());
    sheaf.set_emitter(router.emitter());

    let live_db_path = ctrl_path.with_extension("live.db");
    let writer = Connection::open(&live_db_path).expect("open live db");
    let mode: String = writer
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .expect("set journal_mode=WAL");
    assert_eq!(mode.to_lowercase(), "wal");
    create_observation_schema(&writer).expect("create observation schema");
    let live_db = leyline_cli_lib::daemon::db_pool::LiveDb::new(writer, &live_db_path, 4)
        .expect("build live db");

    // TreeSitterPass runs even on a shallow copy — it's cheap for the
    // small handful of files we drop under `source_dir`. ComplexBuildPass
    // is the load-bearing piece: it consumes the seeded observations and
    // installs the labelled complex we're measuring.
    let passes: Vec<Box<dyn EnrichmentPass>> = vec![
        Box::new(TreeSitterPass),
        Box::new(ComplexBuildPass::new(sheaf.clone())),
    ];

    let ctx = Arc::new(DaemonContext {
        ctrl_path,
        ext: Arc::new(NoExt),
        router: router.clone(),
        live_db,
        enrich_inflight: Arc::new(Mutex::new(HashSet::new())),
        source_dir: Some(source_dir),
        lang_filter: None,
        enrichment_passes: passes,
        state: Arc::new(RwLock::new(DaemonState::initializing())),
        #[cfg(feature = "vec")]
        vec_index: {
            leyline_cli_lib::daemon::vec_index::register_vec();
            Arc::new(leyline_cli_lib::daemon::vec_index::VectorIndex::new(4, None).unwrap())
        },
        #[cfg(feature = "vec")]
        embedder: Arc::new(leyline_cli_lib::daemon::embed::ZeroEmbedder { dim: 4 }),
        #[cfg(feature = "vec")]
        embed_queue: Arc::new(Mutex::new(std::collections::BinaryHeap::new())),
        #[cfg(feature = "text-search")]
        text_search: Arc::new(leyline_text_search::null::NullEngine::new()),
        sheaf: sheaf.clone(),
    });
    (ctx, router, sheaf)
}

/// Seed observation rows so `ComplexBuildPass` builds one region per
/// file (bare-path label) plus one `<path>:sym:<NAME>` region per file.
/// Total labelled regions ≈ 2 × files.
///
/// Edges (co-occurrences) come from pairing each file with a small
/// neighbourhood — sibling files in the same directory — so the complex
/// has meaningful topology beyond isolated nodes. `ComplexBuildPass`
/// uses each row's `mentions` array for the co-occurrence edges.
fn seed_observations(conn: &Connection, files: &[String]) {
    // Group files by parent directory so each observation pairs
    // sibling files — realistic co-change locality.
    let mut by_dir: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, f) in files.iter().enumerate() {
        let dir = Path::new(f)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        by_dir.entry(dir).or_default().push(idx);
    }

    let tx_start = std::time::Instant::now();
    conn.execute_batch("BEGIN").expect("begin tx");
    let mut stmt = conn
        .prepare(
            "INSERT INTO observation (source, payload_kind, mentions, observed_at) \
             VALUES ('ablation', 'agent.session_turn', ?1, ?2)",
        )
        .expect("prep insert");

    let mut ts: i64 = 0;
    for f in files {
        // Observation A: file mentions itself + its `:sym:<NAME>`
        // citation so both labels land in the complex.
        let sym_name = derive_sym_name(f);
        let sym_token = format!("{f}:sym:{sym_name}");
        let mentions_a = serde_json::to_string(&[f.as_str(), sym_token.as_str()]).unwrap();
        stmt.execute(rusqlite::params![mentions_a, ts])
            .expect("insert observation A");
        ts += 1;
    }

    // Observation B per directory: pair up to 4 siblings so the
    // co-occurrence edges form a realistic tight cluster.
    for indices in by_dir.values() {
        let take = indices.len().min(4);
        if take < 2 {
            continue;
        }
        let mentions_b: Vec<&str> = indices[..take].iter().map(|&i| files[i].as_str()).collect();
        let mentions_b_json = serde_json::to_string(&mentions_b).unwrap();
        stmt.execute(rusqlite::params![mentions_b_json, ts])
            .expect("insert observation B");
        ts += 1;
    }

    drop(stmt);
    conn.execute_batch("COMMIT").expect("commit tx");

    eprintln!(
        "seeded {} observations across {} files in {:.2}s",
        ts,
        files.len(),
        tx_start.elapsed().as_secs_f64(),
    );
}

/// Deterministic per-file symbol name so the `<file>:sym:<NAME>`
/// labels are stable across runs (the "adversarial re-run" gate would
/// break if the label set drifted between runs).
fn derive_sym_name(path: &str) -> String {
    let stem = Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("sym");
    // Sanitize to ASCII-alnum; tree-sitter symbols aren't restricted
    // but the token set stays cleaner without punctuation surprises.
    let cleaned: String = stem
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if cleaned.is_empty() {
        "sym".into()
    } else {
        cleaned
    }
}

// ─── Workload drivers ──────────────────────────────────────────────────────

/// Seeded xorshift so the "random" file picks are reproducible across
/// runs. Standard `rand` would work but pulling that dep just for a
/// tiny PRNG bloats the test binary; xorshift64 is a five-line RNG
/// that is more than good enough for uniform-ish selection.
struct SeededRng(u64);
impl SeededRng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn choice<'a, T>(&mut self, slice: &'a [T]) -> &'a T {
        let idx = (self.next() as usize) % slice.len();
        &slice[idx]
    }
}

/// Watermark helper — reads the current line count of the log so a
/// caller can slice the log by workload after all workloads run.
fn log_line_count(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|s| s.lines().filter(|l| !l.is_empty()).count())
        .unwrap_or(0)
}

/// Result of `run_workloads` — the log-line watermarks between each
/// workload boundary so the analysis pass can slice per-workload.
#[derive(Debug, Clone)]
struct Watermarks {
    single_end: usize,
    multi_end: usize,
    rename_end: usize,
    dir_end: usize,
}

/// Run the four workloads against the ctx. Each workload directly calls
/// `emit_watcher_sheaf_invalidate` with a synthesized `changed_files`
/// argument, because the fine-grained diff is a pure function of
/// `changed_files` and the installed label map — full parse-and-enrich
/// on every event would blow the runtime by an order of magnitude
/// without changing the measurement.
fn run_workloads(
    ctx: &Arc<DaemonContext>,
    router: &Arc<EventRouter>,
    files: &[String],
    repo_root: &Path,
    log_path: &Path,
) -> Watermarks {
    let emitter = router.emitter();

    // --- 1. Single-file edits (100 events) ---
    let mut rng = SeededRng::new(0xdeadbeef);
    for _ in 0..100 {
        let f = rng.choice(files).clone();
        leyline_cli_lib::cmd_daemon::emit_watcher_sheaf_invalidate(ctx, &[f], &emitter);
    }
    sheaf_ablation::reset_handle_for_tests();
    let single_end = log_line_count(log_path);
    eprintln!("workload 1 (single-file): 100 events");

    // --- 2. Multi-file commits (30 recent git commits) ---
    let commit_sets = git_recent_commit_file_sets(repo_root, 30);
    let mut multi_count = 0;
    for set in &commit_sets {
        if set.is_empty() {
            continue;
        }
        // Filter to files that actually appear in our labelled corpus
        // (a commit may touch docs/, Taskfile.yml, etc. — we labelled
        // only `.rs` files under `rs/`).
        let scoped: Vec<String> = set.iter().filter(|f| files.contains(f)).cloned().collect();
        if scoped.is_empty() {
            continue;
        }
        leyline_cli_lib::cmd_daemon::emit_watcher_sheaf_invalidate(ctx, &scoped, &emitter);
        multi_count += 1;
    }
    sheaf_ablation::reset_handle_for_tests();
    let multi_end = log_line_count(log_path);
    eprintln!(
        "workload 2 (multi-file commits): {} events (from {} candidate commits)",
        multi_count,
        commit_sets.len(),
    );

    // --- 3. Rename-heavy (30 rename family sets) ---
    let rename_sets = git_rename_file_sets(repo_root, 30);
    let mut rename_count = 0;
    for set in &rename_sets {
        if set.is_empty() {
            continue;
        }
        let scoped: Vec<String> = set.iter().filter(|f| files.contains(f)).cloned().collect();
        if scoped.is_empty() {
            continue;
        }
        leyline_cli_lib::cmd_daemon::emit_watcher_sheaf_invalidate(ctx, &scoped, &emitter);
        rename_count += 1;
    }
    sheaf_ablation::reset_handle_for_tests();
    let rename_end = log_line_count(log_path);
    eprintln!(
        "workload 3 (rename-heavy): {} events (from {} candidate rename groups)",
        rename_count,
        rename_sets.len(),
    );

    // --- 4. Directory-scoped (10 events × 10 files each) ---
    let mut by_dir: HashMap<String, Vec<String>> = HashMap::new();
    for f in files {
        let dir = Path::new(f)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        by_dir.entry(dir).or_default().push(f.clone());
    }
    // Sort directories by file count desc so we pick the fattest 10 —
    // gives every event a full 10-file scope where possible. Ties
    // broken by directory name for reproducibility.
    let mut dirs: Vec<(&String, &Vec<String>)> = by_dir.iter().collect();
    dirs.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));
    let mut dir_count = 0;
    for (_dir, dir_files) in dirs.iter().take(10) {
        let take = dir_files.len().min(10);
        let scope: Vec<String> = dir_files[..take].to_vec();
        leyline_cli_lib::cmd_daemon::emit_watcher_sheaf_invalidate(ctx, &scope, &emitter);
        dir_count += 1;
    }
    sheaf_ablation::reset_handle_for_tests();
    let dir_end = log_line_count(log_path);
    eprintln!("workload 4 (directory-scoped): {dir_count} events");

    Watermarks {
        single_end,
        multi_end,
        rename_end,
        dir_end,
    }
}

/// `git log --name-only -N` grouped by commit. Returns a Vec of file
/// lists, one per commit. Best-effort: if git isn't available or the
/// repo isn't checked out, returns an empty vec (workload 2 skips).
fn git_recent_commit_file_sets(repo_root: &Path, n: usize) -> Vec<Vec<String>> {
    let output = std::process::Command::new("git")
        .args([
            "log",
            &format!("-{n}"),
            "--name-only",
            "--pretty=format:__COMMIT__",
        ])
        .current_dir(repo_root)
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut sets: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for line in stdout.lines() {
        if line == "__COMMIT__" {
            if !current.is_empty() {
                sets.push(std::mem::take(&mut current));
            }
        } else if !line.is_empty() {
            current.push(line.to_string());
        }
    }
    if !current.is_empty() {
        sets.push(current);
    }
    sets
}

/// `git log --diff-filter=R --name-only -N`. Renames appear as
/// `old_path -> new_path` in --name-status but with --name-only the
/// old + new paths appear as separate lines. We treat each rename
/// commit as one file set (both old and new names) so the harness
/// exercises the case where a "file change" event points at a path
/// that may no longer exist in the current tree.
fn git_rename_file_sets(repo_root: &Path, n: usize) -> Vec<Vec<String>> {
    let output = std::process::Command::new("git")
        .args([
            "log",
            &format!("-{n}"),
            "--diff-filter=R",
            "--name-only",
            "--pretty=format:__COMMIT__",
        ])
        .current_dir(repo_root)
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut sets: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for line in stdout.lines() {
        if line == "__COMMIT__" {
            if !current.is_empty() {
                sets.push(std::mem::take(&mut current));
            }
        } else if !line.is_empty() {
            current.push(line.to_string());
        }
    }
    if !current.is_empty() {
        sets.push(current);
    }
    sets
}

// ─── Analysis ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct Event {
    /// Kept for future analyses that need to correlate an event back
    /// to the caller's file scope (e.g. per-directory breakdowns).
    /// Not read by the current stats pass — clippy allow keeps the
    /// field parseable in one place.
    changed_files: Vec<String>,
    sheaf_count: u64,
    naive_count: u64,
    scope: String,
}

fn parse_log(path: &Path) -> Vec<Event> {
    let text = std::fs::read_to_string(path).expect("read ablation log");
    let mut out = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("skipping malformed ablation line: {e}");
                continue;
            }
        };
        let changed_files: Vec<String> = v["changed_files"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let sheaf_count = v["sheaf_count"].as_u64().unwrap_or(0);
        let naive_count = v["naive_count"].as_u64().unwrap_or(0);
        let scope = v["scope"].as_str().unwrap_or("").to_string();
        out.push(Event {
            changed_files,
            sheaf_count,
            naive_count,
            scope,
        });
    }
    out
}

/// Descriptive stats for one workload slice.
#[derive(Debug, Clone)]
struct WorkloadStats {
    name: &'static str,
    events: usize,
    avg_sheaf: f64,
    avg_naive: f64,
    /// Average of the per-event ratio `sheaf_count / naive_count`.
    /// Restricted to events where `naive_count > 0` and
    /// `scope == "changed-only"` (all-known events are 1:1 by
    /// construction and would inflate the average toward 1.0).
    avg_ratio: f64,
    median_ratio: f64,
    p95_ratio: f64,
    /// Events where `sheaf_count > naive_count` — must be zero. A
    /// positive count is a real bug (the fine-grained diff hallucinated
    /// regions the coarse baseline didn't know about).
    failures: usize,
}

fn stats_for(name: &'static str, events: &[Event]) -> WorkloadStats {
    let n = events.len();
    if n == 0 {
        return WorkloadStats {
            name,
            events: 0,
            avg_sheaf: 0.0,
            avg_naive: 0.0,
            avg_ratio: f64::NAN,
            median_ratio: f64::NAN,
            p95_ratio: f64::NAN,
            failures: 0,
        };
    }
    let avg_sheaf = events.iter().map(|e| e.sheaf_count as f64).sum::<f64>() / n as f64;
    let avg_naive = events.iter().map(|e| e.naive_count as f64).sum::<f64>() / n as f64;
    let failures = events
        .iter()
        .filter(|e| e.sheaf_count > e.naive_count)
        .count();

    let mut ratios: Vec<f64> = events
        .iter()
        .filter(|e| e.scope == "changed-only" && e.naive_count > 0)
        .map(|e| e.sheaf_count as f64 / e.naive_count as f64)
        .collect();
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let avg_ratio = if ratios.is_empty() {
        f64::NAN
    } else {
        ratios.iter().sum::<f64>() / ratios.len() as f64
    };
    let median_ratio = percentile(&ratios, 0.50);
    let p95_ratio = percentile(&ratios, 0.95);

    WorkloadStats {
        name,
        events: n,
        avg_sheaf,
        avg_naive,
        avg_ratio,
        median_ratio,
        p95_ratio,
        failures,
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Bucket histogram of the per-event `sheaf_count / naive_count` ratio.
/// Buckets: 0-10%, 10-30%, 30-50%, 50-80%, 80-100%. A "load-bearing"
/// verdict wants mass concentrated in the first two buckets.
#[derive(Debug, Default, Clone)]
struct Histogram {
    b_0_10: usize,
    b_10_30: usize,
    b_30_50: usize,
    b_50_80: usize,
    b_80_100: usize,
    total: usize,
}

fn histogram(events: &[Event]) -> Histogram {
    let mut h = Histogram::default();
    for e in events
        .iter()
        .filter(|e| e.scope == "changed-only" && e.naive_count > 0)
    {
        h.total += 1;
        let r = e.sheaf_count as f64 / e.naive_count as f64;
        if r < 0.10 {
            h.b_0_10 += 1;
        } else if r < 0.30 {
            h.b_10_30 += 1;
        } else if r < 0.50 {
            h.b_30_50 += 1;
        } else if r < 0.80 {
            h.b_50_80 += 1;
        } else {
            h.b_80_100 += 1;
        }
    }
    h
}

fn pct(part: usize, total: usize) -> String {
    if total == 0 {
        "n/a".to_string()
    } else {
        format!("{:.1}%", 100.0 * part as f64 / total as f64)
    }
}

/// Build the aggregate-only stats row (all events, all workloads).
fn aggregate_stats(events: &[Event]) -> WorkloadStats {
    stats_for("aggregate", events)
}

/// Print the study report to stderr in markdown form so the caller can
/// pipe / copy into `docs/research/sheaf-ablation-study.md`.
fn print_report(
    run_label: &str,
    per_workload: &[WorkloadStats],
    aggregate: &WorkloadStats,
    hist: &Histogram,
) {
    eprintln!("\n===== ABLATION REPORT: {run_label} =====\n");
    eprintln!("### Per-workload table");
    eprintln!();
    eprintln!(
        "| workload | events | avg sheaf | avg naive | over-invalidation ratio (naive/sheaf) | avg ratio (sheaf/naive) | median ratio | p95 ratio | failures |"
    );
    eprintln!("|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    for s in per_workload {
        let over = if s.avg_sheaf > 0.0 {
            format!("{:.2}x", s.avg_naive / s.avg_sheaf)
        } else {
            "n/a".into()
        };
        eprintln!(
            "| {} | {} | {:.2} | {:.2} | {} | {:.4} | {:.4} | {:.4} | {} |",
            s.name,
            s.events,
            s.avg_sheaf,
            s.avg_naive,
            over,
            s.avg_ratio,
            s.median_ratio,
            s.p95_ratio,
            s.failures,
        );
    }
    let over_all = if aggregate.avg_sheaf > 0.0 {
        format!("{:.2}x", aggregate.avg_naive / aggregate.avg_sheaf)
    } else {
        "n/a".into()
    };
    eprintln!(
        "| **{}** | {} | {:.2} | {:.2} | {} | {:.4} | {:.4} | {:.4} | {} |",
        aggregate.name,
        aggregate.events,
        aggregate.avg_sheaf,
        aggregate.avg_naive,
        over_all,
        aggregate.avg_ratio,
        aggregate.median_ratio,
        aggregate.p95_ratio,
        aggregate.failures,
    );

    eprintln!("\n### Distribution (sheaf/naive per event, changed-only scope only)\n");
    eprintln!("| bucket | events | share |");
    eprintln!("|---|---:|---:|");
    eprintln!(
        "| 0-10%   | {} | {} |",
        hist.b_0_10,
        pct(hist.b_0_10, hist.total)
    );
    eprintln!(
        "| 10-30%  | {} | {} |",
        hist.b_10_30,
        pct(hist.b_10_30, hist.total)
    );
    eprintln!(
        "| 30-50%  | {} | {} |",
        hist.b_30_50,
        pct(hist.b_30_50, hist.total)
    );
    eprintln!(
        "| 50-80%  | {} | {} |",
        hist.b_50_80,
        pct(hist.b_50_80, hist.total)
    );
    eprintln!(
        "| 80-100% | {} | {} |",
        hist.b_80_100,
        pct(hist.b_80_100, hist.total)
    );
    eprintln!("| **total** | {} | 100% |", hist.total);

    let verdict = classify_verdict(aggregate);
    eprintln!("\n### Verdict: {verdict}");
    eprintln!(
        "\nAggregate avg ratio = {:.4} ({:.1}%); target for LOAD-BEARING is ≤ 0.30 (≤30%); FALSIFIED at ≥ 0.80 (≥80%).",
        aggregate.avg_ratio,
        aggregate.avg_ratio * 100.0
    );
    eprintln!(
        "Failures (sheaf > naive) across all events: {} — must be zero.\n",
        aggregate.failures,
    );
}

fn classify_verdict(agg: &WorkloadStats) -> &'static str {
    if agg.avg_ratio.is_nan() {
        return "INDETERMINATE (no changed-only events observed)";
    }
    if agg.avg_ratio <= 0.30 {
        "LOAD-BEARING"
    } else if agg.avg_ratio < 0.80 {
        "MARGINAL"
    } else {
        "FALSIFIED"
    }
}

// ─── Priming: run the enrichment cycle to install labels ─────────────────

/// Drop a minimal parseable Rust file into `source_dir` so
/// `TreeSitterPass::run` has something to walk without walking the real
/// LLO repo — TreeSitter walking half a million lines of production
/// source dominates the harness runtime and adds no measurement value
/// (we care about ComplexBuildPass output, not TreeSitter output).
fn seed_source_dir(source_dir: &Path) {
    std::fs::create_dir_all(source_dir).expect("mkdir source_dir");
    std::fs::write(source_dir.join("stub.rs"), b"fn stub() -> i32 { 0 }\n").expect("write stub.rs");
}

// ─── The test ───────────────────────────────────────────────────────────────

/// Runs the ablation study twice against the same corpus and asserts
/// the aggregate ratios are within 10% delta (the "adversarial re-run"
/// gate). If they diverge more than that, the study itself is
/// unreliable and the verdict cannot be trusted.
///
/// Prints a per-run markdown report to stderr for hand-copy into
/// `docs/research/sheaf-ablation-study.md`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "SLOW: builds observation-derived complex from the real repo + runs 300+ synthetic \
            events per run × 2 runs (~30s). Explicitly run: cargo test -p leyline-cli-lib \
            --test sheaf_ablation_test -- --ignored --nocapture"]
async fn sheaf_ablation_study() {
    let repo_root = find_repo_root();
    eprintln!("repo root: {}", repo_root.display());
    let files = walk_source_files(&repo_root);
    eprintln!("corpus: {} .rs files under rs/", files.len());
    assert!(
        files.len() >= 100,
        "expected a real corpus with ≥100 files; got {}. Are we in the right repo?",
        files.len(),
    );

    let run_1 = run_one_pass(&repo_root, &files, "run-1");
    let run_2 = run_one_pass(&repo_root, &files, "run-2");

    // Adversarial re-run gate: aggregate avg ratios must be within 10%
    // relative delta of each other. Anything larger and the measurement
    // itself is too noisy to trust.
    let (a1, a2) = (run_1.avg_ratio, run_2.avg_ratio);
    if !a1.is_nan() && !a2.is_nan() {
        let delta = (a1 - a2).abs();
        let base = a1.abs().max(a2.abs()).max(1e-9);
        let rel = delta / base;
        eprintln!(
            "\n===== ADVERSARIAL RE-RUN CHECK =====\nrun-1 avg ratio = {a1:.4}\nrun-2 avg ratio = {a2:.4}\nrelative delta = {:.2}% (bound: 10%)",
            rel * 100.0,
        );
        assert!(
            rel <= 0.10,
            "ablation harness produced divergent results (>10% relative delta between runs); \
             the study is unreliable and the verdict cannot be trusted",
        );
    }
}

/// One pass of the study: fresh temp dir + fresh log + fresh context,
/// then all four workloads. Returns the aggregate stats.
fn run_one_pass(repo_root: &Path, files: &[String], run_label: &str) -> WorkloadStats {
    let dir = TempDir::new().expect("tempdir");
    let source_dir = dir.path().join("src");
    seed_source_dir(&source_dir);

    let log_path = dir.path().join("ablation.jsonl");
    // SAFETY: this test is not run in parallel with any other that
    // touches LEYLINE_SHEAF_ABLATION_LOG (integration tests each get
    // their own binary; unit tests in `sheaf_ablation.rs` are in a
    // different binary and are guarded by the same var-clear discipline).
    //
    // Env var is deliberately NOT set yet — the priming enrichment
    // cycle at step 3 below also calls `emit_watcher_sheaf_invalidate`
    // and would otherwise pollute the log with one bootstrap event.
    // We flip the var on AFTER priming so the log records only the
    // four workloads' events.

    let (ctx, router, sheaf) = build_ctx(dir.path(), source_dir.clone());

    // 1. Do an initial parse so `TreeSitterPass` has an `_ast` baseline.
    {
        let guard = ctx.live_db.writer.lock();
        leyline_cli_lib::cmd_parse::parse_into_conn(&guard, &source_dir, None, None)
            .expect("initial parse");
    }

    // 2. Seed observations against the REAL repo file list — the labels
    //    the complex is built from name real files under `rs/`.
    {
        let guard = ctx.live_db.writer.lock();
        seed_observations(&guard, files);
    }

    // 3. Prime the enrichment cycle so ComplexBuildPass installs the
    //    labelled complex into SheafState. This is the only place the
    //    labelled complex materializes — without this, every emit
    //    falls back to `all-known` and the study produces no signal.
    //    Env var still unset — this cycle's own invalidate emit is
    //    NOT recorded in the ablation log.
    let emitter = router.emitter();
    leyline_cli_lib::cmd_daemon::run_watcher_enrichment(&ctx, &source_dir, &[], &emitter);

    // Now turn on ablation logging — every subsequent emit is recorded.
    unsafe {
        std::env::set_var(sheaf_ablation::ABLATION_LOG_ENV, &log_path);
    }
    sheaf_ablation::reset_handle_for_tests();

    // Sanity: the priming run installed a non-empty complex with
    // labels; otherwise the workload will emit only `all-known` events
    // and the study is meaningless.
    let complex_size = {
        let cache = sheaf.cache().lock();
        cache.complex().map(|cx| cx.nodes.len()).unwrap_or(0)
    };
    assert!(
        complex_size >= 50,
        "priming enrichment must install a labelled complex with ≥50 nodes; got {complex_size}. \
         The observation seed or ComplexBuildPass regressed."
    );
    eprintln!("[{run_label}] complex installed: {complex_size} nodes",);

    // 4. Run the four workloads. `run_workloads` calls
    //    `emit_watcher_sheaf_invalidate` directly per event; the
    //    ablation instrumentation appends one JSON line per call.
    //    Returns per-workload watermarks (log line counts at each
    //    boundary) so we can slice cleanly.
    let marks = run_workloads(&ctx, &router, files, repo_root, &log_path);

    // Optional: copy the raw log to a stable path for post-hoc jq /
    // wc / whatever inspection. Enabled via
    // `LEYLINE_SHEAF_ABLATION_OUT_DIR=<dir>` — points at where each
    // run's `<run-label>.jsonl` should land.
    if let Ok(out_dir) = std::env::var("LEYLINE_SHEAF_ABLATION_OUT_DIR") {
        let out_dir = PathBuf::from(out_dir);
        let _ = std::fs::create_dir_all(&out_dir);
        let dest = out_dir.join(format!("{run_label}.jsonl"));
        if let Err(e) = std::fs::copy(&log_path, &dest) {
            eprintln!("warn: copy {log_path:?} -> {dest:?} failed: {e:#}");
        } else {
            eprintln!("[{run_label}] raw log copied to {}", dest.display());
        }
    }

    // 5. Analyze.
    let events = parse_log(&log_path);
    eprintln!("[{run_label}] logged {} events", events.len());

    // Slice by watermark. Each workload's events land in a contiguous
    // range because they're all written through the same append-only
    // log handle. Bounds-check with `min` so a partial-write scenario
    // (log flush lagged) can't panic the slice.
    let n = events.len();
    let single_end = marks.single_end.min(n);
    let multi_end = marks.multi_end.min(n);
    let rename_end = marks.rename_end.min(n);
    let dir_end = marks.dir_end.min(n);

    let single = &events[..single_end];
    let multi_events = events[single_end..multi_end].to_vec();
    let rename_events = events[multi_end..rename_end].to_vec();
    let dir_events = events[rename_end..dir_end].to_vec();

    let s_single = stats_for("single-file", single);
    let s_multi = stats_for("multi-file commits", &multi_events);
    let s_rename = stats_for("rename-heavy", &rename_events);
    let s_dir = stats_for("directory-scoped", &dir_events);
    let s_agg = aggregate_stats(&events);
    let hist = histogram(&events);

    print_report(
        run_label,
        &[s_single, s_multi, s_rename, s_dir],
        &s_agg,
        &hist,
    );

    // Cleanup env var so we don't leak into other tests.
    unsafe {
        std::env::remove_var(sheaf_ablation::ABLATION_LOG_ENV);
    }
    sheaf_ablation::reset_handle_for_tests();

    s_agg
}
