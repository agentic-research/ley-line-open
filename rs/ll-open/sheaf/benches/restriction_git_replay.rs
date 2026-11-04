//! ADR-0031 caveat #3 — git-replay superset-CORRECTNESS stress for the
//! restriction-addressed call-target review cache (bead
//! `ley-line-open-055f79`, was `f3a81e`).
//!
//! # The one claim this stresses
//!
//! ADR-0031's positive result is: for the call-target review of a function
//! `F`,
//!
//! > **restriction-unchanged ⇒ review-output-unchanged.**
//!
//! The restriction (`restriction_for_call_target`) hashes a *sound
//! superset* of the review's input rows; the review
//! (`review_call_targets`) is the oracle — the resolved call graph itself.
//! The `restriction_review_real` test proved this GO on **9 hand-picked
//! fixtures** plus a structural sound-superset argument. This bench stresses
//! the SAME claim over REAL edits from LLO's own `rs/` history, hunting for
//! a **false-skip**: an edit where the restriction hash is UNCHANGED but the
//! review result CHANGED. One false-skip = the superset definition has a gap
//! (the analog of the cross-file-def-rename dependency the fixture
//! experiment already found is load-bearing).
//!
//! # Method (free ground truth, mirrors `git_replay_invalidation.rs`)
//!
//! Replay N real commits touching `rs/`. For each commit the CORPUS is the
//! set of `.rs` files it changed that parse on BOTH the parent and the
//! commit — a genuinely MULTI-FILE corpus, so the review resolves call
//! targets ACROSS files (co-changed caller+callee) exactly as the fixture
//! `corpus-def-rename` did. Build a `FactSubstrate` for the before-corpus
//! and the after-corpus under `ContainerKeying::Stable` (the deployment-
//! sound reflow-invariant identity, ADR-0031 caveat #1). Then, for every
//! `function_item` region present on both sides whose OWN source bytes
//! changed (a real region-edit), compute:
//!
//! * **restriction changed?** `restriction_for_call_target(before) !=
//!   restriction_for_call_target(after)`, and
//! * **review changed? (oracle)** `review_call_targets(before) !=
//!   review_call_targets(after)`.
//!
//! Tabulate the 2×2 confusion matrix over the region-edit population:
//!
//! | | review unchanged | review changed |
//! |---|---|---|
//! | restriction **unchanged** | true-skip (the win) | **FALSE-SKIP (the gap)** |
//! | restriction **changed**   | wasted-inval        | true-inval |
//!
//! # Verdict rule (falsifiable)
//!
//! `restriction false_skip == 0` over N ≥ 400 real region-edits ⇒ the
//! positive claim survives replay. Any false-skip is printed with its exact
//! edit (commit / file / function) and the missed dependency, and `main`
//! exits non-zero — that is the finding, NOT a bug to engineer away.
//!
//! `WholeObject` (byte hash of the whole corpus) is the sound baseline: the
//! corpus always contains a changed file, so it skips ZERO region-edits. The
//! restriction's true-skips are exactly the recomputes it saves that
//! whole-object caching cannot.
//!
//! # How to run
//!
//! ```text
//! cargo bench -p leyline-sheaf --bench restriction_git_replay
//! # knobs (env): RESTR_N_COMMITS (default 600), RESTR_MAX_EDITS (default 20000),
//! #              RESTR_MAX_CORPUS_FILES (default 40, bounds review join cost)
//! ```
//!
//! `harness = false`: a plain experiment binary. It prints the confusion
//! matrix + verdict to stdout and exits non-zero on any false-skip.

#[path = "../tests/common/mod.rs"]
mod common;

use common::{ReviewTarget, build_substrate, review_targets};
use leyline_sheaf::restriction_cache::{
    ContainerKeying, FactSubstrate, restriction_for_call_target, review_call_targets,
    whole_object_hash,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

// ---------------------------------------------------------------------------
// git plumbing (mirrors git_replay_invalidation.rs)
// ---------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let out = Command::new("git")
        .args(["-C", manifest, "rev-parse", "--show-toplevel"])
        .output()
        .expect("git rev-parse");
    PathBuf::from(String::from_utf8(out.stdout).expect("utf8 toplevel").trim())
}

