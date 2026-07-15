//! Base op handlers for the daemon's UDS protocol.
//!
//! Each op queries the living in-memory SQLite database directly.
//! The arena is used only for periodic snapshots (crash recovery + mache).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use leyline_core::Controller;
use rusqlite::Connection;
use serde_json::json;

#[cfg(feature = "validate")]
use super::wire::ValidateRequest;
use super::wire::{
    AgreementRequest, BASE_OP_NAMES, BaseRequest, InspectNeighborhoodRequest, InspectSymbolRequest,
    LspFile, LspPosition, Ref as WireRef, SearchSymbolsRequest, TokenMapEntry,
};
#[cfg(feature = "hdc")]
use super::wire::{HdcCalibrateRequest, HdcDensityRequest, HdcSearchRequest};
use super::{DaemonContext, DaemonPhase};

// ---------------------------------------------------------------------------
// Public dispatch
// ---------------------------------------------------------------------------

/// Names of ops that mutate the daemon's state and should emit a
/// `daemon.<op>` event after completion. Lives next to the dispatch
/// table so a new mutating op gets a one-stop checklist (add a match
/// arm in `handle_base_op` AND list the name here).
///
/// Single source of truth — `daemon::socket` reads this via
/// `is_state_changing()` rather than maintaining a parallel list.
pub(crate) const STATE_CHANGING_OPS: &[&str] = &["load", "reparse", "flush", "snapshot", "enrich"];

/// Whether an op name belongs to `STATE_CHANGING_OPS`. Used by the UDS
/// dispatch loop to decide if the op deserves a follow-up event.
pub(crate) fn is_state_changing(op: &str) -> bool {
    STATE_CHANGING_OPS.contains(&op)
}

/// Test-only thin wrapper over [`BASE_OP_NAMES`]. Existing drift tests
/// iterate `base_op_names()` against `STATE_CHANGING_OPS` and
/// `mcp::tool_registry`; they continue to work unchanged after
/// b632ee's SoT collapse — they now read from `BASE_OP_NAMES` via this
/// alias.
///
/// If you add a new op, see the checklist on `BASE_OP_NAMES` in
/// `wire.rs` — this function does NOT need updating.
#[cfg(test)]
pub(crate) fn base_op_names() -> Vec<&'static str> {
    BASE_OP_NAMES.to_vec()
}

/// Try to parse the incoming wire line as a typed `BaseRequest` and
/// dispatch. Returns `Some(response)` if the op was recognized AND its
/// args deserialized cleanly; `None` if the wire shape doesn't match any
/// known variant (caller falls through to event / extension dispatch).
///
/// This is the load-bearing entry that `socket.rs` calls. See the
/// checklist on `BASE_OP_NAMES` in `wire.rs` for the steps to add a new
/// op. Drift tests + the compiler's exhaustive-match check on
/// `dispatch_typed` jointly enforce that all required edits land
/// together.
pub fn handle_base_op(ctx: &std::sync::Arc<DaemonContext>, wire_line: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(wire_line).ok()?;
    handle_base_op_value(ctx, parsed)
}

/// Value-accepting variant of `handle_base_op` for callers that have
/// already parsed the wire line into a `serde_json::Value` (notably
/// `socket.rs`, which extracts the `op` tag before this layer runs).
/// Avoids the `value.to_string()` + `serde_json::from_str` round-trip
/// flagged by Copilot on PR #8 — `serde_json::from_value` consumes the
/// already-parsed tree directly.
///
/// Two-stage decode so we can distinguish "unknown op" (return None —
/// caller falls through to extension dispatch) from "known op, bad
/// args" (return Some(error) — wire contract says the client gets a
/// structured error, not a silent miss).
pub fn handle_base_op_value(
    ctx: &std::sync::Arc<DaemonContext>,
    parsed: serde_json::Value,
) -> Option<String> {
    let op = parsed.get("op").and_then(|v| v.as_str())?.to_string();
    if !is_known_base_op(&op) {
        return None;
    }
    let typed_result: std::result::Result<BaseRequest, _> = serde_json::from_value(parsed);
    Some(match typed_result {
        Ok(typed) => dispatch_typed(ctx, typed),
        Err(e) => build_error_response(&format!("{op}: {e}"))
            .unwrap_or_else(|enc| fallback_error_envelope(&format!("dispatch error: {enc}"))),
    })
}

/// Last-resort error envelope when capnp-json encoding itself fails.
/// Serializes through serde_json so the error message gets correct
/// JSON-string escaping even if it contains quotes, backslashes, or
/// control characters. The double-failure path (capnp-json fails AND
/// serde_json fails) emits a hand-rolled string with a generic message;
/// at that point the daemon is in trouble but the wire stays valid.
fn fallback_error_envelope(message: &str) -> String {
    serde_json::to_string(&serde_json::json!({"ok": false, "error": message})).unwrap_or_else(
        |_| String::from(r#"{"ok":false,"error":"capnp-json + serde_json both failed"}"#),
    )
}

/// Whether `op` is one of the canonical base ops the daemon dispatches.
/// Derived from [`BASE_OP_NAMES`] (`wire.rs`) — the single source of
/// truth post-b632ee. Was previously a hand-maintained `matches!`
/// pattern that duplicated `base_op_names()`; the collapse removed the
/// duplication and a silent existing drift (`sheaf_reap` was in this
/// matcher but missing from `base_op_names()`).
fn is_known_base_op(op: &str) -> bool {
    BASE_OP_NAMES.contains(&op)
}

fn dispatch_typed(ctx: &std::sync::Arc<DaemonContext>, req: BaseRequest) -> String {
    let result: Result<String> = match req {
        BaseRequest::Status => op_status(ctx),
        BaseRequest::Flush => op_flush(&ctx.ctrl_path),
        BaseRequest::Load { db } => op_load(&ctx.ctrl_path, &db),
        BaseRequest::Query { sql, limit } => op_query(ctx, &sql, limit),
        BaseRequest::Reparse {
            source,
            lang,
            files,
        } => op_reparse(ctx, source.as_deref(), lang.as_deref(), files.as_deref()),
        BaseRequest::Snapshot => op_snapshot(ctx),
        BaseRequest::Enrich { pass, files } => op_enrich(ctx, &pass, files.as_deref()),
        BaseRequest::ListRoots => op_list_children(ctx, ""),
        BaseRequest::ListChildren { id } => op_list_children(ctx, id.as_deref().unwrap_or("")),
        BaseRequest::ReadContent { id } => op_read_content(ctx, &id),
        BaseRequest::FindCallers { token } => op_find_callers(ctx, &token),
        BaseRequest::FindCallees { id } => op_find_callees(ctx, &id),
        BaseRequest::FindDefs { token } => op_find_defs(ctx, &token),
        BaseRequest::GetNode { id } => op_get_node(ctx, &id),
        BaseRequest::GetRefsMap => op_get_token_map(ctx, "node_refs", TokenMapOp::Refs),
        BaseRequest::GetDefsMap => op_get_token_map(ctx, "node_defs", TokenMapOp::Defs),
        BaseRequest::GetSchema => op_get_schema(),
        BaseRequest::GetDbPath => op_get_db_path(&ctx.ctrl_path),
        BaseRequest::LspHover(p) => op_lsp_hover(ctx, &p),
        BaseRequest::LspDefs(p) => op_lsp_defs(ctx, &p),
        BaseRequest::LspRefs(p) => op_lsp_refs(ctx, &p),
        BaseRequest::LspSymbols(f) => op_lsp_symbols(ctx, &f),
        BaseRequest::LspDiagnostics(f) => op_lsp_diagnostics(ctx, &f),
        #[cfg(feature = "vec")]
        BaseRequest::VecSearch { query, k } => op_vec_search(ctx, &query, k),
        #[cfg(feature = "text-search")]
        BaseRequest::TextSearch { query, k } => op_text_search(ctx, &query, k),
        BaseRequest::SheafSetTopology {
            regions,
            restrictions,
            node_stalk_dim,
        } => super::sheaf_ops::op_sheaf_set_topology(
            &ctx.sheaf,
            &regions,
            &restrictions,
            node_stalk_dim,
        ),
        BaseRequest::SheafUpdateTopology {
            delta,
            node_stalk_dim,
        } => super::sheaf_ops::op_sheaf_update_topology(&ctx.sheaf, &delta, node_stalk_dim),
        BaseRequest::SheafInvalidate { regions, stalks } => {
            super::sheaf_ops::op_sheaf_invalidate(&ctx.sheaf, &ctx.ctrl_path, &regions, &stalks)
        }
        BaseRequest::SheafDefect => super::sheaf_ops::op_sheaf_defect(&ctx.sheaf),
        BaseRequest::SheafStalks => super::sheaf_ops::op_sheaf_stalks(&ctx.sheaf),
        BaseRequest::SheafStatus => super::sheaf_ops::op_sheaf_status(&ctx.sheaf),
        BaseRequest::SheafLearnedWeights => super::sheaf_ops::op_sheaf_learned_weights(&ctx.sheaf),
        BaseRequest::SheafReap => super::sheaf_ops::op_sheaf_reap(&ctx.sheaf),
        BaseRequest::LeylineVersion => op_leyline_version(),
        #[cfg(feature = "validate")]
        BaseRequest::Validate(v) => op_validate(&v),
        #[cfg(feature = "hdc")]
        BaseRequest::HdcSearch(r) => op_hdc_search(ctx, &r),
        #[cfg(feature = "hdc")]
        BaseRequest::HdcCalibrate(r) => op_hdc_calibrate(ctx, &r),
        #[cfg(feature = "hdc")]
        BaseRequest::HdcDensity(r) => op_hdc_density(ctx, &r),
        BaseRequest::InspectSymbol(r) => op_inspect_symbol(ctx, &r),
        BaseRequest::AtPosition(p) => op_at_position(ctx, &p),
        BaseRequest::InspectNeighborhood(r) => op_inspect_neighborhood(ctx, &r),
        BaseRequest::SearchSymbols(r) => op_search_symbols(ctx, &r),
        BaseRequest::Agreement(r) => op_agreement(ctx, &r),
    };
    result.unwrap_or_else(|e| {
        build_error_response(&format!("{e:#}"))
            .unwrap_or_else(|enc| fallback_error_envelope(&format!("handler error: {enc}")))
    })
}

/// Legacy compat wrapper for tests that constructed (op, args:Value) pairs
/// directly. Combines them into the canonical wire shape `{"op": ..., ...args}`
/// and dispatches via `handle_base_op`. New code paths (and `socket.rs`)
/// should call `handle_base_op` with the raw wire line directly.
#[cfg(test)]
fn handle_base_op_legacy(
    ctx: &std::sync::Arc<DaemonContext>,
    op: &str,
    req: &serde_json::Value,
) -> Option<String> {
    let mut combined = req.clone();
    if let serde_json::Value::Object(ref mut m) = combined {
        m.insert("op".into(), json!(op));
    } else {
        // Non-object request (e.g. null) — wrap into a synthetic object so
        // the typed parse sees a valid envelope.
        combined = json!({"op": op});
    }
    let wire = combined.to_string();
    handle_base_op(ctx, &wire)
}

// ---------------------------------------------------------------------------
// Living db access
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Small shared helpers — keep these one-liner-trivial so callers stay readable.
// ---------------------------------------------------------------------------

/// SQL fragment for "the row's `node_id` belongs to file ?1". Use as the WHERE
/// clause of any per-file `_lsp` query. Bind the file path as the first param.
///
/// Convention: node ids look like `"<file>/<ast-path>"`, so the LIKE prefix
/// scopes a query to all nodes in a single file.
const NODE_ID_FOR_FILE: &str = "node_id LIKE ?1 || '%'";

/// Helper for `_lsp_defs` / `_lsp_refs` queries. Both share the
/// `(uri, start_line, start_col, end_line, end_col)` shape with a
/// table-specific column prefix (`def_` / `ref_`). Returns an empty
/// vec if the table doesn't exist yet — that's the "not enriched"
/// signal callers use to trigger lazy enrichment.
/// Whether a table exists in the connection. Used by every LSP-rows
/// helper so a query against a not-yet-enriched table returns an
/// empty Vec (the "needs enrichment" signal callers act on) instead
/// of bubbling up a `no such table` SQL error.
fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |r| r.get(0),
    )
    .unwrap_or(false)
}

