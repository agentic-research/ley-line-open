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
    /// `content_manifest` span rows removed because their manifest was dead.
    ///
    /// Dead means the freshness witness cannot be satisfied: the node is gone,
    /// its `(size, mtime)` moved on, or the witness row is missing entirely.
    /// Such a manifest is already refused by every read — this reclaims the
    /// storage it was pinning.
    pub reaped_manifest_rows: u64,
    /// `content_manifest_meta` witness rows removed (one per dead node).
    pub reaped_manifest_nodes: u64,
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
    // The freshness predicate references content_generation; a pre-b82f56
    // arena being GC'd for the first time post-upgrade migrates here.
    // Idempotent DDL inside this transaction — a dry run's rollback keeps
    // it non-mutating, same as the index above.
    crate::chunked::ensure_generation_infra(&tx).context("ensure generation infra before GC")?;
    let (before_chunk_rows, before_chunk_bytes) =
        chunk_totals(&tx, "", "count CDC chunks before GC")?;

    // Reap dead manifests FIRST, so the reachability pass below sees the
    // chunks they were holding. Doing this after would leave them referenced
    // for another whole cycle — and since every cycle can create new dead
    // manifests, "another cycle" is never.
    let (reaped_manifest_rows, reaped_manifest_nodes) = reap_dead_manifests(&tx)?;

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
        reaped_manifest_rows,
        reaped_manifest_nodes,
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

/// A manifest is dead when its freshness witness cannot be satisfied.
///
/// One predicate covers all three ways that happens, which is why it is
/// written once and applied to both tables:
///
/// * the node is gone — the join finds nothing (path reuse after
///   `remove_node`/`rename_node`, the cross-generation leak from `0330c7`);
/// * the witness disagrees — `(size, mtime)` moved on behind this crate;
/// * the witness row is missing entirely — spans with no meta row.
///
/// In every case `has_chunked_content_in_transaction` already returns false,
/// so reads are unaffected: this reclaims storage that was pinned by a
/// manifest nothing could ever use. A dead manifest is also useless for
/// incremental rechunking, which only accepts a *fresh* previous snapshot,
/// so nothing downstream loses an optimization either.
fn dead_manifest_predicate(table: &str) -> String {
    // Freshness comes from the ONE definition the read gate uses
    // (WITNESS_FRESH_PREDICATE) — a hand-written second copy is how the
    // reaper and the gate drifted apart the first time (types-friend F3:
    // this site was missing the source_len guard the tested Rust arm had,
    // and predates the generation witness entirely).
    format!(
        "NOT EXISTS (
             SELECT 1
               FROM content_manifest_meta AS m
               JOIN nodes AS n ON n.id = m.node_id
              WHERE m.node_id = {table}.node_id
                AND {}
         )",
        crate::chunked::WITNESS_FRESH_PREDICATE
    )
}

