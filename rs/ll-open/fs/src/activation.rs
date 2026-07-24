//! Production activation for chunk-backed content storage.
//!
//! Activation is explicit: opening a writable graph does not create or
//! populate the derived CDC tables. This module supplies one idempotent entry
//! point for library, CLI, and daemon consumers.

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use std::collections::BTreeSet;

use crate::chunked::{
    create_chunked_content_schema, has_chunked_content, store_content_chunked_in_transaction,
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
    let mut last_id = None;

    loop {
        let rows = query_activation_page(conn, last_id.as_deref(), batch_size)?;

        if rows.is_empty() {
            break;
        }
        last_id = rows.last().cloned();

        for node_id in rows {
            match activate_node(conn, &node_id)? {
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
            };
            tx.commit()
                .context("commit final CDC activation freshness check")?;
            return Ok(report);
        };
        tx.commit()
            .context("commit CDC activation convergence check")?;

        match activate_node(conn, &stale_node)? {
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

fn query_activation_page(
    conn: &Connection,
    last_id: Option<&str>,
    batch_size: i64,
) -> Result<Vec<String>> {
    let (sql, cursor): (&str, Option<&str>) = match last_id {
        Some(cursor) => (
            "SELECT id
               FROM nodes
              WHERE kind = 0 AND record IS NOT NULL AND id > ?1
              ORDER BY id
              LIMIT ?2",
            Some(cursor),
        ),
        None => (
            "SELECT id
               FROM nodes
              WHERE kind = 0 AND record IS NOT NULL
              ORDER BY id
              LIMIT ?1",
            None,
        ),
    };
    let mut stmt = conn.prepare(sql).context("prepare CDC activation page")?;
    let mapped = if let Some(cursor) = cursor {
        stmt.query_map(params![cursor, batch_size], read_node_id)
    } else {
        stmt.query_map(params![batch_size], read_node_id)
    }
    .context("query CDC activation page")?;
    mapped
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("decode CDC activation page")
}

fn read_node_id(row: &rusqlite::Row<'_>) -> rusqlite::Result<String> {
    row.get(0)
}

fn first_nonfresh_node(conn: &Connection, batch_size: i64) -> Result<Option<String>> {
    let mut last_id = None;
    loop {
        let rows = query_activation_page(conn, last_id.as_deref(), batch_size)?;
        if rows.is_empty() {
            return Ok(None);
        }
        last_id = rows.last().cloned();
        for node_id in rows {
            if !has_chunked_content(conn, &node_id)
                .with_context(|| format!("verify final CDC freshness for node {node_id}"))?
            {
                return Ok(Some(node_id));
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

fn activate_node(conn: &Connection, node_id: &str) -> Result<NodeActivation> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .with_context(|| format!("begin CDC activation transaction for node {node_id}"))?;
    let source: Option<(Vec<u8>, i64)> = tx
        .query_row(
            "SELECT CAST(record AS BLOB), size
               FROM nodes
              WHERE id = ?1 AND kind = 0 AND record IS NOT NULL",
            params![node_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .with_context(|| format!("read authoritative CDC source for node {node_id}"))?;
    let Some((data, declared_size)) = source else {
        tx.commit()
            .with_context(|| format!("commit skipped CDC activation for node {node_id}"))?;
        return Ok(NodeActivation::Gone);
    };
    ensure!(
        declared_size >= 0 && u64::try_from(declared_size).ok() == u64::try_from(data.len()).ok(),
        "node {node_id} size {declared_size} does not match {} record bytes",
        data.len()
    );
    if has_chunked_content(&tx, node_id)
        .with_context(|| format!("check CDC freshness for node {node_id}"))?
    {
        tx.commit()
            .with_context(|| format!("commit fresh CDC activation for node {node_id}"))?;
        return Ok(NodeActivation::AlreadyFresh);
    }
    store_content_chunked_in_transaction(&tx, node_id, &data)
        .with_context(|| format!("activate CDC for node {node_id}"))?;
    tx.commit()
        .with_context(|| format!("commit CDC activation for node {node_id}"))?;
    Ok(NodeActivation::Populated {
        source_bytes: u64::try_from(data.len()).context("node length exceeds u64")?,
    })
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
    let required = ["id", "kind", "mtime", "record", "size"];
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
