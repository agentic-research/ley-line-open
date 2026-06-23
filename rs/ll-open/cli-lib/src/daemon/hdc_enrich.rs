//! HDC enrichment pass — populates `_hdc` with **function-level**
//! hypervectors from the projected AST.
//!
//! Closes the integration loop of bead `ley-line-open-96b1a9` ("HDC
//! hypervectors as structural stalks (LLO HdcPass + sheaf-cache
//! integration)"). The substrate ops (`hdc_search` / `hdc_density` /
//! `hdc_calibrate`) shipped via `ley-line-open-c32596`, but they query
//! `_hdc` — which stays empty without a populate pass.
//!
//! ## V1 design choices
//!
//! Decisions are all anchored against the math-friend review on bead
//! `ley-line-open-641809` (see `_agent_log/theoretical-foundations-
//! analyst_2026-06-22_agent_log.md`).
//!
//! - **Granularity: function-level.** A first draft used file-level
//!   (one `_hdc` row per `_source` row). Math-friend Q1 killed it:
//!   XOR-bundling N≈50 top-level items concentrates the file-level
//!   pairwise Hamming distance around D/2=4096 with std ≈ 6 bits.
//!   `radius_search`'s usable signal band is wider than that —
//!   results would be indistinguishable noise. Function-level (depth
//!   ~5-7 per the math-friend depth review) keeps signal.
//!
//! - **Scope_id format: `func://{path}::{name}@{start_byte}-{end_byte}`.**
//!   Per Q3. Path-only would lock the schema into a flat namespace
//!   that couldn't add file-level / module-level rows later without
//!   downstream consumers breaking. The `_hdc.scope_id` column is
//!   `TEXT`, so the URI scheme is free at write time and saves a
//!   migration later.
//!
//! - **Languages covered at populate-time: Go + Rust.** Both have
//!   `function_declaration` / `function_item` nodes with a `name`
//!   field. JSON/YAML aren't excluded from the daemon op surface
//!   (those still accept arbitrary content via `hdc_search`'s
//!   encode-side); they're just not populated because "function" has
//!   no meaning there. JSON/YAML in `_source` get counted in
//!   `skipped_lang` and ignored.
//!
//! - **Layers populated: `LayerKind::Ast` only.** Per Q2: single-
//!   layer is adequate at function-level granularity. Adding `Lex`
//!   just to feed `combined_prefilter` triples surface area without
//!   user-facing value until function-level AST is shown to work.
//!
//! - **Auto-calibration after populate when `items_added >= 100`.**
//!   Per Q5. Without `_hdc_baseline` populated, callers passing
//!   uncalibrated `max_distance` to `hdc_search` would get silently-
//!   useless results. `calibrate_and_persist` is sub-second on 10k
//!   rows; auto-running it closes the operational loop.
//!
//! - **Cache: one `SubtreeCache` shared across the whole pass.** Per
//!   Q4: cross-codebook poisoning is prevented by `cache_key`'s
//!   `codebook_tag` mix-in (bead `4ba0cf`). Re-using the cache across
//!   files lets shared subtrees (`if x { ... }` etc.) collapse to one
//!   encode per shape, amortizing the per-subtree blake3 cost.
//!
//! ## Math gates (mandatory pre-merge)
//!
//! Two tests live in `cli-lib/tests/hdc_math_gates.rs`:
//!   1. *Saturation:* encode 50 distinct functions; assert median
//!      pairwise distance ∉ [4090, 4102] AND std >= 30.
//!   2. *Discriminability:* encode 10 functions + trivially-mutated
//!      versions; assert distance(orig, mutant) < median_random / 4.
//!
//! If either fails, the encoder is producing well-distributed bits
//! with no semantic gradient — pretty noise, useless for radius
//! search. These tests are the merge gate, not just "rows exist."

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use rusqlite::Connection;

use super::enrichment::{EnrichmentPass, EnrichmentStats};

/// `_hdc_baseline` auto-calibration threshold. Populates with fewer
/// rows than this skip calibration so the median + MAD estimates have
/// a meaningful sample. Below 100 rows, calibration adds noise instead
/// of removing it.
const AUTO_CALIBRATE_MIN_ROWS: u64 = 100;

/// Default sample size handed to `calibrate_and_persist` when auto-
/// calibration triggers. Matches the `hdc_calibrate` op default.
const AUTO_CALIBRATE_SAMPLE_SIZE: usize = 1000;

