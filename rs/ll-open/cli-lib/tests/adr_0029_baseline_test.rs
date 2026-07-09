//! ADR-0029 F-test measurement harness — BASELINE side (worktree flow).
//!
//! Bead: `ley-line-open-5f9829`. ADR: `docs/adr/0029-cas-backed-workspace.md`
//! (§5 falsifiability protocol).
//!
//! ── Why this file exists ──────────────────────────────────────────────
//! ADR-0029 proposes replacing `git worktree add` with CAS-backed manifest
//! mounts as the agent-dispatch primitive. §5 defines five falsifiable
//! claims (F1w–F5w). Without a documented BASELINE for the current
//! worktree flow, the future mount driver ships blind — there is no
//! reference to compare against, no "did we actually beat the number?"
//! gate.
//!
//! This file measures the BASELINE side of each F-gate. The FUTURE
//! mount-side measurements land in a separate follow-up bead
//! (Phase 1 mount driver — not yet filed as an implementation bead).
//! Each test carries a marker comment pointing at the future assertion
//! that will replace / extend it.
//!
//! ── What runs when ────────────────────────────────────────────────────
//! F3w (isolation), F4w (sub-file), F5w (commit fidelity) are fast and
//! run in `task ci` — no `#[ignore]`. F1w (startup) and F2w (storage)
//! synth a 100MB git repo and do 30 `git worktree add` iterations, which
//! is too slow for every-PR CI. They're `#[ignore]`-gated and run via
//! `task adr-0029:baseline`, which also emits the JSON artifact at
//! `docs/research/adr-0029-baselines.json`. Same fast/slow split mache's
//! `smells:dogfood` vs `smells` uses.
//!
//! ── Adversarial verification note ─────────────────────────────────────
//! Run the harness twice on the same machine — the two JSON artifacts
//! should be within 10% delta on p50/p99. Larger divergence means the
//! measurement itself is unreliable (e.g. filesystem cache effects the
//! warmup didn't cover). Same discipline the ADR-0026 Phase 2.0
//! `read_wall_time` bench used to validate itself before shipping.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::TempDir;

// ── Config ────────────────────────────────────────────────────────────

/// Synthetic-repo size for F1w / F2w. ADR §5 says "500MB test repo";
/// the bead task-spec allows scaling down to 100MB if 500MB is too slow.
/// A 100MB repo is enough for `git worktree add` cost to be legible above
/// the tempdir-setup floor; scaling further would trade legibility for
/// less runner disk pressure. Keep both parameterized so a follow-up can
/// bump under a bigger runner budget.
const F1W_REPO_SIZE_MB: usize = 100;
const F2W_REPO_SIZE_MB: usize = 100;

/// Iterations for F1w wall-time percentile capture. ADR §5.F1w doesn't
/// pin a number. 30 iterations trades slow-path budget (~10-15s at
/// ~300ms per iteration on macOS arm64) for a stable p50 on a shared
/// dev machine — 10 iterations showed ~50% p50 drift run-to-run across
/// three adversarial-verification passes; 30 samples pushes p50
/// variance below the 10% tolerance the bead task-spec asks for.
/// p99 with 30 samples is still edge-of-tail (sample #29 of 30) and
/// remains noisier — documented in the PR body as intrinsic to a
/// small-sample wall-clock measurement on a shared runner.
const F1W_ITERATIONS: usize = 30;

/// Concurrent-agent count for F2w. ADR §1 uses "4 concurrent agents"
/// as its worked example; §5.F2w says "N concurrent agents" without
/// pinning N. Keeping the concrete number = the example number.
const F2W_N_AGENTS: usize = 4;

/// Byte-target for each F4w input file. ADR §5.F4w says "3 subtrees from
/// files that are each > 10KB". 10KB per file gives the whole-file
/// baseline ~30KB — meaningfully above the "manifest of 3 subtrees ~1KB"
/// mount-side prediction, so the 30× reduction the ADR claims is visible.
const F4W_FILE_TARGET_BYTES: usize = 10_000;

