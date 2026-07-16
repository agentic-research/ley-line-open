//! ADR-0030 Rung 2 — Milestone A (bead `ley-line-open-d50164`): the
//! cheap-kill discrimination test for the RENAME-INVARIANT AST-structural
//! stalk.
//!
//! # The claim under test (the gate the byte stalk FAILED)
//!
//! Rung 1 (`embedding_stalk_divergence.rs`) proved that a byte-trigram
//! stalk is ANTI-correlated with fact stability: a cosmetic rename moved
//! the byte stalk FARTHER (`d/D ≈ 0.18`) than a fact-changing callee-swap
//! edit (`d/D ≈ 0.15`), because byte distance tracks trigram churn, not
//! structure. Byte stalks are ruled out.
//!
//! This file re-runs the SAME fixtures with the AST-structural stalk (an
//! HV over the tree-sitter node-kind sequence, identifier text excluded)
//! and asserts the discrimination the byte stalk could not deliver:
//!
//! * a cosmetic RENAME (`total` → `running_sum`; zero fact change) →
//!   distance ≈ 0 (kind sequence is byte-for-byte identical);
//! * a cosmetic WHITESPACE reflow → distance exactly 0 (tree-sitter
//!   ignores layout);
//! * a MEANINGFUL edit that adds a guard branch (`if *value < 0 {
//!   continue; }`) → distance LARGE (new `if_expression` / `continue`
//!   nodes enter the kind sequence).
//!
//! # Kill gate
//!
//! If the structural stalk ALSO fails to separate cosmetic from
//! meaningful (`meaningful <= rename`, i.e. inverted or indistinguishable),
//! ADR-0030 dies at Rung 2 Milestone A and these assertions fail loudly.
//! They PASS, so Milestone B (the git-replay value experiment) is
//! warranted — see `benches/git_replay_invalidation.rs`.
//!
//! # The honest caveat this test surfaces (measured below)
//!
//! The structural stalk's discrimination on the "meaningful" edit comes
//! ENTIRELY from the added branch. A PURE callee swap
//! (`compute_weight` → `compute_penalty`, no branch) is invisible to any
//! kind-structure embedding — both parse to `call_expression >
//! identifier` — yet it changes `node_refs`. `pure_callee_swap_is_a_
//! structural_false_negative` pins that blind spot as a MEASURED fact, so
//! the reader is not misled into thinking structural distance is a
//! complete fact-change detector. Quantifying how often that blind spot
//! fires on real edits is exactly Milestone B.

#[path = "common/mod.rs"]
mod common;

use common::{derive_facts, frac, structural_stalk};

// ---------------------------------------------------------------------------
// Fixtures — identical regions/edits to Rung 1 (embedding_stalk_divergence.rs)
// so the two rungs are measured on the same ground.
// ---------------------------------------------------------------------------

const BEFORE: &str = "\
fn accumulate(items: &[i64]) -> i64 {
    let mut total = 0;
    for value in items {
        total += compute_weight(value);
    }
    total
}
";

/// Cosmetic whitespace reflow — no token changes, derived facts unchanged.
const AFTER_COSMETIC_WS: &str = "\
fn accumulate(items: &[i64]) -> i64 {

    let mut total = 0;
    for value in items {
        total += compute_weight(value);
    }

    total
}
";

/// Cosmetic RENAME (`total` → `running_sum`) — derived facts unchanged.
/// This is the case that inverted the byte stalk in Rung 1.
const AFTER_RENAME: &str = "\
fn accumulate(items: &[i64]) -> i64 {
    let mut running_sum = 0;
    for value in items {
        running_sum += compute_weight(value);
    }
    running_sum
}
";

/// MEANINGFUL edit — swaps the callee (`compute_weight` → `compute_penalty`,
/// new `node_ref`) AND adds a guard branch (`if *value < 0 { continue; }`,
/// new CFG structure). A semantic optimizer MUST NOT skip this.
const AFTER_MEANINGFUL: &str = "\
fn accumulate(items: &[i64]) -> i64 {
    let mut total = 0;
    for value in items {
        if *value < 0 {
            continue;
        }
        total += compute_penalty(value);
    }
    total
}
";

/// PURE callee swap (`compute_weight` → `compute_penalty`), NO structural
/// change. Isolates the known blind spot: facts change, structure does not.
const AFTER_PURE_CALLEE_SWAP: &str = "\
fn accumulate(items: &[i64]) -> i64 {
    let mut total = 0;
    for value in items {
        total += compute_penalty(value);
    }
    total
}
";

