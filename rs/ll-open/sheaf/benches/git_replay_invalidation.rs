//! ADR-0030 Rung 2 — Milestone B (bead `ley-line-open-d50164`): the
//! VALUE experiment. Reached only because Milestone A's discrimination
//! held (`tests/ast_structural_discrimination.rs`).
//!
//! # The one question the whole ADR hinges on
//!
//! Does a RENAME-INVARIANT AST-structural embedding's distance predict
//! whether a region's derived facts (`node_defs` / `node_refs`) changed?
//!
//! # Method (free ground truth)
//!
//! Replay N real commits of LLO's own `rs/` tree. For every function that
//! exists in both the parent and the commit and whose source bytes
//! changed (a real region-edit), compute:
//!
//! * (a) the **structural δ⁰ distance** `d/D` between the before/after
//!   kind sequences (the "close, skip if `d < EPS`" quantity), and
//! * (b) the **oracle**: re-derive `node_defs`/`node_refs` for the
//!   before/after subtrees via `leyline_ts::refs::extract_rust` and ask
//!   whether the fact SET changed.
//!
//! Sweep EPS and tabulate the confusion matrix per edit:
//!
//! * **true-skip**  `d < EPS ∧ ¬facts_changed` — work saved (the payoff);
//! * **false-neg**  `d < EPS ∧  facts_changed` — would serve stale (must
//!   be ≈ 0 at a useful EPS; caught by Rung 3's hash net);
//! * **wasted-inval** `d ≥ EPS ∧ ¬facts_changed` — unnecessary work;
//! * **true-inval** `d ≥ EPS ∧ facts_changed` — correctly invalidated.
//!
//! Baseline SHA gate (any byte change ⇒ invalidate): among these edits it
//! has 0 true-skips and 0 false-negatives by construction.
//!
//! # Verdict rule (falsifiable)
//!
//! SUCCESS = an EPS band with MEANINGFUL true-skip AND near-zero
//! false-negative → the math pays rent, with a number. FALSIFICATION =
//! the ROC is a diagonal (skip decision uncorrelated with fact stability)
//! → structural distance does not predict fact stability → ADR-0030 dies
//! at Rung 2.
//!
//! # How to run
//!
//! ```text
//! cargo bench -p leyline-sheaf --bench git_replay_invalidation
//! # knobs (env): RUNG2_N_COMMITS (default 400), RUNG2_MAX_EDITS (default 8000)
//! ```
//!
//! `harness = false`: a plain experiment binary, not a criterion timing
//! bench. It prints the confusion matrix / ROC to stdout.

#[path = "../tests/common/mod.rs"]
mod common;

use common::{derive_facts_in_tree, frac, kind_sequence, parse_rust, structural_stalk_from_kinds};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use tree_sitter::{Node, Tree};

// ---------------------------------------------------------------------------
// git plumbing
// ---------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let out = Command::new("git")
        .args(["-C", manifest, "rev-parse", "--show-toplevel"])
        .output()
        .expect("git rev-parse");
    PathBuf::from(String::from_utf8(out.stdout).expect("utf8 toplevel").trim())
}

/// Run git against `root`; stdout bytes, or `None` when git exits
/// non-zero (missing object, added/deleted path, binary, …).
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

// ---------------------------------------------------------------------------
// Region (function) extraction + alignment
// ---------------------------------------------------------------------------

/// A `function_item` region: alignment key + subtree byte span.
struct Region {
    start: usize,
    end: usize,
}

fn collect_fn_nodes<'t>(node: Node<'t>, out: &mut Vec<Node<'t>>) {
    if node.kind() == "function_item" {
        out.push(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_fn_nodes(child, out);
    }
}

/// `function_item` regions in document order, keyed by `name#occurrence`.
///
/// Occurrence disambiguates a name appearing more than once in a file
/// (e.g. two `fn new` in two impls). Alignment by (name, order) is
/// imperfect under function REORDERING — a reorder shows up as spurious
/// edits (symmetric noise that never flips the correlation question),
/// documented in the writeup.
fn collect_regions(tree: &Tree, src: &[u8]) -> BTreeMap<String, Region> {
    let mut out = BTreeMap::new();
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    let mut nodes = Vec::new();
    collect_fn_nodes(tree.root_node(), &mut nodes);
    for node in nodes {
        let name = node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(src).ok())
            .unwrap_or("<anon>");
        let c = counts.entry(name.to_string()).or_insert(0);
        let key = format!("{name}#{c}");
        *c += 1;
        out.insert(
            key,
            Region {
                start: node.start_byte(),
                end: node.end_byte(),
            },
        );
    }
    out
}

