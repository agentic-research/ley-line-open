//! Restriction-addressed review cache over LLO's REAL fact substrate.
//!
//! Follow-up to the toy in `src/restriction_review.rs` + its design doc
//! (`docs/superpowers/specs/2026-07-17-restriction-ast-review-toy-design.md`),
//! which named the real experiment: replace the toy line-parser's
//! observables with the live extraction pipeline's fact columns —
//! `node_refs` / `node_defs` / `_imports` / `qualifier` /
//! `container_node_id` — and gate an EXPENSIVE review result on a CHEAP
//! restriction hash.
//!
//! CLAIM UNDER TEST (falsifiable): a cached expensive review result can
//! be safely reused when its cheap fact-specific restriction hash is
//! unchanged, even when the whole-object content hash changed.
//!
//! One review family: the CALL-TARGET review of a function F ("what
//! does F call, and where does each call resolve?").
//!
//! Three artifacts are kept structurally separate — the toy conflated
//! the first two, which made its soundness column true by construction:
//!
//! 1. RESTRICTION (cheap projection, [`restriction_for_call_target`]):
//!    a hash over a sound superset of the review's INPUT rows — F's
//!    container identity, the sorted `(token, qualifier)` pairs of
//!    `node_refs` rows whose container is F, the `(alias, path)` import
//!    rows of F's file whose alias any target token/qualifier names,
//!    and the `node_defs` rows F's target tokens index to (a
//!    token-indexed point lookup, cross-file). It never runs
//!    resolution.
//! 2. REVIEW RESULT (expensive, [`review_call_targets`]): the resolved
//!    call graph — for every call-target row of F, an unindexed
//!    cross-corpus JOIN over all `node_defs` rows plus the import
//!    surface, producing [`ResolvedEdge`]s. This is what a cache would
//!    store and a skip would avoid recomputing.
//! 3. ORACLE: did the review RESULT actually change between before and
//!    after? Computed by running the expensive path on both versions
//!    and comparing outputs — never by consulting the restriction.
//!
//! Compared cache policies:
//! - `WholeObject`: skip iff the byte hash of the WHOLE CORPUS is
//!   unchanged (the only per-object hash that is sound for a
//!   cross-file review; the per-file variant would false-skip the
//!   dep-side def edit — see the `corpus_def_rename` fixture).
//! - `AstShape`: skip iff the identifier-blind named-node-kind
//!   sequence of the corpus is unchanged (ADR-0030 rung 2's
//!   representation).
//! - `Restriction`: skip iff the call-target restriction hash of F is
//!   unchanged.
//!
//! DELIBERATE DEVIATION from the persisted substrate, stated up front:
//! containers are identified by name (`fn:score`), not by the daemon's
//! positional `node_id` paths. A restriction that hashed positional
//! ids would be invalidated by any line shift above F and degenerate
//! into whole-file sensitivity. Stable (name-scoped) container
//! identity is therefore a PRECONDITION for restriction-addressing —
//! that is a finding, not an implementation convenience. Fact rows are
//! compared as sets (like rung 2's oracle): multiplicity of identical
//! `(token, qualifier)` call sites does not enter either the
//! restriction or the review result.

#[path = "common/mod.rs"]
mod common;

use common::{kind_sequence, parse_rust};
use leyline_ts::refs::{ExtractedRef, extract_rust};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::hint::black_box;
use std::time::Instant;
use tree_sitter::Node;

/// Unit separator — cannot occur in tokens, paths, or grammar kinds,
/// so hashed row boundaries are unambiguous.
const US: char = '\u{1f}';

/// The function under review in every fixture.
const TARGET: &str = "fn:score";

