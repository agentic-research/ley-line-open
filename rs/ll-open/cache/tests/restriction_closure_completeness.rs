//! Closure completeness: the second soundness hypothesis (bead
//! `ley-line-open-754aa1`).
//!
//! A restriction cache is sound iff (i) the key is injective on closure
//! sections — the property tests in `src/restriction.rs` — and (ii) the
//! closure is COMPLETE: the review is a function of its restriction to the
//! closure rows. A missed dependency makes the closure too small, and the
//! cache goes unsound whatever the key. This file proves (ii) two ways:
//!
//! (a) **Differential replay.** A synthetic edit history, one edit class per
//!     step, run through the ablation driver: every hit is compared with a
//!     from-scratch recompute, and a mismatch (a false skip) fails the test
//!     for every arm that claims soundness. The same driver runs over real
//!     git history in `benches/restriction_git_replay.rs` (`task
//!     cache:ablation`), which CI's depth-1 checkout cannot host as a test.
//! (b) **Closure mutation.** Drop ONE row from the closure, edit exactly the
//!     substrate row it came from, and recompute: the mutated key must stay
//!     put while the review moves — a detected false skip. A row whose edit
//!     leaves the review unchanged is an over-approximation, reported as
//!     such; a row whose edit also moves another closure row is covered
//!     redundantly ("masked").

#[path = "common/mod.rs"]
mod common;

use common::ablation::{Arm, ArmStats, KeyArm, build_step, print_table, run_step};
use common::test_parser_id;
use leyline_cache::restriction::{
    CALL_TARGET_SCHEME, DefRow, FactSubstrate, ImportRow, Provenance, RefRow, RowKind,
    call_target_closure, restriction_key, review_call_targets,
};
use leyline_cache::strategy::{NoCache, RestrictionKey, WholeInput};
use std::collections::BTreeMap;

// ───────────────────────── (a) differential replay ─────────────────────────

fn corpus(files: &[(&str, &str)]) -> Vec<(String, String)> {
    files
        .iter()
        .map(|(p, s)| (p.to_string(), s.to_string()))
        .collect()
}

const MAIN: &str = "use crate::math::weight;\nuse crate::math::penalty;\n\
pub fn score(v: i64) -> i64 {\n    let a = weight(v);\n    a + 1\n}\n\
pub fn audit(v: i64) -> i64 {\n    penalty(v)\n}\n";

const MATH: &str = "pub fn weight(x: i64) -> i64 { x * 2 }\n\
pub fn penalty(x: i64) -> i64 { x - 1 }\n";

const UTIL: &str = "pub fn noop(x: i64) -> i64 { x }\n";

/// One version per edit class, each applied on top of the previous one, and
/// a final revert so a key seen earlier can hit again.
fn history() -> Vec<(&'static str, Vec<(String, String)>)> {
    let v0 = corpus(&[("main.rs", MAIN), ("math.rs", MATH), ("util.rs", UTIL)]);
    let ws = MAIN.replace("a + 1", "a  +  1");
    let arith = ws.replace("a  +  1", "a * 3");
    let local = arith
        .replace("let a = weight(v);", "let b = weight(v);")
        .replace("a * 3", "b * 3");
    let swap = local.replace("penalty(v)", "weight(v)");
    let renamed_math = MATH.replace("fn weight", "fn weight_v2");
    let util_dup = format!("{UTIL}pub fn penalty(x: i64) -> i64 {{ x }}\n");
    let import_path = swap.replace("crate::math::weight", "crate::util::weight");
    let unrelated = format!("{renamed_math}pub fn helper(x: i64) -> i64 {{ x }}\n");
    vec![
        ("base", v0.clone()),
        (
            "whitespace",
            corpus(&[("main.rs", &ws), ("math.rs", MATH), ("util.rs", UTIL)]),
        ),
        (
            "body-arith",
            corpus(&[("main.rs", &arith), ("math.rs", MATH), ("util.rs", UTIL)]),
        ),
        (
            "local-rename",
            corpus(&[("main.rs", &local), ("math.rs", MATH), ("util.rs", UTIL)]),
        ),
        (
            "callee-swap",
            corpus(&[("main.rs", &swap), ("math.rs", MATH), ("util.rs", UTIL)]),
        ),
        (
            "cross-file-def-rename",
            corpus(&[
                ("main.rs", &swap),
                ("math.rs", &renamed_math),
                ("util.rs", UTIL),
            ]),
        ),
        (
            "new-candidate-def",
            corpus(&[
                ("main.rs", &swap),
                ("math.rs", &renamed_math),
                ("util.rs", &util_dup),
            ]),
        ),
        (
            "import-path",
            corpus(&[
                ("main.rs", &import_path),
                ("math.rs", &renamed_math),
                ("util.rs", &util_dup),
            ]),
        ),
        (
            "unrelated-fn",
            corpus(&[
                ("main.rs", &import_path),
                ("math.rs", &unrelated),
                ("util.rs", &util_dup),
            ]),
        ),
        ("revert", v0),
    ]
}

