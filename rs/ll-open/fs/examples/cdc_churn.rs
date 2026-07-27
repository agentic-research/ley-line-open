//! Cross-generation churn: what a one-commit diff costs CDC under each target.
//!
//! Bead `ley-line-open-b5faa9`. Companion to `cdc_storage_bound`, which
//! measured storage *within* one generation and found the retarget from
//! `nodes.record` to `source_blobs` to be **costless** — 0.0% within-generation
//! dedup either way. Costless is not the same claim as beneficial, and only the
//! first was backed by numbers.
//!
//! CDC's payoff is cross-generation. A rolling hash earns its keep when
//! unchanged regions reproduce identical chunks across a rebuild, so only what
//! moved is re-stored. This measures exactly that: given two projections one
//! commit apart, how many chunks does generation 2 need that generation 1 did
//! not have?
//!
//! **Why no activation.** Both targets are computed directly with
//! `leyline_cdc::chunk`, which is what activation would store. That keeps the
//! comparison honest — identical chunker, identical parameters, only the input
//! differs — and sidesteps `ley-line-open-f7966d`, whose `record JSON` NUMERIC
//! affinity aborts `leyline cdc enable` on real corpora.
//!
//! **What to expect.** `nodes.record` holds AST leaf tokens averaging ~10
//! bytes against a `MIN_CHUNK` of 8 KiB, so the boundary search never fires and
//! every record is one chunk. Any leaf whose text changed is a wholly new
//! chunk; there are no shared boundaries to preserve. `source_blobs` holds
//! whole files at 10–15 KB, so an edit should dirty the chunks it touches and
//! leave the rest byte-identical.
//!
//! Run:
//! ```text
//! cargo run -p leyline-fs --release --no-default-features --features cdc \
//!   --example cdc_churn -- gen1.db gen2.db
//! ```

use std::collections::HashSet;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

/// What one target's chunk store looks like for a single generation.
struct Generation {
    /// Distinct chunk addresses — what `content_chunks` would hold.
    chunks: HashSet<[u8; 32]>,
    /// Total spans — what `content_manifest` would hold, one row each.
    spans: usize,
    /// Bytes fed to the chunker.
    bytes: u64,
    /// Inputs at or above `MIN_CHUNK`, i.e. where the rolling hash can fire.
    over_min: usize,
    /// Inputs considered.
    inputs: usize,
}

fn chunk_all(payloads: impl Iterator<Item = Vec<u8>>) -> Generation {
    let mut acc = Generation {
        chunks: HashSet::new(),
        spans: 0,
        bytes: 0,
        over_min: 0,
        inputs: 0,
    };
    for payload in payloads {
        acc.inputs += 1;
        acc.bytes += payload.len() as u64;
        if payload.len() >= leyline_cdc::MIN_CHUNK {
            acc.over_min += 1;
        }
        for chunk in leyline_cdc::chunk(&payload) {
            acc.spans += 1;
            acc.chunks.insert(*chunk.hash.as_bytes());
        }
    }
    acc
}

fn record_payloads(conn: &Connection) -> Result<Vec<Vec<u8>>> {
    let mut stmt = conn
        .prepare(
            "SELECT CAST(record AS BLOB) FROM nodes \
              WHERE kind = 0 AND record IS NOT NULL",
        )
        .context("prepare nodes.record scan")?;
    let rows = stmt
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .context("scan nodes.record")?;
    Ok(rows.filter_map(Result::ok).collect())
}

fn blob_payloads(conn: &Connection) -> Result<Option<Vec<Vec<u8>>>> {
    let present: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'source_blobs'",
            [],
            |row| row.get(0),
        )
        .optional()
        .context("probe for source_blobs")?;
    if present.is_none() {
        return Ok(None);
    }
    let mut stmt = conn
        .prepare("SELECT blob_bytes FROM source_blobs")
        .context("prepare source_blobs scan")?;
    let rows = stmt
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .context("scan source_blobs")?;
    Ok(Some(rows.filter_map(Result::ok).collect()))
}

fn report(target: &str, g1: &Generation, g2: &Generation) {
    let new: usize = g2.chunks.difference(&g1.chunks).count();
    let retained = g2.chunks.len() - new;
    let churn = if g2.chunks.is_empty() {
        0.0
    } else {
        (new as f64 / g2.chunks.len() as f64) * 100.0
    };
    println!("\n── target: {target} ──");
    println!(
        "  gen1: {:>7} inputs  {:>9} bytes  {:>4} over MIN_CHUNK  {:>7} spans  {:>7} chunks",
        g1.inputs,
        g1.bytes,
        g1.over_min,
        g1.spans,
        g1.chunks.len()
    );
    println!(
        "  gen2: {:>7} inputs  {:>9} bytes  {:>4} over MIN_CHUNK  {:>7} spans  {:>7} chunks",
        g2.inputs,
        g2.bytes,
        g2.over_min,
        g2.spans,
        g2.chunks.len()
    );
    println!(
        "  churn: {new} new chunks, {retained} retained → {churn:.1}% of gen2 is newly stored"
    );
    println!("  manifest rows to rewrite: {}", g2.spans);
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 2 {
        eprintln!("usage: cdc_churn <gen1.db> <gen2.db>");
        std::process::exit(2);
    }
    let (a, b) = (&args[0], &args[1]);
    println!("gen1 = {a}\ngen2 = {b}");
    println!(
        "MIN_CHUNK = {} bytes, MAX_CHUNK = {} bytes",
        leyline_cdc::MIN_CHUNK,
        leyline_cdc::MAX_CHUNK
    );

    let c1 = Connection::open(a).with_context(|| format!("open {a}"))?;
    let c2 = Connection::open(b).with_context(|| format!("open {b}"))?;

    let r1 = chunk_all(record_payloads(&c1)?.into_iter());
    let r2 = chunk_all(record_payloads(&c2)?.into_iter());
    report("nodes.record (what ships today)", &r1, &r2);

    match (blob_payloads(&c1)?, blob_payloads(&c2)?) {
        (Some(b1), Some(b2)) => {
            let s1 = chunk_all(b1.into_iter());
            let s2 = chunk_all(b2.into_iter());
            report("source_blobs (ADR-0028's target)", &s1, &s2);

            let rec_new = r2.chunks.difference(&r1.chunks).count();
            let blob_new = s2.chunks.difference(&s1.chunks).count();
            println!("\n── verdict ──");
            println!("  chunks rewritten per commit: {rec_new} (record) vs {blob_new} (blobs)");
            println!(
                "  manifest rows rewritten:     {} (record) vs {} (blobs)",
                r2.spans, s2.spans
            );
            if r1.over_min == 0 && r2.over_min == 0 {
                println!(
                    "  NOTE: zero record inputs reach MIN_CHUNK, so the rolling hash never\n\
                     \x20       fires on that target — every record is one forced chunk and no\n\
                     \x20       boundary is ever shared. That is arithmetic, not a measurement."
                );
            }
        }
        _ => println!(
            "\n(no source_blobs table in one or both projections — counterfactual skipped)"
        ),
    }
    Ok(())
}
