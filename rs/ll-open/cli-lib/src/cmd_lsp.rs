//! LSP command — spawns a language server, collects symbols + diagnostics,
//! and enriches a .db file with `_lsp*` tables.

use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, DatabaseName};

use leyline_lsp::client::LspClient;
use leyline_lsp::project;

/// Infer LSP language ID from file extension.
fn infer_language_id(ext: &str) -> Option<&'static str> {
    match ext {
        "py" => Some("python"),
        "rs" => Some("rust"),
        "go" => Some("go"),
        "js" => Some("javascript"),
        "ts" => Some("typescript"),
        "jsx" => Some("javascriptreact"),
        "tsx" => Some("typescriptreact"),
        "c" => Some("c"),
        "cpp" | "cc" | "cxx" => Some("cpp"),
        "h" | "hpp" => Some("cpp"),
        "java" => Some("java"),
        "rb" => Some("ruby"),
        "ex" | "exs" => Some("elixir"),
        "lua" => Some("lua"),
        "sh" | "bash" => Some("shellscript"),
        "css" => Some("css"),
        "html" | "htm" => Some("html"),
        "json" => Some("json"),
        "yaml" | "yml" => Some("yaml"),
        "toml" => Some("toml"),
        "md" => Some("markdown"),
        "zig" => Some("zig"),
        "swift" => Some("swift"),
        "kt" | "kts" => Some("kotlin"),
        "tf" | "hcl" => Some("terraform"),
        _ => None,
    }
}

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
            infer_language_id(ext)
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
    for attempt in 0..10 {
        match client.document_symbols(&file_uri).await {
            Ok(s) if !s.is_empty() => {
                symbols = s;
                break;
            }
            Ok(_) => {
                if attempt < 9 {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                }
            }
            Err(e) => {
                if attempt < 9 {
                    log::debug!("symbol poll attempt {attempt}: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                } else {
                    bail!("failed to get document symbols after 10 attempts: {e}");
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

    // Build the output database.
    if let Some(db_path) = merge_db {
        // Merge mode: load existing .db, add LSP data alongside AST.
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

        let enrichment = project::enrich_symbols(&mut client, &conn, &symbols, &file_uri).await?;
        eprintln!("enrichment: {enrichment}");

        let data = conn.serialize(DatabaseName::Main)?;
        std::fs::write(output, &*data)
            .with_context(|| format!("write {}", output.display()))?;
    } else {
        // Standalone mode: create a fresh .db with LSP data.
        let conn = Connection::open_in_memory()?;
        project::project_lsp_into(&symbols, &diagnostics, &file_uri, &conn)?;

        let enrichment = project::enrich_symbols(&mut client, &conn, &symbols, &file_uri).await?;
        eprintln!("enrichment: {enrichment}");

        let data = conn.serialize(DatabaseName::Main)?;
        std::fs::write(output, &*data)
            .with_context(|| format!("write {}", output.display()))?;
    }

    // Graceful shutdown.
    client.shutdown().await?;

    eprintln!("wrote {}", output.display());
    Ok(())
}
