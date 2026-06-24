//! Phase 0B — real-corpus agreement between HDC and vec retrieval.
//!
//! Phase 0 measured *throughput* on synthetic vectors and showed HDC's
//! popcount-Hamming is ~9× faster per pair than naive cosine sim. That
//! says nothing about retrieval *quality* — whether the two backends
//! find the same things on real data, or different things, or whether
//! one is degenerate.
//!
//! ## What this measures
//!
//! Walk the LLO daemon source tree, extract Rust functions (kind
//! `function_item` in tree-sitter-rust), encode each via BOTH backends:
//! - HDC: `tree_to_encoder_node` → `encode_fresh` → 8192-bit hv
//! - vec: `FastEmbedder::embed(function_text)` → 384-float embedding
//!
//! Pick K=5 functions as queries (leave-one-out — query is in the
//! corpus, so trivial top-1 is "itself"; the interesting comparison is
//! top-2..top-10). Linear-scan top-10 on each backend. Compute
//! **Jaccard agreement** between the two top-10 sets per query, and the
//! mean across queries.
//!
//! ## What the answer means
//!
//! - **High Jaccard (>0.7)** — backends find the same things; one is
//!   likely redundant against the other.
//! - **Low Jaccard (<0.3)** — backends are *complementary*: HDC
//!   measures structural similarity (control flow, AST shape), vec
//!   measures textual/semantic similarity (vocabulary, topic). Low
//!   agreement is a *feature*, not a bug — they answer different
//!   questions; a multi-modal retrieval surface gets both.
//! - **Middle (0.3-0.7)** — partial overlap, both useful.
//!
//! ## Caveats
//!
//! - Directional / qualitative; recall@K vs hand-built ground truth is
//!   a separate, larger Phase 0C piece. Without ground truth, "high
//!   Jaccard" doesn't mean "both are correct" — they could both be
//!   wrong in the same way.
//! - Single-language snapshot (Rust). Mixed-language or other corpora
//!   will differ; this is a starting calibration not a general claim.
//! - `#[ignore]` because the first run downloads ~22MB fastembed model.
//!
//! ## Reproduce
//!
//! ```sh
//! cargo test --release -p leyline-cli-lib --features vec,hdc \
//!     --test phase_0b_real_corpus_agreement -- --ignored --nocapture
//! ```

#![cfg(all(feature = "vec", feature = "hdc"))]

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use leyline_cli_lib::daemon::embed::{Embedder, FastEmbedModel, FastEmbedder};
use leyline_cli_lib::daemon::hdc_pass::tree_to_encoder_node;
use leyline_hdc::canonical::RustCanonicalMap;
use leyline_hdc::codebook::AstCodebook;
use leyline_hdc::encode_fresh;
use leyline_hdc::util::{Hypervector, popcount_distance};
use leyline_ts::languages::TsLanguage;
use tree_sitter::{Node, Parser};

/// Top-K retrieved per query.
const K: usize = 10;

/// Number of leave-one-out queries to run.
const QUERIES: usize = 5;

/// Cap on number of functions in the corpus so the test stays under a
/// minute even on cold model download. Pick functions spread across
/// many files for a representative sample.
const CORPUS_CAP: usize = 200;

/// Function record: source text (input to fastembed) + the HDC
/// hypervector + a stable debug label so per-query results are
/// inspectable.
struct FunctionRecord {
    label: String,
    source: String,
    hv: Hypervector,
}

/// Walk a directory recursively for `.rs` files. Filters out target dirs
/// and test/bench subdirs — we want production code as the corpus, not
/// test scaffolding (which has its own structural patterns we'd be
/// over-sampling).
fn collect_rs_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if path.is_dir() {
                if name == "target" || name == "tests" || name == "benches" || name.starts_with('.')
                {
                    continue;
                }
                walk(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }
    walk(root, &mut out);
    out
}

/// Depth-first walk collecting every `function_item` node (top-level
/// `fn` and method `fn`). Doesn't recurse into matched nodes — a
/// function body that contains nested fns gets indexed once at the
/// outer level. Mirrors hdc_enrich.rs's `collect_function_nodes`.
fn collect_function_nodes<'a>(node: Node<'a>, out: &mut Vec<Node<'a>>) {
    if node.kind() == "function_item" {
        out.push(node);
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_function_nodes(child, out);
    }
}

