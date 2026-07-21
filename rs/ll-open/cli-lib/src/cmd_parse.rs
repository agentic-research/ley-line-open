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
use leyline_core::ContentAddressed;
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
use leyline_ts::query_engine::QuerySet;
use leyline_ts::refs::{ExtractedRef, current_extraction_epoch, extract_refs_resolved};
use leyline_ts::schema::{
    create_ast_tables, create_index_schema, create_ir_indexes, create_ir_tables,
    create_pointer_store_tables, create_post_load_indexes_skip_unused, create_qualifier_column,
    create_query_blob_tables, create_refs_tables, create_source_blobs_table, delete_file_rows,
    get_meta, read_file_index, set_meta, sweep_orphaned_dirs,
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
    /// Merkle-AST content address (ADR-0027): the intrinsic `node_hash`
    /// of this node's subtree (κ kind + terminal token + ordered child
    /// hashes; spans/paths/node_ids are OUT). Stamped onto the `_ast`
    /// occurrence row and pointing at the deduped `node_content` row.
    node_hash: [u8; 32],
}

/// A `node_content` row — one per UNIQUE subtree (deduped by `node_hash`).
struct ContentRow {
    node_hash: [u8; 32],
    node_tag: u8,
    kind: String,
    raw_kind: String,
    lang: String,
    token: Option<String>,
    arity: usize,
}

/// A `node_child` row — the git-tree edge parent→child at `ordinal`.
struct ChildRow {
    parent_hash: [u8; 32],
    ordinal: usize,
    child_hash: [u8; 32],
    field: Option<String>,
}

pub(crate) struct ParsedFile {
    rel: String,
    abs_path: String,
    language: String,
    nodes: Vec<ParsedNode>,
    ast_entries: Vec<AstEntry>,
    refs: Vec<ExtractedRef>,
    /// Deduped `node_content` rows for this file's subtrees (post-order,
    /// children before parents). Cross-file dedup happens at SQL insert
    /// time via `INSERT OR IGNORE` on the `node_hash` PK.
    node_contents: Vec<ContentRow>,
    /// `node_child` rows for this file's unique internal nodes.
    node_children: Vec<ChildRow>,
    file_mtime: i64,
    file_size: i64,
    /// BLAKE3-32 of the file bytes. Computed in the rayon worker from the
    /// same `content` slice tree-sitter parsed, so it costs one extra hash
    /// pass over already-in-cache bytes. Populates `_source.contentHash`
    /// (closing the T8.5 TODO) and is the first component of every
    /// `symbol_id` (ADR-0027 / mache ADR-0023).
    content_hash: [u8; 32],
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
    /// ADR-0026 pointer-store blob (bead `ley-line-open-3e87ad`, Phase 1):
    /// canonical bytes of a single `AstNodeList` message containing every
    /// AstEntry for this file. Written to `capnp_blobs.blob_bytes`; the
    /// per-node offsets in `ast_entries.iter().enumerate()` land in
    /// `_ast_pointer.offset_in_blob`. Built in the rayon worker so the
    /// serial insert loop does no capnp work.
    pointer_blob_bytes: Vec<u8>,
    /// BLAKE3-32 of `pointer_blob_bytes`. Populates
    /// `capnp_blobs.blob_hash` (PK) and every `_ast_pointer.blob_hash` FK
    /// for this file's rows.
    pointer_blob_hash: [u8; 32],
    /// ADR-0028 source-blob bytes (bead `ley-line-open-9e4416`, Phase 1):
    /// verbatim file bytes as read from disk by the rayon worker, moved into
    /// `source_blobs.blob_bytes` on the main-thread insert loop. Byte-identical
    /// to the input `content` slice (F1s asserts this). One `Vec<u8>` clone per
    /// file — the same allocation the existing `content_hash` already reads.
    source_blob_bytes: Vec<u8>,
    /// Injections (bead `ley-line-open-c822a6`): (node_id → merkle
    /// node_hash) for every named node of every INJECTED subtree in
    /// this file. Injected nodes have no `_ast` occurrence rows — the
    /// host occurrence layer is untouched by design, so host structural
    /// identity stays independent of the injected grammar's version —
    /// but their fact rows still need the `node_hash` pointer at their
    /// own content-addressed subtree. Merged into `hash_by_id` in the
    /// insert loop, next to the `_ast`-derived entries.
    injected_hashes: Vec<(String, [u8; 32])>,
}

/// Per-parse extraction context threaded through the fold (bead
/// `ley-line-open-e72629`). `queries` is the resolved effective query
/// set (compiled defaults + trusted arena overrides), shared read-only
/// across rayon workers. `bounds` is a per-FILE flag (created inside the
/// worker) set when an override engine trips its resource bounds.
struct ExtractCtx<'a> {
    queries: &'a QuerySet,
    bounds: &'a std::cell::Cell<bool>,
}

// ---------------------------------------------------------------------------
// Batched-insert plumbing
// ---------------------------------------------------------------------------

/// Rows per multi-row VALUES batch. 3000 × 9 columns (`_ast`, the
/// widest table) = 27 000 bound parameters per statement — under
/// SQLite's 32K bound-param cap (`SQLITE_MAX_VARIABLE_NUMBER` default
/// 32 766 since 3.32) with ~5 700 params of headroom. Larger batches
/// collapse more transaction edges per execute; on the mache
/// 765-file bench going from 500 → 2000 → 3000 rows/batch shaves
/// successive 10-15% chunks off insert wall.
///
/// The per-batch SQL string at 3000×9 is ~60 KiB. The prepared-
/// statement cache holds one entry per unique SQL string, so each
/// table pays this cost once (the always-full batch) plus one
/// per-table partial-batch string at flush time. Going past 3000
/// requires a smaller per-row column count or a higher
/// `SQLITE_MAX_VARIABLE_NUMBER` at SQLite build time.
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
    //
    // `source_file` (bead `ley-line-open-caf423`): the shared
    // `nodes.source_file` column carries the originating source file's
    // path for every AST-derived row. Directory nodes (from
    // `collect_dirs`) and the root '' node have no source file and pass
    // `None`. Mache's cross-language rules JOIN on this column; pre-fix
    // it was always NULL and the rules silently reduced to false
    // positives.
    NodeBatch, NodeRow,
    "INSERT OR IGNORE INTO nodes (id, parent_id, name, kind, size, mtime, record, source_file) VALUES ",
    8,
    push_fn: (id: String, parent_id: String, name: String, kind: i32, size: i64, mtime: i64, record: String, source_file: Option<String>),
    push_body: { NodeRow { id, parent_id, name, kind, size, mtime, record, source_file } },
    flatten: |chunk| {
        let mut out: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 8);
        for r in chunk {
            out.push(&r.id);
            out.push(&r.parent_id);
            out.push(&r.name);
            out.push(&r.kind);
            out.push(&r.size);
            out.push(&r.mtime);
            out.push(&r.record);
            out.push(&r.source_file);
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
    "INSERT INTO _ast (node_id, source_id, node_kind, start_byte, end_byte, start_row, start_col, end_row, end_col, node_hash) VALUES ",
    10,
    push_fn: (node_id: String, source_id: String, node_kind: String, start_byte: i64, end_byte: i64, start_row: i64, start_col: i64, end_row: i64, end_col: i64, node_hash: Vec<u8>),
    push_body: { AstRow { node_id, source_id, node_kind, start_byte, end_byte, start_row, start_col, end_row, end_col, node_hash } },
    flatten: |chunk| {
        let mut out: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 10);
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
            out.push(&r.node_hash);
        }
        out
    },
}

batch_table! {
    // Plain INSERT: same rationale as AstBatch — delete_file_rows
    // clears _source rows per file before reparse.
    SourceBatch, SourceRow,
    "INSERT INTO _source (id, language, path, content_hash) VALUES ",
    4,
    push_fn: (id: String, language: String, path: String, content_hash: Vec<u8>),
    push_body: { SourceRow { id, language, path, content_hash } },
    flatten: |chunk| {
        let mut out: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 4);
        for r in chunk {
            out.push(&r.id);
            out.push(&r.language);
            out.push(&r.path);
            out.push(&r.content_hash);
        }
        out
    },
}

batch_table! {
    // Merkle-AST `node_content` rows (ADR-0027). One per UNIQUE subtree.
    // `INSERT OR IGNORE` on the `node_hash` PRIMARY KEY == intrinsic dedup:
    // the second occurrence of an identical subtree (a vendored copy, an
    // empty `__init__.py`, a shared operator leaf) is silently ignored. No
    // `gen` column — content identity is parse-run-invariant by
    // construction, so re-emitting an existing subtree is a clean no-op.
    NodeContentBatch, NodeContentRow,
    "INSERT OR IGNORE INTO node_content (node_hash, node_tag, kind, raw_kind, lang, token, arity) VALUES ",
    7,
    push_fn: (
        node_hash: Vec<u8>, node_tag: i64, kind: String, raw_kind: String,
        lang: String, token: Option<String>, arity: i64
    ),
    push_body: { NodeContentRow { node_hash, node_tag, kind, raw_kind, lang, token, arity } },
    flatten: |chunk| {
        let mut out: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 7);
        for r in chunk {
            out.push(&r.node_hash);
            out.push(&r.node_tag);
            out.push(&r.kind);
            out.push(&r.raw_kind);
            out.push(&r.lang);
            out.push(&r.token);
            out.push(&r.arity);
        }
        out
    },
}

batch_table! {
    // Merkle-AST `node_child` rows (ADR-0027) — the git-tree object.
    // `INSERT OR IGNORE` on the (parent_hash, ordinal) PK dedups the child
    // edges of an already-seen parent subtree. FK endpoints resolve because
    // `node_content` is flushed first.
    NodeChildBatch, NodeChildRow,
    "INSERT OR IGNORE INTO node_child (parent_hash, ordinal, child_hash, field) VALUES ",
    4,
    push_fn: (parent_hash: Vec<u8>, ordinal: i64, child_hash: Vec<u8>, field: Option<String>),
    push_body: { NodeChildRow { parent_hash, ordinal, child_hash, field } },
    flatten: |chunk| {
        let mut out: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 4);
        for r in chunk {
            out.push(&r.parent_hash);
            out.push(&r.ordinal);
            out.push(&r.child_hash);
            out.push(&r.field);
        }
        out
    },
}

batch_table! {
    // `node_refs` occurrence rows (ADR-0027). Shape:
    // (token, node_id, source_id, node_hash, container_node_id).
    // node_hash carries the merkle-AST identity of the occurrence
    // (ADR-0027); container_node_id (bead ley-line-open-6e798d) carries
    // the enclosing κ function/method's node_id for per-caller
    // aggregation without a recursive _ast walk. Keyed by
    // token+node_id+source_id, NEVER by node_hash (the one-to-many
    // invariant).
    //
    // Split from the old shared RefBatch when node_defs gained the
    // `canonical_kind` column (mache-parity follow-up to 6e798d) —
    // refs don't get canonical_kind because a ref site's κ kind is
    // `call_expression` etc., not a symbol kind, so the column would
    // always be NULL.
    //
    // `qualifier` (bead ley-line-open-4dde42) is the receiver/selector
    // text on the BARE-token row of a qualified call's dual-emit pair;
    // NULL on the qualified-token row and on genuinely bare calls.
    RefBatch, RefRow,
    "",
    6,
    push_fn: (token: String, node_id: String, source_id: String, node_hash: Option<Vec<u8>>, container_node_id: Option<String>, qualifier: Option<String>),
    push_body: { RefRow { token, node_id, source_id, node_hash, container_node_id, qualifier } },
    flatten: |chunk| {
        let mut out: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 6);
        for r in chunk {
            out.push(&r.token);
            out.push(&r.node_id);
            out.push(&r.source_id);
            out.push(&r.node_hash);
            out.push(&r.container_node_id);
            out.push(&r.qualifier);
        }
        out
    },
}

impl RefBatch {
    fn flush_batched_for(self, conn: &Connection, prefix: &str) -> Result<()> {
        flush_in_batches(conn, self.rows, prefix, 6, |chunk| {
            let mut out: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 6);
            for r in chunk {
                out.push(&r.token);
                out.push(&r.node_id);
                out.push(&r.source_id);
                out.push(&r.node_hash);
                out.push(&r.container_node_id);
                out.push(&r.qualifier);
            }
            out
        })
    }
}

