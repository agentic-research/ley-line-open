//! Measures what CDC actually costs and actually saves on a REAL projection,
//! so the trade is settled by numbers instead of by arithmetic off the schema.
//!
//! CDC shipped without a storage gate. Every comparable substrate ADR pinned
//! one — ADR-0026 F4, ADR-0028 F4s/F5s — and the ~2x figure quoted for CDC was
//! read off `chunked.rs` (`nodes.record` is never nulled, so content is stored
//! twice minus dedup), never measured. This example is the missing gate:
//!
//! * **F4c — storage bound.** Arena bytes with CDC vs without, on a corpus you
//!   name. The comparison is made on a `VACUUM`ed database at both ends,
//!   because SQLite retains freed pages until compaction and an un-`VACUUM`ed
//!   file size measures history rather than content (the same caveat
//!   [`leyline_fs::gc::GcReport`] documents for `deleted_chunk_bytes`).
//!   `SUM(length(chunk_bytes))` is reported alongside it as the payload-only
//!   number that cannot be inflated by page slack.
//! * **F5c — measured dedup.** Cross-FILE dedup (the same chunk reached from
//!   two manifests in one generation) and cross-GENERATION dedup (the same
//!   chunk still present after every freshness witness has moved on) are
//!   reported separately, because ADR-0028's F4s/F5s treat them separately and
//!   they have different value stories.
//!
//! ## Reading the output honestly
//!
//! The ratio worth arguing about is not `chunk_bytes / record_bytes`. Chunk
//! payload is only one of five objects CDC adds: `content_chunks`,
//! `content_manifest`, `content_manifest_meta`, and two indexes over the
//! manifest. Every manifest and witness row repeats `node_id`, which in this
//! projection is an integer nid — one 8-byte key, where the pre-v5 schema
//! measured below repeated the node's full AST path on every row.
//! `cdc_bytes_per_content_byte` is therefore
//! printed as the headline cost — total arena growth divided by the content
//! CDC was asked to index — and `record_max` / `records_over_min_chunk` are
//! printed next to it so a reader can see whether content-defined chunking
//! ever engaged at all. A record at or below [`leyline_cdc::MIN_CHUNK`] yields
//! exactly one chunk covering the whole record: no boundary search runs, and
//! "dedup" degenerates to interning identical strings.
//!
//! ## Usage
//!
//! ```text
//! leyline parse <corpus> -o corpus.db
//! cargo run -p leyline-fs --release --no-default-features --features cdc \
//!     --example cdc_storage_bound -- corpus.db [more.db ...]
//! ```
//!
//! Each input is copied to a scratch file first, so the run is non-destructive
//! and re-runnable against the same projection.
//!
//! ## Measured 2026-07-27, aarch64 (M-series), SQLite 3.51.3
//!
//! Three corpora, each `leyline parse`d from a `git archive` snapshot so the
//! run is reproducible from a commit id.
//!
//! | corpus  | commit  | files | source bytes | AST leaves | leaf bytes | leaf max |
//! |---------|---------|------:|-------------:|-----------:|-----------:|---------:|
//! | mache   | 7814366 |   756 |    7,497,928 |    391,556 |  3,820,618 |    5,633 |
//! | rosary  | 541f55b |   283 |    4,246,328 |    208,195 |  2,092,662 |    4,347 |
//! | LLO rs/ | 05a6cbf |   354 |    5,425,395 |    254,472 |  2,037,652 |    3,270 |
//!
//! ### F4c — storage bound (measured PRE-v5)
//!
//! These figures were taken against the pre-v5 schema, where `nodes.id` and
//! every reference to it was the node's full ancestry path. They are what
//! motivated projection-v5 (bead `ley-line-open-17c271`) and are kept
//! verbatim as the before-measurement; re-running this harness now reports
//! the after.
//!
//! | corpus  | arena without CDC | arena with CDC | ratio  | growth      | growth / content byte |
//! |---------|------------------:|---------------:|-------:|------------:|----------------------:|
//! | mache   |     2,016,018,432 |  2,419,392,512 | 1.200x | 403,374,080 |               105.6x |
//! | rosary  |       860,028,928 |  1,046,544,384 | 1.217x | 186,515,456 |                89.1x |
//! | LLO rs/ |     1,114,439,680 |  1,355,677,696 | 1.216x | 241,238,016 |               118.4x |
//!
//! The 2.1x gate PASSES on all three, for a reason that does not flatter CDC:
//! the AST projection is already 269x / 203x / 205x its own source, so a third
//! of a gigabyte of manifest disappears into a two-gigabyte denominator.
//! Divided by the content CDC was asked to index, the same growth is 89x-118x.
//!
//! `dbstat` attributes mache's 403,374,080 exactly, and the shape is the
//! finding: `content_manifest` 86.1 MB, `content_manifest_meta` 77.3 MB, its
//! PK autoindex 73.3 MB, `content_manifest`'s PK autoindex 72.7 MB,
//! `content_manifest_span` 72.7 MB, `content_manifest_chunk_hash` 15.8 MB —
//! and `content_chunks`, the actual payload, **3.8 MB**. Chunk bytes are
//! 0.48% / 0.56% / 0.31% of what CDC costs. The other 99.5% was the node's
//! path repeated across two tables and four indexes — which is precisely the
//! freight projection-v5 evicted by keying on an integer nid instead.
//!
//! ### F5c — measured dedup
//!
//! | corpus  | spans -> chunks | span reuse | content bytes -> unique | deduped | bytes saved | bytes spent |
//! |---------|----------------:|-----------:|------------------------:|--------:|------------:|------------:|
//! | mache   | 384,182 -> 40,102 |     9.58x | 3,820,618 -> 1,941,028 |  49.2% |   1,879,590 | 403,374,080 |
//! | rosary  | 204,366 -> 22,650 |     9.02x | 2,092,662 -> 1,043,261 |  50.2% |   1,049,401 | 186,515,456 |
//! | LLO rs/ | 253,749 -> 23,605 |    10.75x | 2,037,652 ->   747,914 |  63.3% |   1,289,738 | 241,238,016 |
//!
//! Cross-generation holds cleanly, in all three senses:
//!
//! 1. **No-op re-activation.** Every node takes the `AlreadyFresh` path;
//!    zero chunks added. (All three corpora.)
//! 2. **Full reparse, identical content.** Every freshness witness
//!    invalidated, so activation repopulates all 208,195 (rosary) / 254,472
//!    (LLO) manifests from scratch — and `unique_chunk_rows` /
//!    `unique_chunk_bytes` come back bit-identical, with GC finding nothing
//!    unreachable.
//! 3. **A real generation change.** mache 7814366 -> 7728f4a in place
//!    (90 files changed, +2,387/-801), reparsed into the same database and
//!    re-activated: the chunk store grew by **72 rows / 10,768 bytes**, 0.55%,
//!    to absorb the whole diff. GC then reaped 8,553 dead manifest rows and
//!    9,269 witness rows and released 375 chunks / 29,791 bytes, leaving the
//!    store net SMALLER than gen 1 (39,799 rows / 1,922,005 bytes). The arena
//!    grew 19,333,120 bytes across the generation — manifest churn, not
//!    payload.
//!
//! Content addressing does exactly what it claims. It just is not worth what
//! it costs here: dedup saves 1-1.9 MB and spends 186-403 MB — between 178x
//! and 215x more than it recovers.
//!
//! ### Why the ratio is what it is
//!
//! `records_over_min_chunk` is **0** on all three corpora. No `nodes.record`
//! reaches [`leyline_cdc::MIN_CHUNK`] — the largest observed is 5,633 bytes
//! against an 8,192-byte floor — so `leyline_cdc::chunk` returns one chunk
//! covering the whole record and the GearHash never rolls. Content-defined
//! chunking is not being exercised at all. What is measured is a
//! hash-addressed intern table over ~10-byte AST tokens (mean 9.8 on mache),
//! keyed by a 32-byte BLAKE3 and — pre-v5 — indexed by a node path that
//! averaged 176 bytes. Path bytes per content byte measured 19.8 on LLO and
//! 14.8 on rosary: the address was an order of magnitude larger than the
//! thing addressed, and it was stored in `content_manifest`, in
//! `content_manifest_meta`, and in four index B-trees over them. Under
//! projection-v5 each of those carries an 8-byte nid instead, and the path
//! vocabulary is interned once in `names` — which is what `locator_bytes`
//! now reports.
//!
//! ### Counterfactual — the target ADR-0028 names
//!
//! ADR-0028 says a CDC layer "would chunk source blobs". Measured:
//!
//! | corpus  | blobs | bytes     | over MIN_CHUNK | spans | unique | deduped |
//! |---------|------:|----------:|---------------:|------:|-------:|--------:|
//! | mache   |   751 | 7,497,928 |            287 | 1,195 |  1,195 |    0.0% |
//! | rosary  |   283 | 4,246,328 |            157 |   573 |    573 |    0.0% |
//! | LLO rs/ |   354 | 5,425,395 |            172 |   745 |    745 |    0.0% |
//!
//! Whole-file dedup inside one generation of one repo is nil on all three,
//! exactly as ADR-0028 predicted ("Two files with byte-identical content: two
//! rows").
//!
//! The win is on the other axis. The manifest would be **1,195 rows instead
//! of 384,182** (573 instead of 204,366; 745 instead of 253,749) — a 321x /
//! 357x / 341x cut in the bookkeeping that is currently 99.5% of the cost.
//! And at 1.6-2.1 chunks per file the incremental story is real if modest: an
//! edit re-stores roughly half a file rather than all of it. That is where
//! CDC's value actually lives, and it is not where CDC is pointed.
//!
//! The file-size distribution is worth stating plainly: these blobs average
//! 10-15 KB against xet's 64 KB target chunk size. The parameters this crate
//! borrowed are tuned for model weights, not source trees.
//!
//! Activation cost, for the record: 5m11s / 3m45s / 5m00s of wall clock to
//! process 3.8 / 2.1 / 2.0 MB — roughly 10 KiB/s, against a chunker that
//! `examples/throughput.rs` in `leyline-cdc` clocks in the hundreds of MiB/s.
//! The bottleneck is one IMMEDIATE transaction per AST leaf.

