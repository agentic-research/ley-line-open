//! Production activation for chunk-backed content storage.
//!
//! Activation is explicit: opening a writable graph does not create or
//! populate the derived CDC tables. This module supplies one idempotent entry
//! point per target — the construct-granular `nodes` walk and the whole-file
//! `source_blobs` walk (see [`crate::blob_chunked`] for why the second target
//! exists and why its manifests carry no freshness witness) — for library,
//! CLI, and daemon consumers.

use anyhow::{Context, Result, ensure};
use leyline_core::Hash;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use std::collections::BTreeSet;

use crate::blob_chunked::{
    create_blob_chunked_schema, has_blob_chunked, store_blob_chunked_in_transaction,
};
use crate::chunked::{
    create_chunked_content_schema, has_chunked_content_in_transaction,
    store_content_chunked_in_transaction,
};

/// Controls bounded activation work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivationOptions {
    /// Number of authoritative rows loaded into memory per query page.
    pub batch_size: usize,
}

impl Default for ActivationOptions {
    fn default() -> Self {
        Self { batch_size: 256 }
    }
}

/// Deterministic summary of one activation invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ActivationReport {
    /// Total eligible readable leaf nodes in committed final state.
    pub eligible_nodes: u64,
    /// Nodes populated or rebuilt by this invocation.
    pub populated_nodes: u64,
    /// Nodes whose committed manifest was already fresh.
    pub already_fresh_nodes: u64,
    /// Authoritative bytes processed by this invocation.
    pub processed_source_bytes: u64,
    /// Total manifest span rows in committed final state.
    pub manifest_rows: u64,
    /// Total unique content-addressed chunk rows in committed final state.
    pub unique_chunk_rows: u64,
    /// Total bytes stored across unique chunk rows in committed final state.
    pub unique_chunk_bytes: u64,
    /// Rows in `blob_manifest` at commit — the `source_blobs` target's index,
    /// present only if that target was activated on this database at some
    /// point. Nonzero here means this `nodes` activation left it behind:
    /// ordinary GC cannot reclaim it, because those rows are still FRESH, not
    /// dead (`ley-line-open-1869d0`). Reported rather than silently carried,
    /// matching `BlobActivationReport::skipped_sub_floor_blobs`.
    pub stranded_source_blobs_manifest_rows: u64,
}

/// Progress emitted after each completely processed query page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ActivationProgress {
    /// Fresh or populated rows visited so far.
    pub visited_nodes: u64,
    /// Total eligible rows observed before processing began.
    pub eligible_nodes: u64,
    /// Rows populated or rebuilt so far.
    pub populated_nodes: u64,
    /// Rows already fresh so far.
    pub already_fresh_nodes: u64,
    /// Authoritative bytes processed so far.
    pub processed_source_bytes: u64,
}

/// Create the CDC schema and backfill every authoritative readable leaf.
///
/// Each node store is its own transaction. A failed or interrupted invocation
/// therefore resumes by skipping manifests whose freshness witness already
/// matches the current `nodes` row.
pub fn activate_chunked_content(
    conn: &Connection,
    options: ActivationOptions,
) -> Result<ActivationReport> {
    activate_chunked_content_with_progress(conn, options, |_| {})
}