fn lsp_5col_position_rows(
    conn: &Connection,
    node_id: &str,
    table: &str,
    col_prefix: &str,
) -> Result<Vec<serde_json::Value>> {
    if !table_exists(conn, table) {
        return Ok(vec![]);
    }
    let sql = format!(
        "SELECT {col_prefix}_uri, {col_prefix}_start_line, {col_prefix}_start_col, \
                {col_prefix}_end_line, {col_prefix}_end_col \
         FROM {table} WHERE node_id = ?1"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt
        .query_map([node_id], |row| {
            Ok(json!({
                "uri":        row.get::<_, String>(0)?,
                "start_line": row.get::<_, i32>(1)?,
                "start_col":  row.get::<_, i32>(2)?,
                "end_line":   row.get::<_, i32>(3)?,
                "end_col":    row.get::<_, i32>(4)?,
            }))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Strip a leading `file://` from an LSP-style URI. Returns the input
/// unchanged if no prefix is present. Centralized so the rule for what
/// counts as "the file path" stays in one spot.
#[inline]
fn normalize_file_uri(s: &str) -> &str {
    s.strip_prefix("file://").unwrap_or(s)
}

/// Promote one or more node ids to the embed queue (no-op without `vec`).
///
/// Called from query ops so the touched nodes' embeddings get refreshed soon
/// by the background drainer.
#[inline]
#[allow(unused_variables)]
fn promote_touched(ctx: &DaemonContext, ids: &[&str]) {
    #[cfg(feature = "vec")]
    {
        for id in ids {
            crate::daemon::embed::promote(&ctx.embed_queue, id);
        }
    }
}

// `with_live_db` (removed 2026-07-08, bead `ley-line-open-ba8294` Phase 2)
// was replaced by `DaemonContext::with_read` / `with_write` methods on
// the context itself. See `daemon/mod.rs` for the new API. Historical
// note: the free function pattern couldn't distinguish read vs write
// intent, which blocked the WAL bead 98fb67 sub-bead 15b's read-pool
// migration. The methods carry the same "hold the mutex for the
// closure" semantics today; the intent-marking becomes structural once
// reads move to a checkout-from-pool path.

/// T2.4: read the substrate's `current_root` and return as a 64-char
/// lowercase hex string for the wire format. Centralizes the
/// "open ctrl + read root" pair that every state-changing op includes
/// in its JSON response. The `"open controller"` context string is
/// part of the wire-error contract — clients see this when the
/// controller path is broken. Wire-format key is `"current_root":
/// "<hex>"` (paired with mache `mache-36d961` epic).
///
/// `pub(crate)` since sheaf gap 3 (bead `ley-line-open-3b3476`): the
/// watcher-driven `daemon.sheaf.invalidate` emit in `cmd_daemon.rs`
/// needs the current root as a cache-invalidation key on its payload.
/// Every other caller stays inside `ops.rs`.
pub(crate) fn read_root_hex(ctrl_path: &Path) -> Result<String> {
    let ctrl = Controller::open_or_create(ctrl_path).context("open controller")?;
    let root = ctrl.current_root();
    let mut s = String::with_capacity(64);
    use std::fmt::Write;
    for b in &root {
        let _ = write!(s, "{b:02x}");
    }
    Ok(s)
}

/// Acquire the living-db lock and snapshot to the arena. Used by every
/// state-changing op (reparse, enrich, snapshot) to publish the latest
/// db image to mache/remote consumers.
///
/// **Lock window:** the write lock is held for the *full* duration of
/// `snapshot_to_arena`, which serializes the entire SQLite database to
/// the on-disk arena (a disk write proportional to db size). This is
/// not a cheap window. Concurrent readers and writers block until the
/// snapshot completes. Don't add work inside this function expecting
/// the lock to be held briefly — it isn't. If we ever want concurrent
/// reads during snapshot, the path is `serialize_with_flags(NO_COPY)`
/// followed by an out-of-lock disk write, but that's a deliberate
/// refactor, not something to assume here. Doc rewritten after iter-35
/// adversarial review caught the previous false minimal-lock claim.
fn snapshot_living_db(ctx: &DaemonContext) -> Result<()> {
    // Snapshot classified as `with_write`: `conn.serialize("main")` needs
    // exclusive access to the DB (SQLite serialize takes an internal
    // schema-write lock). Bead `ley-line-open-ba8294` Phase 2 intent-
    // marking; behavior identical under today's Mutex.
    ctx.with_write(|conn| crate::cmd_daemon::snapshot_to_arena(conn, &ctx.ctrl_path))
}

// ---------------------------------------------------------------------------
// Control ops (don't need the living db)
// ---------------------------------------------------------------------------

fn op_status(ctx: &DaemonContext) -> Result<String> {
    let ctrl = Controller::open_or_create(&ctx.ctrl_path).context("open controller")?;
    let state = ctx.state.read();

    // Collect per-pass status into a Vec the typed-list builder can
    // iterate (sorted by name so the wire is deterministic across
    // HashMap iteration order). Each entry mirrors a `PassStatus`
    // capnp struct.
    let mut entries: Vec<(&String, &super::PassStatus)> = state.enrichment.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));

    // T2.4 wire format: BLAKE3 of arena bytes as 64-char hex.
    let current_root_hex = {
        let root = ctrl.current_root();
        let mut s = String::with_capacity(64);
        use std::fmt::Write;
        for b in &root {
            let _ = write!(s, "{b:02x}");
        }
        s
    };

    let mut builder = capnp::message::Builder::new_default();
    let mut root: leyline_public_schema::daemon_capnp::status_response::Builder =
        builder.init_root();
    root.set_ok(true);
    root.set_phase(state.phase.as_str());
    root.set_current_root(&current_root_hex);
    root.set_arena_path(ctrl.arena_path());
    root.set_arena_size(ctrl.arena_size());
    // Legacy `enrichment @6 :Text` field deliberately left unset —
    // capnp-json omits unset Text on the wire, so the field disappears
    // from the JSON output entirely. The typed shape rides in
    // `enrichmentTyped @10 :List(EnrichmentEntry)` below. Ordinal
    // preserved per ADR-0014 §2 in case any pinned consumer reads it
    // (they'll get the empty default and should migrate to the typed
    // field).
    if let Some(sha) = &state.head_sha {
        root.set_head_sha(sha);
    }
    if let Some(t) = state.last_reparse_at_ms {
        root.set_last_reparse_at_ms(t);
    }
    if let DaemonPhase::Error(msg) = &state.phase {
        root.set_error(msg);
    }
    // Typed enrichment — typed end-to-end, no double parse on
    // consumers. Each entry is `(name, PassStatus { last_run_at_ms,
    // basis, error })`. Unset Text (error) is omitted on the wire;
    // Int64 fields (last_run_at_ms, basis) emit "0" as the
    // not-yet-set sentinel (capnp-json has no skip-if-default
    // annotation).
    let mut enrichment_b = root.init_enrichment_typed(entries.len() as u32);
    for (i, (name, status)) in entries.iter().enumerate() {
        let mut entry = enrichment_b.reborrow().get(i as u32);
        entry.set_name(name);
        let mut s = entry.init_status();
        if let Some(t) = status.last_run_at_ms {
            s.set_last_run_at_ms(t);
        }
        if let Some(b) = status.basis {
            s.set_basis(b);
        }
        if let Some(e) = &status.error {
            s.set_error(e);
        }
    }
    let reader = builder
        .get_root_as_reader::<leyline_public_schema::daemon_capnp::status_response::Reader>()?;
    Ok(capnp_json::to_json(reader)?)
}

fn op_flush(ctrl_path: &Path) -> Result<String> {
    let current_root = read_root_hex(ctrl_path)?;
    let mut builder = capnp::message::Builder::new_default();
    let mut root: leyline_public_schema::daemon_capnp::flush_response::Builder =
        builder.init_root();
    root.set_ok(true);
    root.set_current_root(&current_root);
    let reader = builder
        .get_root_as_reader::<leyline_public_schema::daemon_capnp::flush_response::Reader>()?;
    Ok(capnp_json::to_json(reader)?)
}

fn op_load(ctrl_path: &Path, db_b64: &str) -> Result<String> {
    use base64::Engine;
    let db_bytes = base64::engine::general_purpose::STANDARD
        .decode(db_b64)
        .context("invalid base64 in \"db\" field")?;
    crate::cmd_load::load_into_arena(ctrl_path, &db_bytes)?;
    let current_root = read_root_hex(ctrl_path)?;
    let mut builder = capnp::message::Builder::new_default();
    let mut root: leyline_public_schema::daemon_capnp::load_response::Builder = builder.init_root();
    root.set_ok(true);
    root.set_current_root(&current_root);
    let reader = builder
        .get_root_as_reader::<leyline_public_schema::daemon_capnp::load_response::Reader>()?;
    Ok(capnp_json::to_json(reader)?)
}

// ---------------------------------------------------------------------------
// Reparse + snapshot ops
// ---------------------------------------------------------------------------

fn op_reparse(
    ctx: &DaemonContext,
    source: Option<&str>,
    lang: Option<&str>,
    files: Option<&[String]>,
) -> Result<String> {
    // Inputs:
    //   `source` — directory or single file. If omitted, falls back to ctx.source_dir.
    //   `files`  — optional explicit scope (relative paths under source). When set,
    //              only those files are parsed; unscoped files are untouched.
    //   `lang`   — optional language filter.
    //
    // For Claude Code's PostToolUse hook the natural shape is
    // `{source: "<file>"}`. We accept that and auto-rewrite to
    // `(parent, scope=[basename])` so existing hook callers don't need to
    // know about the directory invariant.
    let source_arg = source
        .map(|s| s.to_string())
        .or_else(|| {
            ctx.source_dir
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
        })
        .context("missing \"source\" field and no --source configured")?;
    let lang = lang.or(ctx.lang_filter.as_deref());

    // Explicit `files: [...]` always takes precedence as the scope.
    let mut explicit_files: Option<Vec<String>> = files.map(|s| s.to_vec());

    // If the caller passed a single-file `source`, reinterpret as parent +
    // scope so we satisfy parse_into_conn's directory invariant. This lets
    // hooks blindly forward `tool_input.file_path` without knowing the
    // project root.
    let source_path = Path::new(&source_arg);
    let (source_dir, derived_scope): (PathBuf, Option<Vec<String>>) = if source_path.is_dir() {
        (source_path.to_path_buf(), None)
    } else if source_path.is_file() {
        // Fall back to ctx.source_dir as the project root if available
        // (lets the relative path stay short); otherwise use the file's
        // own parent directory.
        let project_root = ctx
            .source_dir
            .as_ref()
            .filter(|root| source_path.starts_with(root))
            .cloned();
        match project_root {
            Some(root) => {
                let rel = source_path
                    .strip_prefix(&root)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| {
                        source_path
                            .file_name()
                            .map(|f| f.to_string_lossy().to_string())
                            .unwrap_or_default()
                    });
                (root, Some(vec![rel]))
            }
            None => {
                let parent = source_path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."));
                let basename = source_path
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_default();
                (parent, Some(vec![basename]))
            }
        }
    } else {
        // Path doesn't exist — bubble up the error from parse_into_conn
        // so the caller sees a helpful message.
        (source_path.to_path_buf(), None)
    };

    if explicit_files.is_none() {
        explicit_files = derived_scope;
    }
    let scope: Option<&[String]> = explicit_files.as_deref();

    // Parse directly into the living db. Classified as `with_write` —
    // `parse_into_conn` runs INSERT/UPDATE/COMMIT under the guard.
    // Bead `ley-line-open-ba8294` Phase 2 intent-marking.
    ctx.state.write().phase = DaemonPhase::Parsing;
    let result = match ctx
        .with_write(|conn| crate::cmd_parse::parse_into_conn(conn, &source_dir, lang, scope))
    {
        Ok(r) => r,
        Err(e) => {
            ctx.state.write().phase = DaemonPhase::Error(format!("reparse failed: {e:#}"));
            return Err(e);
        }
    };

    // Snapshot to arena for mache/remote consumers.
    snapshot_living_db(ctx)?;

    {
        let mut s = ctx.state.write();
        s.phase = DaemonPhase::Ready;
        s.last_reparse_at_ms = Some(super::now_ms());
    }

    let current_root = read_root_hex(&ctx.ctrl_path)?;
    let mut builder = capnp::message::Builder::new_default();
    let mut root: leyline_public_schema::daemon_capnp::reparse_response::Builder =
        builder.init_root();
    root.set_ok(true);
    root.set_current_root(&current_root);
    root.set_parsed(result.parsed as u64);
    root.set_unchanged(result.unchanged as u64);
    root.set_deleted(result.deleted as u64);
    root.set_errors(result.errors as u64);
    let mut changed = root.init_changed_files(result.changed_files.len() as u32);
    for (i, f) in result.changed_files.iter().enumerate() {
        changed.set(i as u32, f);
    }
    let reader = builder
        .get_root_as_reader::<leyline_public_schema::daemon_capnp::reparse_response::Reader>()?;
    Ok(capnp_json::to_json(reader)?)
}

fn op_enrich(ctx: &DaemonContext, pass_name: &str, files: Option<&[String]>) -> Result<String> {
    let files: Option<Vec<String>> = files.map(|s| s.to_vec());

    let source_dir = ctx
        .source_dir
        .as_deref()
        .context("no --source configured; cannot run enrichment")?;

    // Enrichment classified as `with_write` — passes INSERT/UPDATE
    // enrichment rows under the guard (LSP bindings, HDC codebooks,
    // embeddings, etc.). Bead `ley-line-open-ba8294` Phase 2.
    ctx.state.write().phase = DaemonPhase::Enriching;
    let stats = ctx.with_write(|conn| {
        crate::daemon::enrichment::run_pass(
            &ctx.enrichment_passes,
            pass_name,
            conn,
            source_dir,
            files.as_deref(),
            Some(&ctx.state),
        )
    })?;
    ctx.state.write().phase = DaemonPhase::Ready;

    // Snapshot to arena after enrichment.
    snapshot_living_db(ctx)?;

    let current_root = read_root_hex(&ctx.ctrl_path)?;
    let mut builder = capnp::message::Builder::new_default();
    let mut root: leyline_public_schema::daemon_capnp::enrich_response::Builder =
        builder.init_root();
    root.set_ok(true);
    root.set_current_root(&current_root);
    let mut passes_b = root.init_passes(stats.len() as u32);
    for (i, s) in stats.iter().enumerate() {
        let mut entry = passes_b.reborrow().get(i as u32);
        entry.set_pass_name(&s.pass_name);
        entry.set_files_processed(s.files_processed);
        entry.set_items_added(s.items_added);
        entry.set_duration_ms(s.duration_ms);
        // Per-pass skip reasons (bead `ley-line-open-661727`). Without
        // this wiring the field was populated on the Rust struct but
        // dropped at the capnp boundary — consumers reading the daemon
        // response (mache et al.) never saw the reasons. JSON-only test
        // round-trip passed; production wire shape was incomplete.
        let mut skipped_b = entry.reborrow().init_skipped(s.skipped.len() as u32);
        for (j, reason) in s.skipped.iter().enumerate() {
            skipped_b.set(j as u32, reason.as_str());
        }
    }
    let reader = builder
        .get_root_as_reader::<leyline_public_schema::daemon_capnp::enrich_response::Reader>()?;
    Ok(capnp_json::to_json(reader)?)
}

fn op_snapshot(ctx: &DaemonContext) -> Result<String> {
    snapshot_living_db(ctx)?;
    let current_root = read_root_hex(&ctx.ctrl_path)?;
    let mut builder = capnp::message::Builder::new_default();
    let mut root: leyline_public_schema::daemon_capnp::snapshot_response::Builder =
        builder.init_root();
    root.set_ok(true);
    root.set_current_root(&current_root);
    let reader = builder
        .get_root_as_reader::<leyline_public_schema::daemon_capnp::snapshot_response::Reader>()?;
    Ok(capnp_json::to_json(reader)?)
}

// ---------------------------------------------------------------------------
// Query ops (use living db directly)
// ---------------------------------------------------------------------------

/// Default row cap for the ad-hoc `op_query` escape hatch. Without a
/// cap, an accidental `SELECT * FROM nodes` on a registry-repo db
/// (629k+ rows) would load every row into memory and serialize a
/// hundreds-of-MB JSON response, locking the daemon and the client.
/// Callers that legitimately need more rows pass `limit` explicitly
/// (with the understanding that they're opting into the cost).
pub(crate) const OP_QUERY_DEFAULT_ROW_LIMIT: usize = 1000;

/// ADR-0026 Phase 2.0 — read-side wall-time profiling scope
/// (bead `ley-line-open-335d34`).
///
/// Mirrors the `LEYLINE_PROFILE=1` sub-phase timing pattern used by
/// `cmd_parse` for the insert path. When the env var is set, emits
/// `[profile] op_query/<query_type>: <n>µs` to stderr on drop. When
/// unset, this is a zero-cost `Instant::now` per op (the drop path
/// short-circuits before formatting).
///
/// Instrumented at the op-dispatch layer (`op_query`, `op_list_children`,
/// `op_find_callers`, `op_find_defs`) so the numbers cover the whole
/// read path — SQL prepare, rows iter, capnp encode, capnp_json serialize.
/// That's the wall-time the F2 gate (§9.2.4) has to beat by ≥2× once
/// the pointer-store consumer lands in Phase 2.1.
///
/// Phase 2.0 measurement infrastructure only — no consumer migration, no
/// wire-side event emission, no behavior change when the env var is unset.
struct ReadProfileTimer {
    query_type: &'static str,
    start: std::time::Instant,
    enabled: bool,
}

impl ReadProfileTimer {
    fn new(query_type: &'static str) -> Self {
        // Read the env var once per op — the same shape `cmd_parse`
        // uses. Cheap enough (a hash-map lookup on macOS/Linux) that
        // per-op reads don't measurably move the read wall-time we're
        // trying to characterize.
        let enabled = std::env::var("LEYLINE_PROFILE").ok().as_deref() == Some("1");
        Self {
            query_type,
            start: std::time::Instant::now(),
            enabled,
        }
    }
}

impl Drop for ReadProfileTimer {
    fn drop(&mut self) {
        if self.enabled {
            let us = self.start.elapsed().as_micros();
            eprintln!("[profile] op_query/{}: {us}µs", self.query_type);
        }
    }
}

/// Raw SQL query — for ad-hoc inspection.
///
/// Caps row output at `req["limit"]` (or `OP_QUERY_DEFAULT_ROW_LIMIT` if
/// not provided). Sets `truncated: true` in the response when the cap is
/// hit, so callers can paginate via `LIMIT/OFFSET` in the SQL itself if
/// they need everything.
fn op_query(ctx: &DaemonContext, sql: &str, limit: Option<usize>) -> Result<String> {
    let limit = limit.unwrap_or(OP_QUERY_DEFAULT_ROW_LIMIT);
    // ADR-0026 Phase 2.0 (bead `ley-line-open-335d34`): profile the
    // whole op path when `LEYLINE_PROFILE=1`. Zero-cost when unset.
    // Drops at function return so the timer covers SQL prepare +
    // rows iter + JSON serialize.
    let _prof = ReadProfileTimer::new("query");

    // Bead `ley-line-open-f0239d`: `op_query` accepts arbitrary SQL and
    // can't statically distinguish reads from writes (`DROP TABLE` looks
    // like a query to the wire). Use the writer connection so every
    // shape of SQL still works — the pre-15b `with_read` classification
    // was a misclass hidden by the shared `Mutex<Connection>`. The
    // destructive-SQL foot-gun documented at
    // `test_op_query_destructive_runs_today_pin_for_6213d4` continues
    // to work (that's the whole point of the pin — 6213d4 will lock
    // this down as an *intentional* change).
    ctx.with_write(|conn| {
        let mut stmt = conn.prepare(sql).context("prepare SQL")?;
        let col_count = stmt.column_count();
        let headers: Vec<String> = (0..col_count)
            .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
            .collect();

        let mut rows_out: Vec<serde_json::Value> = Vec::new();
        let mut rows = stmt.query([]).context("execute SQL")?;
        let mut truncated = false;
        while let Some(row) = rows.next()? {
            if rows_out.len() >= limit {
                // Stop iterating: SQLite has more rows but the caller
                // capped the response. Mark as truncated so clients can
                // paginate with an explicit LIMIT/OFFSET in their SQL.
                truncated = true;
                break;
            }
            let mut obj = serde_json::Map::new();
            for (i, col) in headers.iter().enumerate() {
                let val: String = row.get::<_, String>(i).unwrap_or_default();
                obj.insert(col.clone(), serde_json::Value::String(val));
            }
            rows_out.push(serde_json::Value::Object(obj));
        }

        let mut response = serde_json::Map::new();
        response.insert("ok".into(), json!(true));
        response.insert("columns".into(), json!(headers));
        response.insert("rows".into(), json!(rows_out));
        if truncated {
            response.insert("truncated".into(), json!(true));
            response.insert("limit".into(), json!(limit));
        }
        Ok(serde_json::Value::Object(response).to_string())
    })
}

/// List children of a node (or roots if id="").
///
/// Deliberately omits the `record` column from the per-child Node
/// payload — `nodes.record` can hold full file contents or large JSON
/// blobs, and shipping that on every directory listing balloons the
/// response (raised by Copilot on PR #8). Consumers that want a
/// specific node's record call `op_get_node` or `op_read_content`.
/// The wire still emits the typed Node shape; `record` is `Option<String>`
/// with `skip_serializing_if`, so listings simply drop the key.
fn op_list_children(ctx: &DaemonContext, id: &str) -> Result<String> {
    // ADR-0026 Phase 2.0 (bead `ley-line-open-335d34`) — read-path
    // profile timer. This op backs mache's "get_overview" surface
    // (`list_roots` when id="", `list_children` otherwise); wrapping
    // it captures the whole row-projected read time that Phase 2.x
    // must beat by ≥2× per §9.2.4.
    let _prof = ReadProfileTimer::new("list_children");
    let response = ctx.with_read(|conn| {
        let mut stmt = conn.prepare_cached(
            "SELECT id, parent_id, name, kind, size \
             FROM nodes WHERE parent_id = ?1 ORDER BY name",
        )?;
        let raw: Vec<(String, String, String, i32, i64)> = stmt
            .query_map([id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i32>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })?
            .collect::<Result<_, _>>()?;
        let touched: Vec<&str> = raw.iter().map(|(id, ..)| id.as_str()).collect();
        let mut builder = capnp::message::Builder::new_default();
        let mut root: leyline_public_schema::daemon_capnp::list_children_response::Builder =
            builder.init_root();
        root.set_ok(true);
        let mut children_b = root.init_children(raw.len() as u32);
        for (i, (id, parent_id, name, kind, size)) in raw.iter().enumerate() {
            let mut node = children_b.reborrow().get(i as u32);
            node.set_id(id);
            node.set_parent_id(parent_id);
            node.set_name(name);
            node.set_kind(*kind);
            node.set_size(*size);
            // `record` deliberately omitted on directory listings — keeps
            // listings small. Consumers call get_node / read_content per
            // file when they actually need the record blob.
        }
        let reader = builder.get_root_as_reader::<
            leyline_public_schema::daemon_capnp::list_children_response::Reader,
        >()?;
        Ok((
            capnp_json::to_json(reader)?,
            touched.into_iter().map(String::from).collect::<Vec<_>>(),
        ))
    })?;
    let touched_refs: Vec<&str> = response.1.iter().map(String::as_str).collect();
    promote_touched(ctx, &touched_refs);
    Ok(response.0)
}

/// Read a node's content (the `record` column). Returns
/// `node_not_found_response` for both "no such node" and "node exists but
/// record is NULL" — the helper handles that distinction so all node-by-
/// id lookups share a single semantics.
fn op_read_content(ctx: &DaemonContext, id: &str) -> Result<String> {
    promote_touched(ctx, &[id]);

    ctx.with_read(|conn| match query_node_record(conn, id)? {
        Some(c) => {
            let mut builder = capnp::message::Builder::new_default();
            let mut root: leyline_public_schema::daemon_capnp::read_content_response::Builder =
                builder.init_root();
            root.set_ok(true);
            root.set_content(&c);
            let reader = builder.get_root_as_reader::<
                leyline_public_schema::daemon_capnp::read_content_response::Reader,
            >()?;
            Ok(capnp_json::to_json(reader)?)
        }
        None => Ok(node_not_found_response(id)),
    })
}

/// Find callers of a token (queries node_refs).
fn op_find_callers(ctx: &DaemonContext, token: &str) -> Result<String> {
    // ADR-0026 Phase 2.0 (bead `ley-line-open-335d34`) — mache-side
    // "find_callers" analog. Profile timer emits the row-projected
    // baseline the Phase 2 pointer-store gate has to beat.
    let _prof = ReadProfileTimer::new("find_callers");
    ctx.with_read(|conn| {
        let rows = query_token_refs(conn, token, "node_refs")?;
        let mut builder = capnp::message::Builder::new_default();
        let mut root: leyline_public_schema::daemon_capnp::find_callers_response::Builder =
            builder.init_root();
        root.set_ok(true);
        set_ref_list(root.init_callers(rows.len() as u32), &rows);
        let reader = builder.get_root_as_reader::<
            leyline_public_schema::daemon_capnp::find_callers_response::Reader,
        >()?;
        Ok(capnp_json::to_json(reader)?)
    })
}

/// Find definitions of a token (queries node_defs).
fn op_find_defs(ctx: &DaemonContext, token: &str) -> Result<String> {
    // ADR-0026 Phase 2.0 (bead `ley-line-open-335d34`) — mache-side
    // "find_definition" analog. Same profile-timer discipline as
    // find_callers / list_children / query.
    let _prof = ReadProfileTimer::new("find_defs");
    ctx.with_read(|conn| {
        let rows = query_token_refs(conn, token, "node_defs")?;
        let mut builder = capnp::message::Builder::new_default();
        let mut root: leyline_public_schema::daemon_capnp::find_defs_response::Builder =
            builder.init_root();
        root.set_ok(true);
        set_ref_list(root.init_defs(rows.len() as u32), &rows);
        let reader = builder
            .get_root_as_reader::<leyline_public_schema::daemon_capnp::find_defs_response::Reader>(
            )?;
        Ok(capnp_json::to_json(reader)?)
    })
}

/// Populate a capnp `List(Ref)` from a slice of WireRef rows. Used by
/// find_callers / find_defs / find_callees handlers.
fn set_ref_list(
    mut list: capnp::struct_list::Builder<'_, leyline_public_schema::daemon_capnp::ref_::Owned>,
    rows: &[WireRef],
) {
    for (i, r) in rows.iter().enumerate() {
        let mut entry = list.reborrow().get(i as u32);
        entry.set_node_id(&r.node_id);
        entry.set_source_id(&r.source_id);
    }
}

/// Shared row-fetch helper for `find_callers` / `find_defs`. Returns
/// typed `WireRef` rows so each caller can wrap them in its op-specific
/// typed response. Same SQL shape as the legacy `query_token_in_table`
/// helper, just returns `Vec<WireRef>` directly.
fn query_token_refs(conn: &Connection, token: &str, table: &str) -> Result<Vec<WireRef>> {
    let sql = format!("SELECT node_id, source_id FROM {table} WHERE token = ?1");
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt
        .query_map([token], |row| {
            Ok(WireRef {
                node_id: row.get::<_, String>(0)?,
                source_id: row.get::<_, String>(1)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Find callees of a node — the definitions of every token the node references.
///
/// Forward-direction sibling of `find_callers`/`find_defs`:
/// `find_callers(token)` asks "who references this token?" → reads node_refs.
/// `find_callees(id)`    asks "what does this node reference?" → JOINs
/// node_refs (by node_id) against node_defs (by token) to get the defining
/// nodes. Same `{ok, callees: [{node_id, source_id}]}` output shape as
/// find_callers, so mache's `udsGraph.GetCallees(id)` can mirror its
/// existing `GetCallers(token)` JSON parsing.
///
/// Read-only — does NOT belong in STATE_CHANGING_OPS.
fn op_find_callees(ctx: &DaemonContext, id: &str) -> Result<String> {
    ctx.with_read(|conn| {
        // DISTINCT — a node referencing the same token from multiple sites
        // shouldn't produce duplicate callees. The output is the SET of
        // definitions reachable from the input node, not the multiset of
        // reference sites.
        let sql = "\
            SELECT DISTINCT d.node_id, d.source_id \
            FROM node_refs r \
            JOIN node_defs d ON r.token = d.token \
            WHERE r.node_id = ?1";
        let mut stmt = conn.prepare_cached(sql)?;
        let callees: Vec<WireRef> = stmt
            .query_map([id], |row| {
                Ok(WireRef {
                    node_id: row.get::<_, String>(0)?,
                    source_id: row.get::<_, String>(1)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut builder = capnp::message::Builder::new_default();
        let mut root: leyline_public_schema::daemon_capnp::find_callees_response::Builder =
            builder.init_root();
        root.set_ok(true);
        set_ref_list(root.init_callees(callees.len() as u32), &callees);
        let reader = builder.get_root_as_reader::<
            leyline_public_schema::daemon_capnp::find_callees_response::Reader,
        >()?;
        Ok(capnp_json::to_json(reader)?)
    })
}

/// Bulk-export a `(token → [node_id])` index — the full contents of
/// `node_refs` (refs map) or `node_defs` (defs map), grouped by token.
///
/// Used by mache's `RefsMap()` / `DefsMap()` for graph-wide analysis
/// (Louvain community detection, impact-analysis BFS seeds, architecture
/// diagrams). The per-token `find_callers` / `find_defs` ops are unsuited
/// to bulk consumers because iterating them across every token would be
/// thousands of round-trips.
///
/// `source_id` is intentionally NOT included in the response — bulk
/// consumers want the (token → nodes) map for graph topology; the
/// per-token lookups still expose source_id when needed. Keeps the
/// response compact for large indexes.
///
/// Read-only — NOT in STATE_CHANGING_OPS.
fn op_get_token_map(ctx: &DaemonContext, table: &str, op: TokenMapOp) -> Result<String> {
    ctx.with_read(|conn| {
        // DISTINCT — `node_refs` / `node_defs` have no uniqueness constraint
        // on (token, node_id), so the same node referencing/defining a token
        // from multiple sites would emit duplicate node_ids without this.
        // Downstream graph-wide consumers (community detection, architecture
        // diagrams) expect each (token → node_id) edge once.
        // TODO(perf): if a single response grows large on a registry-scale
        // db, stream rows instead of materializing the full Vec into memory.
        let sql = format!("SELECT DISTINCT token, node_id FROM {table} ORDER BY token, node_id");
        let mut stmt = conn.prepare_cached(&sql)?;
        let pairs: Vec<(String, String)> = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        // Group by token in a single linear pass — the ORDER BY token
        // above means same-token rows are contiguous, so we just track
        // the current token and flush when it changes.
        let mut entries: Vec<TokenMapEntry> = Vec::new();
        let mut current_token: Option<String> = None;
        let mut current_node_ids: Vec<String> = Vec::new();
        for (token, node_id) in pairs {
            if Some(&token) != current_token.as_ref() {
                if let Some(tok) = current_token.take() {
                    entries.push(TokenMapEntry {
                        token: tok,
                        node_ids: std::mem::take(&mut current_node_ids),
                    });
                }
                current_token = Some(token);
            }
            current_node_ids.push(node_id);
        }
        if let Some(tok) = current_token {
            entries.push(TokenMapEntry {
                token: tok,
                node_ids: current_node_ids,
            });
        }

        // Build + emit JSON inside the matching branch so the
        // Builder/Reader types stay consistent. The two responses
        // share a layout today, but reading back as one type and
        // having built as the other is brittle — future schema
        // divergence would silently mis-encode without surfacing
        // a Rust type error. Per Copilot review on PR #12.
        match op {
            TokenMapOp::Refs => {
                let mut builder = capnp::message::Builder::new_default();
                {
                    let mut root:
                        leyline_public_schema::daemon_capnp::get_refs_map_response::Builder =
                        builder.init_root();
                    root.set_ok(true);
                    set_token_map_entries(root.init_entries(entries.len() as u32), &entries);
                }
                let reader = builder.get_root_as_reader::<
                    leyline_public_schema::daemon_capnp::get_refs_map_response::Reader,
                >()?;
                Ok(capnp_json::to_json(reader)?)
            }
            TokenMapOp::Defs => {
                let mut builder = capnp::message::Builder::new_default();
                {
                    let mut root:
                        leyline_public_schema::daemon_capnp::get_defs_map_response::Builder =
                        builder.init_root();
                    root.set_ok(true);
                    set_token_map_entries(root.init_entries(entries.len() as u32), &entries);
                }
                let reader = builder.get_root_as_reader::<
                    leyline_public_schema::daemon_capnp::get_defs_map_response::Reader,
                >()?;
                Ok(capnp_json::to_json(reader)?)
            }
        }
    })
}

/// Populate a capnp `List(TokenMapEntry)` from a slice of TokenMapEntry rows.
/// Shared by `op_get_token_map`'s Refs and Defs branches.
fn set_token_map_entries(
    mut list: capnp::struct_list::Builder<
        '_,
        leyline_public_schema::daemon_capnp::token_map_entry::Owned,
    >,
    entries: &[TokenMapEntry],
) {
    for (i, e) in entries.iter().enumerate() {
        let mut entry = list.reborrow().get(i as u32);
        entry.set_token(&e.token);
        let mut ids = entry.init_node_ids(e.node_ids.len() as u32);
        for (j, nid) in e.node_ids.iter().enumerate() {
            ids.set(j as u32, nid);
        }
    }
}

/// Which bulk-export op we're serving — pinpoints the response variant.
#[derive(Copy, Clone)]
enum TokenMapOp {
    Refs,
    Defs,
}

/// Export LLO's tier topology — the schema layer ownership map mache's
/// `serve_diagram.go` consumes for cross-tier dependency diagrams.
///
/// The tier→crate map is currently HARDCODED here. The real SSOT is the
/// `rs/ll-core/` vs `rs/ll-open/` workspace layout (each subdirectory's
/// member crates per `rs/Cargo.toml`). Keeping this hardcoded means a
/// new crate added to either tier requires touching this function too.
/// TODO: derive from `cargo metadata --no-deps --format-version 1` at
/// daemon startup so workspace truth is the single source. Tracked
/// implicitly under bead cc0305 follow-up.
///
/// `docs/TABLE_CONTRACT.md` describes table-ownership (which enrichment
/// pass writes which table) — NOT crate→tier maps. Don't conflate the
/// two.
///
/// If extension layers register additional tiers via `DaemonExt`,
/// future work expands this; today only the LLO built-in tiers are
/// exposed. Read-only — does NOT belong in STATE_CHANGING_OPS.
fn op_get_schema() -> Result<String> {
    let tiers: &[(&str, &[&str])] = &[
        (
            "ll-core",
            &[
                "leyline-core",
                "leyline-schema",
                "leyline-public-schema",
                "leyline-schema-capnp",
            ],
        ),
        (
            "ll-open",
            &[
                "leyline-fs",
                "leyline-ts",
                "leyline-lsp",
                "leyline-hdc",
                "leyline-cli-lib",
                "leyline-cli",
                "leyline-vcs",
                "leyline-sign",
            ],
        ),
    ];
    let mut builder = capnp::message::Builder::new_default();
    let mut root: leyline_public_schema::daemon_capnp::get_schema_response::Builder =
        builder.init_root();
    root.set_ok(true);
    let mut tiers_b = root.init_tiers(tiers.len() as u32);
    for (i, (name, crates)) in tiers.iter().enumerate() {
        let mut tier = tiers_b.reborrow().get(i as u32);
        tier.set_name(name);
        let mut crates_b = tier.init_crates(crates.len() as u32);
        for (j, c) in crates.iter().enumerate() {
            crates_b.set(j as u32, c);
        }
    }
    let reader = builder
        .get_root_as_reader::<leyline_public_schema::daemon_capnp::get_schema_response::Reader>()?;
    Ok(capnp_json::to_json(reader)?)
}

/// Export the daemon's filesystem paths — the .db location + sibling
/// capnp segment files. mache's `serve_lsp.go` / `serve_find_smells.go`
/// use these for an optional capnp readthrough fast-path; without them
/// they fall back to slower SQL queries. Strictly opt-in optimization;
/// the daemon's normal ops are unaffected.
fn op_get_db_path(ctrl_path: &Path) -> Result<String> {
    let ctrl_str = ctrl_path.to_string_lossy().to_string();
    // The .db path is the ctrl_path with .ctrl swapped for .db (mache's
    // existing discovery convention). Segment-file siblings follow the
    // same prefix.
    let base = ctrl_path.with_extension("");
    let base_str = base.to_string_lossy();
    let mut builder = capnp::message::Builder::new_default();
    let mut root: leyline_public_schema::daemon_capnp::get_db_path_response::Builder =
        builder.init_root();
    root.set_ok(true);
    root.set_db_path(format!("{base_str}.db"));
    root.set_ctrl_path(ctrl_str);
    root.set_bindings_path(format!("{base_str}.bindings.capnp"));
    root.set_ast_path(format!("{base_str}.ast.capnp"));
    root.set_source_path(format!("{base_str}.source.capnp"));
    root.set_head_path(format!("{base_str}.head.capnp"));
    let reader = builder
        .get_root_as_reader::<leyline_public_schema::daemon_capnp::get_db_path_response::Reader>(
        )?;
    Ok(capnp_json::to_json(reader)?)
}

/// Get a single node by ID.
fn op_get_node(ctx: &DaemonContext, id: &str) -> Result<String> {
    promote_touched(ctx, &[id]);

    ctx.with_read(|conn| {
        let row: Option<(String, String, String, i32, i64, Option<String>)> = query_row_opt(
            conn,
            "SELECT id, parent_id, name, kind, size, record FROM nodes WHERE id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i32>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )?;
        match row {
            Some((rid, parent_id, name, kind, size, record)) => {
                let mut builder = capnp::message::Builder::new_default();
                let mut root: leyline_public_schema::daemon_capnp::get_node_response::Builder =
                    builder.init_root();
                root.set_ok(true);
                let mut node = root.init_node();
                node.set_id(&rid);
                node.set_parent_id(&parent_id);
                node.set_name(&name);
                node.set_kind(kind);
                node.set_size(size);
                if let Some(r) = record {
                    node.set_record(&r);
                }
                let reader = builder.get_root_as_reader::<
                    leyline_public_schema::daemon_capnp::get_node_response::Reader,
                >()?;
                Ok(capnp_json::to_json(reader)?)
            }
            None => Ok(node_not_found_response(id)),
        }
    })
}

// ---------------------------------------------------------------------------
// Position-based LSP query ops
// ---------------------------------------------------------------------------

/// Find the node_id at a given (file, line, col) position via the _ast table.
fn find_node_at_position(
    conn: &Connection,
    file: &str,
    line: u32,
    col: u32,
) -> Result<Option<String>> {
    // Find the most specific (smallest range) AST node containing this position.
    query_row_opt(
        conn,
        "SELECT node_id FROM _ast \
         WHERE source_id = ?1 \
           AND start_row <= ?2 AND end_row >= ?2 \
           AND (start_row < ?2 OR start_col <= ?3) \
           AND (end_row > ?2 OR end_col >= ?3) \
         ORDER BY (end_byte - start_byte) ASC \
         LIMIT 1",
        rusqlite::params![file, line, col],
        |row| row.get::<_, String>(0),
    )
}

/// Extract the `file` field from a request, normalizing any leading
/// `file://` prefix. Returns the borrowed slice on success so callers
/// can decide whether to copy or pass through.
///
/// Production op handlers now take typed `LspFile`/`LspPosition` structs
/// (A-3 / bead b69606) and never call this helper. Kept for the unit
/// tests that pin the `file://` normalization rule — `normalize_file_uri`
/// is the live SSOT but this helper documents the request-shape
/// extraction step that used to bridge json → str.
#[cfg(test)]
fn parse_file_arg(req: &serde_json::Value) -> Result<&str> {
    let file = required_str_field(req, "file")?;
    Ok(normalize_file_uri(file))
}

/// Required-string-field extractor used by `parse_file_arg` and the
/// missing-field sweep test. Production handlers reject missing fields
/// via serde decode against `BaseRequest`; this helper survives only
/// to keep the unit tests around it meaningful.
#[cfg(test)]
fn required_str_field<'a>(req: &'a serde_json::Value, field: &'static str) -> Result<&'a str> {
    req.get(field)
        .and_then(|v| v.as_str())
        .with_context(|| format!("missing \"{field}\" field"))
}

/// Run a `query_row`, mapping `QueryReturnedNoRows` to `Ok(None)`. Other
/// errors propagate. Replaces the four-arm match (`Ok→Some / NoRows→None /
/// Err→Err`) that several "id-or-position lookup" ops were carrying inline.
fn query_row_opt<T, P, F>(conn: &Connection, sql: &str, params: P, mapper: F) -> Result<Option<T>>
where
    P: rusqlite::Params,
    F: FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
{
    match conn.query_row(sql, params, mapper) {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Wire-contract error response when a node id doesn't resolve. Used by
/// `op_read_content` and `op_get_node` — both must return the same shape
/// and message so clients can detect "no such node" without brittle string
/// matching.
fn node_not_found_response(id: &str) -> String {
    build_error_response(&format!("node '{id}' not found"))
        .unwrap_or_else(|e| fallback_error_envelope(&format!("node lookup failed: {e}")))
}

/// Build the canonical `{"ok": false, "error": "..."}` envelope via the
/// capnp-json codec. Used by handler error paths, the dispatcher's
/// catch-all, and `node_not_found_response`. Returns an `anyhow::Result`
/// because capnp builder operations can theoretically fail; in practice
/// the fallback string in callers makes the encoding-failure case visible
/// to operators rather than silent.
fn build_error_response(msg: &str) -> Result<String> {
    let mut builder = capnp::message::Builder::new_default();
    let mut root: leyline_public_schema::daemon_capnp::error_response::Builder =
        builder.init_root();
    // `ok` defaults to `false` for capnp Bool — we deliberately don't set
    // it. capnp-json emits the default, so the wire reads `"ok": false`
    // for every error response.
    root.set_error(msg);
    let reader = builder
        .get_root_as_reader::<leyline_public_schema::daemon_capnp::error_response::Reader>()?;
    Ok(capnp_json::to_json(reader)?)
}

/// Fetch the `record` column for a node id. Returns `Ok(None)` for both
/// "no such row" and "row exists but record is NULL" — they're equivalent
/// from a "no content available" client perspective. SQL errors (broken
/// connection, type mismatch, etc.) propagate as `Err`.
///
/// Single source of truth for the `SELECT record FROM nodes WHERE id = ?1`
/// query. Used by `op_read_content` and the embed drain loop. Without this
/// helper the two diverged: `op_read_content` errored on NULL records and
/// the embed loop swallowed all SQL errors via `.ok().flatten()`.
pub(crate) fn query_node_record(conn: &Connection, id: &str) -> Result<Option<String>> {
    let row: Option<Option<String>> = query_row_opt(
        conn,
        "SELECT record FROM nodes WHERE id = ?1",
        [id],
        |row| row.get::<_, Option<String>>(0),
    )?;
    Ok(row.flatten())
}

/// Read-only check: does `file` need lazy LSP enrichment?
///
/// Returns `true` if `_lsp` is missing entirely OR exists but has no
/// rows for this file. **Does NOT trigger enrichment** — the caller
/// must invoke `try_enrich_file` separately, AFTER dropping the
/// connection lock (606e64). The pre-fix `maybe_enrich` called
/// `try_enrich_file` from within a `with_live_db` closure, causing a
/// self-deadlock on `parking_lot::Mutex<Connection>` (which doesn't
/// support reentrant locking).
fn needs_enrich(conn: &Connection, file: &str) -> bool {
    // _lsp table absent ⟹ definitely needs enrich.
    let table_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='_lsp'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(false);

    if !table_exists {
        return true;
    }

    // _lsp present, check if file has any rows.
    let has_data: bool = conn
        .query_row(
            &format!("SELECT COUNT(*) > 0 FROM _lsp WHERE {NODE_ID_FOR_FILE}"),
            [file],
            |r| r.get(0),
        )
        .unwrap_or(false);

    !has_data
}

/// Queue lazy LSP enrichment for a single file on a background task.
///
/// Returns `true` if the work was queued (caller is the first one in;
/// background task will populate `_lsp*` shortly). Returns `false` if
/// the file is already in-flight (per-file gate dedup) or if there's
/// no source_dir to enrich against.
///
/// **60f75d-7a (off-UDS-thread)**: this function NEVER runs the
/// enrichment synchronously on the caller's thread. The actual work
/// — spawning a language server, polling document symbols, writing
/// `_lsp*` rows — happens on tokio's blocking pool via
/// `spawn_blocking`. The UDS connection thread returns immediately,
/// freeing other ops to advance.
///
/// Trade-off: clients calling `lsp_hover` on an un-enriched file get
/// the current (empty) result back fast, with `enriched: true` in the
/// response signaling "retry to get fresh data." The fresh data is
/// available on the next request once the background task lands its
/// `_lsp*` writes.
///
/// **Caller invariant**: must NOT be called while holding the
/// `ctx.live_db` lock — the spawned task acquires it itself via
/// `enrichment::run_pass`. Callers using `with_live_db` to check
/// `needs_enrich` MUST drop the lock before calling this (606e64).
fn try_enrich_file(ctx: &std::sync::Arc<DaemonContext>, file: &str) -> bool {
    // Per-file in-flight gate (606e64): prevent N concurrent hovers
    // on the same un-enriched file from N spawning N LSP servers.
    // First caller inserts; subsequent callers see "in flight" and
    // skip. The first caller's enrichment populates _lsp before the
    // user's next request, so dedup is bounded.
    //
    // Gate runs BEFORE source_dir check so the RAII guard's release
    // path is always exercised (testable) and the gate's contract
    // ("if you got past me, you own the work") is uniform regardless
    // of whether the work itself short-circuits below.
    {
        let mut inflight = ctx.enrich_inflight.lock();
        if !inflight.insert(file.to_string()) {
            // Another caller is already enriching this file. Skip.
            return false;
        }
    }

    // Source-dir check is sync (cheap; just a None match). Done
    // outside the spawned task so we can release the gate immediately
    // if there's nothing to enrich.
    if ctx.source_dir.is_none() {
        // Pop the gate entry — there's no spawned task to release it via Drop.
        ctx.enrich_inflight.lock().remove(file);
        return false;
    }

    // 60f75d-7a: spawn the actual enrichment on the blocking pool so
    // the UDS thread returns immediately. The blocking pool can grow
    // to handle concurrent enrichments without starving async workers.
    let ctx_owned = ctx.clone();
    let file_owned = file.to_string();
    tokio::task::spawn_blocking(move || {
        // RAII: gate is released when this closure returns (success
        // OR error). MUST be inside spawn_blocking — without it, an
        // error path would leak the file into the in-flight set
        // forever, blocking all future enrichment attempts.
        struct InflightGuard {
            set: std::sync::Arc<parking_lot::Mutex<std::collections::HashSet<String>>>,
            file: String,
        }
        impl Drop for InflightGuard {
            fn drop(&mut self) {
                self.set.lock().remove(&self.file);
            }
        }
        let _guard = InflightGuard {
            set: ctx_owned.enrich_inflight.clone(),
            file: file_owned.clone(),
        };

        let source_dir = match &ctx_owned.source_dir {
            Some(d) => d.clone(),
            None => return, // shouldn't reach (sync check above), but be safe
        };

        eprintln!("lazy enrich (bg): triggering LSP for {file_owned}");

        let guard = ctx_owned.live_db.writer.lock();
        let result = crate::daemon::enrichment::run_pass(
            &ctx_owned.enrichment_passes,
            "lsp",
            &guard,
            &source_dir,
            Some(std::slice::from_ref(&file_owned)),
            Some(&ctx_owned.state),
        );
        drop(guard);

        match result {
            Ok(stats) => {
                if let Some(s) = stats.last() {
                    eprintln!("lazy enrich (bg): {} items for {file_owned}", s.items_added);
                }
            }
            Err(e) => {
                log::warn!("lazy enrich failed for {file_owned}: {e:#}");
            }
        }
    });

    true // queued
}

/// Hover info at a position. Auto-enriches if no data exists.
fn op_lsp_hover(ctx: &std::sync::Arc<DaemonContext>, args: &LspPosition) -> Result<String> {
    let file = normalize_file_uri(&args.file).to_string();
    let line = args.line;
    let col = args.col;
    let (result, enriched) = with_lazy_enrich_retry(
        ctx,
        &file,
        |conn| lsp_hover_query(conn, &file, line, col),
        |opt| opt.is_none(),
    )?;
    Ok(hover_response(result, enriched))
}

/// Build the JSON response for `lsp_hover`. The `enriched: true` marker
/// is set whenever a lazy refresh just ran — independent of whether the
/// retry produced a hit. Clients use this to distinguish "no data, never
/// enriched" (worth retrying) from "no data, just enriched" (don't retry).
fn hover_response(result: Option<(String, String)>, enriched: bool) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("ok".to_string(), json!(true));
    match result {
        Some((hover, node_id)) => {
            obj.insert("hover".to_string(), json!(hover));
            obj.insert("node_id".to_string(), json!(node_id));
        }
        None => {
            obj.insert("hover".to_string(), serde_json::Value::Null);
        }
    }
    if enriched {
        obj.insert("enriched".to_string(), json!(true));
    }
    serde_json::Value::Object(obj).to_string()
}

/// Run an LSP query with one optional lazy-enrichment retry. If the first
/// attempt's result satisfies `is_empty` AND the file hasn't been enriched
/// yet, trigger enrichment and re-run the query once. Returns the final
/// result paired with a flag indicating whether the retry actually ran
/// (whether or not it produced a hit).
///
/// Centralizing this collapses the previously-divergent retry logic in
/// `op_lsp_hover` (which dropped `enriched: true` on a null retry) and
/// `op_lsp_position` (which kept it). Both now share one code path.
fn with_lazy_enrich_retry<T, IsEmpty, Query>(
    ctx: &std::sync::Arc<DaemonContext>,
    file: &str,
    mut query: Query,
    is_empty: IsEmpty,
) -> Result<(T, bool)>
where
    Query: FnMut(&Connection) -> Result<T>,
    IsEmpty: Fn(&T) -> bool,
{
    let result = ctx.with_write(|conn| query(conn))?;
    if !is_empty(&result) {
        return Ok((result, false));
    }

    // Step 1: read-only check whether enrichment is needed. Lock held only
    // for the check, not for the enrichment work.
    let needs = ctx.with_read(|conn| Ok(needs_enrich(conn, file)))?;
    if !needs {
        // _lsp data exists for this file; the empty-result is real, not
        // a missing-enrichment artifact. Return as-is.
        return Ok((result, false));
    }

    // Step 2: queue enrichment on the blocking pool. Returns immediately
    // (60f75d-7a) — no live_db lock held, no UDS thread blocked.
    let queued = try_enrich_file(ctx, file);

    // Step 3: return current (empty) result with `enriched=true` if
    // enrichment is in flight. Client sees the retry hint and re-queries
    // on the next tick — by then the background task has populated _lsp.
    //
    // Pre-7a behavior was to BLOCK here for the enrichment to finish, then
    // re-run the query for fresh data on the same request. That blocked
    // the UDS thread for 1-5s. Post-7a: clients accept "retry to get
    // fresh data" semantics; the `enriched: true` marker is the signal.
    Ok((result, queued))
}

fn lsp_hover_query(
    conn: &Connection,
    file: &str,
    line: u32,
    col: u32,
) -> Result<Option<(String, String)>> {
    let node_id = match find_node_at_position(conn, file, line, col)? {
        Some(id) => id,
        None => return Ok(None),
    };
    let hover = query_row_opt(
        conn,
        "SELECT hover_text FROM _lsp_hover WHERE node_id = ?1",
        [&node_id],
        |row| row.get::<_, String>(0),
    )?;
    Ok(hover.map(|text| (text, node_id)))
}

/// Go-to-definition at a position. Auto-enriches if no data exists.
fn op_lsp_defs(ctx: &std::sync::Arc<DaemonContext>, args: &LspPosition) -> Result<String> {
    op_lsp_position(ctx, args, "_lsp_defs", "def", "definitions")
}

/// Find references at a position. Auto-enriches if no data exists.
fn op_lsp_refs(ctx: &std::sync::Arc<DaemonContext>, args: &LspPosition) -> Result<String> {
    op_lsp_position(ctx, args, "_lsp_refs", "ref", "references")
}

/// Shared body for `lsp_defs` and `lsp_refs`. They differ only in the
/// `_lsp_*` table queried, the column prefix in that table, and the JSON
/// key under which results are returned. Both follow the same shape:
/// resolve the node at (file, line, col), pull rows from the 5-column
/// position table, retry once after lazy enrichment if the first attempt
/// is empty.
fn op_lsp_position(
    ctx: &std::sync::Arc<DaemonContext>,
    args: &LspPosition,
    table: &str,
    col_prefix: &str,
    json_key: &str,
) -> Result<String> {
    let file = normalize_file_uri(&args.file).to_string();
    let line = args.line;
    let col = args.col;
    let (rows, enriched) = with_lazy_enrich_retry(
        ctx,
        &file,
        |conn| match find_node_at_position(conn, &file, line, col)? {
            Some(id) => lsp_5col_position_rows(conn, &id, table, col_prefix),
            None => Ok(vec![]),
        },
        |v: &Vec<serde_json::Value>| v.is_empty(),
    )?;
    Ok(lsp_rows_response(json_key, rows, enriched))
}

/// Build the JSON response for an LSP query. Inserts the
/// `enriched: true` marker only when a lazy refresh just ran — the
/// shape clients see on a fresh-cache hit must be identical to the
/// shape on a warm hit (same fields, just no `enriched` key).
///
/// Used by both position queries (defs/refs/hover) where `enriched`
/// reflects whether a retry happened, and by file-level queries
/// (symbols/diagnostics) which always pass `enriched=false`.
fn lsp_rows_response(json_key: &str, rows: Vec<serde_json::Value>, enriched: bool) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("ok".to_string(), json!(true));
    obj.insert(json_key.to_string(), json!(rows));
    if enriched {
        obj.insert("enriched".to_string(), json!(true));
    }
    serde_json::Value::Object(obj).to_string()
}

/// Run a single-file LSP rows query: `prepare_cached` the supplied SQL,
/// bind `file` as `?1`, decode each row via `mapper`, collect into a
/// `Vec<Value>`. Used by `op_lsp_symbols` and `op_lsp_diagnostics`,
/// which differ only in the SELECTed columns and how each row is
/// decoded — both share this pipeline.
///
/// Returns `Ok(vec![])` when `table` doesn't exist yet (the pre-enrichment
/// state). This matches `lsp_5col_position_rows`'s contract — without
/// the guard, queries against a not-yet-enriched `_lsp` raise a SQL
/// error which clients have to special-case. `op_lsp_symbols` and
/// `op_lsp_defs`/`refs` were behaviorally divergent in this respect
/// before the guard was added (caught by adversarial review).
fn query_lsp_rows_for_file<F>(
    conn: &Connection,
    file: &str,
    table: &str,
    sql: &str,
    mapper: F,
) -> Result<Vec<serde_json::Value>>
where
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<serde_json::Value>,
{
    if !table_exists(conn, table) {
        return Ok(vec![]);
    }
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt
        .query_map([file], mapper)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Document symbols for a file.
fn op_lsp_symbols(ctx: &DaemonContext, args: &LspFile) -> Result<String> {
    let file = normalize_file_uri(&args.file);
    let sql = format!(
        "SELECT node_id, symbol_kind, detail, start_line, start_col, end_line, end_col \
         FROM _lsp WHERE {NODE_ID_FOR_FILE}"
    );
    ctx.with_read(|conn| {
        let rows = query_lsp_rows_for_file(conn, file, "_lsp", &sql, |row| {
            Ok(json!({
                "node_id": row.get::<_, String>(0)?,
                "kind": row.get::<_, String>(1)?,
                "detail": row.get::<_, String>(2)?,
                "start_line": row.get::<_, i32>(3)?,
                "start_col": row.get::<_, i32>(4)?,
                "end_line": row.get::<_, i32>(5)?,
                "end_col": row.get::<_, i32>(6)?,
            }))
        })?;
        Ok(lsp_rows_response("symbols", rows, false))
    })
}

/// Diagnostics for a file.
fn op_lsp_diagnostics(ctx: &DaemonContext, args: &LspFile) -> Result<String> {
    let file = normalize_file_uri(&args.file);
    let sql = format!(
        "SELECT node_id, diagnostics, start_line, start_col, end_line, end_col \
         FROM _lsp WHERE {NODE_ID_FOR_FILE} \
         AND diagnostics IS NOT NULL AND diagnostics != ''"
    );
    ctx.with_read(|conn| {
        let rows = query_lsp_rows_for_file(conn, file, "_lsp", &sql, |row| {
            Ok(json!({
                "node_id": row.get::<_, String>(0)?,
                "diagnostics": row.get::<_, String>(1)?,
                "start_line": row.get::<_, i32>(2)?,
                "start_col": row.get::<_, i32>(3)?,
                "end_line": row.get::<_, i32>(4)?,
                "end_col": row.get::<_, i32>(5)?,
            }))
        })?;
        Ok(lsp_rows_response("diagnostics", rows, false))
    })
}

// ---------------------------------------------------------------------------
// vec_search — KNN over the sidecar VectorIndex
// ---------------------------------------------------------------------------

/// `{"op":"vec_search", "query":"text", "k":10}` — embed the query via the
/// active embedder and KNN-search the sidecar VectorIndex. Returns
/// `{ok, results: [{node_id, distance}]}`.
#[cfg(feature = "vec")]
fn op_vec_search(ctx: &DaemonContext, query: &str, k: u32) -> Result<String> {
    let k = k as usize;
    let qvec = ctx.embedder.embed(query).context("embed query")?;
    let results = ctx.vec_index.search(&qvec, k).context("vec search")?;
    let rows: Vec<serde_json::Value> = results
        .into_iter()
        .map(|(id, d)| json!({"node_id": id, "distance": d}))
        .collect();
    Ok(json!({"ok": true, "results": rows}).to_string())
}

// ---------------------------------------------------------------------------
// text_search — XTR-WARP-class retrieval via leyline-text-search
// ---------------------------------------------------------------------------

/// `{"op":"text_search", "query":"text", "k":10}` — unstructured-text search
/// over the engine installed by `DaemonExt::text_search_engine()` (default
/// `NullEngine` returns a structured "no backend" error). Returns
/// `{ok, results: [{node_id, score}]}`. Complementary to `vec_search` —
/// the engines model different access patterns (single-vector KNN vs
/// late-interaction / hybrid).
#[cfg(feature = "text-search")]
fn op_text_search(ctx: &DaemonContext, query: &str, k: u32) -> Result<String> {
    let k = k as usize;
    let hits = ctx
        .text_search
        .search(query, k)
        .map_err(|e| anyhow::anyhow!("text_search: {e}"))?;
    let rows: Vec<serde_json::Value> = hits
        .into_iter()
        .map(|h| json!({"node_id": h.node_id, "score": h.score}))
        .collect();
    Ok(json!({"ok": true, "results": rows}).to_string())
}

// ---------------------------------------------------------------------------
// leyline_version — wire-compat handshake (bead ley-line-open-cb8960)
// ---------------------------------------------------------------------------

/// `{"op":"leyline_version"}` — returns the daemon's runtime version
/// and wire-format identity from the constants in
/// [`crate::daemon::version`]. Takes no arguments. Read-only — NOT in
/// `STATE_CHANGING_OPS`. Idempotent.
///
/// Builds the response through capnp + capnp_json so the wire shape
/// matches the rest of the op surface (`$Json.name(...)` annotations
/// drive snake_case field names; UInt32 stays a JSON number,
/// matching the existing pattern in `SnapshotResponse` etc.).
fn op_leyline_version() -> Result<String> {
    use crate::daemon::version;
    let mut builder = capnp::message::Builder::new_default();
    let mut root: leyline_public_schema::daemon_capnp::leyline_version_response::Builder =
        builder.init_root();
    root.set_ok(true);
    root.set_binary_version(version::BINARY_VERSION);
    root.set_schema_version(version::SCHEMA_VERSION);
    root.set_wire_format_major(version::WIRE_FORMAT_MAJOR);
    root.set_compat_min(version::COMPAT_MIN_SCHEMA_VERSION);
    root.set_build_date(version::BUILD_DATE);
    let reader = builder
        .get_root_as_reader::<leyline_public_schema::daemon_capnp::leyline_version_response::Reader>(
        )?;
    Ok(capnp_json::to_json(reader)?)
}

// ---------------------------------------------------------------------------
// validate — tree-sitter syntactic validation (beads ley-line-open-fa8638,
// ley-line-open-736800)
// ---------------------------------------------------------------------------

/// `{"op":"validate", "content":"...", "language":"go"}` — runs the
/// `leyline-fs::validate` tree-sitter validator on caller-supplied
/// content without persisting anything. Read-only — NOT in
/// `STATE_CHANGING_OPS`.
///
/// Returns `{ ok: bool, errors: [{row, col, byte_start, byte_end,
/// message}], diagnostics: [{line, col, message}] }`:
///
/// - `errors` (bead ley-line-open-736800) — EVERY ERROR/MISSING node
///   from the parse, in document order, using the same tree-sitter
///   grammars the `_ast` producer uses. `row`/`col` are 0-based;
///   `byte_start`/`byte_end` delimit the node in the source buffer
///   (equal for zero-width MISSING nodes). mache renders these into
///   `_diagnostics/ast-errors` for draft-mode UX.
/// - `diagnostics` — legacy first-error-only shape from bead
///   ley-line-open-fa8638, kept for wire compat: `[]` when ok, else
///   exactly one `{line, col, message: "syntax error"}` entry.
///
/// Either `language` (extension key per `language_for_extension`) or
/// `path` (extension extracted) must be supplied; if both are present,
/// `language` wins. Unknown/unsupported languages return the daemon's
/// structured error envelope (`{ok: false, error: "..."}`), never a
/// panic. `content` is UTF-8 source text on the wire.
///
/// Mirrors mache's `writeback/validate.go` so mache can drop the
/// CGO tree-sitter link (mache-36d961 item A5 / mache-37ae8b).
#[cfg(feature = "validate")]
fn op_validate(req: &ValidateRequest) -> Result<String> {
    use leyline_fs::validate::{collect_syntax_errors, language_for_extension, language_for_node};

    let lang = match (req.language.as_deref(), req.path.as_deref()) {
        (Some(l), _) => language_for_extension(l)
            .ok_or_else(|| anyhow::anyhow!("unknown language id: `{l}`"))?,
        (None, Some(p)) => language_for_node(p, None).ok_or_else(|| {
            anyhow::anyhow!("cannot determine language from path `{p}` (no recognized extension)")
        })?,
        (None, None) => {
            return Err(anyhow::anyhow!(
                "validate requires either `language` or `path`"
            ));
        }
    };

    let errors = collect_syntax_errors(req.content.as_bytes(), &lang)
        .map_err(|e| anyhow::anyhow!("validate: {e}"))?;

    let error_objs: Vec<serde_json::Value> = errors
        .iter()
        .map(|e| {
            json!({
                "row": e.row,
                "col": e.col,
                "byte_start": e.byte_start,
                "byte_end": e.byte_end,
                "message": e.message,
            })
        })
        .collect();

    // Legacy fa8638 shape: at most one entry, fixed "syntax error"
    // message, positioned at the first error.
    let diagnostics: Vec<serde_json::Value> = errors
        .first()
        .map(|first| {
            vec![json!({
                "line": first.row,
                "col": first.col,
                "message": "syntax error",
            })]
        })
        .unwrap_or_default();

    // emit_ast (bead `ley-line-open-851f24` follow-up): when the caller
    // opts in, run the extractor pipeline over the same buffer and return
    // `_ast` / `node_defs` / `node_refs` / `_imports` rows in the same
    // response. Mache's writeback linter folds ONE parse into both syntax
    // validation and SQL-shaped AST rows, killing the interim go/parser
    // and unblocking CGO removal on the mache side.
    let ast_payload = if req.emit_ast.unwrap_or(false) {
        // Determine the TsLanguage enum variant that matches. The
        // `language_for_extension` above returns a raw tree-sitter
        // `Language`, but the extractor pipeline keys on `TsLanguage`.
        let ts_lang = req
            .language
            .as_deref()
            .and_then(|l| leyline_ts::languages::TsLanguage::from_name(l).ok())
            .or_else(|| {
                req.path.as_deref().and_then(|p| {
                    let ext = std::path::Path::new(p).extension()?.to_str()?;
                    leyline_ts::languages::TsLanguage::from_name(ext).ok()
                })
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "emit_ast: no `TsLanguage` variant for the requested language \
                     (the extractor pipeline supports a subset of the validator's languages; \
                     rules land per-language)"
                )
            })?;
        // source_id = the caller's `path` (canonical), or a synthetic
        // sentinel when the caller only sent `language` + `content`.
        // Mache's linter fold always sends `path`; the sentinel is for
        // ad-hoc callers.
        let source_id = req.path.as_deref().unwrap_or("<inline>");
        Some(crate::cmd_parse::parse_to_ast_json(
            req.content.as_bytes(),
            ts_lang,
            source_id,
        )?)
    } else {
        None
    };

    let mut response = json!({
        "ok": errors.is_empty(),
        "errors": error_objs,
        "diagnostics": diagnostics,
    });
    if let Some(ast_payload) = ast_payload {
        response["ast"] = ast_payload;
    }
    Ok(response.to_string())
}

// ---------------------------------------------------------------------------
// HDC daemon ops (bead ley-line-open-c32596) — wires the leyline-hdc
// query surface for structural-similarity search. The substrate (the
// `_hdc` table population pass) is a separate enrichment-pass concern;
// these ops query the table. All read-only.
// ---------------------------------------------------------------------------

/// Shared bootstrap for every HDC op: ensure schema + UDFs are
/// registered on the connection. Both leyline-hdc primitives are
/// documented as idempotent — `create_hdc_schema` uses `CREATE TABLE
/// IF NOT EXISTS`; `register_hdc_udfs` re-registration replaces. Cheap
/// to call on every request.
#[cfg(feature = "hdc")]
fn ensure_hdc_ready(conn: &rusqlite::Connection) -> Result<()> {
    leyline_hdc::schema::create_hdc_schema(conn)
        .context("create HDC schema (_hdc, _hdc_combined, _hdc_baseline, _hdc_subtree_cache)")?;
    leyline_hdc::sql_udf::register_hdc_udfs(conn)
        .context("register HDC SQL UDFs (popcount_xor, BUNDLE, BUNDLE_MAJORITY)")?;
    Ok(())
}

/// Encode caller-supplied content into a hypervector via the tree-
/// sitter + canonical-kind-map bridge. Supported language ids are the
/// intersection of leyline-hdc's `CanonicalKindMap` impls and leyline-
/// ts's `TsLanguage` variants — today `go`, `rust`, `json`, `yaml`.
#[cfg(feature = "hdc")]
fn encode_query_hv(content: &str, language: &str) -> Result<leyline_hdc::Hypervector> {
    use leyline_hdc::canonical::CanonicalKindMap;
    use leyline_hdc::codebook::AstCodebook;
    use leyline_hdc::encode_fresh;

    let ts_lang = leyline_ts::languages::TsLanguage::from_name(language)
        .with_context(|| format!("HDC: unknown language `{language}`"))?;

    let kind_map: Box<dyn CanonicalKindMap> = match language.to_lowercase().as_str() {
        "go" | "golang" => Box::new(leyline_hdc::canonical::GoCanonicalMap),
        "rust" | "rs" => Box::new(leyline_hdc::canonical::RustCanonicalMap),
        "json" => Box::new(leyline_hdc::canonical::JsonCanonicalMap),
        "yaml" | "yml" => Box::new(leyline_hdc::canonical::YamlCanonicalMap),
        other => {
            return Err(anyhow::anyhow!(
                "HDC: no CanonicalKindMap for `{other}`; supported: go, rust, json, yaml"
            ));
        }
    };

    let encoder_node =
        super::hdc_pass::parse_and_encode_tree(content, &ts_lang.ts_language(), &*kind_map)
            .ok_or_else(|| {
                anyhow::anyhow!("HDC: tree-sitter parse returned no tree for the given content")
            })?;

    let codebook = AstCodebook;
    Ok(encode_fresh(&encoder_node, &codebook))
}

/// `{"op":"hdc_search", "content":"...", "language":"go", "max_distance":100, "k":10}` —
/// parses + encodes the query content, then runs `radius_search` on the
/// AST layer of `_hdc`. Returns `{ok, results: [{scope_id, distance}]}`.
/// Read-only. If `_hdc` hasn't been populated yet, returns empty results.
#[cfg(feature = "hdc")]
fn op_hdc_search(ctx: &std::sync::Arc<DaemonContext>, req: &HdcSearchRequest) -> Result<String> {
    let hv = encode_query_hv(&req.content, &req.language)?;
    let max_distance = req.max_distance;
    let k = req.k as usize;
    ctx.with_write(|conn| {
        ensure_hdc_ready(conn)?;
        let matches = leyline_hdc::query::radius_search(
            conn,
            leyline_hdc::LayerKind::Ast,
            &hv,
            max_distance,
            k,
        )
        .context("HDC radius_search")?;
        let rows: Vec<serde_json::Value> = matches
            .into_iter()
            .map(|m| json!({"scope_id": m.scope_id, "distance": m.distance}))
            .collect();
        Ok(json!({"ok": true, "results": rows}).to_string())
    })
}

/// `{"op":"hdc_calibrate", "sample_size":1000}` — recomputes median +
/// MAD over `_hdc` row distances per layer; writes to `_hdc_baseline`.
/// Returns `{ok, layers_calibrated: N}`. Read-only with respect to the
/// projected db — only touches HDC sidecar tables, so NOT in
/// `STATE_CHANGING_OPS`.
#[cfg(feature = "hdc")]
fn op_hdc_calibrate(
    ctx: &std::sync::Arc<DaemonContext>,
    req: &HdcCalibrateRequest,
) -> Result<String> {
    let sample_size = req.sample_size;
    ctx.with_write(|conn| {
        ensure_hdc_ready(conn)?;
        // `now_ms` is computed inside the closure rather than at request
        // entry to match the time the rows are actually written; for
        // tests this is a wall-clock value, not a deterministic constant.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let count = leyline_hdc::calibrate::calibrate_and_persist(conn, sample_size, now_ms)
            .context("HDC calibrate_and_persist")?;
        Ok(json!({"ok": true, "layers_calibrated": count}).to_string())
    })
}

/// `{"op":"hdc_density", "content":"...", "language":"go", "max_distance":100}` —
/// counts scopes within `max_distance` of the encoded query HV on the
/// AST layer. Returns `{ok, count: N}`. Read-only.
#[cfg(feature = "hdc")]
fn op_hdc_density(ctx: &std::sync::Arc<DaemonContext>, req: &HdcDensityRequest) -> Result<String> {
    let hv = encode_query_hv(&req.content, &req.language)?;
    let max_distance = req.max_distance;
    ctx.with_write(|conn| {
        ensure_hdc_ready(conn)?;
        let count =
            leyline_hdc::query::density_count(conn, leyline_hdc::LayerKind::Ast, &hv, max_distance)
                .context("HDC density_count")?;
        Ok(json!({"ok": true, "count": count}).to_string())
    })
}

// ---------------------------------------------------------------------------
// inspect_symbol — bundled symbol inspection (bead ley-line-open-c2c4d9,
// L1 of the agent-first surface decomp; ADR-0016 §2).
//
// Composes find_defs / find_callers / find_callees / lsp_hover into one
// response so agents pay one round-trip instead of N. The existing
// primitives stay as-is; this op delegates to the same SQL paths and
// joins the results into the bundle shape ADR-0016 §2 commits to.
//
// Read-only — NOT in STATE_CHANGING_OPS.
// ---------------------------------------------------------------------------

/// One definition row in the bundle. Carries enough location info
/// (file, byte range, line range) for the caller to fetch source
/// directly without a follow-up `get_node` / `read_content` call.
#[derive(Debug)]
struct DefRow {
    node_id: String,
    source_id: String,
    node_kind: String,
    start_line: i32,
    start_col: i32,
    end_line: i32,
    end_col: i32,
    start_byte: i64,
    end_byte: i64,
}

/// `{"op":"inspect_symbol", "symbol_id":"...", "include":[...]?}` —
/// the spine of the agent-first surface. Returns the bundle ADR-0016 §2
/// specifies: definitions + hover + references + callers + callees +
/// freshness, in one round-trip. Read-only.
fn op_inspect_symbol(
    ctx: &std::sync::Arc<DaemonContext>,
    req: &InspectSymbolRequest,
) -> Result<String> {
    let include_filter = build_include_filter(&req.include);

    ctx.with_read(|conn| {
        // Definitions: query node_defs WHERE token = symbol_id,
        // JOINed against _ast for full location info. This is the
        // anchor — empty defs means the symbol is unknown.
        let defs = query_definitions(conn, &req.symbol_id)?;

        // `kind` is derived from the FIRST def's _ast.node_kind,
        // mapped to a broad category. Multiple defs is allowed
        // (overloads, methods with the same name across types);
        // the caller can inspect `definitions` directly.
        let kind = defs
            .first()
            .map(|d| classify_node_kind(&d.node_kind))
            .unwrap_or("unknown");

        // References: every row in node_refs for this token. ADR-0016
        // §2 distinguishes `references` (raw call sites) from
        // `callers` (deduped containing functions). v1: both come
        // from node_refs; `callers` is the DISTINCT-by-node_id
        // projection. Refining `callers` to walk up _ast for the
        // enclosing function is a follow-up.
        let references = query_token_refs(conn, &req.symbol_id, "node_refs")?;

        // Callees: definitions of every token the FIRST definition's
        // node references. Same SQL as op_find_callees, applied to
        // the primary def's node_id.
        let callees = if let Some(primary) = defs.first() {
            query_callees(conn, &primary.node_id)?
        } else {
            Vec::new()
        };

        // Hover: best-effort lookup in _lsp for the primary def's
        // node_id. Returns None if no _lsp row exists (the
        // enrichment pass hasn't run, or this symbol has no
        // language-server data).
        let hover_typed = if let Some(primary) = defs.first() {
            query_hover_typed(conn, &primary.node_id)?
        } else {
            None
        };

        // Freshness: parse_version from `_meta` (matches the
        // enrichment-pass basis-tracking discipline) + the current
        // wall-clock as `parsed_at_ms`. Future revisions can layer
        // source_mtime_ms / stalk_hash per ADR-0016 §7.
        let generation: u64 = leyline_ts::schema::get_meta(conn, "parse_version")
            .ok()
            .flatten()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        let parsed_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        // Build the response. The `include` filter, if non-empty,
        // omits expensive sub-fields. `symbol_id`, `ok`, `kind`,
        // `provenance`, and `certainty` are always included
        // (provenance + certainty per bead ley-line-open-c3555f / L7).
        let mut response = serde_json::Map::new();
        response.insert("ok".to_string(), json!(true));
        response.insert("symbol_id".to_string(), json!(req.symbol_id));
        response.insert("kind".to_string(), json!(kind));

        // Top-level provenance: "composed" because the bundle merges
        // tree-sitter rows (definitions/references) with optional LSP
        // hover. Top-level certainty: "full" if hover_typed is
        // available, "partial" when missing — caller can tell at a
        // glance whether the LSP enrichment pass has reached this
        // symbol.
        let top_certainty = if hover_typed.is_some() {
            "full"
        } else {
            "partial"
        };
        response.insert("provenance".to_string(), json!("composed"));
        response.insert("certainty".to_string(), json!(top_certainty));

        if include_filter.has("definitions") {
            let defs_json: Vec<serde_json::Value> = defs
                .iter()
                .map(|d| {
                    json!({
                        "node_id":    d.node_id,
                        "source_id":  d.source_id,
                        "node_kind":  d.node_kind,
                        "start_line": d.start_line,
                        "start_col":  d.start_col,
                        "end_line":   d.end_line,
                        "end_col":    d.end_col,
                        "start_byte": d.start_byte,
                        "end_byte":   d.end_byte,
                        // L7: definitions are structural truth from
                        // tree-sitter (node_defs ⋈ _ast). Always full
                        // — if a def exists the location is canonical.
                        "provenance": "tree-sitter",
                        "certainty":  "full",
                    })
                })
                .collect();
            response.insert("definitions".to_string(), json!(defs_json));
        }

        if include_filter.has("hover_typed") {
            // L7: when hover is populated, the row came from the
            // _lsp table (LSP enrichment pass writes it). Inject
            // provenance + certainty INTO the hover object so
            // downstream consumers see a uniform per-result shape.
            let hover_with_provenance = match hover_typed {
                Some(serde_json::Value::Object(mut m)) => {
                    m.insert("provenance".to_string(), json!("lsp"));
                    m.insert("certainty".to_string(), json!("full"));
                    serde_json::Value::Object(m)
                }
                other => other.unwrap_or(serde_json::Value::Null),
            };
            response.insert("hover_typed".to_string(), hover_with_provenance);
        }

        if include_filter.has("references") {
            let refs_json: Vec<serde_json::Value> = references
                .iter()
                .map(|r| {
                    json!({
                        "node_id":    r.node_id,
                        "source_id":  r.source_id,
                        // L7: references are tree-sitter structural facts.
                        "provenance": "tree-sitter",
                        "certainty":  "full",
                    })
                })
                .collect();
            response.insert("references".to_string(), json!(refs_json));
        }

        if include_filter.has("callers") {
            // v1: callers ≈ references DISTINCT by node_id. Refining
            // to "the enclosing function of each reference" is a
            // follow-up bead.
            let mut seen = std::collections::HashSet::new();
            let mut callers_json: Vec<serde_json::Value> = Vec::new();
            for r in &references {
                if seen.insert(r.node_id.clone()) {
                    callers_json.push(json!({
                        "node_id":    r.node_id,
                        "source_id":  r.source_id,
                        "provenance": "tree-sitter",
                        "certainty":  "full",
                    }));
                }
            }
            response.insert("callers".to_string(), json!(callers_json));
        }

        if include_filter.has("callees") {
            let callees_json: Vec<serde_json::Value> = callees
                .iter()
                .map(|r| {
                    json!({
                        "node_id":    r.node_id,
                        "source_id":  r.source_id,
                        "provenance": "tree-sitter",
                        "certainty":  "full",
                    })
                })
                .collect();
            response.insert("callees".to_string(), json!(callees_json));
        }

        if include_filter.has("freshness") {
            response.insert(
                "freshness".to_string(),
                json!({"generation": generation, "parsed_at_ms": parsed_at_ms}),
            );
        }

        Ok(serde_json::Value::Object(response).to_string())
    })
}

/// SQL: node_defs ⋈ _ast for definition rows with full location info.
fn query_definitions(conn: &Connection, token: &str) -> Result<Vec<DefRow>> {
    let sql = "\
        SELECT d.node_id, d.source_id, a.node_kind, \
               a.start_row, a.start_col, a.end_row, a.end_col, \
               a.start_byte, a.end_byte \
        FROM node_defs d \
        LEFT JOIN _ast a ON a.node_id = d.node_id AND a.source_id = d.source_id \
        WHERE d.token = ?1";
    let mut stmt = conn.prepare_cached(sql)?;
    let rows: Vec<DefRow> = stmt
        .query_map([token], |row| {
            Ok(DefRow {
                node_id: row.get::<_, String>(0)?,
                source_id: row.get::<_, String>(1)?,
                node_kind: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                start_line: row.get::<_, Option<i32>>(3)?.unwrap_or(0),
                start_col: row.get::<_, Option<i32>>(4)?.unwrap_or(0),
                end_line: row.get::<_, Option<i32>>(5)?.unwrap_or(0),
                end_col: row.get::<_, Option<i32>>(6)?.unwrap_or(0),
                start_byte: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
                end_byte: row.get::<_, Option<i64>>(8)?.unwrap_or(0),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Same SQL as `op_find_callees` — DISTINCT (d.node_id, d.source_id)
/// rows from `node_refs ⋈ node_defs` on token.
fn query_callees(conn: &Connection, node_id: &str) -> Result<Vec<WireRef>> {
    let sql = "\
        SELECT DISTINCT d.node_id, d.source_id \
        FROM node_refs r \
        JOIN node_defs d ON r.token = d.token \
        WHERE r.node_id = ?1";
    let mut stmt = conn.prepare_cached(sql)?;
    let rows: Vec<WireRef> = stmt
        .query_map([node_id], |row| {
            Ok(WireRef {
                node_id: row.get::<_, String>(0)?,
                source_id: row.get::<_, String>(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Best-effort hover lookup via `_lsp` table. Returns None if no row
/// exists for this node_id (enrichment hasn't run, or LSP returned
/// nothing for this symbol).
fn query_hover_typed(conn: &Connection, node_id: &str) -> Result<Option<serde_json::Value>> {
    // The `_lsp` table is the LSP enrichment pass's output (see
    // op_lsp_hover for the canonical shape). v1 uses the simplest
    // available columns: detail / symbol_kind. Future revisions can
    // parse `detail` into params / returns / receiver_type per
    // ADR-0016 §2's hover_typed shape.
    //
    // `_lsp` is created lazily by the LSP enrichment pass; if the
    // table doesn't exist, return None gracefully.
    let has_lsp: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='_lsp'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;
    if !has_lsp {
        return Ok(None);
    }
    let row: Option<(Option<String>, Option<String>)> = query_row_opt(
        conn,
        "SELECT detail, symbol_kind FROM _lsp WHERE node_id = ?1 LIMIT 1",
        [node_id],
        |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
            ))
        },
    )?;
    let Some((detail, symbol_kind)) = row else {
        return Ok(None);
    };
    Ok(Some(json!({
        "signature": detail.unwrap_or_default(),
        "kind":      symbol_kind.unwrap_or_default(),
    })))
}

/// Lightweight set-style filter for the `include` field. Empty input
/// = include everything; non-empty input = include only the listed
/// fields (case-insensitive). Always-on fields (`ok`, `symbol_id`,
/// `kind`) are appended outside this filter.
struct IncludeFilter {
    set: Option<std::collections::HashSet<String>>,
}

impl IncludeFilter {
    fn has(&self, field: &str) -> bool {
        match &self.set {
            None => true,
            Some(s) => s.contains(field),
        }
    }
}

fn build_include_filter(include: &[String]) -> IncludeFilter {
    if include.is_empty() {
        IncludeFilter { set: None }
    } else {
        IncludeFilter {
            set: Some(include.iter().map(|s| s.to_lowercase()).collect()),
        }
    }
}

/// Coarse mapping from tree-sitter `node_kind` to the ADR-0016 §2
/// `kind` enum (function | method | type | variable | constant |
/// unknown). The raw `node_kind` stays in each definition row for
/// callers that need finer detail.
fn classify_node_kind(node_kind: &str) -> &'static str {
    match node_kind {
        "function_declaration" | "function_item" | "function_definition" => "function",
        "method_declaration" | "method_item" => "method",
        "type_declaration" | "struct_item" | "enum_item" | "trait_item" | "type_spec" => "type",
        "var_declaration" | "let_declaration" | "short_var_declaration" => "variable",
        "const_declaration" | "const_spec" | "const_item" => "constant",
        _ => "unknown",
    }
}

// ---------------------------------------------------------------------------
// at_position — position → symbol_id translation (bead ley-line-open-c2e602,
// L2 of the agent-first surface decomp; ADR-0016 §1).
//
// Editor consumers have a cursor; agents have a name. ADR-0016 §1 picks
// symbol-keyed as the default and makes position-keyed an explicit
// translation hop. This op is that hop: (file, line, col) → smallest
// enclosing definition's token → (symbol_id, kind). The caller can
// then feed `symbol_id` into `inspect_symbol` and get the full bundle.
//
// Read-only — NOT in STATE_CHANGING_OPS.
// ---------------------------------------------------------------------------

/// `{"op":"at_position", "file":"...", "line":N, "col":N}` —
/// returns `{ ok, symbol_id, kind }` for the smallest enclosing
/// definition at the given position, or
/// `{ ok: true, symbol_id: null, kind: "unknown" }` when the
/// position lies inside no recognized definition. The null-symbol
/// shape is intentional: a position without a definition is a
/// legitimate query result, not an error.
fn op_at_position(ctx: &std::sync::Arc<DaemonContext>, p: &LspPosition) -> Result<String> {
    let file = normalize_file_uri(&p.file).to_string();
    let line = p.line;
    let col = p.col;
    ctx.with_read(|conn| {
        // Smallest enclosing definition at the position. Joining
        // node_defs against _ast pulls the symbol's token directly,
        // skipping the intermediate node_id resolution step that
        // `find_node_at_position` (the LSP-hover internal helper)
        // would otherwise force.
        let sql = "\
            SELECT d.token, a.node_kind \
            FROM node_defs d \
            JOIN _ast a ON a.node_id = d.node_id AND a.source_id = d.source_id \
            WHERE a.source_id = ?1 \
              AND a.start_row <= ?2 AND a.end_row >= ?2 \
              AND (a.start_row < ?2 OR a.start_col <= ?3) \
              AND (a.end_row > ?2 OR a.end_col >= ?3) \
            ORDER BY (a.end_byte - a.start_byte) ASC \
            LIMIT 1";
        let row: Option<(String, String)> =
            query_row_opt(conn, sql, rusqlite::params![file, line, col], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
        match row {
            Some((token, node_kind)) => Ok(json!({
                "ok": true,
                "symbol_id": token,
                "kind": classify_node_kind(&node_kind),
                "node_kind": node_kind,
            })
            .to_string()),
            None => Ok(json!({
                "ok": true,
                "symbol_id": serde_json::Value::Null,
                "kind": "unknown",
            })
            .to_string()),
        }
    })
}

// ---------------------------------------------------------------------------
// inspect_neighborhood — N-hop expansion (bead ley-line-open-c77690,
// L3 of the agent-first surface decomp; ADR-0016 §5).
//
// Returns the focal symbol's bundle plus truncated bundles for every
// symbol within `depth` hops via the callers/callees relation. The
// truncated shape includes `symbol_id`, `kind`, `definitions`, and
// `hop` (distance from focal); reachable downstream consumers can
// recurse via inspect_symbol if they need full bundles for any
// neighbor.
//
// v1 simplifications (each documented inline):
// - No `max_bytes` byte-cap — byte-counting + JSON-truncation is
//   complex and ADR-0016 §5's "per-distance truncation" maps cleanly
//   to the existing `kind`-only shape at deeper hops. Bounded fan-
//   out via `max_neighbors_per_hop` is enough for v1.
// - No `edge_kinds` filter — always traverses both callers AND
//   callees. Filtering one direction or specific edge types is a
//   follow-up bead.
// - Single round-trip (ADR-0016 §5 falsifiability `writes_for_
//   neighborhood_query == 1`): the entire neighborhood is built
//   inside one `with_live_db` closure and serialized once.
//
// Read-only — NOT in STATE_CHANGING_OPS.
// ---------------------------------------------------------------------------

/// Cap on `depth` regardless of caller input — ADR-0016 §5 says max 4.
const NEIGHBORHOOD_MAX_DEPTH: u32 = 4;

/// Cap on the TOTAL number of neighbor cells emitted per request. Depth
/// alone does not bound response size on co-occurrence-shaped graphs:
/// one observation with k mentions creates a k-clique, so a radius-1
/// ball can already be O(V). The response carries `truncated: true`
/// when this cap (or the per-hop soft cap) cut the expansion short, so
/// consumers can distinguish "small neighborhood" from "clipped
/// neighborhood". Bead `ley-line-open-504341` (P7b).
const NEIGHBORHOOD_MAX_CELLS: usize = 1_000;

/// `{"op":"inspect_neighborhood","symbol_id":"...",...}` — focal
/// symbol + N-hop neighborhood. Returns `{ok, focal, neighbors}`.
fn op_inspect_neighborhood(
    ctx: &std::sync::Arc<DaemonContext>,
    req: &InspectNeighborhoodRequest,
) -> Result<String> {
    let depth = req.depth.min(NEIGHBORHOOD_MAX_DEPTH);
    let per_hop = req.max_neighbors_per_hop as usize;

    ctx.with_read(|conn| {
        // Focal: full definitions for the requested symbol_id.
        let focal_defs = query_definitions(conn, &req.symbol_id)?;
        let focal_kind = focal_defs
            .first()
            .map(|d| classify_node_kind(&d.node_kind))
            .unwrap_or("unknown");

        // BFS over the callers/callees relation. `visited` tracks
        // symbol_ids we've already emitted so a high-degree symbol
        // doesn't appear at multiple hops; the FIRST hop at which a
        // neighbor is reached is the one recorded.
        let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
        visited.insert(req.symbol_id.clone());

        let mut neighbors_out: Vec<serde_json::Value> = Vec::new();
        let mut current_frontier: Vec<String> = vec![req.symbol_id.clone()];
        // Set when either cap (total cells, per-hop soft cap) cut the
        // expansion short — surfaced in the response so consumers can
        // tell a small neighborhood from a clipped one.
        let mut truncated = false;

        'hops: for hop in 1..=depth {
            let mut next_frontier: Vec<String> = Vec::new();

            for focal in &current_frontier {
                // Callees of the focal: definitions of every token
                // the focal references. Re-uses op_find_callees's SQL.
                let callee_defs = query_callees_with_token(conn, focal)?;
                // Callers of the focal: every site that references
                // the focal's token. Joins back to node_defs to get
                // the calling FUNCTION's token (caller-side aggregation).
                let caller_tokens = query_caller_tokens(conn, focal)?;

                for token in callee_defs
                    .into_iter()
                    .chain(caller_tokens)
                    .take(per_hop * 2)
                {
                    if !visited.insert(token.clone()) {
                        continue;
                    }
                    // Hard cap on emitted cells: hop-depth does not bound
                    // response size on clique-shaped co-occurrence graphs
                    // (bead ley-line-open-504341 P7b).
                    if neighbors_out.len() >= NEIGHBORHOOD_MAX_CELLS {
                        truncated = true;
                        break 'hops;
                    }
                    next_frontier.push(token.clone());

                    // Truncated bundle for this neighbor: just defs
                    // and kind, with provenance/certainty per L7.
                    let defs = query_definitions(conn, &token)?;
                    let kind = defs
                        .first()
                        .map(|d| classify_node_kind(&d.node_kind))
                        .unwrap_or("unknown");
                    let defs_json: Vec<serde_json::Value> = defs
                        .iter()
                        .map(|d| {
                            json!({
                                "node_id":    d.node_id,
                                "source_id":  d.source_id,
                                "node_kind":  d.node_kind,
                                "provenance": "tree-sitter",
                                "certainty":  "full",
                            })
                        })
                        .collect();

                    neighbors_out.push(json!({
                        "symbol_id":   token,
                        "kind":        kind,
                        "hop":         hop,
                        "definitions": defs_json,
                        "provenance":  "tree-sitter",
                        "certainty":   "full",
                    }));

                    if neighbors_out.len() >= per_hop * hop as usize {
                        // Soft cap per hop. The fan-out can be
                        // tighter than the request asked if many
                        // neighbors share dedup hits.
                        truncated = true;
                        break;
                    }
                }
            }

            current_frontier = next_frontier;
            if current_frontier.is_empty() {
                break; // No more nodes to expand.
            }
        }

        // Focal block: same shape as inspect_symbol minus the full
        // bundle. Consumers wanting hover/refs on the focal call
        // inspect_symbol directly with the same symbol_id.
        let focal_defs_json: Vec<serde_json::Value> = focal_defs
            .iter()
            .map(|d| {
                json!({
                    "node_id":    d.node_id,
                    "source_id":  d.source_id,
                    "node_kind":  d.node_kind,
                    "start_line": d.start_line,
                    "start_col":  d.start_col,
                    "end_line":   d.end_line,
                    "end_col":    d.end_col,
                    "start_byte": d.start_byte,
                    "end_byte":   d.end_byte,
                    "provenance": "tree-sitter",
                    "certainty":  "full",
                })
            })
            .collect();

        Ok(json!({
            "ok":         true,
            "symbol_id":  req.symbol_id,
            "depth":      depth,
            "focal": {
                "symbol_id":   req.symbol_id,
                "kind":        focal_kind,
                "definitions": focal_defs_json,
                "provenance":  "tree-sitter",
                "certainty":   "full",
            },
            "neighbors":  neighbors_out,
            // True when a cap (total-cell or per-hop) clipped the
            // expansion — a small `neighbors` list with truncated=false
            // really is the whole neighborhood.
            "truncated":  truncated,
            "provenance": "composed",
            "certainty":  "full",
        })
        .to_string())
    })
}

/// Variant of `query_callees` that returns the TOKENS of callees,
/// not the (node_id, source_id) pairs. Tokens are the symbol_ids the
/// neighborhood expansion uses as the next frontier.
fn query_callees_with_token(conn: &Connection, focal_token: &str) -> Result<Vec<String>> {
    // Two-step join: focal token → focal node_id (from node_defs) →
    // tokens this node references (from node_refs).
    let sql = "\
        SELECT DISTINCT r.token \
        FROM node_defs d \
        JOIN node_refs r ON r.node_id = d.node_id AND r.source_id = d.source_id \
        WHERE d.token = ?1 \
          AND r.token != ?1";
    let mut stmt = conn.prepare_cached(sql)?;
    let rows: Vec<String> = stmt
        .query_map([focal_token], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Tokens of functions that REFERENCE the focal symbol. Looks up
/// `node_refs` for the focal token to find the calling nodes, then
/// joins back to `node_defs` to get those nodes' defining tokens
/// (i.e. the enclosing functions' names).
fn query_caller_tokens(conn: &Connection, focal_token: &str) -> Result<Vec<String>> {
    let sql = "\
        SELECT DISTINCT d.token \
        FROM node_refs r \
        JOIN node_defs d ON d.node_id = r.node_id AND d.source_id = r.source_id \
        WHERE r.token = ?1 \
          AND d.token != ?1";
    let mut stmt = conn.prepare_cached(sql)?;
    let rows: Vec<String> = stmt
        .query_map([focal_token], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// ---------------------------------------------------------------------------
// search_symbols — GLOB-pattern symbol search streamed as NDJSON
// (bead ley-line-open-c79953, L4 of the agent-first surface decomp;
// ADR-0016 §6).
//
// Returns one JSON object per matched token, each terminated by `\n`.
// The whole response is the concatenation of N lines into one String —
// path (a) per the scout's design report. NDJSON consumers parse
// line-by-line either way, so the on-wire byte sequence is identical
// to a true chunked-streaming implementation (which is a follow-up
// bead: dispatch_typed → Stream<Item=String> through axum and the UDS
// framer). v1 satisfies the §6 line-delimited shape; it does NOT
// gate, which requires true streaming.
//
// Read-only — NOT in STATE_CHANGING_OPS.
// ---------------------------------------------------------------------------

/// `{"op":"search_symbols", "pattern":"Send*", "limit":100, "kind":"function"?}`
/// → NDJSON. Each matched row emits one line of the form
/// `{"symbol_id","node_id","source_id","kind","provenance":"tree-sitter","certainty":"full"}\n`.
/// Zero matches → empty string (NOT `"\n"`, NOT `"[]"`).
fn op_search_symbols(
    ctx: &std::sync::Arc<DaemonContext>,
    req: &SearchSymbolsRequest,
) -> Result<String> {
    // Empty pattern → zero matches by definition. Short-circuit
    // before touching the db so we don't ship a "GLOB ''" query.
    if req.pattern.is_empty() {
        return Ok(String::new());
    }
    let limit = req.limit.max(1) as i64;
    let kind_filter = req.kind.as_deref().map(|s| s.to_string());

    ctx.with_read(|conn| {
        // node_defs ⋈ _ast on (node_id, source_id). LEFT JOIN matches
        // query_definitions's shape so a token without an _ast row
        // still surfaces (node_kind falls back to ""). DISTINCT
        // collapses duplicate (token, node_id, source_id) tuples
        // that can arise from re-indexing.
        let sql = "\
            SELECT DISTINCT d.token, d.node_id, d.source_id, a.node_kind \
            FROM node_defs d \
            LEFT JOIN _ast a ON a.node_id = d.node_id AND a.source_id = d.source_id \
            WHERE d.token GLOB ?1 \
            ORDER BY d.token \
            LIMIT ?2";
        let mut stmt = conn.prepare_cached(sql)?;
        let rows = stmt.query_map(rusqlite::params![&req.pattern, limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?.unwrap_or_default(),
            ))
        })?;

        // Buffer one NDJSON line per matched row. The kind filter
        // is applied row-side (cheaper than another SQL predicate
        // against the raw `node_kind` variants — classify_node_kind
        // collapses N raw kinds into ~5 categories).
        let mut out = String::new();
        for row in rows {
            let (token, node_id, source_id, node_kind) = row?;
            let kind = classify_node_kind(&node_kind);
            if let Some(want) = kind_filter.as_deref()
                && kind != want
            {
                continue;
            }
            let line = json!({
                "symbol_id":  token,
                "node_id":    node_id,
                "source_id":  source_id,
                "kind":       kind,
                "provenance": "tree-sitter",
                "certainty":  "full",
            })
            .to_string();
            out.push_str(&line);
            out.push('\n');
        }
        Ok(out)
    })
}

// ---------------------------------------------------------------------------
// agreement — cross-source disagreement scoring (bead ley-line-open-c8090f,
// L10 of the agent-first surface decomp; ADR-0020 §3, Gate 3).
//
// Loads each source's observation for `(token, payload_kind)` from the
// `observation` table, decodes each `payload_inline` BLOB as a little-
// endian `Vec<f32>`, builds a degenerate `CellComplex` (one 0-cell per
// source, identity restriction maps over a single edge per disagreement
// pair), and runs `detect_violations`. The per-row violations become the
// op's `defects` field; the sum of squared margins becomes
// `coherence_defect` (the ADR-0020 §3 reserved name — NOT `δ⁰`, which
// is the sheaf-algebra operator's name).
// v1 simplifications (each documented inline):
// - Observation table is lazily created if missing. L8 (the table-
//   schema bead) is unshipped at L10 write time; this op pre-installs
//   the schema so callers can insert observations and immediately
//   query agreement against them. Once L8 lands the redundant CREATE
//   IF NOT EXISTS becomes a no-op.
// - `payload_inline` is interpreted as a little-endian f32 byte
//   sequence: every 4 bytes is one float, length must be divisible
//   by 4. This bypasses ley-line-open-503971's capnp typed-payload
//   registry (also unshipped) for L10; the v1 encoder is "raw f32s".
//   When the registry ships, a `decode_payload(payload_kind, bytes)`
//   helper replaces this branch without touching the op's algebra.
// - `payload_hash` (BlobStore lookup for >`INLINE_THRESHOLD`
//   payloads, ADR-0020 §1) is NOT followed. v1 only reads
//   `payload_inline`. Falling back to BlobStore is a follow-up bead.
// - No filter on observation time / source / window. Every matching
//   row participates.
// - Pairwise complex (vs. a star): for N sources we emit N-1 edges
//   (source 0 ↔ source 1, source 0 ↔ source 2, …). Star around the
//   first source is sufficient to catch any pair where source_i
//   disagrees with source_0; a fully-pairwise variant is a follow-up
//   if the agent surface ever needs the dense matrix.
//
// Read-only — NOT in STATE_CHANGING_OPS.
// ---------------------------------------------------------------------------

/// One observation row decoded for agreement scoring: the source name
/// plus the decoded f32 stalk vector. Mirrors the per-source 0-cell
/// the agreement op feeds into the degenerate `CellComplex`.
#[derive(Debug, Clone)]
struct AgreementRow {
    source: String,
    stalk: Vec<f32>,
}

/// Fetch the latest observation per source for `(token, payload_kind)`.
/// Returns at most one row per `source` (the freshest by `observed_at`).
/// The token filter uses ADR-0020's `observation_by_mentions` index
/// via `json_each` — same shape the future `agreement` Gate 4 property
/// test will exercise.
fn query_agreement_observations(
    conn: &Connection,
    token: &str,
    payload_kind: &str,
) -> Result<Vec<AgreementRow>> {
    // Caller must ensure the schema exists via `create_observation_schema`
    // before calling this function — see `op_agreement`'s two-phase
    // pattern (bead `ley-line-open-f0239d`). The read pool's
    // `query_only=ON` pragma rejects the CREATE TABLE inline; the
    // installer must run through `with_write`.

    // Latest observation per source for this `(token, payload_kind)`.
    // SQLite's "bare-column" aggregation rule (since 3.7.11) means
    // `MAX(observed_at)` plus other bare columns returns those columns
    // from the row that produced the max — exactly the "freshest per
    // source" projection we want without a subquery dance.
    let sql = "\
        SELECT o.source, o.payload_inline, MAX(o.observed_at) AS latest \
        FROM observation o \
        WHERE o.payload_kind = ?1 \
          AND EXISTS ( \
              SELECT 1 FROM json_each(o.mentions) je \
              WHERE je.value = ?2 \
          ) \
          AND o.payload_inline IS NOT NULL \
        GROUP BY o.source \
        ORDER BY o.source ASC";

    let mut stmt = conn.prepare_cached(sql)?;
    let rows: Vec<AgreementRow> = stmt
        .query_map([payload_kind, token], |r| {
            let source: String = r.get(0)?;
            let payload: Vec<u8> = r.get(1)?;
            Ok((source, payload))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|(source, bytes)| {
            decode_inline_payload(&bytes).map(|stalk| AgreementRow { source, stalk })
        })
        .collect();

    Ok(rows)
}

/// Decode a `payload_inline` BLOB as a little-endian `Vec<f32>`. Returns
/// `None` if the byte length is not divisible by 4 (the raw-f32 v1
/// encoder rejects fragmented payloads cleanly so a malformed row drops
/// out instead of crashing the op). When the typed-payload registry
/// (ley-line-open-503971) ships, this is the single point that learns
/// to dispatch by `payload_kind`.
fn decode_inline_payload(bytes: &[u8]) -> Option<Vec<f32>> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.as_chunks::<4>().0 {
        out.push(f32::from_le_bytes(*chunk));
    }
    Some(out)
}

/// Build the degenerate `CellComplex` ADR-0020 §3 specifies:
/// one 0-cell per source, identity restriction maps, and edges between
/// every distinct pair of sources (a star around the first source —
/// sufficient to surface any disagreement against source 0; a fully-
/// pairwise variant would be O(N²) edges and isn't needed for the v1
/// agreement op).
///
/// All stalks MUST share the same dimension — the caller is responsible
/// for filtering rows that don't match. The function returns `None` only
/// when there are zero rows or every row has an empty stalk — i.e.
/// nothing left to compute over. Dimension mismatches MUST be caught
/// by the caller via [`classify_agreement_dims`] BEFORE this is invoked;
/// see bead `ley-line-open-659a39` (math-friend HIGH): silently returning
/// `None` on mismatched dims yielded a `{cd=0, defects=[]}` response
/// indistinguishable from "all sources agree."
/// Floor of the star-edge id space in the agreement complex's shared
/// `cells` keyspace. Node ids are `0..rows.len()` (one per source), edge
/// ids are `AGREEMENT_EDGE_BASE + target` — so the partition holds only
/// while `rows.len() <= AGREEMENT_EDGE_BASE`. `op_agreement` enforces the
/// bound with an explicit error envelope BEFORE building (silently
/// corrupted incidence is wire-indistinguishable from a valid answer);
/// `build_agreement_complex` asserts it as a backstop. Bead
/// `ley-line-open-4fece1`.
const AGREEMENT_EDGE_BASE: u32 = 1_000;

fn build_agreement_complex(
    rows: &[AgreementRow],
) -> Option<(leyline_sheaf::complex::CellComplex, Vec<String>)> {
    use leyline_sheaf::complex::{CellComplex, RestrictionMap};

    assert!(
        rows.len() <= AGREEMENT_EDGE_BASE as usize,
        "build_agreement_complex: {} sources would collide node ids with \
         the star-edge id space (AGREEMENT_EDGE_BASE = {}); the caller must \
         reject oversized source sets first (bead ley-line-open-4fece1)",
        rows.len(),
        AGREEMENT_EDGE_BASE,
    );

    let first = rows.first()?;
    let stalk_dim = first.stalk.len();
    if stalk_dim == 0 {
        return None;
    }
    debug_assert!(
        rows.iter().all(|r| r.stalk.len() == stalk_dim),
        "build_agreement_complex called with mixed stalk dims; caller must \
         classify_agreement_dims first (bead ley-line-open-659a39)",
    );

    let mut cx = CellComplex::new(stalk_dim);
    let mut sources_by_node: Vec<String> = Vec::with_capacity(rows.len());
    for (idx, row) in rows.iter().enumerate() {
        cx.add_node(idx as u32, row.stalk.clone());
        sources_by_node.push(row.source.clone());
    }

    // Star edges around source 0. Edge IDs live in the same `cells`
    // namespace as nodes (see `add_edge`); the AGREEMENT_EDGE_BASE offset
    // keeps them clear of the 0..N node IDs (bound asserted above)
    // without colliding with the cache layer's `EDGE_ID_BASE = 1_000_000`.
    for target in 1..rows.len() as u32 {
        cx.add_edge(
            AGREEMENT_EDGE_BASE + target,
            0,
            target,
            stalk_dim,
            Some("agreement".to_string()),
            RestrictionMap::identity(stalk_dim),
            RestrictionMap::identity(stalk_dim),
            false,
        );
    }

    Some((cx, sources_by_node))
}

/// Classify the stalk-dimensionality of a row set. Returns
/// `Ok(Some(common_dim))` when every row shares the same non-zero dim,
/// `Ok(None)` when there are no rows / every stalk is empty (no algebra
/// possible — empty defects is a truthful answer), or
/// `Err((source, dim, other_source, other_dim))` when two sources
/// disagree on stalk dim — the load-bearing case ADR-0020 §3 is built
/// for, per math-friend bead `ley-line-open-659a39`. The op surfaces
/// the mismatch as an explicit error envelope rather than coercing to
/// an empty-defects success.
fn classify_agreement_dims(
    rows: &[AgreementRow],
) -> std::result::Result<Option<usize>, (String, usize, String, usize)> {
    let mut anchor: Option<(&str, usize)> = None;
    for row in rows {
        let dim = row.stalk.len();
        if dim == 0 {
            continue;
        }
        match anchor {
            None => anchor = Some((row.source.as_str(), dim)),
            Some((src, expected)) if expected != dim => {
                return Err((src.to_string(), expected, row.source.clone(), dim));
            }
            Some(_) => {}
        }
    }
    Ok(anchor.map(|(_, d)| d))
}

/// `{"op":"agreement","token":"...","payload_kind":"..."}` — returns
/// `{ok, token, payload_kind, coherence_defect, defects, source_count,
/// provenance, certainty}`. Builds a degenerate CellComplex from the
/// most-recent `payload_inline` per source and reports
/// `detect_violations`. Read-only.
fn op_agreement(ctx: &std::sync::Arc<DaemonContext>, req: &AgreementRequest) -> Result<String> {
    // Lazy schema install for edge-case callers (test fixtures / fresh
    // daemons) — production ensures the schema via
    // `session_observation_pass`. Uses `with_write` because the schema
    // install is DDL; the read pool's `query_only=ON` pragma would
    // reject the CREATE TABLE otherwise (bead
    // `ley-line-open-f0239d`). If the schema already exists,
    // `create_observation_schema` is a no-op — writer mutex is held
    // for a nanosecond.
    ctx.with_write(crate::daemon::observation_schema::create_observation_schema)?;
    ctx.with_read(|conn| {
        let rows = query_agreement_observations(conn, &req.token, &req.payload_kind)?;
        let source_count = rows.len();

        // Guard the node/edge id-space partition of the agreement complex
        // (bead ley-line-open-4fece1): ≥ AGREEMENT_EDGE_BASE distinct
        // sources for one (token, payload_kind) would silently collide
        // node ids with star-edge ids. Explicit error envelope, same
        // policy as the dim-mismatch case below — user data must not
        // reach the library-level assert.
        if source_count > AGREEMENT_EDGE_BASE as usize {
            return Ok(json!({
                "ok":           false,
                "error":        "too_many_sources",
                "token":        req.token,
                "payload_kind": req.payload_kind,
                "source_count": source_count,
                "detail": format!(
                    "{source_count} sources exceeds the agreement complex's \
                     id-space bound ({AGREEMENT_EDGE_BASE}); narrow the \
                     observation set for this (token, payload_kind)."
                ),
            })
            .to_string());
        }

        // Pre-classify stalk dims. Mismatch → explicit error envelope
        // (bead `ley-line-open-659a39`): silently returning empty defects
        // is wire-indistinguishable from "sources agree" and exactly
        // wrong for ADR-0020 §3's heterogeneous-observer case.
        let common_dim = match classify_agreement_dims(&rows) {
            Ok(d) => d,
            Err((src_a, dim_a, src_b, dim_b)) => {
                return Ok(json!({
                    "ok":             false,
                    "error":          "incompatible_stalk_dims",
                    "token":          req.token,
                    "payload_kind":   req.payload_kind,
                    "source_count":   source_count,
                    "detail": format!(
                        "source {src_a} stalk has dim {dim_a}; source {src_b} stalk has dim {dim_b}. \
                         Agreement requires homogeneous stalk dims under V1 identity restrictions."
                    ),
                })
                .to_string());
            }
        };

        // Build the complex (if possible) and run detect_violations.
        // The single-source / empty cases short-circuit to an empty
        // defects array — the op still returns OK because "no sources
        // disagree" is a valid answer.
        let _ = common_dim;
        let (defects_json, coherence_defect): (Vec<serde_json::Value>, f32) =
            if let Some((cx, sources_by_node)) = build_agreement_complex(&rows) {
                // Mechanical-reach: this is the load-bearing call into
                // `leyline-sheaf`. The Gate 3 spy increments here.
                let violations = cx.detect_violations();
                let mut total = 0.0_f32;
                let defects: Vec<serde_json::Value> = violations
                    .iter()
                    .map(|v| {
                        total += v.margin * v.margin;
                        // Recover the (source_a, source_b) pair from
                        // the edge's incidence — same mapping the
                        // complex builder installed (node 0 ↔ node k
                        // for star edges around source 0).
                        let (src_a, src_b) = cx
                            .incidence
                            .get(&v.edge_id)
                            .map(|&(a, b)| (a as usize, b as usize))
                            .unwrap_or((0, 0));
                        let source_a = sources_by_node.get(src_a).cloned().unwrap_or_default();
                        let source_b = sources_by_node.get(src_b).cloned().unwrap_or_default();
                        json!({
                            "source_a":        source_a,
                            "source_b":        source_b,
                            "edge_id":         v.edge_id,
                            "dimension_index": v.dimension_index,
                            "margin":          v.margin,
                            "severity":        v.margin * v.margin,
                            // L7-style provenance per dimension: the
                            // defect is derived from sheaf algebra over
                            // observations from multiple sources.
                            "provenance":      "sheaf",
                            "certainty":       "full",
                        })
                    })
                    .collect();
                (defects, total)
            } else {
                (Vec::new(), 0.0)
            };

        Ok(json!({
            "ok":               true,
            "token":            req.token,
            "payload_kind":     req.payload_kind,
            "source_count":     source_count,
            "coherence_defect": coherence_defect,
            "defects":          defects_json,
            // Top-level provenance "composed" because the op merges
            // multiple observations from different sources; certainty
            // "full" because the algebra is deterministic given the
            // observation rows.
            "provenance":       "composed",
            "certainty":        "full",
        })
        .to_string())
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::RwLock;
    use std::sync::Arc;
    use tempfile::TempDir;

    // ── Helper unit tests ───────────────────────────────────────────────

    fn normalize_file_uri_strips_prefix() {
        assert_eq!(normalize_file_uri("file:///abs/foo.rs"), "/abs/foo.rs");
    }

    /// ADR-0026 Phase 2.0 (bead `ley-line-open-335d34`): pin the
    /// `LEYLINE_PROFILE=1` gate behavior. When the env var is unset
    /// the timer's `enabled` flag must be false — otherwise every
    /// `op_query` / `list_children` / `find_callers` / `find_defs`
    /// call would emit a stderr line in production. This is a
    /// zero-tolerance regression pin: a refactor that flipped the
    /// default would spam every daemon's stderr.
    #[test]
    fn read_profile_timer_defaults_to_disabled() {
        // Guard the env-var probe against test-order noise:
        // remove_var is what daemon-under-test does when unset, and
        // matches the production shell where LEYLINE_PROFILE isn't
        // exported.
        // SAFETY: single-threaded test with no crossing threads
        // touching the env; set_var/remove_var are `unsafe` in Rust
        // 1.83+ under the same rules.
        // We restore the prior value on drop so parallel tests aren't
        // affected.
        struct EnvGuard {
            key: &'static str,
            prev: Option<String>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                // SAFETY: same-thread scope.
                unsafe {
                    match self.prev.take() {
                        Some(v) => std::env::set_var(self.key, v),
                        None => std::env::remove_var(self.key),
                    }
                }
            }
        }
        let prev = std::env::var("LEYLINE_PROFILE").ok();
        // SAFETY: same-thread scope; guard restores on drop.
        unsafe {
            std::env::remove_var("LEYLINE_PROFILE");
        }
        let _g = EnvGuard {
            key: "LEYLINE_PROFILE",
            prev,
        };
        let t = ReadProfileTimer::new("test_shape");
        assert!(
            !t.enabled,
            "ReadProfileTimer MUST default to disabled when LEYLINE_PROFILE is unset",
        );
        // Explicit drop — verifies the disabled-path Drop doesn't
        // panic (it short-circuits the format).
        drop(t);
    }

    #[test]
    fn normalize_file_uri_passes_through_plain_path() {
        // Already-relative paths and bare paths come through untouched.
        assert_eq!(normalize_file_uri("src/foo.rs"), "src/foo.rs");
        assert_eq!(normalize_file_uri("/abs/foo.rs"), "/abs/foo.rs");
        assert_eq!(normalize_file_uri(""), "");
    }

    #[test]
    fn normalize_file_uri_only_strips_one_prefix() {
        // Defensive: avoid eating extra slashes if the caller has already
        // stripped once. The strip is exact, not greedy.
        assert_eq!(normalize_file_uri("file://file:///x"), "file:///x");
    }

    #[test]
    fn node_id_for_file_clause_shape() {
        // Sanity-check the SQL fragment hasn't drifted from the bind index.
        // If this fragment ever needs ?2 or a different column the call sites
        // must be updated in lockstep.
        assert!(NODE_ID_FOR_FILE.contains("?1"));
        assert!(NODE_ID_FOR_FILE.starts_with("node_id"));
    }

    #[test]
    fn state_changing_ops_pin_known_set() {
        // Bidirectional drift guard: the canonical set must be exactly
        // the hardcoded list, in some order. Iterating the hardcoded
        // list (the previous form) only caught *removal* — adding a
        // new op to STATE_CHANGING_OPS without updating the test
        // would silently pass and a state-changing op would silently
        // emit no event. Equality assertion fails in both directions.
        // (Caught by iter-35 adversarial review.)
        let mut actual: Vec<&str> = STATE_CHANGING_OPS.to_vec();
        actual.sort();
        let expected: Vec<&str> = {
            let mut v = vec!["load", "reparse", "flush", "snapshot", "enrich"];
            v.sort();
            v
        };
        assert_eq!(
            actual, expected,
            "STATE_CHANGING_OPS drift detected — update the hardcoded list \
             (and the matching test) when adding/removing mutating ops",
        );
    }

    #[test]
    fn state_changing_ops_excludes_pure_reads() {
        // The query/observation ops must NOT trigger an event emission.
        // When a new read-only op is added to `handle_base_op`, list it
        // here so a future accidental promotion into STATE_CHANGING_OPS
        // is caught by this guard.
        for op in [
            "status",
            "query",
            "list_children",
            "list_roots",
            "read_content",
            "find_callers",
            "find_callees",
            "find_defs",
            "get_node",
            "get_refs_map",
            "get_defs_map",
            "get_schema",
            "get_db_path",
            "lsp_hover",
            "lsp_defs",
            "lsp_refs",
            "lsp_symbols",
            "lsp_diagnostics",
            #[cfg(feature = "validate")]
            "validate",
            #[cfg(feature = "hdc")]
            "hdc_search",
            #[cfg(feature = "hdc")]
            "hdc_calibrate",
            #[cfg(feature = "hdc")]
            "hdc_density",
            "inspect_symbol",
            "at_position",
            "inspect_neighborhood",
            "search_symbols",
            "agreement",
        ] {
            assert!(
                !is_state_changing(op),
                "read-only op `{op}` should not be state-changing",
            );
        }
    }

    #[test]
    fn state_changing_ops_unknown_returns_false() {
        // Defensive: an op that doesn't exist in the dispatch table must
        // not be called state-changing (avoids spurious events).
        assert!(!is_state_changing("nonexistent_op"));
        assert!(!is_state_changing(""));
    }

    #[tokio::test]
    async fn op_find_token_preserves_caller_supplied_json_key() {
        // Wire contract: op_find_callers must return rows under "callers",
        // op_find_defs under "defs". Clients (mache, hooks) parse the
        // specific key — a refactor that swapped them silently would
        // break clients without any test failing. Pin the dispatch
        // direction explicitly. (Caught by iter-35 adversarial review.)
        // setup() already creates the node_refs/node_defs tables via
        // create_refs_schema; we just need empty tables for the shape
        // test. handle_base_op routes find_callers → node_refs and
        // find_defs → node_defs.
        let (_dir, ctx) = setup();
        let callers = handle_base_op_legacy(&ctx, "find_callers", &json!({"token": "x"})).unwrap();
        let defs = handle_base_op_legacy(&ctx, "find_defs", &json!({"token": "x"})).unwrap();
        let callers_v: serde_json::Value = serde_json::from_str(&callers).unwrap();
        let defs_v: serde_json::Value = serde_json::from_str(&defs).unwrap();
        assert!(
            callers_v.get("callers").is_some(),
            "find_callers must use \"callers\" key; got {callers_v}",
        );
        assert!(
            defs_v.get("defs").is_some(),
            "find_defs must use \"defs\" key; got {defs_v}",
        );
        assert!(
            callers_v.get("defs").is_none() && defs_v.get("callers").is_none(),
            "keys must not cross-pollinate",
        );
    }

    #[test]
    fn query_token_refs_returns_matching_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE node_refs (token TEXT, node_id TEXT, source_id TEXT);
             INSERT INTO node_refs VALUES
               ('foo', 'a/x', 'a.go'),
               ('foo', 'b/y', 'b.go'),
               ('bar', 'c/z', 'c.go');",
        )
        .unwrap();

        let rows = query_token_refs(&conn, "foo", "node_refs").unwrap();
        assert_eq!(rows.len(), 2);
        let ids: std::collections::HashSet<&str> =
            rows.iter().map(|r| r.node_id.as_str()).collect();
        assert!(ids.contains("a/x"));
        assert!(ids.contains("b/y"));

        let none = query_token_refs(&conn, "missing", "node_refs").unwrap();
        assert!(none.is_empty());
    }

    /// Build the `CREATE TABLE` statement for an LSP 5-col position
    /// table (`_lsp_defs` with `def_*` columns, or `_lsp_refs` with
    /// `ref_*`). Replaces two byte-similar CREATE statements that
    /// only differed in their column-prefix substring.
    fn lsp_5col_create_sql(table: &str, prefix: &str) -> String {
        format!(
            "CREATE TABLE {table} (
                node_id TEXT,
                {prefix}_uri TEXT,
                {prefix}_start_line INTEGER,
                {prefix}_start_col INTEGER,
                {prefix}_end_line INTEGER,
                {prefix}_end_col INTEGER
            );"
        )
    }

    #[test]
    fn lsp_5col_position_rows_returns_empty_when_table_missing() {
        // Pre-enrichment state: callers must get an empty vec, not an
        // error. This is the signal that lazy enrichment should fire.
        let conn = Connection::open_in_memory().unwrap();
        let rows = lsp_5col_position_rows(&conn, "any/node", "_lsp_defs", "def").unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn lsp_5col_position_rows_decodes_def_shape() {
        let conn = Connection::open_in_memory().unwrap();
        let create = lsp_5col_create_sql("_lsp_defs", "def");
        conn.execute_batch(&format!(
            "{create}
             INSERT INTO _lsp_defs VALUES
               ('foo/main', 'file:///foo.rs', 10, 4, 12, 0),
               ('bar/baz', 'file:///bar.rs', 1, 0, 1, 8);"
        ))
        .unwrap();

        let rows = lsp_5col_position_rows(&conn, "foo/main", "_lsp_defs", "def").unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r["uri"], "file:///foo.rs");
        assert_eq!(r["start_line"], 10);
        assert_eq!(r["start_col"], 4);
        assert_eq!(r["end_line"], 12);
        assert_eq!(r["end_col"], 0);
    }

    #[test]
    fn lsp_5col_position_rows_handles_ref_prefix() {
        // The same helper services _lsp_refs with a different col prefix.
        let conn = Connection::open_in_memory().unwrap();
        let create = lsp_5col_create_sql("_lsp_refs", "ref");
        conn.execute_batch(&format!(
            "{create}
             INSERT INTO _lsp_refs VALUES ('x/y', 'file:///z.rs', 5, 2, 5, 7);"
        ))
        .unwrap();

        let rows = lsp_5col_position_rows(&conn, "x/y", "_lsp_refs", "ref").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["uri"], "file:///z.rs");
        assert_eq!(rows[0]["start_line"], 5);
    }

    /// Test-helper: dispatch `op` with `req` and assert the response
    /// is a JSON object containing an `error` field. Used by the
    /// input-validation pin triplet (op_load, op_query, op_reparse)
    /// which all share the same expected error-shape contract.
    fn assert_op_errors(
        ctx: &std::sync::Arc<DaemonContext>,
        op: &str,
        req: serde_json::Value,
        why: &str,
    ) {
        let resp = handle_base_op_legacy(ctx, op, &req)
            .unwrap_or_else(|| panic!("op {op} returned None for {why}"));
        let parsed: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert!(
            parsed.get("error").is_some(),
            "{op}: {why} should error; got {parsed}",
        );
    }

    fn setup() -> (TempDir, std::sync::Arc<DaemonContext>) {
        let dir = TempDir::new().unwrap();
        let arena_path = dir.path().join("test.arena");
        let ctrl_path = dir.path().join("test.ctrl");
        let _mmap = leyline_core::create_arena(&arena_path, 2 * 1024 * 1024).unwrap();
        let mut ctrl = leyline_core::Controller::open_or_create(&ctrl_path).unwrap();
        ctrl.set_arena(&arena_path.to_string_lossy(), 2 * 1024 * 1024)
            .unwrap();

        // Create a file-backed WAL living db with the nodes schema. The
        // pool needs a real file to attach to (bead
        // `ley-line-open-f0239d`); `:memory:` connections can't be
        // shared across the pool.
        let live_db_path = ctrl_path.with_extension("live.db");
        let conn = Connection::open(&live_db_path).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        conn.pragma_update(None, "synchronous", "NORMAL").unwrap();
        leyline_ts::schema::create_ast_schema(&conn).unwrap();
        leyline_ts::schema::create_refs_schema(&conn).unwrap();
        let live_db = crate::daemon::db_pool::LiveDb::new(conn, &live_db_path, 4).unwrap();

        #[cfg(feature = "vec")]
        let vec_index = {
            crate::daemon::vec_index::register_vec();
            Arc::new(crate::daemon::vec_index::VectorIndex::new(4, None).unwrap())
        };
        #[cfg(feature = "vec")]
        let embedder: Arc<dyn crate::daemon::embed::Embedder> =
            Arc::new(crate::daemon::embed::ZeroEmbedder { dim: 4 });
        let ctx = DaemonContext {
            ctrl_path,
            ext: Arc::new(crate::daemon::NoExt),
            router: crate::daemon::EventRouter::new(16),
            live_db,
            enrich_inflight: Arc::new(parking_lot::Mutex::new(std::collections::HashSet::new())),
            source_dir: None,
            lang_filter: None,
            enrichment_passes: vec![],
            state: Arc::new(RwLock::new(crate::daemon::DaemonState::initializing())),
            #[cfg(feature = "vec")]
            vec_index,
            #[cfg(feature = "vec")]
            embedder,
            #[cfg(feature = "vec")]
            embed_queue: Arc::new(parking_lot::Mutex::new(std::collections::BinaryHeap::new())),
            #[cfg(feature = "text-search")]
            text_search: Arc::new(leyline_text_search::null::NullEngine::new()),
            sheaf: Arc::new(crate::daemon::sheaf_ops::SheafState::new()),
        };
        (dir, std::sync::Arc::new(ctx))
    }

    /// 5f7100-4 / 606e64: regression pin for the self-deadlock fix.
    ///
    /// Pre-fix, `with_lazy_enrich_retry` called `maybe_enrich` from
    /// inside `with_live_db`, which held the parking_lot::Mutex while
    /// `try_enrich_file` tried to re-acquire it — same-thread
    /// deadlock on a non-reentrant Mutex. Without the fix, this test
    /// would hang forever.
    ///
    /// We use a context with `source_dir: None`, which makes
    /// `try_enrich_file` return false immediately without spawning an
    /// LSP server. The deadlock would have triggered on the lock-
    /// reentry attempt, BEFORE the source_dir check, so this is a
    /// valid regression pin without needing a real LSP environment.
    #[tokio::test]
    async fn with_lazy_enrich_retry_does_not_self_deadlock() {
        let (_dir, ctx) = setup();
        // The setup connection has no _lsp table → needs_enrich = true.
        // try_enrich_file with source_dir=None returns false. Pre-fix
        // would have deadlocked between needs_enrich + try_enrich_file
        // because both acquired the live_db lock under the same
        // closure.

        // Run on a separate thread + std::sync::mpsc so a real
        // deadlock would manifest as a timeout rather than freezing
        // the test harness.
        let ctx_arc = std::sync::Arc::new(ctx);
        let ctx_clone = ctx_arc.clone();
        let (tx, rx) = std::sync::mpsc::channel::<Result<(Option<String>, bool)>>();
        let handle = std::thread::spawn(move || {
            let result = with_lazy_enrich_retry(
                &ctx_clone,
                "deadlock-pin.go",
                |_conn| Ok::<Option<String>, anyhow::Error>(None),
                |opt| opt.is_none(),
            );
            let _ = tx.send(result);
        });
        let outcome = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("with_lazy_enrich_retry timed out — DEADLOCK regression");
        handle.join().unwrap();

        let (result, enriched) = outcome.unwrap();
        assert_eq!(
            result, None,
            "no enrichment ran (no source_dir), result stays None"
        );
        assert!(!enriched, "no enrichment ran, enriched flag must be false");
    }

    /// 5f7100-4 / 606e64: per-file in-flight gate dedupes concurrent
    /// `try_enrich_file` calls. The first call inserts into the set
    /// and runs work; subsequent callers for the same file see the
    /// entry, return false, and skip the spawn.
    ///
    /// This test exercises the gate directly via the inflight set —
    /// no actual LSP work needed (which would require a live source_dir
    /// and language servers). The contract under test: the second
    /// concurrent insert returns false.
    #[tokio::test]
    async fn enrich_inflight_gate_dedupes_concurrent_callers() {
        let (_dir, ctx) = setup();

        // First caller: insert "foo.go".
        let first = ctx.enrich_inflight.lock().insert("foo.go".to_string());
        assert!(first, "first caller must succeed in inserting");

        // Second caller (concurrent simulation): same file already in
        // set; insert returns false → caller skips enrichment.
        let second = ctx.enrich_inflight.lock().insert("foo.go".to_string());
        assert!(
            !second,
            "second concurrent caller must observe inflight, return false"
        );

        // Different file: still allowed.
        let other = ctx.enrich_inflight.lock().insert("bar.go".to_string());
        assert!(other, "different file must be allowed concurrently");
    }

    /// 5f7100-4 / 606e64: gate releases after work completes. Pin so
    /// a future refactor that forgot to remove from the set on success
    /// would surface here as the second call returning false.
    #[tokio::test]
    async fn enrich_inflight_gate_releases_after_work() {
        let (_dir, ctx) = setup();

        // Simulate work cycle: insert + remove (the InflightGuard's
        // Drop does this automatically in production try_enrich_file;
        // we exercise it manually here).
        ctx.enrich_inflight.lock().insert("foo.go".to_string());
        ctx.enrich_inflight.lock().remove("foo.go");

        // Subsequent call: insert succeeds (set is clean).
        let again = ctx.enrich_inflight.lock().insert("foo.go".to_string());
        assert!(again, "after release, file is allowed back into set");
    }

    /// 5f7100-4 / 606e64: try_enrich_file's RAII guard removes the
    /// inflight entry on EARLY return (e.g. enrichment errors out).
    /// Without the guard, an error would leak the file into the set
    /// forever, blocking all future enrichment attempts on that file.
    /// The function's setup() context has no source_dir, so
    /// try_enrich_file returns false on its own check — but the gate
    /// inserts BEFORE that check, so we need to verify cleanup.
    #[tokio::test]
    async fn enrich_inflight_gate_releases_on_no_source_dir() {
        let (_dir, ctx) = setup();
        assert!(
            ctx.source_dir.is_none(),
            "fixture: source_dir must be None for this test",
        );

        // Pre-condition: set is empty.
        assert_eq!(
            ctx.enrich_inflight.lock().len(),
            0,
            "fresh inflight set should be empty",
        );

        // Call try_enrich_file: with no source_dir, it returns false
        // BEFORE doing any LSP work. But the gate insert + RAII guard
        // run regardless. After return, the set must be empty.
        let result = try_enrich_file(&ctx, "foo.go");
        assert!(!result, "no source_dir → try_enrich_file returns false");

        assert_eq!(
            ctx.enrich_inflight.lock().len(),
            0,
            "inflight set must be empty after try_enrich_file returns; \
             RAII guard cleans up even on early-return paths",
        );
    }

    /// 60f75d-7a (off-UDS-thread): try_enrich_file must return
    /// IMMEDIATELY without blocking on the LSP work. Pre-7a, this
    /// function held the live_db lock and ran enrichment::run_pass
    /// synchronously — UDS connection thread blocked for 1-5s while
    /// gopls/rust-analyzer indexed. Post-7a: the work is queued via
    /// `tokio::task::spawn_blocking` and the function returns within
    /// microseconds. Pin so a refactor that re-introduces synchronous
    /// run_pass calls would surface here as the test exceeding the
    /// 100ms threshold.
    ///
    /// We can't easily test the spawned-task completion in a unit test
    /// (would need a real LSP server). What we CAN test: the call
    /// returns fast even when the gate is held by another thread —
    /// proving the work is async, not sync.
    #[tokio::test]
    async fn try_enrich_file_returns_synchronously_within_budget() {
        let (_dir, ctx) = setup();

        let start = std::time::Instant::now();
        let _ = try_enrich_file(&ctx, "foo.go");
        let elapsed = start.elapsed();

        // 100ms is the budget the spawn-based path should fit in
        // easily — actual work is microseconds (gate insert + spawn
        // call). Pre-7a: this would block for 1-5s on real enrichment;
        // even with no source_dir, a refactor that re-introduced sync
        // run_pass would degrade here.
        assert!(
            elapsed < std::time::Duration::from_millis(100),
            "try_enrich_file took {elapsed:?} — must return within 100ms \
             (60f75d-7a: work is queued to blocking pool, NOT run on caller thread)",
        );
    }

    #[tokio::test]
    async fn test_op_status_returns_zero_root_for_fresh_controller() {
        // T2.4: op_status emits `current_root` (hex) — not `generation`.
        // A fresh controller with no arena published yields the
        // 64-char zero sentinel.
        let (_dir, ctx) = setup();
        let result = handle_base_op_legacy(&ctx, "status", &json!({}));
        assert!(result.is_some());
        let parsed: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["current_root"], "0".repeat(64));
    }

    #[tokio::test]
    async fn op_reparse_errors_when_source_neither_field_nor_ctx() {
        // op_reparse pulls `source` from req or falls back to
        // ctx.source_dir. When neither is set, it must surface an
        // actionable error. setup() builds ctx with source_dir: None
        // so this test exercises the missing-everything fallthrough.
        let (_dir, ctx) = setup();
        assert_op_errors(
            &ctx,
            "reparse",
            json!({}),
            "missing source + no ctx fallback",
        );
    }

    #[tokio::test]
    async fn op_query_errors_on_missing_or_invalid_sql() {
        // Input-validation triplet: op_query is the ad-hoc inspection
        // escape hatch. At scale a misconfigured client would
        // otherwise see panic / hang on a large registry db. Pin all
        // three failure modes via the shared assert_op_errors helper.
        let (_dir, ctx) = setup();
        assert_op_errors(&ctx, "query", json!({}), "missing sql");
        assert_op_errors(&ctx, "query", json!({"sql": 42}), "non-string sql");
        assert_op_errors(
            &ctx,
            "query",
            json!({"sql": "SELECT garbage FROM nowhere WHERE x SYNTAX_ERROR"}),
            "invalid sql",
        );
    }

    #[tokio::test]
    async fn op_query_caps_rows_at_default_limit() {
        // Scale-guard pin: op_query's default row cap protects clients
        // from accidental `SELECT * FROM nodes` on a 629k-row registry
        // db (which would serialize hundreds of MB into one JSON
        // response and lock the daemon while doing it).
        //
        // Insert a probe table with > OP_QUERY_DEFAULT_ROW_LIMIT rows;
        // run an unbounded SELECT * via op_query; assert:
        //   - rows length == default cap
        //   - truncated: true is set
        //   - limit field reports the cap that was applied
        let (_dir, ctx) = setup();
        {
            let conn = ctx.live_db.writer.lock();
            conn.execute(
                "CREATE TABLE probe (id INTEGER PRIMARY KEY, payload TEXT)",
                [],
            )
            .unwrap();
            // Use a single batched insert via VALUES for speed.
            let n = OP_QUERY_DEFAULT_ROW_LIMIT + 50;
            let mut sql = String::from("INSERT INTO probe (id, payload) VALUES ");
            for i in 0..n {
                if i > 0 {
                    sql.push(',');
                }
                sql.push_str(&format!("({i}, 'p{i}')"));
            }
            conn.execute(&sql, []).unwrap();
        }

        let result = handle_base_op_legacy(&ctx, "query", &json!({"sql": "SELECT id FROM probe"}))
            .expect("op_query must dispatch");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["ok"], true);
        let rows = parsed["rows"].as_array().expect("rows must be an array");
        assert_eq!(
            rows.len(),
            OP_QUERY_DEFAULT_ROW_LIMIT,
            "default row cap must apply when caller omits `limit`",
        );
        assert_eq!(parsed["truncated"], true, "truncated flag must be set");
        assert_eq!(
            parsed["limit"], OP_QUERY_DEFAULT_ROW_LIMIT as u64,
            "response must report the cap that was applied",
        );
    }

    #[tokio::test]
    async fn op_query_respects_explicit_limit() {
        // Sister to the default-cap pin: caller-supplied `limit`
        // overrides the default. Pinning both halves so a refactor
        // that hardcoded the limit (or accidentally swapped the
        // unwrap_or fallback) surfaces here.
        let (_dir, ctx) = setup();
        {
            let conn = ctx.live_db.writer.lock();
            conn.execute("CREATE TABLE p (i INTEGER)", []).unwrap();
            for i in 0..50 {
                conn.execute("INSERT INTO p VALUES (?1)", [i]).unwrap();
            }
        }

        let result = handle_base_op_legacy(
            &ctx,
            "query",
            &json!({"sql": "SELECT i FROM p", "limit": 10}),
        )
        .expect("op_query must dispatch");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["rows"].as_array().unwrap().len(), 10);
        assert_eq!(parsed["truncated"], true);
        assert_eq!(parsed["limit"], 10);
    }

    #[tokio::test]
    async fn op_query_omits_truncated_when_under_limit() {
        // A query that returns fewer rows than the cap MUST NOT carry
        // the truncated flag — clients use the *presence* of the key
        // to decide whether to paginate. Set-when-not-needed would
        // cause unnecessary follow-up queries.
        let (_dir, ctx) = setup();
        {
            let conn = ctx.live_db.writer.lock();
            conn.execute("CREATE TABLE small (i INTEGER)", []).unwrap();
            for i in 0..5 {
                conn.execute("INSERT INTO small VALUES (?1)", [i]).unwrap();
            }
        }

        let result = handle_base_op_legacy(&ctx, "query", &json!({"sql": "SELECT i FROM small"}))
            .expect("op_query must dispatch");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["rows"].as_array().unwrap().len(), 5);
        assert!(
            parsed.get("truncated").is_none(),
            "under-limit response must omit `truncated` field, got {parsed}",
        );
        assert!(
            parsed.get("limit").is_none(),
            "under-limit response must omit `limit` field, got {parsed}",
        );
    }

    #[tokio::test]
    async fn required_string_ops_all_error_on_missing_field() {
        // Sweep every op that uses required_str_field. Each must
        // surface an actionable error when its required string field
        // is missing — otherwise a misconfigured client (or a future
        // typo in the MCP tool schema) could silently no-op or panic.
        // Uses the shared assert_op_errors helper to keep the sweep
        // compact. If a new op lands that takes a required string
        // field via required_str_field, add it here.
        let (_dir, ctx) = setup();
        let cases: &[(&str, &str)] = &[
            ("enrich", "pass"),
            ("get_node", "id"),
            ("read_content", "id"),
            ("find_callers", "token"),
            ("find_defs", "token"),
            ("lsp_symbols", "file"),
            ("lsp_diagnostics", "file"),
        ];
        for (op, _missing_field) in cases {
            assert_op_errors(&ctx, op, json!({}), &format!("{op} with no required field"));
        }
    }

    #[tokio::test]
    async fn op_load_errors_on_missing_or_invalid_db_field() {
        // Input-validation triplet: op_load takes a base64-encoded .db
        // payload. At scale a misconfigured client sending raw bytes,
        // forgetting the field, or with the wrong type would otherwise
        // see daemon hang or panic. Pin all three via the shared
        // assert_op_errors helper.
        let (_dir, ctx) = setup();
        assert_op_errors(&ctx, "load", json!({}), "missing db field");
        assert_op_errors(&ctx, "load", json!({"db": 42}), "non-string db");
        assert_op_errors(
            &ctx,
            "load",
            json!({"db": "!@#not-base64$%^"}),
            "invalid base64",
        );
    }

    #[tokio::test]
    async fn op_status_wire_format_pins_required_fields() {
        // Wire-format pin. op_status response is consumed by mache +
        // cli status checks; clients dispatch on every field name.
        // Pin all 6 always-emitted fields so renames or drops fail
        // loudly.
        //
        // Post-b0ea2e (capnp-json wire codec): `generation` reappears
        // in the output as the string `"0"` — capnp-json emits every
        // primitive field including defaults, and ADR-0014 §2 forbids
        // removing the ordinal. T2.4's "generation gone from the
        // wire" assertion is dropped; the field is now permanently
        // present at default value, semantically meaningless, with
        // current_root as the canonical identity. Mache + cloister
        // consumers MUST ignore the field.
        let (_dir, ctx) = setup();
        let result = handle_base_op_legacy(&ctx, "status", &json!({})).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let obj = parsed.as_object().expect("op_status returns an object");

        for required_field in [
            "ok",
            "phase",
            "current_root",
            "arena_path",
            "arena_size",
            "enrichment_typed",
        ] {
            assert!(
                obj.contains_key(required_field),
                "op_status JSON must include `{required_field}`; got keys {:?}",
                obj.keys().collect::<Vec<_>>(),
            );
        }
        // b0ea2e: `generation` is present (as `"0"`) but semantically dead.
        // Pin its presence so downstream consumers know what to expect.
        assert_eq!(
            obj.get("generation").and_then(|v| v.as_str()),
            Some("0"),
            "post-b0ea2e: legacy `generation` field present as default \"0\""
        );
        // Legacy `enrichment :Text` field deliberately not emitted —
        // handler leaves the capnp slot unset; capnp-json omits unset
        // Text. The typed shape rides in `enrichment_typed`.
        assert!(
            !obj.contains_key("enrichment"),
            "post-b0ea2e: legacy `enrichment` Text field must NOT be on the wire; \
             got keys {:?}",
            obj.keys().collect::<Vec<_>>(),
        );
        // current_root is 64-char hex (zero sentinel for fresh setup).
        let root = parsed["current_root"]
            .as_str()
            .expect("current_root is a string");
        assert_eq!(root.len(), 64, "current_root is BLAKE3 hex (64 chars)");
        assert_eq!(root, "0".repeat(64), "fresh controller emits zero sentinel");
        // phase from a fresh setup is "initializing".
        assert_eq!(parsed["phase"], "initializing");
        // enrichment_typed is a typed JSON array — no double parse on
        // consumers. Empty on fresh setup (no passes have run).
        let enriched = parsed["enrichment_typed"]
            .as_array()
            .expect("enrichment_typed is a JSON array");
        assert!(
            enriched.is_empty(),
            "fresh setup has no enrichment passes; got {enriched:?}"
        );
    }

    // The pre-parking_lot poison-recovery test lived here — deleted
    // when `ctx.state` swapped `std::sync::RwLock` → `parking_lot::RwLock`.
    // parking_lot never poisons; a panic-during-write leaves the next
    // reader with a plain `RwLockReadGuard`, so `op_status` staying
    // available is now a compile-time property (the read never
    // returned a Result to unwrap). No test can express "must
    // recover from poison" for a lock that has no poison state.
    // The op_status happy-path is covered by other tests in this
    // block.

    #[tokio::test]
    async fn test_op_flush_returns_ok() {
        let (_dir, ctx) = setup();
        let result = handle_base_op_legacy(&ctx, "flush", &json!({}));
        assert!(result.is_some());
        let parsed: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(parsed["ok"], true);
    }

    #[tokio::test]
    async fn test_unknown_op_returns_none() {
        let (_dir, ctx) = setup();
        assert!(handle_base_op_legacy(&ctx, "nonexistent", &json!({})).is_none());
    }

    #[tokio::test]
    async fn handle_base_op_dispatches_every_canonical_name() {
        // Drift guard: if a name is added to `base_op_names()` but not to
        // the `handle_base_op` match table, this test fails. We don't care
        // that some ops return errors with empty bodies — we only care that
        // dispatch returns `Some(...)`.
        let (_dir, ctx) = setup();
        for name in base_op_names() {
            assert!(
                handle_base_op_legacy(&ctx, name, &json!({})).is_some(),
                "handle_base_op did not recognize canonical op `{name}`",
            );
        }
    }

    #[test]
    fn read_root_hex_is_zero_sentinel_for_fresh_controller() {
        // T2.4 wire-contract: a brand-new controller reports
        // current_root as 64 zero hex chars. This is what op_status /
        // op_flush / op_load / op_reparse / op_enrich / op_snapshot
        // surface to clients in the `current_root` field after the
        // breaking version bump.
        let dir = TempDir::new().unwrap();
        let arena_path = dir.path().join("g.arena");
        let ctrl_path = dir.path().join("g.ctrl");
        let _mmap = leyline_core::create_arena(&arena_path, 1024 * 1024).unwrap();
        let mut ctrl = leyline_core::Controller::open_or_create(&ctrl_path).unwrap();
        ctrl.set_arena(&arena_path.to_string_lossy(), 1024 * 1024)
            .unwrap();
        drop(ctrl);

        assert_eq!(read_root_hex(&ctrl_path).unwrap(), "0".repeat(64));
    }

    #[test]
    fn read_root_hex_propagates_open_failure() {
        let bad_path = std::path::Path::new("/dev/null/definitely_not_a_directory/ctrl");
        let err = read_root_hex(bad_path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("open controller"),
            "expected error chain to mention 'open controller', got: {msg}",
        );
    }

    #[test]
    fn query_row_opt_returns_none_for_no_rows() {
        // The whole point of the helper: NoRows must not propagate as an
        // error. If a future rusqlite bump changed that, callers (read_content,
        // get_node, find_node_at_position, lsp_hover_query) would all start
        // returning Err for legitimate "id not found" lookups.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER, name TEXT);")
            .unwrap();
        let r: Option<String> =
            query_row_opt(&conn, "SELECT name FROM t WHERE id = ?1", [42], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(r, None);
    }

    #[test]
    fn query_row_opt_returns_some_for_match() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t (id INTEGER, name TEXT);
             INSERT INTO t VALUES (1, 'alpha'), (2, 'beta');",
        )
        .unwrap();
        let r: Option<String> =
            query_row_opt(&conn, "SELECT name FROM t WHERE id = ?1", [2], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(r.as_deref(), Some("beta"));
    }

    #[test]
    fn query_row_opt_propagates_prepare_phase_errors() {
        // SQL errors at the prepare/query phase (bad table, bad column,
        // syntax) must NOT be swallowed as None — that would hide bugs
        // at runtime. Only the QueryReturnedNoRows variant collapses
        // to None.
        let conn = Connection::open_in_memory().unwrap();
        let r = query_row_opt(&conn, "SELECT * FROM definitely_not_a_table", [], |row| {
            row.get::<_, String>(0)
        });
        assert!(r.is_err(), "expected error for missing table, got Ok");
    }

    #[test]
    fn query_row_opt_propagates_mapper_phase_errors() {
        // Distinct from the prepare-phase test: the mapper closure can
        // also fail (type-mismatch, missing column index). Those errors
        // also must propagate, not collapse to None. The previous test
        // only exercised the prepare path — this one exercises the
        // path through the mapper, where rusqlite returns the error
        // from inside `query_row` itself. (Caught by iter-35 adversarial
        // review.)
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t (id INTEGER, val TEXT); INSERT INTO t VALUES (1, 'hi');",
        )
        .unwrap();
        // Mapper asks for column 1 as i32 but it's TEXT — runtime type
        // mismatch surfaces from inside query_row. Must not be swallowed
        // as None (that would silently hide real type bugs).
        let r: Result<Option<i32>> =
            query_row_opt(&conn, "SELECT val FROM t WHERE id = ?1", [1], |row| {
                row.get::<_, i32>(0)
            });
        assert!(
            r.is_err(),
            "type-mismatch in mapper must propagate as Err, got: {r:?}",
        );
    }

    #[test]
    fn query_node_record_handles_missing_node_and_null_record() {
        // Single source of truth for "no record available." Exercise both
        // states callers care about:
        //   - row absent → Ok(None)
        //   - row present, record IS NULL → Ok(None) (NOT Err)
        //   - row present, record is non-empty → Ok(Some(_))
        // op_read_content used to error on NULL records (mapper asked
        // for non-nullable String); the embed drain handled NULL via
        // Option<String> mapper. Centralizing here unifies behavior.
        let conn = Connection::open_in_memory().unwrap();
        leyline_schema::create_schema(&conn).unwrap();
        leyline_schema::insert_node(&conn, "n_with", "", "n_with", 1, 0, 0, "hello").unwrap();
        // Row with record IS NULL — bypass insert_node since it always
        // takes a non-null record. Use a direct UPDATE.
        leyline_schema::insert_node(&conn, "n_null", "", "n_null", 1, 0, 0, "x").unwrap();
        conn.execute("UPDATE nodes SET record = NULL WHERE id = 'n_null'", [])
            .unwrap();

        assert_eq!(
            query_node_record(&conn, "n_with").unwrap(),
            Some("hello".to_string())
        );
        assert_eq!(query_node_record(&conn, "n_null").unwrap(), None);
        assert_eq!(query_node_record(&conn, "n_missing").unwrap(), None);
    }

    #[test]
    fn query_node_record_propagates_sql_errors() {
        // Drift guard: if `nodes` table doesn't exist (caller has the
        // wrong connection / pre-schema), the helper must surface the
        // error rather than silently return None. embed.rs previously
        // did `.ok().flatten()` which swallowed every SQL failure into
        // a no-op skip — now SQL errors are visible to the caller.
        let conn = Connection::open_in_memory().unwrap();
        let r = query_node_record(&conn, "any-id");
        assert!(
            r.is_err(),
            "missing nodes table must propagate as Err, got {r:?}",
        );
    }

    #[test]
    fn node_not_found_response_pins_wire_contract() {
        // Clients (mache, hooks, etc) parse this error message. Pin the
        // exact shape so a refactor doesn't silently break their detection.
        let body = node_not_found_response("a/b/c");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "node 'a/b/c' not found");
    }

    #[test]
    fn node_not_found_response_quotes_id_for_disambiguation() {
        // The ID is wrapped in single quotes so an empty or whitespace
        // ID still produces a parseable message. If the quoting were
        // dropped, an empty id would yield "node  not found" which
        // looks like a different bug class.
        for id in ["", " ", "a/b", "weird id with spaces"] {
            let body = node_not_found_response(id);
            assert!(
                body.contains(&format!("'{id}'")),
                "expected single-quoted id `{id}` in response, got: {body}",
            );
        }
    }

    #[test]
    fn required_str_field_returns_borrowed_value() {
        let req = json!({"token": "Foo", "id": "abc"});
        assert_eq!(required_str_field(&req, "token").unwrap(), "Foo");
        assert_eq!(required_str_field(&req, "id").unwrap(), "abc");
    }

    #[test]
    fn required_str_field_error_includes_field_name() {
        // Wire contract: clients see this error string and key off the
        // field name. If we drop the field name from the message,
        // user-facing errors get worse without anyone noticing.
        let req = json!({});
        for field in ["token", "id", "sql", "pass", "query"] {
            let err = required_str_field(&req, field).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains(&format!("\"{field}\"")),
                "expected error to name field `{field}`, got: {msg}",
            );
        }
    }

    #[test]
    fn required_str_field_rejects_non_string_values() {
        // Numbers, nulls, arrays, objects all fail the same way. None
        // of these should slip past `as_str()` and hit downstream code.
        for bad in [json!(42), json!(null), json!([]), json!({"x": 1})] {
            let req = json!({"k": bad});
            assert!(required_str_field(&req, "k").is_err());
        }
    }

    #[test]
    fn parse_file_arg_strips_file_uri_prefix() {
        // The helper centralizes `file://` stripping. If a caller passes
        // an LSP-style URI, we must hand back the bare path so the SQL
        // `node_id LIKE ?1 || '%'` clause matches our node-id convention.
        let req = json!({"file": "file:///tmp/foo.rs"});
        assert_eq!(parse_file_arg(&req).unwrap(), "/tmp/foo.rs");
    }

    #[test]
    fn parse_file_arg_passes_plain_path_through() {
        // Plain paths must round-trip unchanged.
        let req = json!({"file": "src/lib.rs"});
        assert_eq!(parse_file_arg(&req).unwrap(), "src/lib.rs");
    }

    #[test]
    fn parse_file_arg_errors_on_missing_field() {
        // The error message is part of the wire contract — clients show
        // it directly. Pin its shape so a refactor doesn't silently
        // change what users see.
        let req = json!({});
        let err = parse_file_arg(&req).unwrap_err();
        assert!(
            format!("{err:#}").contains("missing \"file\""),
            "unexpected error: {err:#}",
        );
    }

    #[test]
    fn parse_file_arg_errors_on_non_string_field() {
        // A non-string `file` (e.g. number, object) must hit the same
        // error path as a missing key — both are equally broken from
        // the caller's perspective.
        for bad in [
            json!({"file": 42}),
            json!({"file": null}),
            json!({"file": []}),
        ] {
            assert!(parse_file_arg(&bad).is_err(), "expected error for {bad}");
        }
    }

    #[test]
    fn query_lsp_rows_for_file_returns_empty_when_table_missing() {
        // Pre-enrichment: the `_lsp` table doesn't exist yet. Helper
        // must return Ok(empty), NOT propagate "no such table" SQL
        // error. Mirrors the behavior of `lsp_5col_position_rows`
        // for defs/refs — both op families behave identically when
        // the underlying enrichment hasn't run. This pins the fix
        // for the asymmetry caught by the iter-35 adversarial review.
        let conn = Connection::open_in_memory().unwrap();
        let sql = format!("SELECT node_id FROM _lsp WHERE {NODE_ID_FOR_FILE}");
        let rows = query_lsp_rows_for_file(&conn, "src/lib.rs", "_lsp", &sql, |row| {
            Ok(json!({"node_id": row.get::<_, String>(0)?}))
        })
        .unwrap();
        assert!(rows.is_empty(), "missing table must yield empty, not error");
    }

    /// Open an in-memory conn with a minimal `_lsp(node_id, foo)`
    /// table — the fixture shape used by the
    /// `query_lsp_rows_for_file_*` tests below. Centralizes the
    /// CREATE so the table shape stays consistent.
    fn lsp_test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE _lsp (node_id TEXT, foo TEXT);")
            .unwrap();
        conn
    }

    #[test]
    fn query_lsp_rows_for_file_returns_empty_when_no_match() {
        // Pre-enrichment / no-rows-for-file is the common pre-LSP case;
        // callers expect an empty Vec, not an error.
        let conn = lsp_test_conn();
        conn.execute_batch("INSERT INTO _lsp VALUES ('other.rs/x', 'bar');")
            .unwrap();
        let sql = format!("SELECT node_id, foo FROM _lsp WHERE {NODE_ID_FOR_FILE}");
        let rows = query_lsp_rows_for_file(&conn, "src/lib.rs", "_lsp", &sql, |row| {
            Ok(json!({"node_id": row.get::<_, String>(0)?, "foo": row.get::<_, String>(1)?}))
        })
        .unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn query_lsp_rows_for_file_collects_matching_rows() {
        // The helper must hand back exactly the rows whose node_id starts
        // with `<file>/` — the LIKE prefix from NODE_ID_FOR_FILE is the
        // boundary between scoped queries (used by symbols/diagnostics)
        // and global queries (used by find_callers/find_defs).
        let conn = lsp_test_conn();
        conn.execute_batch(
            "INSERT INTO _lsp VALUES
                ('src/lib.rs/a', 'one'),
                ('src/lib.rs/b', 'two'),
                ('src/other.rs/c', 'three');",
        )
        .unwrap();
        let sql = format!("SELECT node_id, foo FROM _lsp WHERE {NODE_ID_FOR_FILE}");
        let rows = query_lsp_rows_for_file(&conn, "src/lib.rs", "_lsp", &sql, |row| {
            Ok(json!({"node_id": row.get::<_, String>(0)?, "foo": row.get::<_, String>(1)?}))
        })
        .unwrap();
        assert_eq!(rows.len(), 2, "expected 2 scoped rows, got {rows:?}");
        let foos: std::collections::HashSet<&str> =
            rows.iter().map(|r| r["foo"].as_str().unwrap()).collect();
        assert!(foos.contains("one"));
        assert!(foos.contains("two"));
    }

    #[test]
    fn lsp_rows_response_omits_enriched_when_false() {
        // Pinned shape: `enriched: true` must NOT appear when rows came
        // from the warm cache. Clients distinguish "served from cache"
        // vs "served after a lazy refresh" by the *presence* of the
        // key — adding it always would silently break that signal.
        let body = lsp_rows_response("definitions", vec![], false);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert!(v["definitions"].is_array());
        assert!(
            v.get("enriched").is_none(),
            "warm hit should not include `enriched` key, got {body}",
        );
    }

    #[test]
    fn lsp_rows_response_includes_enriched_when_true() {
        // Symmetric guard: when the helper is told to mark the response
        // as enriched (i.e. second attempt succeeded), the marker must
        // be present and equal to `true`.
        let body = lsp_rows_response("references", vec![json!({"x": 1})], true);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["enriched"], true);
        assert_eq!(v["references"][0]["x"], 1);
    }

    #[test]
    fn hover_response_some_warm_hit_omits_enriched() {
        // Symmetric to lsp_rows_response_omits_enriched_when_false:
        // warm hover hit must NOT carry `enriched` key.
        let body = hover_response(Some(("docs".into(), "n0".into())), false);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["hover"], "docs");
        assert_eq!(v["node_id"], "n0");
        assert!(
            v.get("enriched").is_none(),
            "warm hover hit should not include `enriched`, got {body}",
        );
    }

    #[test]
    fn hover_response_some_after_enrich_marks_enriched() {
        // Lazy-refresh that produced a hit: `enriched: true` must appear.
        let body = hover_response(Some(("docs".into(), "n0".into())), true);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["enriched"], true);
        assert_eq!(v["hover"], "docs");
    }

    #[test]
    fn hover_response_none_after_enrich_still_marks_enriched() {
        // Regression pin for the bug fixed in this commit. Previously when
        // op_lsp_hover ran enrichment but the retry returned None, the
        // response dropped `enriched: true` — clients couldn't distinguish
        // "no data, never enriched" from "no data, just enriched (don't
        // retry)". The new contract: `enriched` reflects whether the retry
        // RAN, not whether it produced a hit.
        let body = hover_response(None, true);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert!(v["hover"].is_null(), "hover must be JSON null, got {body}");
        assert_eq!(
            v["enriched"], true,
            "post-enrich null hover MUST still mark enriched=true: {body}",
        );
    }

    #[test]
    fn hover_response_none_warm_omits_enriched() {
        // Pre-enrichment cold miss: no `enriched` marker (signals to the
        // client that a retry is worth attempting).
        let body = hover_response(None, false);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v["hover"].is_null());
        assert!(
            v.get("enriched").is_none(),
            "cold-miss hover should omit `enriched`, got {body}",
        );
    }

    #[test]
    fn lsp_rows_response_uses_caller_supplied_key() {
        // Drift guard: the helper must use whatever `json_key` the caller
        // passes — `definitions` for op_lsp_defs, `references` for op_lsp_refs.
        // If a future caller picks a new key (e.g. `decls`), the helper must
        // honor it without modification.
        for key in ["definitions", "references", "decls"] {
            let body = lsp_rows_response(key, vec![], false);
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert!(v.get(key).is_some(), "expected key `{key}` in {body}",);
        }
    }

    #[test]
    fn state_changing_ops_subset_of_canonical_names() {
        // Mutating ops must be a subset of the canonical dispatch list.
        // Catches the case where someone retires an op from `handle_base_op`
        // but forgets to remove it from `STATE_CHANGING_OPS`.
        let canonical: std::collections::HashSet<&str> = base_op_names().into_iter().collect();
        for name in STATE_CHANGING_OPS {
            assert!(
                canonical.contains(name),
                "STATE_CHANGING_OPS contains `{name}` but base_op_names() does not",
            );
        }
    }

    #[test]
    fn every_canonical_name_resolves_as_base_request_tag() {
        // Drift guard (b632ee): every entry in BASE_OP_NAMES must
        // correspond to a `BaseRequest` enum variant. Catches the
        // case where a name is added to BASE_OP_NAMES but the matching
        // `BaseRequest::Foo` variant is missing — the `is_known_base_op`
        // gate would accept the wire, then serde would fail with
        // "unknown variant", and the client would see a confusing error
        // instead of a structured "missing field" / "invalid args".
        //
        // We probe by attempting to deserialize `{"op": name}` with no
        // args. Variants with required fields will fail with "missing
        // field" — that's fine; what we reject is "unknown variant".
        use super::BaseRequest;
        use serde_json::json;

        for name in BASE_OP_NAMES {
            let payload = json!({"op": name});
            let result: std::result::Result<BaseRequest, _> = serde_json::from_value(payload);
            if let Err(e) = result {
                let msg = e.to_string();
                assert!(
                    !msg.starts_with("unknown variant"),
                    "BASE_OP_NAMES contains `{name}` but BaseRequest has no matching variant: {msg}",
                );
            }
        }
    }

    #[test]
    fn is_known_base_op_agrees_with_base_op_names() {
        // Drift guard (b632ee): post-collapse, is_known_base_op and
        // base_op_names() both derive from BASE_OP_NAMES — this test
        // pins that derivation invariant. If a future refactor
        // accidentally reintroduces a divergent matcher (the original
        // hand-maintained matches! pattern that this work eliminated),
        // the test breaks immediately.
        for name in base_op_names() {
            assert!(
                is_known_base_op(name),
                "base_op_names() contains `{name}` but is_known_base_op rejects it",
            );
        }
        // Spot-check a few clearly-not-canonical names. These must
        // never be accepted regardless of how BASE_OP_NAMES evolves.
        for bogus in ["", "nonexistent", "STATUS", "status_v2", "sheaf"] {
            assert!(
                !is_known_base_op(bogus),
                "is_known_base_op must reject bogus name `{bogus}`",
            );
        }
    }

    // ── validate op (beads ley-line-open-fa8638, ley-line-open-736800) ──

    /// `validate` with valid Go source returns `{ok: true, errors: [],
    /// diagnostics: []}`. Pins the success-path response shape mache
    /// writeback depends on (so mache can drop the CGO tree-sitter link).
    /// `#[tokio::test]` because `handle_base_op` emits events through
    /// tokio's broadcast channel — same as the `op_find_token_*` pin.
    #[cfg(feature = "validate")]
    #[tokio::test]
    async fn validate_op_valid_go_returns_ok_true() {
        let (_dir, ctx) = setup();
        let response = handle_base_op_legacy(
            &ctx,
            "validate",
            &json!({
                "content": "package main\n\nfunc main() {\n\tprintln(\"hello\")\n}\n",
                "language": "go",
            }),
        )
        .expect("validate op should be dispatched");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            v["ok"],
            json!(true),
            "valid Go should return ok=true; got {v}"
        );
        assert_eq!(
            v["errors"],
            json!([]),
            "valid Go should return empty errors; got {v}",
        );
        assert_eq!(
            v["diagnostics"],
            json!([]),
            "valid Go should return empty diagnostics; got {v}",
        );
    }

    /// `validate` with invalid Go source returns `{ok: false, errors:
    /// [{row, col, byte_start, byte_end, message}], diagnostics:
    /// [{line, col, message}]}`. Pins BOTH shapes: `errors` is the
    /// ley-line-open-736800 contract mache codes against; `diagnostics`
    /// is the legacy fa8638 first-error shape.
    #[cfg(feature = "validate")]
    #[tokio::test]
    async fn validate_op_invalid_go_returns_diagnostic() {
        let (_dir, ctx) = setup();
        let content = "package main\n\nfunc {{{ bad\n";
        let response = handle_base_op_legacy(
            &ctx,
            "validate",
            &json!({
                "content": content,
                "language": "go",
            }),
        )
        .expect("validate op should be dispatched");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            v["ok"],
            json!(false),
            "invalid Go should return ok=false; got {v}"
        );

        // 736800 contract: every ERROR/MISSING node, positioned.
        let errors = v["errors"].as_array().expect("errors must be an array");
        assert!(
            !errors.is_empty(),
            "invalid Go must yield at least one positioned error; got {v}"
        );
        for e in errors {
            let row = e["row"].as_u64().expect("error must carry `row`");
            let byte_start = e["byte_start"]
                .as_u64()
                .expect("error must carry `byte_start`");
            let byte_end = e["byte_end"].as_u64().expect("error must carry `byte_end`");
            assert!(
                e["col"].as_u64().is_some(),
                "error must carry `col`; got {e}"
            );
            assert!(
                e["message"].as_str().is_some_and(|m| !m.is_empty()),
                "error must carry a non-empty `message`; got {e}"
            );
            assert!(
                byte_start <= byte_end && byte_end <= content.len() as u64,
                "byte range must lie within the buffer (start {byte_start}, end {byte_end}, len {})",
                content.len()
            );
            assert!(
                row >= 2,
                "the broken func lives on row 2 (0-based); got row {row} in {e}"
            );
        }

        // Legacy fa8638 shape: exactly one entry, fixed message.
        let diags = v["diagnostics"]
            .as_array()
            .expect("diagnostics must be an array");
        assert_eq!(
            diags.len(),
            1,
            "exactly one diagnostic on first error; got {diags:?}"
        );
        let d = &diags[0];
        assert!(
            d["line"].as_u64().is_some(),
            "diagnostic must carry `line`; got {d}",
        );
        assert!(
            d["col"].as_u64().is_some(),
            "diagnostic must carry `col`; got {d}",
        );
        assert_eq!(
            d["message"],
            json!("syntax error"),
            "diagnostic message must be `syntax error`; got {d}",
        );
        // The legacy entry is the first `errors` entry re-shaped.
        assert_eq!(
            d["line"], errors[0]["row"],
            "diagnostics[0].line must equal errors[0].row; got {v}"
        );
        assert_eq!(
            d["col"], errors[0]["col"],
            "diagnostics[0].col must equal errors[0].col; got {v}"
        );
    }

    /// `validate` enumerates EVERY ERROR/MISSING node, not just the
    /// first (bead ley-line-open-736800 — mache renders the full list
    /// into `_diagnostics/ast-errors` for draft-mode UX). Two broken
    /// functions on different rows must yield errors on both rows.
    #[cfg(feature = "validate")]
    #[tokio::test]
    async fn validate_op_enumerates_all_errors() {
        let (_dir, ctx) = setup();
        let content = "package main\n\nfunc a( {\n}\n\nfunc b( {\n}\n";
        let response = handle_base_op_legacy(
            &ctx,
            "validate",
            &json!({
                "content": content,
                "language": "go",
            }),
        )
        .expect("validate op should be dispatched");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["ok"], json!(false), "broken Go → ok=false; got {v}");

        let errors = v["errors"].as_array().expect("errors must be an array");
        assert!(
            errors.len() >= 2,
            "two broken funcs must yield >= 2 errors, got {}: {v}",
            errors.len()
        );
        let rows: std::collections::BTreeSet<u64> =
            errors.iter().filter_map(|e| e["row"].as_u64()).collect();
        assert!(
            rows.len() >= 2,
            "errors must land on at least two distinct rows, got {rows:?}: {v}"
        );
        // Document order: byte_start must be non-decreasing.
        let starts: Vec<u64> = errors
            .iter()
            .filter_map(|e| e["byte_start"].as_u64())
            .collect();
        assert!(
            starts.windows(2).all(|w| w[0] <= w[1]),
            "errors must be in document order by byte_start; got {starts:?}"
        );
    }

    /// `validate` with an unknown/unsupported language returns the
    /// daemon's structured error envelope — not a panic, and NOT
    /// `ok: true` (bead ley-line-open-736800).
    #[cfg(feature = "validate")]
    #[tokio::test]
    async fn validate_op_unknown_language_structured_error() {
        let (_dir, ctx) = setup();
        let response = handle_base_op_legacy(
            &ctx,
            "validate",
            &json!({
                "content": "package main\n",
                "language": "brainfuck",
            }),
        )
        .expect("validate op should be dispatched (error path is still a response)");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            v["ok"],
            json!(false),
            "unknown language must not be ok:true; got {v}"
        );
        assert!(
            v["error"].as_str().is_some_and(|e| e.contains("brainfuck")),
            "error envelope must name the rejected language; got {v}"
        );
    }

    /// `path`-based inference with an unrecognized extension is also a
    /// structured error, not a false ok (bead ley-line-open-736800).
    #[cfg(feature = "validate")]
    #[tokio::test]
    async fn validate_op_unknown_path_extension_structured_error() {
        let (_dir, ctx) = setup();
        let response = handle_base_op_legacy(
            &ctx,
            "validate",
            &json!({
                "content": "key = value\n",
                "path": "config/settings.toml",
            }),
        )
        .expect("validate op should be dispatched (error path is still a response)");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            v["ok"],
            json!(false),
            "unrecognized extension must not be ok:true; got {v}"
        );
        assert!(
            v["error"].as_str().is_some(),
            "error envelope must carry `error`; got {v}"
        );
    }

    /// `validate` accepts `path` as an alternative to `language`, deriving
    /// the language from the file extension. Mache writeback knows the
    /// path of the file being edited, so this is the cleaner caller shape.
    #[cfg(feature = "validate")]
    #[tokio::test]
    async fn validate_op_path_resolves_language() {
        let (_dir, ctx) = setup();
        let response = handle_base_op_legacy(
            &ctx,
            "validate",
            &json!({
                "content": "fn main() {\n    println!(\"hello\");\n}\n",
                "path": "src/main.rs",
            }),
        )
        .expect("validate op should be dispatched");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            v["ok"],
            json!(true),
            "valid Rust via path should return ok=true; got {v}"
        );
    }

    /// `validate` requires either `language` or `path`; missing both is
    /// a structured error, not a panic or wire-shape regression.
    #[cfg(feature = "validate")]
    #[tokio::test]
    async fn validate_op_missing_language_and_path_errors() {
        let (_dir, ctx) = setup();
        let response =
            handle_base_op_legacy(&ctx, "validate", &json!({"content": "package main\n"}))
                .expect("validate op should be dispatched (error path is still a response)");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        // Error responses go through build_error_response → ErrorResponse
        // envelope; the exact shape is `{ok: false, error: "..."}` per the
        // daemon's error contract.
        assert_eq!(
            v["ok"],
            json!(false),
            "missing language+path should error; got {v}"
        );
    }

    // ── validate emit_ast (bead ley-line-open-851f24 follow-up) ────────

    /// `validate` with `emit_ast: false` (or omitted) MUST NOT carry an
    /// `ast` field. Pins the wire-additive contract — old callers see
    /// exactly the pre-851f24 shape, no new keys.
    #[cfg(feature = "validate")]
    #[tokio::test]
    async fn validate_op_emit_ast_default_omits_ast_field() {
        let (_dir, ctx) = setup();
        let response = handle_base_op_legacy(
            &ctx,
            "validate",
            &json!({
                "content": "package main\n\nfunc main() { println(\"hi\") }\n",
                "language": "go",
            }),
        )
        .expect("validate op dispatches");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert!(
            v.get("ast").is_none(),
            "no emit_ast ⇒ no `ast` field; got {v}"
        );
    }

    /// `validate` with `emit_ast: true` on a valid Go buffer MUST return
    /// the `ast` payload with `_ast` rows + defs + refs + imports.
    /// Pins the SQL-shaped row contract mache's writeback linter folds
    /// against — every row carries a stable `node_id`, hex `node_hash`,
    /// and the caller-supplied `source_id` (== `path`).
    #[cfg(feature = "validate")]
    #[tokio::test]
    async fn validate_op_emit_ast_returns_ast_payload_on_valid_go() {
        let (_dir, ctx) = setup();
        let source = "package main\n\nfunc runServe() {}\n\nfunc main() { runServe() }\n";
        let path = "cmd/serve.go";
        let response = handle_base_op_legacy(
            &ctx,
            "validate",
            &json!({
                "content": source,
                "language": "go",
                "path": path,
                "emit_ast": true,
            }),
        )
        .expect("validate op dispatches");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["ok"], json!(true), "clean Go ⇒ ok=true; got {v}");

        let ast = v.get("ast").expect("emit_ast=true ⇒ `ast` payload");
        assert_eq!(
            ast["source_id"],
            json!(path),
            "source_id must equal caller-supplied `path`; got {ast}"
        );
        assert_eq!(ast["language"], json!("go"), "language name propagates");
        let content_hash = ast["content_hash"].as_str().expect("content_hash string");
        assert_eq!(
            content_hash.len(),
            64,
            "content_hash must be 32-byte hex (64 chars); got {content_hash:?}"
        );

        let ast_rows = ast["ast"].as_array().expect("ast[] array");
        assert!(
            !ast_rows.is_empty(),
            "Go buffer must produce at least one _ast row; got {ast}"
        );
        for row in ast_rows {
            assert!(row["node_id"].as_str().is_some());
            assert_eq!(row["source_id"], json!(path));
            assert!(row["node_kind"].as_str().is_some());
            assert_eq!(
                row["node_hash"].as_str().map(str::len),
                Some(64),
                "node_hash is 32-byte hex; got {row}"
            );
        }

        let defs = ast["defs"].as_array().expect("defs[] array");
        let def_tokens: Vec<&str> = defs.iter().flat_map(|d| d["token"].as_str()).collect();
        assert!(
            def_tokens.contains(&"runServe") && def_tokens.contains(&"main"),
            "defs must include both fn names; got {def_tokens:?}"
        );

        let refs = ast["refs"].as_array().expect("refs[] array");
        let ref_tokens: Vec<&str> = refs.iter().flat_map(|r| r["token"].as_str()).collect();
        assert!(
            ref_tokens.contains(&"runServe"),
            "refs must include the call-site token; got {ref_tokens:?}"
        );
    }

    /// `validate` with `emit_ast: true` on invalid Go still returns
    /// `errors` (the syntax errors). Whether `ast` is present in that
    /// case is a soft contract — mache's fold triggers on `ok: true`
    /// only. Pin it here so the shape doesn't silently regress.
    #[cfg(feature = "validate")]
    #[tokio::test]
    async fn validate_op_emit_ast_invalid_go_still_reports_errors() {
        let (_dir, ctx) = setup();
        let response = handle_base_op_legacy(
            &ctx,
            "validate",
            &json!({
                "content": "package main\n\nfunc {{{ bad\n",
                "language": "go",
                "path": "bad.go",
                "emit_ast": true,
            }),
        )
        .expect("validate op dispatches");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["ok"], json!(false));
        let errors = v["errors"].as_array().unwrap();
        assert!(!errors.is_empty(), "broken Go must yield errors");
    }

    // ── hdc ops (bead ley-line-open-c32596) ────────────────────────────

    /// `hdc_search` returns empty results when `_hdc` is empty (the
    /// substrate hasn't been populated yet). Pins the empty-table
    /// contract — consumers should see `{ok: true, results: []}`, NOT
    /// an error. `ensure_hdc_ready` creates the table lazily on first
    /// call; before that, the query just returns zero rows.
    #[cfg(feature = "hdc")]
    #[tokio::test]
    async fn hdc_search_empty_table_returns_ok_empty() {
        let (_dir, ctx) = setup();
        let response = handle_base_op_legacy(
            &ctx,
            "hdc_search",
            &json!({
                "content": "package main\n\nfunc main() {\n\tprintln(\"hello\")\n}\n",
                "language": "go",
                "max_distance": 100,
                "k": 10,
            }),
        )
        .expect("hdc_search op should be dispatched");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["ok"], json!(true), "empty table → ok:true; got {v}");
        assert_eq!(v["results"], json!([]), "no rows → empty results; got {v}");
    }

    /// `hdc_density` returns count=0 on empty `_hdc`. Same contract as
    /// hdc_search — empty table is not an error.
    #[cfg(feature = "hdc")]
    #[tokio::test]
    async fn hdc_density_empty_table_returns_zero() {
        let (_dir, ctx) = setup();
        let response = handle_base_op_legacy(
            &ctx,
            "hdc_density",
            &json!({
                "content": "fn main() { println!(\"hello\"); }",
                "language": "rust",
            }),
        )
        .expect("hdc_density op should be dispatched");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["ok"], json!(true), "ok:true on empty table; got {v}");
        assert_eq!(v["count"], json!(0), "count=0 on empty table; got {v}");
    }

    /// `hdc_calibrate` returns layers_calibrated=0 on empty `_hdc` —
    /// every layer has <2 rows, so `calibrate_layer` returns None and
    /// nothing is persisted.
    #[cfg(feature = "hdc")]
    #[tokio::test]
    async fn hdc_calibrate_empty_table_calibrates_zero_layers() {
        let (_dir, ctx) = setup();
        let response = handle_base_op_legacy(&ctx, "hdc_calibrate", &json!({}))
            .expect("hdc_calibrate op should be dispatched");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["ok"], json!(true), "ok:true; got {v}");
        assert_eq!(
            v["layers_calibrated"],
            json!(0),
            "no rows → zero layers; got {v}"
        );
    }

    /// `hdc_search` errors cleanly on unsupported language ids. Pins
    /// the error contract — `python` is parseable by leyline-ts but
    /// HDC has no `CanonicalKindMap` for it, so the op returns an
    /// error envelope rather than silently degrading.
    #[cfg(feature = "hdc")]
    #[tokio::test]
    async fn hdc_search_unsupported_language_errors() {
        let (_dir, ctx) = setup();
        let response = handle_base_op_legacy(
            &ctx,
            "hdc_search",
            &json!({
                "content": "print('hello')\n",
                "language": "python",
            }),
        )
        .expect("hdc_search op should be dispatched (error path is still a response)");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            v["ok"],
            json!(false),
            "unsupported lang must error; got {v}",
        );
    }

    /// End-to-end: pre-populate `_hdc` with two rows (one matches the
    /// query content, one doesn't), then assert `hdc_search` returns
    /// the matching row with distance 0 (or near it). Exercises the
    /// full pipeline: encode → SQL UDF → row map. This is the strongest
    /// test — without it, the previous three pass trivially on an empty
    /// table.
    #[cfg(feature = "hdc")]
    #[tokio::test]
    async fn hdc_search_finds_pre_populated_match() {
        let (_dir, ctx) = setup();

        // First, fire any op so ensure_hdc_ready creates the schema.
        // (hdc_search on empty also creates it.)
        let _ = handle_base_op_legacy(
            &ctx,
            "hdc_density",
            &json!({"content": "fn main() {}", "language": "rust"}),
        )
        .unwrap();

        // Now encode a known Rust function and INSERT into _hdc by
        // hand. The scope_id is opaque; what matters is that the same
        // content encodes to the same HV bytes, so searching for the
        // same content finds the row with distance 0.
        let go_src = "package main\n\nfunc main() {\n\tprintln(\"x\")\n}\n";
        let hv = encode_query_hv(go_src, "go").expect("encode_query_hv");
        let hv_bytes = hv.to_vec();

        {
            let live = ctx.live_db.writer.lock();
            live.execute(
                "INSERT INTO _hdc (scope_id, layer_kind, hv, basis) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params!["test:main", "ast", hv_bytes, 0i64],
            )
            .expect("INSERT into _hdc");
        }

        let response = handle_base_op_legacy(
            &ctx,
            "hdc_search",
            &json!({
                "content": go_src,
                "language": "go",
                "max_distance": 100,
                "k": 5,
            }),
        )
        .expect("hdc_search op should be dispatched");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["ok"], json!(true), "ok:true; got {v}");
        let results = v["results"].as_array().expect("results array");
        assert_eq!(results.len(), 1, "expected one match; got {results:?}");
        assert_eq!(
            results[0]["scope_id"],
            json!("test:main"),
            "matched row has scope_id 'test:main'; got {:?}",
            results[0],
        );
        assert_eq!(
            results[0]["distance"],
            json!(0),
            "exact-content match has distance 0; got {:?}",
            results[0],
        );
    }

    // ── inspect_symbol (bead ley-line-open-c2c4d9 / L1) ─────────────────

    /// Empty tables → ok=true with empty arrays. The op should NOT
    /// error on an unknown symbol; an agent might call inspect_symbol
    /// speculatively and the empty bundle is the well-defined "not
    /// found" shape.
    #[tokio::test]
    async fn inspect_symbol_unknown_returns_empty_bundle() {
        let (_dir, ctx) = setup();
        let response =
            handle_base_op_legacy(&ctx, "inspect_symbol", &json!({"symbol_id": "Nonexistent"}))
                .expect("inspect_symbol op should be dispatched");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["symbol_id"], json!("Nonexistent"));
        assert_eq!(v["kind"], json!("unknown"));
        assert_eq!(v["definitions"], json!([]));
        assert_eq!(v["references"], json!([]));
        assert_eq!(v["callers"], json!([]));
        assert_eq!(v["callees"], json!([]));
        assert_eq!(v["hover_typed"], json!(null));
        // freshness still present even when empty — generation may be 0.
        assert!(v["freshness"].is_object(), "freshness must be present");
    }

    /// Populated tables → full bundle. Inserts a definition into
    /// node_defs + matching _ast row, two references into node_refs,
    /// and one callee path (node_refs → node_defs join). Asserts the
    /// bundle composes correctly.
    #[tokio::test]
    async fn inspect_symbol_populated_returns_full_bundle() {
        let (_dir, ctx) = setup();

        {
            let live = ctx.live_db.writer.lock();
            // Define `SendOp` at node_id `pkg/SendOp` in `pkg.go`.
            live.execute_batch(
                "INSERT INTO node_defs (token, node_id, source_id) VALUES \
                   ('SendOp', 'pkg/SendOp', 'pkg.go');
                 INSERT INTO _ast (node_id, source_id, node_kind, \
                                   start_byte, end_byte, start_row, start_col, \
                                   end_row, end_col) VALUES \
                   ('pkg/SendOp', 'pkg.go', 'function_declaration', \
                    100, 250, 5, 0, 15, 1);
                 INSERT INTO node_refs (token, node_id, source_id) VALUES \
                   ('SendOp', 'pkg/A', 'a.go'),
                   ('SendOp', 'pkg/B', 'b.go');
                 -- SendOp references some inner token 'Helper' that's
                 -- defined elsewhere; SendOp's callees should pick up
                 -- 'Helper's definition.
                 INSERT INTO node_refs (token, node_id, source_id) VALUES \
                   ('Helper', 'pkg/SendOp', 'pkg.go');
                 INSERT INTO node_defs (token, node_id, source_id) VALUES \
                   ('Helper', 'pkg/Helper', 'helper.go');",
            )
            .expect("seed test data");
        }

        let response =
            handle_base_op_legacy(&ctx, "inspect_symbol", &json!({"symbol_id": "SendOp"}))
                .expect("inspect_symbol op should be dispatched");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();

        assert_eq!(v["ok"], json!(true), "got: {v}");
        assert_eq!(v["symbol_id"], json!("SendOp"));
        // function_declaration → "function" per classify_node_kind.
        assert_eq!(v["kind"], json!("function"));

        let defs = v["definitions"].as_array().expect("definitions array");
        assert_eq!(defs.len(), 1, "exactly one definition; got {defs:?}");
        assert_eq!(defs[0]["node_id"], json!("pkg/SendOp"));
        assert_eq!(defs[0]["source_id"], json!("pkg.go"));
        assert_eq!(defs[0]["node_kind"], json!("function_declaration"));
        assert_eq!(defs[0]["start_line"], json!(5));
        assert_eq!(defs[0]["start_byte"], json!(100));
        assert_eq!(defs[0]["end_byte"], json!(250));

        let refs = v["references"].as_array().expect("references array");
        assert_eq!(refs.len(), 2, "two refs of SendOp; got {refs:?}");

        // Callees: SendOp's node_id `pkg/SendOp` references 'Helper'
        // (we inserted that node_refs row); 'Helper' is defined at
        // `pkg/Helper`. So callees should include the Helper def.
        let callees = v["callees"].as_array().expect("callees array");
        assert_eq!(callees.len(), 1, "one callee (Helper); got {callees:?}");
        assert_eq!(callees[0]["node_id"], json!("pkg/Helper"));
        assert_eq!(callees[0]["source_id"], json!("helper.go"));

        assert!(
            v["freshness"].is_object(),
            "freshness must be present; got {v}"
        );
    }

    /// `include` filter (non-empty) opts INTO specific sub-fields;
    /// other fields are absent from the response. `symbol_id`, `ok`,
    /// `kind` are always present regardless.
    #[tokio::test]
    async fn inspect_symbol_include_filter_restricts_fields() {
        let (_dir, ctx) = setup();
        let response = handle_base_op_legacy(
            &ctx,
            "inspect_symbol",
            &json!({"symbol_id": "X", "include": ["definitions", "callers"]}),
        )
        .expect("inspect_symbol op should be dispatched");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        // Always-on fields.
        assert!(v["ok"].as_bool().unwrap());
        assert_eq!(v["symbol_id"], json!("X"));
        assert!(v["kind"].is_string());
        // Included by filter.
        assert!(v.get("definitions").is_some(), "definitions included");
        assert!(v.get("callers").is_some(), "callers included");
        // NOT included.
        assert!(
            v.get("references").is_none(),
            "references NOT in include list; got {v}",
        );
        assert!(
            v.get("callees").is_none(),
            "callees NOT in include list; got {v}",
        );
        assert!(
            v.get("hover_typed").is_none(),
            "hover_typed NOT in include list; got {v}",
        );
        assert!(
            v.get("freshness").is_none(),
            "freshness NOT in include list; got {v}",
        );
    }

    /// classify_node_kind pin — make a refactor that renames a
    /// canonical node_kind (or the broad-category mapping) surface
    /// here rather than in production code that quietly returns
    /// "unknown" everywhere.
    #[test]
    fn classify_node_kind_maps_canonical_kinds() {
        assert_eq!(classify_node_kind("function_declaration"), "function");
        assert_eq!(classify_node_kind("function_item"), "function");
        assert_eq!(classify_node_kind("method_declaration"), "method");
        assert_eq!(classify_node_kind("struct_item"), "type");
        assert_eq!(classify_node_kind("type_declaration"), "type");
        assert_eq!(classify_node_kind("const_declaration"), "constant");
        assert_eq!(classify_node_kind("not_a_real_kind"), "unknown");
        assert_eq!(classify_node_kind(""), "unknown");
    }

    // ── at_position (bead ley-line-open-c2e602 / L2) ───────────────────

    /// Position with NO enclosing definition → ok=true with
    /// `symbol_id: null`. Pins the "no symbol here is not an error"
    /// contract — editors hover blank areas all the time.
    #[tokio::test]
    async fn at_position_empty_db_returns_null_symbol() {
        let (_dir, ctx) = setup();
        let response = handle_base_op_legacy(
            &ctx,
            "at_position",
            &json!({"file": "anything.go", "line": 1, "col": 1}),
        )
        .expect("at_position op should be dispatched");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["symbol_id"], json!(null));
        assert_eq!(v["kind"], json!("unknown"));
    }

    /// Position INSIDE a definition's byte range → returns the
    /// definition's token + classified kind. Smallest-enclosing
    /// preference is enforced when nested definitions overlap.
    #[tokio::test]
    async fn at_position_inside_definition_returns_token() {
        let (_dir, ctx) = setup();
        {
            let live = ctx.live_db.writer.lock();
            // SendOp covers rows 5–15; an inner Helper definition
            // covers rows 7–9 (smaller — should win at row 8).
            live.execute_batch(
                "INSERT INTO node_defs (token, node_id, source_id) VALUES \
                   ('SendOp', 'pkg/SendOp', 'pkg.go'),
                   ('Helper', 'pkg/Helper', 'pkg.go');
                 INSERT INTO _ast (node_id, source_id, node_kind, \
                                   start_byte, end_byte, start_row, start_col, \
                                   end_row, end_col) VALUES \
                   ('pkg/SendOp', 'pkg.go', 'function_declaration', \
                    100, 500, 5, 0, 15, 1),
                   ('pkg/Helper', 'pkg.go', 'function_declaration', \
                    150, 250, 7, 0, 9, 1);",
            )
            .expect("seed test data");
        }

        // Row 12 is outside Helper but inside SendOp → SendOp wins.
        let response = handle_base_op_legacy(
            &ctx,
            "at_position",
            &json!({"file": "pkg.go", "line": 12, "col": 5}),
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["symbol_id"], json!("SendOp"));
        assert_eq!(v["kind"], json!("function"));
        assert_eq!(v["node_kind"], json!("function_declaration"));

        // Row 8 is inside Helper (smaller range than SendOp).
        // Smallest-enclosing pick should return Helper.
        let response = handle_base_op_legacy(
            &ctx,
            "at_position",
            &json!({"file": "pkg.go", "line": 8, "col": 2}),
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            v["symbol_id"],
            json!("Helper"),
            "smallest-enclosing should pick Helper; got {v}",
        );
    }

    /// L7 schema gate: provenance + certainty must appear at the top
    /// level AND in every sub-array row, on every inspect_symbol
    /// response path. Test seeds a populated fixture so every sub-
    /// array has at least one row; the assertion walks the response
    /// and confirms the field set is uniform.
    ///
    /// Bead ley-line-open-c3555f. Catches the regression where a new
    /// sub-field is added without provenance/certainty — exactly the
    /// silent-degradation case L7 exists to prevent.
    #[tokio::test]
    async fn inspect_symbol_carries_provenance_and_certainty_everywhere() {
        let (_dir, ctx) = setup();
        {
            let live = ctx.live_db.writer.lock();
            // Definition + reference + callee chain so every sub-
            // array gets at least one row.
            live.execute_batch(
                "INSERT INTO node_defs (token, node_id, source_id) VALUES \
                   ('Foo', 'pkg/Foo', 'pkg.go'),
                   ('Bar', 'pkg/Bar', 'bar.go');
                 INSERT INTO _ast (node_id, source_id, node_kind, \
                                   start_byte, end_byte, start_row, start_col, \
                                   end_row, end_col) VALUES \
                   ('pkg/Foo', 'pkg.go', 'function_declaration', \
                    0, 100, 1, 0, 10, 1);
                 INSERT INTO node_refs (token, node_id, source_id) VALUES \
                   ('Foo', 'caller/site', 'caller.go'),
                   ('Bar', 'pkg/Foo', 'pkg.go');",
            )
            .expect("seed test data");
        }

        let response =
            handle_base_op_legacy(&ctx, "inspect_symbol", &json!({"symbol_id": "Foo"})).unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();

        // Top-level fields.
        assert!(v["provenance"].is_string(), "top-level provenance; got {v}");
        assert!(v["certainty"].is_string(), "top-level certainty; got {v}");

        // Every sub-array row must carry both fields.
        for field in ["definitions", "references", "callers", "callees"] {
            let arr = v[field]
                .as_array()
                .unwrap_or_else(|| panic!("{field} array"));
            assert!(!arr.is_empty(), "{field} should have rows for this fixture");
            for (i, row) in arr.iter().enumerate() {
                assert!(
                    row["provenance"].is_string(),
                    "{field}[{i}] missing provenance: {row}",
                );
                assert!(
                    row["certainty"].is_string(),
                    "{field}[{i}] missing certainty: {row}",
                );
            }
        }
    }

    // ── inspect_neighborhood (bead ley-line-open-c77690 / L3) ─────────

    /// Empty db: focal has no defs, no neighbors.
    #[tokio::test]
    async fn inspect_neighborhood_empty_db_returns_empty_neighbors() {
        let (_dir, ctx) = setup();
        let response =
            handle_base_op_legacy(&ctx, "inspect_neighborhood", &json!({"symbol_id": "X"}))
                .unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["symbol_id"], json!("X"));
        assert_eq!(v["depth"], json!(1));
        assert_eq!(v["focal"]["kind"], json!("unknown"));
        assert_eq!(v["focal"]["definitions"], json!([]));
        assert_eq!(v["neighbors"], json!([]));
    }

    /// Depth-1 hop: focal A calls B and is called by C → neighbors
    /// should contain both B (callee) and C (caller).
    #[tokio::test]
    async fn inspect_neighborhood_depth_1_returns_callers_and_callees() {
        let (_dir, ctx) = setup();
        {
            let live = ctx.live_db.writer.lock();
            // A defined at pkg/A, references B (B is its callee).
            // C is defined at pkg/C, references A (C is A's caller).
            live.execute_batch(
                "INSERT INTO node_defs (token, node_id, source_id) VALUES \
                   ('A', 'pkg/A', 'pkg.go'),
                   ('B', 'pkg/B', 'pkg.go'),
                   ('C', 'pkg/C', 'pkg.go');
                 INSERT INTO _ast (node_id, source_id, node_kind, \
                                   start_byte, end_byte, start_row, start_col, \
                                   end_row, end_col) VALUES \
                   ('pkg/A', 'pkg.go', 'function_declaration', \
                    0, 100, 1, 0, 5, 1),
                   ('pkg/B', 'pkg.go', 'function_declaration', \
                    100, 200, 6, 0, 10, 1),
                   ('pkg/C', 'pkg.go', 'function_declaration', \
                    200, 300, 11, 0, 15, 1);
                 -- A references B (so B is callee of A).
                 INSERT INTO node_refs (token, node_id, source_id) VALUES \
                   ('B', 'pkg/A', 'pkg.go');
                 -- C references A (so C is caller of A).
                 INSERT INTO node_refs (token, node_id, source_id) VALUES \
                   ('A', 'pkg/C', 'pkg.go');",
            )
            .expect("seed test data");
        }

        let response = handle_base_op_legacy(
            &ctx,
            "inspect_neighborhood",
            &json!({"symbol_id": "A", "depth": 1}),
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();

        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["focal"]["kind"], json!("function"));

        let neighbors = v["neighbors"].as_array().expect("neighbors array");
        let neighbor_tokens: std::collections::HashSet<String> = neighbors
            .iter()
            .map(|n| n["symbol_id"].as_str().unwrap().to_string())
            .collect();
        assert!(
            neighbor_tokens.contains("B"),
            "neighbors should include callee B; got {neighbor_tokens:?}",
        );
        assert!(
            neighbor_tokens.contains("C"),
            "neighbors should include caller C; got {neighbor_tokens:?}",
        );
        // All hop=1 at depth=1.
        for n in neighbors {
            assert_eq!(n["hop"], json!(1));
            // L7 contract — every row carries provenance/certainty.
            assert!(n["provenance"].is_string());
            assert!(n["certainty"].is_string());
        }
    }

    /// `depth` is clamped to `NEIGHBORHOOD_MAX_DEPTH` (4) even when
    /// caller asks for more. Pins the ADR-0016 §5 ceiling.
    #[tokio::test]
    async fn inspect_neighborhood_depth_is_capped_at_max() {
        let (_dir, ctx) = setup();
        let response = handle_base_op_legacy(
            &ctx,
            "inspect_neighborhood",
            &json!({"symbol_id": "X", "depth": 999}),
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            v["depth"],
            json!(NEIGHBORHOOD_MAX_DEPTH),
            "depth must clamp to {NEIGHBORHOOD_MAX_DEPTH}; got {v}",
        );
    }

    /// at_position → inspect_symbol composition: the symbol_id
    /// returned by at_position is exactly the input inspect_symbol
    /// expects. Pins that the two ops compose (otherwise the
    /// translation hop ADR-0016 §1 commits to doesn't actually work).
    #[tokio::test]
    async fn at_position_output_is_inspect_symbol_input() {
        let (_dir, ctx) = setup();
        {
            let live = ctx.live_db.writer.lock();
            live.execute_batch(
                "INSERT INTO node_defs (token, node_id, source_id) VALUES \
                   ('Foo', 'pkg/Foo', 'pkg.go');
                 INSERT INTO _ast (node_id, source_id, node_kind, \
                                   start_byte, end_byte, start_row, start_col, \
                                   end_row, end_col) VALUES \
                   ('pkg/Foo', 'pkg.go', 'function_declaration', \
                    0, 100, 1, 0, 10, 1);",
            )
            .expect("seed test data");
        }

        // Step 1: at_position → symbol_id
        let response = handle_base_op_legacy(
            &ctx,
            "at_position",
            &json!({"file": "pkg.go", "line": 5, "col": 5}),
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        let symbol_id = v["symbol_id"].as_str().expect("symbol_id present");

        // Step 2: inspect_symbol → bundle
        let response =
            handle_base_op_legacy(&ctx, "inspect_symbol", &json!({"symbol_id": symbol_id}))
                .unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["symbol_id"], json!("Foo"));
        // Bundle should resolve back to the same definition.
        let defs = v["definitions"].as_array().unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0]["node_id"], json!("pkg/Foo"));
    }

    // ── search_symbols (bead ley-line-open-c79953 / L4) ───────────────

    /// Helper: parse an NDJSON response string into a Vec of JSON
    /// values, one per non-empty line. Pins the line-delimited shape:
    /// each line must independently parse as a JSON object.
    fn parse_ndjson(s: &str) -> Vec<serde_json::Value> {
        s.split('\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).expect("each NDJSON line parses as JSON"))
            .collect()
    }

    /// Empty pattern → empty string (zero lines), NOT `"\n"`, NOT
    /// `"[]"`. Pins the NDJSON zero-result shape ADR-0016 §6 commits
    /// to: a missing trailing newline at zero rows is the unambiguous
    /// "no matches" signal.
    #[tokio::test]
    async fn search_symbols_empty_pattern_returns_empty_string() {
        let (_dir, ctx) = setup();
        let response =
            handle_base_op_legacy(&ctx, "search_symbols", &json!({"pattern": ""})).unwrap();
        assert_eq!(
            response, "",
            "empty pattern → empty string; got {response:?}"
        );
    }

    /// GLOB `Send*` against seeded `SendOp` / `SendBatch` / `Receive`
    /// returns exactly 2 lines, each independently parseable as JSON.
    /// Pins both the GLOB semantics AND the line-delimited contract.
    #[tokio::test]
    async fn search_symbols_glob_pattern_matches_prefix() {
        let (_dir, ctx) = setup();
        {
            let live = ctx.live_db.writer.lock();
            live.execute_batch(
                "INSERT INTO node_defs (token, node_id, source_id) VALUES \
                   ('SendOp',    'pkg/SendOp',    'pkg.go'),
                   ('SendBatch', 'pkg/SendBatch', 'pkg.go'),
                   ('Receive',   'pkg/Receive',   'pkg.go');
                 INSERT INTO _ast (node_id, source_id, node_kind, \
                                   start_byte, end_byte, start_row, start_col, \
                                   end_row, end_col) VALUES \
                   ('pkg/SendOp',    'pkg.go', 'function_declaration', 0, 50, 1, 0, 5, 0),
                   ('pkg/SendBatch', 'pkg.go', 'function_declaration', 50, 100, 6, 0, 10, 0),
                   ('pkg/Receive',   'pkg.go', 'function_declaration', 100, 150, 11, 0, 15, 0);",
            )
            .expect("seed test data");
        }

        let response =
            handle_base_op_legacy(&ctx, "search_symbols", &json!({"pattern": "Send*"})).unwrap();
        let lines = parse_ndjson(&response);
        assert_eq!(lines.len(), 2, "Send* should match 2 rows; got {response}");

        let symbols: std::collections::HashSet<String> = lines
            .iter()
            .map(|l| l["symbol_id"].as_str().unwrap().to_string())
            .collect();
        assert!(symbols.contains("SendOp"), "expected SendOp in {symbols:?}");
        assert!(
            symbols.contains("SendBatch"),
            "expected SendBatch in {symbols:?}",
        );
        assert!(
            !symbols.contains("Receive"),
            "Receive must NOT match Send*; got {symbols:?}",
        );

        // Per-row shape pin: every line carries the L7 provenance +
        // certainty fields plus the canonical row shape.
        for line in &lines {
            assert!(line["symbol_id"].is_string(), "symbol_id; got {line}");
            assert!(line["node_id"].is_string(), "node_id; got {line}");
            assert!(line["source_id"].is_string(), "source_id; got {line}");
            assert_eq!(line["kind"], json!("function"));
            assert_eq!(line["provenance"], json!("tree-sitter"));
            assert_eq!(line["certainty"], json!("full"));
        }
    }

    /// `limit` caps the number of returned rows.
    #[tokio::test]
    async fn search_symbols_limit_is_respected() {
        let (_dir, ctx) = setup();
        {
            let live = ctx.live_db.writer.lock();
            live.execute_batch(
                "INSERT INTO node_defs (token, node_id, source_id) VALUES \
                   ('Match1', 'pkg/Match1', 'pkg.go'),
                   ('Match2', 'pkg/Match2', 'pkg.go'),
                   ('Match3', 'pkg/Match3', 'pkg.go'),
                   ('Match4', 'pkg/Match4', 'pkg.go'),
                   ('Match5', 'pkg/Match5', 'pkg.go');",
            )
            .expect("seed test data");
        }

        let response = handle_base_op_legacy(
            &ctx,
            "search_symbols",
            &json!({"pattern": "Match*", "limit": 2}),
        )
        .unwrap();
        let lines = parse_ndjson(&response);
        assert_eq!(
            lines.len(),
            2,
            "limit=2 should cap to 2 rows; got {} lines: {response}",
            lines.len(),
        );
    }

    /// `kind="function"` filter excludes non-function rows. Pins the
    /// row-side classify_node_kind filtering path.
    #[tokio::test]
    async fn search_symbols_kind_filter_excludes_non_matching() {
        let (_dir, ctx) = setup();
        {
            let live = ctx.live_db.writer.lock();
            live.execute_batch(
                "INSERT INTO node_defs (token, node_id, source_id) VALUES \
                   ('FooFn',     'pkg/FooFn',     'pkg.go'),
                   ('FooStruct', 'pkg/FooStruct', 'pkg.go'),
                   ('FooConst',  'pkg/FooConst',  'pkg.go');
                 INSERT INTO _ast (node_id, source_id, node_kind, \
                                   start_byte, end_byte, start_row, start_col, \
                                   end_row, end_col) VALUES \
                   ('pkg/FooFn',     'pkg.go', 'function_declaration', 0, 50, 1, 0, 5, 0),
                   ('pkg/FooStruct', 'pkg.go', 'struct_item',          50, 100, 6, 0, 10, 0),
                   ('pkg/FooConst',  'pkg.go', 'const_declaration',    100, 150, 11, 0, 15, 0);",
            )
            .expect("seed test data");
        }

        let response = handle_base_op_legacy(
            &ctx,
            "search_symbols",
            &json!({"pattern": "Foo*", "kind": "function"}),
        )
        .unwrap();
        let lines = parse_ndjson(&response);
        assert_eq!(
            lines.len(),
            1,
            "kind=function should match only FooFn; got {response}",
        );
        assert_eq!(lines[0]["symbol_id"], json!("FooFn"));
        assert_eq!(lines[0]["kind"], json!("function"));
    }

    // ── agreement op (bead ley-line-open-c8090f / L10) ────────────────
    //
    // Gate 3 of ADR-0020: the op MUST mechanically reach
    // `CellComplex::detect_violations`. The
    // `DETECT_VIOLATIONS_REACH_COUNT` spy (gated under
    // `leyline-sheaf`'s `test-spy` feature, enabled by cli-lib's dev-
    // dependency entry) is the load-bearing signal. A future refactor
    // that "optimizes away" the algebra call would zero the counter
    // delta and fail this test.

    /// Encode a `Vec<f32>` as the little-endian byte sequence the v1
    /// `agreement` op consumes from `observation.payload_inline`.
    /// Pulled out as a test helper so the fixture is readable instead of
    /// a wall of `f32::to_le_bytes` calls inline.
    fn encode_inline_f32(stalk: &[f32]) -> Vec<u8> {
        let mut out = Vec::with_capacity(stalk.len() * 4);
        for v in stalk {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    /// Insert a single observation row into the daemon's living db with
    /// the v1 raw-f32 inline payload. `mentions` is the JSON array form
    /// ADR-0020 §1 specifies — the agreement op's `EXISTS … json_each`
    /// filter walks this column.
    fn insert_observation(
        ctx: &std::sync::Arc<DaemonContext>,
        source: &str,
        payload_kind: &str,
        token: &str,
        stalk: &[f32],
        observed_at: i64,
    ) {
        let live = ctx.live_db.writer.lock();
        crate::daemon::observation_schema::create_observation_schema(&live)
            .expect("install observation schema");
        let payload = encode_inline_f32(stalk);
        let mentions = format!("[\"{token}\"]");
        live.execute(
            "INSERT INTO observation (source, payload_kind, payload_inline, mentions, observed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![source, payload_kind, payload, mentions, observed_at],
        )
        .expect("insert observation");
    }

    /// Gate 3 (ADR-0020 §3 / falsifiability gate for L10): two
    /// observations of the same `(token, payload_kind)` from different
    /// sources with disagreeing stalk fields MUST surface as a non-
    /// empty `defects` array, and the computation MUST mechanically
    /// reach `CellComplex::detect_violations` (verified via the
    /// `DETECT_VIOLATIONS_REACH_COUNT` spy).
    ///
    /// Falsifiability — this test FAILS if either:
    ///   (a) `detect_violations` is short-circuited / not invoked
    ///       (spy delta == 0), or
    ///   (b) the op returns an empty `defects` array despite the
    ///       fixture injecting disagreement.
    #[tokio::test]
    async fn agreement_gate3_two_sources_disagree_reaches_detect_violations() {
        use leyline_sheaf::complex::DETECT_VIOLATIONS_REACH_COUNT;
        use std::sync::atomic::Ordering;

        let (_dir, ctx) = setup();

        // Fixture: two `code.symbol_def` observations on the same
        // token from different sources, disagreeing on the second
        // coordinate (clearly above the EPS = 1e-4 threshold).
        insert_observation(
            &ctx,
            "tree-sitter",
            "code.symbol_def",
            "sym:Foo",
            &[1.0, 2.0, 3.0],
            1_000,
        );
        insert_observation(
            &ctx,
            "git",
            "code.symbol_def",
            "sym:Foo",
            &[1.0, 7.5, 3.0],
            1_001,
        );

        // Snapshot the spy counter so concurrent tests don't pollute
        // the absolute reading. The op must increment it by ≥ 1.
        let before = DETECT_VIOLATIONS_REACH_COUNT.load(Ordering::Relaxed);

        let response = handle_base_op_legacy(
            &ctx,
            "agreement",
            &json!({"token": "sym:Foo", "payload_kind": "code.symbol_def"}),
        )
        .expect("agreement op recognised");
        let v: serde_json::Value = serde_json::from_str(&response).expect("valid json");

        let after = DETECT_VIOLATIONS_REACH_COUNT.load(Ordering::Relaxed);
        assert!(
            after > before,
            "Gate 3: detect_violations must be reached (spy went {before} → {after}); \
             response was {v}",
        );

        // Wire shape contract.
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["token"], json!("sym:Foo"));
        assert_eq!(v["payload_kind"], json!("code.symbol_def"));
        assert_eq!(v["source_count"], json!(2));
        assert_eq!(v["provenance"], json!("composed"));
        assert_eq!(v["certainty"], json!("full"));

        let defects = v["defects"].as_array().expect("defects array");
        assert!(
            !defects.is_empty(),
            "Gate 3: defects must be non-empty when sources disagree; response was {v}",
        );

        // coherence_defect = Σ margin² — must be > 0 because at least
        // one row's margin is non-zero (sources disagreed).
        let coherence_defect = v["coherence_defect"]
            .as_f64()
            .expect("coherence_defect numeric");
        assert!(
            coherence_defect > 0.0,
            "coherence_defect must be positive when sources disagree; got {coherence_defect}",
        );

        // Each defect row carries the (source_a, source_b) recovery
        // plus L7 provenance/certainty.
        for row in defects {
            assert!(row["source_a"].is_string(), "missing source_a: {row}");
            assert!(row["source_b"].is_string(), "missing source_b: {row}");
            assert!(row["margin"].is_number(), "missing margin: {row}");
            assert!(row["severity"].is_number(), "missing severity: {row}");
            assert_eq!(row["provenance"], json!("sheaf"));
            assert_eq!(row["certainty"], json!("full"));
        }
    }

    /// Sanity case: two observations from different sources that
    /// agree (identical stalks) → defects empty + coherence_defect 0,
    /// but the spy STILL fires (the op constructs the 2-node complex
    /// and runs the algebra; the result is just that no edge crosses
    /// the EPS threshold). Pins that "no disagreement" is a successful
    /// op response, not a short-circuit that bypasses the math.
    #[tokio::test]
    async fn agreement_two_sources_agree_returns_empty_defects() {
        use leyline_sheaf::complex::DETECT_VIOLATIONS_REACH_COUNT;
        use std::sync::atomic::Ordering;

        let (_dir, ctx) = setup();
        insert_observation(
            &ctx,
            "tree-sitter",
            "code.symbol_def",
            "sym:Bar",
            &[4.0, 5.0, 6.0],
            2_000,
        );
        insert_observation(
            &ctx,
            "git",
            "code.symbol_def",
            "sym:Bar",
            &[4.0, 5.0, 6.0],
            2_001,
        );

        let before = DETECT_VIOLATIONS_REACH_COUNT.load(Ordering::Relaxed);
        let response = handle_base_op_legacy(
            &ctx,
            "agreement",
            &json!({"token": "sym:Bar", "payload_kind": "code.symbol_def"}),
        )
        .unwrap();
        let after = DETECT_VIOLATIONS_REACH_COUNT.load(Ordering::Relaxed);

        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert!(
            after > before,
            "agreement-on-agreement should still invoke the algebra",
        );
        assert_eq!(v["defects"], json!([]));
        assert_eq!(v["coherence_defect"].as_f64().unwrap(), 0.0);
        assert_eq!(v["source_count"], json!(2));
    }

    /// Edge case: single source for `(token, payload_kind)` — nothing
    /// to disagree with. Op returns OK with empty defects and 0.0
    /// defect; v1 documents this as the "trivial agreement" path.
    #[tokio::test]
    async fn agreement_single_source_returns_empty_defects() {
        let (_dir, ctx) = setup();
        insert_observation(
            &ctx,
            "tree-sitter",
            "code.symbol_def",
            "sym:Solo",
            &[9.0, 9.0, 9.0],
            3_000,
        );

        let response = handle_base_op_legacy(
            &ctx,
            "agreement",
            &json!({"token": "sym:Solo", "payload_kind": "code.symbol_def"}),
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["defects"], json!([]));
        assert_eq!(v["coherence_defect"].as_f64().unwrap(), 0.0);
        assert_eq!(v["source_count"], json!(1));
    }

    /// No observations exist for the requested `(token, payload_kind)`
    /// — empty result, op still succeeds, table is lazily installed.
    #[tokio::test]
    async fn agreement_no_observations_returns_empty() {
        let (_dir, ctx) = setup();
        let response = handle_base_op_legacy(
            &ctx,
            "agreement",
            &json!({"token": "sym:Missing", "payload_kind": "code.symbol_def"}),
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["source_count"], json!(0));
        assert_eq!(v["defects"], json!([]));
    }

    /// Multiple observations per source: the op picks the LATEST
    /// observation per source (per ADR-0020 §1's `observed_at`
    /// ordering — the freshest-wins projection). Pins that older
    /// observations don't pollute the agreement check.
    #[tokio::test]
    async fn agreement_takes_latest_observation_per_source() {
        use leyline_sheaf::complex::DETECT_VIOLATIONS_REACH_COUNT;
        use std::sync::atomic::Ordering;

        let (_dir, ctx) = setup();
        // Stale (older) tree-sitter observation that disagrees with git.
        insert_observation(
            &ctx,
            "tree-sitter",
            "code.symbol_def",
            "sym:Latest",
            &[0.0, 100.0, 0.0],
            1_000,
        );
        // Fresh tree-sitter observation that AGREES with git.
        insert_observation(
            &ctx,
            "tree-sitter",
            "code.symbol_def",
            "sym:Latest",
            &[1.0, 2.0, 3.0],
            5_000,
        );
        insert_observation(
            &ctx,
            "git",
            "code.symbol_def",
            "sym:Latest",
            &[1.0, 2.0, 3.0],
            4_000,
        );

        let before = DETECT_VIOLATIONS_REACH_COUNT.load(Ordering::Relaxed);
        let response = handle_base_op_legacy(
            &ctx,
            "agreement",
            &json!({"token": "sym:Latest", "payload_kind": "code.symbol_def"}),
        )
        .unwrap();
        let after = DETECT_VIOLATIONS_REACH_COUNT.load(Ordering::Relaxed);

        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert!(after > before);
        assert_eq!(v["source_count"], json!(2));
        // Latest-per-source: tree-sitter's [1,2,3] vs git's [1,2,3] → agree.
        assert_eq!(v["defects"], json!([]));
        assert_eq!(v["coherence_defect"].as_f64().unwrap(), 0.0);
    }

    /// Unit test for the inline-payload decoder: 4-byte chunks → f32s.
    /// Empty / unaligned payloads return None so the op skips the row
    /// rather than panicking.
    #[test]
    fn decode_inline_payload_roundtrips_f32_vec() {
        // Non-pi test fixture (was 3.14159 — flagged by clippy as an
        // approximate PI literal, which suggested std::f32::consts::PI
        // and confuses future readers).
        let original = vec![1.0f32, -2.5, 3.14195, 0.0];
        let bytes = {
            let mut b = Vec::new();
            for v in &original {
                b.extend_from_slice(&v.to_le_bytes());
            }
            b
        };
        let decoded = decode_inline_payload(&bytes).expect("decode succeeds");
        assert_eq!(decoded, original);
    }

    #[test]
    fn decode_inline_payload_rejects_unaligned() {
        // 3 bytes — not a multiple of 4. Must return None, not panic.
        assert!(decode_inline_payload(&[0, 0, 0]).is_none());
        // Empty — also None (nothing to decode).
        assert!(decode_inline_payload(&[]).is_none());
    }

    /// Math-friend bead `ley-line-open-659a39` (HIGH). Two sources
    /// emit stalks of different dims for the same `(token, payload_kind)`.
    /// Pre-fix: op silently returned `{ok:true, source_count:2, defects:[],
    /// coherence_defect:0}` — wire-indistinguishable from "sources agree."
    /// Post-fix: op returns `{ok:false, error:"incompatible_stalk_dims", detail:...}`.
    #[tokio::test]
    async fn agreement_dim_mismatch_returns_explicit_error() {
        let (_dir, ctx) = setup();
        insert_observation(
            &ctx,
            "tree-sitter",
            "code.symbol_def",
            "sym:Foo",
            &[1.0, 2.0, 3.0],
            1_000,
        );
        insert_observation(
            &ctx,
            "git",
            "code.symbol_def",
            "sym:Foo",
            &[1.0, 2.0, 3.0, 4.0],
            1_001,
        );

        let response = handle_base_op_legacy(
            &ctx,
            "agreement",
            &json!({"token": "sym:Foo", "payload_kind": "code.symbol_def"}),
        )
        .expect("op recognized");
        let v: serde_json::Value = serde_json::from_str(&response).expect("valid json");

        assert_eq!(v["ok"], json!(false), "must NOT coerce to ok=true; got {v}");
        assert_eq!(v["error"], json!("incompatible_stalk_dims"));
        assert_eq!(v["source_count"], json!(2));
        let detail = v["detail"].as_str().expect("detail string");
        assert!(
            detail.contains("dim 3"),
            "detail must cite first dim: {detail}"
        );
        assert!(
            detail.contains("dim 4"),
            "detail must cite second dim: {detail}"
        );
    }

    #[test]
    fn classify_agreement_dims_handles_empty_single_and_mixed() {
        // Empty → Ok(None) (no rows, no algebra)
        assert!(matches!(classify_agreement_dims(&[]), Ok(None)));

        // Single row → Ok(Some(dim))
        let single = vec![AgreementRow {
            source: "a".into(),
            stalk: vec![1.0, 2.0, 3.0],
        }];
        assert!(matches!(classify_agreement_dims(&single), Ok(Some(3))));

        // Multi same dim → Ok(Some(dim))
        let homo = vec![
            AgreementRow {
                source: "a".into(),
                stalk: vec![1.0, 2.0],
            },
            AgreementRow {
                source: "b".into(),
                stalk: vec![3.0, 4.0],
            },
        ];
        assert!(matches!(classify_agreement_dims(&homo), Ok(Some(2))));

        // Multi mismatched → Err
        let mixed = vec![
            AgreementRow {
                source: "a".into(),
                stalk: vec![1.0, 2.0],
            },
            AgreementRow {
                source: "b".into(),
                stalk: vec![1.0, 2.0, 3.0],
            },
        ];
        let err = classify_agreement_dims(&mixed).unwrap_err();
        assert_eq!(err.0, "a");
        assert_eq!(err.1, 2);
        assert_eq!(err.2, "b");
        assert_eq!(err.3, 3);
    }
}
