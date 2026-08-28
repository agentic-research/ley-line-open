//! Canonical `nodes` table schema shared by all ley-line crates.
//!
//! This is the contract: mache reads it, leyline-fs reads and writes it,
//! leyline-ts projects tree-sitter ASTs into it. One definition, no drift.
//!
//! # projection-v5: file-scoped integer nids (bead `ley-line-open-17c271`)
//!
//! The pre-v5 `nodes.id` was the node's own ancestry path — an O(depth) TEXT
//! primary key repeated (with its prefix) on every descendant row and in
//! every referencing column. Measured on a 3,150,850-node arena that string
//! was ~72% of arena bytes, and the depth curve was super-linear: identical
//! content cost 5.3× the storage at directory depth 96 vs depth 1.
//!
//! v5 replaces the locator-as-key with a file-scoped integer surrogate:
//!
//! ```text
//! nid = (file_id << 24) | ordinal      // files and their AST nodes
//! nid = -dir_id                        // directories (negative space)
//! ```
//!
//! - `file_id` is interned in `files` (append-only; rows are never deleted,
//!   so ids are never reused and a re-created path re-binds to its old id).
//! - `ordinal` is the node's pre-order rank within its file's parse — the
//!   same dense `0..n-1` the ADR-0026 pointer store already writes as
//!   `blob_ord`. Ordinal 0 is the file's own node (the AST root).
//! - Directories live in `dirs` as an interned adjacency chain; their nids
//!   are the negative of their `dir_id`, so the tree stays one namespace.
//!
//! `parent_nid` and `ord` (sibling index in source order) are STORED — the
//! pre-v5 `parent_id` was derived by string surgery on the id, which is
//! meaningless for an integer key. Display names are NOT stored per AST row:
//! a node's rendered name is `{raw_kind}` when it is its parent's only child
//! of that kind and `{raw_kind}_{k}` (k = rank among same-kind siblings by
//! `ord`, 0-based) otherwise — exactly the pre-v5 writer's `needs_suffix`
//! scheme, now derived at read time by [`node_path`] / [`V_NODE_PATH_DDL`].
//!
//! The path is thereby demoted from identity to DISPLAY (ADR-0034's D6
//! locator/address/content split): [`resolve_path`] turns a rendered path
//! back into a nid, and nothing else in the schema stores one.

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

// ---------------------------------------------------------------------------
// nid scheme
// ---------------------------------------------------------------------------

/// Bits of `nid` reserved for the within-file ordinal.
pub const NID_ORDINAL_BITS: u32 = 24;

/// Mask over the ordinal bits: `nid & NID_ORDINAL_MASK` is the pre-order
/// rank; `nid >> NID_ORDINAL_BITS` is the `file_id`. 24 bits = 16,777,215
/// nodes per file; the writer must refuse a file that overflows it rather
/// than let ordinals bleed into the next file's range.
pub const NID_ORDINAL_MASK: i64 = (1 << NID_ORDINAL_BITS) - 1;

/// The nid of `ordinal` within `file_id`'s range. `ordinal` 0 is the file's
/// own node (the AST root).
#[inline]
pub fn file_nid(file_id: i64, ordinal: i64) -> i64 {
    debug_assert!(file_id > 0, "file_id is 1-based (files.file_id rowid)");
    debug_assert!((0..=NID_ORDINAL_MASK).contains(&ordinal));
    (file_id << NID_ORDINAL_BITS) | ordinal
}

/// The nid of a directory: dirs live in negative space.
#[inline]
pub fn dir_nid(dir_id: i64) -> i64 {
    debug_assert!(dir_id > 0, "dir_id is 1-based (dirs.dir_id rowid)");
    -dir_id
}

/// Inverse of [`dir_nid`]: `Some(dir_id)` when `nid` names a directory.
#[inline]
pub fn nid_dir_id(nid: i64) -> Option<i64> {
    (nid < 0).then_some(-nid)
}

/// The `file_id` a non-negative nid belongs to (`None` for dir nids).
#[inline]
pub fn nid_file_id(nid: i64) -> Option<i64> {
    (nid >= 0).then_some(nid >> NID_ORDINAL_BITS)
}

/// The within-file ordinal of a non-negative nid (`None` for dir nids).
#[inline]
pub fn nid_ordinal(nid: i64) -> Option<i64> {
    (nid >= 0).then_some(nid & NID_ORDINAL_MASK)
}

/// Inclusive nid range `[lo, hi]` owned by `file_id`. This is THE
/// file-scoping predicate of the projection: `WHERE nid BETWEEN ?1 AND ?2`
/// plans as a PRIMARY KEY range SEARCH on every nid-keyed table, replacing
/// the pre-v5 prefix-LIKE (which planned as a full SCAN — no
/// `case_sensitive_like`, non-literal prefix) and its unanchored-`%`
/// over-match hazard.
#[inline]
pub fn file_nid_range(file_id: i64) -> (i64, i64) {
    let lo = file_id << NID_ORDINAL_BITS;
    (lo, lo | NID_ORDINAL_MASK)
}

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

