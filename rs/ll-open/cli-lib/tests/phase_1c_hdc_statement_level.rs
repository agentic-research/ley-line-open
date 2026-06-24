//! Phase 1C — statement-level HDC retrieval, empirical test of math-friend's fix.
//!
//! Phase 1 demonstrated HDC at *function* granularity fails near-similar
//! retrieval: SUM_A vs SUM_B (one deref node off) sit at saturation
//! distance, indistinguishable from random pairs. Math-friend session-2
//! diagnosed the cause (fingerprint-induced base_vector switching
//! cascades up the tree) and recommended **statement-level granularity**
//! as the fix.
//!
//! ## What this test checks
//!
//! Does the fix actually work on the Phase 1 corpus? Hypothesis:
//! - SUM_A and SUM_B share 2 of 3 statements exactly (`let mut t = 0;`
//!   and `t` — only the for-loop body's compound-assignment differs).
//! - Statement-level set-overlap should therefore mark them similar.
//! - SUM_A vs MATCH_B (truly unrelated) share no statements.
//!
//! If this hypothesis holds empirically, statement-level granularity is
//! validated and we can ship the substrate-level change (bead
//! `ley-line-open-3983bf`). If it doesn't hold, we learn something
//! about the failure mode and the substrate change is premature.
//!
//! ## How it works
//!
//! 1. Extract `function_item` nodes from each Phase 1 snippet.
//! 2. From each function body (block), extract every named child as a
//!    "statement-like" subtree (let_declaration, expression_statement,
//!    trailing-expression, etc.).
//! 3. Encode each statement separately via the production HDC encoder.
//! 4. Function similarity = count of (query stmt, candidate stmt) pairs
//!    with Hamming distance ≤ THRESHOLD, normalized by total query stmts.
//!    Set-Jaccard-like score in [0, 1].
//! 5. Run top-K retrieval over the corpus, assert the failure cases from
//!    Phase 1 now succeed.
//!
//! ## Reproduce
//!
//! ```sh
//! cargo test --release -p leyline-cli-lib --features hdc \
//!     --test phase_1c_hdc_statement_level -- --ignored --nocapture
//! ```

#![cfg(feature = "hdc")]

use leyline_cli_lib::daemon::hdc_pass::tree_to_encoder_node;
use leyline_hdc::canonical::RustCanonicalMap;
use leyline_hdc::codebook::AstCodebook;
use leyline_hdc::encode_fresh;
use leyline_hdc::util::{Hypervector, popcount_distance};
use leyline_ts::languages::TsLanguage;
use tree_sitter::{Node, Parser};

/// Hamming distance below which two statement HVs are treated as a
/// "near-match." Tighter than D/2 = 4096 by enough margin that random
/// pairs almost never satisfy it. Picked empirically (random-pair std
/// ≈ 45; threshold 3950 = ~3σ below D/2). The actual contribution to
/// retrieval here is mostly EXACT matches (distance 0) from
/// canonicalization-equivalent statements; this threshold catches
/// near-misses too.
const NEAR_MATCH_RADIUS: u32 = 3950;

const PARSE_A: &str =
    "fn parse_a(s: &str) -> Result<i32, ()> { let n: i32 = s.parse().map_err(|_| ())?; Ok(n * 2) }";
const PARSE_B: &str =
    "fn parse_b(s: &str) -> Result<u64, ()> { let m: u64 = s.parse().map_err(|_| ())?; Ok(m + 1) }";
const PARSE_C: &str = "fn parse_c(s: &str) -> Result<f32, ()> { let x: f32 = s.parse().map_err(|_| ())?; Ok(x / 2.0) }";

const SUM_A: &str = "fn sum_a(v: &[i32]) -> i32 { let mut t = 0i32; for x in v { t += x; } t }";
const SUM_B: &str = "fn sum_b(v: &[u64]) -> u64 { let mut s = 0u64; for x in v { s += *x; } s }";
const SUM_C: &str = "fn sum_c(v: &[f32]) -> f32 { let mut a = 0.0f32; for x in v { a += x; } a }";

const TRIVIAL: &str = "fn id() -> i32 { 42 }";

const COMPLEX: &str = r#"
fn complex_parser(input: &str) -> Result<Vec<i32>, String> {
    let mut out = Vec::new();
    for (i, line) in input.lines().enumerate() {
        if line.is_empty() { continue; }
        let trimmed = line.trim();
        match trimmed.chars().next() {
            Some('#') => continue,
            Some(c) if c.is_ascii_digit() => {
                let n: i32 = trimmed.parse().map_err(|e| format!("line {i}: {e}"))?;
                out.push(n);
            }
            _ => return Err(format!("line {i}: unexpected char")),
        }
    }
    Ok(out)
}
"#;

const MATCH_A: &str = "fn cls(c: char) -> i32 { match c { 'a' => 1, 'b' => 2, _ => 0 } }";
const MATCH_B: &str = "fn pri(c: char) -> i32 { match c { 'x' => 5, 'y' => 9, _ => -1 } }";

