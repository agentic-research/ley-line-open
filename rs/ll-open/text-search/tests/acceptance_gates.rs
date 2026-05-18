//! Acceptance-gate scaffolding for text-search engines against
//! ley-line-open at HEAD.
//!
//! Today these tests assert only structural well-formedness — the corpus
//! parses, every expected path exists on disk, every query is non-empty,
//! every expected list is non-empty. The retrieval-quality threshold
//! (`NDCG@10 ≥ baseline`) is a compile-time const here so wiring it up
//! once a real engine lands is one line of code, not a refactor.
//!
//! Latency-gate scaffolding (p95 ≤ 50ms) is sibling territory — a future
//! `benches/` criterion harness, not a `tests/`. Out of scope for this PR.

use std::collections::BTreeMap;
use std::path::PathBuf;

#[allow(unused_imports)]
use anyhow::{Context, Result};
use serde::Deserialize;

/// NDCG@10 threshold the engine must clear on the labeled corpus to ship.
/// Wired into the gated assertion below; bumped per the falsifiable plan
/// once a real engine reports numbers.
#[allow(dead_code)]
const NDCG_BASELINE: f32 = 0.40;

#[derive(Debug, Deserialize)]
struct Corpus {
    #[serde(rename = "_doc")]
    #[allow(dead_code)]
    doc: String,
    queries: Vec<LabeledQuery>,
}

#[derive(Debug, Deserialize)]
struct LabeledQuery {
    query: String,
    expected_node_ids: Vec<String>,
}

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points at rs/ll-open/text-search; walk up to
    // ley-line-open/.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .ancestors()
        .nth(3)
        .expect("manifest dir has at least 3 ancestors")
        .to_path_buf()
}

fn load_corpus() -> Result<Corpus> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("eval_corpus.json");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read corpus at {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| "parse corpus JSON")
}

#[test]
fn corpus_is_well_formed() -> Result<()> {
    // Pin: the eval corpus is the gate's load-bearing input. A typo
    // that drops `expected_node_ids` or an empty `query` would silently
    // make the gate pass without measuring anything. Assert structure
    // up-front so the gate has something to measure against when the
    // engine wires in.
    let corpus = load_corpus()?;
    assert!(
        !corpus.queries.is_empty(),
        "corpus must carry at least one labeled query",
    );

    let mut seen_queries: BTreeMap<&str, ()> = BTreeMap::new();
    for (i, q) in corpus.queries.iter().enumerate() {
        assert!(
            !q.query.trim().is_empty(),
            "query {i}: text must be non-empty (trimmed)",
        );
        assert!(
            !q.expected_node_ids.is_empty(),
            "query {i} (`{}`): expected_node_ids must be non-empty — \
             a query with no labels can't contribute to NDCG",
            q.query,
        );
        // Light dedup guard so an editor doesn't accidentally double-add.
        assert!(
            seen_queries.insert(q.query.as_str(), ()).is_none(),
            "duplicate query text at index {i}: `{}`",
            q.query,
        );
    }
    Ok(())
}