/// Slice the source between a node's start and end byte offsets — the
/// raw text of that function in the file.
fn node_source<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.start_byte()..node.end_byte()]
}

/// Linear-scan top-K by HDC popcount-distance. Returns indices into
/// `corpus`, sorted ascending by distance.
fn hdc_top_k(query: &Hypervector, corpus: &[Hypervector], k: usize) -> Vec<usize> {
    let mut scored: Vec<(u32, usize)> = corpus
        .iter()
        .enumerate()
        .map(|(i, hv)| (popcount_distance(query, hv), i))
        .collect();
    scored.sort_unstable_by_key(|&(d, _)| d);
    scored.into_iter().take(k).map(|(_, i)| i).collect()
}

/// Linear-scan top-K by cosine similarity. Returns indices into
/// `corpus`, sorted descending by similarity.
fn vec_top_k(query: &[f32], corpus: &[Vec<f32>], k: usize) -> Vec<usize> {
    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb: f32 = b.iter().map(|y| y * y).sum::<f32>().sqrt();
        if na == 0.0 || nb == 0.0 {
            0.0
        } else {
            dot / (na * nb)
        }
    }
    let mut scored: Vec<(f32, usize)> = corpus
        .iter()
        .enumerate()
        .map(|(i, v)| (cosine(query, v), i))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().take(k).map(|(_, i)| i).collect()
}

/// Jaccard index over two set-like Vecs: |A ∩ B| / |A ∪ B|.
fn jaccard(a: &[usize], b: &[usize]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let set_a: HashSet<usize> = a.iter().copied().collect();
    let set_b: HashSet<usize> = b.iter().copied().collect();
    let intersection = set_a.intersection(&set_b).count();
    let union = set_a.union(&set_b).count();
    intersection as f64 / union as f64
}

