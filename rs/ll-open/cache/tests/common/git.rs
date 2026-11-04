//! Git plumbing for the replay benches: the commit list and each commit's
//! `(parent, commit)` corpora. Shared by `leyline-cache`'s
//! `restriction_git_replay` and `leyline-sheaf`'s `sheaf_restriction_replay`,
//! so both arms see the SAME history.

use super::ablation::{Arm, ArmStats, build_step, print_table, run_step};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

/// Replay the history of the repository holding `manifest_dir` through
/// `arms`, print the table, and exit non-zero on any false skip in an arm
/// that claims soundness. Knobs (env): `RESTR_N_COMMITS` (default 60),
/// `RESTR_MAX_CORPUS_FILES` (default 40).
pub fn replay_main(title: &str, manifest_dir: &str, arms: &mut [Box<dyn Arm>]) {
    let root = repo_root(manifest_dir);
    let n_commits = env_usize("RESTR_N_COMMITS", 60);
    let max_files = env_usize("RESTR_MAX_CORPUS_FILES", 40);
    let history = commits(&root, n_commits);
    eprintln!(
        "{title}: root={} commits={} (requested {n_commits}) max_files={max_files}",
        root.display(),
        history.len()
    );

    let mut stats: Vec<ArmStats> = Vec::new();
    let mut steps = 0usize;
    let mut excluded = 0u64;
    let t0 = Instant::now();
    for h in &history {
        let Some((before, after)) = commit_corpora(&root, h, max_files) else {
            continue;
        };
        let (step, ex) = build_step(&h[..12], &before, &after);
        excluded += ex;
        steps += 1;
        run_step(&step, arms, &mut stats);
    }

    println!();
    println!("=== {title} ===");
    println!(
        "commits replayed: {} ({}..{})  steps with a .rs corpus: {steps}  \
         same-signature functions excluded: {excluded}  wall: {:.2?}",
        history.len(),
        history.first().map(|h| &h[..12]).unwrap_or("-"),
        history.last().map(|h| &h[..12]).unwrap_or("-"),
        t0.elapsed()
    );
    print_table(&stats);

    let unsound: u64 = stats
        .iter()
        .filter(|s| s.claims_soundness)
        .map(|s| s.false_skips + s.warm_false_skips)
        .sum();
    println!();
    if unsound == 0 {
        println!("VERDICT: 0 false skips in every arm that claims soundness.");
    } else {
        println!("VERDICT: {unsound} FALSE SKIP(S) in an arm that claims soundness (see above).");
        std::process::exit(1);
    }
}

/// The repository holding `manifest_dir`.
pub fn repo_root(manifest_dir: &str) -> PathBuf {
    let out = Command::new("git")
        .args(["-C", manifest_dir, "rev-parse", "--show-toplevel"])
        .output()
        .expect("git rev-parse");
    PathBuf::from(String::from_utf8(out.stdout).expect("utf8 toplevel").trim())
}

/// Run git against `root`; stdout bytes, or `None` when git exits non-zero
/// (missing object, added/deleted path, binary, …).
fn git_bytes(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

fn git_str(root: &Path, args: &[&str]) -> Option<String> {
    git_bytes(root, args).and_then(|b| String::from_utf8(b).ok())
}

/// The experiment's own churny files — excluded so the replay never
/// measures itself.
fn is_experiment_file(path: &str) -> bool {
    path.ends_with("tests/common/mod.rs")
        || path.ends_with("tests/common/ablation.rs")
        || path.ends_with("tests/common/git.rs")
        || path.contains("restriction_git_replay")
        || path.contains("sheaf_restriction_replay")
        || path.contains("restriction_review_real")
        || path.contains("git_replay_invalidation")
        || path.contains("ast_structural_discrimination")
        || path.contains("embedding_stalk_divergence")
}

/// A knob from the environment.
pub fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// The `n` most recent non-merge commits touching `rs/`, oldest first.
pub fn commits(root: &Path, n: usize) -> Vec<String> {
    let log = git_str(
        root,
        &[
            "log",
            &format!("-n{n}"),
            "--no-merges",
            "--format=%H",
            "--",
            "rs",
        ],
    )
    .expect("git log");
    let mut v: Vec<String> = log
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    v.reverse();
    v
}

/// The `(parent, commit)` corpora of one commit.
pub type Corpora = (Vec<(String, String)>, Vec<(String, String)>);

/// Every `.rs` file under `rs/` that commit `h` changed and that exists on
/// both sides, capped at `max_files`; `None` when there is none.
pub fn commit_corpora(root: &Path, h: &str, max_files: usize) -> Option<Corpora> {
    let changed = git_str(
        root,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            h,
            "--",
            "rs",
        ],
    )?;
    let parent = format!("{h}~1");
    let mut before = Vec::new();
    let mut after = Vec::new();
    for path in changed
        .lines()
        .map(str::trim)
        .filter(|p| p.ends_with(".rs") && !is_experiment_file(p))
    {
        if before.len() >= max_files {
            break;
        }
        let (Some(b), Some(a)) = (
            git_bytes(root, &["show", &format!("{parent}:{path}")]),
            git_bytes(root, &["show", &format!("{h}:{path}")]),
        ) else {
            continue; // added or deleted this commit
        };
        let (Ok(b), Ok(a)) = (String::from_utf8(b), String::from_utf8(a)) else {
            continue;
        };
        before.push((path.to_string(), b));
        after.push((path.to_string(), a));
    }
    (!before.is_empty()).then_some((before, after))
}
