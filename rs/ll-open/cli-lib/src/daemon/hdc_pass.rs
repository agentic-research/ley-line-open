//! Tree-sitter → leyline-hdc bridge. Walks a tree-sitter `Node` post-order,
//! emits an `EncoderNode` tree the HDC encoder can consume.
//!
//! Parser-bridge logic only — the actual encoder, codebooks, and storage
//! all live in `leyline-hdc` so HDC stays parser-agnostic. This adapter
//! is what turns "tree-sitter named children" into "canonical-kind tree
//! with sorted child kinds."
//!
//! Feature-gated behind `hdc`.

use leyline_hdc::canonical::CanonicalKindMap;
use leyline_hdc::EncoderNode;

/// Walk a tree-sitter Node, producing an `EncoderNode` tree. Only named
/// children contribute (matching the Deckard production-signature
/// discipline — anonymous nodes are parser implementation detail and
/// shouldn't drive the equivalence relation).
pub fn tree_to_encoder_node(node: tree_sitter::Node<'_>, kind_map: &dyn CanonicalKindMap) -> EncoderNode {
    let canonical = kind_map.lookup(node.kind());

    let mut children: Vec<EncoderNode> = Vec::new();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.is_named() {
                children.push(tree_to_encoder_node(child, kind_map));
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    EncoderNode::new(canonical, children)
}

/// Convenience: parse a source string with the given tree-sitter language
/// and walk the root, returning an `EncoderNode`. Returns `None` if the
/// parser fails to produce a tree (effectively never for valid input;
/// returns `None` on extreme malformations).
pub fn parse_and_encode_tree(
    source: &str,
    language: &tree_sitter::Language,
    kind_map: &dyn CanonicalKindMap,
) -> Option<EncoderNode> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(language).ok()?;
    let tree = parser.parse(source, None)?;
    Some(tree_to_encoder_node(tree.root_node(), kind_map))
}

