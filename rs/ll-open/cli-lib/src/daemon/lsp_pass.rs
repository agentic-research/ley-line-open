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

/// Derive the capnp binding-event-log path from a connection's
/// backing file (bead `ley-line-open-cdcae2`).
///
/// Returns `None` for `:memory:` connections (where dual-write would
/// have nowhere to land); the daemon's current shape hits this branch
/// until 5f7100-15a switches to a file-backed live db. File-backed
/// connections (the standalone `leyline lsp out.db` path, and any
/// future daemon WAL adoption) get `Some(${db_path}.bindings.capnp)`.
fn sibling_capnp_log(conn: &Connection) -> Option<std::path::PathBuf> {
    let row: rusqlite::Result<String> = conn.query_row(
        "SELECT file FROM pragma_database_list WHERE name = 'main' LIMIT 1",
        [],
        |r| r.get(0),
    );
    match row {
        Ok(s) if !s.is_empty() => {
            let mut p = std::path::PathBuf::from(s);
            p.set_extension("bindings.capnp");
            Some(p)
        }
        _ => None,
    }
}

/// Symbol-poll cadence for daemon-driven enrichment. Tighter than the
/// one-shot `cmd_lsp` path because the daemon reuses the same server
/// across many files in a batch — by the time a second file is opened
/// the server is usually already indexed.
const PASS_SYMBOL_POLL_MAX_ATTEMPTS: usize = 5;
const PASS_SYMBOL_POLL_DELAY: std::time::Duration = std::time::Duration::from_millis(200);

/// Per-language ready-timeout. Returns `Duration::ZERO` to skip the
/// readiness wait entirely (servers that don't emit indexing signals).
///
/// rust-analyzer needs the longest wait — cargo metadata + initial
/// cargo check on a cold workspace can take 10-30s. gopls is faster
/// (~2-5s for typical workspaces). Other servers we bundle finish
/// quickly enough that the syntactic-only path (documentSymbol)
/// returns useful data even before the readiness signal would arrive,
/// so we skip the wait and accept that hover/def/refs may return
/// empty on the first call (the lazy-enrich retry pattern in
/// `op_lsp_hover` re-runs the pass on the next request anyway).
///
/// Bead `ley-line-open-661727`: pre-fix the pass had no timeout at
/// all because it never waited — it just fired queries against an
/// un-indexed server and accepted the empty results. Per-language
/// numbers chosen empirically from rust-analyzer / gopls observed
/// cold-start latencies; bump per-language as needed when a specific
/// server's behaviour surfaces a tighter or looser fit.
fn ready_timeout_for_language(lang: &str) -> std::time::Duration {
    use std::time::Duration;
    match lang {
        // 60s is empirically generous — covers cold cargo workspaces
        // up to ~50 crates on a warm-disk Mac. Larger monorepos
        // routinely exceed even this; the pass falls back to "warn +
        // continue" on timeout (intermittent zero-hover on first
        // touch; subsequent calls hit the warm pooled client and
        // succeed). Bump per real-world pressure rather than guessing
        // higher — the active-probe loop below is what actually
        // verifies semantic readiness, not this number.
        "rust" => Duration::from_secs(60),
        "go" => Duration::from_secs(15),
        "typescript" | "typescriptreact" | "javascript" | "javascriptreact" => {
            Duration::from_secs(10)
        }
        "python" => Duration::from_secs(10),
        "c" | "cpp" => Duration::from_secs(15),
        "java" => Duration::from_secs(30),
        "zig" => Duration::from_secs(10),
        // No-server languages are filtered out earlier; this is the
        // fallback for any bundled language not enumerated above.
        _ => Duration::from_secs(10),
    }
}

