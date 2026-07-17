//! Restriction-addressed review cache over LLO's REAL fact substrate —
//! thin driver over `leyline_sheaf::restriction_cache` (bead
//! `ley-line-open-054048`, was `f38a86`).
//!
//! The restriction / review-result / oracle / substrate logic now lives in
//! the crate's `restriction_cache` module (parser-independent, ships in
//! production); the tree-sitter extraction that populates it lives in
//! `tests/common` (dev/bench-shared). This file keeps only the FIXTURES
//! (real Rust, before → after over a two-file corpus), the evaluation
//! orchestration, and the assertions — so the git-replay bench (`f3a81e`)
//! and the second review family (`f463aa`) reuse the module without
//! duplicating it.
//!
//! CLAIM UNDER TEST (falsifiable): a cached expensive review result can be
//! safely reused when its cheap fact-specific restriction hash is
//! unchanged, even when the whole-object content hash changed.
//!
//! One review family: the CALL-TARGET review of a function `F`.
//!
//! CONTAINER IDENTITY (the `054048` re-key). The original experiment keyed
//! containers by NAME (`fn:score`). The live daemon's `container_node_id`
//! is POSITIONAL (an AST path), so a line change ABOVE `F` shifts it and
//! the restriction degenerates to whole-file sensitivity (stays SOUND,
//! loses the true skips) — ADR-0031 caveat #1. This driver runs the verdict
//! under [`ContainerKeying::Stable`] — a reflow-invariant node_hash-style
//! identity — and the `stable_identity_survives_insert_above` test proves
//! the fix RED→GREEN against [`ContainerKeying::Positional`].

#[path = "common/mod.rs"]
mod common;

use common::{ast_shape_hash, build_substrate};
use leyline_sheaf::restriction_cache::{
    ContainerKeying, FactSubstrate, FixtureResult, Policy, restriction_for_call_target,
    review_call_targets, stats, whole_object_hash,
};
use std::collections::BTreeSet;
use std::hint::black_box;
use std::time::Instant;

/// The function under review in every fixture, resolved to its container
/// identity per the active keying via [`FactSubstrate::container_for_fn`].
const TARGET_FN: &str = "score";

fn target(sub: &FactSubstrate) -> String {
    sub.container_for_fn(TARGET_FN)
        .expect("fixture must define fn score")
        .to_string()
}

// ---------------------------------------------------------------------------
// Fixtures: real Rust, before → after, over a two-file corpus.
// ---------------------------------------------------------------------------

const MAIN_BEFORE: &str = r#"
use crate::math::compute_weight;
use crate::math::compute_penalty;

pub fn score(value: i64) -> i64 {
    let adjusted = value + 1;
    if value > 10 {
        compute_weight(value)
    } else {
        compute_penalty(adjusted)
    }
}

pub fn audit(value: i64) -> i64 {
    let base = value - 2;
    compute_weight(base * 3)
}
"#;

/// Whitespace-only edit inside `score` (blank lines; no comment — a comment
/// is a named node and would move the AST-shape baseline too).
const MAIN_WHITESPACE: &str = r#"
use crate::math::compute_weight;
use crate::math::compute_penalty;

pub fn score(value: i64) -> i64 {

    let adjusted = value + 1;

    if value > 10 {
        compute_weight(value)
    } else {
        compute_penalty(adjusted)
    }
}

pub fn audit(value: i64) -> i64 {
    let base = value - 2;
    compute_weight(base * 3)
}
"#;

/// Local rename: `adjusted` → `shifted`. The local is never a call target,
/// so no node_refs row moves — the load-bearing fixture.
const MAIN_LOCAL_RENAME: &str = r#"
use crate::math::compute_weight;
use crate::math::compute_penalty;

pub fn score(value: i64) -> i64 {
    let shifted = value + 1;
    if value > 10 {
        compute_weight(value)
    } else {
        compute_penalty(shifted)
    }
}

pub fn audit(value: i64) -> i64 {
    let base = value - 2;
    compute_weight(base * 3)
}
"#;