use anyhow::{Context, Result, bail};
use leyline_fs::activation::{ActivationOptions, ActivationReport, activate_chunked_content};
use leyline_fs::gc::{GcOptions, collect_unreachable_chunks};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Falsification threshold from bead `ley-line-open-b5faa9`: chunked arena
/// size must stay within this multiple of the unchunked arena.
const F4C_ARENA_BOUND: f64 = 2.1;

fn main() -> Result<()> {
    let inputs: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if inputs.is_empty() {
        bail!(
            "usage: cdc_storage_bound <projection.db> [more.db ...]\n\
             produce one with: leyline parse <corpus> -o projection.db"
        );
    }
    println!(
        "arch={} sqlite={} min_chunk={} max_chunk={}",
        std::env::consts::ARCH,
        rusqlite::version(),
        leyline_cdc::MIN_CHUNK,
        leyline_cdc::MAX_CHUNK,
    );
    for input in &inputs {
        measure(input)?;
    }
    Ok(())
}

/// Everything the baseline database can say about itself before CDC exists.
struct Census {
    eligible_nodes: u64,
    record_bytes: u64,
    record_max: u64,
    records_over_min_chunk: u64,
    /// Total bytes of path material the projection stores, across the whole
    /// arena.
    ///
    /// Under projection-v5 that is the interned vocabulary in `names` and
    /// nothing else: a node is keyed by an integer nid, and its path is
    /// rendered from the `dirs`/`files` chain at read time rather than
    /// repeated per row. This is the number the pre-v5 measurement below
    /// reported as `node_id_bytes` — 19.8 bytes of path per byte of content
    /// on LLO — and the reason the key was changed.
    locator_bytes: u64,
    /// Rows whose `nodes.size` disagrees with `length(CAST(record AS BLOB))`.
    ///
    /// Non-zero on every real projection measured so far, because `nodes.record`
    /// is declared `JSON` — a type name SQLite gives NUMERIC affinity, so a leaf
    /// token that looks like a number is coerced out of TEXT on insert.
    /// Activation refuses those rows by design, which means it refuses the whole
    /// database. Reported rather than silently repaired.
    size_witness_mismatch: u64,
}