batch_table! {
    // `node_defs` occurrence rows. Wider than RefBatch by one column:
    // `canonical_kind` (mache-parity follow-up to bead
    // `ley-line-open-6e798d`, cross-repo signal 2026-07-13). κ kind of
    // the def itself so consumers filter by symbol-scope κ kind without
    // JOINing node_content. Same (token, node_id, source_id, node_hash,
    // container_node_id) shape as refs, plus `canonical_kind`.
    DefBatch, DefRow,
    "",
    6,
    push_fn: (
        token: String,
        node_id: String,
        source_id: String,
        node_hash: Option<Vec<u8>>,
        container_node_id: Option<String>,
        canonical_kind: Option<&'static str>
    ),
    push_body: { DefRow { token, node_id, source_id, node_hash, container_node_id, canonical_kind } },
    flatten: |chunk| {
        let mut out: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 6);
        for r in chunk {
            out.push(&r.token);
            out.push(&r.node_id);
            out.push(&r.source_id);
            out.push(&r.node_hash);
            out.push(&r.container_node_id);
            out.push(&r.canonical_kind);
        }
        out
    },
}

impl DefBatch {
    fn flush_batched_for(self, conn: &Connection, prefix: &str) -> Result<()> {
        flush_in_batches(conn, self.rows, prefix, 6, |chunk| {
            let mut out: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 6);
            for r in chunk {
                out.push(&r.token);
                out.push(&r.node_id);
                out.push(&r.source_id);
                out.push(&r.node_hash);
                out.push(&r.container_node_id);
                out.push(&r.canonical_kind);
            }
            out
        })
    }
}

batch_table! {
    // `_imports` rows (alias, path, source_id). No `node_hash` — imports are
    // a flat alias→path projection, not an AST occurrence.
    ImportBatch, ImportRow,
    "INSERT INTO _imports (alias, path, source_id) VALUES ",
    3,
    push_fn: (alias: String, path: String, source_id: String),
    push_body: { ImportRow { alias, path, source_id } },
    flatten: |chunk| {
        let mut out: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 3);
        for r in chunk {
            out.push(&r.alias);
            out.push(&r.path);
            out.push(&r.source_id);
        }
        out
    },
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

batch_table! {
    // ADR-0026 pointer-store blob rows (bead `ley-line-open-3e87ad`).
    // `INSERT OR IGNORE` on the `blob_hash` PK == intrinsic dedup: two
    // files with byte-identical AstNodeList canonical bytes share one blob.
    // Phase 1 is per-file granularity; Phase 2 refines toward per-semantic-
    // unit (ADR-0026 §2.2).
    CapnpBlobBatch, CapnpBlobRow,
    "INSERT OR IGNORE INTO capnp_blobs (blob_hash, blob_bytes) VALUES ",
    2,
    push_fn: (blob_hash: Vec<u8>, blob_bytes: Vec<u8>),
    push_body: { CapnpBlobRow { blob_hash, blob_bytes } },
    flatten: |chunk| {
        let mut out: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 2);
        for r in chunk {
            out.push(&r.blob_hash);
            out.push(&r.blob_bytes);
        }
        out
    },
}

batch_table! {
    // ADR-0028 source-blob rows (bead `ley-line-open-9e4416`, Phase 1 dual-
    // store). `INSERT OR IGNORE` on the `blob_hash` PK == intrinsic dedup:
    // two files with byte-identical source content share one blob (F5s).
    // Phase 1 blob unit is per-file; sub-file (CDC) refinement composes with
    // ley-line ADR-014 downstream.
    SourceBlobBatch, SourceBlobRow,
    "INSERT OR IGNORE INTO source_blobs (blob_hash, blob_bytes) VALUES ",
    2,
    push_fn: (blob_hash: Vec<u8>, blob_bytes: Vec<u8>),
    push_body: { SourceBlobRow { blob_hash, blob_bytes } },
    flatten: |chunk| {
        let mut out: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 2);
        for r in chunk {
            out.push(&r.blob_hash);
            out.push(&r.blob_bytes);
        }
        out
    },
}

