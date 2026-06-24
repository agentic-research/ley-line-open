//! Phase 0C — HDC vs vec rank-vector mutual information.
//!
//! Phase 0B used Jaccard-on-top-K, which is a stats heuristic: it
//! ignores rank ordering beyond the K boundary and has no closed-form
//! null. The result (mean Jaccard@9 = 0.037 over 5 leave-one-out queries,
//! ~1.3σ above the random-agreement null of ~5%) was not informative.
//!
//! The principled replacement (per math-friend) is **mutual information
//! on the full rank vector**: for each query q, compute the full
//! ordering of the N-doc corpus under both backends, then estimate
//! `MI(R_HDC, R_vec)` from the joint histogram. MI has a closed-form
//! null under independence, so we get a z-score rather than a vibes
//! check.
//!
//! ## What the answer means
//!
//! - **High MI (> 2σ above null)** — backends order the corpus
//!   correlatedly; rank signal is shared, so one is partially redundant.
//! - **MI ≈ null** — orthogonal axes; the two backends answer different
//!   questions across the whole corpus, not just the top-K boundary.
//!   This is the multi-modal value prop.
//! - **Fraction of theoretical max < 5%** — very weak coupling.
//!
//! ## Method
//!
//! 1. Reuse Phase 0B's corpus / encoder / query selection.
//! 2. For each query, compute full rank vectors r_hdc and r_vec over the
//!    N-doc corpus (rank 0 = best match).
//! 3. Accumulate (r_hdc[i], r_vec[i]) pairs across all queries.
//! 4. Bin into a B×B joint histogram with equal-frequency binning. With
//!    N=200 and 5 queries, B=10 gives ~10 pairs per cell on average.
//! 5. MI = Σ p(a,b) log [p(a,b) / (p(a) p(b))], skipping zero cells.
//! 6. Permutation null: shuffle r_vec independently per query, recompute
//!    MI; repeat 100 times, report mean/std and z-score of observed MI.
//!
//! ## Reproduce
//!
//! ```sh
//! cargo test --release -p leyline-cli-lib --features vec,hdc \
//!     --test phase_0c_mi_agreement -- --ignored --nocapture
//! ```

#![cfg(all(feature = "vec", feature = "hdc"))]

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

/// Number of leave-one-out queries to run (matches Phase 0B).
const QUERIES: usize = 5;

/// Cap on corpus size (matches Phase 0B for like-for-like comparison).
const CORPUS_CAP: usize = 200;

/// Number of bins per axis for the joint histogram. With N=200 and 5
/// queries, B=10 gives ~10 pairs per cell on average — enough to
/// estimate p(a,b) reasonably while keeping log(B) ≈ 2.30 nats as a
/// meaningful theoretical max.
const BINS: usize = 10;

/// Number of permutation trials for the null distribution.
const NULL_TRIALS: usize = 100;

struct FunctionRecord {
    // Kept for parity with Phase 0B and future per-query diagnostics;
    // Phase 0C reports aggregate MI rather than per-query labels.
    #[allow(dead_code)]
    label: String,
    source: String,
    hv: Hypervector,
}

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

fn node_source<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.start_byte()..node.end_byte()]
}

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

/// Compute the full rank vector for HDC popcount-distance: rank[i] is
/// the position of doc `i` when the corpus is sorted ascending by
/// distance to query. Best match has rank 0, worst has rank N-1.
fn hdc_full_ranks(query: &Hypervector, corpus: &[Hypervector]) -> Vec<usize> {
    let n = corpus.len();
    let mut scored: Vec<(u32, usize)> = corpus
        .iter()
        .enumerate()
        .map(|(i, hv)| (popcount_distance(query, hv), i))
        .collect();
    // Ascending: smaller distance is better.
    scored.sort_unstable_by_key(|&(d, _)| d);
    let mut ranks = vec![0usize; n];
    for (rank, (_d, doc_idx)) in scored.into_iter().enumerate() {
        ranks[doc_idx] = rank;
    }
    ranks
}

/// Compute the full rank vector for vec cosine similarity: rank[i] is
/// the position of doc `i` when the corpus is sorted descending by
/// cosine similarity to query. Best match (highest sim) has rank 0.
fn vec_full_ranks(query: &[f32], corpus: &[Vec<f32>]) -> Vec<usize> {
    let n = corpus.len();
    let mut scored: Vec<(f32, usize)> = corpus
        .iter()
        .enumerate()
        .map(|(i, v)| (cosine(query, v), i))
        .collect();
    // Descending: larger similarity is better.
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut ranks = vec![0usize; n];
    for (rank, (_s, doc_idx)) in scored.into_iter().enumerate() {
        ranks[doc_idx] = rank;
    }
    ranks
}

