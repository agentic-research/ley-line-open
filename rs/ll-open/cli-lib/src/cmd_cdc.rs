//! Explicit CDC activation command.

use std::path::Path;

use anyhow::{Context, Result};
use leyline_fs::activation::{
    ActivationOptions, ActivationProgress, ActivationReport, BlobActivationProgress,
    BlobActivationReport, activate_chunked_content_with_progress,
    activate_chunked_source_blobs_with_progress,
};
use leyline_fs::gc::{GcOptions, GcReport, collect_unreachable_chunks};

/// Activate chunk-backed content in an existing SQLite projection.
pub fn enable_database(db: &Path, options: ActivationOptions) -> Result<ActivationReport> {
    enable_database_with_progress(db, options, |_| {})
}

/// Activate a database while forwarding bounded page-level progress.
pub fn enable_database_with_progress<F>(
    db: &Path,
    options: ActivationOptions,
    on_progress: F,
) -> Result<ActivationReport>
where
    F: FnMut(ActivationProgress),
{
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .with_context(|| format!("open CDC database {}", db.display()))?;
    activate_chunked_content_with_progress(&conn, options, on_progress)
        .with_context(|| format!("activate CDC in {}", db.display()))
}

/// Render one stable page-level progress line for stderr.
pub fn format_progress(progress: ActivationProgress) -> String {
    format!(
        "CDC activation: visited={}/{} populated={} already_fresh={} source_bytes={}",
        progress.visited_nodes,
        progress.eligible_nodes,
        progress.populated_nodes,
        progress.already_fresh_nodes,
        progress.processed_source_bytes,
    )
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

/// CLI entry point for `leyline cdc enable` (the default `nodes` target).
pub fn cmd_cdc_enable(db: &Path, batch_size: usize, json: bool) -> Result<()> {
    let report = enable_database_with_progress(db, ActivationOptions { batch_size }, |progress| {
        eprintln!("{}", format_progress(progress))
    })?;
    println!("{}", format_report(report, json)?);
    Ok(())
}

/// Activate chunk-backed `source_blobs` storage in an existing SQLite
/// projection (`--target source-blobs`).
pub fn enable_source_blobs_database(
    db: &Path,
    options: ActivationOptions,
) -> Result<BlobActivationReport> {
    enable_source_blobs_database_with_progress(db, options, |_| {})
}

/// Activate the `source_blobs` target while forwarding bounded page-level
/// progress.
pub fn enable_source_blobs_database_with_progress<F>(
    db: &Path,
    options: ActivationOptions,
    on_progress: F,
) -> Result<BlobActivationReport>
where
    F: FnMut(BlobActivationProgress),
{
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .with_context(|| format!("open CDC database {}", db.display()))?;
    activate_chunked_source_blobs_with_progress(&conn, options, on_progress)
        .with_context(|| format!("activate blob CDC in {}", db.display()))
}

/// Render one stable page-level progress line for stderr.
pub fn format_blob_progress(progress: BlobActivationProgress) -> String {
    format!(
        "CDC source_blobs activation: visited={}/{} populated={} already_fresh={} source_bytes={}",
        progress.visited_blobs,
        progress.eligible_blobs,
        progress.populated_blobs,
        progress.already_fresh_blobs,
        progress.processed_source_bytes,
    )
}

/// Render a stable command result for humans or automation.
pub fn format_blob_report(report: BlobActivationReport, json: bool) -> Result<String> {
    if json {
        return serde_json::to_string(&report).context("encode blob CDC activation report");
    }
    Ok(format!(
        "CDC source_blobs enabled: eligible={} populated={} already_fresh={} \
         skipped_sub_floor={} source_bytes={} manifest_rows={} unique_chunks={} \
         unique_chunk_bytes={}",
        report.eligible_blobs,
        report.populated_blobs,
        report.already_fresh_blobs,
        report.skipped_sub_floor_blobs,
        report.processed_source_bytes,
        report.manifest_rows,
        report.unique_chunk_rows,
        report.unique_chunk_bytes,
    ))
}

/// CLI entry point for `leyline cdc enable --target source-blobs`.
pub fn cmd_cdc_enable_source_blobs(db: &Path, batch_size: usize, json: bool) -> Result<()> {
    let report =
        enable_source_blobs_database_with_progress(db, ActivationOptions { batch_size }, |p| {
            eprintln!("{}", format_blob_progress(p))
        })?;
    println!("{}", format_blob_report(report, json)?);
    Ok(())
}

/// Collect unreachable chunks in an existing SQLite projection.
pub fn gc_database(db: &Path, options: GcOptions) -> Result<GcReport> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .with_context(|| format!("open CDC database {}", db.display()))?;
    collect_unreachable_chunks(&conn, options)
        .with_context(|| format!("collect unreachable CDC chunks in {}", db.display()))
}

/// Render one stable GC result for humans or automation.
pub fn format_gc_report(report: GcReport, json: bool) -> Result<String> {
    if json {
        return serde_json::to_string(&report).context("encode CDC GC report");
    }
    Ok(format!(
        "CDC GC: dry_run={} before_rows={} before_bytes={} \
         unreachable_rows={} unreachable_bytes={} deleted_rows={} deleted_bytes={} \
         remaining_rows={} remaining_bytes={} \
         reaped_manifest_rows={} reaped_manifest_nodes={} \
         reaped_blob_manifest_rows={} reaped_blob_manifest_blobs={}",
        report.dry_run,
        report.before_chunk_rows,
        report.before_chunk_bytes,
        report.unreachable_chunk_rows,
        report.unreachable_chunk_bytes,
        report.deleted_chunk_rows,
        report.deleted_chunk_bytes,
        report.remaining_chunk_rows,
        report.remaining_chunk_bytes,
        report.reaped_manifest_rows,
        report.reaped_manifest_nodes,
        report.reaped_blob_manifest_rows,
        report.reaped_blob_manifest_blobs,
    ))
}

/// CLI entry point for `leyline cdc gc`.
pub fn cmd_cdc_gc(db: &Path, dry_run: bool, json: bool) -> Result<()> {
    let report = gc_database(db, GcOptions { dry_run })?;
    println!("{}", format_gc_report(report, json)?);
    Ok(())
}