/// Body arithmetic: `value + 1` → `value * 3`. No call touched — the other
/// load-bearing fixture.
const MAIN_BODY_ARITH: &str = r#"
use crate::math::compute_weight;
use crate::math::compute_penalty;

pub fn score(value: i64) -> i64 {
    let adjusted = value * 3;
    if value > 10 {
        compute_weight(value)
    } else {
        compute_penalty(adjusted)
    }
}

pub fn audit(value: i64) -> i64 {
    let base = value - 2;
    compute_weight(base * 3)
}
"#;

/// Callee swap in the if-branch: `compute_weight(value)` →
/// `compute_penalty(value)`.
const MAIN_CALLEE_SWAP: &str = r#"
use crate::math::compute_weight;
use crate::math::compute_penalty;

pub fn score(value: i64) -> i64 {
    let adjusted = value + 1;
    if value > 10 {
        compute_penalty(value)
    } else {
        compute_penalty(adjusted)
    }
}

pub fn audit(value: i64) -> i64 {
    let base = value - 2;
    compute_weight(base * 3)
}
"#;

/// Import path change, alias unchanged: the call sites are byte-identical,
/// only the `use` path moves.
const MAIN_IMPORT_PATH: &str = r#"
use crate::math_v2::compute_weight;
use crate::math::compute_penalty;

pub fn score(value: i64) -> i64 {
    let adjusted = value + 1;
    if value > 10 {
        compute_weight(value)
    } else {
        compute_penalty(adjusted)
    }
}

pub fn audit(value: i64) -> i64 {
    let base = value - 2;
    compute_weight(base * 3)
}
"#;

/// Edit ELSEWHERE in F's file: `audit`'s arithmetic changes, `score` is
/// byte-identical. Proves the restriction is a genuine projection.
const MAIN_ELSEWHERE: &str = r#"
use crate::math::compute_weight;
use crate::math::compute_penalty;

pub fn score(value: i64) -> i64 {
    let adjusted = value + 1;
    if value > 10 {
        compute_weight(value)
    } else {
        compute_penalty(adjusted)
    }
}

pub fn audit(value: i64) -> i64 {
    let base = value - 2;
    compute_weight(base + 7)
}
"#;

/// INSERT ABOVE `F` — a new import, a comment, and a whole new function
/// added ABOVE `score`; `score` itself is BYTE-IDENTICAL to `MAIN_BEFORE`.
/// The re-key proof fixture (`054048`): positional keying shifts `score`'s
/// container id (2 → 3 `function_item` siblings pushes `score` from
/// `function_item_0` to `function_item_1`) and wrongly invalidates; stable
/// node_hash keying is unchanged and skips soundly.
const MAIN_INSERT_ABOVE: &str = r#"
use crate::math::compute_weight;
use crate::math::compute_penalty;
use crate::math::compute_extra;

// a newly added helper, inserted above score
pub fn helper_unused(x: i64) -> i64 {
    compute_extra(x) + 100
}

pub fn score(value: i64) -> i64 {
    let adjusted = value + 1;
    if value > 10 {
        compute_weight(value)
    } else {
        compute_penalty(adjusted)
    }
}

pub fn audit(value: i64) -> i64 {
    let base = value - 2;
    compute_weight(base * 3)
}
"#;

/// Qualified-call variant of `score` for the qualifier-swap fixture:
/// dual-emit gives a `mathq::qhelper` row plus a bare `qhelper` row carrying
/// `qualifier = Some("mathq")`.
const MAIN_QUAL_BEFORE: &str = r#"
use crate::math::compute_weight;

pub fn score(value: i64) -> i64 {
    let q = mathq::qhelper(value);
    if value > 10 {
        compute_weight(value)
    } else {
        q
    }
}
"#;

const MAIN_QUAL_AFTER: &str = r#"
use crate::math::compute_weight;

pub fn score(value: i64) -> i64 {
    let q = mathr::qhelper(value);
    if value > 10 {
        compute_weight(value)
    } else {
        q
    }
}
"#;

