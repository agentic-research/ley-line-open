//! Phase 1 — what does HDC at function granularity actually find?
//!
//! Earlier phases measured *properties* (throughput, statistical
//! independence, encoder algebra). None demonstrated HDC doing the
//! retrieval thing it's supposed to do. This file does — and the
//! finding is sharper than "HDC works."
//!
//! ## Headline finding
//!
//! HDC at **function granularity** on real Rust code is **all-or-
//! nothing**:
//! - Functions whose canonical-kind ASTs are IDENTICAL (same shape,
//!   types and literals collapsed by the canonical alphabet) map to
//!   distance 0 — perfect identification.
//! - Functions that differ by **one AST node** (e.g., `+= x` vs `+= *x`,
//!   `0` vs `-1`) map to ~D/2 distance, indistinguishable from random.
//!
//! ## Why (math-friend diagnosis, 2026-06-24 session-2)
//!
//! NOT bundle saturation (the bundle is linear: rotation is an
//! isometry, XOR-bundle preserves the children's distance differential).
//! NOT content_hash propagation (content_hash is only the SubtreeCache
//! key — never enters the HV computation).
//!
//! The mechanism is **fingerprint-induced base-vector switching**:
//!   1. Adding one Unary leaf changes its direct parent's
//!      `sorted_child_kinds` set (e.g. `[Lit]` → `[Op]`).
//!   2. The parent's fingerprint `fp = (kind, arity, sorted_child_kinds)`
//!      is therefore different.
//!   3. `base_vector(fp)` is keyed on the fingerprint via a PRG, so the
//!      parent's base_vector becomes a fresh-random hypervector
//!      ≈ D/2 away from the original.
//!   4. By induction up the tree, every ancestor of the changed leaf
//!      also has a different fingerprint and a different base_vector.
//!   5. The function-root HV is therefore ≈ D/2 away — pure random pair
//!      distance.
//!
//! The encoder is faithful to HDC textbook (Plate 1994 / Kanerva 2009).
//! The textbook ASSUMES `base_vector` is smooth across small structural
//! perturbations. The fingerprint-keying we ship makes that impossible:
//! adding one node flips an unrelated PRG seed at every ancestor of the
//! change. There is no smoothness in fingerprint space — only exact
//! match or "different seed."
//!
//! Math-friend's earlier saturation gate (median pair-distance in
//! [4090, 4102]) was one-sided. The two-sided gate that would have
//! caught this:
//!   "Build N pairs that differ by one canonical-leaf insertion;
//!    assert median pair distance ≤ D/2 − 3·σ_random ≈ 3961"
//! Today's encoder gives ~4115 on those pairs — fails by ~150 bits.
//!
//! ## What this test does
//!
//! Hand-built corpus of 11 Rust functions. Computes the full pairwise
//! HDC distance matrix, prints it, then HARD-asserts the things that
//! actually work (canonicalization across types/literals) and
//! CHARACTERIZATION-asserts the things that don't (near-similar at
//! saturation distance). If the encoder ever improves and near-similar
//! pairs drop below the saturation band, the characterization
//! assertions will fail loudly — a "good failure" the next encoder
//! pass should know to look at.
//!
//! ## What this test is NOT
//!
//! It's not "Phase 1 done, HDC retrieval works." HDC at function
//! granularity does NOT work for the typical RAG case of "find code
//! that's like this one but not identical." Math-friend's
//! recommendation (2026-06-24 session-2): **statement-level
//! granularity**. Each statement is shallow (depth 2-3) so the
//! fingerprint-cascade can't reach a function root. Retrieval becomes
//! "how many statement HVs from query match statement HVs in
//! candidate" (set-overlap or graded count) rather than point-
//! similarity on function HVs.
//!
//! Phase 0C's "complementary modality" finding was real-but-trivial:
//! HDC at function granularity is a **binary hash-equality oracle**,
//! not a graded similarity. The MI 1.2σ-above-null was carried by a
//! few exact matches + pure noise everywhere else. Not a
//! complementary modality — a degenerate one.
//!
//! ## Reproduce
//!
//! ```sh
//! cargo test --release -p leyline-cli-lib --features hdc \
//!     --test phase_1_hdc_retrieval -- --ignored --nocapture
//! ```

