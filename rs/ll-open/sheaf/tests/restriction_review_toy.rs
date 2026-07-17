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
