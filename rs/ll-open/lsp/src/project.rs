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

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::time::{SystemTime, UNIX_EPOCH};

use leyline_schema::create_schema;

use crate::protocol::{
    self, CompletionItem, Diagnostic, DiagnosticSeverity, DocumentSymbol, Hover, Location,
};

pub const LSP_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _lsp (
    nid INTEGER PRIMARY KEY,
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
    nid INTEGER NOT NULL,
    def_token TEXT NOT NULL DEFAULT '',
    def_uri TEXT NOT NULL,
    def_start_line INTEGER NOT NULL,
    def_start_col INTEGER NOT NULL,
    def_end_line INTEGER NOT NULL,
    def_end_col INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_lsp_defs_node ON _lsp_defs(nid);
CREATE INDEX IF NOT EXISTS idx_lsp_defs_token ON _lsp_defs(def_token);";

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
    nid INTEGER NOT NULL,
    referrer_nid INTEGER,
    ref_token TEXT NOT NULL DEFAULT '',
    ref_uri TEXT NOT NULL,
    ref_start_line INTEGER NOT NULL,
    ref_start_col INTEGER NOT NULL,
    ref_end_line INTEGER NOT NULL,
    ref_end_col INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_lsp_refs_node ON _lsp_refs(nid);
CREATE INDEX IF NOT EXISTS idx_lsp_refs_referrer ON _lsp_refs(referrer_nid);
CREATE INDEX IF NOT EXISTS idx_lsp_refs_token ON _lsp_refs(ref_token);";

// The pre-ADR-0013 `migrate_lsp_schema` ALTER pass is gone as of
// projection-v5: every column it stamped is in the base DDL above, and a
// pre-v5 arena is refused at open (`_meta.projection_schema_version`
// mismatch) rather than migrated in place.

pub const LSP_HOVER_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _lsp_hover (
    nid INTEGER PRIMARY KEY,
    hover_text TEXT NOT NULL
);";

pub const LSP_COMPLETIONS_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _lsp_completions (
    nid INTEGER NOT NULL,
    label TEXT NOT NULL,
    kind TEXT,
    detail TEXT,
    documentation TEXT,
    sort_text TEXT
);
CREATE INDEX IF NOT EXISTS idx_lsp_completions_node ON _lsp_completions(nid);";

/// Create the full schema for LSP projection (nodes + all _lsp* tables).
pub fn create_lsp_schema(conn: &Connection) -> Result<()> {
    create_schema(conn)?;
    conn.execute_batch(LSP_DDL)?;
    conn.execute_batch(LSP_DEFS_DDL)?;
    conn.execute_batch(LSP_REFS_DDL)?;
    conn.execute_batch(LSP_HOVER_DDL)?;
    conn.execute_batch(LSP_COMPLETIONS_DDL)?;
    Ok(())
}

// ── Standalone-tree minting (projection-v5) ─────────────────────────────
//
// The standalone LSP projection builds a tree of NAMED things — `symbols/`,
// `diagnostics/error/0`, `definitions/…` — which is exactly what the
// `dirs` + `names` interning chain models. Every standalone node therefore
// lives in directory nid space (`nid = -dir_id`): stable per (parent, name),
// renderable by `node_path`, resolvable by `resolve_path`, and structurally
// outside every file's `(file_id << 24)` range, so per-file cleanup can
// never collide with it. Name collisions (C++/TS overloads — bead
// `ley-line-open-5d3cb6`) collapse to one address here exactly as they did
// under the path scheme; `INSERT OR REPLACE` keeps the last and the
// collection-vs-written warning below reports the loss.

/// Intern one standalone tree node under `parent_dir_id` and upsert its
/// presentation row. Returns the node's `dir_id` (its nid is the negation).
fn standalone_node(
    conn: &Connection,
    parent_dir_id: i64,
    name: &str,
    kind: i32,
    size: i64,
    mtime: i64,
    record: &str,
) -> Result<i64> {
    let name_id = leyline_schema::intern_name(conn, name)?;
    conn.execute(
        "INSERT OR IGNORE INTO dirs (parent_dir_id, name_id) VALUES (?1, ?2)",
        params![parent_dir_id, name_id],
    )?;
    let dir_id: i64 = conn.query_row(
        "SELECT dir_id FROM dirs WHERE parent_dir_id = ?1 AND name_id = ?2",
        params![parent_dir_id, name_id],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO nodes (nid, parent_nid, name_id, kind, ord, size, mtime, record) \
         VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?7)",
        params![
            leyline_schema::dir_nid(dir_id),
            leyline_schema::dir_nid(parent_dir_id),
            name_id,
            kind,
            size,
            mtime,
            record
        ],
    )?;
    Ok(dir_id)
}

/// Intern a whole `/`-separated standalone path (creating links as needed)
/// and return the leaf's nid. Container links created along the way get
/// kind=1 and an empty record.
fn standalone_chain(conn: &Connection, path: &str, mtime: i64) -> Result<i64> {
    let mut cur: i64 = 1; // root
    ensure_root_node(conn, mtime)?;
    for comp in path.split('/') {
        cur = standalone_node(conn, cur, comp, 1, 0, mtime, "")?;
    }
    Ok(leyline_schema::dir_nid(cur))
}

