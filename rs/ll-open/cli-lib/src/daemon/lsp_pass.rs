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

/// Hard ceiling on total time spent enriching a single file (60f75d).
///
/// The poll loop above is a soft 5×200ms = 1s wait for symbol
/// availability. This wraps the entire per-file work (open_file →
/// poll → drain → merge) in a `tokio::time::timeout` so a
/// misbehaving language server (one that returns `Ok(empty)`
/// indefinitely on `documentSymbol`, or hangs on `didOpen`) can't
/// stall the enrichment loop forever.
///
/// Set to 5 seconds — generous enough for cold-start indexing on
/// large files (rust-analyzer first-touch can be slow), tight enough
/// that 50 files × 5s = 4 minutes is the worst-case batch instead of
/// indefinite. Per-file timeout failure logs at warn and the batch
/// proceeds to the next file.
const PASS_FILE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
///
/// Scoped runs use a single `WHERE id IN (?, ?, ...)` query rather
/// than N+1 individual lookups: at registry-repo scale (typical dirty
/// set 1-10 files in a 50k-row _source table) the loop-and-query
/// approach paid round-trip cost per file. Above SQLITE_VAR_LIMIT=999
/// we fall back to an in-memory filter — chunking would require
/// multiple round-trips for marginal gain at that scope size.
fn collect_enrichment_targets(
    conn: &Connection,
    changed_files: Option<&[String]>,
) -> Result<Vec<(String, String)>> {
    const SQLITE_VAR_LIMIT: usize = 999;

    match changed_files {
        // Empty scope → no files to enrich (avoid building "WHERE id IN ()"
        // which is a SQL syntax error).
        Some([]) => Ok(Vec::new()),

        // Small scope → push into IN clause; SQLite uses _source.id PK.
        Some(rels) if rels.len() <= SQLITE_VAR_LIMIT => {
            let placeholders: Vec<&str> = rels.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT id, language FROM _source WHERE id IN ({})",
                placeholders.join(","),
            );
            let mut stmt = conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> =
                rels.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            let rows = stmt.query_map(params.as_slice(), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        }

        // Huge scope → full scan + in-memory filter. Rare; typical dirty
        // sets are 1-10 files. Above 999 we'd need to chunk the IN clause,
        // which buys nothing over a single full scan + HashSet at this size.
        Some(rels) => {
            let scope: std::collections::HashSet<&str> =
                rels.iter().map(String::as_str).collect();
            let mut stmt = conn.prepare("SELECT id, language FROM _source")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let pair = row?;
                if scope.contains(pair.0.as_str()) {
                    out.push(pair);
                }
            }
            Ok(out)
        }

        // No scope → enrich every file in _source.
        None => {
            let mut stmt = conn.prepare("SELECT id, language FROM _source")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        }
    }
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
            Err(e) => {
                // File in the dirty set but unreadable (deleted, permission
                // denied, race with mid-edit save). Log so operators can
                // investigate "why didn't this file get LSP-enriched"
                // without it killing the whole pass.
                log::debug!("lsp_pass: skip {}: {e}", abs_path.display());
                continue;
            }
        };

        let file_uri = format!("file://{}", abs_path.canonicalize().unwrap_or(abs_path.clone()).display());

        // Infer language ID from extension.
        let ext = abs_path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let language_id = language_id_from_ext(ext).unwrap_or(lang);

        // 60f75d: wrap the per-file work in a hard timeout so a
        // misbehaving language server can't stall the enrichment loop
        // forever. On timeout we log::warn, skip this file, and proceed
        // to the next one — the LSP client itself stays alive so a
        // single bad file doesn't poison the whole batch.
        let per_file = async {
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
                return Ok::<(u64, u64), anyhow::Error>((0, 0));
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

            // Enrich with definitions, hover, references.
            let stats = leyline_lsp::project::enrich_symbols(&mut client, conn, &symbols, &file_uri).await?;

            eprintln!(
                "lsp: {rel} — {matched} symbols, {} defs, {} hovers, {} refs",
                stats.definitions, stats.hovers, stats.references
            );

            let enriched = (stats.definitions + stats.hovers + stats.references) as u64;
            Ok((matched as u64, enriched))
        };

        match tokio::time::timeout(PASS_FILE_TIMEOUT, per_file).await {
            Ok(Ok((symbols, enriched))) => {
                total_symbols += symbols;
                total_enriched += enriched;
            }
            Ok(Err(e)) => {
                log::warn!("lsp: enrich failed for {rel}: {e:#}");
            }
            Err(_elapsed) => {
                log::warn!(
                    "lsp: per-file timeout ({:?}) exceeded for {rel}; skipping. \
                     Server may be misbehaving on this file.",
                    PASS_FILE_TIMEOUT,
                );
            }
        }
    }

    client.shutdown().await?;
    Ok((total_symbols, total_enriched))
}

