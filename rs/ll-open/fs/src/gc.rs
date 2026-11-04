//! Explicit, transactional reachability GC for chunk-backed content.

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, Transaction, TransactionBehavior};
use serde::Serialize;

/// Controls one explicit GC invocation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcOptions {
    /// Report unreachable storage without deleting it.
    pub dry_run: bool,
}

/// Deterministic storage accounting for one GC invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct GcReport {
    /// Physical chunk rows before collection.
    pub before_chunk_rows: u64,
    /// Deduplicated chunk payload bytes before collection.
    ///
    /// This is `SUM(length(chunk_bytes))`, not SQLite file size or bytes
    /// returned to the filesystem.
    pub before_chunk_bytes: u64,
    /// Rows not referenced by any committed manifest.
    pub unreachable_chunk_rows: u64,
    /// Deduplicated chunk payload bytes not referenced by any manifest.
    pub unreachable_chunk_bytes: u64,
    /// Rows deleted by this invocation (zero for dry-run).
    pub deleted_chunk_rows: u64,
    /// Deduplicated chunk payload bytes deleted (zero for dry-run).
    ///
    /// SQLite retains freed pages until a separate compaction operation.
    pub deleted_chunk_bytes: u64,
    /// Physical chunk rows after collection.
    pub remaining_chunk_rows: u64,
    /// Deduplicated chunk payload bytes after collection.
    pub remaining_chunk_bytes: u64,
    /// `blob_manifest` span rows removed because their manifest was dead.
    ///
    /// A blob manifest dies exactly one way: its `source_blobs` row is gone.
    /// Rows are immutable under their content address, so there is no
    /// stale-witness arm here (see `blob_chunked`); deletion of the
    /// authoritative row is the entire failure surface.
    pub reaped_blob_manifest_rows: u64,
    /// Distinct blobs whose manifests were removed.
    pub reaped_blob_manifest_blobs: u64,
    /// Whether this invocation was accounting-only.
    pub dry_run: bool,
}

/// Delete chunks unreachable from every committed content manifest.
///
/// Reachability accounting and deletion share one `IMMEDIATE` transaction, so
/// a concurrent manifest writer cannot make a chunk reachable between the
/// decision and the delete. The operation is explicit and off the write path.
pub fn collect_unreachable_chunks(conn: &Connection, options: GcOptions) -> Result<GcReport> {
    validate_gc_schema(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .context("begin CDC reachability GC transaction")?;
    // Idempotent inside this transaction: an arena with the chunk pool but no
    // blob manifest table gains one here, so the predicates below can
    // reference it. A dry run's rollback keeps it non-mutating.
    crate::blob_chunked::ensure_blob_manifest_infra(&tx)
        .context("ensure blob manifest schema before GC")?;
    let (before_chunk_rows, before_chunk_bytes) =
        chunk_totals(&tx, "", "count CDC chunks before GC")?;

    // Reap dead manifests FIRST, so the reachability pass below sees the
    // chunks they were holding. Doing this after would leave them referenced
    // for another whole cycle — and since every cycle can create new dead
    // manifests, "another cycle" is never.
    let (reaped_blob_manifest_rows, reaped_blob_manifest_blobs) = reap_dead_blob_manifests(&tx)?;

    // Reachable means referenced by a blob manifest. A `content_manifest`
    // table left by the removed `nodes` target (`ley-line-open-afa952`) is not
    // consulted: nothing reads it, so the chunks only it references are
    // garbage.
    let unreachable_predicate = "\
        WHERE NOT EXISTS (
            SELECT 1
              FROM blob_manifest AS blob_ref
             WHERE blob_ref.chunk_hash = content_chunks.chunk_hash
        )";
    let (unreachable_chunk_rows, unreachable_chunk_bytes) =
        chunk_totals(&tx, unreachable_predicate, "count unreachable CDC chunks")?;

    let (deleted_chunk_rows, deleted_chunk_bytes) = if options.dry_run {
        (0, 0)
    } else {
        let deleted = tx
            .execute(
                &format!("DELETE FROM content_chunks {unreachable_predicate}"),
                [],
            )
            .context("delete unreachable CDC chunks")?;
        let deleted = u64::try_from(deleted).context("deleted CDC chunk count exceeds u64")?;
        ensure!(
            deleted == unreachable_chunk_rows,
            "CDC GC deleted {deleted} rows after accounting {unreachable_chunk_rows} unreachable"
        );
        (deleted, unreachable_chunk_bytes)
    };

    let (remaining_chunk_rows, remaining_chunk_bytes) =
        chunk_totals(&tx, "", "count CDC chunks after GC")?;
    // The balance the report CLAIMS, enforced inside the transaction rather
    // than assumed by assignment (types-friend F10). The byte figure is the
    // one an operator acts on; rows were already ensured above, bytes were
    // not. Both identities hold on the dry-run path too (deleted = 0,
    // remaining = before), so no branch.
    ensure!(
        before_chunk_rows == deleted_chunk_rows + remaining_chunk_rows,
        "CDC GC row accounting does not balance: {before_chunk_rows} != \
         {deleted_chunk_rows} + {remaining_chunk_rows}"
    );
    ensure!(
        before_chunk_bytes == deleted_chunk_bytes + remaining_chunk_bytes,
        "CDC GC byte accounting does not balance: {before_chunk_bytes} != \
         {deleted_chunk_bytes} + {remaining_chunk_bytes}"
    );
    let report = GcReport {
        before_chunk_rows,
        before_chunk_bytes,
        unreachable_chunk_rows,
        unreachable_chunk_bytes,
        deleted_chunk_rows,
        deleted_chunk_bytes,
        remaining_chunk_rows,
        remaining_chunk_bytes,
        reaped_blob_manifest_rows,
        reaped_blob_manifest_blobs,
        dry_run: options.dry_run,
    };
    if options.dry_run {
        tx.rollback()
            .context("roll back CDC reachability dry-run")?;
    } else {
        tx.commit().context("commit CDC reachability GC")?;
    }
    Ok(report)
}

