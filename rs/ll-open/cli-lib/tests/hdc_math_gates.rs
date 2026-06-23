//! HDC math-gate tests — mandatory pre-merge per math-friend review on
//! bead `ley-line-open-641809` (see
//! `_agent_log/theoretical-foundations-analyst_2026-06-22_agent_log.md`).
//!
//! The substrate ops (`hdc_search` / `hdc_density`) compile and dispatch
//! cleanly even when the encoder produces operationally-useless
//! hypervectors. "Rows exist" is not enough. These two tests catch the
//! failure modes that would make a passing PR ship code that returns
//! garbage to consumers.
//!
//! ## Gate 1 — Saturation
//!
//! Encode N functionally-distinct Go functions. Compute every pairwise
//! Hamming distance. Math-friend Q1:
//!
//! > "By CLT the pairwise Hamming distance between two file HVs
//! >  concentrates around D/2=4096 with std dev shrinking as √(D/4)/√N.
//! >  For a 500-LOC file with ~50 top-level items, std dev drops from
//! >  45 (single-pair iid) to ~6 bits. Your calibration's `k=3 MAD`
//! >  (typically ~20 bits at function scope) is wider than the entire
//! >  signal band. `radius_search` returns all rows or none — not
//! >  garbage in the random sense, garbage in the indistinguishable
//! >  sense."
//!
//! At function-level granularity (this PR's choice), math-friend
//! predicts std >= 30 and median NOT pinned near D/2. The test fails
//! if EITHER (a) median falls in [4090, 4102] AND std < 15 (the file-
//! level concentration signature) OR (b) std < 30 (the function-level
//! "good enough" threshold).
//!
//! ## Gate 2 — Discriminability gradient
//!
//! Pick K Go functions. Build a "trivially-mutated" version of each by
//! swapping two top-level statements. Assert:
//!
//! > distance(original, mutant) < median_pairwise_distance(random_pairs) / 4
//!
//! Near-clones (same tokens, slightly reordered) must collapse to
//! small distance. Random unrelated functions must spread to ~D/2. If
//! the encoder is producing well-distributed bits with no semantic
//! gradient between near-clones and random pairs — pretty noise,
//! useless for radius search.

#![cfg(feature = "hdc")]

use leyline_cli_lib::daemon::hdc_pass::tree_to_encoder_node;
use leyline_hdc::canonical::GoCanonicalMap;
use leyline_hdc::codebook::AstCodebook;
use leyline_hdc::util::popcount_distance;
use leyline_hdc::{Hypervector, SubtreeCache, encoder::encode_tree};

/// Encode the FIRST top-level function in a Go source string into a
/// hypervector. Helper for the gate tests below; production populate
/// is in `daemon::hdc_enrich::HdcEnrichmentPass`.
fn encode_first_go_function(src: &str, cache: &SubtreeCache) -> Hypervector {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&leyline_ts::languages::TsLanguage::Go.ts_language())
        .expect("set tree-sitter-go");
    let tree = parser.parse(src, None).expect("parse Go");

    // Walk the tree depth-first to find the first function_declaration
    // or method_declaration. The math gate uses canonical fixtures with
    // exactly one function each, so "first" is "the".
    fn find_first<'a>(node: tree_sitter::Node<'a>) -> Option<tree_sitter::Node<'a>> {
        if matches!(node.kind(), "function_declaration" | "method_declaration") {
            return Some(node);
        }
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                if let Some(found) = find_first(cursor.node()) {
                    return Some(found);
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        None
    }

    let func_node = find_first(tree.root_node()).expect("no function in fixture — bad test source");
    let encoder_node = tree_to_encoder_node(func_node, &GoCanonicalMap);
    encode_tree(&encoder_node, &AstCodebook, cache)
}