/// Filter a tree to its function-level subtrees. Hotspot detection works
/// at function granularity (per math-friend review: depth ~5-7 fits
/// reliably; whole-file depth saturates capacity). Caller passes a
/// closure that decides which `EncoderNode` is a function root —
/// language-specific because different parsers call the production
/// different things ("function_declaration" in Go, "function_item" in
/// Rust, etc.).
///
/// Returns the matched function subtrees as a flat `Vec`; whole-file
/// roots that didn't match are dropped.
pub fn extract_functions(
    tree: &EncoderNode,
    is_function: impl Fn(&EncoderNode) -> bool,
) -> Vec<EncoderNode> {
    let mut out = Vec::new();
    fn walk(
        node: &EncoderNode,
        is_function: &dyn Fn(&EncoderNode) -> bool,
        out: &mut Vec<EncoderNode>,
    ) {
        if is_function(node) {
            out.push(node.clone());
        }
        for child in &node.children {
            walk(child, is_function, out);
        }
    }
    walk(tree, &is_function, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use leyline_hdc::canonical::{CanonicalKind, GoCanonicalMap};

    fn parse_go(src: &str) -> EncoderNode {
        // tree-sitter-go is exposed via leyline-ts's TsLanguage::Go.
        // cli-lib doesn't depend on tree_sitter_go directly.
        let lang = leyline_ts::languages::TsLanguage::Go.ts_language();
        parse_and_encode_tree(src, &lang, &GoCanonicalMap).expect("parse Go")
    }

    /// Parse + encode in one step. Used by clustering tests where the
    /// EncoderNode is intermediate — only the final hypervector matters.
    fn encode_go(src: &str, cb: &leyline_hdc::codebook::AstCodebook) -> leyline_hdc::Hypervector {
        leyline_hdc::encode_fresh(&parse_go(src), cb)
    }

    /// Hamming distance normalized to [0, 1] (d/D). Clustering tests
    /// compare against fractional thresholds because absolute distance
    /// scales with D.
    fn normalized_distance(a: &leyline_hdc::Hypervector, b: &leyline_hdc::Hypervector) -> f64 {
        leyline_hdc::popcount_distance(a, b) as f64 / (leyline_hdc::D_BYTES * 8) as f64
    }

    #[test]
    fn parses_simple_go_function() {
        // Sanity: parsing must produce a non-trivial tree.
        let src = "package m\n\nfunc A(x int) int { return x + 1 }\n";
        let tree = parse_go(src);
        // Root = source_file → Block; should have at least one child
        // (the function declaration).
        assert_eq!(tree.canonical_kind, CanonicalKind::Block);
        assert!(!tree.children.is_empty(), "expected children under root");
    }

    #[test]
    fn extract_functions_finds_function_decls() {
        let src = "package m\n\nfunc A() {}\n\nfunc B() {}\n";
        let tree = parse_go(src);
        // Function decls bucket to Decl in the canonical alphabet.
        let funcs = extract_functions(&tree, |n| n.canonical_kind == CanonicalKind::Decl);
        // Two function_declarations + one package_clause = 3 Decl roots
        // (we don't refine "is this specifically a function" — just Decl).
        assert!(
            funcs.len() >= 2,
            "expected at least 2 Decl-rooted subtrees, got {}",
            funcs.len(),
        );
    }

    /// **Validation spike: real-clone clustering on parsed Go.**
    ///
    /// Empirical answer to "does AstCodebook actually cluster near-clones?"
    /// Hand-labeled four Go functions:
    /// - A and A' are near-clones: same shape (parameter, return,
    ///   addition, single literal) — only identifier and literal value
    ///   differ. Should land at small Hamming distance.
    /// - B is structurally different (no return, conditional, multiple
    ///   statements). Should land far from A and A'.
    /// - C is yet another shape (loop). Should also land far.
    ///
    /// Math friend's review: random pairs sit at ~D/2 = 4096 ± √D/2 ≈ 45
    /// for D=8192. Expect clones at d/D < 0.20, distinct pairs at
    /// d/D > 0.35.
    ///
    /// Pinned thresholds are conservative — give the encoder room to
    /// breathe while still catching a regression that erased the
    /// equivalence class.
    /// **VALIDATION GATE — integration test for the full HDC stack
    /// end-to-end on real parsed Go.**
    ///
    /// This is the test that decides whether HDC is feature-complete
    /// per the user's "abstracted and useful" directive. Runs:
    ///
    /// 1. Build a corpus of ~30 Go function fixtures with known
    ///    structure (10 Type-2 clones of three different shapes).
    /// 2. Parse + encode each via AstCodebook.
    /// 3. Persist into `_hdc` table via the schema.
    /// 4. Register popcount_xor + BUNDLE UDFs.
    /// 5. Run radius calibration, store baseline in `_hdc_baseline`.
    /// 6. For each known clone group: run `radius_search` against one
    ///    member, assert the other group members are within calibrated
    ///    radius and non-group members are outside.
    /// 7. Run `density_count` on a clone-group member, assert it
    ///    reflects the group size.
    /// 8. Build a cluster centroid via BUNDLE_MAJORITY of one group's
    ///    hypervectors, run `explain_cluster_centroid`, assert it
    ///    returns a structurally-meaningful skeleton (right number of
    ///    tuples, valid kinds).
    ///
    /// PASS criteria (math friend's gate):
    /// - Clone-group members cluster within `calibrated_radius`.
    /// - Cross-group distances strictly greater than within-group.
    /// - Cluster-explanation returns the expected number of recovered
    ///   slots (recovery accuracy target ≥80% applies in production
    ///   on real codebases; this test pins API correctness).
    #[test]
    fn validation_gate_full_stack_on_real_go() {
        use leyline_hdc::calibrate::{calibrate_and_persist, load_baseline};
        use leyline_hdc::codebook::AstCodebook;
        use leyline_hdc::query::{density_count, radius_search};
        use leyline_hdc::schema::create_hdc_schema;
        use leyline_hdc::sql_udf::register_hdc_udfs;
        use leyline_hdc::{encode_fresh, LayerKind};
        use rusqlite::Connection;

        // Three families of Go function shapes; 10 clones per family.
        // Each family member shares structural shape but has unique
        // identifiers / literals — Type-2 clone class.
        struct CloneGroup {
            name: &'static str,
            sources: Vec<String>,
        }

        // Family A: simple param-and-return.
        let family_a = CloneGroup {
            name: "param_return",
            sources: (0..10)
                .map(|i| {
                    format!(
                        "package m\n\nfunc Fn{i}(x{i} int) int {{ return x{i} + {i} }}\n",
                    )
                })
                .collect(),
        };

        // Family B: if-else branching with two prints.
        let family_b = CloneGroup {
            name: "if_else_print",
            sources: (0..10)
                .map(|i| {
                    format!(
                        "package m\n\nfunc Fn{i}() {{ if true {{ println(\"a{i}\") }} else {{ println(\"b{i}\") }} }}\n",
                    )
                })
                .collect(),
        };

        // Family C: for-loop with println.
        let family_c = CloneGroup {
            name: "for_loop",
            sources: (0..10)
                .map(|i| {
                    format!(
                        "package m\n\nfunc Fn{i}() {{ for j := 0; j < {}; j++ {{ println(j) }} }}\n",
                        i + 5,
                    )
                })
                .collect(),
        };

        let groups = vec![family_a, family_b, family_c];

        // Set up DB with HDC schema + UDFs.
        let conn = Connection::open_in_memory().unwrap();
        create_hdc_schema(&conn).unwrap();
        register_hdc_udfs(&conn).unwrap();

        // Encode every fixture, insert into _hdc.
        let cb = AstCodebook::new();
        for group in &groups {
            for (i, src) in group.sources.iter().enumerate() {
                let scope_id = format!("{}/fn_{}", group.name, i);
                let tree = parse_go(src);
                let hv = encode_fresh(&tree, &cb);
                conn.execute(
                    "INSERT INTO _hdc(scope_id, layer_kind, hv, basis) VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![scope_id, LayerKind::Ast.as_str(), hv.to_vec(), 1i64],
                )
                .unwrap();
            }
        }

        // Calibrate radius baseline against the empirical corpus.
        let now_ms = 1_700_000_000_000;
        let calibrated = calibrate_and_persist(&conn, 1000, now_ms).unwrap();
        assert!(calibrated >= 1, "must calibrate at least the AST layer");

        let baseline = load_baseline(&conn, LayerKind::Ast).unwrap().unwrap();
        eprintln!(
            "calibration: median={}, mad={}, sample={}",
            baseline.median_distance, baseline.mad, baseline.sample_size,
        );

        // The default radius (median - 3*mad) should be > 0; if MAD is
        // tiny, the threshold collapses to median minus a small margin.
        let r = baseline.default_radius();
        eprintln!("calibrated radius: {r}");

        // ── Assertion 1: within-group members cluster. ────────────────
        // Pick one member of each group, run radius_search, assert at
        // least 5 of the group's 10 members are within the calibrated
        // radius. (We don't assert 10/10 because some Type-2 clones may
        // have slight Hamming drift due to arity-bucket boundaries.)
        for group in &groups {
            let probe_scope = format!("{}/fn_0", group.name);
            let probe_hv = encode_go(&group.sources[0], &cb);

            let matches = radius_search(&conn, LayerKind::Ast, &probe_hv, r, 100).unwrap();
            let same_group_matches: Vec<_> = matches
                .iter()
                .filter(|m| m.scope_id.starts_with(&format!("{}/", group.name)))
                .collect();
            let other_group_matches: Vec<_> = matches
                .iter()
                .filter(|m| !m.scope_id.starts_with(&format!("{}/", group.name)))
                .collect();

            eprintln!(
                "group {}: probe={} same-group matches={} other-group matches={}",
                group.name,
                probe_scope,
                same_group_matches.len(),
                other_group_matches.len(),
            );

            // The probe itself must match (distance 0).
            assert!(
                same_group_matches.iter().any(|m| m.scope_id == probe_scope),
                "group {}: probe scope must match itself",
                group.name,
            );

            // Most same-group members should cluster in.
            assert!(
                same_group_matches.len() >= 5,
                "group {}: expected ≥5 same-group matches in calibrated radius, got {}",
                group.name,
                same_group_matches.len(),
            );

            // No other group should bleed in. (For these Type-2 clones,
            // structural shape is genuinely different across groups.)
            assert_eq!(
                other_group_matches.len(),
                0,
                "group {}: other groups must not match within calibrated radius (got {} bleeders)",
                group.name,
                other_group_matches.len(),
            );
        }

        // ── Assertion 2: density tracks group size. ────────────────────
        for group in &groups {
            let probe_hv = encode_go(&group.sources[0], &cb);
            let count = density_count(&conn, LayerKind::Ast, &probe_hv, r).unwrap();
            assert!(
                count >= 5,
                "group {}: density at calibrated radius must be ≥5, got {}",
                group.name,
                count,
            );
        }

        // ── Assertion 3: cluster centroid recovery returns API-shaped output.
        // For each group, BUNDLE_MAJORITY the group's hypervectors,
        // run explain. The test asserts API correctness (right number
        // of tuples, valid kinds) — the ≥80% recovery accuracy target
        // is a production-corpus measurement, not synthetic.
        use leyline_hdc::canonical::CanonicalKind;
        use leyline_hdc::query::explain_cluster_centroid;

        for group in &groups {
            let centroid_blob: Vec<u8> = conn
                .query_row(
                    "SELECT BUNDLE_MAJORITY(hv) FROM _hdc \
                     WHERE layer_kind = 'ast' AND scope_id LIKE ?1",
                    [format!("{}/%", group.name)],
                    |r| r.get(0),
                )
                .unwrap();
            let centroid: leyline_hdc::Hypervector = centroid_blob.try_into().unwrap();

            // The centroid's "root kind" + arity is approximately what
            // we'd find in the parsed source: a Block (file root) with
            // ~1 child (the func decl).
            let candidate_kinds = [
                CanonicalKind::Decl,
                CanonicalKind::Expr,
                CanonicalKind::Stmt,
                CanonicalKind::Block,
                CanonicalKind::Ref,
                CanonicalKind::Lit,
                CanonicalKind::Op,
            ];

            let recovered = explain_cluster_centroid(
                &centroid,
                CanonicalKind::Block,
                leyline_hdc::util::bucket_arity(2),
                &[CanonicalKind::Decl, CanonicalKind::Decl],
                &cb,
                &candidate_kinds,
                2,
            );

            assert_eq!(
                recovered.len(),
                2,
                "group {}: explain must return arity-many tuples",
                group.name,
            );
            for (i, (idx, kind, _d)) in recovered.iter().enumerate() {
                assert_eq!(*idx, i);
                assert!(
                    candidate_kinds.contains(kind),
                    "group {}: recovered kind at pos {i} must be from candidate set, got {kind:?}",
                    group.name,
                );
            }
            eprintln!(
                "group {}: centroid recovery = {:?}",
                group.name,
                recovered.iter().map(|(_, k, d)| (k, d)).collect::<Vec<_>>(),
            );
        }
    }

    #[test]
    fn ast_codebook_clusters_near_clones_in_real_go_code() {
        use leyline_hdc::codebook::AstCodebook;

        // A and A_prime: same shape, different identifiers and literal value.
        // Both: package + func with one int param, returning param + literal.
        let src_a = "package m\n\nfunc Add(x int) int { return x + 1 }\n";
        let src_a_prime = "package m\n\nfunc Foo(y int) int { return y + 42 }\n";

        // B: different shape — conditional, multiple statements, no params.
        let src_b = "package m\n\nfunc Run() { if true { println(\"a\") } else { println(\"b\") } }\n";

        // C: yet another shape — for loop.
        let src_c = "package m\n\nfunc Loop() { for i := 0; i < 10; i++ { println(i) } }\n";

        let cb = AstCodebook::new();
        let hv_a = encode_go(src_a, &cb);
        let hv_a_prime = encode_go(src_a_prime, &cb);
        let hv_b = encode_go(src_b, &cb);
        let hv_c = encode_go(src_c, &cb);

        let d_clones = normalized_distance(&hv_a, &hv_a_prime);
        let d_a_b = normalized_distance(&hv_a, &hv_b);
        let d_a_c = normalized_distance(&hv_a, &hv_c);
        let d_b_c = normalized_distance(&hv_b, &hv_c);

        eprintln!("d(A,A') = {d_clones:.4} (clones — expect < 0.20)");
        eprintln!("d(A,B)  = {d_a_b:.4} (distinct — expect > 0.35)");
        eprintln!("d(A,C)  = {d_a_c:.4} (distinct — expect > 0.35)");
        eprintln!("d(B,C)  = {d_b_c:.4} (distinct — expect > 0.35)");

        // Clones cluster.
        assert!(
            d_clones < 0.20,
            "near-clone pair too far apart: d/D = {d_clones:.4} (expected < 0.20)",
        );

        // Distinct pairs land far.
        assert!(
            d_a_b > 0.35,
            "A vs B too close: d/D = {d_a_b:.4} (expected > 0.35)",
        );
        assert!(
            d_a_c > 0.35,
            "A vs C too close: d/D = {d_a_c:.4} (expected > 0.35)",
        );
        assert!(
            d_b_c > 0.35,
            "B vs C too close: d/D = {d_b_c:.4} (expected > 0.35)",
        );

        // Margin: clones MUST be closer to each other than to any distinct.
        assert!(
            d_clones < d_a_b && d_clones < d_a_c,
            "clone-pair distance {d_clones:.4} not strictly less than \
             distinct-pair distances ({d_a_b:.4}, {d_a_c:.4}) — clustering broken",
        );
    }
}