// A larger region, to show the discrimination is not a small-sample artifact.
const BIG_BEFORE: &str = "\
fn summarize(records: &[Record], threshold: i64) -> Summary {
    let mut total = 0i64;
    let mut count = 0usize;
    let mut max_seen = i64::MIN;
    for record in records {
        let weight = compute_weight(record);
        if weight > threshold {
            total += weight;
            count += 1;
            if weight > max_seen {
                max_seen = weight;
            }
        }
    }
    let average = if count > 0 { total / count as i64 } else { 0 };
    Summary { total, count, max_seen, average }
}
";
const BIG_RENAME: &str = "\
fn summarize(records: &[Record], threshold: i64) -> Summary {
    let mut running = 0i64;
    let mut count = 0usize;
    let mut max_seen = i64::MIN;
    for record in records {
        let w = compute_weight(record);
        if w > threshold {
            running += w;
            count += 1;
            if w > max_seen {
                max_seen = w;
            }
        }
    }
    let average = if count > 0 { running / count as i64 } else { 0 };
    Summary { total: running, count, max_seen, average }
}
";
/// Fact-changing AND structure-changing edit in the big region: swap the
/// callee and drop the inner `if weight > max_seen` branch.
const BIG_MEANINGFUL: &str = "\
fn summarize(records: &[Record], threshold: i64) -> Summary {
    let mut total = 0i64;
    let mut count = 0usize;
    let mut max_seen = i64::MIN;
    for record in records {
        let weight = compute_penalty(record);
        if weight > threshold {
            total += weight;
            count += 1;
        }
    }
    let average = if count > 0 { total / count as i64 } else { 0 };
    Summary { total, count, max_seen, average }
}
";

/// Skip threshold as a fraction of the width (`d/D`), carried over
/// verbatim from Rung 1: skip only if the two boundary embeddings are
/// ≥ 0.95 cosine-similar (`d/D = 0.10` ⟺ cosine ≈ cos(0.1π)). Fixed by
/// the representation's geometry, not fitted to the measured distances.
const EPS_FRAC: f64 = 0.10;

fn stalk(src: &str) -> leyline_hdc::Hypervector {
    structural_stalk(src.as_bytes()).expect("fixture parses")
}

// ---------------------------------------------------------------------------
// Milestone A — the discrimination the byte stalk failed
// ---------------------------------------------------------------------------