fn measure(input: &Path) -> Result<()> {
    let work = scratch_path(input);
    std::fs::copy(input, &work).with_context(|| format!("copy {} to scratch", input.display()))?;
    let conn = Connection::open(&work).with_context(|| format!("open {}", work.display()))?;

    println!("\n================ {} ================", input.display());
    let census = census(&conn)?;
    if census.size_witness_mismatch > 0 {
        // Repairing is the only way to reach any number at all, so it happens
        // — but the count is reported, not absorbed.
        conn.execute(
            "UPDATE nodes SET size = length(CAST(record AS BLOB)) \
              WHERE kind = 0 AND record IS NOT NULL \
                AND size <> length(CAST(record AS BLOB))",
            [],
        )
        .context("repair size witnesses")?;
    }
    // Both size readings are taken after the repair and after `VACUUM`, so the
    // ratio compares two compacted databases that differ only by CDC.
    vacuum(&conn)?;
    let baseline_bytes = file_bytes(&work)?;
    report_census(&census, baseline_bytes);

    let started = Instant::now();
    let first = activate_chunked_content(&conn, ActivationOptions::default())
        .context("first CDC activation")?;
    let activate_elapsed = started.elapsed();
    vacuum(&conn)?;
    let chunked_bytes = file_bytes(&work)?;

    println!("\n-- activation");
    println!(
        "  populated={} already_fresh={} processed_source_bytes={} in {:?} ({:.0} KiB/s)",
        first.populated_nodes,
        first.already_fresh_nodes,
        first.processed_source_bytes,
        activate_elapsed,
        (first.processed_source_bytes as f64 / 1024.0) / activate_elapsed.as_secs_f64(),
    );
    println!(
        "  manifest_rows={} unique_chunk_rows={} unique_chunk_bytes={}",
        first.manifest_rows, first.unique_chunk_rows, first.unique_chunk_bytes,
    );

    report_f4c(&census, baseline_bytes, chunked_bytes, &first);
    report_f5c(&conn, &first, baseline_bytes, chunked_bytes)?;

    report_source_blob_counterfactual(&conn, &census)?;

    let gc = collect_unreachable_chunks(&conn, GcOptions { dry_run: true })
        .context("final GC dry run")?;
    println!(
        "\n-- gc (dry run): unreachable_rows={} unreachable_bytes={} reaped_manifest_rows={}",
        gc.unreachable_chunk_rows, gc.unreachable_chunk_bytes, gc.reaped_manifest_rows,
    );

    drop(conn);
    let _ = std::fs::remove_file(&work);
    Ok(())
}

