//! Falsifiers for the `source_blobs` CDC activation target: the sub-floor
//! skip policy, complete tiling + byte-identical range reads, and GC's
//! reachability over the blob manifest.

#![cfg(feature = "cdc")]

use leyline_core::{ContentAddressed, Hash};
use leyline_fs::activation::{ActivationOptions, activate_chunked_source_blobs};
use leyline_fs::blob_chunked::{
    blob_chunks_touched, create_blob_chunked_schema, read_blob_range,
    store_blob_chunked_in_transaction,
};
use leyline_fs::gc::{GcOptions, collect_unreachable_chunks};
use rusqlite::{Connection, params};

/// Deterministic pseudo-random bytes (xorshift64), reproducible without an
/// RNG dependency — the same generator the sibling CDC tests use.
fn prng(seed: u64, n: usize) -> Vec<u8> {
    let mut s = seed | 1;
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 24) as u8
        })
        .collect()
}

fn blob_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    // The CANONICAL contract, not a hand-rolled copy.
    leyline_ts::schema::create_source_blobs_table(&conn).unwrap();
    conn
}

fn insert_blob(conn: &Connection, bytes: &[u8]) -> Hash {
    let hash = bytes.hash();
    conn.execute(
        "INSERT OR IGNORE INTO source_blobs (blob_hash, blob_bytes) VALUES (?1, ?2)",
        params![hash.as_bytes().as_slice(), bytes],
    )
    .unwrap();
    hash
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

fn blob_manifest_rows(conn: &Connection, blob_hash: Hash) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM blob_manifest WHERE blob_hash = ?1",
        params![blob_hash.as_bytes().as_slice()],
        |row| row.get(0),
    )
    .unwrap()
}

#[test]
fn sub_floor_blobs_are_skipped_not_manifested() {
    let conn = blob_db();
    // All strictly below the 8192-byte floor (the literal, not the const —
    // a test comparing against MIN_CHUNK moves its goalposts when the const
    // mutates).
    for (seed, len) in [(1, 0), (2, 100), (3, 4096), (4, 8191)] {
        insert_blob(&conn, &prng(seed, len));
    }

    let report = activate_chunked_source_blobs(&conn, ActivationOptions::default()).unwrap();

    assert_eq!(report.eligible_blobs, 0);
    assert_eq!(report.populated_blobs, 0);
    assert_eq!(report.already_fresh_blobs, 0);
    assert_eq!(report.skipped_sub_floor_blobs, 4);
    assert_eq!(report.processed_source_bytes, 0);
    assert_eq!(report.manifest_rows, 0);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM blob_manifest"), 0);
}

#[test]
fn activation_tiles_every_eligible_blob_and_reads_back_byte_identically() {
    let conn = blob_db();
    // 9 KiB (just above the floor), 96 KiB, and 170 KiB (crosses the
    // 128 KiB MAX_CHUNK ceiling, so it MUST span at least two chunks).
    let bodies: Vec<Vec<u8>> = vec![
        prng(11, 9 * 1024),
        prng(12, 96 * 1024),
        prng(13, 170 * 1024),
    ];
    let hashes: Vec<Hash> = bodies.iter().map(|b| insert_blob(&conn, b)).collect();

    let report = activate_chunked_source_blobs(&conn, ActivationOptions { batch_size: 1 }).unwrap();

    assert_eq!(report.eligible_blobs, 3);
    assert_eq!(report.populated_blobs, 3);
    assert_eq!(report.skipped_sub_floor_blobs, 0);
    assert_eq!(
        report.processed_source_bytes,
        bodies.iter().map(|b| b.len() as u64).sum::<u64>()
    );
    assert!(
        blob_manifest_rows(&conn, hashes[2]) >= 2,
        "a 170 KiB blob crosses the 128 KiB ceiling and must span >1 chunk"
    );

    for (body, hash) in bodies.iter().zip(&hashes) {
        let full = read_blob_range(&conn, *hash, 0..body.len() as u64).unwrap();
        assert_eq!(&full, body, "full-range read must be byte-identical");

        let (lo, hi) = (body.len() as u64 / 3, 2 * body.len() as u64 / 3);
        let interior = read_blob_range(&conn, *hash, lo..hi).unwrap();
        assert_eq!(
            interior,
            body[lo as usize..hi as usize],
            "interior range read must be byte-identical"
        );
    }
}