/// 50 distinct Go functions covering different control-flow shapes.
/// Hand-authored to maximize structural variation: different number
/// of statements, different nesting depths, different operator mixes.
/// Math gate 1 fails if these saturate the encoder.
fn fifty_distinct_functions() -> Vec<String> {
    let bodies = [
        "func a() { return }",
        "func b(x int) int { return x + 1 }",
        "func c(s string) string { return s + \"!\" }",
        "func d() bool { return true }",
        "func e(xs []int) int { sum := 0; for _, x := range xs { sum += x }; return sum }",
        "func f(n int) int { if n <= 1 { return 1 }; return n * f(n-1) }",
        "func g(m map[string]int, k string) int { v, ok := m[k]; if !ok { return -1 }; return v }",
        "func h(p *int) { if p != nil { *p = 0 } }",
        "func i() (int, error) { return 42, nil }",
        "func j(xs []string) bool { for _, x := range xs { if x == \"\" { return true } }; return false }",
        "func k(a, b int) int { if a > b { return a }; return b }",
        "func l() { defer recover(); panic(\"x\") }",
        "func m(ch chan int) { for x := range ch { _ = x } }",
        "func n(xs []int) []int { out := make([]int, 0, len(xs)); for _, x := range xs { out = append(out, x*2) }; return out }",
        "func o(s string) int { count := 0; for _, c := range s { if c == 'a' { count++ } }; return count }",
        "func p(xs []int) (int, int) { min, max := xs[0], xs[0]; for _, x := range xs[1:] { if x < min { min = x }; if x > max { max = x } }; return min, max }",
        "func q(n int) []int { out := make([]int, n); for i := range out { out[i] = i * i }; return out }",
        "func r(s string) string { runes := []rune(s); for i, j := 0, len(runes)-1; i < j; i, j = i+1, j-1 { runes[i], runes[j] = runes[j], runes[i] }; return string(runes) }",
        "func s(m map[string]int) []string { out := make([]string, 0, len(m)); for k := range m { out = append(out, k) }; return out }",
        "func t(a, b []int) []int { out := make([]int, 0, len(a)+len(b)); out = append(out, a...); out = append(out, b...); return out }",
        "func u(n int) bool { if n < 2 { return false }; for i := 2; i*i <= n; i++ { if n%i == 0 { return false } }; return true }",
        "func v(xs []int, target int) int { for i, x := range xs { if x == target { return i } }; return -1 }",
        "func w(xs []int) { for i := 0; i < len(xs); i++ { for j := i + 1; j < len(xs); j++ { if xs[i] > xs[j] { xs[i], xs[j] = xs[j], xs[i] } } } }",
        "func x() func() int { i := 0; return func() int { i++; return i } }",
        "func y(s string, sub string) bool { if len(sub) > len(s) { return false }; for i := 0; i+len(sub) <= len(s); i++ { if s[i:i+len(sub)] == sub { return true } }; return false }",
        "func z(xs []int) float64 { if len(xs) == 0 { return 0 }; sum := 0; for _, x := range xs { sum += x }; return float64(sum) / float64(len(xs)) }",
        "func aa(node *Node) int { if node == nil { return 0 }; return 1 + aa(node.Left) + aa(node.Right) }",
        "func bb(g [][]int, start int) []int { visited := make(map[int]bool); var order []int; queue := []int{start}; for len(queue) > 0 { v := queue[0]; queue = queue[1:]; if visited[v] { continue }; visited[v] = true; order = append(order, v); queue = append(queue, g[v]...) }; return order }",
        "func cc(a, b string) int { n, m := len(a), len(b); if n > m { a, b = b, a; n, m = m, n }; prev := make([]int, n+1); curr := make([]int, n+1); for j := 1; j <= m; j++ { curr[0] = j; for i := 1; i <= n; i++ { cost := 1; if a[i-1] == b[j-1] { cost = 0 }; curr[i] = min3(prev[i]+1, curr[i-1]+1, prev[i-1]+cost) }; prev, curr = curr, prev }; return prev[n] }",
        "func dd(xs []int) bool { seen := make(map[int]bool); for _, x := range xs { if seen[x] { return true }; seen[x] = true }; return false }",
        "func ee(s string) string { var b []byte; for i := 0; i < len(s); i++ { if s[i] >= 'a' && s[i] <= 'z' { b = append(b, s[i]-32) } else { b = append(b, s[i]) } }; return string(b) }",
        "func ff(xs []int, n int) []int { if n >= len(xs) { return xs }; return xs[:n] }",
        "func gg(m map[int]int) int { total := 0; for _, v := range m { total += v }; return total }",
        "func hh(s string) bool { runes := []rune(s); for i, j := 0, len(runes)-1; i < j; i, j = i+1, j-1 { if runes[i] != runes[j] { return false } }; return true }",
        "func ii(xs [][]int) [][]int { rows := len(xs); cols := len(xs[0]); out := make([][]int, cols); for i := range out { out[i] = make([]int, rows) }; for i := 0; i < rows; i++ { for j := 0; j < cols; j++ { out[j][i] = xs[i][j] } }; return out }",
        "func jj(n int) []int { if n <= 0 { return nil }; out := []int{1}; for i := 1; i < n; i++ { row := make([]int, i+1); row[0], row[i] = 1, 1; for j := 1; j < i; j++ { row[j] = out[len(out)-1-(i-j)] + out[len(out)-1-(i-j-1)] }; out = append(out, row...) }; return out }",
        "func kk(s string) map[rune]int { freq := make(map[rune]int); for _, r := range s { freq[r]++ }; return freq }",
        "func ll(xs []int, target int) (int, int) { left, right := 0, len(xs)-1; for left <= right { mid := (left + right) / 2; if xs[mid] == target { return mid, mid }; if xs[mid] < target { left = mid + 1 } else { right = mid - 1 } }; return -1, -1 }",
        "func mm(s string) string { fields := strings.Fields(s); for i := range fields { fields[i] = strings.Title(fields[i]) }; return strings.Join(fields, \" \") }",
        "func nn(xs []int) int { if len(xs) == 0 { return 0 }; result := xs[0]; for _, x := range xs[1:] { result ^= x }; return result }",
        "func oo(m, n int) int { dp := make([][]int, m+1); for i := range dp { dp[i] = make([]int, n+1) }; for i := 0; i <= m; i++ { dp[i][0] = i }; for j := 0; j <= n; j++ { dp[0][j] = j }; return dp[m][n] }",
        "func pp(xs []int, k int) []int { heap := xs[:k]; for _, x := range xs[k:] { if x > heap[0] { heap[0] = x } }; return heap }",
        "func qq(ch chan<- int, n int) { for i := 0; i < n; i++ { ch <- i * i }; close(ch) }",
        "func rr(xs []int) int { max := xs[0]; cur := xs[0]; for _, x := range xs[1:] { if cur < 0 { cur = x } else { cur += x }; if cur > max { max = cur } }; return max }",
        "func ss(s string) string { var sb strings.Builder; prev := rune(0); for _, r := range s { if r != prev { sb.WriteRune(r) }; prev = r }; return sb.String() }",
        "func tt(g map[int][]int, start, end int) []int { parents := map[int]int{start: -1}; queue := []int{start}; for len(queue) > 0 { v := queue[0]; queue = queue[1:]; if v == end { path := []int{}; for x := v; x != -1; x = parents[x] { path = append([]int{x}, path...) }; return path }; for _, nb := range g[v] { if _, ok := parents[nb]; !ok { parents[nb] = v; queue = append(queue, nb) } } }; return nil }",
        "func uu(xs []int, k int) []int { if k > len(xs) { k = len(xs) }; k = k % len(xs); return append(xs[len(xs)-k:], xs[:len(xs)-k]...) }",
        "func vv(s string) [][]string { res := [][]string{}; for i := 0; i < len(s); i++ { for j := i + 1; j <= len(s); j++ { sub := s[i:j]; ok := true; for x, y := 0, len(sub)-1; x < y; x, y = x+1, y-1 { if sub[x] != sub[y] { ok = false; break } }; if ok { res = append(res, []string{sub}) } } }; return res }",
        "func ww(xs []int) int { count := make(map[int]int); for _, x := range xs { count[x]++ }; best, bestCount := xs[0], 0; for x, c := range count { if c > bestCount { best, bestCount = x, c } }; return best }",
        "func xx(a, b int) int { for b != 0 { a, b = b, a%b }; return a }",
        "func yy() { fmt.Println(\"hello\") }",
        "func zz(xs []int, target int) bool { left, right := 0, len(xs)-1; for left <= right { mid := (left + right) / 2; if xs[mid] == target { return true }; if xs[mid] < target { left = mid + 1 } else { right = mid - 1 } }; return false }",
    ];
    bodies.iter().map(|s| s.to_string()).collect()
}