/// Delete blob manifests whose `source_blobs` row is gone. Returns
/// `(span_rows, distinct_blobs)`.
///
/// Deletion of the authoritative row is the ONLY way a blob manifest dies —
/// rows are immutable under their content address, so there is no
/// stale-witness arm (see `blob_chunked`). When the `source_blobs` table
/// itself is absent, EVERY blob manifest is dead by the same reasoning: the
/// manifest is a derived index over `source_blobs` rows, and a database
/// without the table has no authoritative row for any manifest to describe,
/// so the absence is itself the proof.
///
/// Runs unconditionally inside the caller's transaction, including on a dry
/// run: the reachability pass that follows must see post-reap state or the
/// dry-run estimate understates what is actually reclaimable, which is the
/// number the operator is asking for. The caller's rollback is what makes a
/// dry run non-mutating.
fn reap_dead_blob_manifests(tx: &Transaction<'_>) -> Result<(u64, u64)> {
    let source_blobs_present: bool = tx
        .query_row(
            "SELECT COUNT(*) > 0
               FROM sqlite_master
              WHERE type = 'table' AND name = 'source_blobs'",
            [],
            |row| row.get(0),
        )
        .context("probe for source_blobs table")?;
    let dead_predicate = if source_blobs_present {
        "NOT EXISTS (
             SELECT 1
               FROM source_blobs
              WHERE source_blobs.blob_hash = blob_manifest.blob_hash
         )"
    } else {
        "1"
    };

    // Count distinct blobs before the delete removes the evidence; the row
    // count comes from the DELETE itself.
    let blobs: i64 = tx
        .query_row(
            &format!("SELECT COUNT(DISTINCT blob_hash) FROM blob_manifest WHERE {dead_predicate}"),
            [],
            |row| row.get(0),
        )
        .context("count dead blob manifests")?;
    let spans = tx
        .execute(
            &format!("DELETE FROM blob_manifest WHERE {dead_predicate}"),
            [],
        )
        .context("reap dead blob manifests")?;

    Ok((
        u64::try_from(spans).context("reaped blob manifest span count exceeds u64")?,
        u64::try_from(blobs).context("reaped blob manifest blob count exceeds u64")?,
    ))
}