/// Activate CDC and emit one progress update after each completed query page.
pub fn activate_chunked_content_with_progress<F>(
    conn: &Connection,
    options: ActivationOptions,
    mut on_progress: F,
) -> Result<ActivationReport>
where
    F: FnMut(ActivationProgress),
{
    ensure!(
        options.batch_size > 0,
        "CDC activation batch_size must be > 0"
    );
    let batch_size = i64::try_from(options.batch_size)
        .context("CDC activation batch_size exceeds SQLite i64")?;
    validate_nodes_contract(conn)?;
    create_chunked_content_schema(conn)?;

    let estimated_eligible_nodes = query_count(
        conn,
        "SELECT COUNT(*) FROM nodes WHERE kind = 0 AND record IS NOT NULL",
        "count eligible CDC nodes",
    )?;

    let mut populated_nodes = 0_u64;
    let mut already_fresh_nodes = 0_u64;
    let mut processed_source_bytes = 0_u64;
    let mut visited_nodes = 0_u64;
    let mut last_nid = None;

    loop {
        let rows = query_activation_page(conn, last_nid, batch_size)?;

        if rows.is_empty() {
            break;
        }
        last_nid = rows.last().copied();

        // One transaction per PAGE, not per node. The work per node is a small
        // read plus a manifest rewrite; the commit dominated it, so a
        // 391,556-leaf projection paid 391,556 commits and ran at ~10 KiB/s.
        // Resumability survives at page granularity: a crash re-does at most
        // `batch_size` nodes, and re-activation is idempotent via AlreadyFresh.
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
            .context("begin CDC activation page transaction")?;
        for nid in rows {
            match activate_node_in_tx(&tx, nid)? {
                NodeActivation::Gone => {}
                NodeActivation::AlreadyFresh => {
                    visited_nodes = checked_increment(visited_nodes, "visited CDC node count")?;
                    already_fresh_nodes =
                        checked_increment(already_fresh_nodes, "already-fresh CDC node count")?;
                }
                NodeActivation::Populated { source_bytes } => {
                    visited_nodes = checked_increment(visited_nodes, "visited CDC node count")?;
                    populated_nodes =
                        checked_increment(populated_nodes, "populated CDC node count")?;
                    processed_source_bytes = processed_source_bytes
                        .checked_add(source_bytes)
                        .context("processed CDC byte count overflow")?;
                }
            }
        }
        tx.commit().context("commit CDC activation page")?;
        on_progress(ActivationProgress {
            visited_nodes,
            eligible_nodes: estimated_eligible_nodes,
            populated_nodes,
            already_fresh_nodes,
            processed_source_bytes,
        });
    }

    loop {
        // Exclude writers while proving the committed generation complete.
        // A row inserted or changed behind the keyset cursor is repaired
        // directly, keeping query memory bounded by batch_size.
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
            .context("begin final CDC activation freshness check")?;
        let stale_node = first_nonfresh_node(&tx, batch_size)?;
        let Some(stale_node) = stale_node else {
            let report = ActivationReport {
                eligible_nodes: query_count(
                    &tx,
                    "SELECT COUNT(*) FROM nodes WHERE kind = 0 AND record IS NOT NULL",
                    "count final eligible CDC nodes",
                )?,
                populated_nodes,
                already_fresh_nodes,
                processed_source_bytes,
                manifest_rows: query_count(
                    &tx,
                    "SELECT COUNT(*) FROM content_manifest",
                    "count CDC manifest rows",
                )?,
                unique_chunk_rows: query_count(
                    &tx,
                    "SELECT COUNT(*) FROM content_chunks",
                    "count unique CDC chunks",
                )?,
                unique_chunk_bytes: query_count(
                    &tx,
                    "SELECT COALESCE(SUM(length(chunk_bytes)), 0) FROM content_chunks",
                    "sum unique CDC chunk bytes",
                )?,
                stranded_source_blobs_manifest_rows: table_row_count_if_exists(
                    &tx,
                    "blob_manifest",
                )?,
            };
            tx.commit()
                .context("commit final CDC activation freshness check")?;
            return Ok(report);
        };
        tx.commit()
            .context("commit CDC activation convergence check")?;

        match activate_node(conn, stale_node)? {
            NodeActivation::Gone => continue,
            NodeActivation::AlreadyFresh => {
                visited_nodes = checked_increment(visited_nodes, "visited CDC node count")?;
                already_fresh_nodes =
                    checked_increment(already_fresh_nodes, "already-fresh CDC node count")?;
            }
            NodeActivation::Populated { source_bytes } => {
                visited_nodes = checked_increment(visited_nodes, "visited CDC node count")?;
                populated_nodes = checked_increment(populated_nodes, "populated CDC node count")?;
                processed_source_bytes = processed_source_bytes
                    .checked_add(source_bytes)
                    .context("processed CDC byte count overflow")?;
            }
        }
        on_progress(ActivationProgress {
            visited_nodes,
            eligible_nodes: estimated_eligible_nodes,
            populated_nodes,
            already_fresh_nodes,
            processed_source_bytes,
        });
    }
}