/// Interning tables: every path component, tree-sitter kind, directory, and
/// file is stored ONCE. All four are append-only — a row, once written, is
/// never deleted and never renumbered. That append-only discipline is
/// load-bearing:
///
/// - `files.file_id` feeds nids; deleting a row would let SQLite reuse the
///   rowid and re-bind a dead file's nid range to an unrelated new file
///   (the exact reuse hazard `content_manifest` invalidation documents).
///   A deleted file's row simply goes stale; a re-created path re-binds to
///   its old `file_id`.
/// - Renaming a directory is `UPDATE dirs SET name_id = ?` — ONE row for a
///   subtree of any size, because descendants reference the dir by id, not
///   by path.
/// - `dirs.dir_id = 1` is the tree root (name "", parent NULL). The CHECK
///   pins root uniqueness structurally: `UNIQUE(parent_dir_id, name_id)`
///   cannot, because SQLite treats NULLs as distinct in UNIQUE indexes.
pub const INTERN_TABLES_DDL: &str = "\
CREATE TABLE IF NOT EXISTS names (
    name_id INTEGER PRIMARY KEY,
    text TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS kinds (
    kind_id INTEGER PRIMARY KEY,
    lang TEXT NOT NULL,
    raw_kind TEXT NOT NULL,
    UNIQUE(lang, raw_kind)
);
CREATE TABLE IF NOT EXISTS dirs (
    dir_id INTEGER PRIMARY KEY,
    parent_dir_id INTEGER,
    name_id INTEGER NOT NULL,
    CHECK (dir_id = 1 OR parent_dir_id IS NOT NULL),
    UNIQUE(parent_dir_id, name_id)
);
CREATE TABLE IF NOT EXISTS files (
    file_id INTEGER PRIMARY KEY,
    dir_id INTEGER NOT NULL,
    name_id INTEGER NOT NULL,
    UNIQUE(dir_id, name_id)
);";

/// The `nodes` table DDL (table only, no indexes).
///
/// One row per tree node — directories (negative nids), files (ordinal 0 of
/// their range), and AST nodes. `name_id` is set for filesystem rows and
/// NULL for AST rows (their display name derives from `kind_id` + `ord`);
/// `kind_id` is set for AST rows (and the file row, which doubles as the
/// AST root) and NULL for directories.
///
/// `record` keeps its pre-v5 contract (leaf token text / file content —
/// TEXT, deliberately not JSON; see bead `ley-line-open-f7966d`).
/// `record_id` and `source_file` remain for mache's lazy-resolution flow;
/// ley-line's own writers leave both NULL — a row's file is `nid >> 24`,
/// so storing the path per row would re-import the locator freight v5
/// evicts.
pub const NODES_TABLE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS nodes (
    nid INTEGER PRIMARY KEY,
    parent_nid INTEGER,
    name_id INTEGER,
    kind_id INTEGER,
    kind INTEGER NOT NULL,
    ord INTEGER NOT NULL DEFAULT 0,
    size INTEGER DEFAULT 0,
    mtime INTEGER NOT NULL,
    record_id TEXT,
    record TEXT,
    source_file TEXT
);";

/// The `nodes` indexes (no table) — deferred post-load to avoid paying
/// B-tree maintenance per INSERT during bulk parse.
///
/// `idx_parent_kind_ord` serves both directory listing (prefix on
/// `parent_nid`, children in source order) and [`resolve_path`]'s AST-segment
/// step (`parent_nid = ? AND kind_id = ?` ordered by `ord`).
///
/// `idx_source_file` keeps its pre-v5 shape: partial, because ley-line's
/// writers leave `source_file` NULL and a full index over a NULL-only column
/// would add pages per row without serving a query.
pub const NODES_INDEXES_DDL: &str = "\
CREATE INDEX IF NOT EXISTS idx_parent_kind_ord ON nodes(parent_nid, kind_id, ord);
CREATE INDEX IF NOT EXISTS idx_source_file ON nodes(source_file) WHERE source_file IS NOT NULL;";

/// Combined interning + `nodes` table + index DDL. Preserves the pre-split
/// contract for callers that want the schema fully materialized in one
/// batch. Bulk-load callers (e.g. `cmd_parse`) instead call
/// [`create_nodes_table`] (insert phase) and [`create_nodes_indexes`]
/// (post-COMMIT) separately — see bead `ley-line-open-9ccbc7`.
pub const NODES_DDL: &str = "\
CREATE TABLE IF NOT EXISTS names (
    name_id INTEGER PRIMARY KEY,
    text TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS kinds (
    kind_id INTEGER PRIMARY KEY,
    lang TEXT NOT NULL,
    raw_kind TEXT NOT NULL,
    UNIQUE(lang, raw_kind)
);
CREATE TABLE IF NOT EXISTS dirs (
    dir_id INTEGER PRIMARY KEY,
    parent_dir_id INTEGER,
    name_id INTEGER NOT NULL,
    CHECK (dir_id = 1 OR parent_dir_id IS NOT NULL),
    UNIQUE(parent_dir_id, name_id)
);
CREATE TABLE IF NOT EXISTS files (
    file_id INTEGER PRIMARY KEY,
    dir_id INTEGER NOT NULL,
    name_id INTEGER NOT NULL,
    UNIQUE(dir_id, name_id)
);
CREATE TABLE IF NOT EXISTS nodes (
    nid INTEGER PRIMARY KEY,
    parent_nid INTEGER,
    name_id INTEGER,
    kind_id INTEGER,
    kind INTEGER NOT NULL,
    ord INTEGER NOT NULL DEFAULT 0,
    size INTEGER DEFAULT 0,
    mtime INTEGER NOT NULL,
    record_id TEXT,
    record TEXT,
    source_file TEXT
);
CREATE INDEX IF NOT EXISTS idx_parent_kind_ord ON nodes(parent_nid, kind_id, ord);
-- Partial index: see `NODES_INDEXES_DDL` for rationale.
CREATE INDEX IF NOT EXISTS idx_source_file ON nodes(source_file) WHERE source_file IS NOT NULL;";