/// Run git against `root`; stdout bytes, or `None` when git exits non-zero
/// (missing object, added/deleted path, binary, …).
fn git_bytes(root: &PathBuf, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

fn git_str(root: &PathBuf, args: &[&str]) -> Option<String> {
    git_bytes(root, args).and_then(|b| String::from_utf8(b).ok())
}

/// The experiment's own churny files — excluded so the bench never measures
/// itself (their edits would be pure noise about the harness, not the code
/// under review).
fn is_experiment_file(path: &str) -> bool {
    path.ends_with("tests/common/mod.rs")
        || path.contains("restriction_git_replay")
        || path.contains("restriction_review_real")
        || path.contains("git_replay_invalidation")
        || path.contains("ast_structural_discrimination")
        || path.contains("embedding_stalk_divergence")
}

// ---------------------------------------------------------------------------
// Confusion matrix
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Confusion {
    /// restriction unchanged & review unchanged — the sound win.
    true_skip: u64,
    /// restriction unchanged & review CHANGED — the superset gap (must be 0).
    false_skip: u64,
    /// restriction changed & review unchanged — over-invalidation (sound).
    wasted_inval: u64,
    /// restriction changed & review changed — correctly invalidated.
    true_inval: u64,
    /// WholeObject would have skipped (corpus byte-identical) — always 0
    /// here, tracked to make the vs-WholeObject comparison explicit.
    whole_object_skip: u64,
}

impl Confusion {
    fn total(&self) -> u64 {
        self.true_skip + self.false_skip + self.wasted_inval + self.true_inval
    }
}

/// One false-skip, recorded verbatim for the finding (never suppressed).
struct FalseSkip {
    commit: String,
    source_id: String,
    name: String,
    occurrence: u32,
}

// ---------------------------------------------------------------------------
// Per-commit measurement
// ---------------------------------------------------------------------------

/// Alignment key for a function region: (file, name, document-order
/// occurrence). Stable across an edit that leaves F's name in place.
type AlignKey = (String, String, u32);

/// Index a corpus's review targets by alignment key, and record which stable
/// container ids are UNIQUE in the corpus. A non-unique id means two
/// functions share a signature (`Stable` keying is body-invariant), so their
/// refs merge under one container — those targets are excluded so every
/// measured unit is exactly one function.
fn index_targets(
    targets: Vec<ReviewTarget>,
) -> (BTreeMap<AlignKey, ReviewTarget>, BTreeMap<String, u32>) {
    let mut by_key = BTreeMap::new();
    let mut container_mult: BTreeMap<String, u32> = BTreeMap::new();
    for t in targets {
        *container_mult.entry(t.container.clone()).or_insert(0) += 1;
        by_key.insert((t.source_id.clone(), t.name.clone(), t.occurrence), t);
    }
    (by_key, container_mult)
}

#[allow(clippy::too_many_arguments)]
fn measure_commit(
    commit: &str,
    before_corpus: &[(String, String)],
    after_corpus: &[(String, String)],
    cm: &mut Confusion,
    false_skips: &mut Vec<FalseSkip>,
    excluded_collisions: &mut u64,
) {
    let sub_before: FactSubstrate = build_substrate(before_corpus, ContainerKeying::Stable);
    let sub_after: FactSubstrate = build_substrate(after_corpus, ContainerKeying::Stable);

    let (before_by_key, before_mult) =
        index_targets(review_targets(before_corpus, ContainerKeying::Stable));
    let (after_by_key, after_mult) =
        index_targets(review_targets(after_corpus, ContainerKeying::Stable));

    // WholeObject baseline: byte hash of the whole corpus. Same for every
    // region in this commit; a changed file is always present, so it never
    // matches — recorded per region-edit for an honest vs-baseline count.
    let whole_object_unchanged =
        whole_object_hash(before_corpus) == whole_object_hash(after_corpus);

    // Per-file source lookup for byte-span slicing.
    let before_src: BTreeMap<&str, &str> = before_corpus
        .iter()
        .map(|(p, s)| (p.as_str(), s.as_str()))
        .collect();
    let after_src: BTreeMap<&str, &str> = after_corpus
        .iter()
        .map(|(p, s)| (p.as_str(), s.as_str()))
        .collect();

    for (key, tb) in &before_by_key {
        let Some(ta) = after_by_key.get(key) else {
            continue; // function removed/renamed — not a same-region edit
        };

        // Same-signature collision on either side merges refs — exclude.
        if before_mult.get(&tb.container).copied().unwrap_or(0) != 1
            || after_mult.get(&ta.container).copied().unwrap_or(0) != 1
        {
            *excluded_collisions += 1;
            continue;
        }

        let (Some(bs), Some(as_)) = (
            before_src.get(tb.source_id.as_str()),
            after_src.get(ta.source_id.as_str()),
        ) else {
            continue;
        };
        let bytes_b = &bs.as_bytes()[tb.start..tb.end];
        let bytes_a = &as_.as_bytes()[ta.start..ta.end];
        if bytes_b == bytes_a {
            continue; // F's own bytes untouched — not a region-edit
        }

        // The two hashes over the SAME corpus, before vs after.
        let mut scratch = 0u64;
        let restriction_unchanged =
            restriction_for_call_target(&sub_before, &tb.container, &mut scratch)
                == restriction_for_call_target(&sub_after, &ta.container, &mut scratch);

        // The oracle: F's resolved call graph.
        let review_unchanged = review_call_targets(&sub_before, &tb.container, &mut scratch)
            == review_call_targets(&sub_after, &ta.container, &mut scratch);

        match (restriction_unchanged, review_unchanged) {
            (true, true) => cm.true_skip += 1,
            (true, false) => {
                cm.false_skip += 1;
                false_skips.push(FalseSkip {
                    commit: commit.to_string(),
                    source_id: tb.source_id.clone(),
                    name: tb.name.clone(),
                    occurrence: tb.occurrence,
                });
            }
            (false, true) => cm.wasted_inval += 1,
            (false, false) => cm.true_inval += 1,
        }
        if whole_object_unchanged {
            cm.whole_object_skip += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// main — replay, tabulate, verdict
// ---------------------------------------------------------------------------

fn main() {
    let root = repo_root();
    let n_commits: usize = env_usize("RESTR_N_COMMITS", 600);
    let max_edits: usize = env_usize("RESTR_MAX_EDITS", 20_000);
    let max_corpus_files: usize = env_usize("RESTR_MAX_CORPUS_FILES", 40);

    eprintln!("restriction git-replay: root={}", root.display());
    eprintln!("N_COMMITS={n_commits}  MAX_EDITS={max_edits}  MAX_CORPUS_FILES={max_corpus_files}");

    let log = git_str(
        &root,
        &[
            "log",
            &format!("-n{n_commits}"),
            "--no-merges",
            "--format=%H",
            "--",
            "rs",
        ],
    )
    .expect("git log");
    let commits: Vec<&str> = log
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    eprintln!("commits touching rs/: {}", commits.len());

    let mut cm = Confusion::default();
    let mut false_skips: Vec<FalseSkip> = Vec::new();
    let mut excluded_collisions = 0u64;
    let mut commits_scanned = 0usize;
    let mut corpora_measured = 0usize;

    'outer: for h in &commits {
        let parent_rev = format!("{h}~1");
        let Some(changed) = git_str(
            &root,
            &[
                "diff-tree",
                "--no-commit-id",
                "--name-only",
                "-r",
                h,
                "--",
                "rs",
            ],
        ) else {
            continue;
        };
        commits_scanned += 1;

        // The corpus = every changed .rs path that exists AND parses on both
        // sides. Cap the file count to bound the review's unindexed def join.
        let mut before_corpus: Vec<(String, String)> = Vec::new();
        let mut after_corpus: Vec<(String, String)> = Vec::new();
        for path in changed
            .lines()
            .map(str::trim)
            .filter(|p| p.ends_with(".rs"))
            .filter(|p| !is_experiment_file(p))
        {
            if before_corpus.len() >= max_corpus_files {
                break;
            }
            let (Some(before), Some(after)) = (
                git_bytes(&root, &["show", &format!("{parent_rev}:{path}")]),
                git_bytes(&root, &["show", &format!("{h}:{path}")]),
            ) else {
                continue; // file added or deleted this commit
            };
            let (Ok(bs), Ok(as_)) = (String::from_utf8(before), String::from_utf8(after)) else {
                continue; // non-utf8 — not source we parse
            };
            before_corpus.push((path.to_string(), bs));
            after_corpus.push((path.to_string(), as_));
        }
        if before_corpus.is_empty() {
            continue;
        }
        corpora_measured += 1;

        measure_commit(
            h,
            &before_corpus,
            &after_corpus,
            &mut cm,
            &mut false_skips,
            &mut excluded_collisions,
        );
        if cm.total() as usize >= max_edits {
            eprintln!("hit MAX_EDITS cap at {} region-edits", cm.total());
            break 'outer;
        }
    }

    report(
        &cm,
        &false_skips,
        excluded_collisions,
        commits.len(),
        commits_scanned,
        corpora_measured,
    );

    // Verdict gate: any false-skip is the finding — exit non-zero AFTER the
    // report so the confusion matrix and the exact failing edits are printed.
    if cm.false_skip > 0 {
        std::process::exit(1);
    }
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn report(
    cm: &Confusion,
    false_skips: &[FalseSkip],
    excluded_collisions: u64,
    commits: usize,
    commits_scanned: usize,
    corpora_measured: usize,
) {
    let total = cm.total();
    let review_changed = cm.false_skip + cm.true_inval;
    let review_unchanged = cm.true_skip + cm.wasted_inval;

    println!();
    println!("=== ADR-0031 caveat #3 — restriction git-replay superset-correctness ===");
    println!(
        "corpus: LLO rs/ tree, {commits} commits scanned ({commits_scanned} with rs/ changes), \
         {corpora_measured} multi-file commit-corpora measured"
    );
    println!("region-edits measured: {total}");
    println!("  review CHANGED (oracle):   {review_changed}");
    println!("  review UNCHANGED (oracle): {review_unchanged}");
    println!("  same-signature targets excluded (Stable-id collision): {excluded_collisions}");
    if total == 0 {
        println!("no region-edits measured — nothing to report");
        return;
    }

    println!();
    println!("confusion matrix (restriction hash vs review oracle, Stable keying):");
    println!(
        "{:>26} | {:>16} {:>14}",
        "", "review UNCHANGED", "review CHANGED"
    );
    println!("{}", "-".repeat(62));
    println!(
        "{:>26} | {:>16} {:>14}",
        "restriction UNCHANGED", cm.true_skip, cm.false_skip
    );
    println!(
        "{:>26} | {:>16} {:>14}",
        "restriction CHANGED", cm.wasted_inval, cm.true_inval
    );

    let true_skip_rate = cm.true_skip as f64 / review_unchanged.max(1) as f64;
    let false_skip_rate = cm.false_skip as f64 / review_changed.max(1) as f64;
    println!();
    println!(
        "restriction false_skip = {}  (false_skip_rate = {}/{} = {:.4})",
        cm.false_skip, cm.false_skip, review_changed, false_skip_rate
    );
    println!(
        "restriction true_skip  = {}  (true_skip_rate  = {}/{} = {:.4} of review-unchanged edits)",
        cm.true_skip, cm.true_skip, review_unchanged, true_skip_rate
    );
    println!(
        "WholeObject true_skip  = {}  (0 by construction — the corpus always holds a changed file)",
        cm.whole_object_skip
    );
    println!(
        "→ restriction recovers {} sound skips WholeObject cannot, at {} false-skips.",
        cm.true_skip, cm.false_skip
    );

    println!();
    println!("=== VERDICT ===");
    if cm.false_skip == 0 {
        println!(
            "ZERO false-skips over {total} real region-edits → restriction-unchanged ⇒ \
             review-unchanged SURVIVES replay. The ADR-0031 positive claim holds."
        );
    } else {
        println!(
            "{} FALSE-SKIP(S) over {total} region-edits → the restriction superset has a GAP. \
             Exact failing edits (the finding — a missed input dependency):",
            cm.false_skip
        );
        for fs in false_skips {
            println!(
                "  FALSE-SKIP  commit={}  file={}  fn={}#{}",
                &fs.commit[..fs.commit.len().min(12)],
                fs.source_id,
                fs.name,
                fs.occurrence
            );
        }
    }
}