fn node_at<'t>(tree: &'t Tree, start: usize, end: usize) -> Option<Node<'t>> {
    let mut all = Vec::new();
    collect_fn_nodes(tree.root_node(), &mut all);
    all.into_iter()
        .find(|n| n.start_byte() == start && n.end_byte() == end)
}

// ---------------------------------------------------------------------------
// One measured region-edit
// ---------------------------------------------------------------------------

struct Edit {
    dist: f64,
    facts_changed: bool,
}

fn measure_file(before_src: &[u8], after_src: &[u8], edits: &mut Vec<Edit>) {
    let (Some(tb), Some(ta)) = (parse_rust(before_src), parse_rust(after_src)) else {
        return;
    };
    let rb = collect_regions(&tb, before_src);
    let ra = collect_regions(&ta, after_src);

    for (key, reg_b) in &rb {
        let Some(reg_a) = ra.get(key) else {
            continue; // function removed/renamed — not a same-region edit
        };
        let bytes_b = &before_src[reg_b.start..reg_b.end];
        let bytes_a = &after_src[reg_a.start..reg_a.end];
        if bytes_b == bytes_a {
            continue; // untouched region
        }
        let (Some(nb), Some(na)) = (
            node_at(&tb, reg_b.start, reg_b.end),
            node_at(&ta, reg_a.start, reg_a.end),
        ) else {
            continue;
        };

        // (a) structural δ⁰ distance over the kind sequences.
        let mut kb = Vec::new();
        let mut ka = Vec::new();
        kind_sequence(nb, &mut kb);
        kind_sequence(na, &mut ka);
        let dist = frac(
            &structural_stalk_from_kinds(&kb),
            &structural_stalk_from_kinds(&ka),
        );

        // (b) oracle: did the region's derived facts change?
        let facts_changed =
            derive_facts_in_tree(nb, before_src) != derive_facts_in_tree(na, after_src);

        edits.push(Edit {
            dist,
            facts_changed,
        });
    }
}

// ---------------------------------------------------------------------------
// main — replay, sweep, verdict
// ---------------------------------------------------------------------------