#[test]
fn structural_stalk_separates_cosmetic_from_meaningful() {
    let before = stalk(BEFORE);

    let d_ws = frac(&before, &stalk(AFTER_COSMETIC_WS));
    let d_rename = frac(&before, &stalk(AFTER_RENAME));
    let d_meaningful = frac(&before, &stalk(AFTER_MEANINGFUL));

    let big_before = stalk(BIG_BEFORE);
    let big_rename = frac(&big_before, &stalk(BIG_RENAME));
    let big_meaningful = frac(&big_before, &stalk(BIG_MEANINGFUL));

    eprintln!("--- Rung-2 Milestone A: AST-structural stalk discrimination ---");
    eprintln!("small region:");
    eprintln!("  cosmetic whitespace  d/D = {d_ws:.4}   (facts unchanged)");
    eprintln!("  cosmetic rename      d/D = {d_rename:.4}   (facts unchanged)");
    eprintln!("  meaningful (+branch) d/D = {d_meaningful:.4}   (facts changed)");
    eprintln!("big region:");
    eprintln!("  cosmetic rename      d/D = {big_rename:.4}   (facts unchanged)");
    eprintln!("  meaningful (±branch) d/D = {big_meaningful:.4}   (facts changed)");
    eprintln!("EPS_FRAC = {EPS_FRAC} (cosine ≈ 0.95)");

    // Cosmetic edits are rename-INVARIANT: the kind sequence is
    // unchanged, so the stalk is identical (distance 0). This is the
    // property the byte stalk lacked.
    assert_eq!(
        d_ws, 0.0,
        "whitespace reflow must leave the AST-structural stalk identical (d/D={d_ws})",
    );
    assert_eq!(
        d_rename, 0.0,
        "a rename that touches no surface syntax must leave the stalk identical (d/D={d_rename})",
    );
    // HONEST NUANCE (measured): rename-invariance is NOT absolute. In the
    // big region the rename `total`→`running` / `weight`→`w` forces Rust's
    // struct field shorthand `Summary { total }` to expand to
    // `Summary { total: running }` — a genuine grammar-level shape change
    // (`shorthand_field_initializer` → `field_initializer`). So the stalk
    // moves a little (d/D ≈ 0.08). It stays BELOW the skip threshold (still
    // correctly skippable, facts unchanged) and below the meaningful edit,
    // but it is not zero. A rename is structurally invisible only when it
    // does not perturb surface syntax the grammar distinguishes.
    assert!(
        big_rename < EPS_FRAC,
        "big-region rename stays a skippable cosmetic move (d/D={big_rename} < {EPS_FRAC}); \
         it is nonzero only because the rename broke field shorthand",
    );

    // Meaningful (structure-changing) edits move the stalk above EPS.
    assert!(
        d_meaningful > EPS_FRAC,
        "structure-changing edit must exceed the skip threshold: d/D={d_meaningful} !> {EPS_FRAC}",
    );
    assert!(
        big_meaningful > EPS_FRAC,
        "big-region structure change must exceed EPS: d/D={big_meaningful} !> {EPS_FRAC}",
    );

    // THE DISCRIMINATION (the KILL GATE): meaningful must be strictly
    // FARTHER than cosmetic. Rung 1's byte stalk INVERTED this. If the
    // structural stalk also inverts or ties, ADR-0030 dies here.
    assert!(
        d_meaningful > d_rename,
        "KILL GATE: structural stalk must move MORE on a fact-changing structural edit \
         ({d_meaningful}) than on a cosmetic rename ({d_rename}); inverted/tied ⇒ ADR-0030 \
         dies at Rung 2 Milestone A",
    );
    assert!(
        big_meaningful > big_rename,
        "KILL GATE (big region): meaningful ({big_meaningful}) must exceed rename ({big_rename})",
    );
}

/// The oracle agrees with the labels the discrimination test asserts:
/// the cosmetic edits truly leave `node_defs`/`node_refs` unchanged and
/// the meaningful edits truly change them. Pins that "cosmetic" and
/// "meaningful" are not just our opinion but the re-derived ground truth.
#[test]
fn oracle_confirms_fixture_labels() {
    assert_eq!(
        derive_facts(BEFORE.as_bytes()),
        derive_facts(AFTER_COSMETIC_WS.as_bytes()),
        "whitespace reflow must not change derived facts",
    );
    assert_eq!(
        derive_facts(BEFORE.as_bytes()),
        derive_facts(AFTER_RENAME.as_bytes()),
        "a local rename must not change derived facts (defs/refs are token sets)",
    );
    assert_ne!(
        derive_facts(BEFORE.as_bytes()),
        derive_facts(AFTER_MEANINGFUL.as_bytes()),
        "callee swap + new branch must change derived facts",
    );
}

/// THE BLIND SPOT, PINNED AS A MEASURED FACT. A pure callee swap changes
/// `node_refs` (oracle: facts_changed = true) while the AST-structural
/// stalk does not move at all (d/D = 0) — a structural FALSE NEGATIVE. A
/// δ⁰-skip on this edit would serve stale facts. This is not a defect in
/// the stalk; it is the fundamental limit of a rename-invariant
/// representation, and Milestone B measures how often real edits land
/// here.
#[test]
fn pure_callee_swap_is_a_structural_false_negative() {
    let d = frac(&stalk(BEFORE), &stalk(AFTER_PURE_CALLEE_SWAP));
    let facts_changed =
        derive_facts(BEFORE.as_bytes()) != derive_facts(AFTER_PURE_CALLEE_SWAP.as_bytes());

    eprintln!("--- Rung-2 Milestone A: the structural blind spot ---");
    eprintln!("pure callee swap (compute_weight → compute_penalty):");
    eprintln!("  structural stalk d/D = {d:.4}   (would SKIP under any EPS > 0)");
    eprintln!("  oracle facts_changed = {facts_changed}   (node_refs token changed)");

    assert_eq!(
        d, 0.0,
        "a pure callee swap is invisible to a kind-structure stalk (d/D must be 0)",
    );
    assert!(
        facts_changed,
        "a pure callee swap MUST change derived facts (the ref token changed)",
    );
}