fn validate_gc_schema(conn: &Connection) -> Result<()> {
    let table_present = |name: &str| -> Result<bool> {
        conn.query_row(
            "SELECT COUNT(*) > 0
               FROM sqlite_master
              WHERE type = 'table' AND name = ?1",
            [name],
            |row| row.get(0),
        )
        .with_context(|| format!("inspect CDC table {name} for reachability GC"))
    };
    // The pool is the collection target and must already exist; a database
    // without it has no CDC schema at all and is refused rather than mutated.
    // The blob manifest table is created idempotently inside the GC
    // transaction when absent.
    let pool = table_present("content_chunks")?;
    let blob_manifest = table_present("blob_manifest")?;
    ensure!(pool, "missing required CDC tables for reachability GC");
    validate_table_columns(
        conn,
        "content_chunks",
        &["chunk_hash", "chunk_bytes", "chunk_len"],
    )?;
    if blob_manifest {
        validate_table_columns(
            conn,
            "blob_manifest",
            &["blob_hash", "seq", "chunk_hash", "byte_offset", "byte_len"],
        )?;
    }
    Ok(())
}

fn validate_table_columns(conn: &Connection, table: &str, required: &[&str]) -> Result<()> {
    // `table_xinfo` includes generated columns such as content_chunks.chunk_len.
    let sql = format!("PRAGMA table_xinfo({table})");
    let mut statement = conn
        .prepare(&sql)
        .with_context(|| format!("inspect CDC table {table}"))?;
    let columns: std::collections::HashSet<String> = statement
        .query_map([], |row| row.get(1))
        .with_context(|| format!("read CDC table {table} columns"))?
        .collect::<rusqlite::Result<_>>()
        .with_context(|| format!("collect CDC table {table} columns"))?;
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|column| !columns.contains(*column))
        .collect();
    ensure!(
        missing.is_empty(),
        "incompatible CDC table {table}: missing required columns {}",
        missing.join(", ")
    );
    Ok(())
}

