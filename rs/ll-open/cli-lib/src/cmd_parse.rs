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
// Public entry point
// ---------------------------------------------------------------------------

/// Orchestrate a multi-file parse of `source` into `output`.
///
/// Files are parsed in parallel via rayon, then batch-inserted into SQLite
/// in a single transaction. Incremental mode skips unchanged files.
pub fn cmd_parse(source: &Path, output: &Path, lang_filter: Option<&str>) -> Result<()> {
    if !source.is_dir() {
        bail!("{} is not a directory", source.display());
    }

    let lang_filter = lang_filter
        .map(TsLanguage::from_name)
        .transpose()
        .context("invalid --lang")?;

    let mut files = Vec::new();
    collect_files(source, &mut files)?;

    let incremental = output.exists();

    let conn =
        Connection::open(output).with_context(|| format!("open {}", output.display()))?;
    // Perf pragmas for bulk insert.
    // DELETE journal (not WAL) — the .db is a portable snapshot. WAL requires
    // -shm/-wal sidecar files on the same filesystem, breaking portability.
    // synchronous=OFF — no fsync during batch (re-parse on crash is safe).
    // page_size=65536 — larger B-tree pages, fewer page splits.
    conn.pragma_update(None, "journal_mode", "DELETE")?;
    conn.pragma_update(None, "synchronous", "OFF")?;
    conn.pragma_update(None, "page_size", "65536")?;
    conn.pragma_update(None, "cache_size", "-64000")?; // 64MB cache
    create_ast_schema(&conn)?;
    create_refs_schema(&conn)?;
    create_index_schema(&conn)?;

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
        read_file_index(&conn)?
    } else {
        HashMap::new()
    };

    let mut to_parse: Vec<(String, PathBuf, TsLanguage, i64, i64)> = Vec::new();
    let mut unchanged = 0u64;

    for path in &files {
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => continue,
        };
        let lang = match TsLanguage::from_extension(ext) {
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

        to_parse.push((rel_str, path.clone(), lang, file_mtime, file_size));
    }

    // ---- Delete stale rows ----

    let mut deleted = 0u64;
    let current_rels: HashSet<&str> = to_parse.iter().map(|(r, _, _, _, _)| r.as_str()).collect();
    for old_path in old_index.keys() {
        if !current_rels.contains(old_path.as_str())
            && !files.iter().any(|f| {
                f.strip_prefix(source)
                    .unwrap_or(f)
                    .to_string_lossy()
                    == *old_path
            })
        {
            delete_file_rows(&conn, old_path)?;
            deleted += 1;
        }
    }
    for (rel, _, _, _, _) in &to_parse {
        if old_index.contains_key(rel) {
            delete_file_rows(&conn, rel)?;
        }
    }

    // ---- Parallel parse (CPU-bound tree-sitter on all cores) ----

    let parse_start = std::time::Instant::now();

    let parsed_files: Vec<Result<ParsedFile>> = to_parse
        .par_iter()
        .map(|(rel, abs_path, lang, file_mtime, file_size)| {
            let content = std::fs::read(abs_path)
                .with_context(|| format!("read {}", abs_path.display()))?;
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

    for result in parsed_files {
        match result {
            Ok(pf) => {
                let rel_path = Path::new(&pf.rel);
                ensure_dirs(&conn, rel_path, mtime, &mut dirs_created)?;

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

    let swept = sweep_orphaned_dirs(&conn)?;
    if swept > 0 {
        eprintln!("{swept} orphaned dirs removed");
    }

    // ---- Metadata ----

    let source_abs = source.canonicalize().unwrap_or_else(|_| source.to_path_buf());
    set_meta(&conn, "source_root", &source_abs.to_string_lossy())?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    set_meta(&conn, "parse_time", &now.to_string())?;

    eprintln!(
        "{parsed} parsed, {unchanged} unchanged, {deleted} deleted, {errors} errors \
         (parse {parse_elapsed:.1?}, insert {insert_elapsed:.1?}) -> {}",
        output.display()
    );

    Ok(())
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

/// Recursively collect files, skipping hidden/vendor/target directories.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && (name.starts_with('.')
                || name == "node_modules"
                || name == "vendor"
                || name == "target")
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
