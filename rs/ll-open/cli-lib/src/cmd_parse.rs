//! Parse command — walks a source directory, runs tree-sitter on each file,
//! and writes nodes + _ast + _source tables into a SQLite .db.
//!
//! Supports incremental mode: if the output .db already exists, unchanged files
//! (same mtime + size) are skipped, changed files are deleted and re-parsed,
//! and files removed from disk are purged from the database.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use leyline_ts::languages::TsLanguage;
use leyline_ts::refs::extract_go_refs;
use leyline_ts::schema::{
    create_ast_schema, create_index_schema, create_refs_schema, delete_file_rows, insert_ast,
    insert_node, insert_source_ref, read_file_index, set_meta, sweep_orphaned_dirs,
    upsert_file_index,
};
use rusqlite::Connection;
use tree_sitter::TreeCursor;

/// Orchestrate a multi-file parse of `source` into `output`.
///
/// When `output` already exists the parse is incremental: unchanged files are
/// skipped, changed files are deleted and re-parsed, and files that no longer
/// exist on disk are removed from the database.
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

    // Check BEFORE opening conn (sqlite creates the file on open).
    let incremental = output.exists();

    let conn =
        Connection::open(output).with_context(|| format!("open {}", output.display()))?;

    conn.pragma_update(None, "journal_mode", "WAL")?;
    create_ast_schema(&conn)?;
    create_refs_schema(&conn)?;
    create_index_schema(&conn)?;

    let mtime = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as i64;

    // Root directory node (INSERT OR IGNORE so incremental runs don't fail).
    conn.execute(
        "INSERT OR IGNORE INTO nodes (id, parent_id, name, kind, size, mtime, record) \
         VALUES ('', '', '', 1, 0, ?1, '')",
        [mtime],
    )?;

    // ---- Build current-file map (rel_path -> stat + abs path + language) ----

    let old_index = if incremental {
        read_file_index(&conn)?
    } else {
        HashMap::new()
    };

    let mut current_files: HashMap<String, (i64, i64, PathBuf, TsLanguage)> = HashMap::new();
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

        current_files.insert(rel_str, (file_mtime, file_size, path.clone(), lang));
    }

    // ---- Classify files: unchanged vs to_parse (new + changed) ----

    let mut to_parse: Vec<(String, PathBuf, TsLanguage)> = Vec::new();
    let mut unchanged = 0u64;

    for (rel, (file_mtime, file_size, abs_path, lang)) in &current_files {
        if let Some(&(old_m, old_s)) = old_index.get(rel)
            && *file_mtime == old_m
            && *file_size == old_s
        {
            unchanged += 1;
            continue;
        }
        to_parse.push((rel.clone(), abs_path.clone(), *lang));
    }

    // ---- Delete rows for files removed from disk ----

    let mut deleted = 0u64;
    for old_path in old_index.keys() {
        if !current_files.contains_key(old_path) {
            delete_file_rows(&conn, old_path)?;
            deleted += 1;
        }
    }

    // ---- Delete rows for changed files (will be re-parsed below) ----

    for (rel, _, _) in &to_parse {
        if old_index.contains_key(rel) {
            delete_file_rows(&conn, rel)?;
        }
    }

    // ---- Parse new + changed files ----

    let mut dirs_created: HashSet<String> = HashSet::new();
    let mut parsed = 0u64;
    let mut errors = 0u64;

    for (rel, abs_path, lang) in &to_parse {
        let content =
            std::fs::read(abs_path).with_context(|| format!("read {}", abs_path.display()))?;

        let rel_path = Path::new(rel);

        // Create intermediate directory nodes.
        ensure_dirs(&conn, rel_path, mtime, &mut dirs_created)?;

        let abs_str = abs_path.to_string_lossy();
        match project_file(&conn, &content, *lang, rel, mtime, &abs_str) {
            Ok(()) => {
                // Record stat in file index for future incremental runs.
                let (file_mtime, file_size, _, _) = current_files[rel];
                upsert_file_index(&conn, rel, file_mtime, file_size)?;
                parsed += 1;
            }
            Err(e) => {
                eprintln!("warn: {}: {e:#}", abs_path.display());
                errors += 1;
            }
        }
    }

    // ---- Post-sweep: remove orphaned directory nodes ----

    let swept = sweep_orphaned_dirs(&conn)?;
    if swept > 0 {
        eprintln!("{swept} orphaned dirs removed");
    }

    // ---- Write metadata ----

    let source_abs = source.canonicalize().unwrap_or_else(|_| source.to_path_buf());
    set_meta(&conn, "source_root", &source_abs.to_string_lossy())?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    set_meta(&conn, "parse_time", &now.to_string())?;

    eprintln!(
        "{parsed} parsed, {unchanged} unchanged, {deleted} deleted, {errors} errors -> {}",
        output.display()
    );

    Ok(())
}

/// Create directory nodes for each component of a relative file path.
/// e.g. "src/pkg/main.go" creates nodes for "src" and "src/pkg".
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

/// Parse a single file and write nodes + _ast + _source into the database,
/// with all node IDs scoped under the file's relative path.
fn project_file(
    conn: &Connection,
    content: &[u8],
    language: TsLanguage,
    source_id: &str,
    mtime: i64,
    abs_path: &str,
) -> Result<()> {
    // Store file path reference, not content BLOB. Consumers read from disk.
    insert_source_ref(conn, source_id, language.name(), abs_path)?;

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&language.ts_language())
        .context("failed to set tree-sitter language")?;

    let tree = parser
        .parse(content, None)
        .context("tree-sitter parse returned None")?;

    let root = tree.root_node();

    // The file itself is a directory node containing AST children.
    let parent_id = source_id
        .rsplit_once('/')
        .map(|(p, _)| p)
        .unwrap_or("");

    let file_name = source_id
        .rsplit_once('/')
        .map(|(_, n)| n)
        .unwrap_or(source_id);

    insert_node(conn, source_id, parent_id, file_name, 1, 0, mtime, "")?;
    insert_ast(
        conn,
        source_id,
        source_id,
        root.kind(),
        root.start_byte(),
        root.end_byte(),
        root.start_position().row,
        root.start_position().column,
        root.end_position().row,
        root.end_position().column,
    )?;

    let mut cursor = root.walk();
    walk_children(content, &mut cursor, source_id, mtime, conn, source_id, language)?;

    Ok(())
}

/// Recursively walk named AST children, mirroring leyline_ts::project logic
/// but with a file-scoped prefix for all node IDs.
fn walk_children(
    content: &[u8],
    cursor: &mut TreeCursor,
    parent_id: &str,
    mtime: i64,
    conn: &Connection,
    source_id: &str,
    language: TsLanguage,
) -> Result<()> {
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

        insert_ast(
            conn,
            &id,
            source_id,
            kind,
            child.start_byte(),
            child.end_byte(),
            child.start_position().row,
            child.start_position().column,
            child.end_position().row,
            child.end_position().column,
        )?;

        if language == TsLanguage::Go {
            extract_go_refs(child, content, &id, source_id, conn)?;
        }

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
            insert_node(conn, &id, parent_id, &name, 1, 0, mtime, "")?;
            let mut sub_cursor = child.walk();
            walk_children(content, &mut sub_cursor, &id, mtime, conn, source_id, language)?;
        } else {
            let text = child.utf8_text(content).unwrap_or("");
            insert_node(conn, &id, parent_id, &name, 0, text.len() as i64, mtime, text)?;
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
