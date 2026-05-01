//! Parse command — walks a source directory, runs tree-sitter on each file,
//! and writes nodes + _ast + _source tables into a SQLite .db.
//!
//! Performance:
//! - **Incremental**: unchanged files (same mtime+size) are skipped.
//! - **Parallel**: tree-sitter parsing runs on all cores via rayon.
//! - **Batched**: all inserts happen in a single SQLite transaction.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use leyline_ts::languages::TsLanguage;

/// Maximum file size that the parse pass will read into memory. Files
/// larger than this are skipped with a warning and counted as `errors`
/// in the summary. Bound chosen empirically: most source files are well
/// under 1 MiB; common "huge file" cases at registry-repo scale are
/// generated YAML/JSON dumps, vendored package-locks, and minified JS,
/// none of which carry semantic value worth parsing.
///
/// At 8 MiB × N parallel rayon workers, peak memory stays bounded even
/// in the worst case (one max-sized file per worker simultaneously).
/// Without this cap, a single 1 GiB file in the source tree would OOM
/// the daemon during full reparse on small machines.
pub const MAX_PARSE_FILE_SIZE: i64 = 8 * 1024 * 1024;
use leyline_ts::refs::{ExtractedRef, extract_refs};
use leyline_ts::schema::{
    create_ast_schema, create_index_schema, create_refs_schema, delete_file_rows,
    read_file_index, set_meta, sweep_orphaned_dirs,
};
use rayon::prelude::*;
use rusqlite::Connection;

// ---------------------------------------------------------------------------
// Data structures for parallel parse (no DB access)
// ---------------------------------------------------------------------------

struct ParsedNode {
    id: String,
    parent_id: String,
    name: String,
    kind: i32,
    size: i64,
    record: String,
}

struct AstEntry {
    node_id: String,
    source_id: String,
    node_kind: String,
    start_byte: usize,
    end_byte: usize,
    start_row: usize,
    start_col: usize,
    end_row: usize,
    end_col: usize,
}