/// Bulk path renderer: `v_node_path(nid, path)` for every `nodes` row.
///
/// The recursive member walks parents upward; the name of each hop comes
/// from `v_node_name`. Point lookups should use [`node_path`] (Rust) — a
/// recursive-CTE view cannot use the caller's `WHERE nid = ?` to prune the
/// walk, so selecting one row from this view still renders every row. The
/// view exists for bulk export, debugging, and consumers (mache) that want
/// the whole mapping in one scan.
pub const V_NODE_PATH_DDL: &str = "\
CREATE VIEW IF NOT EXISTS v_node_name AS
SELECT n.nid AS nid,
       CASE
         WHEN n.name_id IS NOT NULL THEN (SELECT text FROM names WHERE name_id = n.name_id)
         WHEN (SELECT COUNT(*) FROM nodes s
                WHERE s.parent_nid = n.parent_nid AND s.kind_id = n.kind_id) > 1
           THEN (SELECT raw_kind FROM kinds k WHERE k.kind_id = n.kind_id)
                || '_' ||
                (SELECT COUNT(*) FROM nodes s
                  WHERE s.parent_nid = n.parent_nid AND s.kind_id = n.kind_id
                    AND s.ord < n.ord)
         ELSE (SELECT raw_kind FROM kinds k WHERE k.kind_id = n.kind_id)
       END AS name
FROM nodes n;
CREATE VIEW IF NOT EXISTS v_node_path AS
WITH RECURSIVE walk(nid, path, cursor) AS (
  SELECT n.nid, '', n.nid FROM nodes n
  UNION ALL
  SELECT w.nid,
         CASE WHEN v.name = '' THEN w.path
              WHEN w.path = '' THEN v.name
              ELSE v.name || '/' || w.path END,
         p.parent_nid
  FROM walk w
  JOIN nodes p ON p.nid = w.cursor
  JOIN v_node_name v ON v.nid = w.cursor
)
SELECT nid, path FROM walk WHERE cursor IS NULL;";

// ---------------------------------------------------------------------------
// Schema creation
// ---------------------------------------------------------------------------

/// Create the interning tables, the `nodes` table, its indexes, and the
/// display views (idempotent), and seed the root directory.
pub fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(NODES_DDL)?;
    conn.execute_batch(V_NODE_PATH_DDL)?;
    ensure_root_dir(conn)?;
    Ok(())
}

/// Create the interning tables and the `nodes` table — no indexes, no
/// views. Pair with [`create_nodes_indexes`] (called post-`COMMIT`) for
/// bulk-load paths where index maintenance during INSERT dominates wall
/// clock. Seeds the root directory.
pub fn create_nodes_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(INTERN_TABLES_DDL)?;
    conn.execute_batch(NODES_TABLE_DDL)?;
    ensure_root_dir(conn)?;
    Ok(())
}

/// Create only the `nodes` indexes and display views. Idempotent.
pub fn create_nodes_indexes(conn: &Connection) -> Result<()> {
    conn.execute_batch(NODES_INDEXES_DDL)?;
    conn.execute_batch(V_NODE_PATH_DDL)?;
    Ok(())
}