const ASYNC_A: &str = "fn maybe_run(f: impl Fn() -> i32) -> i32 { f() }";

/// Walk a tree-sitter::Node looking for the first `function_item`
/// descendant. Caller-friendly wrapper; Phase 1 corpus snippets each
/// have exactly one fn at the root.
fn find_first_function<'a>(node: Node<'a>) -> Option<Node<'a>> {
    if node.kind() == "function_item" {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = find_first_function(child) {
            return Some(found);
        }
    }
    None
}

/// Extract every "statement-like" child of a `function_item`'s body
/// block. tree-sitter-rust models the body as a `block` whose named
/// children are statements + optionally a trailing expression. We
/// treat both as separate units — they're the per-line slices a human
/// would call "the statements of this function."
fn extract_statements<'a>(function_item: Node<'a>) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    let Some(body) = function_item.child_by_field_name("body") else {
        return out;
    };
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        out.push(child);
    }
    out
}

/// Per-function record: source label + the per-statement HVs.
struct FnStmts {
    label: &'static str,
    stmt_hvs: Vec<Hypervector>,
}

fn encode_fn(label: &'static str, src: &str, parser: &mut Parser) -> FnStmts {
    let kind_map = RustCanonicalMap;
    let codebook = AstCodebook;
    let tree = parser.parse(src, None).expect("parse");
    let root = tree.root_node();
    let function_item = find_first_function(root).expect("function_item not found");
    let stmts = extract_statements(function_item);
    let stmt_hvs: Vec<Hypervector> = stmts
        .into_iter()
        .map(|stmt| {
            let encoder_node = tree_to_encoder_node(stmt, &kind_map);
            encode_fresh(&encoder_node, &codebook)
        })
        .collect();
    FnStmts { label, stmt_hvs }
}

/// Statement-level set similarity: for each statement in `query.stmt_hvs`,
/// find the BEST matching statement in `candidate.stmt_hvs` (lowest
/// Hamming distance). Count how many query statements have a best-match
/// distance ≤ NEAR_MATCH_RADIUS. Normalize by total query statement
/// count → score in [0, 1].
///
/// This is the simplest "fraction of query statements that have a near
/// match in candidate" metric. Not symmetric (query vs candidate). For
/// retrieval that's fine — query is always the anchor.
fn statement_set_similarity(query: &FnStmts, candidate: &FnStmts) -> (f64, usize) {
    if query.stmt_hvs.is_empty() {
        return (0.0, 0);
    }
    let mut matched = 0;
    for q_hv in &query.stmt_hvs {
        let best = candidate
            .stmt_hvs
            .iter()
            .map(|c_hv| popcount_distance(q_hv, c_hv))
            .min()
            .unwrap_or(u32::MAX);
        if best <= NEAR_MATCH_RADIUS {
            matched += 1;
        }
    }
    let score = matched as f64 / query.stmt_hvs.len() as f64;
    (score, matched)
}