struct ParsedFile {
    rel: String,
    abs_path: String,
    language: String,
    nodes: Vec<ParsedNode>,
    ast_entries: Vec<AstEntry>,
    refs: Vec<ExtractedRef>,
    file_mtime: i64,
    file_size: i64,
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Result of a parse operation, including stats and changed file list.
pub struct ParseResult {
    /// Number of files successfully parsed.
    pub parsed: u64,
    /// Number of files skipped (unchanged mtime+size).
    pub unchanged: u64,
    /// Number of stale files deleted.
    pub deleted: u64,
    /// Number of files that failed to parse.
    pub errors: u64,
    /// Relative paths of files that were actually parsed (not skipped).
    pub changed_files: Vec<String>,
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Orchestrate a multi-file parse of `source` into `output` (file-backed).
///
/// Opens a file-backed SQLite connection with portable pragmas, then
/// delegates to [`parse_into_conn`].
pub fn cmd_parse(source: &Path, output: &Path, lang_filter: Option<&str>) -> Result<()> {
    if !source.is_dir() {
        bail!("{} is not a directory", source.display());
    }

    let conn =
        Connection::open(output).with_context(|| format!("open {}", output.display()))?;
    // Perf pragmas for file-backed bulk insert.
    // DELETE journal (not WAL) — the .db is a portable snapshot. WAL requires
    // -shm/-wal sidecar files on the same filesystem, breaking portability.
    // synchronous=OFF — no fsync during batch (re-parse on crash is safe).
    // page_size=65536 — larger B-tree pages, fewer page splits.
    conn.pragma_update(None, "journal_mode", "DELETE")?;
    conn.pragma_update(None, "synchronous", "OFF")?;
    conn.pragma_update(None, "page_size", "65536")?;
    conn.pragma_update(None, "cache_size", "-64000")?; // 64MB cache

    let result = parse_into_conn(&conn, source, lang_filter, None)?;
    eprintln!(
        "{} parsed, {} unchanged, {} deleted, {} errors -> {}",
        result.parsed, result.unchanged, result.deleted, result.errors,
        output.display()
    );

    Ok(())
}

/// Parse `source` into an already-open connection.
///
/// The caller is responsible for opening the connection (file-backed or
/// `:memory:`) and setting appropriate pragmas. This function creates
/// the schema if needed, then runs incremental parallel parse.
///
/// `scope` restricts the parse to a subset of relative paths (e.g. the dirty
/// set from the git watcher). When `Some`, only files in the scope are stat'd
/// and reparsed, and only those paths are considered for deletion. When
/// `None`, the entire `source` tree is walked.
pub fn parse_into_conn(
    conn: &Connection,
    source: &Path,
    lang_filter: Option<&str>,
    scope: Option<&[String]>,
) -> Result<ParseResult> {
    if !source.is_dir() {
        bail!("{} is not a directory", source.display());
    }

    let lang_filter = lang_filter
        .map(TsLanguage::from_name)
        .transpose()
        .context("invalid --lang")?;

    let files = if let Some(scope) = scope {
        // Scoped pass — caller (typically git watcher) supplied the file set.
        // Pre-size to scope.len(): we may filter out vanished paths but
        // never grow beyond that bound.
        let mut v: Vec<PathBuf> = Vec::with_capacity(scope.len());
        for rel in scope {
            let abs = source.join(rel);
            if abs.exists() {
                v.push(abs);
            }
        }
        v
    } else {
        // Full-tree walk — collect_files doesn't know the file count up
        // front (no cheap way without a pre-pass), so the inner Vec
        // resizes during traversal. Acceptable trade-off: registry-scale
        // walks dominated by stat/readdir cost, not Vec resizing.
        let mut v = Vec::new();
        collect_files(source, &mut v)?;
        v
    };

    // Check if tables already exist (incremental mode).
    let incremental = conn
        .prepare("SELECT 1 FROM _file_index LIMIT 1")
        .is_ok();

    create_ast_schema(conn)?;
    create_refs_schema(conn)?;
    create_index_schema(conn)?;

    let mtime = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as i64;

    conn.execute(
        "INSERT OR IGNORE INTO nodes (id, parent_id, name, kind, size, mtime, record) \
         VALUES ('', '', '', 1, 0, ?1, '')",
        [mtime],
    )?;

    // ---- Classify files ----

    let old_index = if incremental {
        read_file_index(conn)?
    } else {
        HashMap::new()
    };

    // Pre-allocate worst-case (every file gets reparsed) to avoid Vec
    // resizes during the classification loop. At registry-repo scale
    // (50k+ files) the default doubling-resize pattern would do
    // ~16 reallocations from 4-element initial capacity to 50000.
    let mut to_parse: Vec<(String, PathBuf, TsLanguage, i64, i64)> =
        Vec::with_capacity(files.len());
    let mut unchanged = 0u64;
    let mut oversized = 0u64;

    for path in &files {
        // Try extension first, then filename for extensionless files (Dockerfile, etc).
        let lang = path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(TsLanguage::from_extension)
            .or_else(|| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .and_then(TsLanguage::from_filename)
            });
        let lang = match lang {
            Some(l) => l,
            None => continue,
        };
        if let Some(filter) = lang_filter
            && lang != filter
        {
            continue;
        }

        let rel = path.strip_prefix(source).unwrap_or(path);
        let rel_str = rel.to_string_lossy().to_string();

        let meta = std::fs::metadata(path)
            .with_context(|| format!("stat {}", path.display()))?;
        let file_mtime = meta
            .modified()
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as i64;
        let file_size = meta.len() as i64;

        if let Some(&(old_m, old_s)) = old_index.get(&rel_str)
            && file_mtime == old_m
            && file_size == old_s
        {
            unchanged += 1;
            continue;
        }

        // Scale guard: reject files above MAX_PARSE_FILE_SIZE. tree-sitter
        // parses the full source in memory; a 100MB+ file (generated YAML
        // dump, vendored package-lock, minified bundle) would either OOM
        // the worker or take many minutes producing nodes that have no
        // semantic value anyway.
        if file_size > MAX_PARSE_FILE_SIZE {
            log::warn!(
                "skip {rel_str}: size {file_size} bytes exceeds MAX_PARSE_FILE_SIZE \
                 ({MAX_PARSE_FILE_SIZE} bytes)",
            );
            oversized += 1;
            continue;
        }

        to_parse.push((rel_str, path.clone(), lang, file_mtime, file_size));
    }

    // ---- Delete stale rows ----