/// How many AST subtrees the F4w bead scopes. ADR §5.F4w uses "3 subtrees";
/// same across F4w baseline and (future) mount-side measurement.
const F4W_SUBTREE_COUNT: usize = 3;

// ── Git process helpers ───────────────────────────────────────────────

/// Run `git <args>` inside `dir`, panicking on non-zero exit. Kept
/// terse because the test-flow shape is dominated by git invocations
/// and each caller reads better as a one-liner.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
    assert!(
        out.status.success(),
        "git {:?} in {} failed:\n--stdout--\n{}\n--stderr--\n{}",
        args,
        dir.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Synthesize a git repo of ~`size_mb` MB with a single initial commit.
///
/// Content shape: `n_files` files of `size_mb / n_files` MB each. Each
/// file holds deterministic pseudo-random bytes seeded off the file
/// index so the fixture is reproducible run-to-run — mandatory for the
/// "run twice, compare within 10%" adversarial verification. Bytes are
/// non-repeating enough that git's delta compression + packfile shape
/// don't shrink the checkout to a fraction of the intended size.
fn synth_repo(size_mb: usize, tmp: &Path) -> PathBuf {
    let repo = tmp.join("repo");
    fs::create_dir_all(&repo).unwrap();

    // Isolate the fixture from any user-level git config that could
    // add signing / hooks / templates and skew the wall-time numbers.
    // `git init` is fine; we set per-repo config for the identity and
    // gpg-sign bits that would otherwise fail in a bare env.
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@adr-0029.local"]);
    git(&repo, &["config", "user.name", "adr-0029 baseline"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    // Disable auto-CRLF and other line-ending games — F5w's diff hash
    // depends on byte-exact output.
    git(&repo, &["config", "core.autocrlf", "false"]);

    let n_files: usize = 100;
    let bytes_per_file = (size_mb * 1024 * 1024) / n_files;
    for i in 0..n_files {
        let path = repo.join(format!("f{i:03}.dat"));
        fs::write(&path, pseudo_random_bytes(i as u64, bytes_per_file)).unwrap();
    }

    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    repo
}

/// Deterministic pseudo-random bytes seeded off `seed`. LCG (splitmix-shaped
/// step) — fine for "make files that don't dedup" filler; not for anything
/// with cryptographic requirements. `blake3` is available but overkill for
/// filler bytes — this is faster and non-repeating enough for git's
/// content-addressed layer to see each file as distinct.
fn pseudo_random_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut state = seed
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(0xBF58_476D_1CE4_E5B9);
    while out.len() < len {
        state = state
            .wrapping_mul(0x2545_F491_4F6C_DD1D)
            .wrapping_add(0x9E37_79B9_7F4A_7C15);
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Recursive byte-sum of a directory tree. Used for F2w to measure
/// on-disk cost of a worktree. Skips broken symlinks (unlikely in the
/// synth-repo fixture; belt-and-braces for future refactors that might
/// introduce them).
fn dir_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(read) = fs::read_dir(&p) else { continue };
        for entry in read.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}

/// Compute p50/p99 of a duration vector via nearest-rank. Same shape as
/// the read_wall_time bench's helper — reusing the shape keeps the
/// baseline-JSON reader from having to reconcile two percentile
/// conventions.
fn percentiles(mut samples: Vec<Duration>) -> (Duration, Duration) {
    assert!(
        !samples.is_empty(),
        "cannot take percentiles of empty samples"
    );
    samples.sort();
    let p50_idx = ((samples.len() as f64 - 1.0) * 0.50).round() as usize;
    let p99_idx = ((samples.len() as f64 - 1.0) * 0.99).round() as usize;
    (samples[p50_idx], samples[p99_idx])
}

// ── F1w — startup wall-time ───────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct F1wResult {
    p50_ms: u64,
    p99_ms: u64,
    iterations: usize,
    repo_size_mb: usize,
}

/// Measure `git worktree add` wall-time on a synthetic repo.
///
/// One untimed warmup discard (per bead task-spec's "if startup is
/// dominated by first-time filesystem cache effects, do a warm-up
/// iteration + discard, then measure 10 more") — then `F1W_ITERATIONS`
/// timed iterations. Each iteration:
///   1. Time `git worktree add -b wt_<i> <path> HEAD`.
///   2. Cleanup `git worktree remove -f` + `git branch -D` so the next
///      iteration starts from the same steady state.
fn measure_f1w(size_mb: usize) -> F1wResult {
    let tmp = TempDir::new().expect("tmpdir");
    let repo = synth_repo(size_mb, tmp.path());

    // Warmup — untimed. Populates filesystem caches so the first timed
    // iteration doesn't carry cold-cache tax the other 9 don't.
    let wp = tmp.path().join("wt_warmup");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "wt_warmup",
            wp.to_str().unwrap(),
            "HEAD",
        ],
    );
    git(&repo, &["worktree", "remove", "-f", wp.to_str().unwrap()]);
    git(&repo, &["branch", "-D", "wt_warmup"]);

    let mut samples = Vec::with_capacity(F1W_ITERATIONS);
    for i in 0..F1W_ITERATIONS {
        let wp = tmp.path().join(format!("wt_{i}"));
        let branch = format!("wt_{i}");
        let t0 = Instant::now();
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &branch,
                wp.to_str().unwrap(),
                "HEAD",
            ],
        );
        samples.push(t0.elapsed());
        // Cleanup — not timed. Prevents runner disk pressure over 10
        // iterations at 100MB each (would be 1GB otherwise).
        git(&repo, &["worktree", "remove", "-f", wp.to_str().unwrap()]);
        git(&repo, &["branch", "-D", &branch]);
    }

    let (p50, p99) = percentiles(samples);
    F1wResult {
        p50_ms: p50.as_millis() as u64,
        p99_ms: p99.as_millis() as u64,
        iterations: F1W_ITERATIONS,
        repo_size_mb: size_mb,
    }
}