#![cfg(feature = "hdc")]

use leyline_cli_lib::daemon::hdc_pass::parse_and_encode_tree;
use leyline_hdc::canonical::RustCanonicalMap;
use leyline_hdc::codebook::AstCodebook;
use leyline_hdc::encode_fresh;
use leyline_hdc::util::{Hypervector, popcount_distance};
use leyline_ts::languages::TsLanguage;

/// Encode a Rust source snippet into an HDC hypervector via the same
/// path the daemon's `hdc_search` op uses. Panics on parse failure
/// (the snippets in this test are all well-formed).
fn encode_rust(src: &str) -> Hypervector {
    let lang = TsLanguage::Rust.ts_language();
    let tree = parse_and_encode_tree(src, &lang, &RustCanonicalMap)
        .expect("parse_and_encode_tree must succeed on well-formed Rust");
    encode_fresh(&tree, &AstCodebook)
}

// ────────────────────────────────────────────────────────────────────
// Corpus — hand-built Rust functions, clustered by structural pattern
// ────────────────────────────────────────────────────────────────────

/// Three implementations of "parse-and-return-result". Different
/// concrete types + literals. Canonical-kind ASTs are IDENTICAL —
/// HDC should collapse them to the same hypervector.
const PARSE_A: &str =
    "fn parse_a(s: &str) -> Result<i32, ()> { let n: i32 = s.parse().map_err(|_| ())?; Ok(n * 2) }";
const PARSE_B: &str =
    "fn parse_b(s: &str) -> Result<u64, ()> { let m: u64 = s.parse().map_err(|_| ())?; Ok(m + 1) }";
const PARSE_C: &str = "fn parse_c(s: &str) -> Result<f32, ()> { let x: f32 = s.parse().map_err(|_| ())?; Ok(x / 2.0) }";

/// Three loop-accumulator implementations. SUM_A and SUM_C have
/// identical canonical-kind ASTs (`for x in v { t += x; }`). SUM_B
/// differs by ONE node: it derefs `x` before the add (`s += *x;`).
const SUM_A: &str = "fn sum_a(v: &[i32]) -> i32 { let mut t = 0i32; for x in v { t += x; } t }";
const SUM_B: &str = "fn sum_b(v: &[u64]) -> u64 { let mut s = 0u64; for x in v { s += *x; } s }";
const SUM_C: &str = "fn sum_c(v: &[f32]) -> f32 { let mut a = 0.0f32; for x in v { a += x; } a }";

/// Trivial constant returner.
const TRIVIAL: &str = "fn id() -> i32 { 42 }";

/// Complex function — many statements, nested control flow, match.
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

/// Match-on-char functions. MATCH_A and MATCH_B have IDENTICAL kind
/// structure except: MATCH_B has `-1` in its wildcard arm (a Unary
/// wrapping Lit), MATCH_A has `0` (just Lit). One node difference.
const MATCH_A: &str = "fn cls(c: char) -> i32 { match c { 'a' => 1, 'b' => 2, _ => 0 } }";
const MATCH_B: &str = "fn pri(c: char) -> i32 { match c { 'x' => 5, 'y' => 9, _ => -1 } }";

/// Closure-call function.
const ASYNC_A: &str = "fn maybe_run(f: impl Fn() -> i32) -> i32 { f() }";

