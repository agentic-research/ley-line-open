//! Project LSP data into the `nodes` table, optionally merging with tree-sitter AST.
//!
//! Two modes:
//! - Standalone: `/symbols/...` + `/diagnostics/...` as independent trees
//! - Merged: enrich existing AST nodes with LSP metadata via `_lsp` table
//!
//! Additional tables for extended LSP data:
//! - `_lsp_defs`  — go-to-definition results (node_id → definition locations)
//! - `_lsp_refs`  — find-references results (node_id → reference locations)
//! - `_lsp_hover` — hover text per node
//! - `_lsp_completions` — completion items per position

use anyhow::Result;
use rusqlite::{Connection, params};
use std::time::{SystemTime, UNIX_EPOCH};

use leyline_schema::{create_schema, insert_node};

use crate::protocol::{
    self, CompletionItem, Diagnostic, DiagnosticSeverity, DocumentSymbol, Hover, Location,
};

pub const LSP_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _lsp (
    node_id TEXT PRIMARY KEY,
    symbol_kind TEXT,
    detail TEXT,
    start_line INTEGER NOT NULL,
    start_col INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    end_col INTEGER NOT NULL,
    diagnostics TEXT
);
CREATE INDEX IF NOT EXISTS idx_lsp_kind ON _lsp(symbol_kind);";

