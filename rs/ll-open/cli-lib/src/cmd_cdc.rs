//! Explicit CDC activation command.

use std::path::Path;

use anyhow::{Context, Result};
use leyline_fs::activation::{ActivationOptions, ActivationReport, activate_chunked_content};

/// Activate chunk-backed content in an existing SQLite projection.
pub fn enable_database(db: &Path, options: ActivationOptions) -> Result<ActivationReport> {
    let conn = rusqlite::Connection::open(db)
        .with_context(|| format!("open CDC database {}", db.display()))?;
    activate_chunked_content(&conn, options)
        .with_context(|| format!("activate CDC in {}", db.display()))
}

/// Render a stable command result for humans or automation.
pub fn format_report(report: ActivationReport, json: bool) -> Result<String> {
    if json {
        return serde_json::to_string(&report).context("encode CDC activation report");
    }
    Ok(format!(
        "CDC enabled: eligible={} populated={} already_fresh={} source_bytes={} \
         manifest_rows={} unique_chunks={} unique_chunk_bytes={}",
        report.eligible_nodes,
        report.populated_nodes,
        report.already_fresh_nodes,
        report.processed_source_bytes,
        report.manifest_rows,
        report.unique_chunk_rows,
        report.unique_chunk_bytes,
    ))
}

/// CLI entry point for `leyline cdc enable`.
pub fn cmd_cdc_enable(db: &Path, batch_size: usize, json: bool) -> Result<()> {
    let report = enable_database(db, ActivationOptions { batch_size })?;
    println!("{}", format_report(report, json)?);
    Ok(())
}