#[test]
fn range_reads_touch_only_overlapping_manifest_rows() {
    let conn = blob_db();
    let body = prng(21, 1024 * 1024);
    let hash = insert_blob(&conn, &body);
    activate_chunked_source_blobs(&conn, ActivationOptions::default()).unwrap();

    let total_rows = blob_manifest_rows(&conn, hash);
    assert!(total_rows > 4, "need a many-chunk blob, got {total_rows}");

    // Oracle: count overlaps over the manifest rows directly, then demand
    // the shipped predicate selects exactly that many.
    let spans: Vec<(i64, i64)> = conn
        .prepare("SELECT byte_offset, byte_len FROM blob_manifest WHERE blob_hash = ?1")
        .unwrap()
        .query_map(params![hash.as_bytes().as_slice()], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let mid = body.len() as u64 / 2;
    let (start, len) = (mid, 4096usize);
    let expected = spans
        .iter()
        .filter(|(offset, span)| {
            (*offset as u64) < start + len as u64 && (offset + span) as u64 > start
        })
        .count();

    let touched = blob_chunks_touched(&conn, hash, start, len).unwrap();
    assert_eq!(touched, expected, "selection must match the overlap oracle");
    assert!(
        touched < total_rows as usize,
        "a 4 KiB read of a 1 MiB blob must not touch every row"
    );

    let got = read_blob_range(&conn, hash, start..start + len as u64).unwrap();
    assert_eq!(got, body[start as usize..start as usize + len]);
}

/// An unrepresentable batch size is refused before any CDC table is created.
#[test]
fn activation_rejects_an_unrepresentable_batch_size_before_mutating() {
    let conn = blob_db();
    let error = activate_chunked_source_blobs(
        &conn,
        ActivationOptions {
            batch_size: usize::MAX,
        },
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("batch_size exceeds SQLite i64"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM sqlite_master
              WHERE type = 'table' AND (name LIKE 'content_%' OR name LIKE 'blob_%')"
        ),
        0,
        "invalid options must not mutate the database"
    );
}

#[test]
fn gc_keeps_chunks_referenced_only_by_blob_manifests() {
    let conn = blob_db();
    let body = prng(41, 96 * 1024);
    let hash = insert_blob(&conn, &body);
    activate_chunked_source_blobs(&conn, ActivationOptions::default()).unwrap();
    let before = count(&conn, "SELECT COUNT(*) FROM content_chunks");
    assert!(before > 0, "precondition: chunks stored");

    let report = collect_unreachable_chunks(&conn, GcOptions::default()).unwrap();

    // THE regression pin: with a single-table reachability predicate every
    // one of these chunks reads as unreachable and GC destroys live data.
    assert_eq!(report.unreachable_chunk_rows, 0);
    assert_eq!(report.deleted_chunk_rows, 0);
    assert_eq!(report.reaped_blob_manifest_rows, 0);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM content_chunks"), before);
    let full = read_blob_range(&conn, hash, 0..body.len() as u64).unwrap();
    assert_eq!(full, body, "the blob must still reconstruct after GC");
}

#[test]
fn deleting_a_source_blob_lets_gc_reclaim_manifest_and_unshared_chunks() {
    let conn = blob_db();
    // Two blobs sharing an 80 KiB tail: boundary stability gives them shared
    // chunks over the common content, plus unshared chunks in the prefixes.
    let common = prng(51, 80 * 1024);
    let mut kept = prng(52, 32 * 1024);
    kept.extend_from_slice(&common);
    let mut removed = prng(53, 48 * 1024);
    removed.extend_from_slice(&common);

    let kept_hash = insert_blob(&conn, &kept);
    let removed_hash = insert_blob(&conn, &removed);
    activate_chunked_source_blobs(&conn, ActivationOptions::default()).unwrap();

    let removed_manifest_rows = blob_manifest_rows(&conn, removed_hash);
    assert!(removed_manifest_rows > 0);
    let chunks_before = count(&conn, "SELECT COUNT(*) FROM content_chunks");
    // Exactly the chunks the surviving manifest references may remain.
    let kept_distinct_chunks = conn
        .query_row(
            "SELECT COUNT(DISTINCT chunk_hash) FROM blob_manifest WHERE blob_hash = ?1",
            params![kept_hash.as_bytes().as_slice()],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert!(
        kept_distinct_chunks < chunks_before,
        "the removed blob must contribute at least one unshared chunk"
    );

    conn.execute(
        "DELETE FROM source_blobs WHERE blob_hash = ?1",
        params![removed_hash.as_bytes().as_slice()],
    )
    .unwrap();
    let report = collect_unreachable_chunks(&conn, GcOptions::default()).unwrap();

    assert_eq!(
        report.reaped_blob_manifest_rows,
        removed_manifest_rows as u64
    );
    assert_eq!(report.reaped_blob_manifest_blobs, 1);
    assert_eq!(blob_manifest_rows(&conn, removed_hash), 0);
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM content_chunks"),
        kept_distinct_chunks,
        "unshared chunks reclaimed, shared chunks kept"
    );
    assert_eq!(
        report.deleted_chunk_rows,
        (chunks_before - kept_distinct_chunks) as u64
    );
    let survivor = read_blob_range(&conn, kept_hash, 0..kept.len() as u64).unwrap();
    assert_eq!(survivor, kept, "the surviving blob must still reconstruct");
}

#[test]
fn second_activation_is_all_already_fresh() {
    let conn = blob_db();
    for (seed, len) in [(61, 9 * 1024), (62, 96 * 1024)] {
        insert_blob(&conn, &prng(seed, len));
    }
    insert_blob(&conn, &prng(63, 100)); // sub-floor, skipped both times

    let first = activate_chunked_source_blobs(&conn, ActivationOptions::default()).unwrap();
    assert_eq!(first.populated_blobs, 2);
    assert_eq!(first.already_fresh_blobs, 0);

    let second = activate_chunked_source_blobs(&conn, ActivationOptions::default()).unwrap();
    assert_eq!(second.populated_blobs, 0);
    assert_eq!(second.already_fresh_blobs, 2);
    assert_eq!(second.skipped_sub_floor_blobs, 1);
    assert_eq!(second.processed_source_bytes, 0);
    assert_eq!(first.manifest_rows, second.manifest_rows);
    assert_eq!(first.unique_chunk_rows, second.unique_chunk_rows);
    assert_eq!(first.unique_chunk_bytes, second.unique_chunk_bytes);
}

#[test]
fn a_blob_hash_that_does_not_match_its_bytes_is_refused() {
    let conn = blob_db();
    create_blob_chunked_schema(&conn).unwrap();
    let bytes = prng(71, 16 * 1024);
    let wrong_hash = b"not these bytes".hash();

    let tx = conn.unchecked_transaction().unwrap();
    let error = store_blob_chunked_in_transaction(&tx, wrong_hash, &bytes).unwrap_err();
    assert!(
        format!("{error:#}").contains("content address"),
        "unexpected error: {error:#}"
    );
    drop(tx); // rolls back — but nothing may have been staged either

    assert_eq!(count(&conn, "SELECT COUNT(*) FROM blob_manifest"), 0);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM content_chunks"), 0);

    // The same refusal through activation: a source_blobs row whose bytes do
    // not hash to its key is a contract violation, and the walk must stop on
    // it rather than manifest it.
    conn.execute(
        "INSERT INTO source_blobs (blob_hash, blob_bytes) VALUES (?1, ?2)",
        params![wrong_hash.as_bytes().as_slice(), &bytes],
    )
    .unwrap();
    let error = activate_chunked_source_blobs(&conn, ActivationOptions::default()).unwrap_err();
    assert!(
        format!("{error:#}").contains("content address"),
        "unexpected error: {error:#}"
    );
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM blob_manifest"), 0);
}

#[test]
fn activation_requires_the_source_blobs_table_by_name() {
    let conn = Connection::open_in_memory().unwrap();
    let error = activate_chunked_source_blobs(&conn, ActivationOptions::default()).unwrap_err();
    assert!(
        format!("{error:#}").contains("missing required source_blobs table"),
        "unexpected error: {error:#}"
    );
    let cdc_tables: i64 = count(
        &conn,
        "SELECT COUNT(*) FROM sqlite_master
          WHERE type = 'table' AND (name LIKE 'content_%' OR name = 'blob_manifest')",
    );
    assert_eq!(cdc_tables, 0, "rejected databases must not be mutated");
}
