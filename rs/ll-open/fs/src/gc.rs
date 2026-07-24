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
    tx.execute(
        "CREATE INDEX IF NOT EXISTS content_manifest_chunk_hash
             ON content_manifest(chunk_hash)",
        [],
    )
    .context("ensure CDC manifest reachability index")?;
    let (before_chunk_rows, before_chunk_bytes) =
        chunk_totals(&tx, "", "count CDC chunks before GC")?;
    let unreachable_predicate = "\
        WHERE NOT EXISTS (
            SELECT 1
              FROM content_manifest AS manifest
             WHERE manifest.chunk_hash = content_chunks.chunk_hash
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
    let report = GcReport {
        before_chunk_rows,
        before_chunk_bytes,
        unreachable_chunk_rows,
        unreachable_chunk_bytes,
        deleted_chunk_rows,
        deleted_chunk_bytes,
        remaining_chunk_rows,
        remaining_chunk_bytes,
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

fn validate_gc_schema(conn: &Connection) -> Result<()> {
    let present: i64 = conn
        .query_row(
            "SELECT COUNT(*)
               FROM sqlite_master
              WHERE type = 'table'
                AND name IN ('content_chunks', 'content_manifest')",
            [],
            |row| row.get(0),
        )
        .context("inspect CDC tables for reachability GC")?;
    ensure!(
        present == 2,
        "missing required CDC tables for reachability GC"
    );
    validate_table_columns(
        conn,
        "content_chunks",
        &["chunk_hash", "chunk_bytes", "chunk_len"],
    )?;
    validate_table_columns(
        conn,
        "content_manifest",
        &["node_id", "seq", "chunk_hash", "byte_offset", "byte_len"],
    )?;
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
    use crate::chunked::{
        create_chunked_content_schema, invalidate_chunked_content, read_content_chunked,
        store_content_chunked,
    };
    use rusqlite::Connection;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        create_chunked_content_schema(&conn).unwrap();
        conn
    }

    fn count_chunks(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM content_chunks", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn dry_run_accounts_for_unreachable_chunks_without_mutating() {
        let conn = db();
        let data = vec![0x5a; 128 * 1024];
        store_content_chunked(&conn, "removed", &data).unwrap();
        invalidate_chunked_content(&conn, "removed").unwrap();
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
    }

    #[test]
    fn collection_preserves_shared_chunks_until_the_final_manifest_is_gone() {
        let conn = db();
        let data = vec![0x33; 96 * 1024];
        store_content_chunked(&conn, "live", &data).unwrap();
        store_content_chunked(&conn, "removed", &data).unwrap();
        let shared_rows = count_chunks(&conn);
        invalidate_chunked_content(&conn, "removed").unwrap();

        let first = collect_unreachable_chunks(&conn, GcOptions::default()).unwrap();

        assert_eq!(first.unreachable_chunk_rows, 0);
        assert_eq!(first.deleted_chunk_rows, 0);
        assert_eq!(count_chunks(&conn), shared_rows);
        let mut round_trip = vec![0_u8; data.len()];
        assert_eq!(
            read_content_chunked(&conn, "live", &mut round_trip, 0).unwrap(),
            data.len()
        );
        assert_eq!(round_trip, data);

        invalidate_chunked_content(&conn, "live").unwrap();
        let second = collect_unreachable_chunks(&conn, GcOptions::default()).unwrap();
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
        store_content_chunked(&conn, "live", &live).unwrap();
        store_content_chunked(&conn, "removed", &removed).unwrap();
        invalidate_chunked_content(&conn, "removed").unwrap();

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
        let mut round_trip = vec![0_u8; live.len()];
        assert_eq!(
            read_content_chunked(&conn, "live", &mut round_trip, 0).unwrap(),
            live.len()
        );
        assert_eq!(round_trip, live);
    }

    #[test]
    fn collection_is_idempotent_and_reports_deterministic_zeroes() {
        let conn = db();
        store_content_chunked(&conn, "removed", b"historical").unwrap();
        invalidate_chunked_content(&conn, "removed").unwrap();

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
        store_content_chunked(&conn, "removed", b"historical").unwrap();
        invalidate_chunked_content(&conn, "removed").unwrap();
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
                "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'content_%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(created, 0, "rejected databases must not be mutated");
    }

    #[test]
    fn collection_installs_and_uses_the_manifest_hash_index() {
        let conn = db();
        store_content_chunked(&conn, "live", b"still reachable").unwrap();
        conn.execute("DROP INDEX content_manifest_chunk_hash", [])
            .unwrap();

        collect_unreachable_chunks(&conn, GcOptions::default()).unwrap();

        let details: Vec<String> = conn
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT COUNT(*)
                   FROM content_chunks
                  WHERE NOT EXISTS (
                    SELECT 1
                      FROM content_manifest AS manifest
                     WHERE manifest.chunk_hash = content_chunks.chunk_hash
                  )",
            )
            .unwrap()
            .query_map([], |row| row.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            details.iter().any(|detail| {
                detail.contains("SEARCH manifest USING COVERING INDEX content_manifest_chunk_hash")
            }),
            "reachability must use the manifest hash index, plan: {details:?}"
        );
    }

    #[test]
    fn dry_run_rolls_back_the_legacy_projection_index_migration() {
        let conn = db();
        store_content_chunked(&conn, "live", b"still reachable").unwrap();
        conn.execute("DROP INDEX content_manifest_chunk_hash", [])
            .unwrap();

        collect_unreachable_chunks(&conn, GcOptions { dry_run: true }).unwrap();

        let index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                  WHERE type = 'index' AND name = 'content_manifest_chunk_hash'",
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
             CREATE TABLE content_manifest (
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
