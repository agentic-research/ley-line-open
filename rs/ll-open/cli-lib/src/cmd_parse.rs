//! Parse command — walks a source directory, runs tree-sitter on each file,
//! and writes nodes + _ast + _source tables into a SQLite .db.
//!
//! Performance:
//! - **Incremental**: unchanged files (same mtime+size) are skipped.
//! - **Parallel**: tree-sitter parsing runs on all cores via rayon.
//! - **Batched**: all inserts happen in a single SQLite transaction.

use std::collections::{HashMap, HashSet};
use std::io::{BufWriter, Write};
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
    create_ast_tables, create_index_schema, create_post_load_indexes_skip_unused,
    create_refs_tables, delete_file_rows, read_file_index, set_meta, sweep_orphaned_dirs,
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
    /// Pre-serialized capnp bytes for the per-file `SourceFile` record.
    /// Built in the rayon worker so the post-parse main thread just
    /// writes the bytes to the BufWriter — no per-file canonicalize
    /// step. See bead `ley-line-open-cbbedf` Attack 1 (parallelization).
    source_capnp_bytes: Vec<u8>,
    /// Pre-serialized capnp bytes for the file's AstNode records. Same
    /// rationale as `source_capnp_bytes` — moves the ~310ms (per the
    /// mache bench) capnp serialization cost out of the serial insert
    /// phase and into the parallel parse phase.
    ast_capnp_bytes: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Batched-insert plumbing
// ---------------------------------------------------------------------------

/// Rows per multi-row VALUES batch. 2000 × 9 columns (`_ast`, the
/// widest table) = 18 000 bound parameters per statement — well under
/// SQLite's 32K bound-param cap (`SQLITE_MAX_VARIABLE_NUMBER` default
/// 32 766 since 3.32). Larger batches collapse more transaction edges
/// per execute; on the mache 765-file bench going from 500 → 2000
/// rows/batch shaves another ~10-15% off insert wall.
///
/// Capped at 2000 so the per-batch SQL string stays under ~50 KiB —
/// the prepared-statement cache holds one entry per unique SQL string,
/// and bloated strings hurt the cache hit rate for the trailing
/// partial batch (a different SQL string per partial size).
const BULK_BATCH_ROWS: usize = 3000;

/// Build a multi-row VALUES placeholder string: `(?,?,?,...),(?,?,...),...`.
/// `rows` total tuples, each with `cols` placeholders.
fn build_values_clause(rows: usize, cols: usize) -> String {
    let row_tuple = {
        let mut s = String::with_capacity(2 * cols + 2);
        s.push('(');
        for i in 0..cols {
            if i > 0 {
                s.push(',');
            }
            s.push('?');
        }
        s.push(')');
        s
    };
    let mut out = String::with_capacity(row_tuple.len() * rows + rows);
    for i in 0..rows {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&row_tuple);
    }
    out
}

/// Execute a multi-row INSERT against `conn`. `prefix` is the SQL up
/// to and including `VALUES `; this helper appends the placeholder
/// tuples and binds. `params` is borrowed `&dyn ToSql` references that
/// must outlive the statement step.
fn exec_batched(
    conn: &Connection,
    prefix: &str,
    rows: usize,
    cols: usize,
    params: &[&dyn rusqlite::ToSql],
) -> Result<()> {
    if rows == 0 {
        return Ok(());
    }
    let mut sql = String::with_capacity(prefix.len() + rows * (cols * 2 + 3));
    sql.push_str(prefix);
    sql.push_str(&build_values_clause(rows, cols));
    let mut stmt = conn.prepare_cached(&sql)?;
    stmt.execute(rusqlite::params_from_iter(params.iter().copied()))?;
    Ok(())
}

/// Generic flush: drain `rows`-shaped data in BULK_BATCH_ROWS chunks
/// plus a final partial-chunk. `prefix` is the SQL prefix ending in
/// `VALUES `, `cols` is the per-row placeholder count, and `into_params`
/// flattens a slice of rows into a `Vec<&dyn ToSql>` borrowing from
/// the chunk (no per-row allocation). Borrow rules: the returned
/// references live as long as the chunk slice, which is at least the
/// statement-step scope.
fn flush_in_batches<R, F>(
    conn: &Connection,
    rows: Vec<R>,
    prefix: &str,
    cols: usize,
    mut into_params: F,
) -> Result<()>
where
    F: for<'a> FnMut(&'a [R]) -> Vec<&'a dyn rusqlite::ToSql>,
{
    let total = rows.len();
    if total == 0 {
        return Ok(());
    }
    let mut i = 0;
    while i + BULK_BATCH_ROWS <= total {
        let chunk = &rows[i..i + BULK_BATCH_ROWS];
        let params = into_params(chunk);
        exec_batched(conn, prefix, BULK_BATCH_ROWS, cols, &params)?;
        i += BULK_BATCH_ROWS;
    }
    if i < total {
        let chunk = &rows[i..];
        let n = chunk.len();
        let params = into_params(chunk);
        exec_batched(conn, prefix, n, cols, &params)?;
    }
    Ok(())
}

/// Macro to declare a per-table batch buffer + its flush_batched impl.
/// Centralizes the "Vec of owned rows + push() + flush_batched()"
/// boilerplate so each table only spells out its column list and the
/// per-row flatten closure.
///
/// The `Value` wire-type union eats the heterogeneity (TEXT/INTEGER mix)
/// without forcing per-table trait objects.
macro_rules! batch_table {
    (
        $name:ident, $row:ident, $prefix:expr, $cols:expr,
        push_fn: ($($push_arg:ident: $push_ty:ty),*),
        push_body: $push_body:block,
        flatten: |$chunk:ident| $flatten_body:block,
    ) => {
        struct $name {
            rows: Vec<$row>,
        }
        struct $row {
            $($push_arg: $push_ty),*
        }
        impl $name {
            fn with_capacity(cap: usize) -> Self {
                Self { rows: Vec::with_capacity(cap) }
            }
            #[allow(clippy::too_many_arguments)]
            fn push(&mut self, $($push_arg: $push_ty),*) {
                let row = $push_body;
                self.rows.push(row);
            }
            // RefBatch overrides this with `flush_batched_for` to thread
            // the per-table SQL prefix at flush time; the macro-generated
            // version is unused for that one type. `dead_code` is the
            // right knob here — the alternative (an extra macro arm or a
            // separate type per table) trades real complexity for one
            // warning we already understand.
            #[allow(dead_code)]
            fn flush_batched(self, conn: &Connection) -> Result<()> {
                flush_in_batches(conn, self.rows, $prefix, $cols, |$chunk| $flatten_body)
            }
        }
    };
}