/// `_lsp_defs` schema (ADR-0013 Step 1 — ley-line-453f7e).
///
/// Producer-side token extraction: `def_token` carries the textual
/// symbol name so consumer views (mache `v_defs`) can `UNION ALL` with
/// tree-sitter's `node_defs(token, node_id)` without a runtime byte-
/// range join. See `docs/adr/0013-refs-defs-canonical-schema.md` in
/// mache.
pub const LSP_DEFS_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _lsp_defs (
    node_id TEXT NOT NULL,
    def_token TEXT NOT NULL DEFAULT '',
    def_uri TEXT NOT NULL,
    def_start_line INTEGER NOT NULL,
    def_start_col INTEGER NOT NULL,
    def_end_line INTEGER NOT NULL,
    def_end_col INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_lsp_defs_node ON _lsp_defs(node_id);";
// idx_lsp_defs_token is created in migrate_lsp_schema after the
// def_token column is guaranteed to exist (handles the legacy-table
// migration path where def_token isn't in the original CREATE TABLE).

/// `_lsp_refs` schema (ADR-0013 Step 1 — ley-line-453f7e).
///
/// Producer-side token + referrer extraction:
/// - `node_id` is the *target* (the def this ref points at). Per
///   ADR-0013 the canonical name is `target_node_id`; the existing
///   column name is preserved for additive backward-compat. Step 4
///   renames in lockstep with mache.
/// - `referrer_node_id` is the AST node containing the reference site.
///   NULL when the source file is not in `_ast` (cross-repo refs).
/// - `ref_token` is the textual lemma at the ref site, extracted from
///   source bytes at write time. NEVER NULL — defaults to empty if
///   bytes were unavailable, so consumer queries on `ref_token` can
///   filter empties without coalescing.
pub const LSP_REFS_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _lsp_refs (
    node_id TEXT NOT NULL,
    referrer_node_id TEXT,
    ref_token TEXT NOT NULL DEFAULT '',
    ref_uri TEXT NOT NULL,
    ref_start_line INTEGER NOT NULL,
    ref_start_col INTEGER NOT NULL,
    ref_end_line INTEGER NOT NULL,
    ref_end_col INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_lsp_refs_node ON _lsp_refs(node_id);";
// idx_lsp_refs_referrer + idx_lsp_refs_token are created in
// migrate_lsp_schema after the columns are guaranteed to exist —
// see the comment on LSP_DEFS_DDL above.

/// Migrate pre-ADR-0013 `_lsp_*` tables in-place by adding the new
/// columns if they don't already exist. SQLite's `ALTER TABLE ADD
/// COLUMN` errors on duplicate-column-name, so we probe via
/// `pragma_table_info` first.
///
/// Idempotent — safe to call on every `create_lsp_schema` invocation.
fn migrate_lsp_schema(conn: &Connection) -> Result<()> {
    fn has_column(conn: &Connection, table: &str, col: &str) -> Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name == col {
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn ensure_column(conn: &Connection, table: &str, col: &str, decl: &str) -> Result<()> {
        if !has_column(conn, table, col)? {
            conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {col} {decl}"), [])?;
        }
        Ok(())
    }

    // Only ALTER if the table already exists from a pre-ADR-0013 run.
    let lsp_defs_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='_lsp_defs'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(false);
    if lsp_defs_exists {
        ensure_column(conn, "_lsp_defs", "def_token", "TEXT NOT NULL DEFAULT ''")?;
        // ALTER TABLE doesn't add CREATE INDEX clauses; ensure the
        // index exists separately.
        conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_lsp_defs_token ON _lsp_defs(def_token);")?;
    }

    let lsp_refs_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='_lsp_refs'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(false);
    if lsp_refs_exists {
        ensure_column(conn, "_lsp_refs", "referrer_node_id", "TEXT")?;
        ensure_column(conn, "_lsp_refs", "ref_token", "TEXT NOT NULL DEFAULT ''")?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_lsp_refs_referrer ON _lsp_refs(referrer_node_id);
             CREATE INDEX IF NOT EXISTS idx_lsp_refs_token ON _lsp_refs(ref_token);",
        )?;
    }
    Ok(())
}

pub const LSP_HOVER_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _lsp_hover (
    node_id TEXT PRIMARY KEY,
    hover_text TEXT NOT NULL
);";

pub const LSP_COMPLETIONS_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _lsp_completions (
    node_id TEXT NOT NULL,
    label TEXT NOT NULL,
    kind TEXT,
    detail TEXT,
    documentation TEXT,
    sort_text TEXT
);
CREATE INDEX IF NOT EXISTS idx_lsp_completions_node ON _lsp_completions(node_id);";

/// Create the full schema for LSP projection (nodes + all _lsp* tables).
///
/// Idempotent on existing databases — calls `migrate_lsp_schema` to add
/// any new ADR-0013 columns to `_lsp_defs` / `_lsp_refs` if they exist
/// from a pre-ADR-0013 daemon run.
pub fn create_lsp_schema(conn: &Connection) -> Result<()> {
    create_schema(conn)?;
    conn.execute_batch(LSP_DDL)?;
    conn.execute_batch(LSP_DEFS_DDL)?;
    conn.execute_batch(LSP_REFS_DDL)?;
    conn.execute_batch(LSP_HOVER_DDL)?;
    conn.execute_batch(LSP_COMPLETIONS_DDL)?;
    // ADR-0013 Step 1: ensure new columns exist on pre-existing tables.
    migrate_lsp_schema(conn)?;
    Ok(())
}

// ── Standalone projection ──────────────────────────────────────

/// Project LSP symbols and diagnostics into a standalone SQLite database.
///
/// Returns serialized bytes ready for arena load.
pub fn project_lsp(
    symbols: &[DocumentSymbol],
    diagnostics: &[Diagnostic],
    source_uri: &str,
) -> Result<Vec<u8>> {
    let conn = Connection::open_in_memory()?;
    project_lsp_into(symbols, diagnostics, source_uri, &conn)?;
    let data = conn.serialize(rusqlite::DatabaseName::Main)?;
    Ok(data.to_vec())
}

/// Project LSP data into an existing connection.
pub fn project_lsp_into(
    symbols: &[DocumentSymbol],
    diagnostics: &[Diagnostic],
    source_uri: &str,
    conn: &Connection,
) -> Result<()> {
    create_lsp_schema(conn)?;

    let mtime = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    // Root
    insert_node(conn, "", "", "", 1, 0, mtime, "")?;

    // /symbols — document symbol hierarchy
    insert_node(conn, "symbols", "", "symbols", 1, 0, mtime, "")?;
    for sym in symbols {
        walk_symbol(conn, sym, "symbols", mtime)?;
    }

    // /diagnostics — flat list keyed by severity + index
    if !diagnostics.is_empty() {
        insert_node(conn, "diagnostics", "", "diagnostics", 1, 0, mtime, "")?;

        for severity_label in &["error", "warning", "info", "hint"] {
            let severity_val = match *severity_label {
                "error" => DiagnosticSeverity::ERROR,
                "warning" => DiagnosticSeverity::WARNING,
                "info" => DiagnosticSeverity::INFORMATION,
                "hint" => DiagnosticSeverity::HINT,
                _ => continue,
            };
            let matching: Vec<_> = diagnostics
                .iter()
                .filter(|d| d.severity == Some(severity_val))
                .collect();
            if matching.is_empty() {
                continue;
            }

            let group_id = format!("diagnostics/{severity_label}");
            insert_node(
                conn,
                &group_id,
                "diagnostics",
                severity_label,
                1,
                0,
                mtime,
                "",
            )?;

            for (i, diag) in matching.iter().enumerate() {
                let diag_id = format!("{group_id}/{i}");
                let name = format!("{i}");
                let record = serde_json::json!({
                    "message": diag.message,
                    "source": diag.source,
                    "code": diag.code,
                    "range": format!("{}:{}-{}:{}",
                        diag.range.start.line, diag.range.start.character,
                        diag.range.end.line, diag.range.end.character),
                    "uri": source_uri,
                });
                let record_str = record.to_string();
                insert_node(
                    conn,
                    &diag_id,
                    &group_id,
                    &name,
                    0,
                    record_str.len() as i64,
                    mtime,
                    &record_str,
                )?;
            }
        }
    }

    Ok(())
}

// ── Merge into AST ─────────────────────────────────────────────

/// Merge LSP data into an existing database that has tree-sitter AST nodes.
///
/// Matches LSP symbols to AST nodes by overlapping line ranges,
/// writing enrichment data into the `_lsp` table.
pub fn merge_lsp_into_ast(
    symbols: &[DocumentSymbol],
    diagnostics: &[Diagnostic],
    conn: &Connection,
) -> Result<usize> {
    // Ensure _lsp table exists
    conn.execute_batch(LSP_DDL)?;

    let mut matched = 0;

    let has_ast = conn
        .prepare("SELECT COUNT(*) FROM sqlite_master WHERE name = '_ast'")
        .and_then(|mut s| s.query_row([], |r| r.get::<_, i64>(0)))
        .unwrap_or(0)
        > 0;

    for sym in symbols {
        matched += merge_symbol(conn, sym, has_ast, diagnostics)?;
    }

    // Insert diagnostics that didn't match any symbol
    for diag in diagnostics {
        let line = diag.range.start.line;
        let col = diag.range.start.character;
        let diag_node_id = format!("_diag/L{}C{}", line, col);

        let already_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM _lsp WHERE node_id = ?1",
                params![diag_node_id],
                |r| r.get(0),
            )
            .unwrap_or(false);

        if !already_exists {
            let diag_json = serde_json::to_string(&[diag])?;
            conn.execute(
                "INSERT OR IGNORE INTO _lsp (node_id, symbol_kind, detail, \
                 start_line, start_col, end_line, end_col, diagnostics) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    diag_node_id,
                    "diagnostic",
                    diag.message,
                    diag.range.start.line,
                    diag.range.start.character,
                    diag.range.end.line,
                    diag.range.end.character,
                    diag_json,
                ],
            )?;
        }
    }

    Ok(matched)
}

// ── Extended projections ───────────────────────────────────────