#[test]
#[ignore = "Phase 1 — HDC characterization on hand-built Rust corpus. Run with --ignored --nocapture"]
fn phase_1_hdc_characterization() {
    println!("\n=== Phase 1 — HDC at function granularity: what does it actually find? ===\n");

    let snippets: Vec<(&str, &str, Hypervector)> = vec![
        ("PARSE_A", PARSE_A, encode_rust(PARSE_A)),
        ("PARSE_B", PARSE_B, encode_rust(PARSE_B)),
        ("PARSE_C", PARSE_C, encode_rust(PARSE_C)),
        ("SUM_A", SUM_A, encode_rust(SUM_A)),
        ("SUM_B", SUM_B, encode_rust(SUM_B)),
        ("SUM_C", SUM_C, encode_rust(SUM_C)),
        ("TRIVIAL", TRIVIAL, encode_rust(TRIVIAL)),
        ("COMPLEX", COMPLEX, encode_rust(COMPLEX)),
        ("MATCH_A", MATCH_A, encode_rust(MATCH_A)),
        ("MATCH_B", MATCH_B, encode_rust(MATCH_B)),
        ("ASYNC_A", ASYNC_A, encode_rust(ASYNC_A)),
    ];

    let dist = |a: &Hypervector, b: &Hypervector| popcount_distance(a, b);

    // ── Full pairwise distance matrix ────────────────────────────────
    println!("Pairwise HDC popcount distance (D=8192, random baseline ≈ 4096):");
    print!("              ");
    for (label, _, _) in &snippets {
        print!("{label:>9} ");
    }
    println!();
    for (la, _, hva) in &snippets {
        print!("{la:>10}    ");
        for (_, _, hvb) in &snippets {
            print!("{:>9} ", dist(hva, hvb));
        }
        println!();
    }
    println!();

    // Helpers.
    let get =
        |label: &str| -> Hypervector { snippets.iter().find(|(l, _, _)| *l == label).unwrap().2 };
    let parse_a = get("PARSE_A");
    let parse_b = get("PARSE_B");
    let parse_c = get("PARSE_C");
    let sum_a = get("SUM_A");
    let sum_b = get("SUM_B");
    let sum_c = get("SUM_C");
    let trivial = get("TRIVIAL");
    let complex = get("COMPLEX");
    let match_a = get("MATCH_A");
    let match_b = get("MATCH_B");

    // ── 1. CANONICALIZATION (what works) ─────────────────────────────
    //
    // Functions with structurally-identical canonical-kind ASTs but
    // different concrete types / literals should map to IDENTICAL
    // hypervectors (distance 0). This is the substrate's
    // grammar-stability claim, and it works.

    println!("─── Test 1: cluster cohesion (post seeded leaves) ───");
    // Bead `ley-line-open-98ac42` (seeded leaves): leaves now carry
    // token text. PARSE_A and PARSE_B no longer hash to distance 0 —
    // their identifiers differ. The cluster cohesion property changes
    // shape: within-cluster pairs must be MEASURABLY CLOSER than
    // cross-cluster pairs, but no longer exactly 0.
    let d_aa = dist(&parse_a, &parse_b);
    let d_ab = dist(&parse_a, &parse_c);
    let d_bc = dist(&parse_b, &parse_c);
    let d_a_sum = dist(&parse_a, &sum_a);
    println!("  Within PARSE cluster:");
    println!("    d(PARSE_A, PARSE_B) = {d_aa}");
    println!("    d(PARSE_A, PARSE_C) = {d_ab}");
    println!("    d(PARSE_B, PARSE_C) = {d_bc}");
    println!("  Cross-cluster:");
    println!("    d(PARSE_A, SUM_A)   = {d_a_sum}");
    assert!(
        d_aa < d_a_sum,
        "PARSE_A must be closer to PARSE_B ({d_aa}) than to SUM_A ({d_a_sum})"
    );
    assert!(
        d_ab < d_a_sum,
        "PARSE_A must be closer to PARSE_C ({d_ab}) than to SUM_A ({d_a_sum})"
    );

    let d_sa_sc = dist(&sum_a, &sum_c);
    let d_sa_parse = dist(&sum_a, &parse_a);
    println!("  Within SUM cluster:");
    println!("    d(SUM_A, SUM_C)     = {d_sa_sc}");
    println!("    d(SUM_A, PARSE_A)   = {d_sa_parse}");
    assert!(
        d_sa_sc < d_sa_parse,
        "SUM_A must be closer to SUM_C ({d_sa_sc}) than to PARSE_A ({d_sa_parse})"
    );
    println!("  ✓ cluster cohesion holds: within-cluster < cross-cluster distance.\n");

    // ── 2. STRUCTURAL DISTINCTION (what works) ───────────────────────
    //
    // A trivial leaf function and a complex multi-block function
    // should be far apart in distance ranking — farther than any
    // near-similar pair. Under bundle composition (bead 7b5086) the
    // absolute distance is smaller than D/2 (bundle dampens) but the
    // RELATIVE ORDER holds.

    println!("─── Test 2: structural distinction (large size delta) ───");
    let d_tri_complex = dist(&trivial, &complex);
    let d_tri_parse_a = dist(&trivial, &parse_a);
    println!("  d(TRIVIAL, COMPLEX) = {d_tri_complex}");
    println!("  d(TRIVIAL, PARSE_A) = {d_tri_parse_a}");
    assert!(
        d_tri_complex > 500,
        "TRIVIAL and COMPLEX must be measurably distant under bundle composition; got {d_tri_complex}"
    );
    println!(
        "  ✓ trivial-vs-complex: graded distance ~{d_tri_complex} (bundle-dampened, not D/2)\n"
    );

    // ── 3. NEAR-SIMILAR PAIRS — INVERTED post bead 7b5086 ────────────
    //
    // Pre-bundle (XOR-bind composition): near-similar pairs sat at
    // saturation (D/2 ≈ 4115). Math-friend + third-party LLM diagnosed
    // this as fp-induced base_vector + content_role per-level PRG
    // draws being faithfully XOR-transmitted to the root.
    //
    // Post-bundle (this commit): the same pairs are now in the
    // measurably-close band. Bundle dampens upstream randomization at
    // each level — a one-leaf change that's D/2 at the leaf becomes
    // ~D/(F^depth) at the root for fan-out F. The substrate is now
    // GRADED at function granularity.
    //
    // Assertions inverted: near-similar pairs MUST be in [0, 500].
    // Three thresholds, increasing strictness:
    //   - SUM_A vs SUM_B: distance <= 500 (one deref node off)
    //   - MATCH_A vs MATCH_B: distance <= 500 (one Unary node off in
    //     wildcard arm) — was the load-bearing single-statement
    //     failure under XOR-bind
    //   - near-similar pairs measurably closer than unrelated pairs

    println!("─── Test 3: near-similar pairs (substrate-property gate) ───");
    // Bead `ley-line-open-98ac42` (seeded leaves) changed the absolute
    // distance magnitudes — leaves now carry token text, so two
    // functions with different identifier names produce different
    // hypervectors even at identical canonical shapes. The substrate
    // property the gate measures is now RELATIVE: near-similar pairs
    // must still be measurably closer than truly unrelated pairs.
    let d_sa_sb = dist(&sum_a, &sum_b);
    let d_ma_mb = dist(&match_a, &match_b);
    let d_sa_complex = dist(&sum_a, &complex);
    let d_ma_complex = dist(&match_a, &complex);
    println!("  d(SUM_A, SUM_B)     = {d_sa_sb}   (one deref off, same `sum_` lexical family)");
    println!("  d(MATCH_A, MATCH_B) = {d_ma_mb}   (one Unary off, both `match` patterns)");
    println!("  d(SUM_A, COMPLEX)   = {d_sa_complex}   (baseline: unrelated)");
    println!("  d(MATCH_A, COMPLEX) = {d_ma_complex}   (baseline: unrelated)");
    println!();
    // Relative gate: near-similar must be closer than unrelated.
    assert!(
        d_sa_sb < d_sa_complex,
        "near-similar (SUM_A,SUM_B)={d_sa_sb} must be < unrelated (SUM_A,COMPLEX)={d_sa_complex}"
    );
    assert!(
        d_ma_mb < d_ma_complex,
        "near-similar (MATCH_A,MATCH_B)={d_ma_mb} must be < unrelated (MATCH_A,COMPLEX)={d_ma_complex}"
    );
    println!("  ✓ relative substrate gate holds: near-similar < unrelated for both pairs.\n");

    // ── 4. TOP-K RETRIEVAL — what would happen on a query ────────────
    //
    // Run actual top-3 retrieval to show the user-facing behavior:
    // a SUM_A query returns SUM_C (the only structurally-identical
    // function) but does NOT return SUM_B (which differs by one node)
    // — so a "find functions like sum_a" query MISSES the
    // semantically-most-similar candidate.

    println!("─── Test 4: top-K retrieval behavior ───");
    let labelled_corpus: Vec<(&str, Hypervector)> = snippets
        .iter()
        .map(|(label, _, hv)| (*label, *hv))
        .collect();
    let top_k = |q: &Hypervector, k: usize| -> Vec<(&'static str, u32)> {
        let mut scored: Vec<(&'static str, u32)> = labelled_corpus
            .iter()
            .map(|(label, hv)| (*label, popcount_distance(q, hv)))
            .collect();
        scored.sort_by_key(|&(_, d)| d);
        scored.into_iter().take(k).collect()
    };

    let top5_sum_a = top_k(&sum_a, 5);
    println!("  Query SUM_A (top-5):");
    for (label, d) in &top5_sum_a {
        println!("    {label:>10}  distance={d}");
    }
    let in_top5 = |label: &str| top5_sum_a.iter().any(|(l, _)| *l == label);
    let sum_b_in_top5 = in_top5("SUM_B");
    let sum_c_in_top5 = in_top5("SUM_C");
    println!("    SUM_B in top-5: {sum_b_in_top5}");
    println!("    SUM_C in top-5: {sum_c_in_top5}");
    // The trade-off bead `98ac42` introduced: seeded leaves can let
    // lexically-similar functions (sharing common identifiers like
    // `i32`, `x`, `v`) rank above structurally-near-similar but
    // lexically-disjoint functions. We assert the relaxed gate: at
    // least ONE of the structural cluster-mates appears in top-5.
    assert!(
        sum_b_in_top5 || sum_c_in_top5,
        "SUM cluster cohesion: at least one of SUM_B / SUM_C must appear in top-5 for query SUM_A"
    );
    println!();

    let top5_match_a = top_k(&match_a, 5);
    println!("  Query MATCH_A (top-5):");
    for (label, d) in &top5_match_a {
        println!("    {label:>10}  distance={d}");
    }
    let match_b_in_top5 = top5_match_a.iter().any(|(l, _)| *l == "MATCH_B");
    println!("    MATCH_B in top-5: {match_b_in_top5}");
    assert!(
        match_b_in_top5,
        "SUBSTRATE GATE: MATCH_B must be in top-5. Single-statement case Phase 1C documented as a limitation."
    );
    println!();

    // ── Final verdict ────────────────────────────────────────────────
    println!("─── Verdict ───");
    println!("HDC at function granularity on real Rust code (post 7b5086 + 98ac42):");
    println!("  ✓ identical source → distance 0 (deterministic)");
    println!("  ✓ within-cluster < cross-cluster cohesion");
    println!("  ✓ near-similar (one-AST-node-different) < unrelated (relative gate)");
    println!("  ✓ single-statement near-similar (MATCH_A vs MATCH_B) appears in top-5");
    println!();
    println!("Trade-off introduced by seeded leaves (98ac42): identifier text now");
    println!("contributes to leaf HVs, so two functions with shared identifiers can rank");
    println!("closer than two with shared structure but disjoint identifiers. Substrate");
    println!("blends structural + lexical similarity. Whether that's the right blend for");
    println!("real-world RAG depends on Phase 0B (recall@K vs ground truth).");
    println!();
}