    let mut deleted = 0u64;
    let current_rels: HashSet<&str> = to_parse.iter().map(|(r, _, _, _, _)| r.as_str()).collect();

    // Build the rel-path set ONCE for the deletion sweep below. Without
    // this, the inner check did `files.iter().any(|f| strip_prefix +
    // to_string_lossy + cmp)` per old_path — at registry-repo scale
    // (50k old × 50k files) that's billions of string comparisons. The
    // HashSet of relative paths makes the lookup O(1) at the cost of
    // one rel-string per file (already paid by `current_rels`).
    let all_file_rels: HashSet<String> = files
        .iter()
        .map(|f| {
            f.strip_prefix(source)
                .unwrap_or(f)
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    // For a full-tree pass, every path in old_index that isn't in `files` is
    // a deletion candidate. For a scoped pass, only paths in scope can be
    // deleted — paths outside scope are simply not visible to this pass.
    let scope_set: Option<HashSet<&str>> =
        scope.map(|s| s.iter().map(|p| p.as_str()).collect());

    for old_path in old_index.keys() {
        if let Some(set) = &scope_set
            && !set.contains(old_path.as_str())
        {
            continue;
        }
        // Two ways an old_path can survive deletion:
        //   1. It's being reparsed this run (in current_rels), OR
        //   2. It exists on disk but was filtered out (in all_file_rels
        //      but not in current_rels — e.g. extension lost a tree-
        //      sitter mapping or --lang filter excluded it).
        if !current_rels.contains(old_path.as_str())
            && !all_file_rels.contains(old_path.as_str())
        {
            delete_file_rows(conn, old_path)?;
            deleted += 1;
        }
    }
    for (rel, _, _, _, _) in &to_parse {
        if old_index.contains_key(rel) {
            delete_file_rows(conn, rel)?;
        }
    }

    // ---- Parallel parse (CPU-bound tree-sitter on all cores) ----

    // DX: surface a progress line BEFORE the silent rayon parse.
    // At registry-repo scale (50k files) the parallel parse runs
    // ~30s, with no output until the final summary. A user invoking
    // `leyline parse ./helm-charts` would otherwise see silence and
    // wonder if it's hung. This line tells them the work is real
    // and bounded; the final summary still reports timing + counts.
    // Suppress at low scale where the silent path is fine.
    const PARSE_PROGRESS_THRESHOLD: usize = 200;
    if to_parse.len() >= PARSE_PROGRESS_THRESHOLD {
        eprintln!(
            "parsing {} files (skipped {unchanged} unchanged{}{})",
            to_parse.len(),
            if oversized > 0 { format!(", {oversized} oversized") } else { String::new() },
            if deleted > 0 { format!(", {deleted} deleted") } else { String::new() },
        );
    }

    let parse_start = std::time::Instant::now();

    let parsed_files: Vec<Result<ParsedFile>> = to_parse
        .par_iter()
        .map(|(rel, abs_path, lang, file_mtime, file_size)| {
            let content = std::fs::read(abs_path)
                .with_context(|| format!("read {}", abs_path.display()))?;

            // Skip binary files (null byte in first 8KB — same heuristic as git).
            let check_len = content.len().min(8192);
            if content[..check_len].contains(&0) {
                bail!("binary file (null byte in first 8KB): {}", abs_path.display());
            }

            let abs_str = abs_path.to_string_lossy().to_string();
            parse_file_pure(&content, *lang, rel, &abs_str, *file_mtime, *file_size)
        })
        .collect();

    let parse_elapsed = parse_start.elapsed();

    // ---- Batch insert (prepared statements + single transaction) ----

    let insert_start = std::time::Instant::now();
    let mut dirs_created: HashSet<String> = HashSet::new();
    let mut parsed = 0u64;
    let mut errors = 0u64;

    conn.execute_batch("BEGIN")?;

    // Prepare statements once — reuse for all rows (avoids SQL parse per INSERT).
    let mut stmt_node = conn.prepare_cached(
        "INSERT OR REPLACE INTO nodes (id, parent_id, name, kind, size, mtime, record) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    let mut stmt_ast = conn.prepare_cached(
        "INSERT OR REPLACE INTO _ast (node_id, source_id, node_kind, start_byte, end_byte, \
         start_row, start_col, end_row, end_col) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    let mut stmt_source = conn.prepare_cached(
        "INSERT OR REPLACE INTO _source (id, language, path) VALUES (?1, ?2, ?3)",
    )?;
    let mut stmt_ref = conn.prepare_cached(
        "INSERT INTO node_refs (token, node_id, source_id) VALUES (?1, ?2, ?3)",
    )?;
    let mut stmt_def = conn.prepare_cached(
        "INSERT INTO node_defs (token, node_id, source_id) VALUES (?1, ?2, ?3)",
    )?;
    let mut stmt_import = conn.prepare_cached(
        "INSERT INTO _imports (alias, path, source_id) VALUES (?1, ?2, ?3)",
    )?;
    let mut stmt_file_idx = conn.prepare_cached(
        "INSERT OR REPLACE INTO _file_index (path, mtime, size) VALUES (?1, ?2, ?3)",
    )?;

    let mut changed_files: Vec<String> = Vec::new();

    for result in parsed_files {
        match result {
            Ok(pf) => {
                let rel_path = Path::new(&pf.rel);
                ensure_dirs(conn, rel_path, mtime, &mut dirs_created)?;

                stmt_source.execute(rusqlite::params![&pf.rel, &pf.language, &pf.abs_path])?;

                for n in &pf.nodes {
                    stmt_node.execute(rusqlite::params![
                        &n.id, &n.parent_id, &n.name, n.kind, n.size, mtime, &n.record
                    ])?;
                }

                for a in &pf.ast_entries {
                    stmt_ast.execute(rusqlite::params![
                        &a.node_id, &a.source_id, &a.node_kind,
                        a.start_byte, a.end_byte, a.start_row, a.start_col,
                        a.end_row, a.end_col
                    ])?;
                }

                for r in &pf.refs {
                    match r {
                        ExtractedRef::Ref { token, node_id, source_id } => {
                            stmt_ref.execute(rusqlite::params![token, node_id, source_id])?;
                        }
                        ExtractedRef::Def { token, node_id, source_id } => {
                            stmt_def.execute(rusqlite::params![token, node_id, source_id])?;
                        }
                        ExtractedRef::Import { alias, path, source_id } => {
                            stmt_import.execute(rusqlite::params![alias, path, source_id])?;
                        }
                    }
                }

                stmt_file_idx.execute(rusqlite::params![&pf.rel, pf.file_mtime, pf.file_size])?;
                changed_files.push(pf.rel.clone());
                parsed += 1;
            }
            Err(e) => {
                eprintln!("warn: {e:#}");
                errors += 1;
            }
        }
    }

    // Drop prepared statements before COMMIT (releases borrow on conn).
    drop(stmt_node);
    drop(stmt_ast);
    drop(stmt_source);
    drop(stmt_ref);
    drop(stmt_def);
    drop(stmt_import);
    drop(stmt_file_idx);

    conn.execute_batch("COMMIT")?;

    let insert_elapsed = insert_start.elapsed();

    // ---- Post-sweep ----
    //
    // Skip orphaned-dir sweep on scoped passes: it would walk the full
    // _file_index tree and incorrectly drop dirs whose other (out-of-scope)
    // files weren't loaded into this run. Full-tree passes still run it.
    if scope.is_none() {
        let swept = sweep_orphaned_dirs(conn)?;
        if swept > 0 {
            eprintln!("{swept} orphaned dirs removed");
        }
    }

    // ---- Metadata ----

    let source_abs = source.canonicalize().unwrap_or_else(|_| source.to_path_buf());
    set_meta(conn, "source_root", &source_abs.to_string_lossy())?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    set_meta(conn, "parse_time", &now.to_string())?;

    if oversized > 0 {
        eprintln!(
            "{parsed} parsed, {unchanged} unchanged, {deleted} deleted, \
             {errors} errors, {oversized} skipped >{}MB \
             (parse {parse_elapsed:.1?}, insert {insert_elapsed:.1?})",
            MAX_PARSE_FILE_SIZE / (1024 * 1024),
        );
    } else {
        eprintln!(
            "{parsed} parsed, {unchanged} unchanged, {deleted} deleted, {errors} errors \
             (parse {parse_elapsed:.1?}, insert {insert_elapsed:.1?})",
        );
    }

    // Oversized files count as errors at the result level — they
    // weren't parsed, so the caller's "did this run produce data for
    // every file" check stays honest. The dedicated summary field makes
    // it easy for clients to distinguish skip-by-size from parse failure.
    Ok(ParseResult {
        parsed,
        unchanged,
        deleted,
        errors: errors + oversized,
        changed_files,
    })
}

// ---------------------------------------------------------------------------
// Pure file parser (no Connection — safe for rayon)
// ---------------------------------------------------------------------------

/// Parse a single file into a `ParsedFile`. No database access.
fn parse_file_pure(
    content: &[u8],
    language: TsLanguage,
    source_id: &str,
    abs_path: &str,
    file_mtime: i64,
    file_size: i64,
) -> Result<ParsedFile> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&language.ts_language())
        .context("failed to set tree-sitter language")?;