/// One keyset page of eligible nodes, ordered by the projection's own key.
///
/// projection-v5: the cursor is the integer `nid`, so the page walk rides the
/// PRIMARY KEY directly instead of ordering a TEXT ancestry path.
fn query_activation_page(
    conn: &Connection,
    last_nid: Option<i64>,
    batch_size: i64,
) -> Result<Vec<i64>> {
    let sql = match last_nid {
        Some(_) => {
            "SELECT nid
               FROM nodes
              WHERE kind = 0 AND record IS NOT NULL AND nid > ?1
              ORDER BY nid
              LIMIT ?2"
        }
        None => {
            "SELECT nid
               FROM nodes
              WHERE kind = 0 AND record IS NOT NULL
              ORDER BY nid
              LIMIT ?1"
        }
    };
    let mut stmt = conn.prepare(sql).context("prepare CDC activation page")?;
    let mapped = if let Some(cursor) = last_nid {
        stmt.query_map(params![cursor, batch_size], read_nid)
    } else {
        stmt.query_map(params![batch_size], read_nid)
    }
    .context("query CDC activation page")?;
    mapped
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("decode CDC activation page")
}

fn read_nid(row: &rusqlite::Row<'_>) -> rusqlite::Result<i64> {
    row.get(0)
}

fn first_nonfresh_node(tx: &Transaction<'_>, batch_size: i64) -> Result<Option<i64>> {
    let mut last_nid = None;
    loop {
        let rows = query_activation_page(tx, last_nid, batch_size)?;
        if rows.is_empty() {
            return Ok(None);
        }
        last_nid = rows.last().copied();
        for nid in rows {
            if !has_chunked_content_in_transaction(tx, nid)
                .with_context(|| format!("verify final CDC freshness for node {nid}"))?
            {
                return Ok(Some(nid));
            }
        }
    }
}

fn checked_increment(value: u64, context: &'static str) -> Result<u64> {
    value
        .checked_add(1)
        .with_context(|| format!("{context} overflow"))
}

enum NodeActivation {
    Gone,
    AlreadyFresh,
    Populated { source_bytes: u64 },
}

/// Activate one node inside a caller-owned transaction.
///
/// The page loop batches many of these into one commit; the convergence loop
/// uses [`activate_node`] for its single-node repairs. Neither begins nor
/// commits — that is the caller's, so commit granularity is a policy decision
/// rather than a property of this function.
fn activate_node_in_tx(tx: &Transaction<'_>, nid: i64) -> Result<NodeActivation> {
    let source: Option<(Vec<u8>, i64)> = tx
        .query_row(
            "SELECT CAST(record AS BLOB), size
               FROM nodes
              WHERE nid = ?1 AND kind = 0 AND record IS NOT NULL",
            params![nid],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .with_context(|| format!("read authoritative CDC source for node {nid}"))?;
    let Some((data, declared_size)) = source else {
        return Ok(NodeActivation::Gone);
    };
    ensure!(
        declared_size >= 0 && u64::try_from(declared_size).ok() == u64::try_from(data.len()).ok(),
        "node {nid} size {declared_size} does not match {} record bytes",
        data.len()
    );
    if has_chunked_content_in_transaction(tx, nid)
        .with_context(|| format!("check CDC freshness for node {nid}"))?
    {
        return Ok(NodeActivation::AlreadyFresh);
    }
    store_content_chunked_in_transaction(tx, nid, &data)
        .with_context(|| format!("activate CDC for node {nid}"))?;
    Ok(NodeActivation::Populated {
        source_bytes: u64::try_from(data.len()).context("node length exceeds u64")?,
    })
}

/// Activate a single node in its own transaction — the convergence loop's
/// repair path, where one stale row is fixed at a time.
fn activate_node(conn: &Connection, nid: i64) -> Result<NodeActivation> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .with_context(|| format!("begin CDC activation transaction for node {nid}"))?;
    let outcome = activate_node_in_tx(&tx, nid)?;
    tx.commit()
        .with_context(|| format!("commit CDC activation for node {nid}"))?;
    Ok(outcome)
}