// ── F2w — storage cost ────────────────────────────────────────────────

/// Measure mean on-disk bytes per worktree when N agents are spawned
/// off the same repo. Returns MB.
///
/// The mount-side prediction is O(1): all N agents share the CAS and
/// only their CoW deltas add bytes. Under worktree flow, each agent
/// gets a full checkout — so mean-per-agent should be roughly
/// `repo_size_mb`. That gap is exactly what F2w falsifies against.
fn measure_f2w(size_mb: usize, n_agents: usize) -> f64 {
    let tmp = TempDir::new().expect("tmpdir");
    let repo = synth_repo(size_mb, tmp.path());

    let mut total_bytes: u64 = 0;
    for i in 0..n_agents {
        let wp = tmp.path().join(format!("wt_{i}"));
        let branch = format!("wt_{i}");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &branch,
                wp.to_str().unwrap(),
                "HEAD",
            ],
        );
        total_bytes += dir_bytes(&wp);
    }
    (total_bytes as f64) / (n_agents as f64) / 1024.0 / 1024.0
}

// ── F3w — isolation ───────────────────────────────────────────────────

/// Baseline probe: with a "bead scoped to `in_scope.rs`", can an agent
/// running in the worktree still `open("out_of_scope.rs")`?
///
/// The worktree flow gives bare filesystem access — the "scope" is
/// documentation, not enforcement. Under mount flow (§2.4) the manifest
/// literally lacks the entry, so open returns ENOENT. Return `true` if
/// the baseline OPEN SUCCEEDS (which is what falsifies the isolation
/// claim on this side; the ADR's insight is that ONLY the mount flow
/// can flip this to `false`).
fn measure_f3w() -> bool {
    let tmp = TempDir::new().expect("tmpdir");
    // Two files: one that the bead's `files:` scope names, one that it
    // doesn't. Under worktree flow both live on the filesystem; the
    // "scope" is honored by convention only.
    let in_scope = tmp.path().join("in_scope.rs");
    let out_of_scope = tmp.path().join("out_of_scope.rs");
    fs::write(&in_scope, b"// in scope\nfn foo() {}\n").unwrap();
    fs::write(&out_of_scope, b"// secret\nfn bar() {}\n").unwrap();

    // Simulate the agent: it tries to open a file the bead did NOT
    // declare. Under the worktree baseline this succeeds.
    fs::read_to_string(&out_of_scope).is_ok()
}