/// Padding defs make the review's unindexed def JOIN measurably wider than
/// the restriction's indexed lookups, mirroring a corpus where `node_defs`
/// holds far more rows than any one function touches.
const PAD_DEFS: usize = 200;

fn math_src(weight_name: &str, with_unrelated: bool) -> String {
    math_src_padded(weight_name, with_unrelated, PAD_DEFS)
}

fn math_src_padded(weight_name: &str, with_unrelated: bool, pad: usize) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "pub fn {weight_name}(x: i64) -> i64 {{ x * 2 }}\n"
    ));
    s.push_str("pub fn compute_penalty(x: i64) -> i64 { x - 1 }\n");
    s.push_str("pub fn compute_extra(x: i64) -> i64 { x + 5 }\n");
    s.push_str("pub fn qhelper(x: i64) -> i64 { x + 9 }\n");
    if with_unrelated {
        s.push_str("pub fn unrelated_helper(x: i64) -> i64 { x }\n");
    }
    for i in 0..pad {
        s.push_str(&format!("pub fn pad_{i}(x: i64) -> i64 {{ x + {i} }}\n"));
    }
    s
}

fn corpus(main: &str, math: String) -> Vec<(String, String)> {
    vec![
        ("main.rs".to_string(), main.to_string()),
        ("math.rs".to_string(), math),
    ]
}

struct Fixture {
    name: &'static str,
    before: Vec<(String, String)>,
    after: Vec<(String, String)>,
}

fn fixtures() -> Vec<Fixture> {
    let base_math = || math_src("compute_weight", false);
    vec![
        Fixture {
            name: "whitespace",
            before: corpus(MAIN_BEFORE, base_math()),
            after: corpus(MAIN_WHITESPACE, base_math()),
        },
        Fixture {
            name: "local-rename",
            before: corpus(MAIN_BEFORE, base_math()),
            after: corpus(MAIN_LOCAL_RENAME, base_math()),
        },
        Fixture {
            name: "body-arith",
            before: corpus(MAIN_BEFORE, base_math()),
            after: corpus(MAIN_BODY_ARITH, base_math()),
        },
        Fixture {
            name: "elsewhere-edit",
            before: corpus(MAIN_BEFORE, base_math()),
            after: corpus(MAIN_ELSEWHERE, base_math()),
        },
        Fixture {
            name: "unrelated-def-add",
            before: corpus(MAIN_BEFORE, base_math()),
            after: corpus(MAIN_BEFORE, math_src("compute_weight", true)),
        },
        Fixture {
            name: "callee-swap",
            before: corpus(MAIN_BEFORE, base_math()),
            after: corpus(MAIN_CALLEE_SWAP, base_math()),
        },
        Fixture {
            name: "import-path",
            before: corpus(MAIN_BEFORE, base_math()),
            after: corpus(MAIN_IMPORT_PATH, base_math()),
        },
        Fixture {
            name: "qualifier-swap",
            before: corpus(MAIN_QUAL_BEFORE, base_math()),
            after: corpus(MAIN_QUAL_AFTER, base_math()),
        },
        Fixture {
            name: "corpus-def-rename",
            before: corpus(MAIN_BEFORE, base_math()),
            after: corpus(MAIN_BEFORE, math_src("compute_weight_v2", false)),
        },
    ]
}

// ---------------------------------------------------------------------------
// Evaluation — thin orchestration over the module + extraction layer.
// ---------------------------------------------------------------------------

/// Did the review RESULT change between before and after (the oracle)?
/// Independent of the restriction; keying only affects the container the
/// refs are grouped by, not the resolved edge CONTENT.
fn review_changed(fx: &Fixture, keying: ContainerKeying) -> bool {
    let before = build_substrate(&fx.before, keying);
    let after = build_substrate(&fx.after, keying);
    let mut scratch = 0u64;
    review_call_targets(&before, &target(&before), &mut scratch)
        != review_call_targets(&after, &target(&after), &mut scratch)
}