// ---------------------------------------------------------------------------
// Fact substrate: the node_defs / node_refs / _imports rows the live
// pipeline emits, materialized per corpus via `extract_rust`.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DefRow {
    token: String,
    source_id: String,
    kind: String,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct RefRow {
    token: String,
    qualifier: Option<String>,
    container: Option<String>,
    source_id: String,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ImportRow {
    alias: String,
    path: String,
    source_id: String,
}

struct FactSubstrate {
    defs: Vec<DefRow>,
    refs: Vec<RefRow>,
    imports: Vec<ImportRow>,
    /// token → indices into `defs`. Models the indexed point lookup a
    /// daemon-side `node_defs(token)` query performs.
    def_index: BTreeMap<String, Vec<usize>>,
    /// container → indices into `refs`. Models the indexed
    /// per-container `node_refs` lookup.
    refs_by_container: BTreeMap<String, Vec<usize>>,
}

fn build_substrate(corpus: &[(String, String)]) -> FactSubstrate {
    let mut defs = BTreeSet::new();
    let mut refs = BTreeSet::new();
    let mut imports = BTreeSet::new();
    for (path, src) in corpus {
        let tree = parse_rust(src.as_bytes()).expect("fixture source must parse");
        walk_extract(
            tree.root_node(),
            src.as_bytes(),
            path,
            None,
            &mut defs,
            &mut refs,
            &mut imports,
        );
    }
    let defs: Vec<DefRow> = defs.into_iter().collect();
    let refs: Vec<RefRow> = refs.into_iter().collect();
    let imports: Vec<ImportRow> = imports.into_iter().collect();

    let mut def_index: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, d) in defs.iter().enumerate() {
        def_index.entry(d.token.clone()).or_default().push(i);
    }
    let mut refs_by_container: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, r) in refs.iter().enumerate() {
        if let Some(c) = &r.container {
            refs_by_container.entry(c.clone()).or_default().push(i);
        }
    }
    FactSubstrate {
        defs,
        refs,
        imports,
        def_index,
        refs_by_container,
    }
}

/// Per-named-node fold mirroring the daemon's content-addressing walk:
/// `extract_rust` is anchored (only patterns rooted at the node emit),
/// and the nearest enclosing `function_item`'s NAME is threaded as the
/// container identity (see module doc for why name, not position).
fn walk_extract(
    node: Node<'_>,
    src: &[u8],
    source_id: &str,
    container: Option<&str>,
    defs: &mut BTreeSet<DefRow>,
    refs: &mut BTreeSet<RefRow>,
    imports: &mut BTreeSet<ImportRow>,
) {
    for r in extract_rust(&node, src, "n", source_id, container) {
        match r {
            ExtractedRef::Def {
                token,
                canonical_kind,
                ..
            } => {
                defs.insert(DefRow {
                    token,
                    source_id: source_id.to_string(),
                    kind: canonical_kind.unwrap_or("?").to_string(),
                });
            }
            ExtractedRef::Ref {
                token,
                qualifier,
                container_node_id,
                ..
            } => {
                refs.insert(RefRow {
                    token,
                    qualifier,
                    container: container_node_id,
                    source_id: source_id.to_string(),
                });
            }
            ExtractedRef::Import { alias, path, .. } => {
                imports.insert(ImportRow {
                    alias,
                    path,
                    source_id: source_id.to_string(),
                });
            }
        }
    }
    let own_container = (node.kind() == "function_item")
        .then(|| node.child_by_field_name("name"))
        .flatten()
        .and_then(|n| n.utf8_text(src).ok())
        .map(|name| format!("fn:{name}"));
    let child_container = own_container.as_deref().or(container);
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_extract(child, src, source_id, child_container, defs, refs, imports);
    }
}

// ---------------------------------------------------------------------------
// 1. RESTRICTION — cheap projection hash over the review's input rows.
// ---------------------------------------------------------------------------

