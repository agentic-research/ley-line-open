//! Phase 0 — RAG throughput head-to-head: HDC popcount-Hamming vs vec cosine-similarity.
//!
//! Answers the user's "is HDC's cosine-sim equivalent competitive at the same
//! rate?" question. Linear top-k scan over a synthetic corpus, both backends
//! in one process, same hardware, same warmup. Random vectors — this measures
//! the *primitive throughput*, not retrieval quality. Top-K agreement /
//! recall-vs-ground-truth is Phase 0B (real corpus, real encoders).
//!
//! ## Why synthetic vectors
//!
//! For "is the scan rate competitive" we don't need real content. What we need
//! is N vectors of representative shape (HDC D=8192 bits, vec D=384 floats)
//! and a linear-scan top-K. The inner loop's behavior on random data is the
//! same as on real data (popcount sees the same bit-distribution, cosine-sim
//! sees the same float distribution). Real-corpus agreement is a separate
//! question that needs real encoders + real ground truth — out of scope here.
//!
//! ## Numbers reported
//!
//! - ns/pair for HDC popcount
//! - ns/pair for vec cosine sim
//! - ms/query for top-K linear scan at N=10k
//! - throughput ratio (which backend wins, by how much, on this hardware)
//!
//! Gated `#[ignore]` so workspace `cargo test` doesn't pull the fastembed
//! model on every CI run. Invoke with:
//!
//! ```sh
//! cargo test -p leyline-cli-lib --features vec --test phase_0_rag_throughput -- --ignored --nocapture
//! ```

#![cfg(feature = "vec")]

use std::time::Instant;

use leyline_hdc::util::{Hypervector, expand_seed, popcount_distance};

/// Corpus size for the scan-rate test. Picked to give multi-ms wall-clock
/// numbers on both backends so noise doesn't dominate; can grow if needed.
const N: usize = 10_000;

/// Top-K for the scan. K=10 is the typical RAG retrieval shape.
const K: usize = 10;

/// Vec embedding dim — matches FastEmbedder's default (BGESmallENV15 family).
const VEC_DIM: usize = 384;

/// Number of queries per backend. Median wall-clock across queries
/// is the headline number; outliers get trimmed in the report.
const QUERIES: usize = 10;

/// Generate a deterministic-from-seed corpus of HDC hypervectors. Each
/// vector is `expand_seed(i as u64)` — the same primitive the HDC encoder
/// uses for canonical-kind leaf vectors, so the bit-distribution matches
/// what production code will see at scan time.
fn hdc_corpus(n: usize) -> Vec<Hypervector> {
    (0..n).map(|i| expand_seed(i as u64 + 1)).collect()
}

/// Generate a deterministic-from-seed corpus of fastembed-shaped vectors.
/// Uses a linear-congruential PRNG seeded by index — pure f32 in [-1, 1],
/// roughly the range of real embeddings. (Real embeddings concentrate
/// near the unit ball but this matters less for scan-rate than for recall.)
fn vec_corpus(n: usize) -> Vec<Vec<f32>> {
    (0..n)
        .map(|i| {
            let mut seed = (i as u64).wrapping_mul(2_862_933_555_777_941_757_u64) ^ 0xDEADBEEF;
            (0..VEC_DIM)
                .map(|_| {
                    seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                    let x = (seed >> 32) as i32;
                    (x as f32) / (i32::MAX as f32)
                })
                .collect()
        })
        .collect()
}

/// Cosine similarity over two equal-dim f32 vectors. Standard
/// `dot / (|a|·|b|)` — what fastembed-backed retrieval does at scan time.
/// Inlined here so the bench measures the same inner-loop shape callers see.
fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|y| y * y).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

/// Heap-based top-K: track the K smallest distances seen. O(N log K).
fn top_k_hdc(query: &Hypervector, corpus: &[Hypervector], k: usize) -> Vec<(u32, usize)> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    let mut heap: BinaryHeap<(u32, usize)> = BinaryHeap::with_capacity(k + 1);
    for (i, hv) in corpus.iter().enumerate() {
        let d = popcount_distance(query, hv);
        if heap.len() < k {
            heap.push((d, i));
        } else if let Some(&(top, _)) = heap.peek() {
            if d < top {
                heap.pop();
                heap.push((d, i));
            }
        }
    }
    let mut out: Vec<(u32, usize)> = heap.into_sorted_vec();
    out.sort_by_key(|&(d, _)| Reverse(std::cmp::Reverse(d)));
    out
}