/// Active-probe configuration for verifying semantic readiness AFTER
/// the passive `await_ready` notification path has reported success.
///
/// Bead `ley-line-open-661727` (and the post-v0.5.4 cold-start
/// caveat): rust-analyzer's `experimental/serverStatus quiescent:
/// true` is a self-report. The server is allowed to claim quiescence
/// while hover responses still return None (initial-analysis cycle
/// finished but the on-demand query cache hasn't been primed for the
/// requesting file yet). The cold-start race manifests as `25
/// symbols, 0 hovers/defs/refs` even after `await_ready` returned
/// true.
///
/// Active probe: issue a real `hover` request at a known-symbol
/// position from the documentSymbol response. If hover returns
/// `Some(content)` the server IS ready for semantic queries on this
/// file — no self-report needed. If hover returns `None`, wait
/// `PROBE_BACKOFF` and retry up to `PROBE_MAX_ATTEMPTS` times.
///
/// 5 attempts × 1s back-off = 5s extra wait worst case. Adds onto
/// `ready_timeout_for_language` only when the server claimed ready
/// but the probe still misses — i.e. only on cold-start, never on
/// warm-pool reuse.
const PROBE_MAX_ATTEMPTS: usize = 5;
const PROBE_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);

/// Issue a hover request at the first DocumentSymbol's selection
/// range to verify the server can actually answer semantic queries.
/// Returns `true` if hover returned content within `PROBE_MAX_ATTEMPTS`
/// retries, `false` after exhausting attempts.
///
/// Symbols MUST be non-empty — caller already early-returned on empty
/// in the enclosing per-file flow.
async fn verify_ready_via_probe(
    client: &mut leyline_lsp::client::LspClient,
    file_uri: &str,
    symbols: &[leyline_lsp::protocol::DocumentSymbol],
) -> bool {
    let first = &symbols[0];
    let line = first.selection_range.start.line;
    let character = first.selection_range.start.character;

    for attempt in 0..PROBE_MAX_ATTEMPTS {
        match client.hover(file_uri, line, character).await {
            Ok(Some(_)) => return true,
            // None or Err → server hasn't loaded this file's analysis
            // cache yet. Back off + retry. Err on the probe is not
            // fatal — the real per-symbol loop is what we're guarding,
            // and it has its own per-call error handling.
            _ if attempt + 1 < PROBE_MAX_ATTEMPTS => {
                tokio::time::sleep(PROBE_BACKOFF).await;
            }
            _ => return false,
        }
    }
    false
}

/// Per-symbol budget for the hover/def/refs loop AFTER readiness has
/// been verified. Bounds the misbehaving-server case where individual
/// LSP requests hang indefinitely. 30s lets a 50-symbol file finish at
/// ~0.6s/symbol — enough headroom for a slow but not pathological
/// rust-analyzer or gopls. Per-call hangs inside this window are the
/// language server's per-method timeout's job to catch.
const PER_SYMBOL_LOOP_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// Hard ceiling on total time spent enriching a single file (60f75d
/// + bead `ley-line-open-661727`).
///
/// Computed per-language as the sum of three budgets:
///   1. `ready_timeout_for_language(lang)` — passive readiness wait
///      (rust-analyzer 60s, gopls 15s, etc.)
///   2. `PROBE_MAX_ATTEMPTS * PROBE_BACKOFF` — active hover-probe
///      verification (5s)
///   3. `PER_SYMBOL_LOOP_BUDGET` — the actual hover/def/refs loop
///      after readiness is confirmed (30s)
///
/// Pre-v0.5.5 used a static 5s ceiling, which silently tripped before
/// rust-analyzer's 30s `await_ready` (added in v0.5.4) could even
/// finish — the per-file timeout always won, and rust-analyzer
/// hover/def/refs never had a chance. v0.5.5 makes the outer ceiling
/// dynamic so it accommodates the per-language readiness wait.
///
/// Worst-case batch: 50 files × 95s/file (rust) = ~80 min. Real
/// batches are MUCH shorter because the pooled LspClient stays warm
/// after the first file — only the cold-start file pays the full
/// budget; subsequent files in the same batch hit the warm `quiescent`
/// signal in <100ms.
fn pass_file_timeout_for_language(lang: &str) -> std::time::Duration {
    ready_timeout_for_language(lang)
        + PROBE_BACKOFF * PROBE_MAX_ATTEMPTS as u32
        + PER_SYMBOL_LOOP_BUDGET
}