/// Enrichment pass that walks `_source`, parses each Go/Rust file,
/// extracts function nodes, encodes each into a hypervector via
/// `leyline-hdc`, and INSERTs into `_hdc` with a URI-formatted
/// `scope_id`. Calibrates the baseline at the end when the corpus is
/// large enough to make per-layer statistics meaningful.
///
/// Registered in `cmd_daemon` alongside `TreeSitterPass` /
/// `LspEnrichmentPass` / `EmbeddingPass`. Cfg-gated on cli-lib's
/// default-on `hdc` feature.
pub struct HdcEnrichmentPass;

impl EnrichmentPass for HdcEnrichmentPass {
    fn name(&self) -> &str {
        "hdc"
    }

    fn depends_on(&self) -> &[&str] {
        &["tree-sitter"]
    }

    fn reads(&self) -> &[&str] {
        &["_source"]
    }

    fn writes(&self) -> &[&str] {
        // `_hdc_baseline` ships in the write set because auto-
        // calibration writes to it when the corpus is large enough.
        // `_hdc_subtree_cache` is a forward-compat slot for the
        // persistent-cache optimization tracked under hdc-4 follow-
        // ups; today the cache lives in-process per pass.
        &["_hdc", "_hdc_baseline", "_hdc_subtree_cache"]
    }

    fn run(
        &self,
        conn: &Connection,
        source_dir: &Path,
        changed_files: Option<&[String]>,
    ) -> Result<EnrichmentStats> {
        let start = Instant::now();

        leyline_hdc::schema::create_hdc_schema(conn).context("HdcPass: create_hdc_schema")?;
        leyline_hdc::sql_udf::register_hdc_udfs(conn).context("HdcPass: register_hdc_udfs")?;

        let basis: i64 = leyline_ts::schema::get_meta(conn, "parse_version")
            .ok()
            .flatten()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0);

        let mut stmt = conn.prepare_cached("SELECT id, language, path FROM _source")?;
        let rows: Vec<(String, String, String)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let changed_set: Option<std::collections::HashSet<&str>> =
            changed_files.map(|files| files.iter().map(|s| s.as_str()).collect());

        let cache = leyline_hdc::SubtreeCache::new();
        let codebook = leyline_hdc::codebook::AstCodebook;

        let mut insert_stmt = conn.prepare_cached(
            "INSERT OR REPLACE INTO _hdc (scope_id, layer_kind, hv, basis) \
             VALUES (?1, ?2, ?3, ?4)",
        )?;

        let mut files_processed: u64 = 0;
        let mut items_added: u64 = 0;
        let mut skipped_lang: u64 = 0;
        let mut skipped_read: u64 = 0;
        let mut skipped_parse: u64 = 0;