/// Project go-to-definition results into `_lsp_defs` table.
///
/// **ADR-0013 Step 1** (ley-line-453f7e): writes `def_token` — the
/// textual symbol name — alongside the location. The token comes from
/// the caller because it's already in scope (the symbol name we
/// queried `definition` for); avoids re-extracting from source bytes.
pub fn project_definitions(
    conn: &Connection,
    node_id: &str,
    def_token: &str,
    locations: &[Location],
) -> Result<usize> {
    conn.execute_batch(LSP_DEFS_DDL)?;
    migrate_lsp_schema(conn)?;
    let mut count = 0;
    for loc in locations {
        conn.execute(
            "INSERT INTO _lsp_defs (node_id, def_token, def_uri, def_start_line, def_start_col, \
             def_end_line, def_end_col) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                node_id,
                def_token,
                loc.uri.as_str(),
                loc.range.start.line,
                loc.range.start.character,
                loc.range.end.line,
                loc.range.end.character,
            ],
        )?;
        count += 1;
    }
    Ok(count)
}

/// Project find-references results into `_lsp_refs` table.
///
/// **ADR-0013 Step 1** (ley-line-453f7e): writes three additional
/// columns at write time:
///
/// - `ref_token`: textual lemma at the ref site (`source[start..end]`).
///   Provided via `source_lookup(uri)` returning the file's bytes,
///   typically cached by caller across multiple refs in the same file.
/// - `referrer_node_id`: smallest AST node in `_ast` that contains the
///   ref position. Resolved via `_ast` query keyed by `source_id +
///   line/col`. NULL when the file isn't in `_ast` (cross-repo refs,
///   not-yet-parsed files).
/// - `node_id`: unchanged — still the *target* (the def this ref
///   points at). Renamed to `target_node_id` in ADR-0013 Step 4 in
///   lockstep with mache; preserved as `node_id` here for additive
///   migration.
///
/// `source_lookup` returns `None` for URIs whose bytes can't be
/// obtained (file deleted, permission denied, cross-host URI). In that
/// case `ref_token` falls through to empty string. Consumers querying
/// `WHERE ref_token != ''` filter out the gaps.
pub fn project_references(
    conn: &Connection,
    target_node_id: &str,
    locations: &[Location],
    source_lookup: &mut dyn FnMut(&str) -> Option<String>,
) -> Result<usize> {
    conn.execute_batch(LSP_REFS_DDL)?;
    migrate_lsp_schema(conn)?;
    let mut count = 0;
    for loc in locations {
        let uri = loc.uri.as_str();

        // Extract ref_token from source bytes. Empty fallback if the
        // file is unreachable (cross-repo refs, file deleted mid-pass).
        let ref_token = source_lookup(uri)
            .and_then(|bytes| extract_token_at_range(&bytes, &loc.range))
            .unwrap_or_default();

        // Resolve referrer_node_id via _ast. NULL when the file isn't
        // in _ast (cross-repo refs to dependencies, etc.). The call
        // takes the SAME conn that holds _ast — load-bearing.
        let referrer_node_id = lookup_referrer_node_id(conn, uri, &loc.range);

        conn.execute(
            "INSERT INTO _lsp_refs (node_id, referrer_node_id, ref_token, ref_uri, \
             ref_start_line, ref_start_col, ref_end_line, ref_end_col) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                target_node_id,
                referrer_node_id,
                ref_token,
                uri,
                loc.range.start.line,
                loc.range.start.character,
                loc.range.end.line,
                loc.range.end.character,
            ],
        )?;
        count += 1;
    }
    Ok(count)
}

/// Extract the textual token at an LSP `Range` from source bytes.
///
/// LSP ranges are line/character (UTF-16 code units, but we treat as
/// chars for the common ASCII case). For multi-line ranges we
/// concatenate the involved lines with `\n`. Returns `None` if the
/// range is out of bounds.
fn extract_token_at_range(source: &str, range: &protocol::Range) -> Option<String> {
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    let start_line = range.start.line as usize;
    let end_line = range.end.line as usize;
    if start_line >= lines.len() {
        return None;
    }
    let start_col = range.start.character as usize;
    let end_col = range.end.character as usize;

    // Single-line — common case (almost all references are within a single line).
    if start_line == end_line {
        let line = lines[start_line];
        // Strip trailing newline for char counting.
        let line_no_nl = line.strip_suffix('\n').unwrap_or(line);
        let chars: Vec<char> = line_no_nl.chars().collect();
        if start_col > chars.len() || end_col > chars.len() || start_col > end_col {
            return None;
        }
        return Some(chars[start_col..end_col].iter().collect());
    }

    // Multi-line — span across lines. Rare for token references.
    if end_line >= lines.len() {
        return None;
    }
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate().take(end_line + 1).skip(start_line) {
        let stripped = line.strip_suffix('\n').unwrap_or(line);
        let chars: Vec<char> = stripped.chars().collect();
        if i == start_line {
            if start_col > chars.len() {
                return None;
            }
            out.extend(&chars[start_col..]);
            out.push('\n');
        } else if i == end_line {
            if end_col > chars.len() {
                return None;
            }
            out.extend(&chars[..end_col]);
        } else {
            out.extend(&chars);
            out.push('\n');
        }
    }
    Some(out)
}

