//! Restriction-addressed review cache — SECOND review family (bead
//! `ley-line-open-0567e8`, ADR-0031 next-move `f463aa`): the PUBLIC-API
//! review of a definition, keyed on `node_defs` rather than `node_refs`.
//!
//! The call-target family (`restriction_review_real.rs`) proves
//! restriction-addressing for a review that reads a function's CALL SITES
//! (the `node_refs` contained in `F`). This file proves the pattern
//! GENERALIZES to a review that reads a definition's PUBLIC SURFACE — its
//! `node_def`, its visibility, and the SIGNATURE types it names — a
//! different projection of the same substrate, with its OWN restriction key
//! and its OWN false-skip measurement.
//!
//! CLAIM UNDER TEST (falsifiable): a cached expensive public-API review can
//! be safely reused when its cheap signature-scoped restriction hash is
//! unchanged, even when the whole-object content hash changed — AND the key
//! win, unaffected by the function's BODY: a body-only edit (rename a
//! private helper the public fn calls) leaves the public-API restriction
//! unchanged (SKIP) though the call-target restriction of the same function
//! would move. That contrast is the proof the per-family restriction is
//! scoped to what its review actually reads, not to the whole object.
//!
//! Three artifacts, kept structurally separate (see
//! `leyline_sheaf::restriction_cache`):
//!   1. RESTRICTION (cheap, `restriction_for_public_api`): a hash over a
//!      sound superset of the review's input — the def's token/qualifier/
//!      kind, its visibility, the reflow- AND body-invariant SIGNATURE
//!      identity (declaration subtree minus body), the signature TYPE
//!      tokens, and — cross-object — the `node_defs` those type tokens
//!      resolve to across the corpus.
//!   2. REVIEW RESULT (expensive, distinct, `review_public_api`): the
//!      resolved public surface — an unindexed cross-corpus JOIN of each
//!      signature type against all `node_defs`, gated on visibility.
//!   3. ORACLE: run the review on both versions and compare — never consult
//!      the restriction.

#[path = "common/mod.rs"]
mod common;

use common::{ast_shape_hash, build_api_defs, build_substrate};
use leyline_sheaf::restriction_cache::{
    ApiDefRow, ContainerKeying, FactSubstrate, FixtureResult, Policy, restriction_for_public_api,
    review_public_api, stats, whole_object_hash,
};
use std::hint::black_box;
use std::time::Instant;

/// The public function under review in every fixture.
const TARGET_FN: &str = "score";

fn api_target(corpus: &[(String, String)]) -> ApiDefRow {
    build_api_defs(corpus)
        .remove(TARGET_FN)
        .expect("fixture must define fn score")
}

fn substrate(corpus: &[(String, String)]) -> FactSubstrate {
    // Keying is irrelevant to the public-API family (it reads node_defs,
    // not per-container node_refs); Stable matches the call-target driver.
    build_substrate(corpus, ContainerKeying::Stable)
}

// ---------------------------------------------------------------------------
// Fixtures: real Rust, before → after, over a two-file corpus. `score`'s
// signature names a corpus-defined type (`Widget`, from math.rs) so the
// restriction genuinely spans multiple objects, and calls a private helper
// in its BODY so the body-only true-skip is exercised.
// ---------------------------------------------------------------------------

const MAIN_BEFORE: &str = r#"
use crate::math::Widget;
use crate::math::helper;

pub fn score(value: i64, w: Widget) -> i64 {
    let base = helper(value);
    base + w.id
}
"#;

/// Whitespace-only edit inside `score`'s body (blank lines). Signature
/// byte-identical modulo reflow.
const MAIN_WHITESPACE: &str = r#"
use crate::math::Widget;
use crate::math::helper;

pub fn score(value: i64, w: Widget) -> i64 {

    let base = helper(value);

    base + w.id
}
"#;

/// BODY change, signature UNCHANGED — the key win. `base + w.id` becomes
/// `base * w.id + 7`. The public surface is unaffected.
const MAIN_BODY_CHANGE: &str = r#"
use crate::math::Widget;
use crate::math::helper;

pub fn score(value: i64, w: Widget) -> i64 {
    let base = helper(value);
    base * w.id + 7
}
"#;