fn replay() -> Vec<ArmStats> {
    let h = history();
    let mut arms: Vec<Box<dyn Arm>> = vec![
        Box::new(KeyArm::new(NoCache)),
        Box::new(KeyArm::new(WholeInput)),
        Box::new(KeyArm::new(RestrictionKey)),
    ];
    let mut stats = Vec::new();
    for w in h.windows(2) {
        let (step, _) = build_step(w[1].0, &w[0].1, &w[1].1);
        run_step(&step, &mut arms, &mut stats);
    }
    print_table(&stats);
    stats
}

#[test]
fn differential_replay_has_no_false_skip() {
    let stats = replay();
    for s in &stats {
        assert_eq!(
            s.false_skips + s.warm_false_skips,
            0,
            "{} served a stale review: {:?}",
            s.name,
            s.false_skip_log
        );
    }
    let by_name: BTreeMap<&str, &ArmStats> = stats.iter().map(|s| (s.name.as_str(), s)).collect();
    let (none, whole, restr) = (
        by_name["no-cache"],
        by_name["whole-input"],
        by_name["restriction"],
    );

    // Every history step edits some file, so the whole-input key never
    // repeats except on the revert, and no-cache never hits.
    assert_eq!(none.hits, 0);
    assert!(
        restr.hits > whole.hits,
        "the restriction must keep values the whole-input key discards"
    );
    // The history changes the reviewed value of `score` and `audit` at least
    // once each, so the oracle is not vacuous.
    assert!(
        restr.value_changed >= 4,
        "value changed {} times",
        restr.value_changed
    );
    // The restriction misses only where a closure row moved: its false misses
    // are the over-approximation, and the whole-input arm has at least as many.
    assert!(restr.false_misses <= whole.false_misses);
}

// ───────────────────────── (b) closure mutation ────────────────────────────

fn d(token: &str, source: &str, kind: &str) -> DefRow {
    DefRow {
        token: token.into(),
        source_id: source.into(),
        kind: kind.into(),
    }
}

fn r(token: &str, qualifier: Option<&str>, container: &str) -> RefRow {
    RefRow {
        token: token.into(),
        qualifier: qualifier.map(Into::into),
        container: Some(container.into()),
        source_id: "main.rs".into(),
    }
}

fn imp(alias: &str, path: &str) -> ImportRow {
    ImportRow {
        alias: alias.into(),
        path: path.into(),
        source_id: "main.rs".into(),
    }
}

/// `F` calls `weight` (imported twice, so only the first import resolves),
/// `penalty` (resolving to two defs), `m::scale` (qualified through an
/// import of `m`) and `Widget::new` (qualified by a name no import binds).
fn base_rows() -> (Vec<DefRow>, Vec<RefRow>, Vec<ImportRow>) {
    (
        vec![
            d("penalty", "math.rs", "function"),
            d("penalty", "util.rs", "function"),
            d("scale", "m.rs", "function"),
            d("weight", "math.rs", "function"),
            d("new", "widget.rs", "method"),
        ],
        vec![
            r("penalty", None, "F"),
            r("scale", Some("m"), "F"),
            r("weight", None, "F"),
            r("new", Some("Widget"), "F"),
        ],
        vec![
            imp("m", "crate::m"),
            imp("weight", "crate::math::weight"),
            imp("weight", "crate::util::weight"),
        ],
    )
}