batch_table! {
    // ADR-0026 pointer rows — one per `_ast` entry, dual-write for Phase 1
    // (bead `ley-line-open-3e87ad`). `delete_file_rows` clears prior rows
    // per file, so plain `INSERT` doesn't conflict.
    AstPointerBatch, AstPointerRow,
    "INSERT INTO _ast_pointer (node_id, blob_hash, offset_in_blob, kind, source_id) VALUES ",
    5,
    push_fn: (node_id: String, blob_hash: Vec<u8>, offset_in_blob: i64, kind: i64, source_id: String),
    push_body: { AstPointerRow { node_id, blob_hash, offset_in_blob, kind, source_id } },
    flatten: |chunk| {
        let mut out: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 5);
        for r in chunk {
            out.push(&r.node_id);
            out.push(&r.blob_hash);
            out.push(&r.offset_in_blob);
            out.push(&r.kind);
            out.push(&r.source_id);
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
    // The `libc::_exit` shortcut that bypasses the rest of the rust
    // shutdown lives in `cli/src/main.rs` (gated to the parse subcommand
    // success path only). It is not reachable from this function, from
    // integration tests, or from the daemon path — those still go
    // through normal Drop. This `mem::forget` is the local saving:
    // ~65ms of SQLite FD-teardown that the kernel will reclaim on
    // process exit anyway.
    //
    // See bead `ley-line-open-cbbedf` Attack 3.
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
    // Structural qualifier (bead `ley-line-open-4dde42`): additively ALTER
    // legacy (≤ v0.7.8) node_refs shapes that predate the column. Must run
    // after `create_refs_tables` (the ALTER target) and before the insert
    // transaction — the extraction-epoch bump forces those arenas to
    // re-derive facts, and the re-derive INSERT names the column.
    create_qualifier_column(conn)?;
    // Merkle-AST IR (ADR-0027): create node_content/node_child and stamp the
    // additive node_hash column onto _ast/node_defs/node_refs. Must run after
    // the occurrence tables exist (the ALTER targets) and before the insert
    // transaction, so the node_hash FK → node_content is live when
    // PRAGMA foreign_keys=ON enforces it below.
    create_ir_tables(conn)?;
    // ADR-0026 Phase 1 dual-write (bead `ley-line-open-3e87ad`): the pointer-
    // store tables land alongside the row-projected schema. Both populated on
    // every parse; F1 (round-trip integrity) asserts continuous agreement.
    create_pointer_store_tables(conn)?;
    // ADR-0028 Phase 1 dual-store (bead `ley-line-open-9e4416`): source_blobs
    // lands alongside `_source`. `_source.content_hash` (already populated for
    // the Σ head chain) becomes the FK-shaped pointer into source_blobs. F1s
    // (round-trip integrity), F4s (cross-gen dedup), F5s (cross-file dedup),
    // F-rename, and F-git assert continuous agreement.
    create_source_blobs_table(conn)?;
    // Arena-resident query overrides (bead `ley-line-open-e72629`): the
    // override-blob store lands with the rest of the schema so `resolve_query_set`
    // below reads a well-formed (possibly empty) `_queries`/`query_blobs` pair,
    // and so the FK check at COMMIT finds both tables present.
    create_query_blob_tables(conn)?;
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

    // Extraction-rules provenance (bead `ley-line-open-20988a`).
    // Derived facts (node_defs/node_refs/_imports) are keyed on
    // node_hash — a fold over source bytes — so a rules change with
    // unchanged sources is invisible to both the mtime+size skip below
    // and the sheaf's node_hash invalidation (v0.7.8 shipped exactly
    // this: extract_go emission changed, existing arenas kept serving
    // the old node_refs). Compare the epoch that produced the arena's
    // facts against this binary's; on any disagreement — including the
    // missing row every pre-epoch arena has — disable the
    // unchanged-skip so this pass re-derives every file. A rules
    // change is inherently global: one-shot invalidation here, and the
    // sheaf repopulates from the reparse like any other change.
    let extraction_epoch = current_extraction_epoch().to_string();
    // Composite injection epoch (bead `ley-line-open-c822a6`): injected
    // facts depend on inputs the scalar epoch does not see — the host's
    // injections.scm, the injected language's tags.scm, and both
    // grammars. Same staleness shape as above, so the same gate: ANY
    // disagreement (including the missing row every pre-injection arena
    // has) disables the unchanged-skip. The missing-row case is also
    // what delivers injected facts to existing arenas on upgrade — no
    // EXTRACTION_EPOCH bump accompanies the injections feature.
    let injection_epoch = leyline_ts::injections::current_injection_epoch();

    // Arena-resident query overrides (bead `ley-line-open-e72629`): resolve the
    // effective query set (compiled defaults + TRUSTED arena overrides) once per
    // pass. The allowlist is operator-controlled via env — the arena writer must
    // not be able to self-trust a blob. An untrusted/corrupt override is ignored
    // with exactly one stderr line (compiled fallback); a trusted-but-malformed
    // one fails the whole pass loud.
    let trusted_hashes: std::collections::HashSet<String> =
        std::env::var("LLO_TRUSTED_QUERY_HASHES")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|h| h.trim().to_ascii_lowercase())
                    .filter(|h| !h.is_empty())
                    .collect()
            })
            .unwrap_or_default();
    let resolution = leyline_ts::query_engine::resolve_query_set(conn, &trusted_hashes)?;
    for w in &resolution.warnings {
        eprintln!("warn: {w}");
    }
    let query_set = resolution.query_set;
    // Composite query-set epoch — the active override set is a fact-derivation
    // input the scalar extraction_epoch does not see (swapping an arena's
    // override changes emission for byte-identical sources). Same staleness
    // shape as extraction/injection epochs: ANY disagreement — including the
    // missing row every pre-override arena has — disables the unchanged-skip and
    // forces re-derivation; the missing-row case also delivers overrides to an
    // existing arena on first adoption.
    let query_set_epoch = leyline_ts::query_engine::query_set_epoch(&query_set);

    let epoch_current = incremental
        && get_meta(conn, "extraction_epoch").ok().flatten().as_deref()
            == Some(extraction_epoch.as_str())
        && get_meta(conn, "injection_epoch").ok().flatten().as_deref()
            == Some(injection_epoch.as_str())
        && get_meta(conn, "query_set_epoch").ok().flatten().as_deref()
            == Some(query_set_epoch.as_str());
    if incremental && !epoch_current {
        eprintln!(
            "extraction epoch changed (binary epoch {extraction_epoch}, \
             injection composite {injection_epoch}, query-set {query_set_epoch}); \
             re-deriving facts for all files",
        );
    }

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

        if epoch_current
            && let Some(&(old_m, old_s)) = old_index.get(&rel_str)
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
            parse_file_pure(
                &content,
                *lang,
                rel,
                &abs_str,
                *file_mtime,
                *file_size,
                &query_set,
            )
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
    // Batch size: BULK_BATCH_ROWS (== 3000) rows per execute, under
    // SQLite's 32K bound-param cap even for the 9-column _ast table
    // (27 000 params, ~5 700 headroom). The "full batch" SQL string is
    // the same every execute → prepare_cached hits the per-table key
    // once and reuses. The trailing partial batch (< BULK_BATCH_ROWS
    // rows) is flushed with a separately-sized statement; on a
    // 765-file corpus this happens once per table at end of insert.
    //
    // See bead `ley-line-open-cbbedf`.

    let insert_start = std::time::Instant::now();
    let mut parsed = 0u64;
    let mut errors = 0u64;

    // Unified code-fact IR (ADR-0027): enforce `fact_edges` FK endpoints at
    // write time so a dangling edge is a loud insert error, not a silently-
    // zeroed downstream JOIN (the be6136 lesson). `foreign_keys` is a
    // per-connection, no-op-inside-a-transaction pragma, so it must be set
    // before BEGIN. Set here in `parse_into_conn` (not `cmd_parse`) so the
    // in-memory test path gets it too. Only `fact_edges` has FKs; the other
    // tables predate this and are unaffected, including the delete_file_rows
    // sweep above (it touches no FK relationship).
    conn.pragma_update(None, "foreign_keys", "ON")?;

    conn.execute_batch("BEGIN")?;

    // Defer FK enforcement to COMMIT: with immediate FKs, every node_child /
    // _ast.node_hash insert probes node_content's BLOB PK synchronously (~880k
    // per-row probes into a large index → cache thrash). `defer_foreign_keys`
    // batches all checks into a single validation pass at COMMIT — still
    // fail-loud (a dangling edge aborts the COMMIT), but no per-row probe. It
    // is a per-transaction pragma, so it is set after BEGIN and resets on
    // COMMIT. Insert order (node_content before its referrers) still holds, so
    // the deferred check passes on well-formed input.
    conn.pragma_update(None, "defer_foreign_keys", "ON")?;

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
    let mut defs_buf: DefBatch = DefBatch::with_capacity(3_000);
    let mut imports_buf: ImportBatch = ImportBatch::with_capacity(2_000);
    let mut source_buf: SourceBatch = SourceBatch::with_capacity(to_parse.len());
    let mut file_idx_buf: FileIdxBatch = FileIdxBatch::with_capacity(to_parse.len());

    // ADR-0026 pointer store (Phase 1 dual-write, bead `ley-line-open-3e87ad`).
    // One `capnp_blobs` row per file; one `_ast_pointer` row per AstEntry
    // (mirrors the `_ast` row count 1-to-1). Pre-sized like `ast_buf` since
    // pointer rows and _ast rows are in 1-to-1 correspondence.
    let mut blob_buf: CapnpBlobBatch = CapnpBlobBatch::with_capacity(to_parse.len());
    let mut pointer_buf: AstPointerBatch = AstPointerBatch::with_capacity(550_000);

    // ADR-0028 source blobs (Phase 1 dual-store, bead `ley-line-open-9e4416`).
    // One `source_blobs` row per file *before* `INSERT OR IGNORE` dedup; unique
    // source content collapses at flush time. Pre-sized at `to_parse.len()` —
    // the pre-dedup upper bound.
    let mut source_blob_buf: SourceBlobBatch = SourceBlobBatch::with_capacity(to_parse.len());

    // Merkle-AST IR (ADR-0027). `node_content`/`node_child` are the deduped
    // content layer (`INSERT OR IGNORE` collapses identical subtrees across
    // files); `_ast`/`node_defs`/`node_refs` carry the additive `node_hash`
    // pointer. No `gen`/edge machinery: contains is intrinsic (node_child),
    // and defines/references stay as occurrence rows keyed by token+node_id.
    let mut content_buf: NodeContentBatch = NodeContentBatch::with_capacity(550_000);
    let mut child_buf: NodeChildBatch = NodeChildBatch::with_capacity(550_000);
    // Cross-file dedup in memory, NOT via `INSERT OR IGNORE`. The per-file
    // fold already dedups within a file, but identical subtrees recur across
    // files (~2M fold rows collapse to ~150k unique). Letting SQL dedup means
    // ~2M probes into a growing BLOB primary-key B-tree during the insert —
    // the classic "maintain a UNIQUE index during a bulk load" anti-pattern,
    // and the measured cold-parse hot spot. A HashSet lookup is ~2 orders of
    // magnitude cheaper than a BLOB index probe that misses to disk once the
    // index outgrows the page cache. `node_content` is a content-addressed
    // blob store; dedup belongs at the write, not in a per-row index probe.
    let mut seen_content: HashSet<[u8; 32]> = HashSet::with_capacity(200_000);
    let mut seen_edge: HashSet<([u8; 32], i64)> = HashSet::with_capacity(450_000);

    let mut dirs_created: HashSet<String> = HashSet::new();
    let mut changed_files: Vec<String> = Vec::new();

    for result in parsed_files {
        match result {
            Ok(pf) => {
                let rel_path = Path::new(&pf.rel);
                collect_dirs(rel_path, &mut dirs_created, &mut nodes_buf, mtime);

                source_buf.push(
                    pf.rel.clone(),
                    pf.language.clone(),
                    pf.abs_path.clone(),
                    pf.content_hash.to_vec(),
                );

                // ADR-0028 dual-store (Phase 1, bead `ley-line-open-9e4416`).
                // One `source_blobs` row per file, byte-verbatim; `INSERT OR
                // IGNORE` on the `blob_hash` PK collapses byte-identical files
                // to one row (F5s). `_source.content_hash` (pushed above) is
                // the FK-shaped pointer at this blob (F1s asserts round-trip).
                source_blob_buf.push(pf.content_hash.to_vec(), pf.source_blob_bytes.clone());

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

                // Bead `ley-line-open-caf423`: every AST-derived node
                // carries its source file's `_source.id` as
                // `source_file`. The file's own row + every descendant
                // AST node share the same `source_file` (the
                // relative path). Directory nodes (created via
                // `collect_dirs`) intentionally leave `source_file` as
                // `None` — they don't belong to a single file.
                for n in pf.nodes {
                    nodes_buf.push(
                        n.id,
                        n.parent_id,
                        n.name,
                        n.kind,
                        n.size,
                        mtime,
                        n.record,
                        Some(pf.rel.clone()),
                    );
                }

                // Merkle-AST content layer (post-order, children before
                // parents). Cross-file dedup happens here in memory: only the
                // FIRST occurrence of a content hash / (parent,ordinal) edge is
                // buffered, so the SQL insert sees ~150k unique rows, not ~2M.
                // The `INSERT OR IGNORE` prefix stays as a byte-identical-file
                // backstop, but the probe storm is gone.
                for c in pf.node_contents {
                    if seen_content.insert(c.node_hash) {
                        content_buf.push(
                            c.node_hash.to_vec(),
                            c.node_tag as i64,
                            c.kind,
                            c.raw_kind,
                            c.lang,
                            c.token,
                            c.arity as i64,
                        );
                    }
                }
                for c in pf.node_children {
                    if seen_edge.insert((c.parent_hash, c.ordinal as i64)) {
                        child_buf.push(
                            c.parent_hash.to_vec(),
                            c.ordinal as i64,
                            c.child_hash.to_vec(),
                            c.field,
                        );
                    }
                }

                // node_id → node_hash for this file, so the additive
                // node_hash pointer on node_defs/node_refs can be attached
                // by the ref locator (which is always an `_ast` node_id).
                let mut hash_by_id: HashMap<&str, [u8; 32]> =
                    HashMap::with_capacity(pf.ast_entries.len() + pf.injected_hashes.len());
                for a in &pf.ast_entries {
                    hash_by_id.insert(a.node_id.as_str(), a.node_hash);
                }
                // Injections (bead `ley-line-open-c822a6`): injected
                // nodes have no `_ast` rows; their (node_id →
                // node_hash) pairs ride ParsedFile so injected fact
                // rows resolve to their own content-addressed subtrees.
                for (id, h) in &pf.injected_hashes {
                    hash_by_id.insert(id.as_str(), *h);
                }

                for a in &pf.ast_entries {
                    ast_buf.push(
                        a.node_id.clone(),
                        a.source_id.clone(),
                        a.node_kind.clone(),
                        a.start_byte as i64,
                        a.end_byte as i64,
                        a.start_row as i64,
                        a.start_col as i64,
                        a.end_row as i64,
                        a.end_col as i64,
                        a.node_hash.to_vec(),
                    );
                }

                // ADR-0026 dual-write (Phase 1, bead `ley-line-open-3e87ad`).
                // One `capnp_blobs` row per file (the per-file AstNodeList
                // canonical bytes, content-addressed by BLAKE3); one
                // `_ast_pointer` row per AstEntry, mirroring the `_ast` row
                // set. `offset_in_blob` is the list index — the same
                // enumerate() order `serialize_ast_node_list_record` used, so
                // decoding the blob at `offset` byte-identically reproduces
                // this entry's fields (asserted by the F1 integration test).
                blob_buf.push(pf.pointer_blob_hash.to_vec(), pf.pointer_blob_bytes.clone());
                for (offset, a) in pf.ast_entries.iter().enumerate() {
                    pointer_buf.push(
                        a.node_id.clone(),
                        pf.pointer_blob_hash.to_vec(),
                        offset as i64,
                        semantic_kind_tag(&a.node_kind),
                        a.source_id.clone(),
                    );
                }

                for r in pf.refs {
                    match r {
                        ExtractedRef::Ref {
                            token,
                            node_id,
                            source_id,
                            container_node_id,
                            qualifier,
                        } => {
                            let nh = hash_by_id.get(node_id.as_str()).map(|h| h.to_vec());
                            refs_buf.push(
                                token,
                                node_id,
                                source_id,
                                nh,
                                container_node_id,
                                qualifier,
                            );
                        }
                        ExtractedRef::Def {
                            token,
                            node_id,
                            source_id,
                            container_node_id,
                            canonical_kind,
                        } => {
                            let nh = hash_by_id.get(node_id.as_str()).map(|h| h.to_vec());
                            defs_buf.push(
                                token,
                                node_id,
                                source_id,
                                nh,
                                container_node_id,
                                canonical_kind,
                            );
                        }
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

    // Insert-phase sub-timing (gated on LEYLINE_PROFILE=1) — measures
    // where the insert budget goes: main-thread row-buffer loop (which
    // includes the capnp BufWriter writes at :779-786), bulk INSERT
    // flushes, capnp-flush-before-COMMIT, COMMIT itself, and the
    // post-load index build. Immune to background I/O contention
    // because it's measuring wall-time deltas in-process, not sampling.
    let profile_insert = std::env::var("LEYLINE_PROFILE").ok().as_deref() == Some("1");
    let sub_buffer_end = std::time::Instant::now();

    // Flush each table in BULK_BATCH_ROWS-sized chunks via multi-row
    // VALUES inserts. Tail (last <BULK_BATCH_ROWS rows) flushed in one
    // partial-size statement so we don't fall back to per-row execute.
    //
    // FK ordering (ADR-0027): with `foreign_keys = ON` the node_hash FKs
    // (_ast/node_defs/node_refs → node_content, node_child → node_content)
    // are checked immediately per row. `node_content` must therefore land
    // FIRST so every referencing row finds its content target (uncommitted
    // rows in the same transaction satisfy the FK). The post-order fold
    // already emitted content children-before-parents, but the cross-table
    // ordering here is what makes the referencing rows safe.
    content_buf.flush_batched(conn)?;
    child_buf.flush_batched(conn)?;
    nodes_buf.flush_batched(conn)?;
    ast_buf.flush_batched(conn)?;
    // ADR-0028 source-blob dual-store (bead `ley-line-open-9e4416`). Flush
    // source_blobs BEFORE `_source` so the FK-shaped pointer (`_source.
    // content_hash → source_blobs.blob_hash`) always finds its referent —
    // matches the capnp_blobs→_ast_pointer ordering below. Phase 1 doesn't
    // declare the FK (dual-store is additive; the FK becomes load-bearing
    // when Phase 2 flips consumers to reads), but ordering the writes
    // correctly means the promotion is a one-line edit.
    source_blob_buf.flush_batched(conn)?;
    source_buf.flush_batched(conn)?;
    refs_buf.flush_batched_for(
        conn,
        "INSERT INTO node_refs (token, node_id, source_id, node_hash, container_node_id, qualifier) VALUES ",
    )?;
    defs_buf.flush_batched_for(
        conn,
        "INSERT INTO node_defs (token, node_id, source_id, node_hash, container_node_id, canonical_kind) VALUES ",
    )?;
    imports_buf.flush_batched(conn)?;
    file_idx_buf.flush_batched(conn)?;
    // ADR-0026 pointer-store dual-write (bead `ley-line-open-3e87ad`). Flush
    // blobs BEFORE the pointer rows so an implementation that later adds a FK
    // on `_ast_pointer.blob_hash → capnp_blobs.blob_hash` finds every referent
    // present. Phase 1 doesn't declare the FK (dual-write is additive; the
    // FK becomes load-bearing when Phase 2 flips consumers to reads), but
    // ordering the writes correctly means the promotion is a one-line edit.
    blob_buf.flush_batched(conn)?;
    pointer_buf.flush_batched(conn)?;
    let sub_flush_end = std::time::Instant::now();

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
    let sub_capnp_flush_end = std::time::Instant::now();

    conn.execute_batch("COMMIT")?;
    let sub_commit_end = std::time::Instant::now();

    // Merkle-AST IR (ADR-0027): count the UNRESOLVED reference targets —
    // node_refs rows whose token matches no node_defs token (a builtin, an
    // external dependency, or a not-yet-parsed file). This is the exact
    // parity image of the old `fact_edges WHERE dst IS NULL AND kind IN
    // ('references','calls')` count: every ref locator is an `_ast` node so
    // it always resolved to a `src`, and the producer never emitted `calls`,
    // so the old count reduced to "references with no matching def token".
    // Recorded in the Head as a binding-fidelity ratchet (W5 asserts it
    // stays <= baseline). Queried on the main thread here — after COMMIT but
    // before the head thread is spawned and before `create_post_load_indexes`
    // runs — so no second connection contends for the db lock. Whole-db
    // count, so it stays correct if a later run reparses only part of the tree.
    let unbound_facts: u64 = conn
        .query_row(
            "SELECT count(*) FROM node_refs WHERE token NOT IN (SELECT token FROM node_defs)",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n as u64)
        .unwrap_or(0);

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
        std::thread::spawn(move || -> Result<()> { write_head_for_path(&p, unbound_facts) })
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
    // Unified code-fact IR (ADR-0027): the `symbols`/`fact_edges` traversal
    // indexes (idx_symbols_node/kind, idx_edges_src/dst) are deferred to here
    // — same rationale as the other post-load indexes: one sorted scan per
    // index is cheaper than incremental B-tree maintenance during the insert.
    // The UNIQUE symbol_id index the FK targets was built earlier (pre-insert)
    // by create_ir_tables and is not rebuilt here.
    create_ir_indexes(conn)?;
    let sub_index_end = std::time::Instant::now();

    let insert_elapsed = insert_start.elapsed();

    if profile_insert {
        let buf_ms = sub_buffer_end.duration_since(insert_start).as_millis();
        let flush_ms = sub_flush_end.duration_since(sub_buffer_end).as_millis();
        let capnp_ms = sub_capnp_flush_end
            .duration_since(sub_flush_end)
            .as_millis();
        let commit_ms = sub_commit_end
            .duration_since(sub_capnp_flush_end)
            .as_millis();
        let index_ms = sub_index_end.duration_since(sub_commit_end).as_millis();
        eprintln!(
            "  insert-detail: buffer+capnp_write={buf_ms}ms \
             sql_flush={flush_ms}ms capnp_flush={capnp_ms}ms \
             commit={commit_ms}ms index_build={index_ms}ms"
        );
    }

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
    // Merkle-AST IR generation lineage (ADR-0027): the head.capnp root shape
    // changed (span left symbol identity), so this is a new schema
    // generation. Bump the meta marker so consumers can tell the merkle-AST
    // shape (node_content/node_child/_ast.node_hash) from the retired
    // symbols/fact_edges shape.
    set_meta(conn, "ir_schema_version", "merkle-ast-v1")?;
    // Stamp the extraction epoch only on full-tree passes. A scoped
    // pass reparses just the dirty set; stamping the binary's epoch
    // there would mark still-stale out-of-scope facts as current. The
    // warm-start initial reparse and every cold parse are full-tree,
    // so adoption of an arena by a new binary always lands here.
    if scope.is_none() {
        set_meta(conn, "extraction_epoch", &extraction_epoch)?;
        // Composite injection epoch — same full-tree-only rationale as
        // extraction_epoch above (bead `ley-line-open-c822a6`).
        set_meta(conn, "injection_epoch", &injection_epoch)?;
        // Query-set epoch + active-override provenance (bead
        // `ley-line-open-e72629`). `query_source:<lang>` rows make the ACTIVE
        // query-set source observable (`arena:<hex>`); absence of a row means
        // the compiled default. Stale rows from a removed override are cleared
        // first so provenance never over-reports.
        set_meta(conn, "query_set_epoch", &query_set_epoch)?;
        conn.execute("DELETE FROM _meta WHERE key LIKE 'query_source:%'", [])?;
        for (lang, hex) in query_set.active() {
            set_meta(
                conn,
                &format!("query_source:{}", lang.name()),
                &format!("arena:{hex}"),
            )?;
        }
    }
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
///
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
        let seg_count_minus_1 = u32::from_le_bytes([
            file_bytes[i],
            file_bytes[i + 1],
            file_bytes[i + 2],
            file_bytes[i + 3],
        ]);
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
        let msg = capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
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
fn read_head_for_chain(head_path: &Path) -> Result<([u8; 32], u64)> {
    use leyline_schema_capnp::head_capnp::head;

    let bytes = match std::fs::read(head_path) {
        Ok(b) => b,
        Err(_) => return Ok(([0u8; 32], 1)),
    };
    let mut slice: &[u8] = &bytes;
    let msg = match capnp::serialize::read_message(&mut slice, capnp::message::ReaderOptions::new())
    {
        Ok(m) => m,
        Err(_) => return Ok(([0u8; 32], 1)),
    };
    let h: head::Reader = match msg.get_root() {
        Ok(h) => h,
        Err(_) => return Ok(([0u8; 32], 1)),
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

    // S2: verify before adopting this head as chain state. Only runs when the
    // operator has configured a trust set — with no trusted keys there is
    // nothing to verify against, and existing unsigned arenas must keep
    // working.
    let trusted = head_trusted_keys_from_env()?;
    if !trusted.is_empty() {
        let parent = h
            .get_parent_hash()
            .ok()
            .and_then(|p| p.get_bytes().ok())
            .and_then(|b| <[u8; 32]>::try_from(b).ok())
            .unwrap_or([0u8; 32]);
        let signature = h.get_signature().unwrap_or(&[]);
        let verdict = leyline_sign::root_signer::verify_head(
            prev_gen,
            leyline_core::Hash::from_bytes(prev_root),
            leyline_core::Hash::from_bytes(parent),
            signature,
            &trusted,
        );
        match verdict {
            leyline_sign::root_signer::HeadVerdict::Valid => {}
            // Never acceptable: the head was edited after signing, corrupted,
            // or signed by a key this reader does not trust.
            leyline_sign::root_signer::HeadVerdict::Invalid => anyhow::bail!(
                "head at {} failed signature verification — refusing to chain onto it",
                head_path.display()
            ),
            leyline_sign::root_signer::HeadVerdict::Unsigned => {
                if std::env::var("LEYLINE_HEAD_REQUIRE_SIGNATURE")
                    .is_ok_and(|v| matches!(v.trim(), "1" | "true"))
                {
                    anyhow::bail!(
                        "head at {} is unsigned and LEYLINE_HEAD_REQUIRE_SIGNATURE is set",
                        head_path.display()
                    );
                }
            }
        }
    }

    Ok((prev_root, prev_gen.saturating_add(1)))
}

/// S2: the trusted head-verification keys — a comma-separated list of
/// hex-encoded 32-byte Ed25519 public keys in `LEYLINE_HEAD_TRUSTED_KEYS`.
///
/// A list (not a single key) because rotation needs an overlap window where
/// both the outgoing and incoming key verify. A malformed entry is a hard
/// error for the same reason a malformed signing key is: a trust set that
/// silently parsed to empty would disable verification while looking enabled.
fn head_trusted_keys_from_env() -> Result<Vec<leyline_sign::root_signer::VerifyingKey>> {
    let Ok(raw) = std::env::var("LEYLINE_HEAD_TRUSTED_KEYS") else {
        return Ok(Vec::new());
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            leyline_sign::root_signer::verifying_key_from_hex(s)
                .context("LEYLINE_HEAD_TRUSTED_KEYS entry is not a valid Ed25519 public key")
        })
        .collect()
}

/// S1: the optional head signing key — a hex-encoded 32-byte Ed25519 seed in
/// `LEYLINE_HEAD_SIGNING_KEY`. Absent or empty ⇒ heads are written unsigned,
/// byte-identical to pre-S1 behavior.
///
/// A *malformed* value is a hard error rather than a silent fall back to
/// unsigned: a misconfigured signer must never be indistinguishable from
/// "signing is switched off", or you get unsigned heads believing they're
/// signed.
fn head_signer_from_env() -> Result<Option<leyline_sign::root_signer::Ed25519RootSigner>> {
    let Ok(raw) = std::env::var("LEYLINE_HEAD_SIGNING_KEY") else {
        return Ok(None);
    };
    let hex = raw.trim();
    if hex.is_empty() {
        return Ok(None);
    }
    if !hex.is_ascii() || hex.len() != 64 {
        anyhow::bail!(
            "LEYLINE_HEAD_SIGNING_KEY must be 64 hex chars (32-byte Ed25519 seed), got {} chars",
            hex.len()
        );
    }
    let mut seed = [0u8; 32];
    for (i, b) in seed.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .context("LEYLINE_HEAD_SIGNING_KEY is not valid hex")?;
    }
    Ok(Some(
        leyline_sign::root_signer::Ed25519RootSigner::from_seed(&seed),
    ))
}

/// T8.5: compute the segment hash for this run (from `db_path`'s
/// sibling ast/source segment files), read the existing Head for the
/// parent/gen chain, and write the new Head. Pure filesystem work —
/// no SQLite handle required, so a parent caller can dispatch this on
/// a worker thread that runs concurrently with post-COMMIT SQLite work
/// (e.g. `create_post_load_indexes`). See bead `ley-line-open-cbbedf`.
fn write_head_for_path(db_path: &Path, unbound_facts: u64) -> Result<()> {
    let (root, segment_bytes) = hash_segment_files(db_path)?;
    let head_path = with_extension(db_path, "head.capnp");
    let (parent, generation) = read_head_for_chain(&head_path)?;

    use leyline_schema_capnp::head_capnp::head;
    let mut src = capnp::message::Builder::new_default();
    {
        let mut h: head::Builder = src.init_root();
        h.set_generation(generation);
        h.set_segment_bytes(segment_bytes);
        // Unified code-fact IR ratchet (ADR-0027): NULL-dst reference/call
        // edge count, threaded from the caller's post-COMMIT db query.
        h.set_unbound_facts(unbound_facts);
        h.reborrow().init_root_hash().set_bytes(&root);
        h.reborrow().init_parent_hash().set_bytes(&parent);

        // S1: sign the head when a signing key is configured. The signature
        // covers the canonical head digest — BLAKE3(generation ‖ root ‖
        // parent) — not rootHash alone, so it cannot be replayed at another
        // generation or grafted onto a forked chain. No key ⇒ the head is
        // written unsigned, byte-identical to before.
        if let Some(signer) = head_signer_from_env()? {
            use leyline_core::RootSigner;
            let digest = leyline_core::head_digest(
                generation,
                leyline_core::Hash::from_bytes(root),
                leyline_core::Hash::from_bytes(parent),
            );
            let sig = signer.sign(digest).context("sign head digest")?;
            let pk = signer.verifying_key();
            // kid = BLAKE3(pubkey)[..8] — lets a verifier pick the key
            // without trial verification. BLAKE3 per the Σ hash lock.
            let pk_hash = blake3::hash(pk.as_bytes());
            let sig_bytes = sig.to_bytes();
            h.set_signature(&sig_bytes);
            h.set_signer_kid(&pk_hash.as_bytes()[..8]);
        }
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&head_path)
        .with_context(|| format!("open head {}", head_path.display()))?;
    leyline_schema_capnp::canonical::write_canonical_message::<head::Owned, _>(&src, &mut f)
        .context("write Head capnp record")?;
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
    // Mirror parse_into_conn: derive the IR unbound-fact count from the db —
    // node_refs whose token matches no node_defs token (ADR-0027).
    let unbound_facts: u64 = conn
        .query_row(
            "SELECT count(*) FROM node_refs WHERE token NOT IN (SELECT token FROM node_defs)",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n as u64)
        .unwrap_or(0);
    write_head_for_path(&db_path, unbound_facts)
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
    content_hash: &[u8; 32],
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
        // BLAKE3-32 of the file bytes (T8.5 wired in ADR-0027). Feeds the
        // Σ segment hash and — projected into `_source.contentHash` — the
        // `symbol_id` content address consumers join on.
        sf.init_content_hash().set_bytes(content_hash);
    }

    leyline_schema_capnp::canonical::write_canonical_message::<source_file::Owned, _>(&src, buf)
        .context("write SourceFile capnp record")?;
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

    leyline_schema_capnp::canonical::write_canonical_message::<ast_node::Owned, _>(&src, buf)
        .context("write AstNode capnp record")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// ADR-0026 pointer store — Phase 1 dual-write (bead `ley-line-open-3e87ad`)
// ---------------------------------------------------------------------------
//
// Producer helpers for the content-addressed pointer store:
//
// - `serialize_ast_node_list_record` — canonicalize a per-file
//   `AstNodeList` capnp message. Blob content per ADR-0026 §2.1.
// - `semantic_kind_tag` — Phase 1 tag encoding for `_ast_pointer.kind`.
//   The ADR (§2.1) calls out "function, method, type, import" as the
//   semantic surface; Phase 1 encodes the common tree-sitter kinds behind
//   those categories so consumers can pre-filter without decoding the
//   blob. The allowlist is intentionally small — Phase 2 refines against
//   measured mache query patterns (ADR-0026 §2.2).

/// Semantic-kind tag for `_ast_pointer.kind`. Load-bearing surface is
/// small and deliberately conservative — Phase 1's contract is "if you
/// want to filter by semantic surface without decoding the blob, these
/// codes are stable across parse runs." Unknown / non-semantic kinds
/// (identifiers, literals, statements, blocks, punctuation) collapse to
/// `SEMANTIC_KIND_OTHER` and consumers fall back to blob decode.
///
/// Encoding kept as small integer constants (not an enum) so the wire
/// story is dead-simple: `_ast_pointer.kind` is an `INTEGER`, values are
/// documented here, Phase 2 extends by adding new tags at the tail.
pub const SEMANTIC_KIND_OTHER: i64 = 0;
/// Function-like: `function_declaration`, `function_definition`,
/// `function_item`, `method_declaration`.
pub const SEMANTIC_KIND_FUNCTION: i64 = 1;
/// Method-like: `method_definition`, `method_signature_item`.
pub const SEMANTIC_KIND_METHOD: i64 = 2;
/// Type-like: struct/class/interface/type-alias/enum declarations.
pub const SEMANTIC_KIND_TYPE: i64 = 3;
/// Import-like: `import_declaration`, `import_statement`,
/// `use_declaration`, `import_spec`.
pub const SEMANTIC_KIND_IMPORT: i64 = 4;

/// Map a tree-sitter node kind string to its semantic-kind tag. Phase 1
/// covers the categories the ADR calls out (§2.1); Phase 2 refines the
/// allowlist per measured mache query patterns (§2.2).
fn semantic_kind_tag(node_kind: &str) -> i64 {
    match node_kind {
        // Function-like across Go / Rust / Python / JS / TS.
        "function_declaration" | "function_definition" | "function_item" => SEMANTIC_KIND_FUNCTION,
        // Method-like.
        "method_declaration" | "method_definition" | "method_signature_item" => {
            SEMANTIC_KIND_METHOD
        }
        // Type-like: struct / class / interface / enum / type alias.
        "struct_item"
        | "type_declaration"
        | "type_alias"
        | "type_alias_declaration"
        | "type_alias_statement"
        | "class_declaration"
        | "class_definition"
        | "interface_declaration"
        | "enum_declaration"
        | "enum_item"
        | "trait_item"
        | "impl_item" => SEMANTIC_KIND_TYPE,
        // Import-like.
        "import_declaration"
        | "import_statement"
        | "import_spec"
        | "import_from_statement"
        | "use_declaration" => SEMANTIC_KIND_IMPORT,
        _ => SEMANTIC_KIND_OTHER,
    }
}

/// ADR-0026 §2.1 — serialize the per-file `AstNodeList` capnp message
/// (canonical form) into `buf`. Emits ONE root message per file whose
/// `nodes` list contains an `AstNode` for every `AstEntry`, in the same
/// order the entries were folded. The offset of each node in the list is
/// its index in `entries`, which populates `_ast_pointer.offset_in_blob`.
///
/// The canonical bytes are hashed with BLAKE3 to produce `blob_hash`. The
/// bytes then land verbatim in `capnp_blobs.blob_bytes` (no re-
/// canonicalization at read time — the on-disk bytes ARE the blob).
fn serialize_ast_node_list_record(buf: &mut Vec<u8>, entries: &[AstEntry]) -> Result<()> {
    use leyline_schema_capnp::ast_capnp::ast_node_list;

    let mut src = capnp::message::Builder::new_default();
    {
        let list_root: ast_node_list::Builder = src.init_root();
        let n = u32::try_from(entries.len())
            .context("AstNodeList: entries count exceeds u32 (capnp list bound)")?;
        let mut list = list_root.init_nodes(n);
        for (i, a) in entries.iter().enumerate() {
            let mut node = list.reborrow().get(i as u32);
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
    }

    leyline_schema_capnp::canonical::write_canonical_message::<ast_node_list::Owned, _>(&src, buf)
        .context("write AstNodeList capnp record")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Merkle-AST content address (ADR-0027) — node_hash
// ---------------------------------------------------------------------------
//
// A bottom-up (POST-ORDER) fold: a node's `node_hash` is a function of its
// canonical κ kind, its terminal token text (leaves), and the ordered
// hashes of its non-`extra` children (internal nodes). Spans, paths, and
// parse-run node_ids are OUT of the preimage, so a unique subtree hashes to
// one value regardless of where it appears — two byte-identical functions
// in different files share a `node_hash`. Anonymous children (operators
// like `+`/`-`, keywords, punctuation) ARE folded, which is what
// distinguishes `a+b` from `a-b`; comments/`extra` nodes are excluded, so a
// comment-only edit leaves every enclosing hash unchanged.

/// Domain/version tag for the merkle-AST preimage (git-object style).
const NODE_HASH_DOMAIN: &[u8] = b"llo/ast/v1";
/// Node-tag byte: leaf (terminal) node.
const NODE_TAG_LEAF: u8 = 0x00;
/// Node-tag byte: internal (n-ary) node.
const NODE_TAG_INTERNAL: u8 = 0x01;

/// Append `v` as an unsigned LEB128 varint. Length-prefixing (NOT a 0x00
/// delimiter) is what keeps token text unambiguous once string/char
/// literals — which can contain NUL — enter σ.
fn write_uvarint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if v == 0 {
            break;
        }
    }
}

/// Merkle-AST hash of a terminal node: `domain ‖ 0x00 ‖ leaf_tag ‖
/// uvarint(len(kind)) ‖ kind ‖ uvarint(len(token)) ‖ token`. Hashed via the
/// locked σ surface (`ContentAddressed::hash`), never `blake3::hash`
/// directly (the `lint:blake3` gate).
fn hash_leaf(kind: &str, token: &str) -> [u8; 32] {
    let mut p = Vec::with_capacity(NODE_HASH_DOMAIN.len() + 6 + kind.len() + token.len());
    p.extend_from_slice(NODE_HASH_DOMAIN);
    p.push(0x00);
    p.push(NODE_TAG_LEAF);
    write_uvarint(&mut p, kind.len() as u64);
    p.extend_from_slice(kind.as_bytes());
    write_uvarint(&mut p, token.len() as u64);
    p.extend_from_slice(token.as_bytes());
    *p.hash().as_bytes()
}

/// Merkle-AST hash of an internal node: `domain ‖ 0x00 ‖ internal_tag ‖
/// uvarint(len(kind)) ‖ kind ‖ uvarint(child_count) ‖ child_hash[0..n]`
/// (32 bytes each, SOURCE ORDER). Same σ surface as [`hash_leaf`].
fn hash_internal(kind: &str, child_hashes: &[[u8; 32]]) -> [u8; 32] {
    let mut p =
        Vec::with_capacity(NODE_HASH_DOMAIN.len() + 6 + kind.len() + child_hashes.len() * 32);
    p.extend_from_slice(NODE_HASH_DOMAIN);
    p.push(0x00);
    p.push(NODE_TAG_INTERNAL);
    write_uvarint(&mut p, kind.len() as u64);
    p.extend_from_slice(kind.as_bytes());
    write_uvarint(&mut p, child_hashes.len() as u64);
    for h in child_hashes {
        p.extend_from_slice(h);
    }
    *p.hash().as_bytes()
}

// ---------------------------------------------------------------------------
// Pure file parser (no Connection — safe for rayon)
// ---------------------------------------------------------------------------

/// Parse a single file into a `ParsedFile`. No database access.
pub(crate) fn parse_file_pure(
    content: &[u8],
    language: TsLanguage,
    source_id: &str,
    abs_path: &str,
    file_mtime: i64,
    file_size: i64,
    queries: &QuerySet,
) -> Result<ParsedFile> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&language.ts_language())
        .context("failed to set tree-sitter language")?;

    let tree = parser
        .parse(content, None)
        .context("tree-sitter parse returned None")?;

    // BLAKE3 of the file bytes — the byte-level content address feeding
    // `_source.contentHash` (retained, e251083). Complementary to the
    // merkle-AST node_hash (which is structure-level, whitespace-invariant).
    // σ via the one content-address surface (ContentAddressed), not inline
    // blake3 — byte-identical (substrate.rs locks the algorithm) and keeps
    // _source.contentHash on the same σ path as the rest of the Σ substrate.
    // Enforced by the `lint:blake3` gate.
    let content_hash: [u8; 32] = *content.hash().as_bytes();

    let root = tree.root_node();
    let lang_name = language.name();

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
    let mut node_contents: Vec<ContentRow> = Vec::new();
    let mut node_children: Vec<ChildRow> = Vec::new();
    // Per-file dedup: emit a subtree's content/child rows only on first
    // sight. Cross-file dedup is handled by `INSERT OR IGNORE` at flush time.
    let mut seen: HashSet<[u8; 32]> = HashSet::new();
    // Injections (bead `ley-line-open-c822a6`): (node_id → node_hash)
    // for injected-subtree nodes, which have no `_ast` rows. The
    // env-var off-switch is the falsification seam for the host-hash-
    // independence gate, read once per file (rayon workers only read).
    let mut injected_hashes: Vec<(String, [u8; 32])> = Vec::new();
    let injections_on = !leyline_ts::injections::injections_disabled();
    // Arena query overrides (bead `ley-line-open-e72629`): per-file bounds flag
    // set when an override engine trips its resource ceiling on any node.
    let bounds_tripped = std::cell::Cell::new(false);
    let ctx = ExtractCtx {
        queries,
        bounds: &bounds_tripped,
    };

    // File node.
    nodes.push(ParsedNode {
        id: source_id.to_string(),
        parent_id: parent_id.clone(),
        name: file_name,
        kind: 1,
        size: 0,
        record: String::new(),
    });

    // Root AST entry — pushed pre-order with a placeholder node_hash that is
    // patched once the fold returns the root's content address.
    let root_kind = root.kind();
    let root_idx = ast_entries.len();
    ast_entries.push(AstEntry {
        node_id: source_id.to_string(),
        source_id: source_id.to_string(),
        node_kind: root_kind.to_string(),
        start_byte: root.start_byte(),
        end_byte: root.end_byte(),
        start_row: root.start_position().row,
        start_col: root.start_position().column,
        end_row: root.end_position().row,
        end_col: root.end_position().column,
        node_hash: [0u8; 32],
    });

    // Fold the whole tree bottom-up. Creates the `_ast`/`nodes`/refs rows
    // for every NAMED descendant (parent-creates-child, pre-order) and the
    // deduped `node_content`/`node_child` rows for every unique subtree.
    let root_hash = fold_children(
        content,
        root,
        source_id,
        source_id,
        language,
        lang_name,
        // Bead ley-line-open-6e798d: root has no enclosing function/method.
        None,
        injections_on,
        &mut seen,
        &mut nodes,
        &mut ast_entries,
        &mut refs,
        &mut node_contents,
        &mut node_children,
        &mut injected_hashes,
        &ctx,
    );
    ast_entries[root_idx].node_hash = root_hash;

    // Arena query overrides (bead `ley-line-open-e72629`): a pathological
    // override tripped its resource ceiling somewhere in this file. Drop the
    // file's extracted facts — "no facts for this file" — with exactly one
    // stderr line; structural rows stay valid. Never a partial fact set, never
    // a hung parse.
    if bounds_tripped.get() {
        refs.clear();
        eprintln!(
            "warn: query override for {} exceeded its resource bounds on {source_id}; \
             dropped extracted facts for this file",
            language.name()
        );
    }

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
        &content_hash,
    )?;
    // ~150 bytes per AstNode record (canonical: id + source_id + kind +
    // Range); pre-size to avoid re-allocs during the per-node loop.
    let mut ast_capnp_bytes = Vec::with_capacity(ast_entries.len() * 160);
    for a in &ast_entries {
        serialize_ast_node_record(&mut ast_capnp_bytes, a)?;
    }

    // ADR-0026 pointer store (Phase 1 dual-write, bead `ley-line-open-3e87ad`):
    // build the per-file `AstNodeList` canonical capnp blob AND its BLAKE3
    // address in the rayon worker. The main-thread insert loop just moves the
    // byte buffer + hash into the DB, doing no capnp work.
    //
    // Blob size ~= sizeof(ast_capnp_bytes); a single canonical `AstNodeList`
    // message is denser than N concatenated `AstNode` messages (one framing
    // header, one segment table), so this over-allocates safely.
    let mut pointer_blob_bytes = Vec::with_capacity(ast_capnp_bytes.len());
    serialize_ast_node_list_record(&mut pointer_blob_bytes, &ast_entries)?;
    let pointer_blob_hash: [u8; 32] = *pointer_blob_bytes.as_slice().hash().as_bytes();

    // ADR-0028 source-blob bytes (bead `ley-line-open-9e4416`, Phase 1 dual-
    // store). Owning clone of the input source bytes so the main-thread insert
    // loop can move them into `source_blobs.blob_bytes` without re-reading from
    // disk. `content_hash` above is already BLAKE3 of this exact slice, so
    // (blob_hash, blob_bytes) is content-consistent by construction (F1s pins
    // this in-DB; F-git pins hash-compatibility with `git cat-file blob`).
    let source_blob_bytes = content.to_vec();

    Ok(ParsedFile {
        rel: source_id.to_string(),
        abs_path: abs_path.to_string(),
        language: language.name().to_string(),
        nodes,
        ast_entries,
        refs,
        node_contents,
        node_children,
        file_mtime,
        file_size,
        content_hash,
        source_capnp_bytes,
        ast_capnp_bytes,
        pointer_blob_bytes,
        pointer_blob_hash,
        source_blob_bytes,
        injected_hashes,
    })
}

/// Parse `content` under `language` and serialize the resulting AST +
/// extracted refs as JSON matching the shipped `_ast` / `node_defs` /
/// `node_refs` / `_imports` schema. Bead `ley-line-open-851f24`
/// follow-up: powers the daemon's `{"emit_ast": true}` extension on
/// the `validate` op, so mache's writeback linter can fold ONE parse
/// into both syntax validation AND SQL-shaped AST rows — killing the
/// interim `go/parser` and unblocking CGO removal on the mache side.
///
/// Response shape:
///
/// ```json
/// {
///   "source_id": "<source_id>",
///   "language": "<name>",
///   "content_hash": "<hex-32>",
///   "ast": [
///     {"node_id": "...", "source_id": "...", "node_kind": "...",
///      "start_byte": N, "end_byte": N, "start_row": N, "start_col": N,
///      "end_row": N, "end_col": N, "node_hash": "<hex-32>"}
///   ],
///   "defs": [
///     {"token": "...", "node_id": "...", "source_id": "...",
///      "container_node_id": "..."|null, "canonical_kind": "..."|null}
///   ],
///   "refs": [
///     {"token": "...", "node_id": "...", "source_id": "...",
///      "container_node_id": "..."|null, "qualifier": "..."|null}
///   ],
///   "imports": [{"alias": "...", "path": "...", "source_id": "..."}]
/// }
/// ```
///
/// The `source_id` on every row is exactly the `source_id` argument
/// passed in — caller controls the identity (typically the file's
/// path-relative-to-repo or a stable synthetic id). Content hash is
/// BLAKE3-32 of `content`; node hashes are the merkle-AST addresses
/// from ADR-0027 (byte-identical to what `parse_into_conn` produces
/// so a folded row inserts cleanly into an existing `_ast` snapshot).
pub(crate) fn parse_to_ast_json(
    content: &[u8],
    language: leyline_ts::languages::TsLanguage,
    source_id: &str,
) -> Result<serde_json::Value> {
    // file_mtime + file_size default to 0 for in-memory buffers —
    // they're metadata for the file-index row, which callers of
    // parse_to_ast_json don't populate (no `_file_index` in the
    // JSON response shape).
    // No arena connection here (in-memory validate path), so no overrides can
    // apply — the compiled defaults are the effective query set.
    let parsed = parse_file_pure(
        content,
        language,
        source_id,
        source_id,
        0,
        content.len() as i64,
        &QuerySet::compiled(),
    )?;

    let ast: Vec<serde_json::Value> = parsed
        .ast_entries
        .iter()
        .map(|a| {
            serde_json::json!({
                "node_id": a.node_id,
                "source_id": a.source_id,
                "node_kind": a.node_kind,
                "start_byte": a.start_byte,
                "end_byte": a.end_byte,
                "start_row": a.start_row,
                "start_col": a.start_col,
                "end_row": a.end_row,
                "end_col": a.end_col,
                "node_hash": hex::encode(a.node_hash),
            })
        })
        .collect();

    let mut defs = Vec::new();
    let mut refs = Vec::new();
    let mut imports = Vec::new();
    for r in &parsed.refs {
        match r {
            leyline_ts::refs::ExtractedRef::Def {
                token,
                node_id,
                source_id,
                container_node_id,
                canonical_kind,
            } => defs.push(serde_json::json!({
                "token": token,
                "node_id": node_id,
                "source_id": source_id,
                "container_node_id": container_node_id,
                "canonical_kind": canonical_kind,
            })),
            leyline_ts::refs::ExtractedRef::Ref {
                token,
                node_id,
                source_id,
                container_node_id,
                qualifier,
            } => refs.push(serde_json::json!({
                "token": token,
                "node_id": node_id,
                "source_id": source_id,
                "container_node_id": container_node_id,
                "qualifier": qualifier,
            })),
            leyline_ts::refs::ExtractedRef::Import {
                alias,
                path,
                source_id,
            } => imports.push(serde_json::json!({
                "alias": alias,
                "path": path,
                "source_id": source_id,
            })),
        }
    }

    Ok(serde_json::json!({
        "source_id": source_id,
        "language": parsed.language,
        "content_hash": hex::encode(parsed.content_hash),
        "ast": ast,
        "defs": defs,
        "refs": refs,
        "imports": imports,
    }))
}

/// Bottom-up (post-order) fold of `node`'s subtree.
///
/// Returns `node`'s merkle-AST `node_hash`. As a side effect it:
/// - creates an `_ast` occurrence row, a `nodes` row, and any extracted
///   refs for every NAMED child (pre-order, so `_ast`/`nodes` insertion
///   stays in sorted node_id order — the B-tree bulk-load fast path);
/// - stamps each named child's `node_hash` onto its `_ast` row after the
///   child's subtree is folded;
/// - emits one deduped `node_content` row per unique subtree and the
///   `node_child` edges of every unique internal node.
///
/// ALL non-`extra` children (named AND anonymous) are folded into the
/// parent's hash in source order. Anonymous children are terminal tokens
/// (operators/keywords/punctuation) with no children, so they never
/// produce `_ast`/`nodes` rows — they only contribute their leaf hash.
#[allow(clippy::too_many_arguments)]
fn fold_children(
    content: &[u8],
    node: tree_sitter::Node,
    node_id: &str,
    source_id: &str,
    language: TsLanguage,
    lang_name: &str,
    // Bead `ley-line-open-6e798d`: node_id of the nearest enclosing κ
    // `function`/`method` ancestor. `None` at top level. When we descend
    // into a child whose κ canonical kind is `function`/`method`, we
    // pass its own `id` as the new container for its subtree.
    container_node_id: Option<&str>,
    // Injections (bead `ley-line-open-c822a6`): false only under the
    // `LLO_DISABLE_INJECTIONS=1` falsification seam.
    injections_on: bool,
    seen: &mut HashSet<[u8; 32]>,
    nodes: &mut Vec<ParsedNode>,
    ast_entries: &mut Vec<AstEntry>,
    refs: &mut Vec<ExtractedRef>,
    node_contents: &mut Vec<ContentRow>,
    node_children: &mut Vec<ChildRow>,
    injected_hashes: &mut Vec<(String, [u8; 32])>,
    ctx: &ExtractCtx,
) -> [u8; 32] {
    // Gather non-extra children in source order, with tree-sitter field
    // names, and count named children per kind (for the node_id suffix).
    let mut children: Vec<tree_sitter::Node> = Vec::new();
    let mut fields: Vec<Option<&'static str>> = Vec::new();
    let mut named_kind_counts = HashMap::<&str, usize>::new();
    {
        let mut cur = node.walk();
        if cur.goto_first_child() {
            loop {
                let ch = cur.node();
                if !ch.is_extra() {
                    if ch.is_named() {
                        *named_kind_counts.entry(ch.kind()).or_insert(0) += 1;
                    }
                    fields.push(cur.field_name());
                    children.push(ch);
                }
                if !cur.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    let mut child_hashes: Vec<[u8; 32]> = Vec::with_capacity(children.len());
    let mut kind_indices = HashMap::<&str, usize>::new();

    for child in &children {
        if child.is_named() {
            let kind = child.kind();
            let needs_suffix = named_kind_counts[kind] > 1;
            let name = if needs_suffix {
                let idx = kind_indices.entry(kind).or_insert(0);
                let n = format!("{kind}_{idx}");
                *idx += 1;
                n
            } else {
                kind.to_string()
            };
            let id = format!("{node_id}/{name}");

            // _ast occurrence row (pre-order; node_hash patched post-fold).
            let entry_idx = ast_entries.len();
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
                node_hash: [0u8; 32],
            });

            // Refs via the language-dispatched factory.
            // Bead ley-line-open-6e798d: pass the current container so
            // every ExtractedRef::{Def, Ref} the language extractor
            // emits carries the enclosing κ function/method's node_id.
            refs.extend(extract_refs_resolved(
                child,
                content,
                &id,
                source_id,
                language,
                container_node_id,
                ctx.queries,
                ctx.bounds,
            ));

            // Injections (bead `ley-line-open-c822a6`): probe this node
            // against the host language's injections.scm, anchored the
            // same way extract_refs is. A hit reparses the captured
            // byte range under the target grammar and folds the
            // injected subtree into fact + content rows rooted at
            // `{id}#inj{k}` — its OWN content-addressed space; nothing
            // it produces enters this fold's hashes or occurrence rows.
            // The container is the literal's enclosing function: a
            // string literal is never itself a κ container.
            if injections_on
                && let Some(engine) = leyline_ts::injections::injection_engine(language)
            {
                for (k, site) in engine.sites(child, content).into_iter().enumerate() {
                    fold_injected(
                        content,
                        &site,
                        &format!("{id}#inj{k}"),
                        source_id,
                        container_node_id,
                        seen,
                        refs,
                        node_contents,
                        node_children,
                        injected_hashes,
                        ctx,
                    );
                }
            }

            // Structural `nodes` row: kind 1 (dir-like) when the child has
            // named children, else kind 0 (leaf) carrying its text.
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
                    parent_id: node_id.to_string(),
                    name,
                    kind: 1,
                    size: 0,
                    record: String::new(),
                });
            } else {
                let text = child.utf8_text(content).unwrap_or("");
                nodes.push(ParsedNode {
                    id: id.clone(),
                    parent_id: node_id.to_string(),
                    name,
                    kind: 0,
                    size: text.len() as i64,
                    record: text.to_string(),
                });
            }

            // Fold the child's subtree (all non-extra grandchildren — even
            // when it has no NAMED children, its anonymous tokens still shape
            // its hash), then stamp the resulting address onto the occurrence.
            //
            // Bead ley-line-open-6e798d: if THIS child is itself a
            // function/method (per κ), its subtree's container becomes
            // its own id — every ref/def inside it will carry that
            // container. Otherwise the enclosing container passes through
            // unchanged.
            let child_container_owned;
            let child_container = match language.canonical_kind(child.kind()) {
                Some("function") | Some("method") => {
                    child_container_owned = id.clone();
                    Some(child_container_owned.as_str())
                }
                _ => container_node_id,
            };
            let child_hash = fold_children(
                content,
                *child,
                &id,
                source_id,
                language,
                lang_name,
                child_container,
                injections_on,
                seen,
                nodes,
                ast_entries,
                refs,
                node_contents,
                node_children,
                injected_hashes,
                ctx,
            );
            ast_entries[entry_idx].node_hash = child_hash;
            child_hashes.push(child_hash);
        } else {
            // Anonymous child: a terminal token. No _ast/nodes row — it only
            // contributes its leaf hash to this node's fold. `node_id` is
            // unused for anonymous nodes (terminals have no named children).
            // Container passes through — anonymous tokens can't introduce
            // a new function scope.
            let child_hash = fold_children(
                content,
                *child,
                "",
                source_id,
                language,
                lang_name,
                container_node_id,
                injections_on,
                seen,
                nodes,
                ast_entries,
                refs,
                node_contents,
                node_children,
                injected_hashes,
                ctx,
            );
            child_hashes.push(child_hash);
        }
    }

    // Compute this node's content address and emit its deduped content rows.
    let raw_kind = node.kind();
    let canonical = language.canonical_kind(raw_kind).unwrap_or(raw_kind);
    if children.is_empty() {
        // Leaf: hash the terminal token verbatim (length-prefixed, NUL-safe).
        let token = node.utf8_text(content).unwrap_or("");
        let h = hash_leaf(canonical, token);
        if seen.insert(h) {
            node_contents.push(ContentRow {
                node_hash: h,
                node_tag: NODE_TAG_LEAF,
                kind: canonical.to_string(),
                raw_kind: raw_kind.to_string(),
                lang: lang_name.to_string(),
                token: Some(token.to_string()),
                arity: 0,
            });
        }
        h
    } else {
        let h = hash_internal(canonical, &child_hashes);
        if seen.insert(h) {
            node_contents.push(ContentRow {
                node_hash: h,
                node_tag: NODE_TAG_INTERNAL,
                kind: canonical.to_string(),
                raw_kind: raw_kind.to_string(),
                lang: lang_name.to_string(),
                token: None,
                arity: child_hashes.len(),
            });
            for (ord, (ch, field)) in child_hashes.iter().zip(&fields).enumerate() {
                node_children.push(ChildRow {
                    parent_hash: h,
                    ordinal: ord,
                    child_hash: *ch,
                    field: field.map(|f| f.to_string()),
                });
            }
        }
        h
    }
}