fn validate_nodes_contract(conn: &Connection) -> Result<()> {
    let present: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master
             WHERE type = 'table' AND name = 'nodes'",
            [],
            |row| row.get(0),
        )
        .context("probe for required nodes table")?;
    ensure!(present, "missing required nodes table for CDC activation");

    let mut stmt = conn
        .prepare("SELECT name FROM pragma_table_info('nodes')")
        .context("inspect nodes columns for CDC activation")?;
    let actual = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .context("query nodes columns for CDC activation")?
        .collect::<rusqlite::Result<BTreeSet<_>>>()
        .context("decode nodes columns for CDC activation")?;
    let required = ["kind", "mtime", "nid", "record", "size"];
    let missing = required
        .into_iter()
        .filter(|column| !actual.contains(*column))
        .collect::<Vec<_>>();
    ensure!(
        missing.is_empty(),
        "missing required nodes columns: {}",
        missing.join(", ")
    );
    Ok(())
}

fn query_count(conn: &Connection, sql: &str, context: &'static str) -> Result<u64> {
    let value: i64 = conn.query_row(sql, [], |row| row.get(0)).context(context)?;
    ensure!(value >= 0, "{context} returned negative value {value}");
    u64::try_from(value).context("nonnegative SQLite count exceeds u64")
}

/// Row count of `table` if it exists, else 0.
///
/// Used to report the OTHER activation target's manifest rows, so a
/// `nodes`-only or `source_blobs`-only activation never has to CREATE the
/// other target's schema purely to learn it is empty.
/// `create_blob_chunked_schema`'s doc comment declines exactly that cost for
/// `content_manifest`'s nodes generation triggers — this helper keeps the
/// count query from reintroducing it through the back door. `table` is
/// always a static literal from this module, never external input.
fn table_row_count_if_exists(conn: &Connection, table: &str) -> Result<u64> {
    let exists = query_count(
        conn,
        &format!("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = '{table}'"),
        "check for the other CDC target's manifest table",
    )? > 0;
    if !exists {
        return Ok(0);
    }
    query_count(
        conn,
        &format!("SELECT COUNT(*) FROM {table}"),
        "count the other CDC target's stranded manifest rows",
    )
}

// ── source_blobs target ──────────────────────────────────────────────────────

/// Deterministic summary of one `source_blobs` activation invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BlobActivationReport {
    /// Total blobs at or above the chunking floor in committed final state.
    pub eligible_blobs: u64,
    /// Blobs populated or rebuilt by this invocation.
    pub populated_blobs: u64,
    /// Blobs whose committed manifest was already complete.
    pub already_fresh_blobs: u64,
    /// Blobs below the chunking floor in committed final state, skipped by
    /// design. Counted so the policy is visible in every report: a sub-floor
    /// row chunk-stored anyway is pure overhead — the measured failure mode
    /// this target exists to avoid (bead `ley-line-open-baa57f`: 395,173 of
    /// 395,173 nodes sub-floor on a real mache projection, +21% database
    /// size, 440 MB of overhead for 1.9 MB of dedup).
    pub skipped_sub_floor_blobs: u64,
    /// Authoritative bytes processed by this invocation.
    pub processed_source_bytes: u64,
    /// Total blob manifest span rows in committed final state.
    pub manifest_rows: u64,
    /// Total unique chunk rows in committed final state. The pool is shared
    /// with the `nodes` target, so this counts the whole pool.
    pub unique_chunk_rows: u64,
    /// Total bytes stored across unique chunk rows in committed final state.
    pub unique_chunk_bytes: u64,
    /// Rows in `content_manifest` at commit — the `nodes` target's index,
    /// present only if that target was activated on this database at some
    /// point. Nonzero here means this `source_blobs` activation left it
    /// behind: ordinary GC cannot reclaim it, because those rows are still
    /// FRESH, not dead (`ley-line-open-1869d0`).
    pub stranded_nodes_manifest_rows: u64,
}

/// Progress emitted after each completely processed query page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BlobActivationProgress {
    /// Fresh or populated blobs visited so far.
    pub visited_blobs: u64,
    /// Total eligible blobs observed before processing began.
    pub eligible_blobs: u64,
    /// Blobs populated or rebuilt so far.
    pub populated_blobs: u64,
    /// Blobs already complete so far.
    pub already_fresh_blobs: u64,
    /// Authoritative bytes processed so far.
    pub processed_source_bytes: u64,
}

/// The eligibility floor, as a SQL-comparable integer. A blob below
/// [`leyline_cdc::MIN_CHUNK`] chunks to exactly one chunk identical to
/// itself: no possible dedup beyond what `source_blobs`' own
/// `INSERT OR IGNORE` already provides, at the cost of a manifest row, a
/// pool row, and two index entries. Such rows are SKIPPED and counted.
fn chunk_floor() -> Result<i64> {
    i64::try_from(leyline_cdc::MIN_CHUNK).context("chunking floor exceeds SQLite i64")
}