/// Signature change: return type `i64` → `i32`. Text-only (both parse to
/// `primitive_type`), so the identifier-blind AST-shape hash is BLIND to it
/// — the ADR-0030 reproduction on a signature identifier.
const MAIN_SIG_RETURN: &str = r#"
use crate::math::Widget;
use crate::math::helper;

pub fn score(value: i64, w: Widget) -> i32 {
    let base = helper(value);
    base + w.id
}
"#;

/// Visibility change: `pub fn score` → `fn score`. The symbol leaves the
/// public surface entirely.
const MAIN_VIS_PRIVATE: &str = r#"
use crate::math::Widget;
use crate::math::helper;

fn score(value: i64, w: Widget) -> i64 {
    let base = helper(value);
    base + w.id
}
"#;

/// Body-only: the private helper `score` CALLS is renamed `helper` →
/// `helper2` (call site here + def in math.rs). The public-API review never
/// reads the body, so its surface is unchanged — SKIP. (The call-target
/// review of `score` WOULD invalidate on this same edit; that is the
/// family distinction.)
const MAIN_HELPER_RENAME: &str = r#"
use crate::math::Widget;
use crate::math::helper2;

pub fn score(value: i64, w: Widget) -> i64 {
    let base = helper2(value);
    base + w.id
}
"#;

/// Padding defs make the review's unindexed def JOIN measurably wider than
/// the restriction's indexed point lookups.
const PAD_DEFS: usize = 200;

fn math_src(widget_name: &str, helper_name: &str) -> String {
    math_src_padded(widget_name, helper_name, PAD_DEFS)
}

fn math_src_padded(widget_name: &str, helper_name: &str, pad: usize) -> String {
    let mut s = String::new();
    s.push_str(&format!("pub struct {widget_name} {{ pub id: i64 }}\n"));
    s.push_str(&format!(
        "pub fn {helper_name}(x: i64) -> i64 {{ x + 1 }}\n"
    ));
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
    let base_math = || math_src("Widget", "helper");
    vec![
        Fixture {
            name: "whitespace",
            before: corpus(MAIN_BEFORE, base_math()),
            after: corpus(MAIN_WHITESPACE, base_math()),
        },
        Fixture {
            name: "body-change",
            before: corpus(MAIN_BEFORE, base_math()),
            after: corpus(MAIN_BODY_CHANGE, base_math()),
        },
        Fixture {
            name: "helper-rename",
            before: corpus(MAIN_BEFORE, base_math()),
            after: corpus(MAIN_HELPER_RENAME, math_src("Widget", "helper2")),
        },
        Fixture {
            name: "sig-return-type",
            before: corpus(MAIN_BEFORE, base_math()),
            after: corpus(MAIN_SIG_RETURN, base_math()),
        },
        Fixture {
            name: "visibility",
            before: corpus(MAIN_BEFORE, base_math()),
            after: corpus(MAIN_VIS_PRIVATE, base_math()),
        },
        // Cross-file signature-type def rename: `score`'s file is
        // BYTE-IDENTICAL; only the `Widget` struct def in math.rs is renamed
        // to `Panel`. The public surface changes (the named type no longer
        // resolves) and the restriction must invalidate by reaching across
        // the corpus boundary — the multi-object span, the sheaf-shaped
        // property, on the node_defs family.
        Fixture {
            name: "sig-typedef-rename",
            before: corpus(MAIN_BEFORE, base_math()),
            after: corpus(MAIN_BEFORE, math_src("Panel", "helper")),
        },
    ]
}

// ---------------------------------------------------------------------------
// Evaluation.
// ---------------------------------------------------------------------------

/// Did the public-API review RESULT change between before and after (the
/// oracle)? Independent of the restriction.
fn review_changed(fx: &Fixture) -> bool {
    let (sb, ab) = (substrate(&fx.before), substrate(&fx.after));
    let (ta, tb) = (api_target(&fx.before), api_target(&fx.after));
    let mut scratch = 0u64;
    review_public_api(&sb, &ta, &mut scratch) != review_public_api(&ab, &tb, &mut scratch)
}