#[test]
#[ignore = "Phase 1C — statement-level granularity empirical test. Run with --ignored --nocapture"]
fn phase_1c_statement_level_unblocks_near_similar_retrieval() {
    println!("\n=== Phase 1C — does statement-level granularity fix the saturation? ===\n");

    let lang = TsLanguage::Rust.ts_language();
    let mut parser = Parser::new();
    parser.set_language(&lang).expect("set Rust");

    let corpus: Vec<FnStmts> = vec![
        encode_fn("PARSE_A", PARSE_A, &mut parser),
        encode_fn("PARSE_B", PARSE_B, &mut parser),
        encode_fn("PARSE_C", PARSE_C, &mut parser),
        encode_fn("SUM_A", SUM_A, &mut parser),
        encode_fn("SUM_B", SUM_B, &mut parser),
        encode_fn("SUM_C", SUM_C, &mut parser),
        encode_fn("TRIVIAL", TRIVIAL, &mut parser),
        encode_fn("COMPLEX", COMPLEX, &mut parser),
        encode_fn("MATCH_A", MATCH_A, &mut parser),
        encode_fn("MATCH_B", MATCH_B, &mut parser),
        encode_fn("ASYNC_A", ASYNC_A, &mut parser),
    ];

    println!("Statement counts per function:");
    for f in &corpus {
        println!("  {:>10}: {} statements", f.label, f.stmt_hvs.len());
    }
    println!();

    // ── Full similarity matrix ──────────────────────────────────────
    println!("Statement-set similarity (query=row, candidate=col, fraction in [0,1]):");
    print!("              ");
    for f in &corpus {
        print!("{:>10} ", f.label);
    }
    println!();
    for q in &corpus {
        print!("{:>10}    ", q.label);
        for c in &corpus {
            let (score, _matched) = statement_set_similarity(q, c);
            print!("{:>10.2} ", score);
        }
        println!();
    }
    println!();

    // ── Helpers ─────────────────────────────────────────────────────
    let get = |label: &str| -> &FnStmts { corpus.iter().find(|f| f.label == label).unwrap() };

    let top_k_by_label = |query_label: &str, k: usize| -> Vec<(&'static str, f64)> {
        let query = get(query_label);
        let mut scored: Vec<(&'static str, f64)> = corpus
            .iter()
            .map(|c| {
                let (score, _) = statement_set_similarity(query, c);
                (c.label, score)
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.into_iter().take(k).collect()
    };

    // ── Test 1: query SUM_A — failure case from Phase 1 ─────────────
    //
    // Phase 1 result at function granularity: top-3 = [SUM_A, SUM_C, MATCH_B].
    // SUM_B (one node off) was NOT in top-3 — instead an unrelated
    // MATCH_B ranked above it.
    //
    // Statement-level prediction: SUM_A and SUM_B share `let mut x = 0;`
    // and the trailing `x` exactly (canonicalize identical). 2/3
    // statements match. SUM_B should rank in top-3.

    let top3_sum_a = top_k_by_label("SUM_A", 3);
    println!("─── Test 1: query SUM_A (Phase 1 had MISS on SUM_B) ───");
    for (label, score) in &top3_sum_a {
        println!("  top-3: {label:>10}  score={score:.2}");
    }
    let sum_b_in_top3 = top3_sum_a.iter().any(|(l, _)| *l == "SUM_B");
    println!("  SUM_B in top-3: {sum_b_in_top3}");
    assert!(
        sum_b_in_top3,
        "statement-level fix MUST find SUM_B in top-3 (it shares 2/3 statements with SUM_A)"
    );
    println!();

    // ── Test 2: query MATCH_A — failure case from Phase 1 ───────────
    //
    // Phase 1 result: top-3 = [MATCH_A, ASYNC_A, TRIVIAL]. MATCH_B was
    // NOT in top-3 despite being the only other match function.
    //
    // Statement-level prediction: MATCH_A and MATCH_B both have one
    // statement (the match expression). That statement saturates because
    // the wildcard arm differs by one Unary node. So this case may STILL
    // fail — there's no other statement to amortize over.
    //
    // We assert what we actually find; if it fails, the failure tells us
    // statement-level isn't a universal fix and we need to think
    // harder.

    let top3_match_a = top_k_by_label("MATCH_A", 3);
    println!("─── Test 2: query MATCH_A (Phase 1 had MISS on MATCH_B) ───");
    for (label, score) in &top3_match_a {
        println!("  top-3: {label:>10}  score={score:.2}");
    }
    let match_b_in_top3 = top3_match_a.iter().any(|(l, _)| *l == "MATCH_B");
    println!("  MATCH_B in top-3: {match_b_in_top3}");
    if !match_b_in_top3 {
        println!("  → MATCH_A/B is a single-statement case. Statement-level can't amortize.");
        println!("    Open question: is this a fundamental limit (one-statement-functions");
        println!("    have no set to overlap over), or does this case need a different fix");
        println!("    (sub-expression-level granularity, or different bind algebra)?");
    }
    println!();

    // ── Test 3: cluster cohesion ────────────────────────────────────
    //
    // PARSE_A should rank PARSE_B and PARSE_C highly (they share all 2
    // statements — let-binding + return-Ok). At function granularity
    // they were already at distance 0; at statement level they should
    // still be highly similar.

    let top3_parse = top_k_by_label("PARSE_A", 3);
    println!("─── Test 3: query PARSE_A — control case (was distance 0 at fn-level) ───");
    for (label, score) in &top3_parse {
        println!("  top-3: {label:>10}  score={score:.2}");
    }
    let parse_b_in = top3_parse.iter().any(|(l, _)| *l == "PARSE_B");
    let parse_c_in = top3_parse.iter().any(|(l, _)| *l == "PARSE_C");
    println!("  PARSE_B in top-3: {parse_b_in}");
    println!("  PARSE_C in top-3: {parse_c_in}");
    assert!(
        parse_b_in && parse_c_in,
        "statement-level must preserve the exact-match retrieval from function-level"
    );
    println!();

    // ── Verdict ─────────────────────────────────────────────────────
    println!("─── Verdict ───");
    if sum_b_in_top3 && match_b_in_top3 {
        println!("Statement-level granularity SOLVES both Phase 1 failure cases.");
        println!("Substrate change (bead ley-line-open-3983bf) is empirically justified.");
    } else if sum_b_in_top3 {
        println!("Statement-level granularity solves the multi-statement case (SUM_A↔SUM_B)");
        println!("but NOT the single-statement case (MATCH_A↔MATCH_B). The substrate change");
        println!("helps but isn't a universal fix; one-statement-function retrieval needs");
        println!("a different mechanism (sub-expression granularity? structural LSH?).");
    } else {
        println!("Statement-level granularity does NOT solve the multi-statement case.");
        println!("Math-friend's recommendation needs revisiting.");
    }
    println!();
}
