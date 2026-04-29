//! LSP enrichment pass — spawns language servers, enriches the living db
//! with `_lsp*` tables (symbols, definitions, references, hover).
//!
//! Feature-gated behind `lsp`.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use rusqlite::Connection;

use super::enrichment::{EnrichmentPass, EnrichmentStats};

/// Symbol-poll cadence for daemon-driven enrichment. Tighter than the
/// one-shot `cmd_lsp` path because the daemon reuses the same server
/// across many files in a batch — by the time a second file is opened
/// the server is usually already indexed.
const PASS_SYMBOL_POLL_MAX_ATTEMPTS: usize = 5;
const PASS_SYMBOL_POLL_DELAY: std::time::Duration = std::time::Duration::from_millis(200);

/// Single-source-of-truth for LSP language support: ID + extensions + the
/// server invocation. Adding a language means adding one record; this makes
/// drift between "we recognize the file" and "we can launch a server for it"
/// structurally impossible.
///
/// `id` is the canonical LSP language identifier (matches LSP spec values
/// where applicable). `exts` is the set of file extensions that map to this
/// language. `server` is `(binary, args)` for spawning the language server.
struct LspLanguage {
    id: &'static str,
    exts: &'static [&'static str],
    server: (&'static str, &'static [&'static str]),
}

const LSP_LANGUAGES: &[LspLanguage] = &[
    LspLanguage { id: "go",        exts: &["go"],
        server: ("gopls", &["serve"]) },
    LspLanguage { id: "python",    exts: &["py"],
        server: ("pyright-langserver", &["--stdio"]) },
    LspLanguage { id: "rust",      exts: &["rs"],
        server: ("rust-analyzer", &[]) },
    LspLanguage { id: "typescript", exts: &["ts"],
        server: ("typescript-language-server", &["--stdio"]) },
    LspLanguage { id: "typescriptreact", exts: &["tsx"],
        server: ("typescript-language-server", &["--stdio"]) },
    LspLanguage { id: "javascript", exts: &["js"],
        server: ("typescript-language-server", &["--stdio"]) },
    LspLanguage { id: "javascriptreact", exts: &["jsx"],
        server: ("typescript-language-server", &["--stdio"]) },
    LspLanguage { id: "c",         exts: &["c"],
        server: ("clangd", &[]) },
    LspLanguage { id: "cpp",       exts: &["cpp", "cc", "cxx", "h", "hpp"],
        server: ("clangd", &[]) },
    LspLanguage { id: "java",      exts: &["java"],
        server: ("jdtls", &[]) },
    LspLanguage { id: "zig",       exts: &["zig"],
        server: ("zls", &[]) },
];

/// Look up the language server invocation for an LSP language ID.
fn language_server(lang: &str) -> Option<(&'static str, &'static [&'static str])> {
    LSP_LANGUAGES.iter().find(|l| l.id == lang).map(|l| l.server)
}

/// Infer the LSP language ID from a file extension.
fn language_id_from_ext(ext: &str) -> Option<&'static str> {
    LSP_LANGUAGES
        .iter()
        .find(|l| l.exts.contains(&ext))
        .map(|l| l.id)
}

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


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_has_a_server_and_at_least_one_ext() {
        for lang in LSP_LANGUAGES {
            assert!(!lang.id.is_empty(), "language id must not be empty");
            assert!(
                !lang.exts.is_empty(),
                "language `{}` must register at least one extension",
                lang.id,
            );
            assert!(
                !lang.server.0.is_empty(),
                "language `{}` must have a server binary",
                lang.id,
            );
        }
    }

    #[test]
    fn drift_guard_every_ext_resolves_to_a_server() {
        // The whole point of unifying the maps: if we recognize a file
        // extension, we can launch a server for it. If this test fails,
        // a language was registered without a server (or with an empty
        // one), which would cause silent enrichment skips downstream.
        for lang in LSP_LANGUAGES {
            for ext in lang.exts {
                let id = language_id_from_ext(ext)
                    .unwrap_or_else(|| panic!("ext `{ext}` not resolved"));
                let server = language_server(id)
                    .unwrap_or_else(|| panic!("language `{id}` has no server"));
                assert!(!server.0.is_empty(), "ext `{ext}` resolved to empty server");
            }
        }
    }

    #[test]
    fn extension_lookup_known_cases() {
        assert_eq!(language_id_from_ext("rs"),  Some("rust"));
        assert_eq!(language_id_from_ext("go"),  Some("go"));
        assert_eq!(language_id_from_ext("py"),  Some("python"));
        assert_eq!(language_id_from_ext("ts"),  Some("typescript"));
        assert_eq!(language_id_from_ext("tsx"), Some("typescriptreact"));
        assert_eq!(language_id_from_ext("hpp"), Some("cpp"));
    }

    #[test]
    fn extension_lookup_unknown_returns_none() {
        // Drift fix: ruby was previously mapped from `.rb` but had no
        // server, producing silent skips. Removed from the table.
        assert_eq!(language_id_from_ext("rb"),     None);
        assert_eq!(language_id_from_ext("md"),     None);
        assert_eq!(language_id_from_ext("foobar"), None);
        assert_eq!(language_id_from_ext(""),       None);
    }

    #[test]
    fn server_lookup_unknown_returns_none() {
        assert!(language_server("brainfuck").is_none());
        assert!(language_server("").is_none());
        // Sanity: ruby is now genuinely unsupported (was the drift case).
        assert!(language_server("ruby").is_none());
    }

    #[test]
    fn typescript_family_shares_one_server() {
        // Sanity-check the four-way ts/tsx/js/jsx mapping: all four route
        // to typescript-language-server but each declares its own LSP id.
        let ts = language_server("typescript").unwrap();
        let tsx = language_server("typescriptreact").unwrap();
        let js = language_server("javascript").unwrap();
        let jsx = language_server("javascriptreact").unwrap();
        assert_eq!(ts.0, "typescript-language-server");
        assert_eq!(ts, tsx);
        assert_eq!(ts, js);
        assert_eq!(ts, jsx);
    }
}

