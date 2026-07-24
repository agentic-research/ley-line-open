//! Production activation for chunk-backed content storage.
//!
//! Activation is explicit: opening a writable graph does not create or
//! populate the derived CDC tables. This module supplies one idempotent entry
//! point for library, CLI, and daemon consumers.

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};
use serde::Serialize;
use std::collections::BTreeSet;

use crate::chunked::{create_chunked_content_schema, has_chunked_content, store_content_chunked};

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
    validate_nodes_contract(conn)?;
    create_chunked_content_schema(conn)?;

    let eligible_nodes = query_count(
        conn,
        "SELECT COUNT(*) FROM nodes WHERE kind = 0 AND record IS NOT NULL",
        "count eligible CDC nodes",
    )?;

    let mut populated_nodes = 0_u64;
    let mut already_fresh_nodes = 0_u64;
    let mut processed_source_bytes = 0_u64;
    let mut offset = 0_u64;

    loop {
        let rows = {
            let mut stmt = conn
                .prepare(
                    "SELECT id, CAST(record AS BLOB), size
                       FROM nodes
                      WHERE kind = 0 AND record IS NOT NULL
                      ORDER BY id
                      LIMIT ?1 OFFSET ?2",
                )
                .context("prepare CDC activation page")?;
            let mapped = stmt
                .query_map(
                    params![
                        i64::try_from(options.batch_size)
                            .context("CDC activation batch_size exceeds SQLite i64")?,
                        i64::try_from(offset)
                            .context("CDC activation offset exceeds SQLite i64")?,
                    ],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Vec<u8>>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )
                .context("query CDC activation page")?;
            mapped
                .collect::<rusqlite::Result<Vec<_>>>()
                .context("decode CDC activation page")?
        };

        if rows.is_empty() {
            break;
        }
        offset = offset
            .checked_add(u64::try_from(rows.len()).context("CDC page length exceeds u64")?)
            .context("CDC activation offset overflow")?;

        for (node_id, data, declared_size) in rows {
            ensure!(
                declared_size >= 0
                    && u64::try_from(declared_size).ok() == u64::try_from(data.len()).ok(),
                "node {node_id} size {declared_size} does not match {} record bytes",
                data.len()
            );
            if has_chunked_content(conn, &node_id)
                .with_context(|| format!("check CDC freshness for node {node_id}"))?
            {
                already_fresh_nodes = already_fresh_nodes
                    .checked_add(1)
                    .context("already-fresh CDC node count overflow")?;
                continue;
            }
            store_content_chunked(conn, &node_id, &data)
                .with_context(|| format!("activate CDC for node {node_id}"))?;
            populated_nodes = populated_nodes
                .checked_add(1)
                .context("populated CDC node count overflow")?;
            processed_source_bytes = processed_source_bytes
                .checked_add(u64::try_from(data.len()).context("node length exceeds u64")?)
                .context("processed CDC byte count overflow")?;
        }
        on_progress(ActivationProgress {
            visited_nodes: offset,
            eligible_nodes,
            populated_nodes,
            already_fresh_nodes,
            processed_source_bytes,
        });
    }

    Ok(ActivationReport {
        eligible_nodes,
        populated_nodes,
        already_fresh_nodes,
        processed_source_bytes,
        manifest_rows: query_count(
            conn,
            "SELECT COUNT(*) FROM content_manifest",
            "count CDC manifest rows",
        )?,
        unique_chunk_rows: query_count(
            conn,
            "SELECT COUNT(*) FROM content_chunks",
            "count unique CDC chunks",
        )?,
        unique_chunk_bytes: query_count(
            conn,
            "SELECT COALESCE(SUM(length(chunk_bytes)), 0) FROM content_chunks",
            "sum unique CDC chunk bytes",
        )?,
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