/// Delete manifests whose freshness witness is dead. Returns
/// `(span_rows, witness_rows)`.
///
/// Runs unconditionally inside the caller's transaction, including on a dry
/// run: the reachability pass that follows must see post-reap state or the
/// dry-run estimate understates what is actually reclaimable, which is the
/// number the operator is asking for. The caller's rollback is what makes a
/// dry run non-mutating.
fn reap_dead_manifests(tx: &Transaction<'_>) -> Result<(u64, u64)> {
    // Freshness is only evaluable against the live rows. A standalone chunk
    // store has no `nodes` table and no witnesses; there is nothing to prove
    // dead, so reap nothing rather than guessing.
    let evaluable: i64 = tx
        .query_row(
            "SELECT COUNT(*)
               FROM sqlite_master
              WHERE type = 'table'
                AND name IN ('nodes', 'content_manifest_meta')",
            [],
            |row| row.get(0),
        )
        .context("probe for manifest freshness inputs")?;
    if evaluable < 2 {
        return Ok((0, 0));
    }

    // Spans first: the witness rows are the predicate's own input, so
    // deleting them first would make every remaining manifest look dead.
    let spans = tx
        .execute(
            &format!(
                "DELETE FROM content_manifest WHERE {}",
                dead_manifest_predicate("content_manifest")
            ),
            [],
        )
        .context("reap dead manifest spans")?;
    let witnesses = tx
        .execute(
            &format!(
                "DELETE FROM content_manifest_meta WHERE {}",
                dead_manifest_predicate("content_manifest_meta")
            ),
            [],
        )
        .context("reap dead manifest witnesses")?;

    Ok((
        u64::try_from(spans).context("reaped manifest span count exceeds u64")?,
        u64::try_from(witnesses).context("reaped manifest witness count exceeds u64")?,
    ))
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

    /// A `nodes` table plus one row, so manifest freshness is evaluable.
    fn insert_node(conn: &Connection, id: &str, content: &str, mtime: i64) {
        // The CANONICAL contract, not a hand-rolled copy. A fixture that
        // declares its own `nodes` DDL drifts from what producers ship
        // against — which is how `record JSON`'s NUMERIC affinity went
        // unnoticed (bead `ley-line-open-f7966d`).
        leyline_schema::create_nodes_table(conn).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO nodes (id, parent_id, name, kind, size, mtime, record) \
             VALUES (?1, '', ?1, 0, ?2, ?3, ?4)",
            rusqlite::params![id, content.len() as i64, mtime, content],
        )
        .unwrap();
    }

    fn count_manifest_rows(conn: &Connection, node_id: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM content_manifest WHERE node_id = ?1",
            rusqlite::params![node_id],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// Node ids are PATHS and paths get reused. A manifest whose node is gone
    /// is refused by every read (the freshness witness has nothing to join
    /// against) yet still references its chunks — so reachability GC sees them
    /// as live and reclaims nothing, forever.
    ///
    /// Bead `ley-line-open-b5e56f`. This is the hygiene half of `0330c7`:
    /// correctness degrades safely, storage does not.
    #[test]
    fn gc_reaps_manifests_whose_node_is_gone() {
        let conn = db();
        let data = vec![b'x'; 40_000];
        insert_node(&conn, "n", std::str::from_utf8(&data).unwrap(), 1);
        store_content_chunked(&conn, "n", &data).unwrap();
        assert!(count_chunks(&conn) > 0, "precondition: chunks stored");

        // Removed out of band — exactly the path-reuse orphan from 0330c7.
        conn.execute("DELETE FROM nodes WHERE id = 'n'", [])
            .unwrap();

        collect_unreachable_chunks(&conn, GcOptions::default()).unwrap();

        assert_eq!(
            count_manifest_rows(&conn, "n"),
            0,
            "a manifest whose node is gone must be reaped"
        );
        assert_eq!(
            count_chunks(&conn),
            0,
            "its chunks must then be unreachable and collected"
        );
    }

    /// A manifest whose witness disagrees with the live row is refused by
    /// reads and cannot serve incremental rechunking either, so it is dead
    /// weight holding chunks alive.
    #[test]
    fn gc_reaps_manifests_with_a_stale_witness() {
        let conn = db();
        let data = vec![b'y'; 40_000];
        insert_node(&conn, "n", std::str::from_utf8(&data).unwrap(), 1);
        store_content_chunked(&conn, "n", &data).unwrap();

        // Record moved on behind this crate's back: size and mtime both shift.
        conn.execute(
            "UPDATE nodes SET record = 'zzz', size = 3, mtime = 999 WHERE id = 'n'",
            [],
        )
        .unwrap();

        collect_unreachable_chunks(&conn, GcOptions::default()).unwrap();

        assert_eq!(
            count_manifest_rows(&conn, "n"),
            0,
            "a stale manifest must be reaped"
        );
        assert_eq!(count_chunks(&conn), 0, "and its chunks collected");
    }

    /// The safety direction: a manifest that IS fresh must survive GC
    /// untouched, or the collector becomes a cache-destroyer.
    #[test]
    fn gc_preserves_fresh_manifests_and_their_chunks() {
        let conn = db();
        let data = vec![b'z'; 40_000];
        insert_node(&conn, "n", std::str::from_utf8(&data).unwrap(), 1);
        store_content_chunked(&conn, "n", &data).unwrap();
        let before = count_chunks(&conn);

        collect_unreachable_chunks(&conn, GcOptions::default()).unwrap();

        assert!(count_manifest_rows(&conn, "n") > 0, "fresh manifest kept");
        assert_eq!(count_chunks(&conn), before, "fresh chunks kept");
        let mut buf = vec![0u8; data.len()];
        let n = read_content_chunked(&conn, "n", &mut buf, 0).unwrap();
        assert_eq!(n, data.len());
        assert_eq!(buf, data, "and the content still reads back");
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