fn report_census(census: &Census, baseline_bytes: u64) {
    println!("-- baseline (VACUUMed)");
    println!("  arena_bytes={baseline_bytes}");
    println!(
        "  eligible_nodes={} record_bytes={} record_max={} records_over_min_chunk={}",
        census.eligible_nodes,
        census.record_bytes,
        census.record_max,
        census.records_over_min_chunk,
    );
    println!(
        "  locator_bytes={} ({:.2} bytes of interned path per byte of content)",
        census.locator_bytes,
        census.locator_bytes as f64 / census.record_bytes.max(1) as f64,
    );
    if census.records_over_min_chunk == 0 {
        println!(
            "  NOTE: no record exceeds MIN_CHUNK ({}), so leyline_cdc::chunk returns a single \
             chunk per node and no boundary search ever runs — this measures whole-value \
             content addressing, not content-defined chunking",
            leyline_cdc::MIN_CHUNK,
        );
    }
    println!("  size_witness_mismatch={}", census.size_witness_mismatch);
    if census.size_witness_mismatch > 0 {
        println!(
            "  !! {} size witnesses were REPAIRED so activation could run at all; \
             `leyline cdc enable` fails closed on this database as shipped, and the \
             underlying projection defect is not fixed by this example",
            census.size_witness_mismatch,
        );
    }
}

/// F4c: does turning CDC on keep the arena within [`F4C_ARENA_BOUND`]?
fn report_f4c(census: &Census, baseline: u64, chunked: u64, report: &ActivationReport) {
    let arena_ratio = chunked as f64 / baseline.max(1) as f64;
    let logical_ratio = (census.record_bytes + report.unique_chunk_bytes) as f64
        / census.record_bytes.max(1) as f64;
    let growth = chunked.saturating_sub(baseline);
    println!("\n-- F4c storage bound");
    println!("  arena_bytes_without_cdc={baseline}");
    println!("  arena_bytes_with_cdc={chunked}");
    println!(
        "  unique_chunk_bytes={} (SUM(length(chunk_bytes)) — payload only, immune to page slack)",
        report.unique_chunk_bytes,
    );
    println!("  arena_ratio={arena_ratio:.3}x (gate: <= {F4C_ARENA_BOUND}x)");
    println!(
        "  logical_content_ratio={logical_ratio:.3}x  \
         (record_bytes + unique_chunk_bytes) / record_bytes"
    );
    println!(
        "  cdc_bytes_per_content_byte={:.1}x  \
         arena growth {growth} to index {} content bytes",
        growth as f64 / census.record_bytes.max(1) as f64,
        census.record_bytes,
    );
    println!(
        "  chunk_payload_share_of_growth={:.1}%  \
         (the rest is manifest + witness + index)",
        100.0 * report.unique_chunk_bytes as f64 / growth.max(1) as f64,
    );
    println!(
        "  VERDICT(arena): {}",
        if arena_ratio <= F4C_ARENA_BOUND {
            "PASS"
        } else {
            "FAIL"
        }
    );
}