// ---------------------------------------------------------------------------
// Injections (bead ley-line-open-c822a6)
// ---------------------------------------------------------------------------

/// Reparse one injection site under its target grammar and fold the
/// injected subtree into fact + content rows.
///
/// The injected subtree gets its OWN content-addressed root: its
/// hashes are computed by the same [`hash_leaf`]/[`hash_internal`]
/// fold — over the INJECTED grammar's kinds — and land as
/// `node_content`/`node_child` rows (`lang` = the injected language),
/// so a standalone file with the same statement bytes dedups to the
/// same rows. Nothing here touches the HOST fold's preimages,
/// `_ast`/`nodes` occurrence rows, or the pointer/capnp stores — host
/// structural identity is independent of the injected grammar's
/// version by construction (pinned by
/// `inj_host_node_hashes_independent_of_injection_pass`).
///
/// Node identity: the injected root is `root_id` =
/// `{host_node_id}#inj{k}` (built by the caller); descendants follow
/// the host fold's `{parent}/{kind}[_{idx}]` naming. `#` cannot occur
/// in host node_ids (path + grammar-kind derived), so the scheme
/// cannot collide. Facts carry the HOST file as `source_id` and the
/// host's enclosing function as the initial container.
///
/// Failure shape: an unloadable grammar, rejected range, or failed
/// parse degrades to zero facts — never an error for the host parse
/// (same contract as `extract_refs` on unsupported languages).
#[allow(clippy::too_many_arguments)]
fn fold_injected(
    content: &[u8],
    site: &leyline_ts::injections::InjectionSite,
    root_id: &str,
    source_id: &str,
    container_node_id: Option<&str>,
    seen: &mut HashSet<[u8; 32]>,
    refs: &mut Vec<ExtractedRef>,
    node_contents: &mut Vec<ContentRow>,
    node_children: &mut Vec<ChildRow>,
    injected_hashes: &mut Vec<(String, [u8; 32])>,
    ctx: &ExtractCtx,
) {
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&site.language.ts_language()).is_err() {
        return;
    }
    if parser.set_included_ranges(&[site.range]).is_err() {
        return;
    }
    let Some(tree) = parser.parse(content, None) else {
        return;
    };
    let root = tree.root_node();
    let root_hash = fold_injected_node(
        content,
        root,
        root_id,
        source_id,
        site.language,
        container_node_id,
        seen,
        refs,
        node_contents,
        node_children,
        injected_hashes,
        ctx,
    );
    injected_hashes.push((root_id.to_string(), root_hash));
}