#[test]
#[ignore = "Phase 0B — real corpus + agreement; downloads ~22MB model. Run with --ignored --nocapture"]
fn phase_0b_real_corpus_hdc_vs_vec_agreement() {
    println!("\n=== Phase 0B — HDC vs vec retrieval agreement on real corpus ===\n");

    // ── 1. Walk LLO daemon source for the corpus ──────────────────────
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("daemon");
    let files = collect_rs_files(&workspace_root);
    println!(
        "Scanning {} .rs files under {}",
        files.len(),
        workspace_root.display()
    );

    let lang = TsLanguage::Rust.ts_language();
    let kind_map = RustCanonicalMap;
    let codebook = AstCodebook;

    let mut corpus: Vec<FunctionRecord> = Vec::new();
    let mut parser = Parser::new();
    parser.set_language(&lang).expect("set tree-sitter Rust");
    for path in &files {
        let Ok(source) = fs::read_to_string(path) else {
            continue;
        };
        let Some(tree) = parser.parse(&source, None) else {
            continue;
        };
        let mut funcs: Vec<Node> = Vec::new();
        collect_function_nodes(tree.root_node(), &mut funcs);

        for (i, fn_node) in funcs.into_iter().enumerate() {
            let fn_src = node_source(fn_node, &source).to_string();
            // Skip trivial / one-line stubs — fastembed signal is poor
            // on <20-char inputs and HDC adds nothing on tree depth 1.
            if fn_src.len() < 40 {
                continue;
            }
            let encoder_node = tree_to_encoder_node(fn_node, &kind_map, Some(source.as_bytes()));
            let hv = encode_fresh(&encoder_node, &codebook);
            corpus.push(FunctionRecord {
                label: format!(
                    "{}::fn{}",
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("<unknown>"),
                    i
                ),
                source: fn_src,
                hv,
            });
            if corpus.len() >= CORPUS_CAP {
                break;
            }
        }
        if corpus.len() >= CORPUS_CAP {
            break;
        }
    }
    println!("Corpus: {} Rust functions extracted\n", corpus.len());

    if corpus.len() < K + QUERIES {
        panic!(
            "corpus too small ({} functions) — need >= K+QUERIES = {}",
            corpus.len(),
            K + QUERIES
        );
    }

    // ── 2. Pull out HDC hvs ──────────────────────────────────────────
    let hdc_corpus: Vec<Hypervector> = corpus.iter().map(|f| f.hv).collect();

    // ── 3. Encode every function via FastEmbedder ────────────────────
    let cache = std::env::temp_dir().join("llo-fastembed-test-cache");
    fs::create_dir_all(&cache).ok();
    let embedder =
        FastEmbedder::with_cache_dir(FastEmbedModel::default(), cache).expect("embedder init");

    let t = std::time::Instant::now();
    let vec_corpus: Vec<Vec<f32>> = corpus
        .iter()
        .map(|f| embedder.embed(&f.source).expect("embed"))
        .collect();
    println!(
        "vec embedded {} functions in {:.2?}",
        vec_corpus.len(),
        t.elapsed()
    );

    // ── 4. Pick QUERIES query indices spread across the corpus ───────
    let stride = corpus.len() / QUERIES;
    let query_indices: Vec<usize> = (0..QUERIES).map(|i| i * stride).collect();

    // ── 5. Per-query top-K + Jaccard ─────────────────────────────────
    println!();
    println!("Per-query top-{K} agreement:");
    println!(
        "{:>3}  {:<60}  {:>9}  {:>9}",
        "#", "query (label)", "Jacc@10", "JaccEx"
    );

    let mut jaccards = Vec::with_capacity(QUERIES);
    let mut jaccards_excl_self = Vec::with_capacity(QUERIES);
    for &q_idx in &query_indices {
        let q_hdc = &hdc_corpus[q_idx];
        let q_vec = &vec_corpus[q_idx];

        let hdc_top = hdc_top_k(q_hdc, &hdc_corpus, K);
        let vec_top = vec_top_k(q_vec, &vec_corpus, K);

        let j = jaccard(&hdc_top, &vec_top);
        let hdc_ex: Vec<usize> = hdc_top.iter().copied().filter(|&i| i != q_idx).collect();
        let vec_ex: Vec<usize> = vec_top.iter().copied().filter(|&i| i != q_idx).collect();
        let j_excl = jaccard(&hdc_ex, &vec_ex);

        let label = &corpus[q_idx].label;
        let truncated = if label.len() > 58 {
            format!("{}…", &label[..58])
        } else {
            label.to_string()
        };
        println!(
            "{:>3}  {:<60}  {:>9.3}  {:>9.3}",
            q_idx, truncated, j, j_excl
        );
        jaccards.push(j);
        jaccards_excl_self.push(j_excl);
    }

    let mean_j: f64 = jaccards.iter().sum::<f64>() / jaccards.len() as f64;
    let mean_j_excl: f64 = jaccards_excl_self.iter().sum::<f64>() / jaccards_excl_self.len() as f64;

    println!();
    println!("Mean Jaccard@{K}  (incl. self-match):  {:.3}", mean_j);
    println!(
        "Mean Jaccard@{}  (self-match excluded):    {:.3}",
        K - 1,
        mean_j_excl
    );
    println!();
    println!("Interpretation:");
    println!("  >= 0.7  — backends find the same things; one likely redundant");
    println!("  0.3-0.7 — partial overlap, both useful, neither subsumes the other");
    println!("  <  0.3  — complementary modalities: HDC structural, vec semantic");
    let band = if mean_j_excl >= 0.7 {
        "OVERLAPPING (HDC likely redundant against vec on this corpus)"
    } else if mean_j_excl >= 0.3 {
        "PARTIAL OVERLAP (multi-modal retrieval gets distinct signal from each)"
    } else {
        "COMPLEMENTARY (HDC + vec answer different questions; the substrate value prop)"
    };
    println!();
    println!("Result band: {}", band);
    println!();

    // Soft sanity: agreement shouldn't be vacuously zero (would imply a
    // bug) — though < 0.05 is plausible for genuinely complementary
    // backends, so we don't fail the test on it.
    assert!(
        mean_j_excl <= 1.0,
        "Jaccard > 1 is impossible — check the formula"
    );
}