/// Hash of the sound superset of facts the call-target review of
/// `container` depends on. Indexed lookups only; no resolution logic.
/// `rows_touched` counts substrate rows read — the cost proxy shared
/// with [`review_call_targets`].
fn restriction_for_call_target(
    sub: &FactSubstrate,
    container: &str,
    rows_touched: &mut u64,
) -> [u8; 32] {
    let mut buf = String::with_capacity(512);
    push_row(&mut buf, &["container", container]);

    // (a) F's own call-target rows: sorted (token, qualifier). BTreeSet
    // storage order makes the indexed slice already sorted.
    let mut tokens: BTreeSet<&str> = BTreeSet::new();
    let mut qualifiers: BTreeSet<&str> = BTreeSet::new();
    let mut files: BTreeSet<&str> = BTreeSet::new();
    for &i in index_slice(&sub.refs_by_container, container) {
        let r = &sub.refs[i];
        *rows_touched += 1;
        push_row(
            &mut buf,
            &["ref", &r.token, r.qualifier.as_deref().unwrap_or("")],
        );
        tokens.insert(&r.token);
        if let Some(q) = &r.qualifier {
            qualifiers.insert(q);
        }
        files.insert(&r.source_id);
    }

    // (b) the relevant import surface: (alias, path) rows in F's file
    // whose alias one of F's target tokens or qualifiers names.
    for imp in &sub.imports {
        *rows_touched += 1;
        if files.contains(imp.source_id.as_str())
            && (tokens.contains(imp.alias.as_str()) || qualifiers.contains(imp.alias.as_str()))
        {
            push_row(&mut buf, &["import", &imp.alias, &imp.path]);
        }
    }

    // (c) the def rows the target tokens can resolve to — token-indexed
    // point lookups, cross-file. This is what makes the restriction a
    // sound superset for a CROSS-ITEM review: a dep-side def change
    // must invalidate even though F's own file is byte-identical.
    for token in &tokens {
        if let Some(rows) = sub.def_index.get(*token) {
            for &i in rows {
                let d = &sub.defs[i];
                *rows_touched += 1;
                push_row(&mut buf, &["def", &d.token, &d.source_id, &d.kind]);
            }
        }
    }

    Sha256::digest(buf.as_bytes()).into()
}

/// Append one US-delimited, newline-terminated canonical row.
fn push_row(buf: &mut String, fields: &[&str]) {
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            buf.push(US);
        }
        buf.push_str(f);
    }
    buf.push('\n');
}

fn index_slice<'a>(index: &'a BTreeMap<String, Vec<usize>>, key: &str) -> &'a [usize] {
    index.get(key).map(Vec::as_slice).unwrap_or(&[])
}

// ---------------------------------------------------------------------------
// 2. REVIEW RESULT — the expensive resolved call graph.
// ---------------------------------------------------------------------------

/// One resolved call edge of F: which def rows the target token joins
/// to across the whole corpus, and through which import.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ResolvedEdge {
    token: String,
    qualifier: Option<String>,
    via_import: Option<String>,
    candidates: Vec<DefRow>,
}