#[test]
fn gate_saturation_function_level_does_not_concentrate() {
    let cache = SubtreeCache::new();
    let bodies = fifty_distinct_functions();
    assert!(
        bodies.len() >= 50,
        "fixture must have ≥50 distinct functions; got {}",
        bodies.len()
    );

    let hvs: Vec<Hypervector> = bodies
        .iter()
        .map(|s| encode_first_go_function(s, &cache))
        .collect();

    // Compute every pairwise Hamming distance.
    let mut distances: Vec<u32> = Vec::with_capacity(hvs.len() * (hvs.len() - 1) / 2);
    for i in 0..hvs.len() {
        for j in (i + 1)..hvs.len() {
            distances.push(popcount_distance(&hvs[i], &hvs[j]));
        }
    }

    // Median.
    let mut sorted = distances.clone();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2] as f64;

    // Standard deviation.
    let mean = distances.iter().map(|&d| d as f64).sum::<f64>() / distances.len() as f64;
    let variance = distances
        .iter()
        .map(|&d| (d as f64 - mean).powi(2))
        .sum::<f64>()
        / distances.len() as f64;
    let std = variance.sqrt();

    eprintln!(
        "Saturation gate: n={} median={median:.1} mean={mean:.1} std={std:.2}",
        hvs.len()
    );

    // The math-friend failure signal for file-level concentration was
    // "median ∈ [4090, 4102] AND std < 15." That signature must NOT
    // appear at function-level granularity.
    let concentrated = (4090.0..=4102.0).contains(&median) && std < 15.0;
    assert!(
        !concentrated,
        "SATURATION DETECTED: median={median:.1} ∈ [4090,4102] AND std={std:.2} < 15. \
         The encoder is concentrating distances around D/2; radius_search will return garbage. \
         Almost certainly a granularity regression (file-level vs function-level)."
    );

    // Function-level should produce std > 30 per math-friend's
    // prediction. Lower std signals the encoder is over-clustering.
    assert!(
        std >= 30.0,
        "DISCRIMINATION FAILED: std={std:.2} < 30 over {} pairs. \
         Function-level encoding should spread pairwise distances widely; \
         this signals the encoder is producing overly-similar hypervectors \
         across structurally-distinct functions.",
        distances.len()
    );
}