/// Create the blob CDC schema and backfill every eligible `source_blobs` row.
///
/// Mirrors [`activate_chunked_content`], minus everything mutation forced on
/// the `nodes` walk: rows are content-addressed and immutable, so there is no
/// freshness witness to capture and "already fresh" means the committed
/// manifest exists and tiles the blob completely.
pub fn activate_chunked_source_blobs(
    conn: &Connection,
    options: ActivationOptions,
) -> Result<BlobActivationReport> {
    activate_chunked_source_blobs_with_progress(conn, options, |_| {})
}

/// Activate the `source_blobs` target and emit one progress update after each
/// completed query page.
pub fn activate_chunked_source_blobs_with_progress<F>(
    conn: &Connection,
    options: ActivationOptions,
    mut on_progress: F,
) -> Result<BlobActivationReport>
where
    F: FnMut(BlobActivationProgress),
{
    ensure!(
        options.batch_size > 0,
        "CDC activation batch_size must be > 0"
    );
    let batch_size = i64::try_from(options.batch_size)
        .context("CDC activation batch_size exceeds SQLite i64")?;
    let floor = chunk_floor()?;
    validate_source_blobs_contract(conn)?;
    create_blob_chunked_schema(conn)?;

    let estimated_eligible_blobs = query_count(
        conn,
        &format!("SELECT COUNT(*) FROM source_blobs WHERE byte_len >= {floor}"),
        "count eligible CDC blobs",
    )?;

    let mut populated_blobs = 0_u64;
    let mut already_fresh_blobs = 0_u64;
    let mut processed_source_bytes = 0_u64;
    let mut visited_blobs = 0_u64;
    let mut last_hash: Option<Vec<u8>> = None;

    loop {
        let rows = query_blob_activation_page(conn, last_hash.as_deref(), batch_size, floor)?;
        if rows.is_empty() {
            break;
        }
        last_hash = rows.last().cloned();

        // One transaction per PAGE, not per blob — the same rationale as the
        // nodes walk: the commit dominates the per-row work, and page
        // granularity keeps a crash's redo bounded by batch_size while
        // re-activation stays idempotent via AlreadyFresh.
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
            .context("begin blob CDC activation page transaction")?;
        for blob_hash in rows {
            match activate_blob_in_tx(&tx, &blob_hash)? {
                BlobActivation::Gone => {}
                BlobActivation::AlreadyFresh => {
                    visited_blobs = checked_increment(visited_blobs, "visited CDC blob count")?;
                    already_fresh_blobs =
                        checked_increment(already_fresh_blobs, "already-fresh CDC blob count")?;
                }
                BlobActivation::Populated { source_bytes } => {
                    visited_blobs = checked_increment(visited_blobs, "visited CDC blob count")?;
                    populated_blobs =
                        checked_increment(populated_blobs, "populated CDC blob count")?;
                    processed_source_bytes = processed_source_bytes
                        .checked_add(source_bytes)
                        .context("processed CDC blob byte count overflow")?;
                }
            }
        }
        tx.commit().context("commit blob CDC activation page")?;
        on_progress(BlobActivationProgress {
            visited_blobs,
            eligible_blobs: estimated_eligible_blobs,
            populated_blobs,
            already_fresh_blobs,
            processed_source_bytes,
        });
    }

    loop {
        // Exclude writers while proving the committed set complete. A blob
        // inserted behind the keyset cursor is repaired directly, keeping
        // query memory bounded by batch_size. (Rows cannot CHANGE behind the
        // cursor — they are immutable — but cmd_parse can still be inserting
        // new ones concurrently.)
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
            .context("begin final blob CDC activation completeness check")?;
        let incomplete_blob = first_incomplete_blob(&tx, batch_size, floor)?;
        let Some(incomplete_blob) = incomplete_blob else {
            let report = BlobActivationReport {
                eligible_blobs: query_count(
                    &tx,
                    &format!("SELECT COUNT(*) FROM source_blobs WHERE byte_len >= {floor}"),
                    "count final eligible CDC blobs",
                )?,
                populated_blobs,
                already_fresh_blobs,
                skipped_sub_floor_blobs: query_count(
                    &tx,
                    &format!("SELECT COUNT(*) FROM source_blobs WHERE byte_len < {floor}"),
                    "count sub-floor CDC blobs",
                )?,
                processed_source_bytes,
                manifest_rows: query_count(
                    &tx,
                    "SELECT COUNT(*) FROM blob_manifest",
                    "count CDC blob manifest rows",
                )?,
                unique_chunk_rows: query_count(
                    &tx,
                    "SELECT COUNT(*) FROM content_chunks",
                    "count unique CDC chunks",
                )?,
                unique_chunk_bytes: query_count(
                    &tx,
                    "SELECT COALESCE(SUM(length(chunk_bytes)), 0) FROM content_chunks",
                    "sum unique CDC chunk bytes",
                )?,
                stranded_nodes_manifest_rows: table_row_count_if_exists(&tx, "content_manifest")?,
            };
            tx.commit()
                .context("commit final blob CDC activation completeness check")?;
            return Ok(report);
        };
        tx.commit()
            .context("commit blob CDC activation convergence check")?;

        match activate_blob(conn, &incomplete_blob)? {
            BlobActivation::Gone => continue,
            BlobActivation::AlreadyFresh => {
                visited_blobs = checked_increment(visited_blobs, "visited CDC blob count")?;
                already_fresh_blobs =
                    checked_increment(already_fresh_blobs, "already-fresh CDC blob count")?;
            }
            BlobActivation::Populated { source_bytes } => {
                visited_blobs = checked_increment(visited_blobs, "visited CDC blob count")?;
                populated_blobs = checked_increment(populated_blobs, "populated CDC blob count")?;
                processed_source_bytes = processed_source_bytes
                    .checked_add(source_bytes)
                    .context("processed CDC blob byte count overflow")?;
            }
        }
        on_progress(BlobActivationProgress {
            visited_blobs,
            eligible_blobs: estimated_eligible_blobs,
            populated_blobs,
            already_fresh_blobs,
            processed_source_bytes,
        });
    }
}