/// Bottom-up fold of an INJECTED subtree — [`fold_children`] minus the
/// occurrence layer. Same traversal (non-`extra` children in source
/// order), same node_id naming, same κ container threading, same
/// [`hash_leaf`]/[`hash_internal`] content addressing with deduped
/// `node_content`/`node_child` emission; but no `nodes`/`_ast` rows —
/// injected nodes record their (node_id → node_hash) pairs in
/// `injected_hashes` instead, which is how their fact rows get the
/// `node_hash` pointer the `node_defs`/`node_refs` FK requires.
///
/// Kept as its own function rather than a mode flag on
/// [`fold_children`]: the shared behavior is pinned externally — the
/// node_id scheme by `inj_injected_node_id_scheme_pinned`, the hash
/// fold by `inj_own_ca_root_dedups_with_standalone_sql` (injected vs
/// standalone hash equality fails loudly if the folds drift).
#[allow(clippy::too_many_arguments)]
fn fold_injected_node(
    content: &[u8],
    node: tree_sitter::Node,
    node_id: &str,
    source_id: &str,
    language: TsLanguage,
    container_node_id: Option<&str>,
    seen: &mut HashSet<[u8; 32]>,
    refs: &mut Vec<ExtractedRef>,
    node_contents: &mut Vec<ContentRow>,
    node_children: &mut Vec<ChildRow>,
    injected_hashes: &mut Vec<(String, [u8; 32])>,
    ctx: &ExtractCtx,
) -> [u8; 32] {
    let mut children: Vec<tree_sitter::Node> = Vec::new();
    let mut fields: Vec<Option<&'static str>> = Vec::new();
    let mut named_kind_counts = HashMap::<&str, usize>::new();
    {
        let mut cur = node.walk();
        if cur.goto_first_child() {
            loop {
                let ch = cur.node();
                if !ch.is_extra() {
                    if ch.is_named() {
                        *named_kind_counts.entry(ch.kind()).or_insert(0) += 1;
                    }
                    fields.push(cur.field_name());
                    children.push(ch);
                }
                if !cur.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    let mut child_hashes: Vec<[u8; 32]> = Vec::with_capacity(children.len());
    let mut kind_indices = HashMap::<&str, usize>::new();

    for child in &children {
        if child.is_named() {
            let kind = child.kind();
            let needs_suffix = named_kind_counts[kind] > 1;
            let name = if needs_suffix {
                let idx = kind_indices.entry(kind).or_insert(0);
                let n = format!("{kind}_{idx}");
                *idx += 1;
                n
            } else {
                kind.to_string()
            };
            let id = format!("{node_id}/{name}");

            refs.extend(extract_refs_resolved(
                child,
                content,
                &id,
                source_id,
                language,
                container_node_id,
                ctx.queries,
                ctx.bounds,
            ));

            let child_container_owned;
            let child_container = match language.canonical_kind(child.kind()) {
                Some("function") | Some("method") => {
                    child_container_owned = id.clone();
                    Some(child_container_owned.as_str())
                }
                _ => container_node_id,
            };
            let child_hash = fold_injected_node(
                content,
                *child,
                &id,
                source_id,
                language,
                child_container,
                seen,
                refs,
                node_contents,
                node_children,
                injected_hashes,
                ctx,
            );
            injected_hashes.push((id, child_hash));
            child_hashes.push(child_hash);
        } else {
            let child_hash = fold_injected_node(
                content,
                *child,
                "",
                source_id,
                language,
                container_node_id,
                seen,
                refs,
                node_contents,
                node_children,
                injected_hashes,
                ctx,
            );
            child_hashes.push(child_hash);
        }
    }

    let raw_kind = node.kind();
    let canonical = language.canonical_kind(raw_kind).unwrap_or(raw_kind);
    let lang_name = language.name();
    if children.is_empty() {
        let token = node.utf8_text(content).unwrap_or("");
        let h = hash_leaf(canonical, token);
        if seen.insert(h) {
            node_contents.push(ContentRow {
                node_hash: h,
                node_tag: NODE_TAG_LEAF,
                kind: canonical.to_string(),
                raw_kind: raw_kind.to_string(),
                lang: lang_name.to_string(),
                token: Some(token.to_string()),
                arity: 0,
            });
        }
        h
    } else {
        let h = hash_internal(canonical, &child_hashes);
        if seen.insert(h) {
            node_contents.push(ContentRow {
                node_hash: h,
                node_tag: NODE_TAG_INTERNAL,
                kind: canonical.to_string(),
                raw_kind: raw_kind.to_string(),
                lang: lang_name.to_string(),
                token: None,
                arity: child_hashes.len(),
            });
            for (ord, (ch, field)) in child_hashes.iter().zip(&fields).enumerate() {
                node_children.push(ChildRow {
                    parent_hash: h,
                    ordinal: ord,
                    child_hash: *ch,
                    field: field.map(|f| f.to_string()),
                });
            }
        }
        h
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
fn collect_dirs(rel: &Path, created: &mut HashSet<String>, nodes_buf: &mut NodeBatch, mtime: i64) {
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
            // Directory-only rows: `source_file` stays `None`. Only
            // file nodes + their AST descendants carry the path.
            nodes_buf.push(
                accumulated.clone(),
                parent,
                name,
                1,
                0,
                mtime,
                String::new(),
                None,
            );
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

/// Recursively collect files. Skips gitignored entries (via the
/// `ignore` crate — see `crate::walk`) AND directories matched by
/// `is_bloat_dir`. See `tests::collect_files_skips_known_bloat_dirs`
/// for the skip-list pin and `tests::collect_files_respects_gitignore`
/// for the gitignore pin.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    crate::walk::walk_into(dir, out)
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
    fn sweep_orphaned_dirs_runs_when_files_are_deleted_between_parses() {
        // Skeptic finding on bead `ley-line-open-cbbedf`: the sweep-skip
        // optimization in `parse_into_conn` only fires when `deleted == 0`,
        // which is the cold-parse path. The "deleted > 0" path was logically
        // correct but had no test exercising it — meaning a future refactor
        // could break the sweep-fires path and CI would stay green.
        //
        // This test pins the contract: parse a tree, delete one file's
        // parent dir, reparse, assert the orphan dir is gone from `nodes`.
        let td = TempDir::new().unwrap();
        let root = td.path();
        std::fs::create_dir_all(root.join("doomed")).unwrap();
        std::fs::write(root.join("doomed/a.go"), b"package m\n").unwrap();
        std::fs::write(root.join("keep.go"), b"package m\n").unwrap();

        let conn = Connection::open_in_memory().unwrap();
        let _ = parse_into_conn(&conn, root, None, None).unwrap();

        // Confirm the dir row exists after the cold parse.
        let dir_before: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE id = 'doomed' AND kind = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dir_before, 1, "doomed/ dir row must exist after cold parse");

        // Remove the file AND its parent dir from disk, then reparse.
        std::fs::remove_file(root.join("doomed/a.go")).unwrap();
        std::fs::remove_dir(root.join("doomed")).unwrap();
        let r2 = parse_into_conn(&conn, root, None, None).unwrap();
        assert!(r2.deleted >= 1, "incremental must observe ≥1 deletion");

        // The sweep-runs path must fire and remove the now-orphaned dir.
        let dir_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE id = 'doomed' AND kind = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            dir_after, 0,
            "sweep_orphaned_dirs must remove the orphaned dir row when its only \
             child was deleted (deleted > 0 path of parse_into_conn)",
        );

        // Sanity: keep.go's file row survives.
        let keep_present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _file_index WHERE path = 'keep.go'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(keep_present, 1);
    }

    #[test]
    fn batched_inserts_preserve_record_content_not_just_row_count() {
        // Skeptic finding on bead `ley-line-open-cbbedf`: row-count parity
        // (which `parse_into_conn_skips_oversized_files` and friends cover)
        // is necessary but not sufficient — a chunk-boundary misalignment
        // in the multi-row VALUES batch could shift bound params between
        // rows, producing same-count-different-content output. This test
        // spot-checks that `_source` AND `_ast` rows for distinct files
        // survive the batched-insert path with their bound parameters
        // correctly aligned (no row-to-row leakage).
        let td = TempDir::new().unwrap();
        let root = td.path();
        // Two files so we exercise the multi-file batched path (single-row
        // batches would have hidden a chunk-boundary bug too).
        std::fs::write(root.join("a.go"), b"package alpha\n\nfunc Aaa() {}\n").unwrap();
        std::fs::write(root.join("b.go"), b"package beta\n\nfunc Bbb() {}\n").unwrap();

        let conn = Connection::open_in_memory().unwrap();
        parse_into_conn(&conn, root, None, None).unwrap();

        // _source(id, language, path) — file-backed parse stores the
        // canonicalized absolute path (not the relative one), so we
        // query by filename suffix rather than equality. Pin: each
        // file has exactly one row with language='go'.
        let a_row: (String, String) = conn
            .query_row(
                "SELECT id, language FROM _source WHERE path LIKE '%/a.go'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let b_row: (String, String) = conn
            .query_row(
                "SELECT id, language FROM _source WHERE path LIKE '%/b.go'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let (a_source_id, a_lang) = a_row;
        let (b_source_id, b_lang) = b_row;
        assert_eq!(a_lang, "go", "_source.language for a.go must be 'go'");
        assert_eq!(b_lang, "go", "_source.language for b.go must be 'go'");
        assert_ne!(
            a_source_id, b_source_id,
            "distinct files must have distinct _source.id",
        );

        // _ast(node_id, source_id, node_kind, ...) — pin: exactly one
        // function_declaration per file AND it joins to the correct
        // _source.id. If batched VALUES misaligned source_id across
        // rows, the count for one file would be 0 and the other would
        // be doubled.
        let a_fn_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _ast \
                 WHERE node_kind = 'function_declaration' AND source_id = ?1",
                [&a_source_id],
                |r| r.get(0),
            )
            .unwrap();
        let b_fn_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _ast \
                 WHERE node_kind = 'function_declaration' AND source_id = ?1",
                [&b_source_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            a_fn_count, 1,
            "a.go must contribute exactly 1 function_declaration via batched insert",
        );
        assert_eq!(
            b_fn_count, 1,
            "b.go must contribute exactly 1 function_declaration via batched insert",
        );
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

    #[test]
    fn collect_files_respects_gitignore() {
        // Bead ley-line-open-25685d: a `.gitignore` at the tree root
        // must exclude matching paths from the ingest walk. Without
        // this, users with big ignored `data/` / model-checkpoint dirs
        // get them indexed (blows up DB size + adds smell-rule noise)
        // and downstream consumers (mache) have to git-archive the
        // tracked tree before feeding it to LLO.
        let td = TempDir::new().unwrap();
        let root = td.path();

        std::fs::write(root.join("source.go"), b"package m").unwrap();
        std::fs::write(root.join(".gitignore"), b"data/\nbig.bin\n").unwrap();

        let data = root.join("data");
        std::fs::create_dir(&data).unwrap();
        std::fs::write(data.join("checkpoint.bin"), b"\x00\x01\x02").unwrap();
        std::fs::write(root.join("big.bin"), b"\x00").unwrap();

        let mut found = Vec::new();
        collect_files(root, &mut found).unwrap();

        let names: Vec<String> = found
            .iter()
            .filter_map(|p| p.file_name().and_then(|s| s.to_str().map(str::to_owned)))
            .collect();
        assert!(
            names.contains(&"source.go".to_string()),
            "source.go must be collected: {names:?}",
        );
        assert!(
            !names.contains(&"checkpoint.bin".to_string()),
            "data/checkpoint.bin is gitignored; must NOT be collected: {names:?}",
        );
        assert!(
            !names.contains(&"big.bin".to_string()),
            "big.bin is gitignored; must NOT be collected: {names:?}",
        );
    }

    #[test]
    fn collect_files_gitignore_works_without_a_git_repo() {
        // The walker uses `require_git(false)` so a `.gitignore` in a
        // plain directory (test fixture, extracted tarball) is honored
        // even without a `.git` directory present. Pin so a future
        // refactor that flips this back to `require_git(true)` fails
        // loudly instead of silently regressing consumers.
        let td = TempDir::new().unwrap();
        let root = td.path();
        assert!(
            !root.join(".git").exists(),
            "test precondition: temp dir is not a git repo",
        );

        std::fs::write(root.join("keep.go"), b"package k").unwrap();
        std::fs::write(root.join(".gitignore"), b"skip.go\n").unwrap();
        std::fs::write(root.join("skip.go"), b"package s").unwrap();

        let mut found = Vec::new();
        collect_files(root, &mut found).unwrap();

        let names: Vec<String> = found
            .iter()
            .filter_map(|p| p.file_name().and_then(|s| s.to_str().map(str::to_owned)))
            .collect();
        assert!(names.contains(&"keep.go".to_string()), "{names:?}");
        assert!(!names.contains(&"skip.go".to_string()), "{names:?}");
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
    ///
    /// And rootHash equals BLAKE3 of the segment files in canonical order.
    /// S1 end-to-end: with a signing key configured, the head written to disk
    /// carries a signature that verifies against the canonical head digest.
    /// Without this test, "the head is signed" is a claim, not a fact.
    #[test]
    #[serial_test::serial(env_leyline_head_keys)]
    fn head_is_signed_and_verifies_when_key_configured() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("signed.db");
        std::fs::write(&db, b"").unwrap();

        let seed = [3u8; 32];
        let hex: String = seed.iter().map(|b| format!("{b:02x}")).collect();
        // SAFETY: edition-2024 env mutation, serialized against every other
        // test that touches the head signing/trust vars by the `serial` label.
        unsafe { std::env::set_var("LEYLINE_HEAD_SIGNING_KEY", &hex) };
        let wrote = write_head_for_path(&db, 0);
        unsafe { std::env::remove_var("LEYLINE_HEAD_SIGNING_KEY") };
        wrote.expect("write signed head");

        let bytes = std::fs::read(with_extension(&db, "head.capnp")).unwrap();
        let msg = capnp::serialize::read_message(&mut &bytes[..], Default::default()).unwrap();
        let h: leyline_schema_capnp::head_capnp::head::Reader = msg.get_root().unwrap();

        let sig_bytes = h.get_signature().unwrap();
        assert_eq!(sig_bytes.len(), 64, "head must carry an Ed25519 signature");
        assert_eq!(
            h.get_signer_kid().unwrap().len(),
            8,
            "head must carry a kid"
        );

        // Re-derive the digest the way a verifier would, from the head itself.
        let root: [u8; 32] = h.get_root_hash().unwrap().get_bytes().unwrap()[..]
            .try_into()
            .unwrap();
        let parent: [u8; 32] = h.get_parent_hash().unwrap().get_bytes().unwrap()[..]
            .try_into()
            .unwrap();
        let digest = leyline_core::head_digest(
            h.get_generation(),
            leyline_core::Hash::from_bytes(root),
            leyline_core::Hash::from_bytes(parent),
        );

        let sig = ed25519_dalek::Signature::from_bytes(sig_bytes.try_into().unwrap());
        let signer = leyline_sign::root_signer::Ed25519RootSigner::from_seed(&seed);
        assert!(
            <leyline_sign::root_signer::Ed25519RootSigner as leyline_core::RootSigner>::verify(
                digest,
                &sig,
                &signer.verifying_key()
            ),
            "the written head signature must verify against the canonical digest"
        );
    }

    /// Write a signed head, then rewrite it on disk with `generation` bumped
    /// while keeping the original signature — the cheapest real tamper. A
    /// verifying reader must refuse it.
    fn tamper_head_generation(head_path: &Path) {
        use leyline_schema_capnp::head_capnp::head;
        let bytes = std::fs::read(head_path).unwrap();
        let msg = capnp::serialize::read_message(&mut &bytes[..], Default::default()).unwrap();
        let old: head::Reader = msg.get_root().unwrap();

        let mut out = capnp::message::Builder::new_default();
        {
            let mut h = out.init_root::<head::Builder>();
            h.set_generation(old.get_generation() + 1); // <- the tamper
            h.set_segment_bytes(old.get_segment_bytes());
            h.set_unbound_facts(old.get_unbound_facts());
            h.set_signature(old.get_signature().unwrap());
            h.set_signer_kid(old.get_signer_kid().unwrap());
            h.reborrow()
                .init_root_hash()
                .set_bytes(old.get_root_hash().unwrap().get_bytes().unwrap());
            h.reborrow()
                .init_parent_hash()
                .set_bytes(old.get_parent_hash().unwrap().get_bytes().unwrap());
        }
        let mut buf = Vec::new();
        capnp::serialize::write_message(&mut buf, &out).unwrap();
        std::fs::write(head_path, buf).unwrap();
    }

    /// S2: a head whose fields were edited after signing must not be adopted
    /// as chain state. Without this, S1's signature is decorative.
    #[test]
    #[serial_test::serial(env_leyline_head_keys)]
    fn read_head_rejects_a_tampered_head_when_trust_set_configured() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("tampered.db");
        std::fs::write(&db, b"").unwrap();

        let seed = [5u8; 32];
        let sk_hex: String = seed.iter().map(|b| format!("{b:02x}")).collect();
        let signer = leyline_sign::root_signer::Ed25519RootSigner::from_seed(&seed);
        let pk_hex: String = signer
            .verifying_key()
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        // SAFETY: serialized by the `serial` label above.
        unsafe { std::env::set_var("LEYLINE_HEAD_SIGNING_KEY", &sk_hex) };
        let wrote = write_head_for_path(&db, 0);
        unsafe { std::env::remove_var("LEYLINE_HEAD_SIGNING_KEY") };
        wrote.expect("write signed head");

        let head_path = with_extension(&db, "head.capnp");

        // Untampered + trusted ⇒ reads fine.
        unsafe { std::env::set_var("LEYLINE_HEAD_TRUSTED_KEYS", &pk_hex) };
        let clean = read_head_for_chain(&head_path);
        assert!(clean.is_ok(), "a validly signed head must read: {clean:?}");

        tamper_head_generation(&head_path);
        let tampered = read_head_for_chain(&head_path);
        unsafe { std::env::remove_var("LEYLINE_HEAD_TRUSTED_KEYS") };

        assert!(
            tampered.is_err(),
            "a tampered head must be refused, got {tampered:?}"
        );
    }

    /// With no trust set configured, behavior is unchanged from pre-S2: an
    /// unsigned head still reads. Signing is opt-in; verification must not
    /// break every existing arena.
    #[test]
    #[serial_test::serial(env_leyline_head_keys)]
    fn read_head_accepts_unsigned_head_when_no_trust_set() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("unsigned.db");
        std::fs::write(&db, b"").unwrap();
        write_head_for_path(&db, 0).expect("write unsigned head");
        let got = read_head_for_chain(&with_extension(&db, "head.capnp"));
        assert!(got.is_ok(), "unsigned head must still read: {got:?}");
    }

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
