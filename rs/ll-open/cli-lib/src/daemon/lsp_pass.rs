//! LSP enrichment pass — spawns language servers, enriches the living db
//! with `_lsp*` tables (symbols, definitions, references, hover).
//!
//! Feature-gated behind `lsp`.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use rusqlite::Connection;

use leyline_lsp::languages::{language_id_from_ext, language_server};

use super::enrichment::{EnrichmentPass, EnrichmentStats};

/// Symbol-poll cadence for daemon-driven enrichment. Tighter than the
/// one-shot `cmd_lsp` path because the daemon reuses the same server
/// across many files in a batch — by the time a second file is opened
/// the server is usually already indexed.
const PASS_SYMBOL_POLL_MAX_ATTEMPTS: usize = 5;
const PASS_SYMBOL_POLL_DELAY: std::time::Duration = std::time::Duration::from_millis(200);

/// LSP enrichment pass.
///
/// Spawns language servers for each language found in `_source`, collects
/// document symbols, merges into the living db's `_lsp*` tables. Enriches
/// each symbol with go-to-definition, hover, and references.
pub struct LspEnrichmentPass;

impl EnrichmentPass for LspEnrichmentPass {
    fn name(&self) -> &str {
        "lsp"
    }

    fn depends_on(&self) -> &[&str] {
        &["tree-sitter"]
    }

    fn reads(&self) -> &[&str] {
        &["_source", "_ast", "nodes"]
    }

    fn writes(&self) -> &[&str] {
        &["_lsp", "_lsp_defs", "_lsp_refs", "_lsp_hover", "_lsp_completions"]
    }

    fn run(
        &self,
        conn: &Connection,
        source_dir: &Path,
        changed_files: Option<&[String]>,
    ) -> Result<EnrichmentStats> {
        // LSP is async — bridge into the tokio runtime.
        let handle = tokio::runtime::Handle::try_current()
            .context("LspEnrichmentPass requires a tokio runtime")?;

        let source_dir = source_dir.to_path_buf();
        let files = collect_enrichment_targets(conn, changed_files)?;

        if files.is_empty() {
            return Ok(EnrichmentStats {
                pass_name: "lsp".to_string(),
                files_processed: 0,
                items_added: 0,
                duration_ms: 0,
            });
        }

        // Create LSP schema tables.
        leyline_lsp::project::create_lsp_schema(conn)?;

        // Group files by language.
        let mut by_lang: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for (rel, lang) in &files {
            by_lang
                .entry(lang.clone())
                .or_default()
                .push((rel.clone(), lang.clone()));
        }

        let start = Instant::now();
        let mut total_symbols = 0u64;
        let mut total_enriched = 0u64;

        for (lang, lang_files) in &by_lang {
            let (server_cmd, server_args) = match language_server(lang) {
                Some(s) => s,
                None => {
                    eprintln!("lsp: no server for language '{lang}', skipping {} file(s)", lang_files.len());
                    continue;
                }
            };

            // Check if the server is available.
            if which::which(server_cmd).is_err() {
                eprintln!("lsp: {server_cmd} not on PATH, skipping {lang}");
                continue;
            }

            let root_uri = format!(
                "file://{}",
                source_dir
                    .canonicalize()
                    .unwrap_or_else(|_| source_dir.clone())
                    .display()
            );

            // Spawn one server per language, enrich all files.
            let result = tokio::task::block_in_place(|| {
                handle.block_on(enrich_files(
                    conn,
                    server_cmd,
                    server_args,
                    &root_uri,
                    &source_dir,
                    lang,
                    lang_files,
                ))
            });

            match result {
                Ok((syms, enriched)) => {
                    total_symbols += syms;
                    total_enriched += enriched;
                }
                Err(e) => {
                    eprintln!("lsp: {server_cmd} failed for {lang}: {e:#}");
                }
            }
        }

        Ok(EnrichmentStats {
            pass_name: "lsp".to_string(),
            files_processed: files.len() as u64,
            items_added: total_symbols + total_enriched,
            duration_ms: start.elapsed().as_millis() as u64,
        })
    }
}