/// Keyset page over eligible blobs, ordered by `blob_hash`. BLOB comparison
/// in SQLite is memcmp — a deterministic total order, which is all a cursor
/// needs.
fn query_blob_activation_page(
    conn: &Connection,
    last_hash: Option<&[u8]>,
    batch_size: i64,
    floor: i64,
) -> Result<Vec<Vec<u8>>> {
    let (sql, cursor): (&str, Option<&[u8]>) = match last_hash {
        Some(cursor) => (
            "SELECT blob_hash
               FROM source_blobs
              WHERE byte_len >= ?1 AND blob_hash > ?2
              ORDER BY blob_hash
              LIMIT ?3",
            Some(cursor),
        ),
        None => (
            "SELECT blob_hash
               FROM source_blobs
              WHERE byte_len >= ?1
              ORDER BY blob_hash
              LIMIT ?2",
            None,
        ),
    };
    let mut stmt = conn
        .prepare(sql)
        .context("prepare blob CDC activation page")?;
    let mapped = if let Some(cursor) = cursor {
        stmt.query_map(params![floor, cursor, batch_size], read_blob_hash)
    } else {
        stmt.query_map(params![floor, batch_size], read_blob_hash)
    }
    .context("query blob CDC activation page")?;
    mapped
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("decode blob CDC activation page")
}

fn read_blob_hash(row: &rusqlite::Row<'_>) -> rusqlite::Result<Vec<u8>> {
    row.get(0)
}

fn first_incomplete_blob(
    tx: &Transaction<'_>,
    batch_size: i64,
    floor: i64,
) -> Result<Option<Vec<u8>>> {
    let mut last_hash: Option<Vec<u8>> = None;
    loop {
        let rows = query_blob_activation_page(tx, last_hash.as_deref(), batch_size, floor)?;
        if rows.is_empty() {
            return Ok(None);
        }
        last_hash = rows.last().cloned();
        for blob_hash in rows {
            if !has_blob_chunked(tx, decode_blob_hash(&blob_hash)?)
                .context("verify final blob CDC completeness")?
            {
                return Ok(Some(blob_hash));
            }
        }
    }
}