// ── F4w — sub-file granularity ────────────────────────────────────────

/// Under worktree flow, an agent working on a bead scoped to 3 AST
/// subtrees must load the whole containing file for each subtree
/// (there is no sub-file granularity in the filesystem view).
///
/// This measures the sum of the 3 files' bytes — the "whole-files"
/// number that the future mount flow will replace with a sum of the
/// 3 individual subtree byte-lengths (~1KB total per ADR §5.F4w).
fn measure_f4w() -> u64 {
    let tmp = TempDir::new().expect("tmpdir");
    // Three files, each ~10KB, each containing multiple function-like
    // AST subtrees. The bead is scoped to ONE subtree per file — under
    // worktree flow the agent nonetheless loads the whole file.
    let mut whole_file_bytes = 0u64;
    for i in 0..F4W_SUBTREE_COUNT {
        let path = tmp.path().join(format!("mod_{i}.rs"));
        let mut content = String::with_capacity(F4W_FILE_TARGET_BYTES + 512);
        // Emit function-shaped Rust so a real parser (tree-sitter) would
        // see multiple AST subtrees per file. Baseline doesn't need the
        // parser — but keeping the file shape realistic makes the mount-
        // side follow-up test's input a drop-in replacement.
        let mut fn_idx = 0;
        while content.len() < F4W_FILE_TARGET_BYTES {
            content.push_str(&format!(
                "fn mod_{i}_fn_{fn_idx}(a: u64, b: u64) -> u64 {{\n    let x = a.wrapping_mul({fn_idx});\n    let y = b.wrapping_add({fn_idx});\n    x ^ y\n}}\n\n"
            ));
            fn_idx += 1;
        }
        fs::write(&path, &content).unwrap();
        whole_file_bytes += fs::metadata(&path).unwrap().len();
    }
    whole_file_bytes
}

// ── F5w — commit fidelity ─────────────────────────────────────────────

/// The canonical edit script the F5w test applies. Constant so the mount-
/// side follow-up test can apply the identical edits and diff-compare.
///
/// Semantics: rename `foo` -> `bar`, add a second `bar();` call site,
/// delete the `// LEGACY:` comment. Matches ADR §5.F5w's worked example.
const F5W_ORIGINAL: &str =
    "// LEGACY: rename me\nfn foo() {\n    let x = 41;\n}\n\nfn main() {\n    foo();\n}\n";

const F5W_EDITED: &str =
    "fn bar() {\n    let x = 41;\n}\n\nfn main() {\n    bar();\n    bar();\n}\n";

/// Result of the F5w baseline capture — the raw diff (stored as an
/// in-tree fixture so mount-side can byte-compare) and its BLAKE3 hex
/// (stored in the JSON artifact for fast index / drift detection).
#[derive(Debug)]
struct F5wResult {
    diff: String,
    diff_blake3_hex: String,
}

/// Capture the git diff produced when the F5w edit script is applied via
/// the worktree flow.
///
/// Concretely: init a repo with `F5W_ORIGINAL`, `git worktree add` a new
/// branch, write `F5W_EDITED`, capture `git diff`. That diff is the
/// baseline artifact; the mount-side follow-up test must produce a
/// byte-identical diff to pass F5w.
fn measure_f5w() -> F5wResult {
    let tmp = TempDir::new().expect("tmpdir");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@adr-0029.local"]);
    git(&repo, &["config", "user.name", "adr-0029 baseline"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    git(&repo, &["config", "core.autocrlf", "false"]);

    fs::write(repo.join("src.rs"), F5W_ORIGINAL).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    // Worktree flow: spawn a fresh worktree on a new branch, edit inside.
    let wt = tmp.path().join("wt_edit");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "edit",
            wt.to_str().unwrap(),
            "HEAD",
        ],
    );
    fs::write(wt.join("src.rs"), F5W_EDITED).unwrap();

    // git diff — captured from inside the worktree so the header paths
    // are `a/src.rs b/src.rs` regardless of the outer tempdir layout.
    let diff = git(&wt, &["diff", "--no-color", "--no-ext-diff"]);
    let diff_blake3_hex = blake3::hash(diff.as_bytes()).to_hex().to_string();
    F5wResult {
        diff,
        diff_blake3_hex,
    }
}

