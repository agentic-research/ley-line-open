//! Criterion benches for the load-bearing HDC operations.
//!
//! Run with `cargo bench -p leyline-hdc`. Outputs land in
//! `target/criterion/`. Compare runs by passing `--save-baseline <name>`
//! and `--baseline <name>` on subsequent runs.
//!
//! Three benchmark groups:
//! 1. `popcount_distance` — the hottest inner-loop primitive.
//! 2. `encode_tree` — cold vs warm cache, depth 5/7/9 trees.
//! 3. `bundle_majority` — both Rust impl and SQL UDF, increasing N.

use std::hint::black_box;
use std::sync::Mutex;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use leyline_hdc::canonical::CanonicalKind;
use leyline_hdc::codebook::AstCodebook;
use leyline_hdc::encoder::{encode_tree, EncoderNode, SubtreeCache};
use leyline_hdc::sheaf::HvCellComplex;
use leyline_hdc::sql_udf::register_hdc_udfs;
use leyline_hdc::util::{expand_seed, popcount_distance, Hypervector};
use leyline_hdc::D_BYTES;

/// Build a balanced ternary-fanout tree of given depth.
/// Depth 5 ≈ 121 nodes (3⁵+...+1), depth 7 ≈ 1093, depth 9 ≈ 9841.
fn balanced_tree(depth: usize) -> EncoderNode {
    if depth == 0 {
        return EncoderNode::leaf(CanonicalKind::Op);
    }
    EncoderNode::new(
        CanonicalKind::Block,
        vec![
            balanced_tree(depth - 1),
            balanced_tree(depth - 1),
            balanced_tree(depth - 1),
        ],
    )
}

fn count_nodes(n: &EncoderNode) -> usize {
    1 + n.children.iter().map(count_nodes).sum::<usize>()
}

fn bench_popcount(c: &mut Criterion) {
    let a = expand_seed(0xCAFE);
    let b = expand_seed(0xBEEF);

    let mut g = c.benchmark_group("popcount_distance");
    g.throughput(Throughput::Bytes(D_BYTES as u64 * 2));
    g.bench_function("D=8192", |bencher| {
        bencher.iter(|| popcount_distance(black_box(&a), black_box(&b)));
    });
    g.finish();
}

fn bench_encode(c: &mut Criterion) {
    let cb = AstCodebook::new();
    let mut g = c.benchmark_group("encode_tree");
    for depth in [5usize, 7, 9] {
        let tree = balanced_tree(depth);
        let nodes = count_nodes(&tree);
        g.throughput(Throughput::Elements(nodes as u64));
        g.bench_with_input(BenchmarkId::new("cold", depth), &tree, |bencher, t| {
            bencher.iter(|| {
                let cache = SubtreeCache::new();
                encode_tree(black_box(t), black_box(&cb), &cache)
            });
        });
        // Warm cache: every subtree already cached after the first encode.
        let cache = SubtreeCache::new();
        encode_tree(&tree, &cb, &cache);
        g.bench_with_input(BenchmarkId::new("warm", depth), &tree, |bencher, t| {
            bencher.iter(|| encode_tree(black_box(t), black_box(&cb), &cache));
        });
    }
    g.finish();
}

fn bench_bundle_majority(c: &mut Criterion) {
    let mut g = c.benchmark_group("bundle_majority");
    for n in [10usize, 100, 1000] {
        let stalks: Vec<Hypervector> = (0..n).map(|i| expand_seed(i as u64 + 1)).collect();
        g.throughput(Throughput::Elements(n as u64));

        // Rust impl.
        g.bench_with_input(BenchmarkId::new("rust", n), &stalks, |bencher, s| {
            bencher.iter(|| HvCellComplex::bundle_majority(black_box(s)));
        });

        // SQL UDF on a fresh in-memory connection — registration overhead
        // is one-shot per run, but bencher iterates the SELECT only.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        register_hdc_udfs(&conn).unwrap();
        conn.execute("CREATE TABLE hvs(hv BLOB NOT NULL)", []).unwrap();
        for s in &stalks {
            conn.execute("INSERT INTO hvs(hv) VALUES (?1)", [s.as_slice()]).unwrap();
        }
        // Wrap in a Mutex so the closure can hold a non-Sync ref across
        // criterion's iteration model.
        let conn = Mutex::new(conn);
        g.bench_with_input(BenchmarkId::new("sql", n), &n, |bencher, _| {
            bencher.iter(|| {
                let c = conn.lock().unwrap();
                let bytes: Vec<u8> = c
                    .query_row("SELECT BUNDLE_MAJORITY(hv) FROM hvs", [], |r| r.get(0))
                    .unwrap();
                black_box(bytes)
            });
        });
    }
    g.finish();
}

criterion_group!(benches, bench_popcount, bench_encode, bench_bundle_majority);
criterion_main!(benches);