/// Make sure the root directory's presentation row exists.
fn ensure_root_node(conn: &Connection, mtime: i64) -> Result<()> {
    let root_name = leyline_schema::intern_name(conn, "")?;
    conn.execute(
        "INSERT OR IGNORE INTO nodes (nid, parent_nid, name_id, kind, ord, mtime, record) \
         VALUES (-1, NULL, ?1, 1, 0, ?2, '')",
        params![root_name, mtime],
    )?;
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
    let data = conn.serialize("main")?;
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
    ensure_root_node(conn, mtime)?;

    // /symbols — document symbol hierarchy
    let symbols_dir = standalone_node(conn, 1, "symbols", 1, 0, mtime, "")?;
    let collected = walk_symbols(conn, symbols, symbols_dir, mtime)?;

    // ASSERT THE MECHANISM FIRED (bead `ley-line-open-2607d2`).
    //
    // `walk_symbol` keys `_lsp` on `{parent_id}/{name}`, so two constructs
    // sharing a name in one scope — C++ / TypeScript overloads, TS declaration
    // merging — collide, and `INSERT OR REPLACE` keeps the last. mache measured
    // it against real servers: clangd emits 3 `DocumentSymbol`s for 3 overloads
    // of `add` and exactly one row survived (bead `ley-line-open-5d3cb6`).
    //
    // The counters the CLI prints ("N symbols collected") describe the
    // COLLECTION pass, so they reported 3 while 1 was written. A metric derived
    // from a mechanism must assert the mechanism fired; this compares intent
    // against outcome and says so when they disagree.
    //
    // Deliberately a WARNING, not an error. Overloads are ordinary in C++ and
    // TypeScript, so failing here would break every parse of those languages
    // for a defect in the ADDRESS SCHEME, not in the source. Whether the fix is
    // a discriminator or an explicitly many-to-one address is an ADR-0034
    // amendment (`ley-line-open-5d3cb6`); until it lands, silent loss becomes
    // reported loss.
    let written: usize = conn
        .query_row("SELECT COUNT(*) FROM _lsp", [], |r| r.get::<_, i64>(0))
        .map(|n| n as usize)
        .unwrap_or(collected);
    if written < collected {
        eprintln!(
            "warn: {} of {collected} symbols reached _lsp — {} dropped by \
             name collision (overloads/declaration merging). See \
             ley-line-open-5d3cb6; addresses are not unique per occurrence.",
            written,
            collected - written,
        );
    }

    // /diagnostics — flat list keyed by severity + index
    if !diagnostics.is_empty() {
        let diags_dir = standalone_node(conn, 1, "diagnostics", 1, 0, mtime, "")?;

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

            let group_dir = standalone_node(conn, diags_dir, severity_label, 1, 0, mtime, "")?;

            for (i, diag) in matching.iter().enumerate() {
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
                standalone_node(
                    conn,
                    group_dir,
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

    // Insert diagnostics that didn't match any symbol. Their synthetic
    // addresses live in standalone (directory) nid space — `_diag/L{l}C{c}`
    // interned per position — since no AST node owns them.
    let mtime = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    for diag in diagnostics {
        let line = diag.range.start.line;
        let col = diag.range.start.character;
        let diag_nid = standalone_chain(conn, &format!("_diag/L{line}C{col}"), mtime)?;

        let already_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM _lsp WHERE nid = ?1",
                params![diag_nid],
                |r| r.get(0),
            )
            .unwrap_or(false);

        if !already_exists {
            let diag_json = serde_json::to_string(&[diag])?;
            conn.execute(
                "INSERT OR IGNORE INTO _lsp (nid, symbol_kind, detail, \
                 start_line, start_col, end_line, end_col, diagnostics) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    diag_nid,
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
    nid: i64,
    def_token: &str,
    locations: &[Location],
) -> Result<usize> {
    conn.execute_batch(LSP_DEFS_DDL)?;
    let mut count = 0;
    for loc in locations {
        conn.execute(
            "INSERT INTO _lsp_defs (nid, def_token, def_uri, def_start_line, def_start_col, \
             def_end_line, def_end_col) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                nid,
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
    binding_log: Option<&std::path::Path>,
) -> Result<usize> {
    // ley-line-open-6b332d: legacy `_lsp_refs` schema is created/migrated for
    // *backwards compatibility* — old `.db` files written by pre-ley-line-open-6b332d
    // LLO have rows in this table, and consumers (mache + others) may
    // still read them as a legacy fallback. New writes go to the
    // BindingRecord capnp event log only. The DDL stays so SELECTs
    // against the table don't error on a fresh `.db`.
    conn.execute_batch(LSP_REFS_DDL)?;

    // ley-line-open-cdcae2: open the binding event log for append. `None` skips the
    // dual-write (e.g. tests, :memory: connections without a path).
    let mut binding_writer = match binding_log {
        Some(p) => Some(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .with_context(|| format!("open binding event log {}", p.display()))?,
        ),
        None => None,
    };

    let mut count = 0;
    for loc in locations {
        let uri = loc.uri.as_str();

        // ley-line-open-6af0b8: source bytes feed both ref_token AND qualifier; fetch
        // once per location (the upstream closure caches per-URI).
        let bytes_opt = source_lookup(uri);

        // Extract ref_token from source bytes. Empty fallback if the
        // file is unreachable (cross-repo refs, file deleted mid-pass).
        let ref_token = bytes_opt
            .as_ref()
            .and_then(|b| extract_token_at_range(b, &loc.range))
            .unwrap_or_default();

        // ley-line-open-6af0b8: extract qualifier from source bytes. Empty when the
        // ref is a bare-identifier call (no preceding `.`), when the
        // file isn't reachable, or when the byte before the ref site
        // isn't a dot.
        let qualifier = bytes_opt
            .as_ref()
            .and_then(|b| extract_qualifier(b, &loc.range))
            .unwrap_or_default();

        // ley-line-open-cdcae2: feed the BindingRecord's `refSiteNodeId` — smallest
        // enclosing AST node at the ref site (typically a leaf
        // identifier). `None` when the file isn't in `_ast` (cross-
        // repo refs to dependencies, etc.).
        let referrer_node_id = lookup_referrer_node_id(conn, uri, &loc.range);

        // ley-line-open-cdcae2: feed the BindingRecord's `constructNodeId` — smallest
        // enclosing function/method/constructor. `find_callers` MCP and
        // `node_refs` shape want construct level, not leaf. This is the
        // disambiguation that Falsifiability B's 100% mismatch revealed
        // at the SQL boundary (cdcae2 alignment-options comment,
        // 2026-05-08).
        let construct_node_id = lookup_construct_node_id(conn, uri, &loc.range);

        // ley-line-open-6b332d: the typed BindingRecord IS the contract. SQL
        // `_lsp_refs` writes have been retired — schema-as-protocol
        // failure modes (be6136-class) are now structurally precluded
        // because no SQL surface exists for producer/consumer to
        // disagree on column-by-column.
        if let Some(w) = binding_writer.as_mut() {
            write_binding_record(
                w,
                target_node_id,
                &ref_token,
                construct_node_id.as_deref().unwrap_or(""),
                referrer_node_id.as_deref().unwrap_or(""),
                uri,
                &loc.range,
                &qualifier,
            )?;
        }

        count += 1;
    }
    Ok(count)
}

/// ley-line-open-6af0b8: extract the qualifier (LHS of a `selector_expression`) for a
/// ref location, by scanning source bytes immediately before the ref
/// site. Returns `Some("pkg")` for `pkg.Method`, `Some("obj")` for
/// `obj.method`, `None` for bare-identifier calls.
///
/// Pure text scan — no AST or LSP query. This is correct for the
/// structural distinction the consumer wants (qualified vs unqualified)
/// and is intentionally agnostic to whether the qualifier names a
/// package, an object, or a chained selector. Chained selectors
/// `a.b.c` resolve qualifier to the immediate predecessor (`b`),
/// matching how `selector_expression` nests in tree-sitter.
fn extract_qualifier(source: &str, range: &protocol::Range) -> Option<String> {
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    let line_idx = range.start.line as usize;
    if line_idx >= lines.len() {
        return None;
    }
    let line_start_offset: usize = lines[..line_idx].iter().map(|l| l.len()).sum();
    let col = range.start.character as usize;
    let abs_offset = line_start_offset + col;

    let bytes = source.as_bytes();
    if abs_offset == 0 || abs_offset > bytes.len() {
        return None;
    }
    // The byte immediately before the ref site must be a `.`.
    if bytes[abs_offset - 1] != b'.' {
        return None;
    }

    // Scan backwards through identifier chars to find the qualifier.
    let dot_pos = abs_offset - 1;
    let mut start = dot_pos;
    while start > 0 {
        let c = bytes[start - 1];
        if c.is_ascii_alphanumeric() || c == b'_' {
            start -= 1;
        } else {
            break;
        }
    }
    if start == dot_pos {
        return None; // dot present but no identifier preceding
    }
    std::str::from_utf8(&bytes[start..dot_pos])
        .ok()
        .map(|s| s.to_string())
}

/// AST node kinds that count as a "callable construct" for
/// `constructNodeId` resolution. Curated per language; intentionally
/// excludes inline lambdas (`arrow_function`, `anonymous_function`,
/// `lambda`) — `find_callers` UX wants the named scope, not the
/// closure that hosts the call.
///
/// Adding a language: append the kind names tree-sitter produces for
/// "directly callable named scope." Don't add class/struct/impl —
/// those are container kinds, not constructs (a different layer
/// consumers can derive on their own by walking parents).
const CONSTRUCT_KINDS: &[&str] = &[
    // Go
    "function_declaration",
    "method_declaration",
    // Python
    "function_definition",
    // Rust
    "function_item",
    // TypeScript / JavaScript
    "method_definition",
    // Java / Kotlin / C#
    "constructor_declaration",
];

/// ley-line-open-cdcae2: resolve the smallest enclosing function/method/constructor
/// node at the ref site. Mirrors `lookup_referrer_node_id` but filters
/// by `node_kind IN CONSTRUCT_KINDS`. Returns `None` when the file
/// isn't projected, when the ref sits outside any construct (top-level
/// declarations), or when the source language uses construct kinds we
/// don't yet recognize.
fn lookup_construct_node_id(
    conn: &Connection,
    ref_uri: &str,
    range: &protocol::Range,
) -> Option<String> {
    let abs_path = ref_uri.strip_prefix("file://").unwrap_or(ref_uri);
    // projection-v5: `_source.file_id` bounds the file's nid range, so the
    // span search is a PK range SEARCH instead of a source_id scan.
    let file_id: i64 = conn
        .query_row(
            "SELECT file_id FROM _source WHERE path = ?1 LIMIT 1",
            [abs_path],
            |r| r.get(0),
        )
        .ok()?;
    let (lo, hi) = leyline_schema::file_nid_range(file_id);

    let line = range.start.line as i64;
    let col = range.start.character as i64;

    // Build the SQL `IN (...)` placeholders from CONSTRUCT_KINDS at
    // call time — keeping the kind list in one place (the const
    // above) avoids drift between the schema's documented set and the
    // SQL filter.
    let placeholders = (0..CONSTRUCT_KINDS.len())
        .map(|i| format!("?{}", i + 5))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT a.nid FROM _ast a JOIN kinds k ON k.kind_id = a.kind_id \
         WHERE a.nid BETWEEN ?1 AND ?2 \
           AND a.start_row <= ?3 AND a.end_row >= ?3 \
           AND (a.start_row < ?3 OR a.start_col <= ?4) \
           AND (a.end_row > ?3 OR a.end_col >= ?4) \
           AND k.raw_kind IN ({placeholders}) \
         ORDER BY (a.end_byte - a.start_byte) ASC, a.nid ASC \
         LIMIT 1"
    );

    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> =
        vec![Box::new(lo), Box::new(hi), Box::new(line), Box::new(col)];
    for kind in CONSTRUCT_KINDS {
        params_vec.push(Box::new(*kind));
    }
    let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();

    let nid: i64 = conn
        .query_row(&sql, params_refs.as_slice(), |r| r.get(0))
        .ok()?;
    // The BindingRecord wire contract carries the node's DISPLAY path (a
    // Text field, path-shaped since T8.2) — render it; the integer key
    // stays projection-internal.
    leyline_schema::node_path(conn, nid).ok().flatten()
}

/// ley-line-open-cdcae2: serialize a single BindingRecord and append it to the binding
/// event log. The log is plain capnp framed messages back-to-back —
/// readers iterate via `capnp::serialize::read_message` until EOF.
///
/// Per the post-RTFM canonical-encoding commitment in ADR-0014, the
/// producer writes via `set_root_canonical` so the on-disk bytes are
/// byte-stable across additive schema changes. This is what makes
/// Σ root advance only when the *projection's actual content* changes,
/// not when the schema gains a default-valued field.
#[allow(clippy::too_many_arguments)] // private helper; refactoring to a
// struct would shuffle 8 single-use values for no payoff. Each parameter
// is named at the call site (single caller in project_references) so the
// readability is preserved.
fn write_binding_record(
    writer: &mut std::fs::File,
    target_node_id: &str,
    ref_token: &str,
    construct_node_id: &str,
    ref_site_node_id: &str,
    ref_uri: &str,
    range: &protocol::Range,
    qualifier: &str,
) -> Result<()> {
    use leyline_schema_capnp::binding_capnp::binding_record;

    let mut src = capnp::message::Builder::new_default();
    {
        let mut rec: binding_record::Builder = src.init_root();
        rec.set_target_node_id(target_node_id);
        rec.set_ref_token(ref_token);
        rec.set_construct_node_id(construct_node_id);
        rec.set_ref_site_node_id(ref_site_node_id);
        rec.set_ref_uri(ref_uri);
        // parseGen left at 0 for now; bead `ley-line-open-ce55b1`
        // wires it to the Σ root advance. Note: capnp `parseGen` is
        // a per-segment counter, distinct from the V1 substrate
        // `generation` field that v0.2.0 removed from the public API.
        rec.set_parse_gen(0);
        // ley-line-open-6af0b8: qualifier — empty string for bare-identifier calls.
        rec.set_qualifier(qualifier);
        let mut r = rec.init_ref_range();
        {
            let mut s = r.reborrow().init_start();
            s.set_line(range.start.line);
            s.set_column(range.start.character);
            s.set_byte(0); // byte offset not available from LSP Range
        }
        {
            let mut e = r.reborrow().init_end();
            e.set_line(range.end.line);
            e.set_column(range.end.character);
            e.set_byte(0);
        }
    }

    leyline_schema_capnp::canonical::write_canonical_message::<binding_record::Owned, _>(
        &src, writer,
    )
    .context("write BindingRecord to event log")?;
    Ok(())
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
fn lookup_referrer_node_id(
    conn: &Connection,
    ref_uri: &str,
    range: &protocol::Range,
) -> Option<String> {
    // Translate file:// URI to absolute path.
    let abs_path = ref_uri.strip_prefix("file://").unwrap_or(ref_uri);

    // projection-v5: `_source.file_id` bounds the file's nid range — the
    // span search below is a PRIMARY KEY range SEARCH.
    let file_id: i64 = conn
        .query_row(
            "SELECT file_id FROM _source WHERE path = ?1 LIMIT 1",
            [abs_path],
            |r| r.get(0),
        )
        .ok()?;
    let (lo, hi) = leyline_schema::file_nid_range(file_id);

    // Smallest AST node enclosing (line, col).
    let line = range.start.line as i64;
    let col = range.start.character as i64;
    let nid: i64 = conn
        .query_row(
            "SELECT nid FROM _ast \
             WHERE nid BETWEEN ?1 AND ?2 \
               AND start_row <= ?3 AND end_row >= ?3 \
               AND (start_row < ?3 OR start_col <= ?4) \
               AND (end_row > ?3 OR end_col >= ?4) \
             ORDER BY (end_byte - start_byte) ASC, nid ASC \
             LIMIT 1",
            rusqlite::params![lo, hi, line, col],
            |r| r.get(0),
        )
        .ok()?;
    // Wire contract: the BindingRecord's refSiteNodeId is a display path.
    leyline_schema::node_path(conn, nid).ok().flatten()
}

/// Project hover result into `_lsp_hover` table.
pub fn project_hover(conn: &Connection, nid: i64, hover: &Hover) -> Result<()> {
    conn.execute_batch(LSP_HOVER_DDL)?;
    let text = protocol::hover_to_plaintext(hover);
    conn.execute(
        "INSERT OR REPLACE INTO _lsp_hover (nid, hover_text) VALUES (?1, ?2)",
        params![nid, text],
    )?;
    Ok(())
}

/// Project completion items into `_lsp_completions` table.
pub fn project_completions(conn: &Connection, nid: i64, items: &[CompletionItem]) -> Result<usize> {
    conn.execute_batch(LSP_COMPLETIONS_DDL)?;
    let mut count = 0;
    for item in items {
        let kind_name = protocol::completion_kind_name(item.kind);
        let doc = item
            .documentation
            .as_ref()
            .map(protocol::completion_doc_text);
        conn.execute(
            "INSERT INTO _lsp_completions (nid, label, kind, detail, documentation, sort_text) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![nid, item.label, kind_name, item.detail, doc, item.sort_text,],
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
    // Standalone-space minting is idempotent per (parent, name). Real
    // errors (missing schema, locked db, type mismatch) MUST propagate —
    // silently swallowing them would let children land under a missing
    // parent dir, producing orphans downstream walks can't navigate to.
    let parent_nid = standalone_chain(conn, &format!("definitions/{node_id}"), mtime)?;
    let parent_dir = -parent_nid;

    for (i, loc) in locations.iter().enumerate() {
        let record = serde_json::json!({
            "uri": loc.uri.as_str(),
            "range": format!("{}:{}-{}:{}",
                loc.range.start.line, loc.range.start.character,
                loc.range.end.line, loc.range.end.character),
        });
        let record_str = record.to_string();
        standalone_node(
            conn,
            parent_dir,
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
    // See the matching explanation in project_definitions_into_nodes.
    let parent_nid = standalone_chain(conn, &format!("references/{node_id}"), mtime)?;
    let parent_dir = -parent_nid;

    for (i, loc) in locations.iter().enumerate() {
        let record = serde_json::json!({
            "uri": loc.uri.as_str(),
            "range": format!("{}:{}-{}:{}",
                loc.range.start.line, loc.range.start.character,
                loc.range.end.line, loc.range.end.character),
        });
        let record_str = record.to_string();
        standalone_node(
            conn,
            parent_dir,
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
    binding_log: Option<&std::path::Path>,
) -> Result<EnrichmentStats> {
    let positions = flatten_symbols(symbols, "symbols");
    let mut stats = EnrichmentStats::default();

    // Per-URI source-bytes cache for the duration of this enrichment
    // pass. Files referenced from multiple symbols are read once.
    // Lives on the heap (not in DaemonContext) so it's bounded by the
    // pass's lifetime — drops at function exit.
    let mut source_cache: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();

    let mtime = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    for pos in &positions {
        // projection-v5: the symbol's standalone address ("symbols/…") is
        // interned into directory nid space — minted here whether or not the
        // standalone tree was projected first, which preserves the pre-v5
        // behaviour of `_lsp_defs`/`_lsp_hover` keys being standalone-space
        // addresses in every mode.
        let pos_nid = standalone_chain(conn, &pos.node_id, mtime)?;

        // Definition — pass the symbol's textual name as `def_token`.
        match client.definition(file_uri, pos.line, pos.character).await {
            Ok(locs) if !locs.is_empty() => {
                stats.definitions += project_definitions(conn, pos_nid, &pos.name, &locs)?;
            }
            _ => {}
        }

        // Hover
        if let Ok(Some(hover)) = client.hover(file_uri, pos.line, pos.character).await {
            project_hover(conn, pos_nid, &hover)?;
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
                stats.references +=
                    project_references(conn, &pos.node_id, &locs, &mut lookup, binding_log)?;
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

/// Disambiguate same-named siblings, per the locator role in ADR-0034 and the
/// impossibility result on `ley-line-open-5d3cb6`.
///
/// `{parent_id}/{name}` is not unique: C++ and TypeScript permit overloads, and
/// TS permits declaration merging, so a scope can hold several constructs with
/// one name. Stored through `INSERT OR REPLACE` into a `PRIMARY KEY`, the
/// survivors overwrite each other — clangd emitted 3 symbols for 3 overloads of
/// `add` and one row reached `_lsp`.
///
/// The address is built in two tiers, and the order matters:
///
/// 1. **δ — a discriminator.** Overloads are DISCERNIBLE, so they must not be
///    separated by position. Here δ is the server's own `detail` (clangd gives
///    `long (long, long)`), which is legitimate *in this projection*: `_lsp` is
///    the LSP enrichment pass, already server-derived by construction, so
///    `detail` is local data rather than an external dependency. ADR-0034 D5
///    rules `detail` out as SUBSTRATE identity — that still holds; this is the
///    locator for a server-derived table, not the substrate address.
///
/// 2. **cohort-ordinal — position, and only where position is all there is.**
///    Applied ONLY within a cohort δ cannot separate. The impossibility proof
///    shows some ordinal is unavoidable for byte-identical twins: inserting an
///    identical copy above or below yields the same file, so no snapshot-local
///    function can tell the two positions apart. Restricting the ordinal to
///    indiscernible cohorts concedes exactly the stability the theorem forces
///    and no more.
///
/// Singletons keep the bare `{parent_id}/{name}` — the overwhelming majority of
/// symbols, and every language that cannot overload, are untouched.
fn sibling_names(syms: &[DocumentSymbol]) -> Vec<String> {
    use std::collections::HashMap;

    let mut by_name: HashMap<&str, usize> = HashMap::new();
    for s in syms {
        *by_name.entry(s.name.as_str()).or_insert(0) += 1;
    }

    // Within a colliding name, how many share each δ? A δ-group of one is
    // discernible and needs no ordinal.
    let mut by_delta: HashMap<(&str, &str), usize> = HashMap::new();
    for s in syms {
        if by_name[s.name.as_str()] > 1 {
            *by_delta
                .entry((s.name.as_str(), s.detail.as_deref().unwrap_or("")))
                .or_insert(0) += 1;
        }
    }

    let mut seen: HashMap<(&str, &str), usize> = HashMap::new();
    let mut out = Vec::with_capacity(syms.len());
    for s in syms {
        let name = s.name.as_str();
        if by_name[name] == 1 {
            out.push(name.to_string());
            continue;
        }
        let detail = s.detail.as_deref().unwrap_or("");
        let delta = sanitize_delta(detail);
        let key = (name, detail);
        let nth = seen.entry(key).or_insert(0);
        // Ordinal only inside an indiscernible cohort.
        let disambiguated = if by_delta[&key] == 1 {
            format!("{name}#{delta}")
        } else {
            format!("{name}#{delta}~{nth}")
        };
        *nth += 1;
        out.push(disambiguated);
    }
    out
}

/// `/` separates path segments and `#`/`~` delimit the discriminator, so a δ
/// carrying them would make the id ambiguous. Collapse whitespace too: servers
/// format signatures inconsistently across versions, and an id that changes
/// because clangd added a space is a rebind with no cause.
fn sanitize_delta(detail: &str) -> String {
    let collapsed: String = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.replace(['/', '#', '~'], "_")
}

/// Insert a sibling group, disambiguating same-named members first.
///
/// Ids MUST be computed over the whole group: whether `add` needs a
/// discriminator is a property of its siblings, not of `add` alone. Deriving
/// the id inside `walk_symbol` — one symbol at a time, blind to its peers — is
/// what made the collision unrepresentable and therefore silent.
fn walk_symbols(
    conn: &Connection,
    syms: &[DocumentSymbol],
    parent_dir_id: i64,
    mtime: i64,
) -> Result<usize> {
    let names = sibling_names(syms);
    let mut visited = 0_usize;
    for (sym, name) in syms.iter().zip(names.iter()) {
        visited += walk_symbol(conn, sym, parent_dir_id, name, mtime)?;
    }
    Ok(visited)
}

fn walk_symbol(
    conn: &Connection,
    sym: &DocumentSymbol,
    parent_dir_id: i64,
    name: &str,
    mtime: i64,
) -> Result<usize> {
    let kind_name = protocol::symbol_kind_name(sym.kind);
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
    let dir_id = standalone_node(
        conn,
        parent_dir_id,
        name,
        node_kind,
        record_str.len() as i64,
        mtime,
        &record_str,
    )?;

    // Also write to _lsp table, keyed by the standalone node's nid.
    conn.execute(
        "INSERT OR REPLACE INTO _lsp (nid, symbol_kind, detail, \
         start_line, start_col, end_line, end_col) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            leyline_schema::dir_nid(dir_id),
            kind_name,
            detail,
            sym.range.start.line,
            sym.range.start.character,
            sym.range.end.line,
            sym.range.end.character,
        ],
    )?;

    let mut visited = 1_usize;
    if let Some(children) = &sym.children {
        visited += walk_symbols(conn, children, dir_id, mtime)?;
    }

    Ok(visited)
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
    let ast_nid: Option<i64> = if has_ast {
        conn.query_row(
            "SELECT nid FROM _ast \
             WHERE start_row = ?1 AND start_col <= ?2 \
               AND end_row >= ?3 \
             ORDER BY (end_byte - start_byte) ASC, nid ASC \
             LIMIT 1",
            params![
                sym.selection_range.start.line,
                sym.selection_range.start.character,
                sym.selection_range.end.line,
            ],
            |r| r.get(0),
        )
        .ok()
    } else {
        None
    };

    let effective_nid = match ast_nid {
        Some(nid) => {
            matched += 1;
            nid
        }
        // Unmatched symbols get a synthetic standalone-space address —
        // `_lsp/{line}:{col}` interned into dirs, exactly the shape the
        // pre-v5 string key spelled out.
        None => {
            let mtime = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            standalone_chain(
                conn,
                &format!(
                    "_lsp/{}:{}",
                    sym.range.start.line, sym.range.start.character
                ),
                mtime,
            )?
        }
    };

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
        "INSERT OR REPLACE INTO _lsp (nid, symbol_kind, detail, \
         start_line, start_col, end_line, end_col, diagnostics) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            effective_nid,
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
    use leyline_schema::insert_node;
    use std::str::FromStr;

    use crate::protocol::{Position, Range, SymbolKind, Uri};

    use std::io::Cursor;

    /// Resolve a rendered display path to its nid — the projection-v5 read
    /// boundary the daemon and mache use.
    fn resolve(conn: &Connection, path: &str) -> i64 {
        leyline_schema::resolve_path(conn, path)
            .unwrap()
            .unwrap_or_else(|| panic!("path must resolve: {path:?}"))
    }

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

    /// Bead `ley-line-open-5d3cb6`, confirmed by mache against real servers:
    /// clangd emits 3 `DocumentSymbol`s for 3 C++ overloads of `add`, and
    /// typescript-language-server does the same for TS overloads — but only
    /// ONE row reached `_lsp`, the last one written.
    ///
    /// Cause: `walk_symbol` keys on `{parent_id}/{name}`, and `_lsp.node_id`
    /// was `TEXT PRIMARY KEY`, so overloads collide by construction and
    /// `INSERT OR REPLACE` keeps the survivor.
    ///
    /// This is the silent-success class (`ley-line-open-2607d2`) in its purest
    /// form: the CLI printed "3 symbols collected, 3 defs" while writing one.
    /// Those counters describe the COLLECTION pass, so any assertion on them
    /// passes while the data is dropped — which is why this test counts ROWS
    /// rather than trusting the reported total.
    ///
    /// TypeScript is the real exposure, not C++: overloads and declaration
    /// merging are ordinary TS, and that one server covers
    /// typescript/tsx/javascript/jsx.
    #[test]
    fn overloaded_symbols_all_reach_the_lsp_table() {
        // Three same-named siblings at distinct ranges — the shape clangd and
        // typescript-language-server both produce for overloads.
        let symbols = vec![
            make_symbol("add", SymbolKind::FUNCTION, 1, 3, vec![]),
            make_symbol("add", SymbolKind::FUNCTION, 5, 7, vec![]),
            make_symbol("add", SymbolKind::FUNCTION, 9, 11, vec![]),
        ];

        let bytes = project_lsp(&symbols, &[], "file:///overloads.cpp").unwrap();
        let mut conn = Connection::open_in_memory().unwrap();
        conn.deserialize_read_exact("main", Cursor::new(&bytes), bytes.len(), true)
            .unwrap();

        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM _lsp", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            rows,
            symbols.len() as i64,
            "every collected symbol must reach _lsp; overloads must not \
             overwrite each other",
        );

        // And they must remain distinguishable — a row per overload is only
        // useful if its own span survived.
        // The `nodes` projection must not drop them either — walk_symbol
        // writes both tables from the same address.
        let node_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM v_node_path WHERE path LIKE 'symbols/add%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            node_rows, 3,
            "the nodes projection must keep every overload"
        );

        let spans: i64 = conn
            .query_row(
                "SELECT COUNT(DISTINCT l.start_line) FROM _lsp l \
                 JOIN v_node_path p ON p.nid = l.nid WHERE p.path LIKE '%add%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(spans, 3, "each overload must keep its own start_line");
    }

    /// The δ tier, and the property that justifies it (bead
    /// `ley-line-open-5d3cb6`).
    ///
    /// DISCERNIBLE overloads — distinct signatures, which is the ordinary case
    /// in C++ and TypeScript — must be separated by δ and NOT by position. The
    /// test that matters is reorder: swapping two overloads in the file must
    /// leave both addresses untouched. An ordinal scheme (`add_0`, `add_1`)
    /// fails this, which is exactly why ADR-0034 D4's "node_id disambiguates"
    /// clause conceded more stability than the impossibility theorem requires.
    #[test]
    fn discernible_overloads_are_separated_by_delta_not_position() {
        let mk = |detail: &str, line: u32| {
            let mut s = make_symbol("add", SymbolKind::FUNCTION, line, line + 1, vec![]);
            s.detail = Some(detail.to_string());
            s
        };

        let ids_of = |syms: &[DocumentSymbol]| -> Vec<String> {
            let bytes = project_lsp(syms, &[], "file:///ov.cpp").unwrap();
            let mut conn = Connection::open_in_memory().unwrap();
            conn.deserialize_read_exact("main", Cursor::new(&bytes), bytes.len(), true)
                .unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT p.path FROM _lsp l JOIN v_node_path p ON p.nid = l.nid \
                     ORDER BY p.path",
                )
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect::<Vec<_>>()
        };

        let forward = vec![
            mk("int (int, int)", 1),
            mk("long (long, long)", 5),
            mk("double (double, double)", 9),
        ];
        let ids = ids_of(&forward);
        assert_eq!(ids.len(), 3, "all three overloads must persist: {ids:?}");

        // δ, not position: no ordinal suffix appears when δ already separates.
        for id in &ids {
            assert!(
                !id.contains('~'),
                "discernible overloads must not carry a cohort ordinal: {id}",
            );
        }

        // The load-bearing property. Reorder the SAME three overloads; every
        // address must be identical, because none of them moved scope or
        // changed signature.
        let reordered = vec![
            mk("double (double, double)", 1),
            mk("int (int, int)", 5),
            mk("long (long, long)", 9),
        ];
        assert_eq!(
            ids,
            ids_of(&reordered),
            "δ addresses must survive reordering — this is what an ordinal \
             scheme cannot do, and why δ is tried first",
        );
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
            uri: Uri::from_str(uri).unwrap(),
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
        conn.deserialize_read_exact("main", Cursor::new(&bytes), bytes.len(), true)
            .unwrap();

        // Check symbol hierarchy
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM v_node_path WHERE path LIKE 'symbols/%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 4); // load_model, MyClass, __init__, forward

        // Check MyClass is a container with children
        let kind: i32 = conn
            .query_row(
                "SELECT kind FROM nodes WHERE nid = ?1",
                [resolve(&conn, "symbols/MyClass")],
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
                "SELECT COUNT(*) FROM nodes WHERE parent_nid = ?1",
                [resolve(&conn, "diagnostics/error")],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(err_count, 1);

        let warn_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE parent_nid = ?1",
                [resolve(&conn, "diagnostics/warning")],
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
        conn.deserialize_read_exact("main", Cursor::new(&bytes), bytes.len(), true)
            .unwrap();

        let (kind, start, end): (String, i64, i64) = conn
            .query_row(
                "SELECT symbol_kind, start_line, end_line FROM _lsp WHERE nid = ?1",
                [resolve(&conn, "symbols/main")],
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
                nid INTEGER PRIMARY KEY,
                kind_id INTEGER NOT NULL,
                start_byte INTEGER NOT NULL,
                end_byte INTEGER NOT NULL,
                start_row INTEGER NOT NULL,
                start_col INTEGER NOT NULL,
                end_row INTEGER NOT NULL,
                end_col INTEGER NOT NULL
            );",
        )
        .unwrap();

        // Mint a one-file, one-construct v5 fixture: file test.py, its
        // function_definition at ordinal 1.
        let file_id = leyline_schema::ensure_file_id(&conn, "test.py").unwrap();
        let base = leyline_schema::file_nid(file_id, 0);
        let fn_nid = base + 1;
        let k_fn = leyline_schema::intern_kind(&conn, "python", "function_definition").unwrap();
        let name_id = leyline_schema::intern_name(&conn, "test.py").unwrap();
        insert_node(&conn, base, Some(-1), Some(name_id), None, 1, 0, 0, 0, "").unwrap();
        insert_node(&conn, fn_nid, Some(base), None, Some(k_fn), 1, 0, 0, 0, "").unwrap();
        conn.execute(
            "INSERT INTO _ast VALUES (?1, ?2, 100, 500, 5, 0, 20, 0)",
            params![fn_nid, k_fn],
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

        let (nid, kind): (i64, String) = conn
            .query_row(
                "SELECT nid, symbol_kind FROM _lsp WHERE symbol_kind = 'function'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(nid, fn_nid, "the LSP row must key on the matched AST nid");
        assert_eq!(kind, "function");

        let diag_json: Option<String> = conn
            .query_row(
                "SELECT diagnostics FROM _lsp WHERE nid = ?1",
                [fn_nid],
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

        let my_func_nid = 4242;
        let count = project_definitions(&conn, my_func_nid, "my_func", &locs).unwrap();
        assert_eq!(count, 2);

        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _lsp_defs WHERE nid = ?1",
                [my_func_nid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 2);

        let uri: String = conn
            .query_row(
                "SELECT def_uri FROM _lsp_defs WHERE nid = ?1 ORDER BY def_start_line LIMIT 1",
                [my_func_nid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(uri, "file:///src/lib.rs");

        // ADR-0013 Step 1: def_token populated.
        let token: String = conn
            .query_row(
                "SELECT def_token FROM _lsp_defs WHERE nid = ?1 LIMIT 1",
                [my_func_nid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(token, "my_func", "def_token must equal the symbol name");
    }

    /// ley-line-open-6b332d: count returned by project_references reflects the number
    /// of BindingRecords emitted (or *would have been emitted* if a
    /// binding_log path were provided). With ley-line-open-6b332d retiring the SQL
    /// writer, the SQL `_lsp_refs` count is no longer a meaningful
    /// proxy — the function's return value IS the contract.
    #[test]
    fn project_references_count_matches_locations() {
        let conn = Connection::open_in_memory().unwrap();
        let locs = vec![
            make_location("file:///a.py", 5, 0),
            make_location("file:///b.py", 15, 8),
            make_location("file:///c.py", 100, 2),
        ];
        let mut nop_lookup = |_uri: &str| -> Option<String> { None };
        let count = project_references(&conn, "my_var", &locs, &mut nop_lookup, None).unwrap();
        assert_eq!(count, 3, "one record per Location");
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
            start: Position {
                line: 1,
                character: 4,
            },
            end: Position {
                line: 1,
                character: 7,
            },
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
            start: Position {
                line: 0,
                character: 1,
            },
            end: Position {
                line: 2,
                character: 2,
            },
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
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 99,
                character: 0,
            },
        };
        assert!(extract_token_at_range(src, &too_far).is_none());

        let off_end = Range {
            start: Position {
                line: 0,
                character: 5,
            },
            end: Position {
                line: 0,
                character: 6,
            },
        };
        assert!(extract_token_at_range(src, &off_end).is_none());
    }

    /// ADR-0013 Step 1: project_references with a working
    /// source_lookup populates `ref_token` from the bytes provided.
    /// Pin the round-trip: lookup returns bytes → extract token →
    /// stored in `ref_token` column.
    /// ADR-0013 Step 1 + ley-line-open-6b332d: ref_token is populated from source
    /// bytes, surfaced in the BindingRecord capnp event log (the
    /// post-ley-line-open-6b332d contract; SQL `_lsp_refs` writes are retired).
    #[test]
    fn project_references_populates_ref_token() {
        use leyline_schema_capnp::binding_capnp::binding_record;
        use std::collections::HashMap;
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("test.bindings.capnp");
        let conn = Connection::open_in_memory().unwrap();

        let locs = vec![make_location("file:///main.go", 1, 4)];
        let mut locs = locs;
        locs[0].range.end.character = 7;

        let bytes_by_uri: HashMap<String, String> = HashMap::from([(
            "file:///main.go".to_string(),
            "fn foo() {\n    bar(baz);\n}\n".to_string(),
        )]);
        let mut lookup = |uri: &str| bytes_by_uri.get(uri).cloned();

        project_references(&conn, "fn_foo", &locs, &mut lookup, Some(&log_path)).unwrap();

        let bytes = std::fs::read(&log_path).unwrap();
        let mut slice: &[u8] = &bytes;
        let msg = capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
            .unwrap();
        let rec: binding_record::Reader = msg.get_root().unwrap();
        assert_eq!(
            rec.get_ref_token().unwrap().to_str().unwrap(),
            "bar",
            "ADR-0013 Step 1: ref_token populated from source bytes",
        );
    }

    /// ley-line-open-cdcae2: when binding_log = Some(path), each location emits a
    /// capnp BindingRecord readable back. Pin the round-trip and
    /// the parity invariant: every SQL `_lsp_refs` row has a
    /// corresponding capnp record with the same target/refUri/range.
    #[test]
    fn project_references_dual_writes_capnp_binding_log() {
        use leyline_schema_capnp::binding_capnp::binding_record;
        use std::collections::HashMap;
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("test.bindings.capnp");
        let conn = Connection::open_in_memory().unwrap();

        // Seed _ast + _source so construct/refSite resolution exercises
        // the full lookup paths (not just the SQL writes). The v5 fixture
        // mints one file with a function_declaration wrapping an identifier;
        // the wire values below are their RENDERED display paths.
        leyline_schema::create_schema(&conn).unwrap();
        conn.execute_batch(
            "CREATE TABLE _source (id TEXT PRIMARY KEY, language TEXT, path TEXT, file_id INTEGER UNIQUE);
             CREATE TABLE _ast (
                 nid INTEGER PRIMARY KEY,
                 kind_id INTEGER NOT NULL,
                 start_byte INTEGER NOT NULL,
                 end_byte INTEGER NOT NULL,
                 start_row INTEGER NOT NULL,
                 start_col INTEGER NOT NULL,
                 end_row INTEGER NOT NULL,
                 end_col INTEGER NOT NULL
             );",
        )
        .unwrap();
        let file_id = leyline_schema::ensure_file_id(&conn, "main.go").unwrap();
        leyline_schema::ensure_dir_nodes(&conn, "main.go", 0).unwrap();
        let base = leyline_schema::file_nid(file_id, 0);
        let name_id = leyline_schema::intern_name(&conn, "main.go").unwrap();
        let k_fn = leyline_schema::intern_kind(&conn, "go", "function_declaration").unwrap();
        let k_ident = leyline_schema::intern_kind(&conn, "go", "identifier").unwrap();
        conn.execute(
            "INSERT INTO _source VALUES ('main.go', 'go', '/canon/main.go', ?1)",
            [file_id],
        )
        .unwrap();
        insert_node(&conn, base, Some(-1), Some(name_id), None, 1, 0, 0, 0, "").unwrap();
        insert_node(
            &conn,
            base + 1,
            Some(base),
            None,
            Some(k_fn),
            1,
            0,
            0,
            0,
            "",
        )
        .unwrap();
        insert_node(
            &conn,
            base + 2,
            Some(base + 1),
            None,
            Some(k_ident),
            0,
            0,
            0,
            0,
            "",
        )
        .unwrap();
        // Function (construct level) wraps the inner identifier (leaf).
        conn.execute(
            "INSERT INTO _ast VALUES \
             (?1, ?2, 0, 100, 0, 0, 3, 0), \
             (?3, ?4, 30, 33, 1, 4, 1, 7)",
            params![base + 1, k_fn, base + 2, k_ident],
        )
        .unwrap();

        let mut locs = vec![make_location("file:///canon/main.go", 1, 4)];
        locs[0].range.end.character = 7;

        let bytes_by_uri: HashMap<String, String> = HashMap::from([(
            "file:///canon/main.go".to_string(),
            "fn foo() {\n    bar(baz);\n}\n".to_string(),
        )]);
        let mut lookup = |uri: &str| bytes_by_uri.get(uri).cloned();

        project_references(&conn, "fn_bar", &locs, &mut lookup, Some(&log_path)).unwrap();

        // Read the capnp log back.
        let mut bytes: &[u8] = &std::fs::read(&log_path).unwrap();
        let msg = capnp::serialize::read_message(&mut bytes, capnp::message::ReaderOptions::new())
            .unwrap();
        let rec: binding_record::Reader = msg.get_root().unwrap();

        assert_eq!(
            rec.get_target_node_id().unwrap().to_str().unwrap(),
            "fn_bar"
        );
        assert_eq!(rec.get_ref_token().unwrap().to_str().unwrap(), "bar");
        assert_eq!(
            rec.get_construct_node_id().unwrap().to_str().unwrap(),
            "main.go/function_declaration",
            "ley-line-open-cdcae2: constructNodeId resolves to the rendered \
             path of the function_declaration",
        );
        assert_eq!(
            rec.get_ref_site_node_id().unwrap().to_str().unwrap(),
            "main.go/function_declaration/identifier",
            "ley-line-open-cdcae2: refSiteNodeId resolves to the rendered \
             path of the leaf identifier",
        );
        assert_eq!(
            rec.get_ref_uri().unwrap().to_str().unwrap(),
            "file:///canon/main.go",
        );
        let r = rec.get_ref_range().unwrap();
        let s = r.get_start().unwrap();
        let e = r.get_end().unwrap();
        assert_eq!(s.get_line(), 1);
        assert_eq!(s.get_column(), 4);
        assert_eq!(e.get_column(), 7);

        // ley-line-open-6af0b8: qualifier — bare-identifier call `bar(...)` has no
        // preceding `.`, so qualifier is empty.
        assert_eq!(
            rec.get_qualifier().unwrap().to_str().unwrap(),
            "",
            "ley-line-open-6af0b8: bare-identifier call has empty qualifier",
        );

        // ley-line-open-6b332d: SQL `_lsp_refs` parity assertion has been retired
        // along with the SQL writer. The capnp record IS the
        // contract. The previous parity check (SQL `referrer_node_id`
        // == capnp `refSiteNodeId`) had no failure mode after ley-line-open-6b332d —
        // there's no second source of truth to disagree.
    }

    /// ley-line-open-6af0b8: extract_qualifier picks up `pkg` from `pkg.Method` —
    /// the simplest qualified-call shape Go/Rust/TS produce.
    #[test]
    fn extract_qualifier_basic() {
        // "  pkg.Method(arg)" — Method starts at col 6
        let src = "  pkg.Method(arg)\n";
        let mut range = make_location("file:///x", 0, 6).range;
        range.end.character = 12; // span "Method"
        assert_eq!(extract_qualifier(src, &range).as_deref(), Some("pkg"));
    }

    /// ley-line-open-6af0b8: chained selector `a.b.c` — qualifier of `c` is `b`,
    /// the *immediate* predecessor (matches selector_expression nesting).
    #[test]
    fn extract_qualifier_chained_returns_immediate() {
        let src = "x = a.b.c()\n";
        // c starts at col 8
        let mut range = make_location("file:///x", 0, 8).range;
        range.end.character = 9;
        assert_eq!(extract_qualifier(src, &range).as_deref(), Some("b"));
    }

    /// ley-line-open-6af0b8: bare-identifier call `Foo()` has no preceding dot — None.
    #[test]
    fn extract_qualifier_bare_call_returns_none() {
        let src = "Foo()\n";
        let mut range = make_location("file:///x", 0, 0).range;
        range.end.character = 3;
        assert!(extract_qualifier(src, &range).is_none());
    }

    /// ley-line-open-6af0b8: dot present but no identifier before it (e.g. mid-string,
    /// truncated source). Treat as no qualifier rather than crash.
    #[test]
    fn extract_qualifier_orphan_dot_returns_none() {
        let src = ".Foo()\n";
        let mut range = make_location("file:///x", 0, 1).range;
        range.end.character = 4;
        assert!(extract_qualifier(src, &range).is_none());
    }

    /// ley-line-open-6af0b8: out-of-range location (LSP gave a position past EOF) —
    /// safe-None, not panic. Defensive against producer/consumer drift.
    #[test]
    fn extract_qualifier_out_of_bounds_returns_none() {
        let src = "abc\n";
        let mut range = make_location("file:///x", 5, 0).range;
        range.end.character = 5;
        assert!(extract_qualifier(src, &range).is_none());
    }

    /// ley-line-open-6af0b8: end-to-end — when source bytes are present and the call
    /// is qualified, the BindingRecord written to the capnp log
    /// carries the qualifier. mache-42118e closes structurally:
    /// `fan_out_skew` becomes a qualifier-aware structural metric
    /// without an internal AST rewalker.
    #[test]
    fn project_references_writes_qualifier_when_qualified() {
        use leyline_schema_capnp::binding_capnp::binding_record;
        use std::collections::HashMap;
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("test.bindings.capnp");
        let conn = Connection::open_in_memory().unwrap();

        let mut locs = vec![make_location("file:///canon/main.go", 0, 4)];
        // Span the "Method" portion of "pkg.Method(arg)".
        locs[0].range.start.character = 4;
        locs[0].range.end.character = 10;

        let bytes_by_uri: HashMap<String, String> = HashMap::from([(
            "file:///canon/main.go".to_string(),
            "pkg.Method(arg)\n".to_string(),
        )]);
        let mut lookup = |uri: &str| bytes_by_uri.get(uri).cloned();

        project_references(&conn, "tgt", &locs, &mut lookup, Some(&log_path)).unwrap();

        let mut bytes: &[u8] = &std::fs::read(&log_path).unwrap();
        let msg = capnp::serialize::read_message(&mut bytes, capnp::message::ReaderOptions::new())
            .unwrap();
        let rec: binding_record::Reader = msg.get_root().unwrap();
        assert_eq!(rec.get_ref_token().unwrap().to_str().unwrap(), "Method");
        assert_eq!(
            rec.get_qualifier().unwrap().to_str().unwrap(),
            "pkg",
            "ley-line-open-6af0b8: qualifier round-trips through capnp dual-write",
        );
    }

    /// ley-line-open-cdcae2: lookup_construct_node_id finds the smallest enclosing
    /// function/method/constructor. Pin the per-language kind set
    /// (CONSTRUCT_KINDS) by exercising all of them.
    #[test]
    fn lookup_construct_node_id_per_language() {
        let cases = [
            ("function_declaration", "go_fn"),
            ("method_declaration", "go_method"),
            ("function_definition", "py_fn"),
            ("function_item", "rs_fn"),
            ("method_definition", "ts_method"),
            ("constructor_declaration", "java_ctor"),
        ];

        for (kind, _label) in cases {
            let conn = Connection::open_in_memory().unwrap();
            let (base, _) = v5_single_construct_fixture(&conn, kind);
            let mut range = make_location("file:///x/f", 1, 4).range;
            range.end.character = 7;
            // The wire value is the RENDERED path of the construct node —
            // "f/<kind>" for a singleton child of file "f".
            assert_eq!(
                lookup_construct_node_id(&conn, "file:///x/f", &range).as_deref(),
                Some(format!("f/{kind}").as_str()),
                "construct kind {kind} must resolve (fixture base {base})",
            );
        }
    }

    /// Build a minimal projection-v5 arena with one file "f" containing one
    /// construct node of `kind` (ordinal 1) and one `identifier` inside it
    /// (ordinal 2), spans matching the pre-v5 fixture. Returns
    /// `(base_nid, construct_nid)`.
    fn v5_single_construct_fixture(conn: &Connection, kind: &str) -> (i64, i64) {
        leyline_schema::create_schema(conn).unwrap();
        conn.execute_batch(
            "CREATE TABLE _source (id TEXT PRIMARY KEY, language TEXT, path TEXT, file_id INTEGER UNIQUE);
             CREATE TABLE _ast (
                 nid INTEGER PRIMARY KEY,
                 kind_id INTEGER NOT NULL,
                 start_byte INTEGER NOT NULL,
                 end_byte INTEGER NOT NULL,
                 start_row INTEGER NOT NULL,
                 start_col INTEGER NOT NULL,
                 end_row INTEGER NOT NULL,
                 end_col INTEGER NOT NULL
             );",
        )
        .unwrap();
        let file_id = leyline_schema::ensure_file_id(conn, "f").unwrap();
        leyline_schema::ensure_dir_nodes(conn, "f", 0).unwrap();
        let base = leyline_schema::file_nid(file_id, 0);
        let name_id = leyline_schema::intern_name(conn, "f").unwrap();
        let k_construct = leyline_schema::intern_kind(conn, "test", kind).unwrap();
        let k_ident = leyline_schema::intern_kind(conn, "test", "identifier").unwrap();
        conn.execute(
            "INSERT INTO _source VALUES ('f', '?', '/x/f', ?1)",
            [file_id],
        )
        .unwrap();
        insert_node(conn, base, Some(-1), Some(name_id), None, 1, 0, 0, 0, "").unwrap();
        insert_node(
            conn,
            base + 1,
            Some(base),
            None,
            Some(k_construct),
            1,
            0,
            0,
            0,
            "",
        )
        .unwrap();
        insert_node(
            conn,
            base + 2,
            Some(base + 1),
            None,
            Some(k_ident),
            0,
            0,
            0,
            0,
            "",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO _ast VALUES (?1, ?2, 0, 100, 0, 0, 3, 0), (?3, ?4, 30, 33, 1, 4, 1, 7)",
            params![base + 1, k_construct, base + 2, k_ident],
        )
        .unwrap();
        (base, base + 1)
    }

    /// ADR-0013 Step 1 (be6136) + ley-line-open-6b332d: when `_source` and `_ast`
    /// are populated and the LSP location's URI resolves to a known
    /// source path, the BindingRecord's `refSiteNodeId` is set to the
    /// smallest enclosing AST node (not empty). Pin the byte-range
    /// join via the capnp output (the post-ley-line-open-6b332d contract).
    #[test]
    fn project_references_populates_referrer_node_id() {
        use leyline_schema_capnp::binding_capnp::binding_record;
        use std::collections::HashMap;
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("test.bindings.capnp");
        let conn = Connection::open_in_memory().unwrap();

        leyline_schema::create_schema(&conn).unwrap();
        conn.execute_batch(
            "CREATE TABLE _source (id TEXT PRIMARY KEY, language TEXT, path TEXT, file_id INTEGER UNIQUE);
             CREATE TABLE _ast (
                 nid INTEGER PRIMARY KEY,
                 kind_id INTEGER NOT NULL,
                 start_byte INTEGER NOT NULL,
                 end_byte INTEGER NOT NULL,
                 start_row INTEGER NOT NULL,
                 start_col INTEGER NOT NULL,
                 end_row INTEGER NOT NULL,
                 end_col INTEGER NOT NULL
             );",
        )
        .unwrap();
        let file_id = leyline_schema::ensure_file_id(&conn, "main.go").unwrap();
        leyline_schema::ensure_dir_nodes(&conn, "main.go", 0).unwrap();
        let base = leyline_schema::file_nid(file_id, 0);
        let name_id = leyline_schema::intern_name(&conn, "main.go").unwrap();
        let k_fn = leyline_schema::intern_kind(&conn, "go", "function_declaration").unwrap();
        let k_block = leyline_schema::intern_kind(&conn, "go", "block").unwrap();
        conn.execute(
            "INSERT INTO _source VALUES ('main.go', 'go', '/canonical/main.go', ?1)",
            [file_id],
        )
        .unwrap();
        insert_node(&conn, base, Some(-1), Some(name_id), None, 1, 0, 0, 0, "").unwrap();
        insert_node(
            &conn,
            base + 1,
            Some(base),
            None,
            Some(k_fn),
            1,
            0,
            0,
            0,
            "",
        )
        .unwrap();
        insert_node(
            &conn,
            base + 2,
            Some(base + 1),
            None,
            Some(k_block),
            1,
            0,
            0,
            0,
            "",
        )
        .unwrap();
        // Enclosing function at lines 0..3, plus a tighter inner block
        // at lines 1..2. The smaller (end_byte - start_byte) wins —
        // pinning the ORDER BY in lookup_referrer_node_id.
        conn.execute(
            "INSERT INTO _ast VALUES \
             (?1, ?2, 0, 100, 0, 0, 3, 0), \
             (?3, ?4, 30, 60, 1, 0, 2, 0)",
            params![base + 1, k_fn, base + 2, k_block],
        )
        .unwrap();

        let mut locs = vec![make_location("file:///canonical/main.go", 1, 4)];
        locs[0].range.end.character = 7;

        let bytes_by_uri: HashMap<String, String> = HashMap::from([(
            "file:///canonical/main.go".to_string(),
            "fn foo() {\n    bar(baz);\n}\n".to_string(),
        )]);
        let mut lookup = |uri: &str| bytes_by_uri.get(uri).cloned();

        project_references(&conn, "fn_foo", &locs, &mut lookup, Some(&log_path)).unwrap();

        let bytes = std::fs::read(&log_path).unwrap();
        let mut slice: &[u8] = &bytes;
        let msg = capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
            .unwrap();
        let rec: binding_record::Reader = msg.get_root().unwrap();
        assert_eq!(
            rec.get_ref_site_node_id().unwrap().to_str().unwrap(),
            "main.go/function_declaration/block",
            "refSiteNodeId resolves to the smallest enclosing AST node's \
             rendered path",
        );
    }

    /// be6136 + ley-line-open-6b332d: lookup_referrer_node_id misses when
    /// `_source.path` disagrees with the file:// URI by even one byte
    /// (e.g. macOS `/tmp` vs `/private/tmp`). The capnp record's
    /// `refSiteNodeId` falls through to empty string when the lookup
    /// fails. Pin the *negative* path: a future change that
    /// re-introduces a normalization mismatch silently surfaces here
    /// as an empty refSiteNodeId.
    #[test]
    fn lookup_referrer_node_id_returns_none_on_path_mismatch() {
        use leyline_schema_capnp::binding_capnp::binding_record;
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("test.bindings.capnp");
        let conn = Connection::open_in_memory().unwrap();

        conn.execute_batch(
            "CREATE TABLE _source (id TEXT PRIMARY KEY, language TEXT, path TEXT);
             CREATE TABLE _ast (
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
        // Stored path is un-canonicalized.
        conn.execute(
            "INSERT INTO _source VALUES ('main.go', 'go', '/tmp/main.go')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO _ast VALUES \
             ('fn_outer', 'main.go', 'function_declaration', 0, 100, 0, 0, 3, 0)",
            [],
        )
        .unwrap();

        // URI is canonicalized — mismatches `/tmp/main.go` in `_source.path`.
        let mut locs = vec![make_location("file:///private/tmp/main.go", 1, 4)];
        locs[0].range.end.character = 7;

        let mut nop_lookup = |_uri: &str| -> Option<String> { None };
        project_references(&conn, "fn_foo", &locs, &mut nop_lookup, Some(&log_path)).unwrap();

        let bytes = std::fs::read(&log_path).unwrap();
        let mut slice: &[u8] = &bytes;
        let msg = capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
            .unwrap();
        let rec: binding_record::Reader = msg.get_root().unwrap();
        assert_eq!(
            rec.get_ref_site_node_id().unwrap().to_str().unwrap(),
            "",
            "path mismatch must produce empty refSiteNodeId (regression pin)",
        );
    }

    /// ADR-0013 Step 1 + ley-line-open-6b332d: when source_lookup returns None
    /// (file unavailable, cross-repo URI), the BindingRecord's
    /// `refToken` defaults to empty string — NEVER null in the schema.
    /// Pin so consumer queries filtering on non-empty stay correct.
    #[test]
    fn project_references_ref_token_defaults_empty_on_lookup_miss() {
        use leyline_schema_capnp::binding_capnp::binding_record;
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("test.bindings.capnp");
        let conn = Connection::open_in_memory().unwrap();
        let locs = vec![make_location("file:///unreachable.rs", 0, 0)];
        let mut nop_lookup = |_uri: &str| -> Option<String> { None };

        project_references(&conn, "n0", &locs, &mut nop_lookup, Some(&log_path)).unwrap();

        let bytes = std::fs::read(&log_path).unwrap();
        let mut slice: &[u8] = &bytes;
        let msg = capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
            .unwrap();
        let rec: binding_record::Reader = msg.get_root().unwrap();
        assert_eq!(
            rec.get_ref_token().unwrap().to_str().unwrap(),
            "",
            "refToken defaults to empty, not null",
        );
    }

    /// `create_lsp_schema` is idempotent and its base DDL carries every
    /// ADR-0013 column at birth. (The pre-v5 in-place `migrate_lsp_schema`
    /// ALTER pass is gone — projection-v5 refuses pre-v5 arenas at open
    /// rather than patching them.)
    #[test]
    fn create_lsp_schema_is_idempotent_and_complete() {
        let conn = Connection::open_in_memory().unwrap();
        create_lsp_schema(&conn).unwrap();
        // Second call must not error.
        create_lsp_schema(&conn).unwrap();
        // And the columns are present.
        for (table, col) in [
            ("_lsp_defs", "nid"),
            ("_lsp_defs", "def_token"),
            ("_lsp_refs", "nid"),
            ("_lsp_refs", "ref_token"),
            ("_lsp_refs", "referrer_nid"),
        ] {
            let present: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM pragma_table_info(?1) WHERE name = ?2",
                    [table, col],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(present, "{table}.{col} must exist in the base DDL");
        }
    }

    // The pre-ADR-0013 in-place migration test is gone with the migration
    // itself: projection-v5 refuses pre-v5 arenas at open, so no code path
    // ever ALTERs a legacy `_lsp_*` shape anymore.

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

        project_hover(&conn, 77, &hover).unwrap();

        let text: String = conn
            .query_row(
                "SELECT hover_text FROM _lsp_hover WHERE nid = 77",
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

        let count = project_completions(&conn, 105, &items).unwrap();
        assert_eq!(count, 2);

        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _lsp_completions WHERE nid = 105",
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
