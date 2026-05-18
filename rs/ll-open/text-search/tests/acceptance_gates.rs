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

/// Acceptance gate — to be turned on once a real engine is wired.
///
/// Today: ignored. The body documents the falsifiable contract so the
/// next reviewer can flip it on without rediscovering the design:
///
/// 1. Build an engine (the real WitchcraftEngine — not the stub).
/// 2. Read every file referenced in `expected_node_ids` across the
///    corpus, upsert it into the engine with the relative path as the
///    `node_id`.
/// 3. `finalize()`.
/// 4. For each query, run `engine.search(query, 10)`. Compute NDCG@10
///    against the corpus labels (binary relevance: hit iff the returned
///    node_id is in `expected_node_ids`).
/// 5. Average across queries. Assert `>= NDCG_BASELINE`.
///
/// When this test is un-ignored, the substrate-non-leak gate's daemon
/// half (Σ root unchanged across indexing) must move out of `// TODO`
/// in `substrate_non_leak.rs` too.
#[test]
#[ignore = "pending real engine — see leyline_text_search::witchcraft for the rusqlite-skew blocker"]
fn ndcg_at_10_meets_baseline() {
    // Intentionally left as a runtime panic — switching from `#[ignore]`
    // to live without writing the body would otherwise pass silently.
    panic!(
        "NDCG@10 gate is scaffold-only today (NDCG_BASELINE = {NDCG_BASELINE}). \
         Implement the steps in the docstring once the real WitchcraftEngine lands."
    );
}