fn build(rows: (Vec<DefRow>, Vec<RefRow>, Vec<ImportRow>)) -> FactSubstrate {
    FactSubstrate::from_rows(rows.0, rows.1, rows.2, BTreeMap::new(), test_parser_id())
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Outcome {
    /// Mutated key unchanged, review changed: the dropped row is necessary.
    FalseSkipDetected,
    /// The edit to that row leaves the review unchanged: over-approximation.
    OverApproximation,
    /// The review changed but another closure row moved too: the dependency
    /// is also carried by another row.
    Masked,
}

/// Edit the substrate row a closure row came from. Returns the after
/// substrate and the after container.
fn perturb(p: Provenance) -> (FactSubstrate, &'static str) {
    let (mut defs, mut refs, mut imports) = base_rows();
    let mut container = "F";
    match p {
        Provenance::Query => {
            for r in &mut refs {
                r.container = Some("F~".into());
            }
            container = "F~";
        }
        // A fresh qualifier that names no import.
        Provenance::Ref(i) => {
            refs[i].qualifier = Some(format!("{}~", refs[i].qualifier.as_deref().unwrap_or("")));
        }
        Provenance::Import(i) => imports[i].path.push('~'),
        Provenance::Def(i) => defs[i].kind.push('~'),
        Provenance::SigType(_) => unreachable!("not a call-target row"),
    }
    (build((defs, refs, imports)), container)
}

fn mutation_outcomes() -> Vec<(RowKind, Provenance, Outcome)> {
    let before = build(base_rows());
    let closure_b = call_target_closure(&before, "F", &mut 0);
    let pid = test_parser_id();
    let full_b = restriction_key(CALL_TARGET_SCHEME, pid, &closure_b);
    let value_b = review_call_targets(&before, "F", &mut 0);

    let mut out = Vec::new();
    for row in &closure_b {
        let p = row.provenance;
        let (after, target) = perturb(p);
        let closure_a = call_target_closure(&after, target, &mut 0);
        let value_a = review_call_targets(&after, target, &mut 0);
        let changed = value_a != value_b;
        // The unmutated key is sound on every edit.
        if changed {
            assert_ne!(
                full_b,
                restriction_key(CALL_TARGET_SCHEME, pid, &closure_a),
                "the full closure must see the edit to {p:?}"
            );
        }
        let drop = |c: &[leyline_cache::restriction::Row]| -> Vec<_> {
            c.iter().filter(|r| r.provenance != p).cloned().collect()
        };
        let mutated_b = restriction_key(CALL_TARGET_SCHEME, pid, &drop(&closure_b));
        let mutated_a = restriction_key(CALL_TARGET_SCHEME, pid, &drop(&closure_a));
        let outcome = match (changed, mutated_b == mutated_a) {
            (false, _) => Outcome::OverApproximation,
            (true, true) => Outcome::FalseSkipDetected,
            (true, false) => Outcome::Masked,
        };
        out.push((row.kind, p, outcome));
    }
    out
}

#[test]
fn dropping_a_closure_row_makes_the_cache_unsound() {
    let outcomes = mutation_outcomes();
    println!("closure mutation (call-target, {} rows):", outcomes.len());
    for (k, p, o) in &outcomes {
        println!("  {k:?} {p:?}: {o:?}");
    }
    let expected = vec![
        // The container id names the query; the review value never reads it,
        // so two functions with equal closures could share a value.
        (
            RowKind::Container,
            Provenance::Query,
            Outcome::OverApproximation,
        ),
        (RowKind::Ref, Provenance::Ref(0), Outcome::FalseSkipDetected),
        // `m::scale`'s qualifier also selects the `m` import row, so editing
        // it moves that row too: the qualifier dependency is carried twice.
        // `Widget::new`'s qualifier binds no import, and there dropping the
        // ref row alone is detected.
        (RowKind::Ref, Provenance::Ref(1), Outcome::Masked),
        (RowKind::Ref, Provenance::Ref(2), Outcome::FalseSkipDetected),
        (RowKind::Ref, Provenance::Ref(3), Outcome::FalseSkipDetected),
        (
            RowKind::Import,
            Provenance::Import(0),
            Outcome::FalseSkipDetected,
        ),
        (
            RowKind::Import,
            Provenance::Import(1),
            Outcome::FalseSkipDetected,
        ),
        // The second `weight` import: the review resolves through the first
        // matching import only.
        (
            RowKind::Import,
            Provenance::Import(2),
            Outcome::OverApproximation,
        ),
        (RowKind::Def, Provenance::Def(4), Outcome::FalseSkipDetected),
        (RowKind::Def, Provenance::Def(0), Outcome::FalseSkipDetected),
        (RowKind::Def, Provenance::Def(1), Outcome::FalseSkipDetected),
        (RowKind::Def, Provenance::Def(2), Outcome::FalseSkipDetected),
        (RowKind::Def, Provenance::Def(3), Outcome::FalseSkipDetected),
    ];
    assert_eq!(outcomes, expected);
}