/// Resolve the `referrer_node_id` for a ref location: query `_ast`
/// for the smallest AST node containing `(line, col)` in the file
/// identified by `ref_uri`. Returns `None` if the URI can't be
/// translated to an `_ast.source_id` or no enclosing range exists.
///
/// The translation `ref_uri` → `_ast.source_id` goes through `_source`
/// where `path` is the absolute path. ADR-0013 Step 1's "byte-range
/// join at write time" — done once here so consumers don't need to
/// JOIN at query time.
fn lookup_referrer_node_id(conn: &Connection, ref_uri: &str, range: &protocol::Range) -> Option<String> {
    // Translate file:// URI to absolute path.
    let abs_path = ref_uri.strip_prefix("file://").unwrap_or(ref_uri);

    // Resolve to _source.id (the same string used as _ast.source_id).
    // _source.path is the absolute path; .id is what _ast keys on.
    let source_id: String = conn
        .query_row(
            "SELECT id FROM _source WHERE path = ?1 LIMIT 1",
            [abs_path],
            |r| r.get(0),
        )
        .ok()?;

    // Smallest AST node enclosing (line, col).
    let line = range.start.line as i64;
    let col = range.start.character as i64;
    conn.query_row(
        "SELECT node_id FROM _ast \
         WHERE source_id = ?1 \
           AND start_row <= ?2 AND end_row >= ?2 \
           AND (start_row < ?2 OR start_col <= ?3) \
           AND (end_row > ?2 OR end_col >= ?3) \
         ORDER BY (end_byte - start_byte) ASC \
         LIMIT 1",
        rusqlite::params![source_id, line, col],
        |r| r.get::<_, String>(0),
    )
    .ok()
}

/// Project hover result into `_lsp_hover` table.
pub fn project_hover(conn: &Connection, node_id: &str, hover: &Hover) -> Result<()> {
    conn.execute_batch(LSP_HOVER_DDL)?;
    let text = protocol::hover_to_plaintext(hover);
    conn.execute(
        "INSERT OR REPLACE INTO _lsp_hover (node_id, hover_text) VALUES (?1, ?2)",
        params![node_id, text],
    )?;
    Ok(())
}

/// Project completion items into `_lsp_completions` table.
pub fn project_completions(
    conn: &Connection,
    node_id: &str,
    items: &[CompletionItem],
) -> Result<usize> {
    conn.execute_batch(LSP_COMPLETIONS_DDL)?;
    let mut count = 0;
    for item in items {
        let kind_name = protocol::completion_kind_name(item.kind);
        let doc = item
            .documentation
            .as_ref()
            .map(protocol::completion_doc_text);
        conn.execute(
            "INSERT INTO _lsp_completions (node_id, label, kind, detail, documentation, sort_text) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                node_id,
                item.label,
                kind_name,
                item.detail,
                doc,
                item.sort_text,
            ],
        )?;
        count += 1;
    }
    Ok(count)
}

/// Project definitions into standalone nodes tree under `/definitions/{node_id}/`.
pub fn project_definitions_into_nodes(
    conn: &Connection,
    node_id: &str,
    locations: &[Location],
    mtime: i64,
) -> Result<()> {
    if locations.is_empty() {
        return Ok(());
    }
    // Both inserts use INSERT OR REPLACE so re-inserting an existing
    // dir-node is a no-op. Real errors (missing schema, locked db,
    // type mismatch) MUST propagate — silently swallowing them via
    // `let _ =` would let children land under a missing parent dir,
    // producing orphaned nodes that downstream walks can't navigate to.
    insert_node(conn, "definitions", "", "definitions", 1, 0, mtime, "")?;

    let parent_id = format!("definitions/{node_id}");
    insert_node(conn, &parent_id, "definitions", node_id, 1, 0, mtime, "")?;

    for (i, loc) in locations.iter().enumerate() {
        let def_id = format!("{parent_id}/{i}");
        let record = serde_json::json!({
            "uri": loc.uri.as_str(),
            "range": format!("{}:{}-{}:{}",
                loc.range.start.line, loc.range.start.character,
                loc.range.end.line, loc.range.end.character),
        });
        let record_str = record.to_string();
        insert_node(
            conn,
            &def_id,
            &parent_id,
            &format!("{i}"),
            0,
            record_str.len() as i64,
            mtime,
            &record_str,
        )?;
    }
    Ok(())
}

/// Project references into standalone nodes tree under `/references/{node_id}/`.
pub fn project_references_into_nodes(
    conn: &Connection,
    node_id: &str,
    locations: &[Location],
    mtime: i64,
) -> Result<()> {
    if locations.is_empty() {
        return Ok(());
    }
    // INSERT OR REPLACE is idempotent for the dir-nodes; propagate the
    // real errors (missing schema, locked db) instead of orphaning
    // children under a parent that failed to insert. See the matching
    // explanation in project_definitions_into_nodes.
    insert_node(conn, "references", "", "references", 1, 0, mtime, "")?;

    let parent_id = format!("references/{node_id}");
    insert_node(conn, &parent_id, "references", node_id, 1, 0, mtime, "")?;

    for (i, loc) in locations.iter().enumerate() {
        let ref_id = format!("{parent_id}/{i}");
        let record = serde_json::json!({
            "uri": loc.uri.as_str(),
            "range": format!("{}:{}-{}:{}",
                loc.range.start.line, loc.range.start.character,
                loc.range.end.line, loc.range.end.character),
        });
        let record_str = record.to_string();
        insert_node(
            conn,
            &ref_id,
            &parent_id,
            &format!("{i}"),
            0,
            record_str.len() as i64,
            mtime,
            &record_str,
        )?;
    }
    Ok(())
}

// ── Enrichment: query extended LSP data for each symbol ───────

/// Represents a flattened symbol with its node_id, selection position,
/// and the symbol's textual name (used as `def_token` per ADR-0013
/// Step 1).
pub struct SymbolPosition {
    pub node_id: String,
    pub line: u32,
    pub character: u32,
    /// The symbol's textual name (e.g., `"Read"`, `"foo"`). Captured
    /// at flatten time so `enrich_symbols` can pass it directly to
    /// `project_definitions` as the `def_token` without re-parsing.
    pub name: String,
}

/// Flatten a DocumentSymbol tree into (node_id, selection_range start) pairs.
pub fn flatten_symbols(symbols: &[DocumentSymbol], parent_id: &str) -> Vec<SymbolPosition> {
    let mut out = Vec::new();
    for sym in symbols {
        let id = format!("{parent_id}/{}", sym.name);
        out.push(SymbolPosition {
            node_id: id.clone(),
            line: sym.selection_range.start.line,
            character: sym.selection_range.start.character,
            name: sym.name.clone(),
        });
        if let Some(children) = &sym.children {
            out.extend(flatten_symbols(children, &id));
        }
    }
    out
}