fn chunk_totals(conn: &Connection, predicate: &str, context: &'static str) -> Result<(u64, u64)> {
    let sql = format!(
        "SELECT COUNT(*), COALESCE(SUM(length(chunk_bytes)), 0)
           FROM content_chunks
           {predicate}"
    );
    let (rows, bytes): (i64, i64) = conn
        .query_row(&sql, [], |row| Ok((row.get(0)?, row.get(1)?)))
        .context(context)?;
    ensure!(rows >= 0, "{context} returned negative row count {rows}");
    ensure!(bytes >= 0, "{context} returned negative byte count {bytes}");
    Ok((
        u64::try_from(rows).context("CDC chunk row count exceeds u64")?,
        u64::try_from(bytes).context("CDC chunk byte count exceeds u64")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::{GcOptions, collect_unreachable_chunks};
    use crate::blob_chunked::{
        create_blob_chunked_schema, read_blob_range, store_blob_chunked_in_transaction,
    };
    use leyline_core::{ContentAddressed, Hash};
    use rusqlite::Connection;

    /// A database carrying `source_blobs` and the blob CDC schema.
    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        leyline_ts::schema::create_source_blobs_table(&conn).unwrap();
        create_blob_chunked_schema(&conn).unwrap();
        conn
    }

    fn count_chunks(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM content_chunks", [], |row| row.get(0))
            .unwrap()
    }

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

    /// Insert `bytes` as a `source_blobs` row and store its chunk manifest.
    fn store(conn: &Connection, bytes: &[u8]) -> Hash {
        let hash = bytes.hash();
        conn.execute(
            "INSERT OR IGNORE INTO source_blobs (blob_hash, blob_bytes) VALUES (?1, ?2)",
            rusqlite::params![hash.as_bytes().as_slice(), bytes],
        )
        .unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        store_blob_chunked_in_transaction(&tx, hash, bytes).unwrap();
        tx.commit().unwrap();
        hash
    }

    /// Delete the authoritative row — the one way a blob manifest dies.
    fn remove(conn: &Connection, hash: Hash) {
        conn.execute(
            "DELETE FROM source_blobs WHERE blob_hash = ?1",
            rusqlite::params![hash.as_bytes().as_slice()],
        )
        .unwrap();
    }

    fn count_manifest_rows(conn: &Connection, hash: Hash) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM blob_manifest WHERE blob_hash = ?1",
            rusqlite::params![hash.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// A `content_manifest` table left by the removed `nodes` target
    /// (`ley-line-open-afa952`) does not pin chunks: nothing reads it, so the
    /// chunks only it references are collected.
    #[test]
    fn a_legacy_node_manifest_does_not_pin_chunks() {
        let conn = db();
        let kept = store(&conn, &vec![b'k'; 40_000]);
        let tx = conn.unchecked_transaction().unwrap();
        let mut pool = crate::chunk_pool::SqliteBlobStore::new(&tx);
        let orphan_hash =
            leyline_core::BlobStore::put(&mut pool, b"only a legacy manifest names me").unwrap();
        tx.commit().unwrap();
        conn.execute_batch(
            "CREATE TABLE content_manifest (
                nid INTEGER NOT NULL, seq INTEGER NOT NULL, chunk_hash BLOB NOT NULL,
                byte_offset INTEGER NOT NULL, byte_len INTEGER NOT NULL,
                PRIMARY KEY (nid, seq)
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO content_manifest VALUES (1, 0, ?1, 0, 31)",
            rusqlite::params![orphan_hash.as_bytes().as_slice()],
        )
        .unwrap();
        let before = count_chunks(&conn);

        let report = collect_unreachable_chunks(&conn, GcOptions::default()).unwrap();

        assert_eq!(report.deleted_chunk_rows, 1, "the legacy-only chunk goes");
        assert_eq!(count_chunks(&conn), before - 1);
        assert!(count_manifest_rows(&conn, kept) > 0, "live manifest kept");
    }

    /// A manifest whose `source_blobs` row is gone still references its
    /// chunks, so without the reap reachability GC would see them as live and
    /// reclaim nothing, forever (bead `ley-line-open-b5e56f`).
    #[test]
    fn gc_reaps_manifests_whose_blob_is_gone() {
        let conn = db();
        let hash = store(&conn, &vec![b'x'; 40_000]);
        assert!(count_chunks(&conn) > 0, "precondition: chunks stored");

        remove(&conn, hash);
        collect_unreachable_chunks(&conn, GcOptions::default()).unwrap();

        assert_eq!(
            count_manifest_rows(&conn, hash),
            0,
            "a manifest whose blob is gone must be reaped"
        );
        assert_eq!(
            count_chunks(&conn),
            0,
            "its chunks must then be unreachable and collected"
        );
    }

    /// The blob reaper's COUNTS are the contract, not just its side effect:
    /// mutation testing replaced `reap_dead_blob_manifests` with every
    /// constant tuple and the integration suite couldn't object — the diff
    /// gate runs lib tests only, so the killer has to live here. Exact
    /// equality against independently observed row/blob counts refutes all
    /// four constants at once; the quiet phase pins the no-dead case.
    #[test]
    fn gc_reports_exact_blob_reap_counts() {
        let conn = db();
        // One blob past MAX_CHUNK (>= 2 manifest rows) plus one small
        // eligible blob: total rows >= 3 and distinct blobs == 2, so no
        // 0/1-valued tuple can coincide with the real counts.
        let big = vec![b'a'; 200_000];
        let small = vec![b'b'; 9_000];
        for bytes in [&big, &small] {
            let hash = leyline_core::ContentAddressed::hash(&bytes[..]);
            conn.execute(
                "INSERT OR IGNORE INTO source_blobs (blob_hash, blob_bytes) VALUES (?1, ?2)",
                rusqlite::params![hash.as_bytes().as_slice(), bytes],
            )
            .unwrap();
        }
        crate::activation::activate_chunked_source_blobs(
            &conn,
            crate::activation::ActivationOptions::default(),
        )
        .unwrap();
        let manifest_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM blob_manifest", [], |row| row.get(0))
            .unwrap();
        assert!(manifest_rows >= 3, "precondition: {manifest_rows} rows");

        // Nothing is dead yet: the reap must report exactly zero work.
        let quiet = collect_unreachable_chunks(&conn, GcOptions::default()).unwrap();
        assert_eq!(quiet.reaped_blob_manifest_rows, 0);
        assert_eq!(quiet.reaped_blob_manifest_blobs, 0);

        conn.execute("DELETE FROM source_blobs", []).unwrap();
        let report = collect_unreachable_chunks(&conn, GcOptions::default()).unwrap();
        assert_eq!(
            report.reaped_blob_manifest_rows,
            u64::try_from(manifest_rows).unwrap(),
            "reaped span rows must equal the rows that existed"
        );
        assert_eq!(
            report.reaped_blob_manifest_blobs, 2,
            "reaped blobs must equal the distinct dead blobs"
        );
        assert_eq!(count_chunks(&conn), 0, "their chunks must then collect");
    }

    /// The safety direction: a live manifest must survive GC untouched, or
    /// the collector becomes a cache-destroyer.
    #[test]
    fn gc_preserves_live_manifests_and_their_chunks() {
        let conn = db();
        let data = vec![b'z'; 40_000];
        let hash = store(&conn, &data);
        let before = count_chunks(&conn);

        collect_unreachable_chunks(&conn, GcOptions::default()).unwrap();

        assert!(count_manifest_rows(&conn, hash) > 0, "live manifest kept");
        assert_eq!(count_chunks(&conn), before, "live chunks kept");
        let len = data.len() as u64;
        assert_eq!(
            read_blob_range(&conn, hash, 0..len).unwrap(),
            data,
            "and the content still reads back"
        );
    }

    #[test]
    fn dry_run_accounts_for_unreachable_chunks_without_mutating() {
        let conn = db();
        let data = vec![0x5a; 128 * 1024];
        let hash = store(&conn, &data);
        remove(&conn, hash);
        let before = count_chunks(&conn);
        assert!(before > 0);

        let report = collect_unreachable_chunks(&conn, GcOptions { dry_run: true }).unwrap();

        assert_eq!(report.before_chunk_rows, before as u64);
        assert_eq!(report.unreachable_chunk_rows, before as u64);
        assert_eq!(report.deleted_chunk_rows, 0);
        assert_eq!(report.remaining_chunk_rows, before as u64);
        assert_eq!(count_chunks(&conn), before);
        assert!(report.unreachable_chunk_bytes > 0);
        assert_eq!(report.deleted_chunk_bytes, 0);
        assert_eq!(report.remaining_chunk_bytes, report.before_chunk_bytes);
        assert!(
            count_manifest_rows(&conn, hash) > 0,
            "the dry run's reap rolled back"
        );
    }

    /// Two blobs sharing chunks: removing one keeps the shared chunks until
    /// the last manifest naming them is gone.
    #[test]
    fn collection_preserves_shared_chunks_until_the_final_manifest_is_gone() {
        let conn = db();
        // Same pseudo-random 600 KB prefix, different tails: CDC boundaries
        // are content-defined, so the prefix chunks are stored once and named
        // by both manifests.
        let shared = prng(0x5ead, 600_000);
        let mut first_bytes = shared.clone();
        first_bytes.extend_from_slice(&prng(1, 200_000));
        let mut second_bytes = shared.clone();
        second_bytes.extend_from_slice(&prng(2, 200_000));
        let first_hash = store(&conn, &first_bytes);
        let second_hash = store(&conn, &second_bytes);
        let all_rows = count_chunks(&conn);
        remove(&conn, second_hash);

        let first = collect_unreachable_chunks(&conn, GcOptions::default()).unwrap();

        let remaining_after_first = count_chunks(&conn);
        assert!(
            remaining_after_first < all_rows,
            "the removed blob's own chunks go"
        );
        assert_eq!(first.remaining_chunk_rows as i64, remaining_after_first);
        let len = first_bytes.len() as u64;
        assert_eq!(
            read_blob_range(&conn, first_hash, 0..len).unwrap(),
            first_bytes,
            "the surviving blob still reads back through the shared chunks"
        );

        remove(&conn, first_hash);
        let second = collect_unreachable_chunks(&conn, GcOptions::default()).unwrap();
        let shared_rows = remaining_after_first;
        assert_eq!(second.deleted_chunk_rows, shared_rows as u64);
        assert_eq!(second.deleted_chunk_bytes, second.before_chunk_bytes);
        assert_eq!(second.remaining_chunk_rows, 0);
        assert_eq!(second.remaining_chunk_bytes, 0);
    }

    #[test]
    fn collection_deletes_only_orphans_and_live_content_still_reconstructs() {
        let conn = db();
        let live = vec![0x11; 96 * 1024];
        let removed = vec![0x77; 96 * 1024];
        let live_hash = store(&conn, &live);
        let removed_hash = store(&conn, &removed);
        remove(&conn, removed_hash);

        let report = collect_unreachable_chunks(&conn, GcOptions::default()).unwrap();

        assert!(report.deleted_chunk_rows > 0);
        assert!(report.remaining_chunk_rows > 0);
        assert_eq!(
            report.before_chunk_rows,
            report.deleted_chunk_rows + report.remaining_chunk_rows
        );
        assert_eq!(
            report.before_chunk_bytes,
            report.deleted_chunk_bytes + report.remaining_chunk_bytes
        );
        let len = live.len() as u64;
        assert_eq!(read_blob_range(&conn, live_hash, 0..len).unwrap(), live);
    }

    #[test]
    fn collection_is_idempotent_and_reports_deterministic_zeroes() {
        let conn = db();
        let hash = store(&conn, b"historical");
        remove(&conn, hash);

        let first = collect_unreachable_chunks(&conn, GcOptions::default()).unwrap();
        let second = collect_unreachable_chunks(&conn, GcOptions::default()).unwrap();

        assert!(first.deleted_chunk_rows > 0);
        assert_eq!(second.before_chunk_rows, 0);
        assert_eq!(second.before_chunk_bytes, 0);
        assert_eq!(second.unreachable_chunk_rows, 0);
        assert_eq!(second.unreachable_chunk_bytes, 0);
        assert_eq!(second.deleted_chunk_rows, 0);
        assert_eq!(second.deleted_chunk_bytes, 0);
        assert_eq!(second.remaining_chunk_rows, 0);
        assert_eq!(second.remaining_chunk_bytes, 0);
    }

    #[test]
    fn failed_collection_rolls_back_every_chunk() {
        let conn = db();
        let hash = store(&conn, b"historical");
        remove(&conn, hash);
        let before = count_chunks(&conn);
        conn.execute_batch(
            "CREATE TRIGGER fail_gc BEFORE DELETE ON content_chunks
             BEGIN SELECT RAISE(ABORT, 'injected GC failure'); END;",
        )
        .unwrap();

        let error = collect_unreachable_chunks(&conn, GcOptions::default()).unwrap_err();

        assert!(
            format!("{error:#}").contains("delete unreachable CDC chunks"),
            "unexpected error: {error:#}"
        );
        assert_eq!(count_chunks(&conn), before);
        assert!(
            count_manifest_rows(&conn, hash) > 0,
            "the failed collection's reap rolled back too"
        );
    }

    #[test]
    fn collection_refuses_a_database_without_the_cdc_schema() {
        let conn = Connection::open_in_memory().unwrap();

        let error = collect_unreachable_chunks(&conn, GcOptions::default()).unwrap_err();

        assert!(
            format!("{error:#}").contains("missing required CDC tables"),
            "unexpected error: {error:#}"
        );
        let created: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                  WHERE name LIKE 'content_%' OR name LIKE 'blob_%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(created, 0, "rejected databases must not be mutated");
    }

    #[test]
    fn collection_installs_and_uses_the_manifest_hash_index() {
        let conn = db();
        store(&conn, b"still reachable");
        conn.execute("DROP INDEX blob_manifest_chunk_hash", [])
            .unwrap();

        collect_unreachable_chunks(&conn, GcOptions::default()).unwrap();

        let details: Vec<String> = conn
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT COUNT(*)
                   FROM content_chunks
                  WHERE NOT EXISTS (
                    SELECT 1
                      FROM blob_manifest AS blob_ref
                     WHERE blob_ref.chunk_hash = content_chunks.chunk_hash
                  )",
            )
            .unwrap()
            .query_map([], |row| row.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            details.iter().any(|detail| {
                detail.contains("SEARCH blob_ref USING COVERING INDEX blob_manifest_chunk_hash")
            }),
            "reachability must use the manifest hash index, plan: {details:?}"
        );
    }

    #[test]
    fn dry_run_rolls_back_the_legacy_projection_index_migration() {
        let conn = db();
        store(&conn, b"still reachable");
        conn.execute("DROP INDEX blob_manifest_chunk_hash", [])
            .unwrap();

        collect_unreachable_chunks(&conn, GcOptions { dry_run: true }).unwrap();

        let index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                  WHERE type = 'index' AND name = 'blob_manifest_chunk_hash'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            index_count, 0,
            "dry-run must not persist the compatibility index"
        );
    }

    #[test]
    fn collection_rejects_lookalike_tables_without_mutation() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE content_chunks (
                chunk_hash BLOB PRIMARY KEY,
                payload BLOB NOT NULL
             );
             CREATE TABLE blob_manifest (
                node_id TEXT NOT NULL,
                chunk_hash BLOB NOT NULL
             );
             INSERT INTO content_chunks VALUES (x'01', x'aa');",
        )
        .unwrap();

        let error = collect_unreachable_chunks(&conn, GcOptions::default()).unwrap_err();

        assert!(
            format!("{error:#}").contains("incompatible CDC table"),
            "unexpected error: {error:#}"
        );
        assert_eq!(count_chunks(&conn), 1);
    }
}
