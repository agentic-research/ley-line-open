//! Toy falsification harness for restriction-addressed review caching.
//!
//! This is intentionally small and deterministic: it asks whether a
//! fact-specific AST restriction hash catches review-fact changes that a
//! structure-only AST shape hash would miss.

use leyline_sheaf::restriction_review::{CachePolicy, ReviewFactKind, compare_review_cache};

const BEFORE: &str = r#"
use crate::math::compute_weight;

pub fn score(value: i64) -> i64 {
    if value > 10 {
        compute_weight(value)
    } else {
        0
    }
}
"#;

const AFTER_WHITESPACE: &str = r#"
use crate::math::compute_weight;

pub fn score(value: i64) -> i64 {

    if value > 10 {
        compute_weight(value)
    } else {
        0
    }
}
"#;

const AFTER_CALLEE_SWAP: &str = r#"
use crate::math::compute_weight;

pub fn score(value: i64) -> i64 {
    if value > 10 {
        compute_penalty(value)
    } else {
        0
    }
}
"#;

const AFTER_UNWRAP: &str = r#"
use crate::math::compute_weight;

pub fn score(value: i64) -> i64 {
    if value > 10 {
        compute_weight(value).unwrap()
    } else {
        0
    }
}
"#;

const AFTER_PUBLIC_SIGNATURE: &str = r#"
use crate::math::compute_weight;

pub fn score(value: i64, bias: i64) -> i64 {
    if value > 10 {
        compute_weight(value)
    } else {
        bias
    }
}
"#;

const AFTER_IMPORT_SURFACE: &str = r#"
use crate::math::compute_penalty;

pub fn score(value: i64) -> i64 {
    if value > 10 {
        compute_weight(value)
    } else {
        0
    }
}
"#;

const AFTER_BRANCH_CONDITION: &str = r#"
use crate::math::compute_weight;

pub fn score(value: i64) -> i64 {
    if value >= 10 {
        compute_weight(value)
    } else {
        0
    }
}
"#;

const LOCAL_RENAME_BEFORE: &str = r#"
use crate::math::compute_weight;

pub fn score(value: i64) -> i64 {
    let adjusted = value + 1;
    if value > 10 {
        compute_weight(value)
    } else {
        adjusted
    }
}
"#;

const LOCAL_RENAME_AFTER: &str = r#"
use crate::math::compute_weight;

pub fn score(value: i64) -> i64 {
    let local_value = value + 1;
    if value > 10 {
        compute_weight(value)
    } else {
        local_value
    }
}
"#;

fn outcome(src: &str, policy: CachePolicy) -> bool {
    compare_review_cache(BEFORE, src)
        .outcome(policy)
        .expect("policy outcome present")
        .false_skip
}

fn skips(src: &str, policy: CachePolicy) -> bool {
    compare_review_cache(BEFORE, src)
        .outcome(policy)
        .expect("policy outcome present")
        .would_skip
}

fn per_fact_skips(src: &str, fact: ReviewFactKind) -> bool {
    compare_review_cache(BEFORE, src)
        .fact_outcome(fact, CachePolicy::ReviewRestriction)
        .expect("per-fact policy outcome present")
        .would_skip
}

#[test]
fn cosmetic_edit_is_safe_skip_for_review_restrictions() {
    let report = compare_review_cache(BEFORE, AFTER_WHITESPACE);
    eprintln!("{}", report.as_table_row("whitespace"));

    assert!(report.changed_facts.is_empty());
    assert!(!skips(AFTER_WHITESPACE, CachePolicy::WholeObject));
    assert!(skips(AFTER_WHITESPACE, CachePolicy::AstShape));
    assert!(skips(AFTER_WHITESPACE, CachePolicy::ReviewRestriction));
    assert!(!outcome(AFTER_WHITESPACE, CachePolicy::ReviewRestriction));
}

#[test]
fn structure_only_hash_falsely_skips_identifier_sensitive_review_changes() {
    for (label, src, expected_fact) in [
        (
            "callee-swap",
            AFTER_CALLEE_SWAP,
            ReviewFactKind::CallTargetChanged,
        ),
        (
            "unwrap-introduced",
            AFTER_UNWRAP,
            ReviewFactKind::UsesUnwrap,
        ),
        (
            "branch-condition",
            AFTER_BRANCH_CONDITION,
            ReviewFactKind::BranchConditionChanged,
        ),
    ] {
        let report = compare_review_cache(BEFORE, src);
        eprintln!("{}", report.as_table_row(label));

        assert!(
            report.changed_facts.contains(&expected_fact),
            "{label}: expected changed fact {expected_fact:?}; got {:?}",
            report.changed_facts
        );
        assert!(
            outcome(src, CachePolicy::AstShape),
            "{label}: structure-only AST policy should be falsified"
        );
        assert!(
            !outcome(src, CachePolicy::ReviewRestriction),
            "{label}: review restriction policy must not false-skip"
        );
    }
}

#[test]
fn review_restrictions_catch_public_and_import_surface_changes() {
    for (label, src, expected_fact) in [
        (
            "public-signature",
            AFTER_PUBLIC_SIGNATURE,
            ReviewFactKind::PublicSignatureChanged,
        ),
        (
            "import-surface",
            AFTER_IMPORT_SURFACE,
            ReviewFactKind::ImportSurfaceChanged,
        ),
    ] {
        let report = compare_review_cache(BEFORE, src);
        eprintln!("{}", report.as_table_row(label));

        assert!(
            report.changed_facts.contains(&expected_fact),
            "{label}: expected changed fact {expected_fact:?}; got {:?}",
            report.changed_facts
        );
        assert!(!skips(src, CachePolicy::ReviewRestriction));
        assert!(!outcome(src, CachePolicy::ReviewRestriction));
    }
}

#[test]
fn non_whitespace_fact_irrelevant_edit_is_a_real_safe_skip() {
    let report = compare_review_cache(LOCAL_RENAME_BEFORE, LOCAL_RENAME_AFTER);
    eprintln!("{}", report.as_table_row("local-rename"));

    assert!(report.changed_facts.is_empty());
    assert!(!report.outcome(CachePolicy::WholeObject).unwrap().would_skip);
    assert!(report.outcome(CachePolicy::AstShape).unwrap().would_skip);
    assert!(
        report
            .outcome(CachePolicy::ReviewRestriction)
            .unwrap()
            .would_skip
    );
    assert!(
        report
            .fact_outcome(
                ReviewFactKind::CallTargetChanged,
                CachePolicy::ReviewRestriction
            )
            .unwrap()
            .saved_recompute
    );
}

#[test]
fn review_restrictions_are_per_fact_not_one_combined_key() {
    let report = compare_review_cache(BEFORE, AFTER_CALLEE_SWAP);
    eprintln!("{}", report.as_table_row("callee-swap-per"));

    assert!(!per_fact_skips(
        AFTER_CALLEE_SWAP,
        ReviewFactKind::CallTargetChanged
    ));
    assert!(per_fact_skips(
        AFTER_CALLEE_SWAP,
        ReviewFactKind::PublicSignatureChanged
    ));
    assert!(per_fact_skips(
        AFTER_CALLEE_SWAP,
        ReviewFactKind::ImportSurfaceChanged
    ));
    assert!(per_fact_skips(
        AFTER_CALLEE_SWAP,
        ReviewFactKind::BranchConditionChanged
    ));
    assert!(per_fact_skips(
        AFTER_CALLEE_SWAP,
        ReviewFactKind::UsesUnwrap
    ));
}