/// Is `score`'s public-API restriction hash unchanged across the edit?
/// `true` == the Restriction policy would SKIP.
fn restriction_unchanged(fx: &Fixture) -> bool {
    let (sb, ab) = (substrate(&fx.before), substrate(&fx.after));
    let (ta, tb) = (api_target(&fx.before), api_target(&fx.after));
    let mut scratch = 0u64;
    restriction_for_public_api(&sb, &ta, &mut scratch)
        == restriction_for_public_api(&ab, &tb, &mut scratch)
}

fn evaluate(fx: &Fixture) -> FixtureResult {
    let mut skips = std::collections::BTreeMap::new();
    skips.insert(
        Policy::WholeObject,
        whole_object_hash(&fx.before) == whole_object_hash(&fx.after),
    );
    skips.insert(
        Policy::AstShape,
        ast_shape_hash(&fx.before) == ast_shape_hash(&fx.after),
    );
    skips.insert(Policy::Restriction, restriction_unchanged(fx));

    FixtureResult {
        name: fx.name.to_string(),
        review_changed: review_changed(fx),
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

/// The substrate + ApiDefRow must contain the facts the experiment reasons
/// about — loud failure here beats a silently-green verdict on wrong facts.
#[test]
fn public_api_extraction_sanity() {
    let c = corpus(MAIN_BEFORE, math_src("Widget", "helper"));
    let api = api_target(&c);
    assert_eq!(api.token, "score");
    assert!(api.is_public, "score is declared pub");
    // Signature names the corpus type `Widget` and the primitive `i64`;
    // the private helper's name is a BODY call, so it must NOT appear.
    assert!(
        api.sig_type_tokens.contains(&"Widget".to_string()),
        "signature type Widget must be captured, got {:?}",
        api.sig_type_tokens
    );
    assert!(api.sig_type_tokens.contains(&"i64".to_string()));
    assert!(
        !api.sig_type_tokens.iter().any(|t| t == "helper"),
        "body call target must not leak into signature types"
    );

    // The corpus really defines the type the signature names.
    let sub = substrate(&c);
    assert!(sub.defs.iter().any(|d| d.token == "Widget"));
    assert!(sub.defs.iter().any(|d| d.token == "pad_0"));

    // A private function is not on the public surface.
    let priv_api = api_target(&corpus(MAIN_VIS_PRIVATE, math_src("Widget", "helper")));
    assert!(!priv_api.is_public);
}

/// The family-distinction proof: the SAME edit (rename the private helper
/// `score` calls) is a body-only change. The public-API restriction is
/// UNCHANGED (skip), because the review never reads the body — while the
/// whole-object hash changed. This is what makes the public-API family a
/// genuinely different projection, not a relabeled call-target review.
#[test]
fn body_only_edit_leaves_public_api_restriction_unchanged() {
    let fx = Fixture {
        name: "helper-rename",
        before: corpus(MAIN_BEFORE, math_src("Widget", "helper")),
        after: corpus(MAIN_HELPER_RENAME, math_src("Widget", "helper2")),
    };
    assert!(
        !review_changed(&fx),
        "oracle: renaming a body-called private helper must not change score's public surface"
    );
    assert!(
        restriction_unchanged(&fx),
        "public-API restriction must skip a body-only edit"
    );
    assert_ne!(
        whole_object_hash(&fx.before),
        whole_object_hash(&fx.after),
        "the edit really changed bytes — the skip is a true win over whole-object CAS"
    );
}

/// The verdict table: per-fixture skip decisions vs the oracle, then
/// aggregate rates per policy, with the public-API family's OWN false-skip
/// measurement. Run with `--nocapture` to see it.
#[test]
fn public_api_review_verdict() {
    let results: Vec<FixtureResult> = fixtures().iter().map(evaluate).collect();

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

    // THE claim: zero false skips across every review-changing fixture. If
    // this trips, the restriction was not a sound superset — report the
    // fixture, don't patch it.
    let restr = stats(&results, Policy::Restriction);
    assert_eq!(
        restr.false_skips, 0,
        "public-API restriction false-skipped a review-changing fixture — not a sound superset"
    );

    // The key win: body change + whitespace + body-called-helper rename are
    // review-preserving and the restriction skips them while whole-object
    // CAS recomputes.
    for name in ["whitespace", "body-change", "helper-rename"] {
        let r = result(&results, name);
        assert!(!r.review_changed, "{name}: oracle should be unchanged");
        assert!(
            r.skips[&Policy::Restriction],
            "{name}: public-API restriction should skip"
        );
        assert!(
            !r.skips[&Policy::WholeObject],
            "{name}: whole-object hash must have changed"
        );
    }

    // Superset proof: every public-surface-relevant edit invalidates —
    // including the cross-file type-def rename `score`'s file never sees.
    for name in ["sig-return-type", "visibility", "sig-typedef-rename"] {
        let r = result(&results, name);
        assert!(r.review_changed, "{name}: oracle should be changed");
        assert!(
            !r.skips[&Policy::Restriction],
            "{name}: public-API restriction must recompute"
        );
    }

    // ADR-0030 reproduction on a SIGNATURE identifier: the identifier-blind
    // shape hash false-skips the `i64` → `i32` return-type change (both are
    // `primitive_type`, so the kind sequence is identical).
    let r = result(&results, "sig-return-type");
    assert!(
        r.skips[&Policy::AstShape],
        "sig-return-type: AST shape is blind to the i64/i32 identifier text"
    );
    let shape = stats(&results, Policy::AstShape);
    assert!(
        shape.false_skips > 0,
        "AstShape should reproduce ADR-0030 on a signature identifier"
    );

    // WholeObject: sound but wasteful — every fixture edits some byte, so it
    // never skips; the restriction saves the recomputes it cannot.
    let whole = stats(&results, Policy::WholeObject);
    assert_eq!(whole.false_skips, 0);
    assert!(
        restr.sound_skips > whole.sound_skips,
        "restriction must save recomputes whole-object cannot (true_skip > WholeObject)"
    );
}

/// restriction_cost < review_cost — measured as substrate rows touched
/// (deterministic, asserted at every scale) and wall time (swept over corpus
/// sizes; the wall-time assert is pinned at the largest scale where the
/// asymptotics dominate the SHA-256 constant, mirroring the call-target
/// driver's crossover finding).
#[test]
fn public_api_restriction_is_cheaper_than_review() {
    eprintln!(
        "\n{:>9} {:>10} {:>9} {:>12} {:>12} {:>9} {:>10}",
        "def_rows", "restr_ops", "rev_ops", "restr_time", "rev_time", "op_ratio", "time_ratio"
    );

    let scales: &[(usize, u32)] = &[(200, 2000), (2000, 500), (10000, 100)];
    let mut last: Option<(std::time::Duration, std::time::Duration)> = None;
    for &(pad, iters) in scales {
        let c = corpus(MAIN_BEFORE, math_src_padded("Widget", "helper", pad));
        let sub = substrate(&c);
        let api = api_target(&c);

        let mut restriction_ops = 0u64;
        black_box(restriction_for_public_api(&sub, &api, &mut restriction_ops));
        let mut review_ops = 0u64;
        black_box(review_public_api(&sub, &api, &mut review_ops));

        let t0 = Instant::now();
        for _ in 0..iters {
            let mut c = 0u64;
            black_box(restriction_for_public_api(&sub, &api, &mut c));
        }
        let restriction_time = t0.elapsed() / iters;
        let t1 = Instant::now();
        for _ in 0..iters {
            let mut c = 0u64;
            black_box(review_public_api(&sub, &api, &mut c));
        }
        let review_time = t1.elapsed() / iters;

        eprintln!(
            "{:>9} {:>10} {:>9} {:>12} {:>12} {:>8.1}x {:>9.1}x",
            sub.defs.len(),
            restriction_ops,
            review_ops,
            format!("{restriction_time:.1?}"),
            format!("{review_time:.1?}"),
            review_ops as f64 / restriction_ops.max(1) as f64,
            review_time.as_secs_f64() / restriction_time.as_secs_f64()
        );

        assert!(
            restriction_ops < review_ops,
            "restriction ({restriction_ops} rows) must touch fewer rows than review ({review_ops})"
        );
        last = Some((restriction_time, review_time));
    }

    let (restriction_time, review_time) = last.expect("at least one scale");
    assert!(
        restriction_time < review_time,
        "at the largest corpus the restriction ({restriction_time:?}) must be cheaper in wall \
         time than the review join ({review_time:?})"
    );
}

fn skip_word(skip: bool) -> &'static str {
    if skip { "SKIP" } else { "recompute" }
}