/// F5c: cross-file and cross-generation dedup, measured separately.
fn report_f5c(
    conn: &Connection,
    first: &ActivationReport,
    baseline: u64,
    chunked: u64,
) -> Result<()> {
    println!("\n-- F5c dedup");
    let span_reuse = first.manifest_rows as f64 / first.unique_chunk_rows.max(1) as f64;
    let byte_ratio = first.processed_source_bytes as f64 / first.unique_chunk_bytes.max(1) as f64;
    println!("  CROSS-FILE (one generation)");
    println!(
        "    manifest_rows={} -> unique_chunk_rows={} = {span_reuse:.2}x span reuse",
        first.manifest_rows, first.unique_chunk_rows,
    );
    println!(
        "    processed_source_bytes={} -> unique_chunk_bytes={} = {byte_ratio:.2}x, \
         {:.1}% of content bytes deduplicated away",
        first.processed_source_bytes,
        first.unique_chunk_bytes,
        100.0
            * (1.0 - first.unique_chunk_bytes as f64 / first.processed_source_bytes.max(1) as f64),
    );
    println!(
        "    bytes saved by dedup={} vs arena growth={} -> net {}",
        first
            .processed_source_bytes
            .saturating_sub(first.unique_chunk_bytes),
        chunked.saturating_sub(baseline),
        if first
            .processed_source_bytes
            .saturating_sub(first.unique_chunk_bytes)
            > chunked.saturating_sub(baseline)
        {
            "SAVING"
        } else {
            "COST"
        },
    );

    println!("  CROSS-GENERATION");
    let unchanged = activate_chunked_content(conn, ActivationOptions::default())
        .context("re-activate unchanged corpus")?;
    println!(
        "    no-op re-activation: populated={} already_fresh={} new_chunk_rows={} ({})",
        unchanged.populated_nodes,
        unchanged.already_fresh_nodes,
        unchanged
            .unique_chunk_rows
            .saturating_sub(first.unique_chunk_rows),
        if unchanged.populated_nodes == 0 {
            "AlreadyFresh path confirmed"
        } else {
            "AlreadyFresh path DID NOT hold"
        },
    );

    // A real reparse rewrites every `nodes` row, so every freshness witness
    // moves on and activation must redo all the work. Bumping `mtime` models
    // exactly that at the chunk layer without needing a second corpus: the
    // content is byte-identical, so a content-addressed store must add zero
    // new chunks while a path-addressed one would double.
    conn.execute(
        "UPDATE nodes SET mtime = mtime + 1 WHERE kind = 0 AND record IS NOT NULL",
        [],
    )
    .context("invalidate every freshness witness")?;
    let restated = activate_chunked_content(conn, ActivationOptions::default())
        .context("re-activate after witness invalidation")?;
    println!(
        "    full reparse (every witness stale, content identical): populated={} \
         new_chunk_rows={} new_chunk_bytes={} ({})",
        restated.populated_nodes,
        restated
            .unique_chunk_rows
            .saturating_sub(first.unique_chunk_rows),
        restated
            .unique_chunk_bytes
            .saturating_sub(first.unique_chunk_bytes),
        if restated.unique_chunk_rows == first.unique_chunk_rows {
            "zero new chunks — cross-generation dedup holds"
        } else {
            "NEW CHUNKS APPEARED — cross-generation dedup does NOT hold"
        },
    );
    Ok(())
}