// Tests for the language registry now live in `leyline-lsp::languages::tests`
// (single source of truth — see ley-line-open-5f7100-10).

#[cfg(test)]
mod tests {
    use super::*;

    /// 60f75d: the per-file timeout caps worst-case batch duration.
    /// Pin both the value (5s) and the relation to the soft poll
    /// timeout (PASS_SYMBOL_POLL_MAX_ATTEMPTS * PASS_SYMBOL_POLL_DELAY
    /// = 1s) — the hard timeout MUST exceed the soft poll's max wait,
    /// otherwise normal cold-start indexing trips the timeout.
    #[test]
    fn pass_file_timeout_exceeds_symbol_poll_max() {
        let soft_max = PASS_SYMBOL_POLL_DELAY * PASS_SYMBOL_POLL_MAX_ATTEMPTS as u32;
        assert!(
            PASS_FILE_TIMEOUT > soft_max,
            "PASS_FILE_TIMEOUT ({:?}) must exceed soft poll max ({:?}) — \
             otherwise cold-start indexing trips the timeout on every file",
            PASS_FILE_TIMEOUT, soft_max,
        );
        // Pin the actual value too. A refactor that bumped this to
        // 5 minutes (effectively unbounded for an interactive
        // workflow) would surface here. If the value legitimately
        // needs to change, update the doc on PASS_FILE_TIMEOUT and
        // this assertion together.
        assert_eq!(
            PASS_FILE_TIMEOUT,
            std::time::Duration::from_secs(5),
            "PASS_FILE_TIMEOUT pinned at 5s — see doc comment for rationale",
        );
    }

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

    /// Build a minimal _source table for the enrichment-targets tests.
    fn fresh_source_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE _source (id TEXT PRIMARY KEY, language TEXT, path TEXT);
             INSERT INTO _source (id, language, path) VALUES \
                ('a.go',  'go',     '/abs/a.go'),  \
                ('b.rs',  'rust',   '/abs/b.rs'),  \
                ('c.py',  'python', '/abs/c.py'),  \
                ('d.yml', 'yaml',   '/abs/d.yml');",
        )
        .unwrap();
        conn
    }

    #[test]
    fn collect_enrichment_targets_none_returns_all() {
        // Pin: changed_files = None means "enrich everything in _source."
        let conn = fresh_source_conn();
        let mut got = collect_enrichment_targets(&conn, None).unwrap();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("a.go".to_string(), "go".to_string()),
                ("b.rs".to_string(), "rust".to_string()),
                ("c.py".to_string(), "python".to_string()),
                ("d.yml".to_string(), "yaml".to_string()),
            ],
        );
    }

    #[test]
    fn collect_enrichment_targets_small_scope_uses_in_clause() {
        // Pin: scoped run returns only the requested files. The IN-
        // clause path replaced an N+1 query loop — same semantics,
        // 1 round-trip instead of N. Includes a non-existent path
        // ("missing.go") to confirm it's silently dropped (typical
        // git-watcher case: dirty file deleted before reparse arrives).
        let conn = fresh_source_conn();
        let scope = vec![
            "a.go".to_string(),
            "c.py".to_string(),
            "missing.go".to_string(),
        ];
        let mut got = collect_enrichment_targets(&conn, Some(&scope)).unwrap();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("a.go".to_string(), "go".to_string()),
                ("c.py".to_string(), "python".to_string()),
            ],
            "scoped run must return ONLY the existing scoped files",
        );
    }

    #[test]
    fn collect_enrichment_targets_empty_scope_returns_empty() {
        // Edge case pin: empty scope MUST return Vec::new(), not
        // construct invalid SQL like "WHERE id IN ()". Without the
        // empty-arm guard, the IN-clause builder would produce a SQL
        // syntax error.
        let conn = fresh_source_conn();
        let got = collect_enrichment_targets(&conn, Some(&[])).unwrap();
        assert!(got.is_empty(), "empty scope must produce empty result, got {got:?}");
    }
}