// ── JSON artifact ─────────────────────────────────────────────────────

/// Path of the committed baseline JSON, relative to CARGO_MANIFEST_DIR
/// (`rs/ll-open/cli-lib/`). The `../../../` climb lands in the workspace
/// root, which contains `docs/research/`.
const BASELINE_JSON_REL: &str = "../../../docs/research/adr-0029-baselines.json";

/// Path of the committed baseline diff fixture (for F5w byte-compare).
const BASELINE_DIFF_REL: &str = "../../../docs/research/adr-0029-f5w-baseline.diff";

fn workspace_relative(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn uname_srm() -> String {
    Command::new("uname")
        .args(["-srm"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn today_iso() -> String {
    Command::new("date")
        .args(["-u", "+%Y-%m-%d"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn head_sha() -> String {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn format_baseline_json(
    f1: &F1wResult,
    f2_mb: f64,
    f3_readable: bool,
    f4_bytes: u64,
    f5: &F5wResult,
) -> String {
    format!(
        r#"{{
  "schema": "llo-adr-0029-baselines/v1",
  "captured_at": "{captured_at}",
  "captured_at_sha": "{sha}",
  "hardware": "{hardware}",
  "leyline_version": "{version}",
  "measurements": {{
    "f1w_worktree_startup": {{ "p50_ms": {p50}, "p99_ms": {p99}, "iterations": {iters}, "repo_size_mb": {repo_mb} }},
    "f2w_worktree_storage_per_agent_mb": {f2},
    "f3w_worktree_out_of_scope_readable": {f3},
    "f4w_worktree_bytes_loaded_per_3_subtree_bead": {f4},
    "f5w_worktree_diff_blake3": "{f5_hash}"
  }}
}}
"#,
        captured_at = today_iso(),
        sha = head_sha(),
        hardware = uname_srm(),
        version = env!("CARGO_PKG_VERSION"),
        p50 = f1.p50_ms,
        p99 = f1.p99_ms,
        iters = f1.iterations,
        repo_mb = f1.repo_size_mb,
        f2 = format!("{:.2}", f2_mb),
        f3 = f3_readable,
        f4 = f4_bytes,
        f5_hash = f5.diff_blake3_hex,
    )
}

// ── Fast tests (run in `task ci`) ─────────────────────────────────────

/// F3w — baseline side (worktree flow).
/// Future mount-side measurement will assert: OPEN returns ENOENT/EACCES
/// on out-of-scope paths (the manifest lacks the entry — §2.4).
/// See ADR-0029 §5.F3w and the Phase 1 mount driver follow-up bead.
#[test]
fn f3w_isolation_baseline() {
    let readable = measure_f3w();
    assert!(
        readable,
        "F3w baseline: worktree flow gives bare filesystem access, so \
         out-of-scope reads MUST succeed. If this failed, either the \
         synth fixture didn't lay files down correctly or the test is \
         being run under an unusually restrictive filesystem sandbox — \
         either way the baseline assumption is broken and the mount-side \
         follow-up test cannot compare against it."
    );
}

/// F4w — baseline side (worktree flow).
/// Future mount-side measurement will assert: manifest-resolved bytes
/// for the same 3-subtree bead < 5KB (per ADR §5.F4w pass criterion).
/// See ADR-0029 §5.F4w and the Phase 1 mount driver follow-up bead.
#[test]
fn f4w_sub_file_baseline() {
    let bytes = measure_f4w();
    // The bead is scoped to 3 subtrees. Under worktree flow the agent
    // loads whole files, so the byte-count should be ~3 * F4W_FILE_TARGET_BYTES.
    // Tolerance: functions push each file slightly over the target; assert
    // between target and 2× target to catch fixture regressions without
    // being brittle to formatting tweaks.
    let lower = (F4W_SUBTREE_COUNT * F4W_FILE_TARGET_BYTES) as u64;
    let upper = (F4W_SUBTREE_COUNT * F4W_FILE_TARGET_BYTES * 2) as u64;
    assert!(
        bytes >= lower && bytes <= upper,
        "F4w baseline: expected whole-file bytes in [{lower}, {upper}], got {bytes}"
    );
}

/// F5w — baseline side (worktree flow).
/// Future mount-side measurement will assert: manifest → git commit
/// produces byte-identical `git diff` output (per ADR §5.F5w pass
/// criterion). The committed diff fixture at `docs/research/adr-0029-f5w-baseline.diff`
/// is what the mount-side test byte-compares against.
/// See ADR-0029 §5.F5w and the Phase 1 mount driver follow-up bead.
#[test]
fn f5w_commit_fidelity_baseline() {
    let r = measure_f5w();
    // Sanity checks — the diff is a real unified diff over `src.rs`.
    assert!(
        r.diff.contains("--- a/src.rs"),
        "F5w baseline: git diff header missing — captured diff was:\n{}",
        r.diff
    );
    assert!(
        r.diff.contains("+++ b/src.rs"),
        "F5w baseline: git diff header missing — captured diff was:\n{}",
        r.diff
    );
    assert!(
        r.diff.contains("-fn foo()") && r.diff.contains("+fn bar()"),
        "F5w baseline: rename edit not visible in diff — captured diff was:\n{}",
        r.diff
    );
    // BLAKE3 hex is 64 chars.
    assert_eq!(r.diff_blake3_hex.len(), 64);

    // If the committed baseline fixture exists (i.e. we're not in a
    // fresh checkout that pre-dates the initial `task adr-0029:baseline`
    // run), the freshly-captured diff must byte-match it. This is the
    // regression gate: any change to git's diff output, tree-hash
    // stability, or the F5W_ORIGINAL/EDITED constants will fail here.
    let fixture_path = workspace_relative(BASELINE_DIFF_REL);
    if fixture_path.exists() {
        let committed =
            fs::read_to_string(&fixture_path).expect("read committed baseline diff fixture");
        assert_eq!(
            r.diff,
            committed,
            "F5w baseline: freshly-captured diff drifted from committed \
             fixture at {}. Re-run `task adr-0029:baseline` to \
             regenerate, or investigate whether F5W_ORIGINAL/EDITED \
             changed unintentionally.",
            fixture_path.display()
        );
    }
}

// ── Slow tests (run via `task adr-0029:baseline`) ─────────────────────

/// F1w — baseline side (worktree flow).
/// Future mount-side measurement will assert: mount startup p99_ms <
/// worktree p99_ms / 5 (per ADR §5.F1w pass criterion of < 1s on a 500MB
/// repo — proportionally scaled to the 100MB fixture used here).
/// See ADR-0029 §5.F1w and the Phase 1 mount driver follow-up bead.
#[test]
#[ignore = "SLOW: synths 100MB git repo + 30 `git worktree add` iterations. \
            Run via `task adr-0029:baseline`."]
fn f1w_startup_baseline() {
    let r = measure_f1w(F1W_REPO_SIZE_MB);
    // Baseline sanity: p99 must be non-zero (otherwise measurement is
    // broken); p50 <= p99 (percentile invariant).
    assert!(r.p99_ms > 0, "F1w baseline: p99 was 0ms — timer floor bug?");
    assert!(
        r.p50_ms <= r.p99_ms,
        "F1w baseline: p50 ({}) > p99 ({}) — percentile bug",
        r.p50_ms,
        r.p99_ms,
    );
    eprintln!(
        "F1w baseline (worktree startup): p50={}ms p99={}ms iters={} repo_size={}MB",
        r.p50_ms, r.p99_ms, r.iterations, r.repo_size_mb,
    );
}

/// F2w — baseline side (worktree flow).
/// Future mount-side measurement will assert: storage_per_agent_mb <
/// 10 (per ADR §5.F2w pass criterion — CoW delta bounds).
/// See ADR-0029 §5.F2w and the Phase 1 mount driver follow-up bead.
#[test]
#[ignore = "SLOW: synths 100MB git repo + 4 `git worktree add` iterations. \
            Run via `task adr-0029:baseline`."]
fn f2w_storage_baseline() {
    let mb_per_agent = measure_f2w(F2W_REPO_SIZE_MB, F2W_N_AGENTS);
    // Baseline expectation: each worktree gets a full checkout, so
    // mean-per-agent should be within an order of magnitude of
    // repo_size_mb. Loose bound: at least half the repo size (catches
    // filesystem tricks that would hide the O(N) cost).
    let lower = (F2W_REPO_SIZE_MB as f64) * 0.5;
    assert!(
        mb_per_agent >= lower,
        "F2w baseline: expected >= {lower}MB/agent (roughly full checkout), \
         got {mb_per_agent}MB. If this fails on a runner with \
         copy-on-write filesystem trickery (APFS clonefile?), the F2w \
         baseline needs a runner-shape adjustment."
    );
    eprintln!(
        "F2w baseline (worktree storage): {:.2}MB/agent × {} agents = {:.2}MB total (repo_size={}MB)",
        mb_per_agent,
        F2W_N_AGENTS,
        mb_per_agent * (F2W_N_AGENTS as f64),
        F2W_REPO_SIZE_MB,
    );
}

/// Emits the baseline JSON artifact + F5w diff fixture to
/// `docs/research/`. Runs all 5 measurements from scratch so the JSON
/// captures a single coherent snapshot — do not rely on the individual
/// `#[test]` functions to have run first.
///
/// This is the entry point for `task adr-0029:baseline`. Ignored by
/// default so `cargo test --workspace` never rewrites the committed
/// artifact under a contributor's feet.
#[test]
#[ignore = "SLOW + WRITES to docs/research/. Run via `task adr-0029:baseline` \
            when refreshing the committed baseline artifact."]
fn emit_baseline_artifact() {
    // Same measurement order as the ADR §5 gate order so the JSON reads
    // top-to-bottom the way the ADR does.
    eprintln!(
        "F1w: measuring worktree startup on {}MB repo…",
        F1W_REPO_SIZE_MB
    );
    let f1 = measure_f1w(F1W_REPO_SIZE_MB);
    eprintln!("  p50={}ms p99={}ms", f1.p50_ms, f1.p99_ms);

    eprintln!(
        "F2w: measuring worktree storage for {} agents on {}MB repo…",
        F2W_N_AGENTS, F2W_REPO_SIZE_MB
    );
    let f2 = measure_f2w(F2W_REPO_SIZE_MB, F2W_N_AGENTS);
    eprintln!("  {:.2}MB/agent", f2);

    eprintln!("F3w: probing out-of-scope readability under worktree flow…");
    let f3 = measure_f3w();
    eprintln!("  out_of_scope_readable={f3}");

    eprintln!("F4w: measuring whole-file bytes for 3-subtree bead…");
    let f4 = measure_f4w();
    eprintln!("  {f4} bytes");

    eprintln!("F5w: capturing diff hash for canonical edit script…");
    let f5 = measure_f5w();
    eprintln!("  blake3={}", f5.diff_blake3_hex);

    let json = format_baseline_json(&f1, f2, f3, f4, &f5);
    let json_path = workspace_relative(BASELINE_JSON_REL);
    fs::write(&json_path, &json)
        .unwrap_or_else(|e| panic!("write baseline JSON to {}: {e}", json_path.display()));
    eprintln!("wrote {}", json_path.display());

    let diff_path = workspace_relative(BASELINE_DIFF_REL);
    fs::write(&diff_path, &f5.diff).unwrap_or_else(|e| {
        panic!(
            "write baseline diff fixture to {}: {e}",
            diff_path.display()
        )
    });
    eprintln!("wrote {}", diff_path.display());

    // Echo the JSON to stderr for CI logs / adversarial-verification
    // diff pasting.
    eprintln!("── baseline JSON ──\n{json}");
}
