//! LSP command — spawns a language server, collects symbols + diagnostics,
//! and enriches a .db file with `_lsp*` tables.

use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, DatabaseName};

use leyline_lsp::client::LspClient;
use leyline_lsp::project;

/// Maximum poll attempts for `documentSymbol` after opening a file.
/// Some servers (rust-analyzer) need a beat to index — try a few times
/// before giving up.
const SYMBOL_POLL_MAX_ATTEMPTS: usize = 10;

/// Delay between `documentSymbol` poll attempts. Total max wait is
/// roughly `(SYMBOL_POLL_MAX_ATTEMPTS - 1) * SYMBOL_POLL_DELAY`.
const SYMBOL_POLL_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

// Language ID inference moved to leyline-lsp::languages — single source of
// truth shared with the daemon's lsp_pass.

/// Run the LSP subcommand.
///
/// Spawns a language server, opens the input file, collects symbols and
/// diagnostics, then writes (or merges into) a `.db` file.
pub async fn cmd_lsp(
    server: &str,
    server_args: &[String],
    input: &Path,
    output: &Path,
    merge_db: Option<&Path>,
    language_id: Option<&str>,
) -> Result<()> {
    // Canonicalize the input path and derive URIs.
    let input = input
        .canonicalize()
        .with_context(|| format!("canonicalize {}", input.display()))?;

    let language_id = match language_id {
        Some(id) => id.to_string(),
        None => {
            let ext = input
                .extension()
                .and_then(|e| e.to_str())
                .context("input file has no extension; pass --language-id")?;
            leyline_lsp::languages::language_id_from_ext(ext)
                .map(|s| s.to_string())
                .with_context(|| format!("unknown extension .{ext}; pass --language-id"))?
        }
    };

    let source_text =
        std::fs::read_to_string(&input).with_context(|| format!("read {}", input.display()))?;

    let file_uri = format!("file://{}", input.display());

    let root_uri = input
        .parent()
        .map(|p| format!("file://{}", p.display()))
        .unwrap_or_else(|| "file:///".to_string());

    // Start the LSP client.
    let args_refs: Vec<&str> = server_args.iter().map(|s| s.as_str()).collect();
    let mut client = LspClient::start(server, &args_refs, &root_uri)
        .await
        .with_context(|| format!("start LSP server: {server}"))?;

    // Open the file.
    client
        .open_file(&file_uri, &language_id, &source_text)
        .await
        .context("open file in LSP server")?;

    // Poll for document symbols (servers may need time to index).
    let mut symbols = Vec::new();
    for attempt in 0..SYMBOL_POLL_MAX_ATTEMPTS {
        match client.document_symbols(&file_uri).await {
            Ok(s) if !s.is_empty() => {
                symbols = s;
                break;
            }
            Ok(_) => {
                if attempt + 1 < SYMBOL_POLL_MAX_ATTEMPTS {
                    tokio::time::sleep(SYMBOL_POLL_DELAY).await;
                }
            }
            Err(e) => {
                if attempt + 1 < SYMBOL_POLL_MAX_ATTEMPTS {
                    log::debug!("symbol poll attempt {attempt}: {e}");
                    tokio::time::sleep(SYMBOL_POLL_DELAY).await;
                } else {
                    bail!(
                        "failed to get document symbols after {SYMBOL_POLL_MAX_ATTEMPTS} attempts: {e}"
                    );
                }
            }
        }
    }

    // Drain pending notifications (diagnostics arrive asynchronously).
    client.drain_notifications().await;

    // Flatten diagnostics from (uri, Vec<Diagnostic>) pairs.
    let diagnostics: Vec<_> = client
        .diagnostics
        .iter()
        .flat_map(|(_, diags)| diags.clone())
        .collect();

    eprintln!(
        "{} symbols, {} diagnostics collected",
        symbols.len(),
        diagnostics.len()
    );

    // Build the output database. Merge-mode loads an existing .db
    // and grafts LSP data alongside the AST; standalone-mode starts
    // fresh. Both paths converge to a single conn that's then
    // enriched, serialized, and written — factored out below to
    // remove a 6-line duplication.
    let conn = if let Some(db_path) = merge_db {
        let db_bytes =
            std::fs::read(db_path).with_context(|| format!("read {}", db_path.display()))?;

        let mut conn = Connection::open_in_memory()?;
        conn.deserialize_read_exact(
            DatabaseName::Main,
            Cursor::new(&db_bytes),
            db_bytes.len(),
            false,
        )
        .context("deserialize existing database")?;

        let matched = project::merge_lsp_into_ast(&symbols, &diagnostics, &conn)?;
        eprintln!("{matched} symbols matched to AST nodes");
        conn
    } else {
        let conn = Connection::open_in_memory()?;
        project::project_lsp_into(&symbols, &diagnostics, &file_uri, &conn)?;
        conn
    };

    let enrichment = project::enrich_symbols(&mut client, &conn, &symbols, &file_uri).await?;
    eprintln!("enrichment: {enrichment}");

    let data = conn.serialize(DatabaseName::Main)?;
    std::fs::write(output, &*data)
        .with_context(|| format!("write {}", output.display()))?;

    // Graceful shutdown.
    client.shutdown().await?;

    eprintln!("wrote {}", output.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Cross-registry alignment pin. cmd_lsp passes a language id from
    /// `leyline_lsp::languages::language_id_from_ext` into the LSP
    /// `didOpen` notification; the same files are parsed via
    /// `leyline_ts::TsLanguage::from_extension` upstream. The two
    /// registries must agree on every extension they share so a
    /// file's parser-id and lsp-id stay in lockstep — otherwise a
    /// .pyi stub or .markdown file silently lands in one path but
    /// not the other.
    ///
    /// Past drift cases that have been closed:
    ///   - python now includes .pyi in lsp registry (Python stubs).
    ///   - markdown now includes .markdown in lsp registry.
    ///
    /// If a new ext is added to ts but missed in lsp, this test fails
    /// with a clear "registry drift" message pointing at the ext.
    #[test]
    fn ts_and_lsp_extension_registries_agree() {
        use leyline_lsp::languages::language_id_from_ext;
        use leyline_ts::languages::TsLanguage;

        let probes: &[(&str, &str)] = &[
            ("html", "html"),
            ("htm", "html"),
            ("md", "markdown"),
            ("markdown", "markdown"),
            ("json", "json"),
            ("yaml", "yaml"),
            ("yml", "yaml"),
            ("go", "go"),
            ("py", "python"),
            ("pyi", "python"),
            ("ex", "elixir"),
            ("exs", "elixir"),
        ];

        for (ext, expected_id) in probes {
            assert!(
                TsLanguage::from_extension(ext).is_some(),
                "ts must recognize ext `{ext}`",
            );
            let lsp_id = language_id_from_ext(ext);
            assert_eq!(
                lsp_id,
                Some(*expected_id),
                "registry drift: ts knows `{ext}` as `{expected_id}` but \
                 lsp::language_id_from_ext returned {lsp_id:?}; add `{ext}` \
                 to the corresponding LspLanguage entry in lsp/languages.rs",
            );
        }
    }
}
