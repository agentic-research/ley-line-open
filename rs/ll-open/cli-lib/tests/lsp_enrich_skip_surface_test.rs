//! Bead `ley-line-open-661727`: the LSP enrichment pass must surface
//! per-language skip reasons in `EnrichmentStats.skipped` so callers
//! can distinguish "no work needed" from "server not available"
//! from "no bundled server for this language."
//!
//! Pre-fix behavior: skip reasons only went to stderr via `eprintln!`;
//! the JSON-serialized response showed `items_added: 0` with no field
//! callers could parse to learn the cause. Mache (mache-303036)
//! couldn't tell whether `pass=lsp, files=["x.rs"]` had silently
//! skipped because rust-analyzer wasn't on PATH or because the scope
//! had matched nothing in `_source`.
//!
//! This test pins the new contract: when the LSP pass cannot run a
//! server for a language in the scope, the resulting `EnrichmentStats`
//! carries a human-readable skip reason naming the language + count.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use rusqlite::Connection;
use tempfile::TempDir;

use leyline_cli_lib::daemon::enrichment::{EnrichmentPass, EnrichmentStats};
use leyline_cli_lib::daemon::lsp_pass::LspEnrichmentPass;

/// Helper: build a `_source` row for a single Markdown file under a
/// scratch source dir. Markdown is registered in
/// `leyline_lsp::languages` with `server: None`, so any LSP enrich
/// call against it triggers the "no bundled server" skip path.
fn setup_markdown_source(source_dir: &std::path::Path) -> Result<(Connection, String)> {
    let md_path = source_dir.join("README.md");
    std::fs::write(&md_path, "# heading\n\nbody text.\n")?;

    let conn = Connection::open_in_memory()?;

    // Minimal _source schema — id is the relative path, language is
    // the LSP language ID. Real daemon parses populate this via
    // `leyline_ts::schema::create_schema`; we hand-roll the subset
    // needed for the LSP pass to discover the file.
    conn.execute_batch(
        "CREATE TABLE _source (id TEXT PRIMARY KEY, language TEXT NOT NULL, path TEXT);",
    )?;
    conn.execute(
        "INSERT INTO _source (id, language, path) VALUES (?1, 'markdown', ?2)",
        rusqlite::params!["README.md", md_path.to_string_lossy().to_string()],
    )?;

    Ok((conn, "README.md".to_string()))
}

#[tokio::test(flavor = "multi_thread")]
async fn lsp_enrich_no_server_surfaces_skip_reason() {
    let tmp = TempDir::new().unwrap();
    let (conn, rel) = setup_markdown_source(tmp.path()).unwrap();

    let pass = LspEnrichmentPass::new();
    let stats = pass
        .run(&conn, tmp.path(), Some(&[rel.clone()]))
        .expect("pass should succeed even when no server is bundled for the language");

    // The pass attempted one file, wrote nothing, and recorded WHY.
    assert_eq!(stats.pass_name, "lsp");
    assert_eq!(
        stats.files_processed, 1,
        "the markdown file IS in the scope, so files_processed counts it even though enrichment skipped"
    );
    assert_eq!(
        stats.items_added, 0,
        "no LSP server for markdown ⇒ zero items added"
    );
    assert!(
        !stats.skipped.is_empty(),
        "skip reason must be surfaced; pre-bead-661727 this was silent (eprintln only)",
    );
    let joined = stats.skipped.join(" | ");
    assert!(
        joined.contains("markdown"),
        "skip reason must name the affected language; got: {joined}"
    );
    assert!(
        joined.to_lowercase().contains("no bundled") || joined.to_lowercase().contains("no server"),
        "skip reason must explain WHY (no bundled server for the language); got: {joined}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn lsp_enrich_scope_mismatch_surfaces_skip_reason() {
    // Scope names a file that doesn't exist in `_source`. Pre-fix the
    // pass returned `EnrichmentStats { items_added: 0 }` with no signal
    // distinguishing this from "ran successfully, found nothing to do."
    let tmp = TempDir::new().unwrap();
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE _source (id TEXT PRIMARY KEY, language TEXT NOT NULL, path TEXT);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO _source (id, language, path) VALUES ('actual.rs', 'rust', '/tmp/actual.rs')",
        [],
    )
    .unwrap();

    let pass = LspEnrichmentPass::new();
    let scope = vec!["caller-asked-for-this-but-it-isnt-in-source.rs".to_string()];
    let stats = pass
        .run(&conn, tmp.path(), Some(&scope))
        .expect("pass should succeed cleanly on scope mismatch");

    assert_eq!(stats.files_processed, 0);
    assert_eq!(stats.items_added, 0);
    assert!(
        !stats.skipped.is_empty(),
        "scope-matched-nothing must surface a skip reason so callers can debug \
         path-shape mismatches between caller-supplied changed_files and _source.id"
    );
    let joined = stats.skipped.join(" | ");
    assert!(
        joined.contains("scope") || joined.contains("_source"),
        "skip reason must name what didn't match; got: {joined}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn lsp_enrich_response_serializes_skipped_field() {
    // Wire-format contract: skipped reasons must round-trip through the
    // EnrichmentStats serializer so mache (Go consumer) sees them in
    // the daemon's JSON response. `#[serde(skip_serializing_if = "Vec::is_empty")]`
    // keeps the field absent when empty; present when populated.

    let empty = EnrichmentStats {
        pass_name: "demo".to_string(),
        files_processed: 0,
        items_added: 0,
        duration_ms: 0,
        skipped: Vec::new(),
    };
    let json = serde_json::to_value(&empty).unwrap();
    assert!(
        json.get("skipped").is_none(),
        "empty skipped vec must be omitted from JSON (wire-format hygiene)"
    );

    let populated = EnrichmentStats {
        pass_name: "lsp".to_string(),
        files_processed: 1,
        items_added: 0,
        duration_ms: 5,
        skipped: vec![
            "no bundled LSP server for language 'markdown' (1 file(s) skipped)".to_string(),
        ],
    };
    let json = serde_json::to_value(&populated).unwrap();
    let arr = json
        .get("skipped")
        .and_then(|v| v.as_array())
        .expect("populated skipped must serialize as a JSON array");
    assert_eq!(arr.len(), 1);
    assert!(arr[0].as_str().unwrap().contains("markdown"));
}

// Silence unused-import warnings on the `Arc` / `PathBuf` shims that
// keep this file self-contained against future helper additions.
#[allow(dead_code)]
fn _unused_imports_anchor() {
    let _: Option<Arc<()>> = None;
    let _: Option<PathBuf> = None;
}