/// What CDC would find if it were pointed at whole-file source instead of AST
/// leaf tokens.
///
/// ADR-0028 names `source_blobs` as the thing a future CDC layer would chunk
/// ("Content-defined chunking … is a possible Phase 3 layer that would give
/// sub-file dedup for free"). What shipped chunks `nodes.record`. The two are
/// not the same corpus and they are not the same size class, so the difference
/// is worth a number rather than an argument. This section chunks the
/// `source_blobs` payload in memory and reports what a chunk store over it
/// would hold. It writes nothing.
///
/// Skipped silently when the projection has no `source_blobs` table — a
/// database written by another producer legitimately does not.
fn report_source_blob_counterfactual(conn: &Connection, census: &Census) -> Result<()> {
    let present: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master \
              WHERE type = 'table' AND name = 'source_blobs'",
            [],
            |row| row.get(0),
        )
        .context("probe for source_blobs")?;
    if !present {
        return Ok(());
    }
    let mut statement = conn
        .prepare("SELECT blob_bytes FROM source_blobs")
        .context("prepare source_blobs scan")?;
    let rows = statement
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .context("scan source_blobs")?;

    let mut blobs = 0_u64;
    let mut total_bytes = 0_u64;
    let mut spans = 0_u64;
    let mut over_min_chunk = 0_u64;
    let mut unique: std::collections::HashMap<leyline_core::Hash, usize> =
        std::collections::HashMap::new();
    for row in rows {
        let bytes = row.context("decode source blob")?;
        blobs += 1;
        total_bytes += bytes.len() as u64;
        if bytes.len() > leyline_cdc::MIN_CHUNK {
            over_min_chunk += 1;
        }
        for c in leyline_cdc::chunk(&bytes) {
            spans += 1;
            unique.insert(c.hash, c.len);
        }
    }
    let unique_bytes: u64 = unique.values().map(|len| *len as u64).sum();
    println!("\n-- counterfactual: CDC over source_blobs (ADR-0028's named target)");
    println!(
        "  blobs={blobs} bytes={total_bytes} over_min_chunk={over_min_chunk} \
         (vs {} AST records over MIN_CHUNK)",
        census.records_over_min_chunk,
    );
    println!(
        "  spans={spans} unique_chunks={} unique_chunk_bytes={unique_bytes} \
         ({:.2}x span reuse, {:.1}% of bytes deduplicated)",
        unique.len(),
        spans as f64 / unique.len().max(1) as f64,
        100.0 * (1.0 - unique_bytes as f64 / total_bytes.max(1) as f64),
    );
    println!(
        "  manifest rows a source_blobs manifest would need: {spans} \
         (vs the AST manifest's row per leaf)",
    );
    Ok(())
}

fn census(conn: &Connection) -> Result<Census> {
    let mut census = Census {
        eligible_nodes: scalar(
            conn,
            "SELECT COUNT(*) FROM nodes WHERE kind = 0 AND record IS NOT NULL",
        )?,
        record_bytes: scalar(
            conn,
            "SELECT COALESCE(SUM(length(CAST(record AS BLOB))), 0) FROM nodes \
              WHERE kind = 0 AND record IS NOT NULL",
        )?,
        record_max: scalar(
            conn,
            "SELECT COALESCE(MAX(length(CAST(record AS BLOB))), 0) FROM nodes \
              WHERE kind = 0 AND record IS NOT NULL",
        )?,
        records_over_min_chunk: 0,
        locator_bytes: scalar(conn, "SELECT COALESCE(SUM(length(text)), 0) FROM names")?,
        size_witness_mismatch: scalar(
            conn,
            "SELECT COUNT(*) FROM nodes WHERE kind = 0 AND record IS NOT NULL \
               AND size <> length(CAST(record AS BLOB))",
        )?,
    };
    census.records_over_min_chunk = scalar(
        conn,
        &format!(
            "SELECT COUNT(*) FROM nodes WHERE kind = 0 AND record IS NOT NULL \
               AND length(CAST(record AS BLOB)) > {}",
            leyline_cdc::MIN_CHUNK
        ),
    )?;
    Ok(census)
}

fn scalar(conn: &Connection, sql: &str) -> Result<u64> {
    let value: i64 = conn
        .query_row(sql, [], |row| row.get(0))
        .with_context(|| format!("query: {sql}"))?;
    u64::try_from(value).context("negative count")
}

/// `VACUUM` before every size reading. SQLite keeps freed pages on the
/// freelist, so an un-compacted file size measures what the database once
/// held, not what it holds.
fn vacuum(conn: &Connection) -> Result<()> {
    conn.execute_batch("VACUUM").context("vacuum")
}

fn file_bytes(path: &Path) -> Result<u64> {
    Ok(std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .len())
}

fn scratch_path(input: &Path) -> PathBuf {
    let mut name = input.file_name().unwrap_or_default().to_os_string();
    name.push(".cdc-storage-bound.scratch");
    input.with_file_name(name)
}