batch_table! {
    // `INSERT OR IGNORE`: file-level nodes (kind=0) are deleted per file
    // via `delete_file_rows` before reparse, so they don't conflict.
    // But dir nodes (kind=1) inserted by `collect_dirs` may exist from
    // a prior parse — on incremental reparse, dirs survive across runs.
    // `OR IGNORE` skips the dup-PK row in that case, matching the
    // pre-9ccbc7 `INSERT OR IGNORE INTO nodes ... VALUES (?,?,?,1,...)`
    // behavior `ensure_dirs` used. File/AST node rows still write
    // their new values (no PK collision because their rows were just
    // deleted). On cold parse there are no conflicts; `OR IGNORE`
    // costs the same as plain `INSERT` (single B-tree insert).
    NodeBatch, NodeRow,
    "INSERT OR IGNORE INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ",
    7,
    push_fn: (id: String, parent_id: String, name: String, kind: i32, size: i64, mtime: i64, record: String),
    push_body: { NodeRow { id, parent_id, name, kind, size, mtime, record } },
    flatten: |chunk| {
        let mut out: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 7);
        for r in chunk {
            out.push(&r.id);
            out.push(&r.parent_id);
            out.push(&r.name);
            out.push(&r.kind);
            out.push(&r.size);
            out.push(&r.mtime);
            out.push(&r.record);
        }
        out
    },
}

batch_table! {
    // Plain `INSERT` (not `OR REPLACE`): `delete_file_rows` runs before
    // the parse loop and clears _ast rows for every file we're about
    // to reparse, so there's no PK conflict to handle. The `OR REPLACE`
    // path pays a per-row PK lookup even when no conflict exists; on
    // the mache 765-file bench that's ~535K extra B-tree probes. See
    // bead `ley-line-open-cbbedf`.
    AstBatch, AstRow,
    "INSERT INTO _ast (node_id, source_id, node_kind, start_byte, end_byte, start_row, start_col, end_row, end_col) VALUES ",
    9,
    push_fn: (node_id: String, source_id: String, node_kind: String, start_byte: i64, end_byte: i64, start_row: i64, start_col: i64, end_row: i64, end_col: i64),
    push_body: { AstRow { node_id, source_id, node_kind, start_byte, end_byte, start_row, start_col, end_row, end_col } },
    flatten: |chunk| {
        let mut out: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 9);
        for r in chunk {
            out.push(&r.node_id);
            out.push(&r.source_id);
            out.push(&r.node_kind);
            out.push(&r.start_byte);
            out.push(&r.end_byte);
            out.push(&r.start_row);
            out.push(&r.start_col);
            out.push(&r.end_row);
            out.push(&r.end_col);
        }
        out
    },
}

batch_table! {
    // Plain INSERT: same rationale as AstBatch — delete_file_rows
    // clears _source rows per file before reparse.
    SourceBatch, SourceRow,
    "INSERT INTO _source (id, language, path) VALUES ",
    3,
    push_fn: (id: String, language: String, path: String),
    push_body: { SourceRow { id, language, path } },
    flatten: |chunk| {
        let mut out: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 3);
        for r in chunk {
            out.push(&r.id);
            out.push(&r.language);
            out.push(&r.path);
        }
        out
    },
}

batch_table! {
    RefBatch, RefRow,
    // NOTE: `prefix` is rebound at flush time below since refs/defs/imports
    // share row shape but target different tables. See post-macro impl.
    "",
    3,
    push_fn: (col0: String, col1: String, col2: String),
    push_body: { RefRow { col0, col1, col2 } },
    flatten: |chunk| {
        let mut out: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 3);
        for r in chunk {
            out.push(&r.col0);
            out.push(&r.col1);
            out.push(&r.col2);
        }
        out
    },
}

// Override RefBatch::flush_batched to thread the per-table SQL prefix.
// node_refs / node_defs / _imports share the (TEXT, TEXT, TEXT) shape
// but live in different tables; rebinding `prefix` per call keeps the
// macro-generated buffer reusable across all three.
impl RefBatch {
    fn flush_batched_for(self, conn: &Connection, prefix: &str) -> Result<()> {
        flush_in_batches(conn, self.rows, prefix, 3, |chunk| {
            let mut out: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 3);
            for r in chunk {
                out.push(&r.col0);
                out.push(&r.col1);
                out.push(&r.col2);
            }
            out
        })
    }
}