/// Equal-frequency bin a rank value in [0, n) into one of `bins` bins.
/// Rank r maps to bin ⌊r * bins / n⌋, clamped to bins-1.
fn rank_to_bin(rank: usize, n: usize, bins: usize) -> usize {
    let b = (rank * bins) / n;
    if b >= bins { bins - 1 } else { b }
}

/// Estimate mutual information from a list of (bin_a, bin_b) pairs using
/// a B×B joint histogram. Returns MI in nats. Pairs in zero cells of
/// the joint contribute nothing (0 * log 0 = 0 by convention).
fn mutual_information(pairs: &[(usize, usize)], bins: usize) -> f64 {
    if pairs.is_empty() {
        return 0.0;
    }
    let n = pairs.len() as f64;
    let mut joint = vec![vec![0u64; bins]; bins];
    let mut marginal_a = vec![0u64; bins];
    let mut marginal_b = vec![0u64; bins];
    for &(a, b) in pairs {
        joint[a][b] += 1;
        marginal_a[a] += 1;
        marginal_b[b] += 1;
    }
    let mut mi = 0.0;
    for a in 0..bins {
        for b in 0..bins {
            let n_ab = joint[a][b] as f64;
            if n_ab == 0.0 {
                continue;
            }
            let n_a = marginal_a[a] as f64;
            let n_b = marginal_b[b] as f64;
            let p_ab = n_ab / n;
            let p_a = n_a / n;
            let p_b = n_b / n;
            mi += p_ab * (p_ab / (p_a * p_b)).ln();
        }
    }
    mi
}

/// Small splittable LCG for deterministic shuffles in the permutation
/// null. Don't need cryptographic randomness; we just need a stable,
/// dependency-free PRNG for in-place Fisher-Yates.
struct Lcg(u64);
impl Lcg {
    fn new(seed: u64) -> Self {
        // Avoid the zero state.
        Lcg(seed.wrapping_add(0x9E37_79B9_7F4A_7C15))
    }
    fn next_u64(&mut self) -> u64 {
        // SplitMix64.
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn gen_range(&mut self, n: usize) -> usize {
        (self.next_u64() as usize) % n
    }
}

fn fisher_yates_shuffle<T>(slice: &mut [T], rng: &mut Lcg) {
    let n = slice.len();
    for i in (1..n).rev() {
        let j = rng.gen_range(i + 1);
        slice.swap(i, j);
    }
}

#[test]
#[ignore = "Phase 0C — rank-vector MI; downloads ~22MB fastembed model. Run with --ignored --nocapture"]
fn phase_0c_hdc_vs_vec_rank_mutual_information() {
    println!("\n=== Phase 0C — HDC vs vec rank-vector mutual information ===\n");

    // ── 1. Walk LLO daemon source for the corpus (same as 0B) ────────
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
    let n = corpus.len();
    println!("Corpus: {} Rust functions extracted\n", n);

    if n < QUERIES + 1 {
        panic!(
            "corpus too small ({} functions) — need >= QUERIES+1 = {}",
            n,
            QUERIES + 1
        );
    }

    // ── 2. Pull HDC hvs into a flat array ────────────────────────────
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
        "vec embedded {} functions in {:.2?}\n",
        vec_corpus.len(),
        t.elapsed()
    );

    // ── 4. Pick QUERIES query indices spread across the corpus (same
    // stride as 0B for like-for-like comparison) ────────────────────
    let stride = n / QUERIES;
    let query_indices: Vec<usize> = (0..QUERIES).map(|i| i * stride).collect();

    // ── 5. Per-query full rank vectors → pairs ───────────────────────
    // Store per-query r_vec ranks so we can permute them in the null
    // without recomputing cosine similarities.
    let mut per_query_hdc_ranks: Vec<Vec<usize>> = Vec::with_capacity(QUERIES);
    let mut per_query_vec_ranks: Vec<Vec<usize>> = Vec::with_capacity(QUERIES);
    for &q_idx in &query_indices {
        let r_hdc = hdc_full_ranks(&hdc_corpus[q_idx], &hdc_corpus);
        let r_vec = vec_full_ranks(&vec_corpus[q_idx], &vec_corpus);
        per_query_hdc_ranks.push(r_hdc);
        per_query_vec_ranks.push(r_vec);
    }