/// Seed `dirs` row 1 — the tree root (name "", parent NULL) — exactly once.
///
/// `INSERT OR IGNORE` keyed on the PRIMARY KEY, not the UNIQUE pair: SQLite
/// treats NULLs as distinct in UNIQUE indexes, so `(NULL, '')` would
/// otherwise insert a second root per call.
fn ensure_root_dir(conn: &Connection) -> Result<()> {
    let root_name = intern_name(conn, "")?;
    conn.execute(
        "INSERT OR IGNORE INTO dirs (dir_id, parent_dir_id, name_id) VALUES (1, NULL, ?1)",
        [root_name],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Interning
// ---------------------------------------------------------------------------

/// Intern a path component (dir or file name). Returns its stable `name_id`.
pub fn intern_name(conn: &Connection, text: &str) -> Result<i64> {
    conn.execute("INSERT OR IGNORE INTO names (text) VALUES (?1)", [text])?;
    conn.query_row("SELECT name_id FROM names WHERE text = ?1", [text], |r| {
        r.get(0)
    })
    .context("names row must exist after INSERT OR IGNORE")
}

/// Intern a tree-sitter kind under its language. Returns its stable
/// `kind_id`.
pub fn intern_kind(conn: &Connection, lang: &str, raw_kind: &str) -> Result<i64> {
    conn.execute(
        "INSERT OR IGNORE INTO kinds (lang, raw_kind) VALUES (?1, ?2)",
        params![lang, raw_kind],
    )?;
    conn.query_row(
        "SELECT kind_id FROM kinds WHERE lang = ?1 AND raw_kind = ?2",
        params![lang, raw_kind],
        |r| r.get(0),
    )
    .context("kinds row must exist after INSERT OR IGNORE")
}

/// Intern the directory chain of `dir_path` (e.g. `"a/b/c"`; `""` is the
/// root) and return the leaf `dir_id`. Creates missing links; existing
/// links keep their ids.
pub fn intern_dir_chain(conn: &Connection, dir_path: &str) -> Result<i64> {
    let mut cur: i64 = 1; // root
    if dir_path.is_empty() {
        return Ok(cur);
    }
    for comp in dir_path.split('/') {
        let name_id = intern_name(conn, comp)?;
        conn.execute(
            "INSERT OR IGNORE INTO dirs (parent_dir_id, name_id) VALUES (?1, ?2)",
            params![cur, name_id],
        )?;
        cur = conn
            .query_row(
                "SELECT dir_id FROM dirs WHERE parent_dir_id = ?1 AND name_id = ?2",
                params![cur, name_id],
                |r| r.get(0),
            )
            .context("dirs row must exist after INSERT OR IGNORE")?;
    }
    Ok(cur)
}

/// Look up or create the `file_id` for `rel_path` (a `/`-separated path
/// relative to the tree root). Append-only: a path parses to the same
/// `file_id` for the life of the arena, and ids are never reused.
pub fn ensure_file_id(conn: &Connection, rel_path: &str) -> Result<i64> {
    let (dir_path, file_name) = match rel_path.rsplit_once('/') {
        Some((d, f)) => (d, f),
        None => ("", rel_path),
    };
    let dir_id = intern_dir_chain(conn, dir_path)?;
    let name_id = intern_name(conn, file_name)?;
    conn.execute(
        "INSERT OR IGNORE INTO files (dir_id, name_id) VALUES (?1, ?2)",
        params![dir_id, name_id],
    )?;
    conn.query_row(
        "SELECT file_id FROM files WHERE dir_id = ?1 AND name_id = ?2",
        params![dir_id, name_id],
        |r| r.get(0),
    )
    .context("files row must exist after INSERT OR IGNORE")
}

/// Intern the directory chain of `file_rel_path`'s parent and make sure a
/// presentation row exists in `nodes` for the root and every link (negative
/// nids, `INSERT OR IGNORE` so an existing dir row keeps its mtime).
/// Returns the leaf `dir_id` — the file node's parent is `dir_nid` of it.
pub fn ensure_dir_nodes(conn: &Connection, file_rel_path: &str, mtime: i64) -> Result<i64> {
    let dir_path = file_rel_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let root_name = intern_name(conn, "")?;
    conn.execute(
        "INSERT OR IGNORE INTO nodes (nid, parent_nid, name_id, kind, ord, mtime, record) \
         VALUES (?1, NULL, ?2, 1, 0, ?3, '')",
        params![dir_nid(1), root_name, mtime],
    )?;
    let mut cur: i64 = 1;
    if dir_path.is_empty() {
        return Ok(cur);
    }
    for comp in dir_path.split('/') {
        let name_id = intern_name(conn, comp)?;
        conn.execute(
            "INSERT OR IGNORE INTO dirs (parent_dir_id, name_id) VALUES (?1, ?2)",
            params![cur, name_id],
        )?;
        let next: i64 = conn
            .query_row(
                "SELECT dir_id FROM dirs WHERE parent_dir_id = ?1 AND name_id = ?2",
                params![cur, name_id],
                |r| r.get(0),
            )
            .context("dirs row must exist after INSERT OR IGNORE")?;
        conn.execute(
            "INSERT OR IGNORE INTO nodes (nid, parent_nid, name_id, kind, ord, mtime, record) \
             VALUES (?1, ?2, ?3, 1, 0, ?4, '')",
            params![dir_nid(next), dir_nid(cur), name_id, mtime],
        )?;
        cur = next;
    }
    Ok(cur)
}

/// The `file_id` of `rel_path`, if the arena has ever parsed it.
pub fn lookup_file_id(conn: &Connection, rel_path: &str) -> Result<Option<i64>> {
    let (dir_path, file_name) = match rel_path.rsplit_once('/') {
        Some((d, f)) => (d, f),
        None => ("", rel_path),
    };
    let mut cur: i64 = 1;
    if !dir_path.is_empty() {
        for comp in dir_path.split('/') {
            let next: Option<i64> = conn
                .query_row(
                    "SELECT d.dir_id FROM dirs d JOIN names n ON n.name_id = d.name_id \
                     WHERE d.parent_dir_id = ?1 AND n.text = ?2",
                    params![cur, comp],
                    |r| r.get(0),
                )
                .optional()?;
            match next {
                Some(d) => cur = d,
                None => return Ok(None),
            }
        }
    }
    conn.query_row(
        "SELECT f.file_id FROM files f JOIN names n ON n.name_id = f.name_id \
         WHERE f.dir_id = ?1 AND n.text = ?2",
        params![cur, file_name],
        |r| r.get(0),
    )
    .optional()
    .map_err(Into::into)
}

// ---------------------------------------------------------------------------
// Row insert
// ---------------------------------------------------------------------------

/// Insert a single node row.
///
/// `INSERT OR REPLACE` keeps the pre-v5 upsert contract: re-parsing the
/// same tree rewrites rows in place, and a changed file simply replaces its
/// range.
#[allow(clippy::too_many_arguments)]
pub fn insert_node(
    conn: &Connection,
    nid: i64,
    parent_nid: Option<i64>,
    name_id: Option<i64>,
    kind_id: Option<i64>,
    kind: i32,
    ord: i64,
    size: i64,
    mtime: i64,
    record: &str,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO nodes (nid, parent_nid, name_id, kind_id, kind, ord, size, mtime, record) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![nid, parent_nid, name_id, kind_id, kind, ord, size, mtime, record],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Display: nid → path, path → nid
// ---------------------------------------------------------------------------

/// Render one node's display name — the interned text for filesystem rows,
/// `{raw_kind}[_{k}]` for AST rows (`needs_suffix` parity with the pre-v5
/// writer: suffixed iff the parent has >1 child of this kind; `k` is the
/// rank among same-kind siblings ordered by `ord`, 0-based).
fn node_display_name(conn: &Connection, nid: i64) -> Result<Option<String>> {
    type NodeNameRow = (Option<i64>, Option<i64>, Option<i64>, i64);
    let row: Option<NodeNameRow> = conn
        .query_row(
            "SELECT parent_nid, name_id, kind_id, ord FROM nodes WHERE nid = ?1",
            [nid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((parent_nid, name_id, kind_id, ord)) = row else {
        return Ok(None);
    };
    if let Some(name_id) = name_id {
        let text: String = conn.query_row(
            "SELECT text FROM names WHERE name_id = ?1",
            [name_id],
            |r| r.get(0),
        )?;
        return Ok(Some(text));
    }
    let Some(kind_id) = kind_id else {
        bail!("nodes row {nid} has neither name_id nor kind_id");
    };
    let raw_kind: String = conn.query_row(
        "SELECT raw_kind FROM kinds WHERE kind_id = ?1",
        [kind_id],
        |r| r.get(0),
    )?;
    let same_kind: i64 = conn.query_row(
        "SELECT COUNT(*) FROM nodes WHERE parent_nid IS ?1 AND kind_id = ?2",
        params![parent_nid, kind_id],
        |r| r.get(0),
    )?;
    if same_kind > 1 {
        let rank: i64 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE parent_nid IS ?1 AND kind_id = ?2 AND ord < ?3",
            params![parent_nid, kind_id, ord],
            |r| r.get(0),
        )?;
        Ok(Some(format!("{raw_kind}_{rank}")))
    } else {
        Ok(Some(raw_kind))
    }
}

/// Render a nid's full display path (`""` for the root directory), walking
/// `parent_nid` upward. Point-lookup counterpart of [`V_NODE_PATH_DDL`].
pub fn node_path(conn: &Connection, nid: i64) -> Result<Option<String>> {
    let mut segments: Vec<String> = Vec::new();
    let mut cursor = Some(nid);
    while let Some(cur) = cursor {
        let Some(name) = node_display_name(conn, cur)? else {
            // Dangling parent chain, or `nid` not in `nodes` at all.
            return Ok(None);
        };
        if !name.is_empty() {
            segments.push(name);
        }
        cursor = conn
            .query_row("SELECT parent_nid FROM nodes WHERE nid = ?1", [cur], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .optional()?
            .flatten();
    }
    segments.reverse();
    Ok(Some(segments.join("/")))
}

/// Resolve a display path back to its nid — the inverse of [`node_path`].
///
/// Walks `dirs`/`files` by interned name while the prefix names filesystem
/// levels, then AST segments by the `{raw_kind}[_{k}]` scheme. For an AST
/// segment the singleton form is tried first (a segment that IS a raw kind
/// with exactly one such child); otherwise the trailing `_{k}` splits off as
/// the same-kind rank. `""` resolves to the root directory's nid.
pub fn resolve_path(conn: &Connection, path: &str) -> Result<Option<i64>> {
    if path.is_empty() {
        return Ok(Some(dir_nid(1)));
    }
    // Longest filesystem prefix: descend dirs while components match; at
    // each level, a matching file switches the walk into nid space.
    let comps: Vec<&str> = path.split('/').collect();
    let mut dir_id: i64 = 1;
    let mut i = 0;
    let mut node: Option<i64> = None;
    while i < comps.len() {
        let comp = comps[i];
        // A file at the current level?
        let file: Option<i64> = conn
            .query_row(
                "SELECT f.file_id FROM files f JOIN names n ON n.name_id = f.name_id \
                 WHERE f.dir_id = ?1 AND n.text = ?2",
                params![dir_id, comp],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(file_id) = file {
            node = Some(file_nid(file_id, 0));
            i += 1;
            break;
        }
        // A subdirectory?
        let sub: Option<i64> = conn
            .query_row(
                "SELECT d.dir_id FROM dirs d JOIN names n ON n.name_id = d.name_id \
                 WHERE d.parent_dir_id = ?1 AND n.text = ?2",
                params![dir_id, comp],
                |r| r.get(0),
            )
            .optional()?;
        match sub {
            Some(d) => {
                dir_id = d;
                i += 1;
            }
            None => return Ok(None),
        }
    }
    let mut cur = match node {
        Some(n) => n,
        // Path named a directory.
        None => return Ok(Some(dir_nid(dir_id))),
    };
    // AST segments.
    for comp in &comps[i..] {
        let next = resolve_ast_segment(conn, cur, comp)?;
        match next {
            Some(n) => cur = n,
            None => return Ok(None),
        }
    }
    Ok(Some(cur))
}

/// Resolve one `{raw_kind}[_{k}]` segment under `parent_nid`.
fn resolve_ast_segment(conn: &Connection, parent_nid: i64, segment: &str) -> Result<Option<i64>> {
    // Singleton form: the segment is a raw kind with EXACTLY one such child
    // (the writer only renders the bare kind in that case — a bare kind with
    // multiple children cannot be a rendered name, so it is not a match).
    let singleton: Option<i64> = conn
        .query_row(
            "SELECT CASE WHEN COUNT(*) = 1 THEN MIN(n.nid) END \
             FROM nodes n JOIN kinds k ON k.kind_id = n.kind_id \
             WHERE n.parent_nid = ?1 AND k.raw_kind = ?2",
            params![parent_nid, segment],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    if let Some(nid) = singleton {
        return Ok(Some(nid));
    }
    // Suffixed form: split the trailing `_{k}`. Raw kinds contain
    // underscores, so only the LAST underscore with an all-digit tail is a
    // candidate rank.
    let Some((kind, rank_str)) = segment.rsplit_once('_') else {
        return Ok(None);
    };
    if rank_str.is_empty() || !rank_str.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(None);
    }
    let rank: i64 = rank_str.parse().unwrap_or(-1);
    conn.query_row(
        "SELECT n.nid FROM nodes n JOIN kinds k ON k.kind_id = n.kind_id \
         WHERE n.parent_nid = ?1 AND k.raw_kind = ?2 \
         ORDER BY n.ord LIMIT 1 OFFSET ?3",
        params![parent_nid, kind, rank],
        |r| r.get(0),
    )
    .optional()
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        conn
    }

    // ── nid scheme ─────────────────────────────────────────────────────

    #[test]
    fn nid_scheme_round_trips_and_partitions() {
        let nid = file_nid(3, 41);
        assert_eq!(nid_file_id(nid), Some(3));
        assert_eq!(nid_ordinal(nid), Some(41));
        assert_eq!(nid_dir_id(nid), None);

        let d = dir_nid(7);
        assert!(d < 0);
        assert_eq!(nid_dir_id(d), Some(7));
        assert_eq!(nid_file_id(d), None);

        // The range of file f ends exactly one below file f+1's base —
        // an off-by-one here is the failure F4d's trap exists to catch.
        let (lo, hi) = file_nid_range(3);
        assert_eq!(lo, file_nid(3, 0));
        assert_eq!(hi, file_nid(3, NID_ORDINAL_MASK));
        assert_eq!(hi + 1, file_nid(4, 0));
    }

    // ── schema + interning ─────────────────────────────────────────────

    #[test]
    fn create_schema_is_idempotent() {
        let conn = mem();
        create_schema(&conn).unwrap();
        // Exactly one root even after repeated creation: the UNIQUE pair
        // cannot pin (NULL, "") so the PK seed must.
        let roots: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM dirs WHERE parent_dir_id IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(roots, 1);
    }

    #[test]
    fn interning_is_stable_and_deduplicating() {
        let conn = mem();
        let a = intern_name(&conn, "src").unwrap();
        let b = intern_name(&conn, "src").unwrap();
        let c = intern_name(&conn, "lib").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);

        let k1 = intern_kind(&conn, "go", "identifier").unwrap();
        let k2 = intern_kind(&conn, "go", "identifier").unwrap();
        let k3 = intern_kind(&conn, "python", "identifier").unwrap();
        assert_eq!(k1, k2);
        assert_ne!(k1, k3, "kinds are interned per language");
    }

    #[test]
    fn dir_chain_reuses_shared_prefixes() {
        let conn = mem();
        let abc = intern_dir_chain(&conn, "a/b/c").unwrap();
        let abd = intern_dir_chain(&conn, "a/b/d").unwrap();
        let abc2 = intern_dir_chain(&conn, "a/b/c").unwrap();
        assert_eq!(abc, abc2);
        assert_ne!(abc, abd);
        // Three links for a/b/c plus one for d plus the root.
        let dirs: i64 = conn
            .query_row("SELECT COUNT(*) FROM dirs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(dirs, 5);
    }

    #[test]
    fn file_ids_are_stable_and_never_reused() {
        let conn = mem();
        let a = ensure_file_id(&conn, "src/a.go").unwrap();
        let b = ensure_file_id(&conn, "src/b.go").unwrap();
        assert_ne!(a, b);
        assert_eq!(ensure_file_id(&conn, "src/a.go").unwrap(), a);
        assert_eq!(lookup_file_id(&conn, "src/a.go").unwrap(), Some(a));
        assert_eq!(lookup_file_id(&conn, "src/zzz.go").unwrap(), None);

        // Append-only: `files` rows are never deleted, so the max id only
        // grows and a re-created path re-binds to its old id.
        let c = ensure_file_id(&conn, "src/c.go").unwrap();
        assert!(c > b);
        assert_eq!(ensure_file_id(&conn, "src/a.go").unwrap(), a);
    }

    // ── display round-trip ─────────────────────────────────────────────

    /// Hand-built tree:
    ///
    /// ```text
    /// ""                          dir 1        nid -1
    /// src/                        dir 2        nid -2
    /// src/a.go                    file 1       nid  1<<24        (root, kind source_file)
    ///   function_declaration      ord 0        nid  base+1   (singleton kind)
    ///     identifier              ord 0        nid  base+2   (two identifiers → _0)
    ///     identifier              ord 1        nid  base+3   (→ _1)
    /// ```
    fn display_fixture() -> (Connection, i64) {
        let conn = mem();
        let file_id = ensure_file_id(&conn, "src/a.go").unwrap();
        let base = file_nid(file_id, 0);
        let dir_src = intern_dir_chain(&conn, "src").unwrap();
        let n_src = intern_name(&conn, "src").unwrap();
        let n_ago = intern_name(&conn, "a.go").unwrap();
        let k_root = intern_kind(&conn, "go", "source_file").unwrap();
        let k_fn = intern_kind(&conn, "go", "function_declaration").unwrap();
        let k_id = intern_kind(&conn, "go", "identifier").unwrap();

        let root_name = intern_name(&conn, "").unwrap();
        insert_node(
            &conn,
            dir_nid(1),
            None,
            Some(root_name),
            None,
            1,
            0,
            0,
            1,
            "",
        )
        .unwrap();
        insert_node(
            &conn,
            dir_nid(dir_src),
            Some(dir_nid(1)),
            Some(n_src),
            None,
            1,
            0,
            0,
            1,
            "",
        )
        .unwrap();
        insert_node(
            &conn,
            base,
            Some(dir_nid(dir_src)),
            Some(n_ago),
            Some(k_root),
            1,
            0,
            0,
            1,
            "",
        )
        .unwrap();
        insert_node(
            &conn,
            base + 1,
            Some(base),
            None,
            Some(k_fn),
            1,
            0,
            0,
            1,
            "",
        )
        .unwrap();
        insert_node(
            &conn,
            base + 2,
            Some(base + 1),
            None,
            Some(k_id),
            0,
            0,
            1,
            1,
            "x",
        )
        .unwrap();
        insert_node(
            &conn,
            base + 3,
            Some(base + 1),
            None,
            Some(k_id),
            0,
            1,
            1,
            1,
            "y",
        )
        .unwrap();
        (conn, base)
    }

    #[test]
    fn node_path_renders_the_pre_v5_display_scheme() {
        let (conn, base) = display_fixture();
        assert_eq!(node_path(&conn, dir_nid(1)).unwrap().unwrap(), "");
        assert_eq!(node_path(&conn, base).unwrap().unwrap(), "src/a.go");
        assert_eq!(
            node_path(&conn, base + 1).unwrap().unwrap(),
            "src/a.go/function_declaration",
            "a singleton kind renders bare — no suffix"
        );
        assert_eq!(
            node_path(&conn, base + 2).unwrap().unwrap(),
            "src/a.go/function_declaration/identifier_0",
            "same-kind siblings suffix by rank in `ord` order"
        );
        assert_eq!(
            node_path(&conn, base + 3).unwrap().unwrap(),
            "src/a.go/function_declaration/identifier_1"
        );
    }

    #[test]
    fn resolve_path_inverts_node_path_for_every_row() {
        let (conn, _) = display_fixture();
        let nids: Vec<i64> = {
            let mut stmt = conn.prepare("SELECT nid FROM nodes").unwrap();
            let v = stmt
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            v
        };
        assert!(nids.len() >= 6, "fixture must produce rows");
        for nid in nids {
            let path = node_path(&conn, nid).unwrap().unwrap();
            assert_eq!(
                resolve_path(&conn, &path).unwrap(),
                Some(nid),
                "resolve_path must invert node_path for {path:?}"
            );
        }
    }

    #[test]
    fn resolve_path_rejects_a_bare_kind_with_multiple_children() {
        let (conn, _) = display_fixture();
        // `identifier` is never a rendered name here (there are two), so it
        // must not resolve; the suffixed forms must.
        assert_eq!(
            resolve_path(&conn, "src/a.go/function_declaration/identifier").unwrap(),
            None
        );
        assert_eq!(resolve_path(&conn, "src/a.go/nope").unwrap(), None);
        assert_eq!(resolve_path(&conn, "missing/a.go").unwrap(), None);
    }

    #[test]
    fn v_node_path_matches_the_rust_renderer_for_every_row() {
        let (conn, _) = display_fixture();
        let mut stmt = conn.prepare("SELECT nid, path FROM v_node_path").unwrap();
        let rows: Vec<(i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(rows.len() >= 6);
        for (nid, view_path) in rows {
            assert_eq!(
                node_path(&conn, nid).unwrap().unwrap(),
                view_path,
                "view and Rust renderer must agree on nid {nid}"
            );
        }
    }

    #[test]
    fn directory_rename_is_one_row_and_re_renders_descendants() {
        let (conn, base) = display_fixture();
        let renamed = intern_name(&conn, "pkg").unwrap();
        let n: usize = conn
            .execute(
                "UPDATE dirs SET name_id = ?1 WHERE dir_id = \
                 (SELECT dir_id FROM dirs d JOIN names n ON n.name_id = d.name_id WHERE n.text = 'src')",
                [renamed],
            )
            .unwrap();
        assert_eq!(n, 1, "a directory rename must be exactly one row");
        // The dir's OWN nodes row still names it via name_id — same rename
        // must apply there for the display layer. Two rows total, k-free.
        let n2: usize = conn
            .execute(
                "UPDATE nodes SET name_id = ?1 WHERE nid = \
                 (SELECT -dir_id FROM dirs d JOIN names n ON n.name_id = d.name_id WHERE n.text = 'pkg')",
                [renamed],
            )
            .unwrap();
        assert_eq!(n2, 1);
        assert_eq!(
            node_path(&conn, base + 2).unwrap().unwrap(),
            "pkg/a.go/function_declaration/identifier_0",
            "descendant paths re-derive from the renamed link"
        );
    }

    #[test]
    fn ensure_dir_nodes_is_idempotent_and_renders() {
        let conn = mem();
        let leaf = ensure_dir_nodes(&conn, "a/b/c.go", 100).unwrap();
        assert_eq!(node_path(&conn, dir_nid(leaf)).unwrap().unwrap(), "a/b");
        // Second call with a different mtime must not disturb existing rows
        // (INSERT OR IGNORE — a reparse must not rewrite other dirs' rows,
        // which is exactly what F4d watches).
        let leaf2 = ensure_dir_nodes(&conn, "a/b/c.go", 999).unwrap();
        assert_eq!(leaf, leaf2);
        let mtime: i64 = conn
            .query_row(
                "SELECT mtime FROM nodes WHERE nid = ?1",
                [dir_nid(leaf)],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mtime, 100, "existing dir rows keep their original mtime");
    }

    /// Review-gate 2 of the identity ladder (bead `ley-line-open-17c271`):
    /// renaming a directory is O(1) rows regardless of subtree size. Under
    /// the pre-v5 path keys, renaming a dir over k descendants rewrote k
    /// ancestry strings (every row's PK plus every referencing column);
    /// under v5 the descendants reference the dir by id, so the rename is
    /// the `dirs` link row plus the dir's own presentation row — two
    /// single-row UPDATEs at k = 100,000 exactly as at k = 3.
    #[test]
    fn directory_rename_is_one_row_at_one_hundred_thousand_descendants() {
        let conn = mem();
        let dir_id = intern_dir_chain(&conn, "big").unwrap();
        let n_big = intern_name(&conn, "big").unwrap();
        let root_name = intern_name(&conn, "").unwrap();
        insert_node(&conn, dir_nid(1), None, Some(root_name), None, 1, 0, 0, 1, "").unwrap();
        insert_node(
            &conn,
            dir_nid(dir_id),
            Some(dir_nid(1)),
            Some(n_big),
            None,
            1,
            0,
            0,
            1,
            "",
        )
        .unwrap();

        // 100 files under big/, 1000 AST rows each = 100,000 descendants.
        let k_id = intern_kind(&conn, "go", "identifier").unwrap();
        conn.execute_batch("BEGIN").unwrap();
        for f in 0..100 {
            let file_id = ensure_file_id(&conn, &format!("big/f{f}.go")).unwrap();
            let base = file_nid(file_id, 0);
            let fname = intern_name(&conn, &format!("f{f}.go")).unwrap();
            insert_node(
                &conn,
                base,
                Some(dir_nid(dir_id)),
                Some(fname),
                Some(k_id),
                1,
                0,
                0,
                1,
                "",
            )
            .unwrap();
            let mut stmt = conn
                .prepare_cached(
                    "INSERT INTO nodes (nid, parent_nid, kind_id, kind, ord, mtime, record) \
                     VALUES (?1, ?2, ?3, 0, ?4, 1, '')",
                )
                .unwrap();
            for i in 1..=999 {
                stmt.execute(params![base + i, base, k_id, i - 1]).unwrap();
            }
        }
        conn.execute_batch("COMMIT").unwrap();
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert!(total >= 100_000, "fixture must hold ≥100k rows; got {total}");

        // Snapshot a descendant's path pre-rename and a checksum over every
        // row EXCEPT the renamed dir's own presentation row.
        let sample = file_nid(lookup_file_id(&conn, "big/f42.go").unwrap().unwrap(), 500);
        assert!(node_path(&conn, sample).unwrap().unwrap().starts_with("big/"));
        let untouched_checksum = |conn: &Connection| -> (i64, i64) {
            conn.query_row(
                "SELECT COUNT(*), COALESCE(SUM(nid * 31 + COALESCE(parent_nid, 0)), 0) \
                 FROM nodes WHERE nid <> ?1",
                [dir_nid(dir_id)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        let before = untouched_checksum(&conn);

        // THE rename: two single-row UPDATEs, k-free.
        let renamed = intern_name(&conn, "renamed").unwrap();
        let n1 = conn
            .execute(
                "UPDATE dirs SET name_id = ?1 WHERE dir_id = ?2",
                params![renamed, dir_id],
            )
            .unwrap();
        let n2 = conn
            .execute(
                "UPDATE nodes SET name_id = ?1 WHERE nid = ?2",
                params![renamed, dir_nid(dir_id)],
            )
            .unwrap();
        assert_eq!(
            (n1, n2),
            (1, 1),
            "a rename over 100k descendants must change exactly one row per \
             surface — the pre-v5 scheme rewrote all 100k"
        );

        assert_eq!(
            untouched_checksum(&conn),
            before,
            "no descendant row may move on a rename"
        );
        assert!(
            node_path(&conn, sample)
                .unwrap()
                .unwrap()
                .starts_with("renamed/"),
            "descendant display paths must re-derive from the renamed link"
        );
    }

    #[test]
    fn insert_node_upserts_in_place() {
        let conn = mem();
        insert_node(&conn, 42, None, None, Some(1), 0, 0, 1, 100, "a").unwrap();
        insert_node(&conn, 42, None, None, Some(1), 0, 0, 9, 200, "b").unwrap();
        let (size, mtime, record): (i64, i64, String) = conn
            .query_row(
                "SELECT size, mtime, record FROM nodes WHERE nid = 42",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((size, mtime, record.as_str()), (9, 200, "b"));
    }

    #[test]
    fn schema_creates_index_and_views() {
        let conn = mem();
        for (ty, name) in [
            ("index", "idx_parent_kind_ord"),
            ("index", "idx_source_file"),
            ("view", "v_node_name"),
            ("view", "v_node_path"),
        ] {
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = ?1 AND name = ?2",
                    params![ty, name],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(exists, "missing {ty}: {name}");
        }
    }
}