/// LSP enrichment pass.
///
/// Spawns language servers for each language found in `_source`, collects
/// document symbols, merges into the living db's `_lsp*` tables. Enriches
/// each symbol with go-to-definition, hover, and references.
///
/// **60f75d-7b — server pool**: holds a `tokio::sync::Mutex<HashMap<String,
/// LspClient>>` keyed by language id. First call for a language spawns
/// the server (gopls/rust-analyzer/pyright/clangd/jdtls/zls); subsequent
/// calls reuse the cached client. The pre-7b implementation spawned a
/// fresh server per call and shut it down on completion — every lazy-
/// enrich on a previously-seen language paid the full cold-start cost
/// (gopls index, rust-analyzer build graph, etc.).
///
/// The pool is global per-pass (one HashMap protected by one async
/// Mutex) so concurrent same-language enrichments serialize through it.
/// LSP servers are mostly single-threaded over their stdin pipe anyway;
/// the serialization is rarely the bottleneck. Per-language sub-mutexes
/// (cross-language concurrency) is a future optimization.
///
/// Lifecycle: pool grows until daemon shutdown. No idle eviction in v1
/// — typical session uses 1-3 languages, ~hundreds of MB each, total
/// well under 1GB. The OS reclaims spawned servers on daemon exit.
pub struct LspEnrichmentPass {
    pool: std::sync::Arc<
        tokio::sync::Mutex<std::collections::HashMap<String, leyline_lsp::client::LspClient>>,
    >,
}

impl Default for LspEnrichmentPass {
    fn default() -> Self {
        Self::new()
    }
}