        for (_source_id, language, path) in &rows {
            if let Some(set) = changed_set.as_ref()
                && !set.contains(path.as_str())
            {
                continue;
            }

            let Some(setup) = resolve_hdc_setup(language) else {
                skipped_lang += 1;
                continue;
            };

            let abs_path = source_dir.join(path);
            let content = match std::fs::read_to_string(&abs_path) {
                Ok(s) => s,
                Err(e) => {
                    log::debug!(
                        "HdcPass: skip {path} ({language}) — read {}: {e}",
                        abs_path.display()
                    );
                    skipped_read += 1;
                    continue;
                }
            };

            let mut parser = tree_sitter::Parser::new();
            if parser.set_language(&setup.ts_language).is_err() {
                skipped_parse += 1;
                continue;
            }
            let Some(tree) = parser.parse(&content, None) else {
                skipped_parse += 1;
                continue;
            };

            let mut function_nodes = Vec::new();
            collect_function_nodes(tree.root_node(), setup.is_function, &mut function_nodes);

            files_processed += 1;

            for func_node in function_nodes {
                let Some(name) = func_node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(content.as_bytes()).ok())
                else {
                    // Anonymous closures / unnamed methods — skip
                    // silently. They're real Go/Rust constructs but
                    // don't have a stable scope_id; encoding them
                    // would produce rows that consumers can't
                    // address.
                    continue;
                };

                let start_byte = func_node.start_byte();
                let end_byte = func_node.end_byte();

                let encoder_node =
                    super::hdc_pass::tree_to_encoder_node(func_node, &*setup.kind_map);
                let hv = leyline_hdc::encoder::encode_tree(&encoder_node, &codebook, &cache);
                let hv_bytes = hv.to_vec();

                // URI-formatted scope_id (math-friend Q3): future
                // file-level / module-level rows can use distinct
                // schemes (`file://`, `module://`) without breaking
                // downstream parsers.
                let scope_id = format!("func://{path}::{name}@{start_byte}-{end_byte}");

                insert_stmt.execute(rusqlite::params![
                    scope_id,
                    leyline_hdc::LayerKind::Ast.as_str(),
                    hv_bytes,
                    basis,
                ])?;
                items_added += 1;
            }
        }

        // Auto-calibrate if the corpus is large enough to give
        // meaningful per-layer median + MAD (math-friend Q5).
        if items_added >= AUTO_CALIBRATE_MIN_ROWS {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let calibrated = leyline_hdc::calibrate::calibrate_and_persist(
                conn,
                AUTO_CALIBRATE_SAMPLE_SIZE,
                now_ms,
            )
            .context("HdcPass: auto-calibrate baseline")?;
            log::info!(
                "HdcPass: items_added={items_added} files_processed={files_processed} skipped_lang={skipped_lang} skipped_read={skipped_read} skipped_parse={skipped_parse} calibrated_layers={calibrated}"
            );
        } else {
            log::info!(
                "HdcPass: items_added={items_added} files_processed={files_processed} skipped_lang={skipped_lang} skipped_read={skipped_read} skipped_parse={skipped_parse} (auto-calibrate skipped, corpus < {AUTO_CALIBRATE_MIN_ROWS})"
            );
        }

        Ok(EnrichmentStats {
            pass_name: "hdc".to_string(),
            files_processed,
            items_added,
            duration_ms: start.elapsed().as_millis() as u64,
        })
    }
}

/// Per-language predicate + canonical-map pair. `resolve_hdc_setup`
/// returns one of these for languages in the populate-time support
/// set (Go + Rust today). Returning a struct instead of a tuple keeps
/// the call site readable.
struct HdcLangSetup {
    ts_language: tree_sitter::Language,
    kind_map: Box<dyn leyline_hdc::canonical::CanonicalKindMap>,
    /// Predicate that returns true when a tree-sitter `Node` is a
    /// function-level scope (top-level function, method, or closure
    /// the populate should index). Anonymous closures pass the
    /// predicate but get skipped later — they have no stable
    /// `scope_id`.
    is_function: fn(&tree_sitter::Node) -> bool,
}

/// Resolve a `_source.language` string to the parser + canonical map +
/// function predicate. Returns `None` for languages outside the
/// populate-time HDC support set (today: `go`, `rust`).
///
/// JSON/YAML have `CanonicalKindMap` impls and parse via leyline-ts,
/// but they have no notion of "function" — they're skipped at populate
/// because indexing every JSON value would explode `_hdc` row count
/// without producing useful structural similarity. The daemon ops
/// still accept JSON/YAML inputs (the encode-side of `hdc_search`
/// works on any language); they just don't get populated rows to
/// match against.
fn resolve_hdc_setup(language: &str) -> Option<HdcLangSetup> {
    use leyline_hdc::canonical::{CanonicalKindMap, GoCanonicalMap, RustCanonicalMap};

    let ts_language = leyline_ts::languages::TsLanguage::from_name(language)
        .ok()?
        .ts_language();

    match language.to_lowercase().as_str() {
        "go" | "golang" => Some(HdcLangSetup {
            ts_language,
            kind_map: Box::new(GoCanonicalMap) as Box<dyn CanonicalKindMap>,
            is_function: is_go_function,
        }),
        "rust" | "rs" => Some(HdcLangSetup {
            ts_language,
            kind_map: Box::new(RustCanonicalMap) as Box<dyn CanonicalKindMap>,
            is_function: is_rust_function,
        }),
        _ => None,
    }
}

/// Go function-node predicate. Top-level `func F() { ... }` is
/// `function_declaration`; methods (`func (r *R) F()`) are
/// `method_declaration`. tree-sitter-go's `function_lit` (closures)
/// are intentionally NOT matched here — they're anonymous so they
/// can't get a stable `scope_id`.
fn is_go_function(node: &tree_sitter::Node) -> bool {
    matches!(node.kind(), "function_declaration" | "method_declaration")
}