/// Collect files to enrich from the _source table.
fn collect_enrichment_targets(
    conn: &Connection,
    changed_files: Option<&[String]>,
) -> Result<Vec<(String, String)>> {
    let mut files = Vec::new();

    if let Some(changed) = changed_files {
        // Only enrich changed files.
        let mut stmt = conn.prepare(
            "SELECT id, language FROM _source WHERE id = ?1",
        )?;
        for rel in changed {
            if let Ok(row) = stmt.query_row([rel], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            }) {
                files.push(row);
            }
        }
    } else {
        // Enrich all files.
        let mut stmt = conn.prepare("SELECT id, language FROM _source")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            files.push(row?);
        }
    }

    Ok(files)
}

/// Spawn an LSP server, enrich a batch of files, shut down.
async fn enrich_files(
    conn: &Connection,
    server_cmd: &str,
    server_args: &[&str],
    root_uri: &str,
    source_dir: &Path,
    lang: &str,
    files: &[(String, String)],
) -> Result<(u64, u64)> {
    let mut client = leyline_lsp::client::LspClient::start(server_cmd, server_args, root_uri)
        .await
        .with_context(|| format!("start {server_cmd}"))?;

    let mut total_symbols = 0u64;
    let mut total_enriched = 0u64;

    for (rel, _lang_id) in files {
        let abs_path = source_dir.join(rel);
        let source_text = match std::fs::read_to_string(&abs_path) {
            Ok(t) => t,
            Err(_) => continue,
        };

        let file_uri = format!("file://{}", abs_path.canonicalize().unwrap_or(abs_path.clone()).display());

        // Infer language ID from extension.
        let ext = abs_path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let language_id = language_id_from_ext(ext).unwrap_or(lang);

        client.open_file(&file_uri, language_id, &source_text).await?;

        // Poll for symbols (servers may need indexing time).
        let mut symbols = Vec::new();
        for attempt in 0..PASS_SYMBOL_POLL_MAX_ATTEMPTS {
            match client.document_symbols(&file_uri).await {
                Ok(s) if !s.is_empty() => {
                    symbols = s;
                    break;
                }
                _ if attempt + 1 < PASS_SYMBOL_POLL_MAX_ATTEMPTS => {
                    tokio::time::sleep(PASS_SYMBOL_POLL_DELAY).await;
                }
                _ => break,
            }
        }

        if symbols.is_empty() {
            continue;
        }

        // Drain diagnostics.
        client.drain_notifications().await;
        let diagnostics: Vec<_> = client
            .diagnostics
            .iter()
            .flat_map(|(_, diags)| diags.clone())
            .collect();

        // Merge symbols into AST nodes.
        let matched = leyline_lsp::project::merge_lsp_into_ast(&symbols, &diagnostics, conn)?;
        total_symbols += matched as u64;

        // Enrich with definitions, hover, references.
        let stats = leyline_lsp::project::enrich_symbols(&mut client, conn, &symbols, &file_uri).await?;
        total_enriched += (stats.definitions + stats.hovers + stats.references) as u64;

        eprintln!(
            "lsp: {rel} — {matched} symbols, {} defs, {} hovers, {} refs",
            stats.definitions, stats.hovers, stats.references
        );
    }

    client.shutdown().await?;
    Ok((total_symbols, total_enriched))
}

// Tests for the language registry now live in `leyline-lsp::languages::tests`
// (single source of truth — see ley-line-open-5f7100-10).

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsp_enrichment_pass_trait_metadata_pin() {
        // Third in the EnrichmentPass-metadata triplet. resolve_order
        // keys on name + depends_on; drift breaks dep resolution
        // silently. The 5 _lsp* tables in writes are the schema-
        // partition contract.
        crate::daemon::enrichment::assert_pass_metadata(
            &LspEnrichmentPass,
            "lsp",
            &["tree-sitter"],
            &["_source", "_ast", "nodes"],
            &["_lsp", "_lsp_defs", "_lsp_refs", "_lsp_hover", "_lsp_completions"],
        );
    }
}