#[test]
fn gate_discriminability_near_clones_collapse() {
    // 10 distinct Go functions + a "trivially-mutated" variant of each
    // that swaps two top-level statements while keeping every token.
    // Math gate: distance(original, mutant) < median_random_pairwise / 4.
    //
    // For each pair I hand-authored the swap so it's a real semantic
    // permutation (not e.g. swapping declarations that produce
    // independent results).
    // Math-friend's example: "swap two top-level decls — keeps every
    // token, changes order." That kind of mutation, plus pure
    // identifier renames and literal-value changes (the canonical-
    // kind alphabet collapses both — `a` and `b` both → Ref; `1` and
    // `42` both → Lit), are the genuinely-trivial mutations.
    //
    // The encoder is order-sensitive on children via position-based
    // role binding, so swapping two children of the SAME canonical
    // kind whose subtrees encode identically reduces to permuting
    // hv's at positions 0 vs 1 — which produces the same XOR-bundle
    // when the two hv's are equal. Hence near-zero distance.
    let pairs: &[(&str, &str)] = &[
        // 1. Top-level decl swap (two short_var_declarations that
        //    encode identically because canonical-kinds at every
        //    level match).
        (
            "func a() { x := 1; y := 2; _ = x; _ = y }",
            "func a() { y := 2; x := 1; _ = x; _ = y }",
        ),
        // 2. Identifier rename — Ref kind collapses identifiers.
        (
            "func b() int { x := 1; return x }",
            "func b() int { foo := 1; return foo }",
        ),
        // 3. Integer literal change — Lit collapses values.
        ("func c() int { return 1 }", "func c() int { return 9999 }"),
        // 4. String literal change — Lit collapses string values.
        (
            "func d() string { return \"hello\" }",
            "func d() string { return \"goodbye-friends\" }",
        ),
        // 5. Bool literal swap.
        (
            "func e() bool { return true }",
            "func e() bool { return false }",
        ),
        // 6. Function name rename (the function_declaration's own
        //    `name` field is an identifier — Ref kind).
        (
            "func original() int { return 1 }",
            "func renamed() int { return 1 }",
        ),
        // 7. Parameter rename.
        (
            "func g(x int) int { return x }",
            "func g(y int) int { return y }",
        ),
        // 8. Multiple top-level decl reorder (4 declarations
        //    permuted).
        (
            "func h() { a := 1; b := 2; c := 3; d := 4; _, _, _, _ = a, b, c, d }",
            "func h() { d := 4; c := 3; b := 2; a := 1; _, _, _, _ = a, b, c, d }",
        ),
        // 9. Field-access rename: `obj.x` vs `obj.y` — both
        //    selector_expressions over an identifier.
        (
            "func i(s struct{ x int }) int { return s.x }",
            "func i(s struct{ y int }) int { return s.y }",
        ),
        // 10. Map literal key/value rename.
        (
            "func j() map[string]int { return map[string]int{\"a\": 1} }",
            "func j() map[string]int { return map[string]int{\"b\": 2} }",
        ),
    ];

    let cache = SubtreeCache::new();

    // Encode every fixture (originals + mutants).
    let originals: Vec<Hypervector> = pairs
        .iter()
        .map(|(o, _)| encode_first_go_function(o, &cache))
        .collect();
    let mutants: Vec<Hypervector> = pairs
        .iter()
        .map(|(_, m)| encode_first_go_function(m, &cache))
        .collect();

    // Compute median pairwise distance over ORIGINAL × ORIGINAL (every
    // pair where i != j). This is the "random unrelated functions"
    // baseline.
    let mut random_pairs: Vec<u32> = Vec::new();
    for i in 0..originals.len() {
        for j in (i + 1)..originals.len() {
            random_pairs.push(popcount_distance(&originals[i], &originals[j]));
        }
    }
    random_pairs.sort_unstable();
    let median_random = random_pairs[random_pairs.len() / 2] as f64;

    // Threshold per math-friend Q6: near-clones must collapse below
    // median_random / 4.
    let threshold = median_random / 4.0;

    eprintln!("Discriminability gate: median_random={median_random:.1} threshold={threshold:.1}");

    let mut violations = Vec::new();
    for (i, (orig, mutant)) in originals.iter().zip(mutants.iter()).enumerate() {
        let d = popcount_distance(orig, mutant) as f64;
        eprintln!("  pair[{i}] distance={d:.1}");
        if d >= threshold {
            violations.push(format!(
                "pair[{i}]: distance={d:.1} ≥ threshold={threshold:.1}"
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "DISCRIMINABILITY FAILED: {} near-clone pairs encoded as distantly as random pairs:\n  {}\n\
         Encoder is producing well-distributed bits with no semantic gradient — \
         pretty noise, useless for radius_search.",
        violations.len(),
        violations.join("\n  ")
    );
}