    // Accumulate observed (bin_a, bin_b) pairs across queries.
    let mut observed_pairs: Vec<(usize, usize)> = Vec::with_capacity(QUERIES * n);
    for (r_hdc, r_vec) in per_query_hdc_ranks.iter().zip(per_query_vec_ranks.iter()) {
        for i in 0..n {
            let a = rank_to_bin(r_hdc[i], n, BINS);
            let b = rank_to_bin(r_vec[i], n, BINS);
            observed_pairs.push((a, b));
        }
    }

    let observed_mi = mutual_information(&observed_pairs, BINS);
    let theoretical_max = (BINS as f64).ln();

    // ── 6. Permutation null ──────────────────────────────────────────
    // For each trial, independently shuffle the r_vec ranks within
    // each query, recompute MI. This destroys the per-doc pairing
    // between r_hdc and r_vec while preserving the marginal rank
    // distributions exactly. Mean/std across trials gives the null.
    let mut rng = Lcg::new(0xC0DE_FACE_DEAD_BEEF);
    let mut null_mis: Vec<f64> = Vec::with_capacity(NULL_TRIALS);
    for _ in 0..NULL_TRIALS {
        let mut perm_pairs: Vec<(usize, usize)> = Vec::with_capacity(QUERIES * n);
        for (r_hdc, r_vec) in per_query_hdc_ranks.iter().zip(per_query_vec_ranks.iter()) {
            // Shuffle a copy of r_vec to break the pairing.
            let mut shuffled = r_vec.clone();
            fisher_yates_shuffle(&mut shuffled, &mut rng);
            for i in 0..n {
                let a = rank_to_bin(r_hdc[i], n, BINS);
                let b = rank_to_bin(shuffled[i], n, BINS);
                perm_pairs.push((a, b));
            }
        }
        null_mis.push(mutual_information(&perm_pairs, BINS));
    }
    let null_mean: f64 = null_mis.iter().sum::<f64>() / null_mis.len() as f64;
    let null_var: f64 = null_mis
        .iter()
        .map(|m| (m - null_mean).powi(2))
        .sum::<f64>()
        / null_mis.len() as f64;
    let null_std = null_var.sqrt();
    let z = if null_std > 0.0 {
        (observed_mi - null_mean) / null_std
    } else {
        0.0
    };

    // ── 7. Report ────────────────────────────────────────────────────
    let to_bits = |nats: f64| nats / std::f64::consts::LN_2;
    let frac_max = observed_mi / theoretical_max;

    println!(
        "N={}, queries={}, bins={} (equal-frequency)",
        n, QUERIES, BINS
    );
    println!("Pairs accumulated: {}\n", observed_pairs.len());

    println!(
        "Observed MI:           {:.4} nats  /  {:.4} bits",
        observed_mi,
        to_bits(observed_mi)
    );
    println!(
        "Theoretical max:       {:.4} nats  /  {:.4} bits",
        theoretical_max,
        to_bits(theoretical_max)
    );
    println!("Fraction of max:       {:.2}%\n", frac_max * 100.0);

    println!("Null distribution ({} permutations):", NULL_TRIALS);
    println!("  Mean:                {:.4} nats", null_mean);
    println!("  Std:                 {:.4} nats", null_std);
    println!("  Observed - null:     {:.2} σ\n", z);

    let interp = if z >= 2.0 {
        "SIGNAL: observed > 2σ above null — backends order the corpus correlatedly"
    } else if z <= -2.0 {
        "ANTI-CORRELATION: observed > 2σ below null — implausible, check for bug"
    } else if frac_max < 0.05 {
        "ORTHOGONAL: observed ≈ null, fraction of max < 5% — backends essentially independent"
    } else {
        "WEAK COUPLING: observed near null but with some shared rank signal"
    };
    println!("Interpretation:");
    println!(
        "  Observed > 2σ above null → backends ordering the corpus correlatedly (signal exists)"
    );
    println!(
        "  Observed ≈ null         → orthogonal axes (no rank-correlation; the multi-modal value prop)"
    );
    println!("  Fraction of max < 5%    → very weak coupling; backends essentially independent\n");
    println!("Result: {}\n", interp);

    // Soft sanity: MI cannot exceed log(B); cannot be negative.
    assert!(
        observed_mi >= 0.0,
        "MI cannot be negative (got {}) — bug in estimator",
        observed_mi
    );
    assert!(
        observed_mi <= theoretical_max + 1e-9,
        "MI cannot exceed log(B)={} (got {}) — bug in estimator",
        theoretical_max,
        observed_mi
    );
}
