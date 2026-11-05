//! `SheafCache` is advisory: a region it keeps is a candidate for reuse, not
//! a valid value (bead `ley-line-open-f04610`).
//!
//! # The replay case this fixture is shrunk from
//!
//! `task cache:ablation`, step `1ec25f4a1fdd` (#469): the free function
//! `evidence` in `cli-lib/tests/execution_transport.rs` calls
//! `digest(..)`. Its bytes did not change. The commit added an unrelated
//! method `pub fn digest(&self)` in `runtime/src/lib.rs`, so the call-target
//! review of `evidence` (every corpus def of each called token) gained a
//! candidate. No aligned region's bytes changed in a way that reaches
//! `evidence`, and the new def is not a region at all, so `SheafCache` kept
//! `evidence` and served its parent value: a false skip, in both the
//! heuristic and the δ⁰ mode. The restriction key binds the def rows of every
//! token the function reads, so it missed and recomputed.
//!
//! All 154 heuristic false skips of the 60-commit replay share that shape:
//! the function's bytes are unchanged and its value moved through a fact
//! outside them (145 through a called token's def set, 9 through an import
//! edit in the same file).
//!
//! # The contract pinned here
//!
//! 1. `SheafCache` keeps the region and serves the stale value (the
//!    measured behavior; it is advisory and claims no soundness).
//! 2. The restriction key serves no stale value on the same step.
//! 3. The module doc and the crate README state that `SheafCache` is
//!    advisory and must not gate reuse.

#[path = "../../cache/tests/common/mod.rs"]
mod common;
#[path = "sheaf_arm/mod.rs"]
mod sheaf_arm;

use common::ablation::{Arm, ArmStats, KeyArm, build_step, run_step};
use leyline_cache::strategy::RestrictionKey;
use sheaf_arm::SheafArm;

const TRANSPORT: &str = "rs/ll-open/cli-lib/tests/execution_transport.rs";
const RUNTIME: &str = "rs/ll-open/runtime/src/lib.rs";

const TRANSPORT_SRC: &str = r#"
fn evidence(out: &mut Vec<u8>) {
    out.push(digest(7));
}

fn digest(byte: u8) -> u8 {
    byte
}
"#;

const RUNTIME_BEFORE: &str = r#"
pub struct Share;

impl Share {
    pub fn path(&self) -> u8 {
        0
    }
}
"#;

/// The parent plus one new method whose name collides with the free
/// function `evidence` calls. `path` is byte-identical.
const RUNTIME_AFTER: &str = r#"
pub struct Share;

impl Share {
    pub fn path(&self) -> u8 {
        0
    }

    pub fn digest(&self) -> String {
        String::new()
    }
}
"#;

fn corpus(runtime: &str) -> Vec<(String, String)> {
    vec![
        (TRANSPORT.to_string(), TRANSPORT_SRC.to_string()),
        (RUNTIME.to_string(), runtime.to_string()),
    ]
}

fn replay() -> Vec<ArmStats> {
    let (step, excluded) = build_step("fixture", &corpus(RUNTIME_BEFORE), &corpus(RUNTIME_AFTER));
    assert_eq!(excluded, 0, "every fixture function aligns");
    let evidence = step
        .targets
        .iter()
        .find(|t| t.name == "evidence")
        .expect("evidence is an aligned region");
    assert_eq!(
        evidence.bytes_before, evidence.bytes_after,
        "the fixture's premise: evidence's own bytes do not change"
    );
    assert_ne!(
        evidence.value_before, evidence.value_after,
        "the fixture's premise: evidence's call-target review does change"
    );
    assert!(
        step.targets.iter().all(|t| t.bytes_before == t.bytes_after),
        "no aligned region's bytes change, so on_change gets no root"
    );

    let mut arms: Vec<Box<dyn Arm>> = vec![
        Box::new(KeyArm::new(RestrictionKey)),
        Box::new(SheafArm::new(false)),
        Box::new(SheafArm::new(true)),
    ];
    let mut stats = Vec::new();
    run_step(&step, &mut arms, &mut stats);
    stats
}

#[test]
fn sheaf_cache_keeps_a_region_whose_value_changed_outside_its_bytes() {
    let stats = replay();
    for s in &stats[1..] {
        assert!(!s.claims_soundness, "{} must not claim soundness", s.name);
        assert_eq!(
            s.false_skip_log,
            vec![format!("fixture {TRANSPORT} evidence#0")],
            "{}: SheafCache keeps evidence and serves its parent value \
             (advisory, bead ley-line-open-f04610)",
            s.name
        );
    }
}

#[test]
fn restriction_key_recomputes_the_region_sheaf_cache_keeps() {
    let stats = replay();
    let restriction = &stats[0];
    assert_eq!(restriction.name, "restriction");
    assert!(restriction.claims_soundness);
    assert_eq!(
        restriction.false_skips + restriction.warm_false_skips,
        0,
        "the restriction key must serve no stale value: {:?}",
        restriction.false_skip_log
    );
    assert!(
        restriction.hits < restriction.lookups,
        "the restriction key must miss evidence"
    );
}

/// The advisory statement, verbatim, in both places a consumer reads.
const ADVISORY: &str = "`SheafCache` is advisory: a region it keeps is a candidate for reuse, \
                        not a valid value, and its keep decision must not gate reuse.";

fn squash(s: &str) -> String {
    s.lines()
        .map(|l| l.trim_start().trim_start_matches("//!").trim())
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn module_doc_and_readme_state_that_sheaf_cache_is_advisory() {
    let want = squash(ADVISORY);
    let module_doc: String = include_str!("../src/cache.rs")
        .lines()
        .take_while(|l| l.starts_with("//!"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        squash(&module_doc).contains(&want),
        "cache.rs module doc must state: {ADVISORY}"
    );
    assert!(
        squash(include_str!("../README.md")).contains(&want),
        "the sheaf README must state: {ADVISORY}"
    );
}