/// Query definition, hover, references for each symbol and project into _lsp_* tables.
///
/// Completions are skipped in enrichment because they're position-contextual
/// (useful at edit time, not for static analysis snapshots).
///
/// **ADR-0013 Step 1** (ley-line-453f7e): maintains a per-URI source-
/// bytes cache so `project_references` can extract the textual lemma
/// at each ref site (`ref_token`) and resolve the AST node containing
/// the ref (`referrer_node_id` via `_ast` JOIN). Cache keyed by URI;
/// most refs land in the same file (fast path), cross-file refs miss
/// once and cache for subsequent siblings.
pub async fn enrich_symbols(
    client: &mut crate::client::LspClient,
    conn: &Connection,
    symbols: &[DocumentSymbol],
    file_uri: &str,
) -> Result<EnrichmentStats> {
    let positions = flatten_symbols(symbols, "symbols");
    let mut stats = EnrichmentStats::default();

    // Per-URI source-bytes cache for the duration of this enrichment
    // pass. Files referenced from multiple symbols are read once.
    // Lives on the heap (not in DaemonContext) so it's bounded by the
    // pass's lifetime — drops at function exit.
    let mut source_cache: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();

    for pos in &positions {
        // Definition — pass the symbol's textual name as `def_token`.
        match client.definition(file_uri, pos.line, pos.character).await {
            Ok(locs) if !locs.is_empty() => {
                stats.definitions += project_definitions(conn, &pos.node_id, &pos.name, &locs)?;
            }
            _ => {}
        }

        // Hover
        if let Ok(Some(hover)) = client.hover(file_uri, pos.line, pos.character).await {
            project_hover(conn, &pos.node_id, &hover)?;
            stats.hovers += 1;
        }

        // References — caller-supplied source_lookup closure for token
        // extraction. Cache miss reads the file, populates the entry
        // (`Some(bytes)` or `None` on read failure).
        match client.references(file_uri, pos.line, pos.character).await {
            Ok(locs) if !locs.is_empty() => {
                let mut lookup = |uri: &str| -> Option<String> {
                    if let Some(cached) = source_cache.get(uri) {
                        return cached.clone();
                    }
                    let abs_path = uri.strip_prefix("file://").unwrap_or(uri);
                    let bytes = std::fs::read_to_string(abs_path).ok();
                    source_cache.insert(uri.to_string(), bytes.clone());
                    bytes
                };
                stats.references += project_references(conn, &pos.node_id, &locs, &mut lookup)?;
            }
            _ => {}
        }
    }

    Ok(stats)
}

/// Stats from enrichment pass.
#[derive(Debug, Default)]
pub struct EnrichmentStats {
    pub definitions: usize,
    pub hovers: usize,
    pub references: usize,
}

impl std::fmt::Display for EnrichmentStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} defs, {} hovers, {} refs",
            self.definitions, self.hovers, self.references
        )
    }
}

// ── Private helpers ────────────────────────────────────────────

fn walk_symbol(conn: &Connection, sym: &DocumentSymbol, parent_id: &str, mtime: i64) -> Result<()> {
    let kind_name = protocol::symbol_kind_name(sym.kind);
    let id = format!("{parent_id}/{}", sym.name);
    let has_children = sym.children.as_ref().is_some_and(|c| !c.is_empty());

    let detail = sym.detail.as_deref().unwrap_or("");
    let record = serde_json::json!({
        "kind": kind_name,
        "detail": detail,
        "range": format!("{}:{}-{}:{}",
            sym.range.start.line, sym.range.start.character,
            sym.range.end.line, sym.range.end.character),
    });
    let record_str = record.to_string();

    let node_kind = if has_children { 1 } else { 0 };
    insert_node(
        conn,
        &id,
        parent_id,
        &sym.name,
        node_kind,
        record_str.len() as i64,
        mtime,
        &record_str,
    )?;

    // Also write to _lsp table
    conn.execute(
        "INSERT OR REPLACE INTO _lsp (node_id, symbol_kind, detail, \
         start_line, start_col, end_line, end_col) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            kind_name,
            detail,
            sym.range.start.line,
            sym.range.start.character,
            sym.range.end.line,
            sym.range.end.character,
        ],
    )?;

    if let Some(children) = &sym.children {
        for child in children {
            walk_symbol(conn, child, &id, mtime)?;
        }
    }

    Ok(())
}