/// The expensive path: for every call-target row of `container`,
/// resolve the import it travels through (file-scoped scan) and JOIN
/// against ALL `node_defs` rows in the corpus (unindexed scan — the
/// honest cost of cross-item resolution, and still only a stand-in for
/// a real review, which would be an analysis or an LLM pass on top of
/// these edges; the measured gap below is a lower bound).
fn review_call_targets(
    sub: &FactSubstrate,
    container: &str,
    rows_touched: &mut u64,
) -> BTreeSet<ResolvedEdge> {
    let mut out = BTreeSet::new();
    for &i in index_slice(&sub.refs_by_container, container) {
        let r = &sub.refs[i];
        *rows_touched += 1;

        let mut via_import = None;
        for imp in &sub.imports {
            *rows_touched += 1;
            if via_import.is_none()
                && imp.source_id == r.source_id
                && (imp.alias == r.token || r.qualifier.as_deref() == Some(imp.alias.as_str()))
            {
                via_import = Some(imp.path.clone());
            }
        }

        let mut candidates = Vec::new();
        for d in &sub.defs {
            *rows_touched += 1;
            if d.token == r.token {
                candidates.push(d.clone());
            }
        }
        candidates.sort();

        out.insert(ResolvedEdge {
            token: r.token.clone(),
            qualifier: r.qualifier.clone(),
            via_import,
            candidates,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Baseline policies.
// ---------------------------------------------------------------------------

fn whole_object_hash(corpus: &[(String, String)]) -> [u8; 32] {
    let mut h = Sha256::new();
    for (path, src) in corpus {
        h.update(format!("{path}{US}{src}\n").as_bytes());
    }
    h.finalize().into()
}

/// Identifier-blind structural hash: the pre-order named-node kind
/// sequence of every file (rung 2's representation, exact-hashed).
fn ast_shape_hash(corpus: &[(String, String)]) -> [u8; 32] {
    let mut h = Sha256::new();
    for (path, src) in corpus {
        let tree = parse_rust(src.as_bytes()).expect("fixture source must parse");
        let mut kinds: Vec<&'static str> = Vec::new();
        kind_sequence(tree.root_node(), &mut kinds);
        h.update(format!("{path}{US}{}\n", kinds.join("\u{1f}")).as_bytes());
    }
    h.finalize().into()
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

/// Whitespace-only edit inside `score` (blank lines; no comment — a
/// comment is a named node and would move the AST-shape baseline too).
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

/// Local rename: `adjusted` → `shifted`. The local is never a call
/// target, so no node_refs row moves — the load-bearing fixture.
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

/// Body arithmetic: `value + 1` → `value * 3`. No call touched — the
/// other load-bearing fixture.
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

/// Import path change, alias unchanged: the call sites are
/// byte-identical, only the `use` path moves.
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

/// Edit ELSEWHERE in F's file: `audit`'s arithmetic changes, `score`
/// is byte-identical. Proves the restriction is a genuine projection.
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

/// Qualified-call variant of `score` for the qualifier-swap fixture:
/// dual-emit gives a `mathq::qhelper` row plus a bare `qhelper` row
/// carrying `qualifier = Some("mathq")`.
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

/// Padding defs make the review's unindexed def JOIN measurably wider
/// than the restriction's indexed lookups, mirroring a corpus where
/// `node_defs` holds far more rows than any one function touches.
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
// Evaluation.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Policy {
    WholeObject,
    AstShape,
    Restriction,
}

struct FixtureResult {
    name: &'static str,
    review_changed: bool,
    skips: BTreeMap<Policy, bool>,
}

fn evaluate(fx: &Fixture) -> FixtureResult {
    let before = build_substrate(&fx.before);
    let after = build_substrate(&fx.after);

    // 3. ORACLE — the expensive result computed on both versions.
    let mut scratch = 0u64;
    let review_changed = review_call_targets(&before, TARGET, &mut scratch)
        != review_call_targets(&after, TARGET, &mut scratch);

    let mut skips = BTreeMap::new();
    skips.insert(
        Policy::WholeObject,
        whole_object_hash(&fx.before) == whole_object_hash(&fx.after),
    );
    skips.insert(
        Policy::AstShape,
        ast_shape_hash(&fx.before) == ast_shape_hash(&fx.after),
    );
    skips.insert(
        Policy::Restriction,
        restriction_for_call_target(&before, TARGET, &mut scratch)
            == restriction_for_call_target(&after, TARGET, &mut scratch),
    );

    FixtureResult {
        name: fx.name,
        review_changed,
        skips,
    }
}

#[derive(Default)]
struct PolicyStats {
    false_skips: usize,
    sound_skips: usize,
}

fn stats(results: &[FixtureResult], policy: Policy) -> PolicyStats {
    let mut s = PolicyStats::default();
    for r in results {
        if r.skips[&policy] {
            if r.review_changed {
                s.false_skips += 1;
            } else {
                s.sound_skips += 1;
            }
        }
    }
    s
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

/// The substrate must contain the rows the experiment reasons about —
/// loud failure here beats a silently-green verdict on wrong facts.
#[test]
fn substrate_extraction_sanity() {
    let sub = build_substrate(&corpus(MAIN_BEFORE, math_src("compute_weight", false)));

    let score_refs: BTreeSet<(&str, Option<&str>)> = index_slice(&sub.refs_by_container, TARGET)
        .iter()
        .map(|&i| (sub.refs[i].token.as_str(), sub.refs[i].qualifier.as_deref()))
        .collect();
    assert!(score_refs.contains(&("compute_weight", None)));
    assert!(score_refs.contains(&("compute_penalty", None)));

    assert!(
        sub.imports
            .iter()
            .any(|i| i.alias == "compute_weight" && i.path == "crate::math::compute_weight")
    );
    assert!(sub.def_index.contains_key("compute_weight"));
    assert!(sub.def_index.contains_key("pad_0"));

    // Qualified dual-emit: the bare row carries the qualifier.
    let qual = build_substrate(&corpus(MAIN_QUAL_BEFORE, math_src("compute_weight", false)));
    let qual_refs: BTreeSet<(&str, Option<&str>)> = index_slice(&qual.refs_by_container, TARGET)
        .iter()
        .map(|&i| {
            (
                qual.refs[i].token.as_str(),
                qual.refs[i].qualifier.as_deref(),
            )
        })
        .collect();
    assert!(qual_refs.contains(&("qhelper", Some("mathq"))));
    assert!(qual_refs.contains(&("mathq::qhelper", None)));
}

/// The verdict table: per-fixture skip decisions vs the oracle, then
/// aggregate rates per policy. Run with `--nocapture` to see it.
#[test]
fn restriction_review_verdict() {
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

    // Restriction: zero false skips across every fixture where the
    // review result changed. THE claim; if this trips, the restriction
    // was not a sound superset — report the fixture, don't patch it.
    let restr = stats(&results, Policy::Restriction);
    assert_eq!(
        restr.false_skips, 0,
        "restriction false-skipped a review-changing fixture — not a sound superset"
    );

    // The two load-bearing fixtures: semantic (non-whitespace) edits
    // that the call-target restriction must skip soundly while
    // whole-object CAS recomputes.
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

    // Projection proof: edits outside F (same file and dep file) leave
    // the restriction unchanged — it is strictly less than the object.
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

    // ADR-0030 reproduction: the identifier-blind shape hash false-
    // skips the callee swap (and, being blind to identifier text, the
    // other call-target-relevant families too).
    let r = result(&results, "callee-swap");
    assert!(
        r.skips[&Policy::AstShape],
        "callee-swap: AST shape is blind to identifier text"
    );
    let shape = stats(&results, Policy::AstShape);
    assert!(shape.false_skips > 0, "AstShape should reproduce ADR-0030");

    // WholeObject: sound but wasteful — every fixture edits some byte
    // in the corpus, so it never skips.
    let whole = stats(&results, Policy::WholeObject);
    assert_eq!(whole.false_skips, 0);
    assert!(
        restr.sound_skips > whole.sound_skips,
        "restriction must save recomputes whole-object cannot"
    );
}

/// restriction_cost < review_cost — measured as substrate rows touched
/// (deterministic, asserted at every scale) and wall time (swept over
/// corpus sizes, because it has a crossover: the restriction pays a
/// constant SHA-256 + buffer cost that a small enough in-memory join
/// undercuts, while the review's row touches grow with the corpus.
/// The wall-time assert is pinned at the largest scale, where the
/// asymptotics dominate the constants; the small-scale inversion is
/// printed, not hidden — it is part of the finding.)
#[test]
fn restriction_is_cheaper_than_review() {
    eprintln!(
        "\n{:>9} {:>10} {:>9} {:>12} {:>12} {:>9} {:>10}",
        "def_rows", "restr_ops", "rev_ops", "restr_time", "rev_time", "op_ratio", "time_ratio"
    );

    let scales: &[(usize, u32)] = &[(200, 2000), (2000, 500), (10000, 100)];
    let mut last: Option<(u64, u64, std::time::Duration, std::time::Duration)> = None;
    for &(pad, iters) in scales {
        let sub = build_substrate(&corpus(
            MAIN_BEFORE,
            math_src_padded("compute_weight", false, pad),
        ));

        let mut restriction_ops = 0u64;
        black_box(restriction_for_call_target(
            &sub,
            TARGET,
            &mut restriction_ops,
        ));
        let mut review_ops = 0u64;
        black_box(review_call_targets(&sub, TARGET, &mut review_ops));

        let t0 = Instant::now();
        for _ in 0..iters {
            let mut c = 0u64;
            black_box(restriction_for_call_target(&sub, TARGET, &mut c));
        }
        let restriction_time = t0.elapsed() / iters;
        let t1 = Instant::now();
        for _ in 0..iters {
            let mut c = 0u64;
            black_box(review_call_targets(&sub, TARGET, &mut c));
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