    let tree = parser
        .parse(content, None)
        .context("tree-sitter parse returned None")?;

    let root = tree.root_node();

    let parent_id = source_id
        .rsplit_once('/')
        .map(|(p, _)| p)
        .unwrap_or("")
        .to_string();

    let file_name = source_id
        .rsplit_once('/')
        .map(|(_, n)| n)
        .unwrap_or(source_id)
        .to_string();

    let mut nodes = Vec::new();
    let mut ast_entries = Vec::new();
    let mut refs = Vec::new();

    // File node.
    nodes.push(ParsedNode {
        id: source_id.to_string(),
        parent_id: parent_id.clone(),
        name: file_name,
        kind: 1,
        size: 0,
        record: String::new(),
    });

    // Root AST entry.
    ast_entries.push(AstEntry {
        node_id: source_id.to_string(),
        source_id: source_id.to_string(),
        node_kind: root.kind().to_string(),
        start_byte: root.start_byte(),
        end_byte: root.end_byte(),
        start_row: root.start_position().row,
        start_col: root.start_position().column,
        end_row: root.end_position().row,
        end_col: root.end_position().column,
    });

    // Walk AST.
    let mut cursor = root.walk();
    walk_children_pure(
        content, &mut cursor, source_id, source_id, language,
        &mut nodes, &mut ast_entries, &mut refs,
    );