/// Rust function-node predicate. `fn foo() { ... }` and methods
/// (`impl T { fn foo() }`) both have kind `function_item`. Closures
/// (`|x| x + 1`) are `closure_expression` and intentionally NOT
/// matched (same anonymity reason).
fn is_rust_function(node: &tree_sitter::Node) -> bool {
    node.kind() == "function_item"
}

/// Depth-first walk of `node` collecting every descendant that matches
/// `is_function`. Doesn't recurse INTO matched nodes — a function
/// definition's body might contain nested function definitions
/// (Go closures via `var f = func() {}`; Rust nested `fn`), but those
/// are typically encoded as part of the parent function's
/// hypervector. If they need to be addressable separately, a follow-
/// up bead can switch this to a full walk.
fn collect_function_nodes<'a>(
    node: tree_sitter::Node<'a>,
    is_function: fn(&tree_sitter::Node) -> bool,
    out: &mut Vec<tree_sitter::Node<'a>>,
) {
    if is_function(&node) {
        out.push(node);
        return;
    }
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_function_nodes(cursor.node(), is_function, out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::enrichment::assert_pass_metadata;

    #[test]
    fn hdc_pass_metadata_pinned() {
        let pass = HdcEnrichmentPass;
        assert_pass_metadata(
            &pass,
            "hdc",
            &["tree-sitter"],
            &["_source"],
            &["_hdc", "_hdc_baseline", "_hdc_subtree_cache"],
        );
    }

    #[test]
    fn resolve_hdc_setup_covers_go_and_rust() {
        assert!(resolve_hdc_setup("go").is_some());
        assert!(resolve_hdc_setup("golang").is_some());
        assert!(resolve_hdc_setup("rust").is_some());
        assert!(resolve_hdc_setup("rs").is_some());

        // JSON/YAML have CanonicalKindMaps but no "function" concept —
        // intentionally NOT populated. Daemon-op encode-side still
        // accepts them via the resolve_query_hv path in ops.rs.
        assert!(resolve_hdc_setup("json").is_none());
        assert!(resolve_hdc_setup("yaml").is_none());

        // Other languages: no canonical map (yet).
        assert!(resolve_hdc_setup("python").is_none());
        assert!(resolve_hdc_setup("html").is_none());

        // Unknown languages: leyline-ts rejects.
        assert!(resolve_hdc_setup("klingon").is_none());
    }

    #[test]
    fn collect_function_nodes_finds_top_level_go_functions() {
        let src = "package main\n\nfunc one() {}\n\nfunc two() {}\n\nfunc (r *R) m() {}\n";
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&leyline_ts::languages::TsLanguage::Go.ts_language())
            .unwrap();
        let tree = parser.parse(src, None).unwrap();
        let mut out = Vec::new();
        collect_function_nodes(tree.root_node(), is_go_function, &mut out);
        assert_eq!(
            out.len(),
            3,
            "expected 3 function-level scopes; got {out:?}"
        );
    }

    #[test]
    fn collect_function_nodes_finds_top_level_rust_functions() {
        let src = "fn one() {}\nfn two(x: i32) -> i32 { x + 1 }\nstruct S; impl S { fn three(&self) {} }\n";
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&leyline_ts::languages::TsLanguage::Rust.ts_language())
            .unwrap();
        let tree = parser.parse(src, None).unwrap();
        let mut out = Vec::new();
        collect_function_nodes(tree.root_node(), is_rust_function, &mut out);
        assert_eq!(
            out.len(),
            3,
            "expected 3 function-level scopes; got {out:?}"
        );
    }

    #[test]
    fn collect_function_nodes_does_not_recurse_into_matched_nodes() {
        // If a function definition contains a nested closure that
        // happens to match the predicate (Go: it doesn't; Rust:
        // closure_expression doesn't match function_item — same), the
        // walker correctly stops at the outer match. This pins the
        // contract.
        let src = "fn outer() { fn inner() {} }\n";
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&leyline_ts::languages::TsLanguage::Rust.ts_language())
            .unwrap();
        let tree = parser.parse(src, None).unwrap();
        let mut out = Vec::new();
        collect_function_nodes(tree.root_node(), is_rust_function, &mut out);
        // Only the outer function — the inner is technically also a
        // function_item but we stop at the first match per
        // `collect_function_nodes`'s contract.
        assert_eq!(out.len(), 1, "expected 1 (outer only); got {out:?}");
    }
}