/// Is the call-target restriction hash of `score` unchanged across the
/// edit under `keying`? `true` == the Restriction policy would SKIP.
fn restriction_unchanged(fx: &Fixture, keying: ContainerKeying) -> bool {
    let before = build_substrate(&fx.before, keying);
    let after = build_substrate(&fx.after, keying);
    let mut scratch = 0u64;
    restriction_for_call_target(&before, &target(&before), &mut scratch)
        == restriction_for_call_target(&after, &target(&after), &mut scratch)
}

fn evaluate(fx: &Fixture, keying: ContainerKeying) -> FixtureResult {
    let mut skips = std::collections::BTreeMap::new();
    skips.insert(
        Policy::WholeObject,
        whole_object_hash(&fx.before) == whole_object_hash(&fx.after),
    );
    skips.insert(
        Policy::AstShape,
        ast_shape_hash(&fx.before) == ast_shape_hash(&fx.after),
    );
    skips.insert(Policy::Restriction, restriction_unchanged(fx, keying));

    FixtureResult {
        name: fx.name.to_string(),
        review_changed: review_changed(fx, keying),
        skips,
    }
}

fn result<'a>(results: &'a [FixtureResult], name: &str) -> &'a FixtureResult {
    results
        .iter()
        .find(|r| r.name == name)
        .expect("fixture present")
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// The substrate must contain the rows the experiment reasons about — loud
/// failure here beats a silently-green verdict on wrong facts.
#[test]
fn substrate_extraction_sanity() {
    let sub = build_substrate(
        &corpus(MAIN_BEFORE, math_src("compute_weight", false)),
        ContainerKeying::Stable,
    );
    let score = target(&sub);

    let score_refs: BTreeSet<(&str, Option<&str>)> = sub
        .refs
        .iter()
        .filter(|r| r.container.as_deref() == Some(score.as_str()))
        .map(|r| (r.token.as_str(), r.qualifier.as_deref()))
        .collect();
    assert!(score_refs.contains(&("compute_weight", None)));
    assert!(score_refs.contains(&("compute_penalty", None)));

    assert!(
        sub.imports
            .iter()
            .any(|i| i.alias == "compute_weight" && i.path == "crate::math::compute_weight")
    );
    assert!(sub.defs.iter().any(|d| d.token == "compute_weight"));
    assert!(sub.defs.iter().any(|d| d.token == "pad_0"));

    // Qualified dual-emit: the bare row carries the qualifier.
    let qual = build_substrate(
        &corpus(MAIN_QUAL_BEFORE, math_src("compute_weight", false)),
        ContainerKeying::Stable,
    );
    let qscore = target(&qual);
    let qual_refs: BTreeSet<(&str, Option<&str>)> = qual
        .refs
        .iter()
        .filter(|r| r.container.as_deref() == Some(qscore.as_str()))
        .map(|r| (r.token.as_str(), r.qualifier.as_deref()))
        .collect();
    assert!(qual_refs.contains(&("qhelper", Some("mathq"))));
    assert!(qual_refs.contains(&("mathq::qhelper", None)));
}

/// The re-key proof (bead `054048`, ADR-0031 caveat #1). An edit that
/// INSERTS lines ABOVE `score` — a new import, a comment, and a whole new
/// function — leaving `score` byte-identical, must leave the call-target
/// restriction UNCHANGED so the cache soundly skips. With POSITIONAL keying
/// the container id shifts and the restriction wrongly invalidates (RED);
/// with STABLE node_hash keying it is unchanged (GREEN).
#[test]
fn stable_identity_survives_insert_above() {
    let fx = Fixture {
        name: "insert-above",
        before: corpus(MAIN_BEFORE, math_src("compute_weight", false)),
        after: corpus(MAIN_INSERT_ABOVE, math_src("compute_weight", false)),
    };

    // The oracle: score's resolved call graph is genuinely unchanged, so a
    // skip is SOUND. (Keying-independent.)
    assert!(
        !review_changed(&fx, ContainerKeying::Stable),
        "oracle: score's call-target review must be unchanged by an insert ABOVE it"
    );

    // RED — positional container id shifts (function_item_0 → _1), so the
    // restriction hash changes and the cache wastefully recomputes: whole-
    // file sensitivity, the ADR-0031 caveat-#1 degeneration.
    assert!(
        !restriction_unchanged(&fx, ContainerKeying::Positional),
        "positional keying should invalidate on insert-above (the bug this bead fixes)"
    );

    // GREEN — stable node_hash container id is reflow-invariant, so the
    // restriction hash is unchanged and the skip is sound.
    assert!(
        restriction_unchanged(&fx, ContainerKeying::Stable),
        "stable node_hash keying must survive insert-above and skip soundly"
    );
}