fn merge_symbol(
    conn: &Connection,
    sym: &DocumentSymbol,
    has_ast: bool,
    diagnostics: &[Diagnostic],
) -> Result<usize> {
    let kind_name = protocol::symbol_kind_name(sym.kind);
    let detail = sym.detail.as_deref().unwrap_or("");
    let mut matched = 0;

    // Try to find matching AST node by line range
    let node_id = if has_ast {
        conn.query_row(
            "SELECT node_id FROM _ast \
             WHERE start_row = ?1 AND start_col <= ?2 \
               AND end_row >= ?3 \
             ORDER BY (end_byte - start_byte) ASC \
             LIMIT 1",
            params![
                sym.selection_range.start.line,
                sym.selection_range.start.character,
                sym.selection_range.end.line,
            ],
            |r| r.get::<_, String>(0),
        )
        .ok()
    } else {
        None
    };

    let effective_id = node_id.unwrap_or_else(|| {
        format!(
            "_lsp/{}:{}",
            sym.range.start.line, sym.range.start.character
        )
    });

    if !effective_id.starts_with("_lsp/") {
        matched += 1;
    }

    // Collect diagnostics that fall within this symbol's range
    let sym_diags: Vec<_> = diagnostics
        .iter()
        .filter(|d| {
            d.range.start.line >= sym.range.start.line && d.range.end.line <= sym.range.end.line
        })
        .collect();
    let diag_json = if sym_diags.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&sym_diags)?)
    };

    conn.execute(
        "INSERT OR REPLACE INTO _lsp (node_id, symbol_kind, detail, \
         start_line, start_col, end_line, end_col, diagnostics) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            effective_id,
            kind_name,
            detail,
            sym.range.start.line,
            sym.range.start.character,
            sym.range.end.line,
            sym.range.end.character,
            diag_json,
        ],
    )?;

    // Recurse into children
    if let Some(children) = &sym.children {
        for child in children {
            matched += merge_symbol(conn, child, has_ast, diagnostics)?;
        }
    }

    Ok(matched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Position, Range, SymbolKind, Url};
    use rusqlite::DatabaseName;
    use std::io::Cursor;

    #[test]
    fn create_lsp_schema_creates_all_indexes() {
        // Scale-problem pin. The 4 _lsp* indexes accelerate the
        // hot-path MCP queries (find_callers, find_defs, hover) on
        // populated repos. The helm/charts ingest hit idx_parent_name
        // at 185 MB doing real work; LSP indexes scale similarly when
        // a real language server populates _lsp_refs/defs across a
        // 50k-symbol corpus. A refactor that DROP'd any of these from
        // their _DDL would silently degrade query latency to full-
        // table scan. Pin existence directly via sqlite_master.
        let conn = Connection::open_in_memory().unwrap();
        create_lsp_schema(&conn).unwrap();
        for index_name in [
            "idx_lsp_kind",
            "idx_lsp_defs_node",
            "idx_lsp_refs_node",
            "idx_lsp_completions_node",
        ] {
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='index' AND name=?1",
                    [index_name],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(exists, "missing LSP index: {index_name}");
        }
    }

    fn make_symbol(
        name: &str,
        kind: SymbolKind,
        start_line: u32,
        end_line: u32,
        children: Vec<DocumentSymbol>,
    ) -> DocumentSymbol {
        #[allow(deprecated)] // tags field is deprecated but required
        DocumentSymbol {
            name: name.to_string(),
            detail: Some(format!("{name}() -> None")),
            kind,
            tags: None,
            deprecated: None,
            range: Range {
                start: Position {
                    line: start_line,
                    character: 0,
                },
                end: Position {
                    line: end_line,
                    character: 0,
                },
            },
            selection_range: Range {
                start: Position {
                    line: start_line,
                    character: 4,
                },
                end: Position {
                    line: start_line,
                    character: 4 + name.len() as u32,
                },
            },
            children: Some(children),
        }
    }

    fn make_diag(msg: &str, severity: DiagnosticSeverity, line: u32) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position { line, character: 0 },
                end: Position {
                    line,
                    character: 10,
                },
            },
            severity: Some(severity),
            code: None,
            code_description: None,
            source: Some("test".to_string()),
            message: msg.to_string(),
            related_information: None,
            tags: None,
            data: None,
        }
    }

    fn make_location(uri: &str, line: u32, col: u32) -> Location {
        Location {
            uri: Url::parse(uri).unwrap(),
            range: Range {
                start: Position {
                    line,
                    character: col,
                },
                end: Position {
                    line,
                    character: col + 5,
                },
            },
        }
    }

    #[test]
    fn project_symbols_standalone() {
        let symbols = vec![
            make_symbol("load_model", SymbolKind::FUNCTION, 5, 20, vec![]),
            make_symbol(
                "MyClass",
                SymbolKind::CLASS,
                22,
                50,
                vec![
                    make_symbol("__init__", SymbolKind::METHOD, 23, 30, vec![]),
                    make_symbol("forward", SymbolKind::METHOD, 32, 48, vec![]),
                ],
            ),
        ];
        let diagnostics = vec![
            make_diag("unused variable 'x'", DiagnosticSeverity::WARNING, 10),
            make_diag("syntax error", DiagnosticSeverity::ERROR, 25),
        ];

        let bytes = project_lsp(&symbols, &diagnostics, "file:///test.py").unwrap();
        assert!(!bytes.is_empty());

        let mut conn = Connection::open_in_memory().unwrap();
        conn.deserialize_read_exact(DatabaseName::Main, Cursor::new(&bytes), bytes.len(), true)
            .unwrap();

        // Check symbol hierarchy
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE id LIKE 'symbols/%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 4); // load_model, MyClass, __init__, forward

        // Check MyClass is a directory with children
        let kind: i32 = conn
            .query_row(
                "SELECT kind FROM nodes WHERE id = 'symbols/MyClass'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kind, 1);

        // Check _lsp table populated
        let lsp_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _lsp", [], |r| r.get(0))
            .unwrap();
        assert_eq!(lsp_count, 4);

        // Check diagnostics grouped by severity
        let err_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE parent_id = 'diagnostics/error'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(err_count, 1);

        let warn_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE parent_id = 'diagnostics/warning'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(warn_count, 1);
    }

    #[test]
    fn lsp_table_has_line_ranges() {
        let symbols = vec![make_symbol("main", SymbolKind::FUNCTION, 0, 10, vec![])];

        let bytes = project_lsp(&symbols, &[], "test.py").unwrap();
        let mut conn = Connection::open_in_memory().unwrap();
        conn.deserialize_read_exact(DatabaseName::Main, Cursor::new(&bytes), bytes.len(), true)
            .unwrap();

        let (kind, start, end): (String, i64, i64) = conn
            .query_row(
                "SELECT symbol_kind, start_line, end_line FROM _lsp WHERE node_id = 'symbols/main'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(kind, "function");
        assert_eq!(start, 0);
        assert_eq!(end, 10);
    }

    #[test]
    fn merge_into_ast_db() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS _ast (
                node_id TEXT PRIMARY KEY,
                source_id TEXT NOT NULL,
                node_kind TEXT NOT NULL,
                start_byte INTEGER NOT NULL,
                end_byte INTEGER NOT NULL,
                start_row INTEGER NOT NULL,
                start_col INTEGER NOT NULL,
                end_row INTEGER NOT NULL,
                end_col INTEGER NOT NULL
            );",
        )
        .unwrap();

        insert_node(&conn, "", "", "", 1, 0, 0, "").unwrap();
        insert_node(
            &conn,
            "function_definition",
            "",
            "function_definition",
            1,
            0,
            0,
            "",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO _ast VALUES ('function_definition', 'test.py', 'function_definition', 100, 500, 5, 0, 20, 0)",
            [],
        )
        .unwrap();

        let symbols = vec![make_symbol(
            "load_model",
            SymbolKind::FUNCTION,
            5,
            20,
            vec![],
        )];
        let diags = vec![make_diag("unused import", DiagnosticSeverity::WARNING, 8)];

        let matched = merge_lsp_into_ast(&symbols, &diags, &conn).unwrap();
        assert_eq!(matched, 1);

        let (node_id, kind): (String, String) = conn
            .query_row(
                "SELECT node_id, symbol_kind FROM _lsp WHERE symbol_kind = 'function'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(node_id, "function_definition");
        assert_eq!(kind, "function");

        let diag_json: Option<String> = conn
            .query_row(
                "SELECT diagnostics FROM _lsp WHERE node_id = 'function_definition'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(diag_json.is_some());
        assert!(diag_json.unwrap().contains("unused import"));
    }

    #[test]
    fn project_definitions_table() {
        let conn = Connection::open_in_memory().unwrap();
        let locs = vec![
            make_location("file:///src/lib.rs", 10, 4),
            make_location("file:///src/util.rs", 42, 0),
        ];

        let count = project_definitions(&conn, "my_func", "my_func", &locs).unwrap();
        assert_eq!(count, 2);

        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _lsp_defs WHERE node_id = 'my_func'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 2);

        let uri: String = conn
            .query_row(
                "SELECT def_uri FROM _lsp_defs WHERE node_id = 'my_func' ORDER BY def_start_line LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(uri, "file:///src/lib.rs");

        // ADR-0013 Step 1: def_token populated.
        let token: String = conn
            .query_row(
                "SELECT def_token FROM _lsp_defs WHERE node_id = 'my_func' LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(token, "my_func", "def_token must equal the symbol name");
    }

    #[test]
    fn project_references_table() {
        let conn = Connection::open_in_memory().unwrap();
        let locs = vec![
            make_location("file:///a.py", 5, 0),
            make_location("file:///b.py", 15, 8),
            make_location("file:///c.py", 100, 2),
        ];

        // No source bytes available (no real files); ref_token falls
        // through to empty. Per ADR-0013 contract: ref_token defaults
        // to '' on lookup failure, never NULL.
        let mut nop_lookup = |_uri: &str| -> Option<String> { None };
        let count = project_references(&conn, "my_var", &locs, &mut nop_lookup).unwrap();
        assert_eq!(count, 3);

        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _lsp_refs WHERE node_id = 'my_var'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 3);
    }

    /// ADR-0013 Step 1 (ley-line-453f7e): `extract_token_at_range`
    /// reads the textual lemma from source bytes given an LSP range.
    /// Pin single-line behavior — the common case for token references.
    #[test]
    fn extract_token_single_line() {
        use crate::protocol::{Position, Range};
        let src = "fn foo() {\n    bar(baz);\n}\n";
        // bar at line 1, col 4..7
        let range = Range {
            start: Position { line: 1, character: 4 },
            end: Position { line: 1, character: 7 },
        };
        assert_eq!(extract_token_at_range(src, &range).as_deref(), Some("bar"));
    }

    /// ADR-0013 Step 1: extract_token_at_range handles multi-line
    /// ranges by concatenating with `\n`. Rare but real for some
    /// languages (Python multi-line statements via parens).
    #[test]
    fn extract_token_multi_line() {
        use crate::protocol::{Position, Range};
        let src = "abc\ndef\nghi\n";
        // From line 0 col 1 to line 2 col 2 → "bc\ndef\ngh"
        let range = Range {
            start: Position { line: 0, character: 1 },
            end: Position { line: 2, character: 2 },
        };
        assert_eq!(
            extract_token_at_range(src, &range).as_deref(),
            Some("bc\ndef\ngh"),
        );
    }

    /// ADR-0013 Step 1: out-of-bounds ranges return None (rather than
    /// truncating or panicking). Pin so a refactor that accidentally
    /// returns garbage on bad ranges surfaces here.
    #[test]
    fn extract_token_out_of_bounds_returns_none() {
        use crate::protocol::{Position, Range};
        let src = "abc\n";
        let too_far = Range {
            start: Position { line: 0, character: 0 },
            end: Position { line: 99, character: 0 },
        };
        assert!(extract_token_at_range(src, &too_far).is_none());

        let off_end = Range {
            start: Position { line: 0, character: 5 },
            end: Position { line: 0, character: 6 },
        };
        assert!(extract_token_at_range(src, &off_end).is_none());
    }

    /// ADR-0013 Step 1: project_references with a working
    /// source_lookup populates `ref_token` from the bytes provided.
    /// Pin the round-trip: lookup returns bytes → extract token →
    /// stored in `ref_token` column.
    #[test]
    fn project_references_populates_ref_token() {
        use std::collections::HashMap;
        let conn = Connection::open_in_memory().unwrap();

        // ref at line 1, col 4..7 of "fn foo() {\n    bar(baz);\n}\n"
        // → token "bar"
        let locs = vec![make_location("file:///main.go", 1, 4)];
        // Adjust the location's range to span "bar" (4..7).
        let mut locs = locs;
        locs[0].range.end.character = 7;

        let bytes_by_uri: HashMap<String, String> = HashMap::from([(
            "file:///main.go".to_string(),
            "fn foo() {\n    bar(baz);\n}\n".to_string(),
        )]);
        let mut lookup = |uri: &str| bytes_by_uri.get(uri).cloned();

        project_references(&conn, "fn_foo", &locs, &mut lookup).unwrap();

        let token: String = conn
            .query_row(
                "SELECT ref_token FROM _lsp_refs WHERE node_id = 'fn_foo' LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            token, "bar",
            "ADR-0013 Step 1: ref_token must be populated from source bytes",
        );
    }

    /// ADR-0013 Step 1: when source_lookup returns None (file
    /// unavailable, cross-repo URI), ref_token defaults to empty
    /// string — NEVER NULL. Pin so consumer queries on `ref_token != ''`
    /// stay correct.
    #[test]
    fn project_references_ref_token_defaults_empty_on_lookup_miss() {
        let conn = Connection::open_in_memory().unwrap();
        let locs = vec![make_location("file:///unreachable.rs", 0, 0)];
        let mut nop_lookup = |_uri: &str| -> Option<String> { None };

        project_references(&conn, "n0", &locs, &mut nop_lookup).unwrap();

        let token: String = conn
            .query_row(
                "SELECT ref_token FROM _lsp_refs WHERE node_id = 'n0' LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(token, "", "ref_token must default to empty, not NULL");
    }

    /// ADR-0013 Step 1: migrate_lsp_schema is idempotent — calling
    /// it on a fresh db (via create_lsp_schema) and then again does
    /// not error. Pin so a refactor that makes ALTER TABLE non-idempotent
    /// surfaces here.
    #[test]
    fn migrate_lsp_schema_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        create_lsp_schema(&conn).unwrap();
        // Second call must not error. (CREATE TABLE IF NOT EXISTS +
        // pragma_table_info-guarded ALTER TABLE = idempotent.)
        create_lsp_schema(&conn).unwrap();
        // And the columns are present.
        let has_def_token: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM pragma_table_info('_lsp_defs') WHERE name='def_token'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let has_ref_token: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM pragma_table_info('_lsp_refs') WHERE name='ref_token'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let has_referrer: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM pragma_table_info('_lsp_refs') WHERE name='referrer_node_id'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(has_def_token, "_lsp_defs.def_token must exist");
        assert!(has_ref_token, "_lsp_refs.ref_token must exist");
        assert!(has_referrer, "_lsp_refs.referrer_node_id must exist");
    }

    /// ADR-0013 Step 1: a pre-existing pre-ADR-0013 schema (built
    /// before this commit) gets the new columns added on first call
    /// to `create_lsp_schema`. Pin the in-place migration path.
    #[test]
    fn migrate_lsp_schema_adds_columns_to_old_tables() {
        let conn = Connection::open_in_memory().unwrap();
        // Simulate the old schema (no def_token / ref_token /
        // referrer_node_id columns).
        conn.execute_batch(
            "CREATE TABLE _lsp_defs (
                node_id TEXT NOT NULL,
                def_uri TEXT NOT NULL,
                def_start_line INTEGER NOT NULL,
                def_start_col INTEGER NOT NULL,
                def_end_line INTEGER NOT NULL,
                def_end_col INTEGER NOT NULL
            );
            CREATE TABLE _lsp_refs (
                node_id TEXT NOT NULL,
                ref_uri TEXT NOT NULL,
                ref_start_line INTEGER NOT NULL,
                ref_start_col INTEGER NOT NULL,
                ref_end_line INTEGER NOT NULL,
                ref_end_col INTEGER NOT NULL
            );",
        )
        .unwrap();

        // Insert pre-existing data (must survive the migration).
        conn.execute(
            "INSERT INTO _lsp_defs (node_id, def_uri, def_start_line, def_start_col, def_end_line, def_end_col) \
             VALUES ('legacy_def', 'file:///old.go', 0, 0, 0, 5)",
            [],
        )
        .unwrap();

        // Trigger the migration.
        create_lsp_schema(&conn).unwrap();

        // Old data preserved.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _lsp_defs WHERE node_id = 'legacy_def'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "legacy data must survive migration");

        // New column present + default empty for legacy row.
        let token: String = conn
            .query_row("SELECT def_token FROM _lsp_defs WHERE node_id = 'legacy_def'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(token, "", "legacy rows have empty def_token (DEFAULT '')");
    }

    #[test]
    fn project_hover_table() {
        use lsp_types::{HoverContents, MarkupContent, MarkupKind};

        let conn = Connection::open_in_memory().unwrap();
        let hover = Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::PlainText,
                value: "def load_model(path: str) -> Model".to_string(),
            }),
            range: None,
        };

        project_hover(&conn, "load_model", &hover).unwrap();

        let text: String = conn
            .query_row(
                "SELECT hover_text FROM _lsp_hover WHERE node_id = 'load_model'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(text, "def load_model(path: str) -> Model");
    }

    #[test]
    fn project_completions_table() {
        let conn = Connection::open_in_memory().unwrap();
        let items = vec![
            CompletionItem {
                label: "append".to_string(),
                kind: Some(lsp_types::CompletionItemKind::METHOD),
                detail: Some("list.append(x)".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "extend".to_string(),
                kind: Some(lsp_types::CompletionItemKind::METHOD),
                detail: Some("list.extend(iterable)".to_string()),
                ..Default::default()
            },
        ];

        let count = project_completions(&conn, "L10C5", &items).unwrap();
        assert_eq!(count, 2);

        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _lsp_completions WHERE node_id = 'L10C5'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 2);

        let (label, kind): (String, String) = conn
            .query_row(
                "SELECT label, kind FROM _lsp_completions WHERE label = 'append'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(label, "append");
        assert_eq!(kind, "method");
    }
}