fn main() {
    let root = repo_root();
    let n_commits: usize = env_usize("RUNG2_N_COMMITS", 400);
    let max_edits: usize = env_usize("RUNG2_MAX_EDITS", 8000);

    eprintln!("Rung-2 git-replay: root={}", root.display());
    eprintln!("N_COMMITS={n_commits}  MAX_EDITS={max_edits}");

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

    let mut edits: Vec<Edit> = Vec::new();
    let mut files_scanned = 0usize;
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
        for path in changed
            .lines()
            .map(str::trim)
            .filter(|p| p.ends_with(".rs"))
        {
            // Skip the experiment's own churny files.
            if path.contains("git_replay_invalidation")
                || path.ends_with("tests/common/mod.rs")
                || path.ends_with("ast_structural_discrimination.rs")
                || path.ends_with("embedding_stalk_divergence.rs")
            {
                continue;
            }
            let (Some(before), Some(after)) = (
                git_bytes(&root, &["show", &format!("{parent_rev}:{path}")]),
                git_bytes(&root, &["show", &format!("{h}:{path}")]),
            ) else {
                continue; // file added or deleted this commit
            };
            files_scanned += 1;
            measure_file(&before, &after, &mut edits);
            if edits.len() >= max_edits {
                eprintln!("hit MAX_EDITS cap at {} edits", edits.len());
                break 'outer;
            }
        }
    }

    report(&edits, commits.len(), files_scanned);
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn report(edits: &[Edit], commits: usize, files_scanned: usize) {
    let total = edits.len();
    let changed = edits.iter().filter(|e| e.facts_changed).count();
    let unchanged = total - changed;
    eprintln!("files_scanned={files_scanned}  region_edits={total}");

    println!();
    println!("=== ADR-0030 Rung 2 — git-replay value experiment ===");
    println!(
        "corpus: LLO rs/ tree, {commits} commits scanned, {files_scanned} (file,commit) pairs"
    );
    println!("region-edits measured: {total}");
    println!("  facts CHANGED (oracle):   {changed}");
    println!("  facts UNCHANGED (oracle): {unchanged}");
    if total == 0 {
        println!("no edits measured — nothing to report");
        return;
    }

    let mean = |want: bool| -> f64 {
        let (mut s, mut n) = (0.0, 0usize);
        for e in edits {
            if e.facts_changed == want {
                s += e.dist;
                n += 1;
            }
        }
        if n == 0 { f64::NAN } else { s / n as f64 }
    };
    let mean_changed = mean(true);
    let mean_unchanged = mean(false);
    println!();
    println!(
        "mean structural d/D  |  facts changed = {mean_changed:.4}   facts unchanged = {mean_unchanged:.4}   (Δ = {:.4})",
        mean_changed - mean_unchanged
    );
    println!("(if these are ~equal, structural distance is blind to fact change → diagonal ROC)");

    // How many fact-changing edits are structurally INVISIBLE (d==0)?
    // These are the irreducible false-negatives at any EPS>0 — the
    // pure-callee-swap blind spot, measured on real data.
    let invisible_changed = edits
        .iter()
        .filter(|e| e.facts_changed && e.dist == 0.0)
        .count();
    let invisible_unchanged = edits
        .iter()
        .filter(|e| !e.facts_changed && e.dist == 0.0)
        .count();
    println!();
    println!("structurally-invisible edits (d/D == 0):");
    println!(
        "  facts changed  & d==0: {invisible_changed}   (irreducible false-negatives — the blind spot)"
    );
    println!("  facts unchanged & d==0: {invisible_unchanged}   (free true-skips at any EPS>0)");

    // EPS sweep.
    println!();
    println!("EPS sweep (skip iff d/D < EPS):");
    println!(
        "{:>5} | {:>9} {:>8} | {:>9} {:>8} | {:>8} {:>7}",
        "EPS", "true-skip", "false-N", "skip-rate", "FN-rate", "wasted", "recall"
    );
    println!("{}", "-".repeat(70));
    let mut roc: Vec<(f64, f64, f64)> = Vec::new(); // (eps, fn_rate, skip_rate)
    for i in 0..=30 {
        let eps = i as f64 * 0.01;
        let (mut true_skip, mut false_neg, mut wasted, mut true_inval) = (0usize, 0, 0, 0);
        for e in edits {
            match (e.dist < eps, e.facts_changed) {
                (true, false) => true_skip += 1,
                (true, true) => false_neg += 1,
                (false, false) => wasted += 1,
                (false, true) => true_inval += 1,
            }
        }
        let skip_rate = true_skip as f64 / unchanged.max(1) as f64;
        let fn_rate = false_neg as f64 / changed.max(1) as f64;
        let recall = true_inval as f64 / changed.max(1) as f64;
        roc.push((eps, fn_rate, skip_rate));
        if i % 2 == 0 {
            println!(
                "{:>5.2} | {:>9} {:>8} | {:>8.1}% {:>7.2}% | {:>8} {:>6.1}%",
                eps,
                true_skip,
                false_neg,
                skip_rate * 100.0,
                fn_rate * 100.0,
                wasted,
                recall * 100.0,
            );
        }
    }

    // Verdict: best true-skip under FN budgets.
    let best = |budget: f64| -> Option<(f64, f64)> {
        roc.iter()
            .filter(|&&(_, fnr, _)| fnr <= budget)
            .max_by(|a, b| a.2.partial_cmp(&b.2).unwrap())
            .map(|&(eps, _, s)| (eps, s))
    };

    println!();
    println!("=== VERDICT ===");
    match best(0.0) {
        Some((eps, s)) => println!(
            "strict zero-FN:  EPS={eps:.2} → skips {:.1}% of fact-unchanged edits, ZERO would-serve-stale.",
            s * 100.0
        ),
        None => println!("strict zero-FN:  no EPS>0 achieves zero false-negatives."),
    }
    match best(0.01) {
        Some((eps, s)) => println!(
            "≤1%-FN budget:   EPS={eps:.2} → true-skip {:.1}% of fact-unchanged edits.",
            s * 100.0
        ),
        None => println!("≤1%-FN budget:   unreachable."),
    }
    println!(
        "separation Δ(mean d/D) = {:.4}   (changed {mean_changed:.4} − unchanged {mean_unchanged:.4})",
        mean_changed - mean_unchanged
    );
    println!(
        "baseline SHA gate over these {total} edits: 0 true-skips, 0 false-negatives (invalidates every byte change)."
    );
}