/// A `source_blobs` primary key that is not a 32-byte BLAKE3 hash violates
/// the table's contract outright — a hard error, not a skip, because every
/// row in this table claims to be content-addressed.
fn decode_blob_hash(blob_hash: &[u8]) -> Result<Hash> {
    let bytes: [u8; 32] = blob_hash.try_into().map_err(|_| {
        anyhow::anyhow!(
            "source_blobs key has {} bytes, expected 32",
            blob_hash.len()
        )
    })?;
    Ok(Hash::from_bytes(bytes))
}

enum BlobActivation {
    Gone,
    AlreadyFresh,
    Populated { source_bytes: u64 },
}

/// Activate one blob inside a caller-owned transaction. Same division of
/// labor as [`activate_node_in_tx`]: the page loop batches many of these
/// into one commit, the convergence loop repairs one at a time.
fn activate_blob_in_tx(tx: &Transaction<'_>, blob_hash: &[u8]) -> Result<BlobActivation> {
    let blob_hash = decode_blob_hash(blob_hash)?;
    if has_blob_chunked(tx, blob_hash).context("check blob CDC completeness")? {
        return Ok(BlobActivation::AlreadyFresh);
    }
    let bytes: Option<Vec<u8>> = tx
        .query_row(
            "SELECT blob_bytes FROM source_blobs WHERE blob_hash = ?1",
            params![blob_hash.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()
        .with_context(|| format!("read authoritative bytes for blob {blob_hash}"))?;
    let Some(bytes) = bytes else {
        return Ok(BlobActivation::Gone);
    };
    // The content-address integrity check lives in the store: a row whose
    // bytes do not hash to its key is refused, never manifested.
    store_blob_chunked_in_transaction(tx, blob_hash, &bytes)
        .with_context(|| format!("activate CDC for blob {blob_hash}"))?;
    // Liveness, not paranoia: the convergence loop repairs whatever this
    // probe reports non-fresh, so a store that does not satisfy its own
    // freshness probe turns that loop into an infinite spin on one blob —
    // observed as a mutants TIMEOUT (sqlite_integer → constant corrupted the
    // manifest rows) rather than any wrong answer. Same-transaction, so no
    // writer can race the check; in production the store writes exactly what
    // the probe reads and this cannot fire.
    ensure!(
        has_blob_chunked(tx, blob_hash).context("re-check blob CDC completeness")?,
        "activation stored blob {blob_hash} without making it fresh — refusing to spin"
    );
    Ok(BlobActivation::Populated {
        source_bytes: u64::try_from(bytes.len()).context("blob length exceeds u64")?,
    })
}

/// Activate a single blob in its own transaction — the convergence loop's
/// repair path.
fn activate_blob(conn: &Connection, blob_hash: &[u8]) -> Result<BlobActivation> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .context("begin blob CDC activation transaction")?;
    let outcome = activate_blob_in_tx(&tx, blob_hash)?;
    tx.commit().context("commit blob CDC activation")?;
    Ok(outcome)
}

fn validate_source_blobs_contract(conn: &Connection) -> Result<()> {
    let present: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master
             WHERE type = 'table' AND name = 'source_blobs'",
            [],
            |row| row.get(0),
        )
        .context("probe for required source_blobs table")?;
    ensure!(
        present,
        "missing required source_blobs table for CDC activation \
         (produced by `leyline parse`'s dual-store, ADR-0028)"
    );

    let mut stmt = conn
        // `table_xinfo` includes the generated byte_len column.
        .prepare("SELECT name FROM pragma_table_xinfo('source_blobs')")
        .context("inspect source_blobs columns for CDC activation")?;
    let actual = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .context("query source_blobs columns for CDC activation")?
        .collect::<rusqlite::Result<BTreeSet<_>>>()
        .context("decode source_blobs columns for CDC activation")?;
    let required = ["blob_hash", "blob_bytes", "byte_len"];
    let missing = required
        .into_iter()
        .filter(|column| !actual.contains(*column))
        .collect::<Vec<_>>();
    ensure!(
        missing.is_empty(),
        "missing required source_blobs columns: {}",
        missing.join(", ")
    );
    Ok(())
}

// In-module falsifiers for the diff-scoped mutants gate (lib tests only —
// a killer in tests/ is invisible to it). Same reasoning as blob_chunked's.
#[cfg(test)]
mod tests {
    use super::*;
    use leyline_core::ContentAddressed;