#[test]
fn corpus_expected_paths_exist_on_disk() -> Result<()> {
    // Pin: every expected node_id is a path relative to the repo root
    // and must resolve to a real file. A typo in the corpus (e.g.
    // `READEM.md`) would silently zero-out NDCG for that row. Fail
    // up-front instead.
    let corpus = load_corpus()?;
    let root = repo_root();

    let mut missing: Vec<String> = vec![];
    for q in &corpus.queries {
        for id in &q.expected_node_ids {
            let path = root.join(id);
            if !path.exists() {
                missing.push(format!(
                    "query `{}`: expected `{}` (resolved `{}`) does not exist",
                    q.query,
                    id,
                    path.display(),
                ));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "corpus references {} path(s) that don't exist at repo HEAD:\n  - {}",
        missing.len(),
        missing.join("\n  - "),
    );
    Ok(())
}

/// Acceptance gate — turns on when `engine-witchcraft` is enabled AND a
/// real T5 assets bundle is staged at `WITCHCRAFT_ASSETS_DIR`.
///
/// Falsifiable contract:
/// 1. Build a `WitchcraftEngine` against the staged assets.
/// 2. For every distinct `expected_node_ids` entry across all queries,
///    read the file from disk and `upsert(rel_path, body)`.
/// 3. `finalize()` once.
/// 4. For each query, `engine.search(query, 10)`. Score by NDCG@10
///    against the corpus labels (binary relevance: hit iff a returned
///    `Hit::node_id` is in that query's `expected_node_ids`).
/// 5. Average across queries. Assert `>= NDCG_BASELINE`.
///
/// Today this body is the scaffold — the assertion compiles and runs
/// only when both feature gates align. CI without the assets bundle
/// skips silently with a clear stderr line; CI with the bundle runs it.
/// Bumping the baseline is the loop: tighten only after measuring.
#[cfg(feature = "engine-witchcraft")]
#[test]
fn ndcg_at_10_meets_baseline() -> anyhow::Result<()> {
    use leyline_text_search::TextSearchEngine;
    use leyline_text_search::witchcraft::WitchcraftEngine;

    let Some(assets) = std::env::var_os("WITCHCRAFT_ASSETS_DIR") else {
        eprintln!(
            "skipping ndcg_at_10_meets_baseline: WITCHCRAFT_ASSETS_DIR not set. \
             Stage a T5 tokenizer + safetensors dir and re-run to measure."
        );
        return Ok(());
    };

    let corpus = load_corpus()?;
    let root = repo_root();
    let tmp = tempfile::tempdir()?;
    let engine = WitchcraftEngine::open(tmp.path().join("eval.db"), std::path::Path::new(&assets))
        .map_err(|e| anyhow::anyhow!("open engine: {e}"))?;

    // Ingest the union of all labeled paths once.
    let mut labeled: std::collections::BTreeSet<String> = Default::default();
    for q in &corpus.queries {
        for id in &q.expected_node_ids {
            labeled.insert(id.clone());
        }
    }
    for id in &labeled {
        let body = std::fs::read_to_string(root.join(id))
            .with_context(|| format!("read corpus body for {id}"))?;
        engine
            .upsert(id, &body)
            .map_err(|e| anyhow::anyhow!("upsert {id}: {e}"))?;
    }
    engine
        .finalize()
        .map_err(|e| anyhow::anyhow!("finalize: {e}"))?;

    // NDCG@10 with binary relevance. IDCG for a query with `m` relevant
    // docs at top-`m` positions = sum_{i=1..min(m,10)} 1/log2(i+1).
    let mut sum_ndcg = 0.0_f32;
    for q in &corpus.queries {
        let hits = engine
            .search(&q.query, 10)
            .map_err(|e| anyhow::anyhow!("search `{}`: {e}", q.query))?;
        let relevant: std::collections::HashSet<&str> =
            q.expected_node_ids.iter().map(String::as_str).collect();
        let mut dcg = 0.0_f32;
        for (i, h) in hits.iter().enumerate() {
            if relevant.contains(h.node_id.as_str()) {
                dcg += 1.0 / ((i as f32 + 2.0).log2());
            }
        }
        let m = relevant.len().min(10);
        let idcg: f32 = (0..m).map(|i| 1.0 / ((i as f32 + 2.0).log2())).sum();
        let ndcg = if idcg > 0.0 { dcg / idcg } else { 0.0 };
        sum_ndcg += ndcg;
    }
    let mean = sum_ndcg / corpus.queries.len() as f32;
    eprintln!("ndcg_at_10_meets_baseline: NDCG@10 = {mean:.3} (baseline = {NDCG_BASELINE:.3})");
    assert!(
        mean >= NDCG_BASELINE,
        "NDCG@10 = {mean:.3} below baseline {NDCG_BASELINE:.3} on labeled corpus",
    );
    Ok(())
}