    Ok(ParsedFile {
        rel: source_id.to_string(),
        abs_path: abs_path.to_string(),
        language: language.name().to_string(),
        nodes,
        ast_entries,
        refs,
        file_mtime,
        file_size,
    })
}

/// Recursively walk named AST children, collecting into vectors.
#[allow(clippy::too_many_arguments)]
fn walk_children_pure(
    content: &[u8],
    cursor: &mut tree_sitter::TreeCursor,
    parent_id: &str,
    source_id: &str,
    language: TsLanguage,
    nodes: &mut Vec<ParsedNode>,
    ast_entries: &mut Vec<AstEntry>,
    refs: &mut Vec<ExtractedRef>,
) {
    let node = cursor.node();

    let mut children: Vec<tree_sitter::Node> = Vec::new();
    let mut kind_counts = HashMap::<&str, usize>::new();

    let mut child_cursor = node.walk();
    if child_cursor.goto_first_child() {
        loop {
            let child = child_cursor.node();
            if child.is_named() {
                *kind_counts.entry(child.kind()).or_insert(0) += 1;
                children.push(child);
            }
            if !child_cursor.goto_next_sibling() {
                break;
            }
        }
    }

    let mut kind_indices = HashMap::<&str, usize>::new();

    for child in &children {
        let kind = child.kind();
        let needs_suffix = kind_counts[kind] > 1;

        let name = if needs_suffix {
            let idx = kind_indices.entry(kind).or_insert(0);
            let n = format!("{kind}_{idx}");
            *idx += 1;
            n
        } else {
            kind.to_string()
        };

        let id = format!("{parent_id}/{name}");

        ast_entries.push(AstEntry {
            node_id: id.clone(),
            source_id: source_id.to_string(),
            node_kind: kind.to_string(),
            start_byte: child.start_byte(),
            end_byte: child.end_byte(),
            start_row: child.start_position().row,
            start_col: child.start_position().column,
            end_row: child.end_position().row,
            end_col: child.end_position().column,
        });

        // Extract refs via the language-dispatched factory.
        let extracted = extract_refs(child, content, &id, source_id, language);
        refs.extend(extracted);

        let has_named_children = {
            let mut c = child.walk();
            let mut found = false;
            if c.goto_first_child() {
                loop {
                    if c.node().is_named() {
                        found = true;
                        break;
                    }
                    if !c.goto_next_sibling() {
                        break;
                    }
                }
            }
            found
        };

        if has_named_children {
            nodes.push(ParsedNode {
                id: id.clone(),
                parent_id: parent_id.to_string(),
                name: name.clone(),
                kind: 1,
                size: 0,
                record: String::new(),
            });
            let mut sub_cursor = child.walk();
            walk_children_pure(
                content, &mut sub_cursor, &id, source_id, language,
                nodes, ast_entries, refs,
            );
        } else {
            let text = child.utf8_text(content).unwrap_or("");
            nodes.push(ParsedNode {
                id: id.clone(),
                parent_id: parent_id.to_string(),
                name,
                kind: 0,
                size: text.len() as i64,
                record: text.to_string(),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create directory nodes for each component of a relative file path.
fn ensure_dirs(
    conn: &Connection,
    rel: &Path,
    mtime: i64,
    created: &mut HashSet<String>,
) -> Result<()> {
    let mut accumulated = String::new();
    let components: Vec<_> = rel
        .parent()
        .into_iter()
        .flat_map(|p| p.components())
        .collect();

    for comp in components {
        let name = comp.as_os_str().to_string_lossy();
        let parent = accumulated.clone();
        if accumulated.is_empty() {
            accumulated = name.to_string();
        } else {
            accumulated = format!("{accumulated}/{name}");
        }
        if created.insert(accumulated.clone()) {
            conn.execute(
                "INSERT OR IGNORE INTO nodes (id, parent_id, name, kind, size, mtime, record) \
                 VALUES (?1, ?2, ?3, 1, 0, ?4, '')",
                rusqlite::params![&accumulated, &parent, &*name, mtime],
            )?;
        }
    }
    Ok(())
}

/// True when the directory name should be excluded from the parse walk.
/// Decoupled from `collect_files` so tests can assert membership without
/// constructing a temp-dir per case, and so future entries can be added
/// in one place. The list is conservative — only directories that are
/// *definitively* generated/cached/vendored, never legitimate sources.
///
/// At registry-repo scale (50k+ files) a single un-skipped vendored copy
/// or pyc cache can 10× the walk's file count.
pub(crate) fn is_bloat_dir(name: &str) -> bool {
    // Hidden directories: .git, .venv, .tox, .pytest_cache, .next, .cache, ...
    if name.starts_with('.') {
        return true;
    }
    matches!(
        name,
        "node_modules"
        | "vendor"
        | "target"
        // Python bytecode cache. Always generated; never legitimate source.
        | "__pycache__"
        // PEP 582 local-deps (rare but real, contains third-party packages).
        | "__pypackages__"
        // Python virtualenv (when not dot-prefixed). Common: `python -m venv venv`.
        | "venv"
    )
}

/// Recursively collect files, skipping bloat directories per
/// `is_bloat_dir`. See `tests::collect_files_skips_known_bloat_dirs`
/// for the skip-list pin.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && is_bloat_dir(name)
        {
            continue;
        }

        if path.is_dir() {
            collect_files(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn collect_files_skips_known_bloat_dirs() {
        // Scale-problem pin. The skip-list keeps registry-repo walks
        // bounded — a 50k-file Aports clone with a vendored copy of
        // any large dependency, or a Python repo with __pycache__
        // hierarchies under every package, would 10× the walk if any
        // entry slipped out of the skip-list. Pin every entry by
        // constructing a minimal repo with each bloat dir + a sibling
        // source file and asserting only the source file is collected.
        let td = TempDir::new().unwrap();
        let root = td.path();

        // The one file we expect to find.
        std::fs::write(root.join("source.go"), b"package m").unwrap();

        // Create one bloat dir per skip-list entry. The set must stay
        // in sync with `is_bloat_dir`; a refactor that drops one of
        // these names from the matcher fails this test loudly.
        let bloat_names = [
            ".git",
            ".cache",
            ".venv",        // dot-prefix
            ".pytest_cache",// dot-prefix
            "node_modules",
            "vendor",
            "target",
            "__pycache__",
            "__pypackages__",
            "venv",
        ];
        for bloat in bloat_names {
            let dir = root.join(bloat);
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("inner.go"), b"package x").unwrap();
        }

        let mut found = Vec::new();
        collect_files(root, &mut found).unwrap();
        assert_eq!(
            found.len(),
            1,
            "only source.go should be collected, got {found:?}",
        );
        assert_eq!(
            found[0].file_name().and_then(|s| s.to_str()),
            Some("source.go"),
        );
    }

    #[test]
    fn is_bloat_dir_does_not_falsely_match_normal_names() {
        // Sister pin: legitimate source-bearing directory names must
        // not be caught by the bloat matcher. Pinning these explicitly
        // means a future "skip all uppercase dirs" or similar over-
        // aggressive rewrite would break here. Includes names that
        // *contain* bloat substrings (e.g. "node_modules_helper",
        // "venvironment") to catch a refactor that switched from
        // exact-match to substring-match.
        for name in [
            "src",
            "lib",
            "pkg",
            "internal",
            "cmd",
            "tests",
            "vendored_data", // contains "vendor"
            "subtarget",     // contains "target"
            "venvironment",  // contains "venv"
            "node_modules_helper", // contains "node_modules"
            "__init__.py",   // begins with __ but is not __pycache__/__pypackages__
            "_internal",     // begins with _ but not __
            "build",         // intentionally NOT in skip-list (often source)
            "dist",          // intentionally NOT in skip-list (often source)
        ] {
            assert!(
                !is_bloat_dir(name),
                "is_bloat_dir(`{name}`) must be false, but matched",
            );
        }
    }

    #[test]
    fn parse_into_conn_skips_oversized_files() {
        // Scale-guard pin. parse_into_conn must skip files larger than
        // MAX_PARSE_FILE_SIZE rather than reading them into memory. A
        // 100MB+ generated YAML in a registry repo would otherwise OOM
        // a worker or take many minutes producing nodes with no semantic
        // value. The skip is reflected in the returned `errors` count
        // (so callers' "did every file land?" check stays honest) and
        // logged via log::warn with the path.
        //
        // Construct a 9 MiB file (1 byte over the cap) alongside a
        // small one. The small file must parse, the big one must skip,
        // and the result MUST count exactly one error from the skip.
        let td = TempDir::new().unwrap();
        let root = td.path();

        // Small file — must parse.
        std::fs::write(root.join("small.go"), b"package m\n").unwrap();

        // Huge file — `MAX_PARSE_FILE_SIZE + 1` bytes of valid Go.
        // Padding with newlines keeps it valid Go (just a `package m\n`
        // followed by a million empty lines).
        let mut huge = Vec::with_capacity(MAX_PARSE_FILE_SIZE as usize + 1);
        huge.extend_from_slice(b"package m\n");
        huge.resize(MAX_PARSE_FILE_SIZE as usize + 1, b'\n');
        std::fs::write(root.join("huge.go"), &huge).unwrap();

        let conn = Connection::open_in_memory().unwrap();
        let result = parse_into_conn(&conn, root, None, None).unwrap();

        assert_eq!(result.parsed, 1, "small.go must parse cleanly");
        assert_eq!(
            result.errors, 1,
            "huge.go must contribute exactly 1 error (skip-by-size)",
        );

        // Sanity: the small file's nodes are present, huge.go's are absent.
        let small_present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _file_index WHERE path = 'small.go'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(small_present, 1);
        let huge_present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _file_index WHERE path = 'huge.go'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(huge_present, 0, "huge.go must NOT have been indexed");
    }

    #[test]
    fn collect_files_descends_into_normal_dirs() {
        // Sister pin: normal directories ARE descended. Pin so a
        // refactor over-aggressively pruning (e.g. skip every dir
        // starting with a letter) wouldn't silently miss source.
        let td = TempDir::new().unwrap();
        let root = td.path();
        let pkg = root.join("pkg");
        std::fs::create_dir(&pkg).unwrap();
        let nested = pkg.join("util");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("helper.go"), b"package u").unwrap();

        let mut found = Vec::new();
        collect_files(root, &mut found).unwrap();
        assert_eq!(found.len(), 1);
        assert!(found[0].ends_with("pkg/util/helper.go"));
    }
}