    /// Literal, not `MIN_CHUNK`: a floor test that reads the const moves its
    /// goalposts with the mutation (the CDC bounds lesson, twice now).
    #[test]
    fn the_chunking_floor_is_the_xet_floor() {
        assert_eq!(chunk_floor().unwrap(), 8192);
    }

    /// Direct unit coverage for `table_row_count_if_exists`, not just the
    /// end-to-end `switching_targets_reports_the_abandoned_index_as_stranded`
    /// falsifier in `fs/tests/cdc_source_blobs.rs`: this crate's diff-scoped
    /// mutants slice runs `-C --lib` only (`tools/mutants_diff.sh`), so a
    /// mutant here is invisible to any `tests/*.rs` integration file no
    /// matter how thorough. The three real answers, in order:
    ///   1. table absent → 0 (kills `Ok(0)`, trivially — but also the
    ///      baseline every other case must beat).
    ///   2. table present with 3 rows → 3, not merely nonzero (kills both
    ///      `Ok(0)` AND `Ok(1)`; a mutant surviving on a "nonzero" assertion
    ///      alone is exactly the report-formatting gap this module already
    ///      exists to avoid).
    ///   3. same table, real count still 3 — proves the exists-check itself
    ///      is live: `> 0` mutated to `< 0` on an UNSIGNED count is always
    ///      false, so a broken exists-check silently reports 0 even though
    ///      the table is right there with rows in it.
    #[test]
    fn table_row_count_if_exists_reports_zero_absent_and_the_real_count_present() {
        let conn = Connection::open_in_memory().unwrap();
        assert_eq!(
            table_row_count_if_exists(&conn, "definitely_not_a_real_table").unwrap(),
            0,
            "a table that was never created must report 0, not error"
        );

        conn.execute_batch("CREATE TABLE probe (id INTEGER PRIMARY KEY)")
            .unwrap();
        for _ in 0..3 {
            conn.execute("INSERT INTO probe DEFAULT VALUES", [])
                .unwrap();
        }
        assert_eq!(
            table_row_count_if_exists(&conn, "probe").unwrap(),
            3,
            "an existing table must report its REAL row count, not merely nonzero"
        );
    }

    /// The contract failure is a NAMED refusal, not whatever SQL error
    /// happens to fall out downstream — a mutant that skips the validation
    /// still errors on the missing table, so the message text is the
    /// distinguishing observable.
    #[test]
    fn activation_without_the_source_blobs_table_names_the_contract() {
        let conn = Connection::open_in_memory().unwrap();
        let err = activate_chunked_source_blobs(&conn, ActivationOptions::default()).unwrap_err();
        assert!(
            format!("{err:#}").contains("missing required source_blobs table"),
            "must refuse by name: {err:#}"
        );
    }

    /// The convergence probe's two answers, asserted directly: an eligible
    /// blob with no manifest is reported (an unconditional `Ok(None)` here
    /// silently voids the completeness proof), and a converged state is not.
    #[test]
    fn the_completeness_probe_reports_an_unactivated_blob_then_convergence() {
        let conn = Connection::open_in_memory().unwrap();
        leyline_ts::schema::create_source_blobs_table(&conn).unwrap();
        crate::blob_chunked::ensure_blob_manifest_infra(&conn).unwrap();
        let bytes = vec![b'q'; 9_000];
        let hash = bytes[..].hash();
        conn.execute(
            "INSERT INTO source_blobs (blob_hash, blob_bytes) VALUES (?1, ?2)",
            rusqlite::params![hash.as_bytes().as_slice(), bytes],
        )
        .unwrap();

        let tx = conn.unchecked_transaction().unwrap();
        let found = first_incomplete_blob(&tx, 16, chunk_floor().unwrap()).unwrap();
        assert_eq!(
            found.as_deref(),
            Some(hash.as_bytes().as_slice()),
            "an eligible blob with no manifest is incomplete"
        );
        drop(tx);

        activate_chunked_source_blobs(&conn, ActivationOptions::default()).unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        assert_eq!(
            first_incomplete_blob(&tx, 16, chunk_floor().unwrap()).unwrap(),
            None,
            "after activation the probe must report convergence"
        );
    }
}