batch_table! {
    // Plain INSERT: same rationale as AstBatch — delete_file_rows
    // clears _file_index rows per file before reparse.
    FileIdxBatch, FileIdxRow,
    "INSERT INTO _file_index (path, mtime, size) VALUES ",
    3,
    push_fn: (path: String, mtime: i64, size: i64),
    push_body: { FileIdxRow { path, mtime, size } },
    flatten: |chunk| {
        let mut out: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 3);
        for r in chunk {
            out.push(&r.path);
            out.push(&r.mtime);
            out.push(&r.size);
        }
        out
    },
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

    let conn = Connection::open(output).with_context(|| format!("open {}", output.display()))?;
    // Perf pragmas for file-backed bulk insert.
    // DELETE journal (not WAL) — the .db is a portable snapshot. WAL requires
    // -shm/-wal sidecar files on the same filesystem, breaking portability.
    // synchronous=OFF — no fsync during batch (re-parse on crash is safe).
    // page_size=65536 — larger B-tree pages, fewer page splits.
    // cache_size=-262144 (256 MB) — fits the working set of `_ast` (~120 MB)
    //   + `nodes` (~80 MB) entirely in memory for the mache benchmark. The
    //   prior 64 MB cap forced LRU eviction during the bulk-insert pass,
    //   causing repeated re-reads of B-tree interior pages. At registry-
    //   repo scale the cache caps gracefully via SQLite's LRU eviction.
    // temp_store=MEMORY — rollback journal stays in RAM (we're not crash-
    //   safe with synchronous=OFF anyway; a crash mid-parse discards the
    //   half-built db and the user reparses cold).
    // mmap_size=256 MB — memory-map the db file so SQLite reads (e.g. PK
    //   lookups during INSERT) go through the kernel page cache directly
    //   instead of pread/copy-to-buffer per page.
    conn.pragma_update(None, "journal_mode", "DELETE")?;
    conn.pragma_update(None, "synchronous", "OFF")?;
    conn.pragma_update(None, "page_size", "65536")?;
    conn.pragma_update(None, "cache_size", "-262144")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "mmap_size", 268_435_456_i64)?;

    let result = parse_into_conn(&conn, source, lang_filter, None)?;
    eprintln!(
        "{} parsed, {} unchanged, {} deleted, {} errors -> {}",
        result.parsed,
        result.unchanged,
        result.deleted,
        result.errors,
        output.display()
    );

    // Skip the SQLite connection's Drop on the way out — on macOS the
    // close call burns ~65 ms (cache teardown + page-table release),
    // which is pure user-visible wall time after the real work is
    // done. With `synchronous=OFF` and `journal_mode=DELETE` there's
    // no pending fsync owed and no journal to clean up (the journal
    // was deleted at COMMIT). The kernel will close the FD when the
    // process exits.
    //
    // We don't `process::exit` here because cmd_parse is also called
    // from integration tests; the test runner would exit early. The
    // caller (main) can choose to exit after this returns.
    //
    // See bead `ley-line-open-cbbedf`.
    std::mem::forget(conn);

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
    let incremental = conn.prepare("SELECT 1 FROM _file_index LIMIT 1").is_ok();

    // Tables only (no secondary indexes). At registry-repo scale the
    // bulk INSERT loop pays O(rows × indexes × log N) on B-tree
    // maintenance — the mache benchmark (764 files, 534k _ast rows)
    // attributes ~3s of the 4.1s insert phase to per-row index
    // updates. Indexes get rebuilt in one shot after `COMMIT` via
    // `create_post_load_indexes`. See bead `ley-line-open-9ccbc7`.
    create_ast_tables(conn)?;
    create_refs_tables(conn)?;
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

        let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
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
    let scope_set: Option<HashSet<&str>> = scope.map(|s| s.iter().map(|p| p.as_str()).collect());

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
        if !current_rels.contains(old_path.as_str()) && !all_file_rels.contains(old_path.as_str()) {
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
            if oversized > 0 {
                format!(", {oversized} oversized")
            } else {
                String::new()
            },
            if deleted > 0 {
                format!(", {deleted} deleted")
            } else {
                String::new()
            },
        );
    }

    // Sort to_parse by relative path so the post-parse iteration
    // generates SQL inserts in alphabetical key order. The `_ast` and
    // `nodes` tables use path-derived TEXT primary keys; inserts in
    // sorted order land in the tail of each B-tree leaf page rather
    // than splitting random interior pages, which is a 1.3-1.5×
    // speedup on bulk-load of TEXT PK tables (per SQLite optimizer
    // notes on "sorted INSERT amortization"). On the mache 765-file
    // bench this saves ~150-200ms across the nodes + _ast flushes.
    to_parse.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    let parse_start = std::time::Instant::now();

    let parsed_files: Vec<Result<ParsedFile>> = to_parse
        .par_iter()
        .map(|(rel, abs_path, lang, file_mtime, file_size)| {
            let content =
                std::fs::read(abs_path).with_context(|| format!("read {}", abs_path.display()))?;

            // Skip binary files (null byte in first 8KB — same heuristic as git).
            let check_len = content.len().min(8192);
            if content[..check_len].contains(&0) {
                bail!(
                    "binary file (null byte in first 8KB): {}",
                    abs_path.display()
                );
            }

            // Canonicalize so `_source.path` matches the LSP-derived
            // file:// URI (lsp_pass.rs canonicalizes before constructing
            // the URI). Without this, on macOS `/tmp` vs `/private/tmp`
            // and elsewhere any symlink-rooted path produces a path
            // mismatch in `lookup_referrer_node_id` — every lookup
            // misses, every `_lsp_refs.referrer_node_id` is NULL
            // (be6136). Fall back to the original path if canonicalize
            // fails (e.g. broken symlink), preserving prior behavior.
            let canon = abs_path.canonicalize().unwrap_or_else(|_| abs_path.clone());
            let abs_str = canon.to_string_lossy().to_string();
            parse_file_pure(&content, *lang, rel, &abs_str, *file_mtime, *file_size)
        })
        .collect();

    let parse_elapsed = parse_start.elapsed();

    // ---- Batch insert (multi-row VALUES + single transaction) ----
    //
    // The bulk-insert hot path: 534K nodes, 535K _ast rows on the mache
    // benchmark. Single-row INSERTs via prepare_cached pay a transaction-
    // edge cost per row (statement step, B-tree page split, locking
    // arbitration). Multi-row `VALUES(...),(...)` batches collapse that
    // by ~10×: SQLite parses one statement, executes it once, and the
    // B-tree maintenance amortizes across rows.
    //
    // Batch size: BULK_BATCH_ROWS (== 500) rows per execute, well under
    // SQLite's 32K bound-param cap even for the 9-column _ast table
    // (4500 params). The "full batch" SQL string is the same every
    // execute → prepare_cached hits the per-table key once and reuses.
    // The trailing partial batch (< BULK_BATCH_ROWS rows) is flushed
    // with a separately-sized statement; on a 765-file corpus this
    // happens once per table at end of insert.
    //
    // See bead `ley-line-open-cbbedf`.

    let insert_start = std::time::Instant::now();
    let mut parsed = 0u64;
    let mut errors = 0u64;

    conn.execute_batch("BEGIN")?;

    // capnp dual-write (bead `ley-line-open-cdf098`) — open snapshot
    // files alongside the SQL writes. Truncate-and-rewrite semantics:
    // each parse run produces a fresh snapshot of `_ast` and `_source`.
    // `:memory:` connections skip (no path to write next to). The
    // segment-hashing → Σ root advance is bead `ley-line-open-ce55b1`.
    let (mut ast_writer, mut source_writer) = sibling_snapshot_writers(conn);

    // Per-table row buffers. Owned strings/values so we can hand
    // ToSql references to params_from_iter without lifetime gymnastics
    // through the parsed_files Vec.
    //
    // Pre-allocate at the per-file estimate × file_count: ~700 nodes
    // and ~700 ast entries per file on the mache benchmark, so 500K
    // capacity each is the right ballpark to avoid mid-loop reallocs.
    let mut nodes_buf: NodeBatch = NodeBatch::with_capacity(550_000);
    let mut ast_buf: AstBatch = AstBatch::with_capacity(550_000);
    let mut refs_buf: RefBatch = RefBatch::with_capacity(40_000);
    let mut defs_buf: RefBatch = RefBatch::with_capacity(3_000);
    let mut imports_buf: RefBatch = RefBatch::with_capacity(2_000);
    let mut source_buf: SourceBatch = SourceBatch::with_capacity(to_parse.len());
    let mut file_idx_buf: FileIdxBatch = FileIdxBatch::with_capacity(to_parse.len());

    let mut dirs_created: HashSet<String> = HashSet::new();
    let mut changed_files: Vec<String> = Vec::new();

    for result in parsed_files {
        match result {
            Ok(pf) => {
                let rel_path = Path::new(&pf.rel);
                collect_dirs(rel_path, &mut dirs_created, &mut nodes_buf, mtime);

                source_buf.push(pf.rel.clone(), pf.language.clone(), pf.abs_path.clone());

                // capnp dual-write (`ley-line-open-cdf098`): same fields
                // as the SQL row, typed and content-addressable. The
                // per-message capnp serialization happened in the rayon
                // worker (`parse_file_pure`); here we just append the
                // pre-built byte buffer to the BufWriter. See bead
                // `ley-line-open-cbbedf`.
                if let Some(w) = source_writer.as_mut() {
                    w.write_all(&pf.source_capnp_bytes)
                        .context("write SourceFile capnp bytes")?;
                }
                if let Some(w) = ast_writer.as_mut() {
                    w.write_all(&pf.ast_capnp_bytes)
                        .context("write AstNode capnp bytes")?;
                }

                for n in pf.nodes {
                    nodes_buf.push(n.id, n.parent_id, n.name, n.kind, n.size, mtime, n.record);
                }

                for a in pf.ast_entries {
                    ast_buf.push(
                        a.node_id,
                        a.source_id,
                        a.node_kind,
                        a.start_byte as i64,
                        a.end_byte as i64,
                        a.start_row as i64,
                        a.start_col as i64,
                        a.end_row as i64,
                        a.end_col as i64,
                    );
                }

                for r in pf.refs {
                    match r {
                        ExtractedRef::Ref {
                            token,
                            node_id,
                            source_id,
                        } => refs_buf.push(token, node_id, source_id),
                        ExtractedRef::Def {
                            token,
                            node_id,
                            source_id,
                        } => defs_buf.push(token, node_id, source_id),
                        ExtractedRef::Import {
                            alias,
                            path,
                            source_id,
                        } => imports_buf.push(alias, path, source_id),
                    }
                }

                file_idx_buf.push(pf.rel.clone(), pf.file_mtime, pf.file_size);
                changed_files.push(pf.rel);
                parsed += 1;
            }
            Err(e) => {
                eprintln!("warn: {e:#}");
                errors += 1;
            }
        }
    }

    // Flush each table in BULK_BATCH_ROWS-sized chunks via multi-row
    // VALUES inserts. Tail (last <BULK_BATCH_ROWS rows) flushed in one
    // partial-size statement so we don't fall back to per-row execute.
    nodes_buf.flush_batched(conn)?;
    ast_buf.flush_batched(conn)?;
    source_buf.flush_batched(conn)?;
    refs_buf.flush_batched_for(
        conn,
        "INSERT INTO node_refs (token, node_id, source_id) VALUES ",
    )?;
    defs_buf.flush_batched_for(
        conn,
        "INSERT INTO node_defs (token, node_id, source_id) VALUES ",
    )?;
    imports_buf.flush_batched_for(
        conn,
        "INSERT INTO _imports (alias, path, source_id) VALUES ",
    )?;
    file_idx_buf.flush_batched(conn)?;

    // Flush the capnp dual-write `BufWriter`s before COMMIT and before
    // `write_head_after_parse` reads the segments for hashing —
    // otherwise the buffered tail would be invisible to the Σ root
    // computation, yielding a hash that disagrees with the on-disk
    // bytes once the writer is dropped. Drop after flush so the file
    // handle is closed by the time the head pass runs.
    if let Some(mut w) = ast_writer.take() {
        w.flush().context("flush ast.capnp BufWriter")?;
    }
    if let Some(mut w) = source_writer.take() {
        w.flush().context("flush source.capnp BufWriter")?;
    }

    conn.execute_batch("COMMIT")?;

    // Pre-grab the db_path so we can dispatch the head-write hash pass
    // (pure filesystem work, reads ast.capnp + source.capnp) on a worker
    // thread that runs concurrently with `create_post_load_indexes`
    // (CPU + SQLite-disk work on the .db file). The two workloads touch
    // disjoint files and need no SQLite handle for the head pass beyond
    // the path, so the parallel run is safe. On the mache bench this
    // collapses the 169ms head pass entirely into the 365ms index pass.
    // See bead `ley-line-open-cbbedf` Attack 3.
    let db_path_for_head: Option<std::path::PathBuf> = {
        let row: rusqlite::Result<String> = conn.query_row(
            "SELECT file FROM pragma_database_list WHERE name = 'main' LIMIT 1",
            [],
            |r| r.get(0),
        );
        match row {
            Ok(s) if !s.is_empty() => Some(std::path::PathBuf::from(s)),
            _ => None,
        }
    };
    let head_handle = db_path_for_head.map(|p| {
        std::thread::spawn(move || -> Result<()> {
            write_head_for_path(&p)
        })
    });

    // Build secondary indexes in one pass now that all rows are
    // landed. SQLite materializes each index by a single sorted scan
    // (~O(rows · log rows)) which is roughly an order of magnitude
    // cheaper than incremental per-row B-tree maintenance during the
    // INSERT loop. Idempotent (`IF NOT EXISTS`) so the incremental-
    // reparse path (where indexes already exist from the prior run)
    // is a no-op. See bead `ley-line-open-9ccbc7`.
    //
    // `idx_source_file` is intentionally excluded from this hot path —
    // it's a partial index whose predicate (`source_file IS NOT NULL`)
    // is false for every ley-line-produced row (only mache populates
    // `source_file`). Building it on cold parse still costs a full
    // 535K-row scan (~45 ms on the mache bench) even though the
    // resulting index is empty; the mache flow paths build their own
    // schema with the indexes mache needs, so skipping here is safe.
    // See bead `ley-line-open-cbbedf` Attack 3.
    create_post_load_indexes_skip_unused(conn)?;

    let insert_elapsed = insert_start.elapsed();

    // ---- Post-sweep ----
    //
    // Skip orphaned-dir sweep on scoped passes: it would walk the full
    // _file_index tree and incorrectly drop dirs whose other (out-of-scope)
    // files weren't loaded into this run. Full-tree passes still run it.
    //
    // Cold-parse fast-path: when no files were deleted this run, no dir
    // node can be orphaned — `ensure_dirs` only inserts dirs whose
    // descendant file we're parsing, so every dir we touched has at
    // least one child by construction, and we didn't touch any other
    // dirs. The full-scan DELETE is pure overhead. Pre-Attack 3 this
    // burned ~500ms on the mache 765-file bench (an O(N) scan of the
    // 535K-row nodes table without an `idx_kind` to accelerate it).
    // See bead `ley-line-open-cbbedf` Attack 3.
    let sweep_close_start = std::time::Instant::now();
    if scope.is_none() && deleted > 0 {
        let swept = sweep_orphaned_dirs(conn)?;
        if swept > 0 {
            eprintln!("{swept} orphaned dirs removed");
        }
    }

    // ---- Metadata ----

    let source_abs = source
        .canonicalize()
        .unwrap_or_else(|_| source.to_path_buf());
    set_meta(conn, "source_root", &source_abs.to_string_lossy())?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    set_meta(conn, "parse_time", &now.to_string())?;
    let sweep_close_elapsed = sweep_close_start.elapsed();

    // Σ root advance (bead `ley-line-open-ce55b1`) — join the worker
    // thread spawned right after COMMIT to overlap head-write FS work
    // with post-COMMIT index creation. Best-effort: a head-write
    // failure logs and doesn't fail the parse. `:memory:` connections
    // skipped (no head_handle in that case).
    let head_write_start = std::time::Instant::now();
    if let Some(h) = head_handle {
        match h.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => log::warn!("Σ head-write failed (parse otherwise OK): {e:#}"),
            Err(_) => log::warn!("Σ head-write thread panicked (parse otherwise OK)"),
        }
    }
    let head_write_elapsed = head_write_start.elapsed();

    // Per-phase timing trace — single line, stderr, surfacing the
    // wall-clock split so the next person debugging cold-parse can see
    // where time goes without rebuilding with custom timing prints.
    // See bead `ley-line-open-cbbedf` for the 1500ms gate this enables.
    let wall_elapsed = parse_elapsed + insert_elapsed + sweep_close_elapsed + head_write_elapsed;
    if oversized > 0 {
        eprintln!(
            "{parsed} parsed, {unchanged} unchanged, {deleted} deleted, \
             {errors} errors, {oversized} skipped >{}MB \
             parse={}ms insert={}ms head_write={}ms sweep_close={}ms wall={}ms",
            MAX_PARSE_FILE_SIZE / (1024 * 1024),
            parse_elapsed.as_millis(),
            insert_elapsed.as_millis(),
            head_write_elapsed.as_millis(),
            sweep_close_elapsed.as_millis(),
            wall_elapsed.as_millis(),
        );
    } else {
        eprintln!(
            "{parsed} parsed, {unchanged} unchanged, {deleted} deleted, {errors} errors \
             parse={}ms insert={}ms head_write={}ms sweep_close={}ms wall={}ms",
            parse_elapsed.as_millis(),
            insert_elapsed.as_millis(),
            head_write_elapsed.as_millis(),
            sweep_close_elapsed.as_millis(),
            wall_elapsed.as_millis(),
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
// T8.5 Σ root advance (segment hash → Head chain)
// ---------------------------------------------------------------------------

/// T8.5: canonical order of capnp segment files for hashing. Matches
/// the comment in `head.capnp`: `source.capnp || ast.capnp ||
/// bindings.capnp`. Stable, lexicographic-by-suffix. Files that don't
/// exist in this run are simply skipped (their absence contributes
/// nothing to the hash) — keeps the chain meaningful when binding
/// dual-write hasn't run yet (e.g. parse-only without enrichment).
const SEGMENT_FILE_SUFFIXES: &[&str] = &["source.capnp", "ast.capnp", "bindings.capnp"];

/// T8.5+RTFM: hash the run's capnp segment files in canonical order
/// over **canonical bytes** (segment-table prefix stripped per the
/// canonical-encoding spec, bullet 2: *"the segment table shall not
/// be included"*). Returns `(rootHash, totalCanonicalBytes)`.
///
/// Hashing canonical bytes (not raw on-disk bytes) gives Σ root
/// **byte-stability across additive schema changes**: appending a
/// field at `@N` with default value does not change the canonical
/// encoding for instances that don't set it (encoding spec, bullet 3).
/// IPLD/ATproto precedent: the CID is the version. Schema evolution
/// is handled at the typed-reading level, not by versioning the wire.
///
/// **Fast path**: when every message in the file is single-segment
/// (the case for all `set_root_canonical`-produced messages — which is
/// what `write_source_file_record` and `write_ast_node_record` always
/// emit), the on-disk format reduces to a stream of
/// `[8-byte header][N*8 bytes canonical data]` records. We can hash
/// the canonical bytes by walking the headers and feeding the data
/// slices to BLAKE3 directly — no `capnp::serialize::read_message`
/// parse, no `canonicalize()` rebuild. Empirically this is ~6× faster
/// on the mache 163 MB ast.capnp bench. See bead `ley-line-open-cbbedf`.
///
/// **Defensive path**: if a record's segment count is anything other
/// than 1 (legacy producer, future change), fall back to the
/// `read_message` + `canonicalize()` route for that whole file. The
/// fallback is opt-in for the single file with a non-canonical record,
/// not the whole hash — so a single legacy file doesn't pay the slow
/// path for the entire set.
fn hash_segment_files(db_path: &Path) -> Result<([u8; 32], u64)> {
    let mut hasher = blake3::Hasher::new();
    let mut total: u64 = 0;
    for suffix in SEGMENT_FILE_SUFFIXES {
        let p = with_extension(db_path, suffix);
        if !p.exists() {
            continue;
        }
        let file_bytes =
            std::fs::read(&p).with_context(|| format!("read segment {}", p.display()))?;
        let bytes_after = hash_canonical_stream_fast(&file_bytes, &mut hasher)
            .or_else(|| {
                // Legacy producer / multi-segment record / corruption —
                // fall through to the read_message+canonicalize path so
                // the contract is honored even when the fast path's
                // assumptions don't hold.
                hash_canonical_stream_slow(&file_bytes, &mut hasher, &p).ok()
            })
            .ok_or_else(|| anyhow::anyhow!("parse segment {}", p.display()))?;
        total = total.saturating_add(bytes_after);
    }
    Ok((*hasher.finalize().as_bytes(), total))
}

/// Fast canonical-bytes hash: walks the on-disk capnp stream as
/// `[8-byte header][segment]` records, feeding each segment's bytes
/// (the canonical bytes per `set_root_canonical`) directly to the
/// running BLAKE3 hasher. Returns `Some(total_canonical_bytes)` on
/// success, `None` if any record is multi-segment (≠ canonical-form-
/// from-producer) — the caller falls back to the slow path on `None`.
///
/// Invariant: the producer (`write_source_file_record`,
/// `write_ast_node_record`) always writes single-segment messages via
/// `set_root_canonical`. The Cap'n Proto framing format for a single
/// segment message is exactly:
///   - 4 bytes: `segment_count - 1` (== 0 for single-segment)
///   - 4 bytes: segment length in words
///   - segment_length * 8 bytes: segment data
/// No padding required since the header is already 8-byte aligned.
fn hash_canonical_stream_fast(file_bytes: &[u8], hasher: &mut blake3::Hasher) -> Option<u64> {
    const WORD_BYTES: usize = 8;
    const HEADER_BYTES: usize = 8;
    let mut total: u64 = 0;
    let mut i = 0;
    while i < file_bytes.len() {
        if i + HEADER_BYTES > file_bytes.len() {
            return None; // truncated header
        }
        let seg_count_minus_1 =
            u32::from_le_bytes([file_bytes[i], file_bytes[i + 1], file_bytes[i + 2], file_bytes[i + 3]]);
        if seg_count_minus_1 != 0 {
            return None; // multi-segment — fall back to slow path
        }
        let seg_words = u32::from_le_bytes([
            file_bytes[i + 4],
            file_bytes[i + 5],
            file_bytes[i + 6],
            file_bytes[i + 7],
        ]) as usize;
        i += HEADER_BYTES;
        let seg_bytes = seg_words * WORD_BYTES;
        if i + seg_bytes > file_bytes.len() {
            return None; // truncated segment
        }
        let canonical = &file_bytes[i..i + seg_bytes];
        hasher.update(canonical);
        total = total.saturating_add(seg_bytes as u64);
        i += seg_bytes;
    }
    Some(total)
}

/// Slow canonical-bytes hash via `read_message` + `canonicalize()`.
/// Kept as the fallback when `hash_canonical_stream_fast` returns
/// `None` (legacy producer or multi-segment record). The pre-9ccbc7
/// implementation; preserved verbatim for the fallback contract.
fn hash_canonical_stream_slow(
    file_bytes: &[u8],
    hasher: &mut blake3::Hasher,
    p: &Path,
) -> Result<u64> {
    let mut total: u64 = 0;
    let mut slice: &[u8] = file_bytes;
    while !slice.is_empty() {
        let msg =
            capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
                .with_context(|| format!("parse segment {}", p.display()))?;
        let canonical_words = msg
            .canonicalize()
            .with_context(|| format!("canonicalize segment {}", p.display()))?;
        let canonical_bytes = capnp::Word::words_to_bytes(&canonical_words);
        total = total.saturating_add(canonical_bytes.len() as u64);
        hasher.update(canonical_bytes);
    }
    Ok(total)
}

/// T8.5: read the existing `${db}.head.capnp`, returning the chain
/// state. Returns `(parentHash, generation)` where parentHash is the
/// previous root (zero if no Head exists yet) and generation is the
/// next monotonic counter value (1 if no Head exists yet).
fn read_head_for_chain(head_path: &Path) -> ([u8; 32], u64) {
    use leyline_schema_capnp::head_capnp::head;

    let bytes = match std::fs::read(head_path) {
        Ok(b) => b,
        Err(_) => return ([0u8; 32], 1),
    };
    let mut slice: &[u8] = &bytes;
    let msg = match capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
    {
        Ok(m) => m,
        Err(_) => return ([0u8; 32], 1),
    };
    let h: head::Reader = match msg.get_root() {
        Ok(h) => h,
        Err(_) => return ([0u8; 32], 1),
    };
    let prev_root = match h.get_root_hash() {
        Ok(rh) => rh
            .get_bytes()
            .ok()
            .and_then(|b| <[u8; 32]>::try_from(b).ok())
            .unwrap_or([0u8; 32]),
        Err(_) => [0u8; 32],
    };
    let prev_gen = h.get_generation();
    (prev_root, prev_gen.saturating_add(1))
}

/// T8.5: compute the segment hash for this run (from `db_path`'s
/// sibling ast/source segment files), read the existing Head for the
/// parent/gen chain, and write the new Head. Pure filesystem work —
/// no SQLite handle required, so a parent caller can dispatch this on
/// a worker thread that runs concurrently with post-COMMIT SQLite work
/// (e.g. `create_post_load_indexes`). See bead `ley-line-open-cbbedf`.
fn write_head_for_path(db_path: &Path) -> Result<()> {
    let (root, segment_bytes) = hash_segment_files(db_path)?;
    let head_path = with_extension(db_path, "head.capnp");
    let (parent, generation) = read_head_for_chain(&head_path);

    use leyline_schema_capnp::head_capnp::head;
    let mut src = capnp::message::Builder::new_default();
    {
        let mut h: head::Builder = src.init_root();
        h.set_generation(generation);
        h.set_segment_bytes(segment_bytes);
        h.reborrow().init_root_hash().set_bytes(&root);
        h.reborrow().init_parent_hash().set_bytes(&parent);
    }
    let mut canonical = capnp::message::Builder::new_default();
    canonical
        .set_root_canonical(src.get_root_as_reader::<head::Reader>()?)
        .context("canonicalize Head")?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&head_path)
        .with_context(|| format!("open head {}", head_path.display()))?;
    capnp::serialize::write_message(&mut f, &canonical).context("write Head capnp record")?;
    Ok(())
}

/// T8.5: thin wrapper around `write_head_for_path` that pulls the
/// db_path from a SQLite connection. Skips when the connection isn't
/// file-backed (`:memory:`) — same gating as T8.3's snapshot writers.
/// Kept for callers that don't have the path on hand and don't need
/// the parallel-dispatch shape that `parse_into_conn` uses internally.
#[allow(dead_code)]
fn write_head_after_parse(conn: &Connection) -> Result<()> {
    let row: rusqlite::Result<String> = conn.query_row(
        "SELECT file FROM pragma_database_list WHERE name = 'main' LIMIT 1",
        [],
        |r| r.get(0),
    );
    let db_path = match row {
        Ok(s) if !s.is_empty() => std::path::PathBuf::from(s),
        _ => return Ok(()),
    };
    write_head_for_path(&db_path)
}

// ---------------------------------------------------------------------------
// T8.3 capnp dual-write helpers
// ---------------------------------------------------------------------------

/// T8.3: derive `(ast.capnp, source.capnp)` snapshot paths from a
/// connection's backing file. `:memory:` returns `(None, None)` and
/// the caller skips the dual-write. Each parse run truncates and
/// rewrites these files — they're snapshots of `_ast` and `_source`,
/// not append-only event logs (the binding log in T8.2 is append-only
/// because LSP enrichment calls accumulate; parse is a single pass).
///
/// Returns `BufWriter<File>` so each `capnp::serialize::write_message`
/// call batches its (typically tiny) byte sequence in userspace
/// instead of issuing a `write(2)` per message. On the mache benchmark
/// (534k AstNode records) raw `File` writes burned ~3.5s in
/// `write_message` alone; with default 8 KiB userspace buffering the
/// system-call rate drops by ~30×. See bead `ley-line-open-9ccbc7`.
type CapnpWriter = BufWriter<std::fs::File>;

fn sibling_snapshot_writers(conn: &Connection) -> (Option<CapnpWriter>, Option<CapnpWriter>) {
    let row: rusqlite::Result<String> = conn.query_row(
        "SELECT file FROM pragma_database_list WHERE name = 'main' LIMIT 1",
        [],
        |r| r.get(0),
    );
    let db_path = match row {
        Ok(s) if !s.is_empty() => std::path::PathBuf::from(s),
        _ => return (None, None),
    };

    let ast_path = with_extension(&db_path, "ast.capnp");
    let source_path = with_extension(&db_path, "source.capnp");

    let open = |p: &Path| -> Option<CapnpWriter> {
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(p)
            .ok()
            .map(BufWriter::new)
    };

    (open(&ast_path), open(&source_path))
}

/// `set_extension` replaces only the *last* dotted component, so
/// `foo.bar.db` → `foo.bar.ast.capnp`. We want that exact behavior:
/// the snapshot files sit beside the db file.
fn with_extension(p: &Path, ext: &str) -> std::path::PathBuf {
    let mut out = p.to_path_buf();
    out.set_extension(ext);
    out
}

/// T8.3: serialize a single `SourceFile` capnp message to a byte buffer.
/// Per the post-RTFM canonical-encoding commitment in ADR-0014, the
/// producer writes via `set_root_canonical` so the on-disk bytes are
/// byte-stable across additive schema changes (encoding spec bullet 3:
/// *"adding a new field to a struct does not affect the canonical
/// encoding of messages that do not set that field"*).
///
/// Writes into a `Vec<u8>` so the parallel parse phase can call this
/// concurrently (the BufWriter path is single-threaded — one
/// `&mut CapnpWriter` per main-thread iteration). Main thread later
/// concatenates the per-file buffers into the BufWriter. See bead
/// `ley-line-open-cbbedf`.
fn serialize_source_file_record(
    buf: &mut Vec<u8>,
    id: &str,
    language: &str,
    canonical_path: &str,
    mtime: i64,
    size: i64,
) -> Result<()> {
    use leyline_schema_capnp::source_capnp::source_file;

    let mut src = capnp::message::Builder::new_default();
    {
        let mut sf: source_file::Builder = src.init_root();
        sf.set_id(id);
        sf.set_language(language);
        sf.set_canonical_path(canonical_path);
        sf.set_mtime(mtime as u64);
        sf.set_size(size as u64);
        // contentHash left empty for now — T8.5 wires BLAKE3.
        let _hash = sf.init_content_hash();
    }

    let mut canonical = capnp::message::Builder::new_default();
    canonical
        .set_root_canonical(src.get_root_as_reader::<source_file::Reader>()?)
        .context("canonicalize SourceFile")?;
    capnp::serialize::write_message(buf, &canonical).context("write SourceFile capnp record")?;
    Ok(())
}

/// T8.3: serialize a single `AstNode` capnp message to a byte buffer —
/// canonical form per the ADR-0014 producer commitment (see
/// serialize_source_file_record).
fn serialize_ast_node_record(buf: &mut Vec<u8>, a: &AstEntry) -> Result<()> {
    use leyline_schema_capnp::ast_capnp::ast_node;

    let mut src = capnp::message::Builder::new_default();
    {
        let mut node: ast_node::Builder = src.init_root();
        node.set_node_id(&a.node_id);
        node.set_source_id(&a.source_id);
        node.set_node_kind(&a.node_kind);
        let mut r = node.init_range();
        {
            let mut s = r.reborrow().init_start();
            s.set_line(a.start_row as u32);
            s.set_column(a.start_col as u32);
            s.set_byte(a.start_byte as u64);
        }
        {
            let mut e = r.reborrow().init_end();
            e.set_line(a.end_row as u32);
            e.set_column(a.end_col as u32);
            e.set_byte(a.end_byte as u64);
        }
    }

    let mut canonical = capnp::message::Builder::new_default();
    canonical
        .set_root_canonical(src.get_root_as_reader::<ast_node::Reader>()?)
        .context("canonicalize AstNode")?;
    capnp::serialize::write_message(buf, &canonical).context("write AstNode capnp record")?;
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
        content,
        &mut cursor,
        source_id,
        source_id,
        language,
        &mut nodes,
        &mut ast_entries,
        &mut refs,
    );

    // Pre-serialize capnp records in the rayon worker so the post-
    // parse main-thread loop just writes pre-built byte buffers to the
    // BufWriter — moves the per-file canonicalize cost out of the
    // serial insert phase. See bead `ley-line-open-cbbedf`.
    let mut source_capnp_bytes = Vec::with_capacity(256);
    serialize_source_file_record(
        &mut source_capnp_bytes,
        source_id,
        language.name(),
        abs_path,
        file_mtime,
        file_size,
    )?;
    // ~150 bytes per AstNode record (canonical: id + source_id + kind +
    // Range); pre-size to avoid re-allocs during the per-node loop.
    let mut ast_capnp_bytes = Vec::with_capacity(ast_entries.len() * 160);
    for a in &ast_entries {
        serialize_ast_node_record(&mut ast_capnp_bytes, a)?;
    }

    Ok(ParsedFile {
        rel: source_id.to_string(),
        abs_path: abs_path.to_string(),
        language: language.name().to_string(),
        nodes,
        ast_entries,
        refs,
        file_mtime,
        file_size,
        source_capnp_bytes,
        ast_capnp_bytes,
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
                content,
                &mut sub_cursor,
                &id,
                source_id,
                language,
                nodes,
                ast_entries,
                refs,
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

/// Append directory-node rows (one per path component) to the nodes
/// batch buffer. Deduplicates via the `created` set so a 50k-file
/// registry repo with deeply-shared dir hierarchies doesn't emit
/// duplicate `<prefix>` rows. The dir rows use `kind = 1` and empty
/// `record`, matching the legacy `ensure_dirs` behavior (which did the
/// same insert through `INSERT OR IGNORE`).
///
/// Why no `INSERT OR IGNORE`: nodes_buf already de-dupes via the
/// `created` set, and `INSERT OR REPLACE` (used by nodes_buf below) is
/// idempotent for matching primary keys. The `OR IGNORE` here was
/// defensive against the per-file loop re-inserting the same dir; the
/// set membership check accomplishes the same.
fn collect_dirs(
    rel: &Path,
    created: &mut HashSet<String>,
    nodes_buf: &mut NodeBatch,
    mtime: i64,
) {
    let mut accumulated = String::new();
    let components: Vec<_> = rel
        .parent()
        .into_iter()
        .flat_map(|p| p.components())
        .collect();

    for comp in components {
        let name = comp.as_os_str().to_string_lossy().into_owned();
        let parent = accumulated.clone();
        if accumulated.is_empty() {
            accumulated = name.clone();
        } else {
            accumulated = format!("{accumulated}/{name}");
        }
        if created.insert(accumulated.clone()) {
            nodes_buf.push(accumulated.clone(), parent, name, 1, 0, mtime, String::new());
        }
    }
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
    let entries = std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))?;

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
            ".venv",         // dot-prefix
            ".pytest_cache", // dot-prefix
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
            "vendored_data",       // contains "vendor"
            "subtarget",           // contains "target"
            "venvironment",        // contains "venv"
            "node_modules_helper", // contains "node_modules"
            "__init__.py",         // begins with __ but is not __pycache__/__pypackages__
            "_internal",           // begins with _ but not __
            "build",               // intentionally NOT in skip-list (often source)
            "dist",                // intentionally NOT in skip-list (often source)
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

    /// T8.3: file-backed parse emits both `${db}.ast.capnp` and
    /// `${db}.source.capnp` snapshots alongside the `.db`. The capnp
    /// records' fields agree with the SQL rows. Pin: SQL-row count ==
    /// capnp-message count for both tables.
    #[test]
    fn parse_into_conn_dual_writes_capnp_snapshots() {
        use leyline_schema_capnp::ast_capnp::ast_node;
        use leyline_schema_capnp::source_capnp::source_file;
        let td = TempDir::new().unwrap();
        let src = td.path().join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("main.go"), b"package m\n\nfunc Foo() {}\n").unwrap();

        let db_path = td.path().join("out.db");
        let conn = Connection::open(&db_path).unwrap();
        let r = parse_into_conn(&conn, &src, None, None).unwrap();
        assert_eq!(r.parsed, 1, "fixture file must parse");

        let ast_log = with_extension(&db_path, "ast.capnp");
        let source_log = with_extension(&db_path, "source.capnp");
        assert!(ast_log.exists(), "T8.3: ast.capnp snapshot must exist");
        assert!(
            source_log.exists(),
            "T8.3: source.capnp snapshot must exist"
        );

        // Read SourceFile snapshot — should have one record matching
        // the fixture file. Iterate to EOF (capnp messages back-to-
        // back, same convention as binding.capnp).
        let mut bytes: &[u8] = &std::fs::read(&source_log).unwrap();
        let mut sf_count = 0;
        let mut saw_main_go = false;
        while !bytes.is_empty() {
            let msg =
                capnp::serialize::read_message(&mut bytes, capnp::message::ReaderOptions::new())
                    .unwrap();
            let sf: source_file::Reader = msg.get_root().unwrap();
            sf_count += 1;
            if sf.get_id().unwrap().to_str().unwrap() == "main.go" {
                saw_main_go = true;
                assert_eq!(sf.get_language().unwrap().to_str().unwrap(), "go");
                assert!(
                    sf.get_canonical_path()
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .ends_with("/main.go"),
                    "canonicalPath must point to the actual file",
                );
            }
        }
        assert_eq!(sf_count, 1);
        assert!(saw_main_go, "main.go SourceFile record must be present");

        // Parity: SQL `_source` row count == capnp message count.
        let sql_source_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _source", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            sql_source_count, sf_count,
            "T8.3 parity: SQL _source rows == capnp SourceFile messages",
        );

        // AST snapshot: count messages, parity-check against SQL.
        let mut bytes: &[u8] = &std::fs::read(&ast_log).unwrap();
        let mut ast_count = 0;
        let mut saw_function_kind = false;
        while !bytes.is_empty() {
            let msg =
                capnp::serialize::read_message(&mut bytes, capnp::message::ReaderOptions::new())
                    .unwrap();
            let node: ast_node::Reader = msg.get_root().unwrap();
            ast_count += 1;
            if node.get_node_kind().unwrap().to_str().unwrap() == "function_declaration" {
                saw_function_kind = true;
            }
        }
        let sql_ast_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _ast", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            sql_ast_count, ast_count,
            "T8.3 parity: SQL _ast rows == capnp AstNode messages",
        );
        assert!(
            saw_function_kind,
            "fixture's `func Foo()` must show up as a function_declaration AstNode",
        );
    }

    /// T8.5: parse twice; head.capnp chains correctly:
    /// - run 1: parentHash == [0;32] (sentinel), generation == 1, rootHash != 0
    /// - run 2: parentHash == run1.rootHash, generation == 2
    /// And rootHash equals BLAKE3 of the segment files in canonical order.
    #[test]
    fn parse_into_conn_chains_head_across_runs() {
        use leyline_schema_capnp::head_capnp::head;
        let td = TempDir::new().unwrap();
        let src = td.path().join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("a.go"), b"package m\n\nfunc Foo() {}\n").unwrap();
        let db_path = td.path().join("out.db");
        let head_path = with_extension(&db_path, "head.capnp");

        // Run 1.
        {
            let conn = Connection::open(&db_path).unwrap();
            parse_into_conn(&conn, &src, None, None).unwrap();
        }
        let bytes = std::fs::read(&head_path).unwrap();
        let mut slice: &[u8] = &bytes;
        let msg = capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
            .unwrap();
        let h: head::Reader = msg.get_root().unwrap();
        let run1_root: [u8; 32] = h
            .get_root_hash()
            .unwrap()
            .get_bytes()
            .unwrap()
            .try_into()
            .unwrap();
        let run1_parent: [u8; 32] = h
            .get_parent_hash()
            .unwrap()
            .get_bytes()
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(
            run1_parent, [0u8; 32],
            "T8.5: first parse parent must be zero"
        );
        assert_eq!(h.get_generation(), 1, "T8.5: first parse gen == 1");
        assert_ne!(run1_root, [0u8; 32], "T8.5: rootHash must be non-zero");

        // Independently re-hash to verify the rootHash is correct.
        let (independent_hash, _) = hash_segment_files(&db_path).unwrap();
        assert_eq!(
            run1_root, independent_hash,
            "T8.5: rootHash must equal BLAKE3 of segment files",
        );

        // Run 2 — modify the file so the segment changes.
        std::fs::write(
            src.join("a.go"),
            b"package m\n\nfunc Foo() {}\nfunc Bar() {}\n",
        )
        .unwrap();
        {
            let conn = Connection::open(&db_path).unwrap();
            parse_into_conn(&conn, &src, None, None).unwrap();
        }
        let bytes = std::fs::read(&head_path).unwrap();
        let mut slice: &[u8] = &bytes;
        let msg = capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
            .unwrap();
        let h: head::Reader = msg.get_root().unwrap();
        let run2_root: [u8; 32] = h
            .get_root_hash()
            .unwrap()
            .get_bytes()
            .unwrap()
            .try_into()
            .unwrap();
        let run2_parent: [u8; 32] = h
            .get_parent_hash()
            .unwrap()
            .get_bytes()
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(
            run2_parent, run1_root,
            "T8.5: run2 parentHash must == run1 rootHash (chain invariant)",
        );
        assert_eq!(h.get_generation(), 2, "T8.5: gen monotonically increments");
        assert_ne!(
            run2_root, run1_root,
            "rootHash differs because segment changed"
        );
    }

    /// T8 canonical-encoding (post-RTFM, ADR-0014): hashing the same
    /// run's segment files must yield the same `rootHash` regardless
    /// of whether the producer wrote canonical or non-canonical bytes,
    /// because `hash_segment_files` re-canonicalizes on read. Also
    /// pins the structural property: a fresh head.capnp's `rootHash`
    /// equals an independent `hash_segment_files()` call against the
    /// same db. Pin guards the byte-stability invariant the math-
    /// friend's analysis and the RTFM dossier both flag as load-
    /// bearing.
    #[test]
    fn segment_hash_is_canonical_byte_stable() {
        let td = TempDir::new().unwrap();
        let src = td.path().join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("a.go"), b"package m\n\nfunc Foo() {}\n").unwrap();
        let db_path = td.path().join("out.db");

        let conn = Connection::open(&db_path).unwrap();
        parse_into_conn(&conn, &src, None, None).unwrap();
        drop(conn);

        let (h1, total1) = hash_segment_files(&db_path).unwrap();
        let (h2, total2) = hash_segment_files(&db_path).unwrap();
        assert_eq!(h1, h2, "hash_segment_files must be deterministic");
        assert_eq!(total1, total2, "canonical-byte total must be deterministic");
        assert_ne!(h1, [0u8; 32], "non-zero rootHash with real data");

        // Read the head.capnp written by parse_into_conn; assert it
        // matches the independent hash. This is the consumer-verifiability
        // property: a third party can validate Σ root by re-hashing the
        // segments themselves, not by trusting the producer.
        use leyline_schema_capnp::head_capnp::head;
        let head_path = with_extension(&db_path, "head.capnp");
        let bytes = std::fs::read(&head_path).unwrap();
        let mut slice: &[u8] = &bytes;
        let msg = capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
            .unwrap();
        let h: head::Reader = msg.get_root().unwrap();
        let stored: [u8; 32] = h
            .get_root_hash()
            .unwrap()
            .get_bytes()
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(
            stored, h1,
            "Head.rootHash must equal independent canonical hash of segments",
        );

        // Pin: total canonical bytes is non-zero AND strictly less than
        // raw file bytes (canonical form strips segment-table prefixes).
        let raw_total: u64 = SEGMENT_FILE_SUFFIXES
            .iter()
            .map(|s| {
                std::fs::metadata(with_extension(&db_path, s))
                    .map(|m| m.len())
                    .unwrap_or(0)
            })
            .sum();
        assert!(
            total1 < raw_total,
            "canonical bytes ({total1}) must be < raw bytes ({raw_total}) — segment table stripped"
        );
    }

    /// T8.3: `:memory:` connections must NOT attempt the capnp dual-
    /// write (no path to write next to). Pin so a future refactor that
    /// changes `sibling_snapshot_writers` doesn't accidentally write
    /// to `cwd/.ast.capnp` or fail with a panic on the fallback path.
    #[test]
    fn parse_into_conn_memory_skips_capnp_snapshots() {
        let td = TempDir::new().unwrap();
        let src = td.path().join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("main.go"), b"package m\n").unwrap();

        let conn = Connection::open_in_memory().unwrap();
        parse_into_conn(&conn, &src, None, None).unwrap();

        // No files should have been written into the cwd or temp dir.
        assert!(
            !td.path().join(".ast.capnp").exists() && !td.path().join(".source.capnp").exists(),
            "T8.3: :memory: parse must not produce capnp snapshots",
        );
    }
}