/// The verdict table: per-fixture skip decisions vs the oracle, then
/// aggregate rates per policy. Run under STABLE keying (the `054048` re-key)
/// so the reproduced ADR-0031 result already uses the deployment-sound
/// container identity. Run with `--nocapture` to see it.
#[test]
fn restriction_review_verdict() {
    let keying = ContainerKeying::Stable;
    let results: Vec<FixtureResult> = fixtures().iter().map(|f| evaluate(f, keying)).collect();

    eprintln!(
        "\n{:<20} {:>14} {:>12} {:>10} {:>13}",
        "fixture", "review_changed", "WholeObject", "AstShape", "Restriction"
    );
    for r in &results {
        eprintln!(
            "{:<20} {:>14} {:>12} {:>10} {:>13}",
            r.name,
            r.review_changed,
            skip_word(r.skips[&Policy::WholeObject]),
            skip_word(r.skips[&Policy::AstShape]),
            skip_word(r.skips[&Policy::Restriction]),
        );
    }

    let changed = results.iter().filter(|r| r.review_changed).count();
    let unchanged = results.len() - changed;
    eprintln!(
        "\n{:<14} {:>16} {:>15} {:>16}",
        "policy", "false_skip_rate", "true_skip_rate", "recompute_saved"
    );
    for policy in [Policy::WholeObject, Policy::AstShape, Policy::Restriction] {
        let s = stats(&results, policy);
        eprintln!(
            "{:<14} {:>13}/{:<2} {:>12}/{:<2} {:>16}",
            format!("{policy:?}"),
            s.false_skips,
            changed,
            s.sound_skips,
            unchanged,
            s.sound_skips
        );
    }

    // --- Success criteria ---

    // Restriction: zero false skips across every fixture where the review
    // result changed. THE claim; if this trips, the restriction was not a
    // sound superset — report the fixture, don't patch it.
    let restr = stats(&results, Policy::Restriction);
    assert_eq!(
        restr.false_skips, 0,
        "restriction false-skipped a review-changing fixture — not a sound superset"
    );

    // The two load-bearing fixtures: semantic (non-whitespace) edits that
    // the call-target restriction must skip soundly while whole-object CAS
    // recomputes.
    for name in ["local-rename", "body-arith"] {
        let r = result(&results, name);
        assert!(!r.review_changed, "{name}: oracle should be unchanged");
        assert!(
            r.skips[&Policy::Restriction],
            "{name}: restriction should skip"
        );
        assert!(
            !r.skips[&Policy::WholeObject],
            "{name}: whole-object hash must have changed"
        );
    }

    // Projection proof: edits outside F (same file and dep file) leave the
    // restriction unchanged — it is strictly less than the object.
    for name in ["elsewhere-edit", "unrelated-def-add"] {
        let r = result(&results, name);
        assert!(
            r.skips[&Policy::Restriction] && !r.review_changed,
            "{name}: restriction should skip soundly"
        );
    }

    // Superset proof: every review-relevant edit family invalidates,
    // including the dep-side def change F's file never sees.
    for name in [
        "callee-swap",
        "import-path",
        "qualifier-swap",
        "corpus-def-rename",
    ] {
        let r = result(&results, name);
        assert!(r.review_changed, "{name}: oracle should be changed");
        assert!(
            !r.skips[&Policy::Restriction],
            "{name}: restriction must recompute"
        );
    }

    // ADR-0030 reproduction: the identifier-blind shape hash false-skips the
    // callee swap (and, being blind to identifier text, the other call-
    // target-relevant families too).
    let r = result(&results, "callee-swap");
    assert!(
        r.skips[&Policy::AstShape],
        "callee-swap: AST shape is blind to identifier text"
    );
    let shape = stats(&results, Policy::AstShape);
    assert!(shape.false_skips > 0, "AstShape should reproduce ADR-0030");

    // WholeObject: sound but wasteful — every fixture edits some byte in the
    // corpus, so it never skips.
    let whole = stats(&results, Policy::WholeObject);
    assert_eq!(whole.false_skips, 0);
    assert!(
        restr.sound_skips > whole.sound_skips,
        "restriction must save recomputes whole-object cannot"
    );
}