/// Heap-based top-K for cosine sim. Higher score = better, so we pop the
/// smallest score from the heap each time. O(N log K).
fn top_k_vec(query: &[f32], corpus: &[Vec<f32>], k: usize) -> Vec<(f32, usize)> {
    use std::collections::BinaryHeap;
    // BinaryHeap is max-heap by default. We want to keep the K largest
    // similarities, so flip signs: push -sim, pop the most-negative (= smallest sim).
    let mut heap: BinaryHeap<(i64, usize)> = BinaryHeap::with_capacity(k + 1);
    for (i, v) in corpus.iter().enumerate() {
        let sim = cosine_sim(query, v);
        // Encode as i64 so BinaryHeap orders correctly across NaNs.
        let key = -((sim * 1_000_000_000.0) as i64);
        if heap.len() < k {
            heap.push((key, i));
        } else if let Some(&(top, _)) = heap.peek()
            && key < top
        {
            heap.pop();
            heap.push((key, i));
        }
    }
    let mut out: Vec<(f32, usize)> = heap
        .into_sorted_vec()
        .iter()
        .map(|&(k, i)| (-(k as f32) / 1_000_000_000.0, i))
        .collect();
    out.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// Sorted-median over a Vec of nanosecond timings. Trims nothing — the
/// caller picks the percentile. Returns `(p50, p95)` for the headline
/// "typical query" + "tail latency" pair.
fn p50_p95(mut samples: Vec<u128>) -> (u128, u128) {
    samples.sort_unstable();
    let p50 = samples[samples.len() / 2];
    let p95 = samples[(samples.len() * 95) / 100];
    (p50, p95)
}

#[test]
#[ignore = "Phase 0 RAG-throughput head-to-head — run explicitly with --ignored --nocapture"]
fn phase_0_hdc_vs_vec_scan_rate() {
    // Build corpora once; these are the constants the per-query scans see.
    println!("\n=== Phase 0 — HDC vs vec linear-scan throughput ===");
    println!("N = {N}, K = {K}, VEC_DIM = {VEC_DIM}, queries = {QUERIES}\n");

    let t0 = Instant::now();
    let hdc_c = hdc_corpus(N);
    let hdc_build = t0.elapsed();
    println!(
        "HDC corpus built in {:?} ({} hvs @ 1024 bytes)",
        hdc_build,
        hdc_c.len()
    );

    let t0 = Instant::now();
    let vec_c = vec_corpus(N);
    let vec_build = t0.elapsed();
    println!(
        "vec corpus built in {:?} ({} vectors @ {} floats = {} bytes)",
        vec_build,
        vec_c.len(),
        VEC_DIM,
        VEC_DIM * 4
    );

    // Warmup: one full scan each, results discarded. Lets the CPU caches
    // settle and the branch predictor warm up before timed runs.
    {
        let q = expand_seed(0xCAFE_0000);
        let _ = top_k_hdc(&q, &hdc_c, K);
        let q: Vec<f32> = (0..VEC_DIM).map(|i| (i as f32) * 0.001).collect();
        let _ = top_k_vec(&q, &vec_c, K);
    }

    // ── HDC timed runs ──────────────────────────────────────────────────
    let mut hdc_query_ns: Vec<u128> = Vec::with_capacity(QUERIES);
    for q_seed in 0..QUERIES as u64 {
        let q = expand_seed(0xCAFE_0000 + q_seed);
        let t = Instant::now();
        let top = top_k_hdc(&q, &hdc_c, K);
        let elapsed = t.elapsed();
        hdc_query_ns.push(elapsed.as_nanos());
        // Light sanity check: top-K not empty.
        assert_eq!(top.len(), K, "top-K must return K results");
    }

    // ── vec timed runs ──────────────────────────────────────────────────
    let mut vec_query_ns: Vec<u128> = Vec::with_capacity(QUERIES);
    for q_seed in 0..QUERIES {
        let q: Vec<f32> = (0..VEC_DIM)
            .map(|i| ((i + q_seed) as f32 * 0.013).sin())
            .collect();
        let t = Instant::now();
        let top = top_k_vec(&q, &vec_c, K);
        let elapsed = t.elapsed();
        vec_query_ns.push(elapsed.as_nanos());
        assert_eq!(top.len(), K, "top-K must return K results");
    }

    let (hdc_p50, hdc_p95) = p50_p95(hdc_query_ns.clone());
    let (vec_p50, vec_p95) = p50_p95(vec_query_ns.clone());

    let hdc_ns_per_pair = hdc_p50 as f64 / N as f64;
    let vec_ns_per_pair = vec_p50 as f64 / N as f64;
    let ratio = vec_ns_per_pair / hdc_ns_per_pair;

    println!("\n=== Results ===");
    println!("                    p50 (full scan)   p95              ns/pair   relative");
    println!(
        "HDC popcount        {:>10.3} ms   {:>10.3} ms   {:>6.2}    1.00× (baseline)",
        hdc_p50 as f64 / 1_000_000.0,
        hdc_p95 as f64 / 1_000_000.0,
        hdc_ns_per_pair,
    );
    println!(
        "vec cosine sim      {:>10.3} ms   {:>10.3} ms   {:>6.2}    {:.2}×",
        vec_p50 as f64 / 1_000_000.0,
        vec_p95 as f64 / 1_000_000.0,
        vec_ns_per_pair,
        ratio,
    );
    println!();
    println!(
        "Interpretation: {}",
        if ratio > 1.0 {
            format!(
                "HDC is {:.2}× FASTER per pair at this corpus size on this hardware.",
                ratio
            )
        } else {
            format!(
                "vec is {:.2}× FASTER per pair at this corpus size on this hardware.",
                1.0 / ratio
            )
        }
    );
    println!(
        "Per-byte rate: HDC popcount = 32× bits/byte vs vec cosine = 8× bits/byte (1 f32 = 4 bytes);"
    );
    println!("at fixed storage cost HDC carries more representational capacity per scanned byte.");
    println!();
}