impl LspEnrichmentPass {
    /// Construct a new pass with an empty server pool. Servers are
    /// spawned lazily on first use per language.
    pub fn new() -> Self {
        Self {
            pool: std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }
}

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
        &[
            "_lsp",
            "_lsp_defs",
            "_lsp_refs",
            "_lsp_hover",
            "_lsp_completions",
        ]
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
            // Scope didn't match any rows in `_source`. The common cause
            // (per bead `ley-line-open-661727`) is a path-shape mismatch
            // between caller-supplied `changed_files` and what the
            // tree-sitter pass stored as `_source.id`. Surface this in
            // the response so callers don't see `items_added: 0` and
            // misread it as "no work to do."
            let mut skipped = Vec::new();
            if let Some(req) = changed_files {
                skipped.push(format!(
                    "scope matched no _source.id rows; requested {} file(s): {:?}",
                    req.len(),
                    req,
                ));
            }
            return Ok(EnrichmentStats {
                pass_name: "lsp".to_string(),
                files_processed: 0,
                items_added: 0,
                duration_ms: 0,
                skipped,
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
        // Per-language skip reasons surfaced in the response. Each entry
        // names the language, the file count it covers, and the reason.
        // Bead `ley-line-open-661727` — silent stderr-only skips meant
        // callers couldn't tell "no work needed" from "server missing"
        // from "no bundled server for this language."
        let mut skipped: Vec<String> = Vec::new();

        for (lang, lang_files) in &by_lang {
            let (server_cmd, server_args) = match language_server(lang) {
                Some(s) => s,
                None => {
                    let reason = format!(
                        "no bundled LSP server for language '{lang}' ({} file(s) skipped)",
                        lang_files.len()
                    );
                    eprintln!("lsp: {reason}");
                    skipped.push(reason);
                    continue;
                }
            };

            // Check if the server is available.
            if which::which(server_cmd).is_err() {
                let reason = format!(
                    "language server '{server_cmd}' not on PATH for language '{lang}' ({} file(s) skipped)",
                    lang_files.len()
                );
                eprintln!("lsp: {reason}");
                skipped.push(reason);
                continue;
            }

            let root_uri = format!(
                "file://{}",
                source_dir
                    .canonicalize()
                    .unwrap_or_else(|_| source_dir.clone())
                    .display()
            );

            // 60f75d-7b: reuse pooled client across enrichment batches.
            // First call for `lang` spawns the server; subsequent calls
            // skip the spawn cost entirely.
            let pool = self.pool.clone();
            let result = tokio::task::block_in_place(|| {
                handle.block_on(enrich_files_pooled(
                    conn,
                    pool,
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
                    let reason =
                        format!("language server '{server_cmd}' failed for '{lang}': {e:#}");
                    eprintln!("lsp: {reason}");
                    skipped.push(reason);
                }
            }
        }

        Ok(EnrichmentStats {
            pass_name: "lsp".to_string(),
            files_processed: files.len() as u64,
            items_added: total_symbols + total_enriched,
            duration_ms: start.elapsed().as_millis() as u64,
            skipped,
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
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(Into::into)
        }

        // Huge scope → full scan + in-memory filter. Rare; typical dirty
        // sets are 1-10 files. Above 999 we'd need to chunk the IN clause,
        // which buys nothing over a single full scan + HashSet at this size.
        Some(rels) => {
            let scope: std::collections::HashSet<&str> = rels.iter().map(String::as_str).collect();
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
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(Into::into)
        }
    }
}

/// 60f75d-7b: pool-aware wrapper. Locks the pool, finds-or-spawns
/// the client for `lang`, runs `enrich_files_with_client` against
/// the pooled handle, returns. The client is NOT shut down — it
/// stays in the pool for the next call.
///
/// Holds the pool lock across the entire enrichment batch (single
/// concurrent enrichment globally during this call). Per-language
/// sub-mutexes for cross-language concurrency is a future
/// optimization tracked under 60f75d.
#[allow(clippy::too_many_arguments)]
async fn enrich_files_pooled(
    conn: &Connection,
    pool: std::sync::Arc<
        tokio::sync::Mutex<std::collections::HashMap<String, leyline_lsp::client::LspClient>>,
    >,
    server_cmd: &str,
    server_args: &[&str],
    root_uri: &str,
    source_dir: &Path,
    lang: &str,
    files: &[(String, String)],
) -> Result<(u64, u64)> {
    let mut pool_guard = pool.lock().await;

    // Lookup-or-spawn for this language.
    let client = match pool_guard.get_mut(lang) {
        Some(c) => c,
        None => {
            let new_client =
                leyline_lsp::client::LspClient::start(server_cmd, server_args, root_uri)
                    .await
                    .with_context(|| format!("start {server_cmd} (pool insert)"))?;
            pool_guard.entry(lang.to_string()).or_insert(new_client)
        }
    };

    enrich_files_with_client(client, conn, source_dir, lang, files).await
}

/// Run a batch of files through an existing LspClient. Does NOT
/// spawn or shut down the client — caller manages lifecycle (the pool
/// keeps it alive across calls).
async fn enrich_files_with_client(
    client: &mut leyline_lsp::client::LspClient,
    conn: &Connection,
    source_dir: &Path,
    lang: &str,
    files: &[(String, String)],
) -> Result<(u64, u64)> {
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

        let file_uri = format!(
            "file://{}",
            abs_path
                .canonicalize()
                .unwrap_or(abs_path.clone())
                .display()
        );

        // Infer language ID from extension.
        let ext = abs_path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let language_id = language_id_from_ext(ext).unwrap_or(lang);

        // 60f75d: wrap the per-file work in a hard timeout so a
        // misbehaving language server can't stall the enrichment loop
        // forever. On timeout we log::warn, skip this file, and proceed
        // to the next one — the LSP client itself stays alive so a
        // single bad file doesn't poison the whole batch.
        let per_file = async {
            client
                .open_file(&file_uri, language_id, &source_text)
                .await?;

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

            // Bead `ley-line-open-661727` — readiness gate before
            // semantic queries. `documentSymbol` above is syntactic and
            // returns immediately; hover/definition/references need the
            // server's project model loaded (cargo metadata + cargo
            // check for rust-analyzer; module-graph for gopls; etc.).
            // Pre-fix the pass would race the indexer and write 25
            // skeleton _lsp rows with 0 hovers/defs/refs.
            //
            // `await_ready` polls for `experimental/serverStatus
            // quiescent: true` (rust-analyzer extension) or `$/progress`
            // end for an indexing token. Servers that don't emit either
            // hit the timeout — those calls were going to return empty
            // anyway; the timeout caps the wait at the per-language
            // cost. The handler skips the wait entirely when timeout = 0.
            let ready_timeout = ready_timeout_for_language(lang);
            if !ready_timeout.is_zero() {
                let was_ready = client.await_ready(ready_timeout).await;
                if !was_ready {
                    log::warn!(
                        "lsp: {lang} server didn't signal ready within {ready_timeout:?} \
                         for {rel}; issuing hover/defs/refs anyway (may return empty)"
                    );
                }

                // Active-probe verification — the server's self-report
                // can lie. rust-analyzer's `quiescent: true` means
                // "initial analysis cycle finished," not "hover at
                // arbitrary positions returns content." On cold-start
                // the on-demand query cache for THIS file isn't primed
                // yet even after quiescence; the symptom is `25
                // symbols, 0 hovers/defs/refs` despite await_ready
                // returning true.
                //
                // verify_ready_via_probe issues a real hover at the
                // first symbol's selection range and retries with
                // back-off until it returns Some(content) or the
                // probe budget exhausts. Adds 0-5s on cold-start,
                // costs nothing on the warm-pool path.
                if was_ready {
                    let probe_ok = verify_ready_via_probe(client, &file_uri, &symbols).await;
                    if !probe_ok {
                        log::warn!(
                            "lsp: {lang} server signalled ready for {rel} but probe \
                             hover returned None after {PROBE_MAX_ATTEMPTS} attempts; \
                             issuing the per-symbol loop anyway (cache may still warm \
                             during the loop)"
                        );
                    }
                }
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

            // capnp BindingRecord dual-write target (bead
            // `ley-line-open-cdcae2`) — sit next to
            // the live db file. `:memory:` connections (current daemon
            // shape) report empty; we skip the dual-write there. Lights
            // up automatically once 5f7100-15a switches the daemon to a
            // file-backed live db.
            let binding_log = sibling_capnp_log(conn);

            // Enrich with definitions, hover, references.
            let stats = leyline_lsp::project::enrich_symbols(
                client,
                conn,
                &symbols,
                &file_uri,
                binding_log.as_deref(),
            )
            .await?;

            eprintln!(
                "lsp: {rel} — {matched} symbols, {} defs, {} hovers, {} refs",
                stats.definitions, stats.hovers, stats.references
            );

            let enriched = (stats.definitions + stats.hovers + stats.references) as u64;
            Ok((matched as u64, enriched))
        };

        let per_file_timeout = pass_file_timeout_for_language(lang);
        match tokio::time::timeout(per_file_timeout, per_file).await {
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
                    per_file_timeout,
                );
            }
        }
    }

    // No shutdown — the pool keeps the client alive across calls (60f75d-7b).
    Ok((total_symbols, total_enriched))
}

// Tests for the language registry now live in `leyline-lsp::languages::tests`
// (single source of truth — see ley-line-open-5f7100-10).

#[cfg(test)]
mod tests {
    use super::*;

    /// 60f75d + bead `ley-line-open-661727`: the per-file timeout
    /// must accommodate readiness wait + active probe + per-symbol
    /// loop. Pre-v0.5.5 used a static 5s ceiling that silently tripped
    /// before rust-analyzer's 30s `await_ready` could finish — the
    /// outer timeout always won and rust-analyzer never got the chance
    /// to answer hover/def/refs.
    ///
    /// Pin the per-language composition for the three load-bearing
    /// languages (rust, go, python) so a refactor that drops the
    /// dynamic computation surfaces here.
    #[test]
    fn pass_file_timeout_exceeds_readiness_wait_per_language() {
        for lang in ["rust", "go", "python", "typescript", "java", "zig"] {
            let total = pass_file_timeout_for_language(lang);
            let ready = ready_timeout_for_language(lang);
            assert!(
                total > ready,
                "per-file timeout for {lang} ({total:?}) must exceed its readiness wait \
                 ({ready:?}) — otherwise the wrapping timeout trips before await_ready \
                 can finish, silently killing semantic queries"
            );
            let probe_budget = PROBE_BACKOFF * PROBE_MAX_ATTEMPTS as u32;
            assert!(
                total >= ready + probe_budget + PER_SYMBOL_LOOP_BUDGET,
                "per-file timeout for {lang} ({total:?}) must include readiness ({ready:?}) \
                 + probe budget ({probe_budget:?}) + per-symbol loop ({PER_SYMBOL_LOOP_BUDGET:?})"
            );
        }
    }

    /// Pin the rust-analyzer cold-start budget. If this value moves,
    /// it's a deliberate signal — update the comment on
    /// `ready_timeout_for_language` and re-verify against real cold
    /// cargo workspaces.
    #[test]
    fn rust_analyzer_ready_timeout_pinned() {
        assert_eq!(
            ready_timeout_for_language("rust"),
            std::time::Duration::from_secs(60),
            "rust-analyzer cold-start budget pinned at 60s; bump if real-world cold \
             cargo workspaces routinely exceed it"
        );
    }

    /// Pin the active-probe configuration. The probe is the substantive
    /// safety net against `quiescent: true` lying — its parameters
    /// shouldn't drift without a deliberate doc-update commit.
    #[test]
    fn probe_config_pinned() {
        assert_eq!(PROBE_MAX_ATTEMPTS, 5);
        assert_eq!(PROBE_BACKOFF, std::time::Duration::from_secs(1));
    }

    #[test]
    fn lsp_enrichment_pass_trait_metadata_pin() {
        // Third in the EnrichmentPass-metadata triplet. resolve_order
        // keys on name + depends_on; drift breaks dep resolution
        // silently. The 5 _lsp* tables in writes are the schema-
        // partition contract.
        crate::daemon::enrichment::assert_pass_metadata(
            &LspEnrichmentPass::new(),
            "lsp",
            &["tree-sitter"],
            &["_source", "_ast", "nodes"],
            &[
                "_lsp",
                "_lsp_defs",
                "_lsp_refs",
                "_lsp_hover",
                "_lsp_completions",
            ],
        );
    }

    /// 60f75d-7b: pool starts empty. First call for a language inserts;
    /// subsequent calls find the cached client. Pin the empty-on-new
    /// invariant — without it, a refactor that pre-populated the pool
    /// (e.g. for "warm-start") would silently change spawn semantics.
    #[tokio::test]
    async fn lsp_pass_pool_starts_empty() {
        let pass = LspEnrichmentPass::new();
        let pool = pass.pool.lock().await;
        assert_eq!(pool.len(), 0, "fresh pool must be empty");
    }

    /// 60f75d-7b: each `LspEnrichmentPass::new()` gets its own pool
    /// (fresh empty HashMap). Pin so a refactor that accidentally made
    /// the pool a `static`/global (which would break per-daemon
    /// isolation) surfaces here.
    #[tokio::test]
    async fn lsp_pass_pool_is_per_instance() {
        let p1 = LspEnrichmentPass::new();
        let p2 = LspEnrichmentPass::new();
        // Insert a sentinel into p1's pool by direct manipulation.
        // (Real spawn requires gopls-on-PATH; pinning structural
        // isolation doesn't need a real client.)
        // Compare Arc identities — they must be distinct.
        assert!(
            !std::sync::Arc::ptr_eq(&p1.pool, &p2.pool),
            "two pass instances must have independent pools",
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
        assert!(
            got.is_empty(),
            "empty scope must produce empty result, got {got:?}"
        );
    }
}