/// restriction_cost < review_cost — measured as substrate rows touched
/// (deterministic, asserted at every scale) and wall time (swept over
/// corpus sizes, because it has a crossover: the restriction pays a constant
/// SHA-256 + buffer cost that a small enough in-memory join undercuts, while
/// the review's row touches grow with the corpus. The wall-time assert is
/// pinned at the largest scale, where the asymptotics dominate the
/// constants; the small-scale inversion is printed, not hidden — it is part
/// of the finding.)
#[test]
fn restriction_is_cheaper_than_review() {
    eprintln!(
        "\n{:>9} {:>10} {:>9} {:>12} {:>12} {:>9} {:>10}",
        "def_rows", "restr_ops", "rev_ops", "restr_time", "rev_time", "op_ratio", "time_ratio"
    );

    let scales: &[(usize, u32)] = &[(200, 2000), (2000, 500), (10000, 100)];
    let mut last: Option<(u64, u64, std::time::Duration, std::time::Duration)> = None;
    for &(pad, iters) in scales {
        let sub = build_substrate(
            &corpus(MAIN_BEFORE, math_src_padded("compute_weight", false, pad)),
            ContainerKeying::Stable,
        );
        let score = target(&sub);

        let mut restriction_ops = 0u64;
        black_box(restriction_for_call_target(
            &sub,
            &score,
            &mut restriction_ops,
        ));
        let mut review_ops = 0u64;
        black_box(review_call_targets(&sub, &score, &mut review_ops));

        let t0 = Instant::now();
        for _ in 0..iters {
            let mut c = 0u64;
            black_box(restriction_for_call_target(&sub, &score, &mut c));
        }
        let restriction_time = t0.elapsed() / iters;
        let t1 = Instant::now();
        for _ in 0..iters {
            let mut c = 0u64;
            black_box(review_call_targets(&sub, &score, &mut c));
        }
        let review_time = t1.elapsed() / iters;

        eprintln!(
            "{:>9} {:>10} {:>9} {:>12} {:>12} {:>8.1}x {:>9.1}x",
            sub.defs.len(),
            restriction_ops,
            review_ops,
            format!("{restriction_time:.1?}"),
            format!("{review_time:.1?}"),
            review_ops as f64 / restriction_ops as f64,
            review_time.as_secs_f64() / restriction_time.as_secs_f64()
        );

        assert!(
            restriction_ops < review_ops,
            "restriction ({restriction_ops} rows) must touch fewer rows than review ({review_ops})"
        );
        last = Some((restriction_ops, review_ops, restriction_time, review_time));
    }

    let (_, _, restriction_time, review_time) = last.expect("at least one scale");
    assert!(
        restriction_time < review_time,
        "at the largest corpus the restriction ({restriction_time:?}) must be cheaper in wall \
         time than the review join ({review_time:?})"
    );
}

fn skip_word(skip: bool) -> &'static str {
    if skip { "SKIP" } else { "recompute" }
}
