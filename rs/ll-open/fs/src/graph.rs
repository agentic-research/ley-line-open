use anyhow::{Context, Result};
use crossbeam_queue::ArrayQueue;
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
#[cfg(feature = "splice")]
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use leyline_core::mmap::{mmap_read, mmap_write};
use leyline_core::{ArenaHeader, ContentAddressed, Controller};
// Used only by `batch_splice`'s post-reproject invalidation, which needs both
// features; importing it unconditionally would warn on leaner builds.
#[cfg(all(feature = "cdc", feature = "splice"))]
use rusqlite::OptionalExtension;

use crate::SqliteGraph;

/// A node in the filesystem tree (maps 1:1 to a row in the `nodes` table).
#[derive(Debug, Clone)]
pub struct Node {
    pub id: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime_nanos: i64,
}

/// Abstract graph interface for the FUSE layer.
///
/// Read methods are required; write methods default to EROFS.
pub trait Graph: Send + Sync {
    fn get_node(&self, id: &str) -> Result<Option<Node>>;
    fn lookup_child(&self, parent_id: &str, name: &str) -> Result<Option<Node>>;
    fn list_children(&self, parent_id: &str) -> Result<Vec<Node>>;
    fn read_content(&self, id: &str, buf: &mut [u8], offset: u64) -> Result<usize>;

    /// Return all file-node `(id, content)` pairs in one pass.
    ///
    /// Used by the embedding pipeline to avoid N+1 queries. Default
    /// implementation walks the tree via `list_children` + `read_content`;
    /// backends with indexed storage should override with a single query.
    fn all_file_contents(&self) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        let mut stack = vec![String::new()];
        while let Some(parent_id) = stack.pop() {
            for child in self.list_children(&parent_id)? {
                if child.is_dir {
                    stack.push(child.id);
                } else {
                    let size = child.size.max(1) as usize;
                    let mut buf = vec![0u8; size];
                    let n = self.read_content(&child.id, &mut buf, 0)?;
                    buf.truncate(n);
                    if buf.is_empty() {
                        continue;
                    }
                    if let Ok(text) = String::from_utf8(buf)
                        && !text.trim().is_empty()
                    {
                        out.push((child.id, text));
                    }
                }
            }
        }
        Ok(out)
    }

    fn write_content(&self, _id: &str, _data: &[u8], _offset: u64) -> Result<usize> {
        anyhow::bail!("read-only filesystem")
    }
    fn create_node(&self, _parent_id: &str, _name: &str, _is_dir: bool) -> Result<String> {
        anyhow::bail!("read-only filesystem")
    }
    fn remove_node(&self, _id: &str) -> Result<()> {
        anyhow::bail!("read-only filesystem")
    }
    fn truncate(&self, _id: &str) -> Result<()> {
        anyhow::bail!("read-only filesystem")
    }
    fn rename_node(&self, _id: &str, _new_parent_id: &str, _new_name: &str) -> Result<()> {
        anyhow::bail!("read-only filesystem")
    }

    /// Flush pending splice for a node (called on FUSE flush/NFS write completion).
    /// Default no-op; implementations with splice support override.
    fn flush_node(&self, _id: &str) -> Result<()> {
        Ok(())
    }

    /// Batch splice: apply multiple edits atomically (ADR-007 commit path).
    ///
    /// Each edit is `(node_id, Option<new_text>)`:
    /// - `Some(text)` replaces the node's byte range with `text`
    /// - `None` deletes the node (splices byte range with `""`)
    ///
    /// Edits are grouped by source file, checked for byte-range overlaps,
    /// applied bottom-up (highest `start_byte` first), then reprojected.
    fn batch_splice(&self, _edits: &[(String, Option<String>)]) -> Result<()> {
        anyhow::bail!("batch splice not supported")
    }

    fn serialize(&self) -> Result<Vec<u8>> {
        anyhow::bail!("graph does not support serialization")
    }

    fn flush_to_arena(&self) -> Result<()> {
        anyhow::bail!("graph does not support arena flush")
    }
}

/// Wraps [`SqliteGraph`] behind a `Mutex` to satisfy `Send + Sync`.
///
/// Queries the `nodes` table schema defined by `leyline_schema` — since
/// projection-v5 (bead `ley-line-open-17c271`) keyed on an integer `nid`
/// rather than the node's ancestry path:
///
/// ```sql
/// CREATE TABLE nodes (
///     nid INTEGER PRIMARY KEY,   -- (file_id << 24) | ordinal, or -dir_id
///     parent_nid INTEGER,
///     name_id INTEGER,           -- interned; NULL for AST rows
///     kind_id INTEGER,           -- interned; NULL for directories
///     kind INTEGER NOT NULL,     -- 0=file, 1=dir
///     ord INTEGER NOT NULL,
///     size INTEGER DEFAULT 0,
///     mtime INTEGER NOT NULL,
///     record TEXT
/// );
/// ```
///
/// **The [`Graph`] trait boundary keeps STRING ids.** Those ids are mount
/// paths — the FUSE/NFS wire contract — and demoting the path from identity
/// to display (ADR-0034 D6) is a storage change, not a protocol change. Every
/// method therefore translates at the SQL layer: `resolve_path` on the way in,
/// rendered names on the way out. Display names are NOT stored per AST row,
/// so listings join `v_node_name`.
pub struct SqliteGraphAdapter {
    writer: Mutex<SqliteGraph>,
    readers: ArrayQueue<(SqliteGraph, u64)>,
    /// Cached serialized bytes for creating new readers on pool exhaustion.
    reader_bytes: Mutex<Vec<u8>>,
    /// Bumped on each write; readers stamped with stale generations are dropped.
    reader_gen: AtomicU64,
    /// Default tree-sitter language for extensionless files (e.g. `source`).
    #[cfg(feature = "validate")]
    default_language: Option<tree_sitter::Language>,
    /// Shadow copy of content saved on truncate, restored on validation failure.
    /// Key: node ID, Value: old content before truncate.
    #[cfg(feature = "validate")]
    shadow: Mutex<HashMap<String, String>>,
    /// Nodes with pending splice (write accumulated, splice fires on flush).
    #[cfg(feature = "splice")]
    pending_splice: Mutex<HashSet<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteRefreshOutcome {
    #[cfg(not(feature = "cdc"))]
    Disabled,
    #[cfg(feature = "cdc")]
    Skipped,
    #[cfg(feature = "cdc")]
    Full { bytes_scanned: usize },
    #[cfg(feature = "cdc")]
    Incremental {
        prefix_kept: usize,
        tail_reused: usize,
        rehashed: usize,
        bytes_scanned: usize,
    },
}

impl SqliteGraphAdapter {
    pub fn new(graph: SqliteGraph) -> Self {
        Self::build(graph, None)
    }

    /// Create an adapter with a specific reader pool capacity.
    pub fn new_with_pool_size(graph: SqliteGraph, pool_size: usize) -> Self {
        Self::build(graph, Some(pool_size))
    }

    fn build(graph: SqliteGraph, pool_size: Option<usize>) -> Self {
        let bytes = graph.serialize().unwrap_or_default();
        let pool_size = pool_size
            .unwrap_or_else(|| Self::compute_pool_size(bytes.len()))
            .max(1); // ArrayQueue panics on 0
        let readers = ArrayQueue::new(pool_size);
        for _ in 0..pool_size {
            if let Ok(reader) = SqliteGraph::from_bytes(&bytes) {
                let _ = readers.push((reader, 0));
            }
        }
        Self {
            writer: Mutex::new(graph),
            readers,
            reader_bytes: Mutex::new(bytes),
            reader_gen: AtomicU64::new(0),
            #[cfg(feature = "validate")]
            default_language: None,
            #[cfg(feature = "validate")]
            shadow: Mutex::new(HashMap::new()),
            #[cfg(feature = "splice")]
            pending_splice: Mutex::new(HashSet::new()),
        }
    }

    /// Compute reader pool size from DB size.
    /// Target: ~16 MB total reader memory. Min 2, max 8.
    fn compute_pool_size(db_size: usize) -> usize {
        if db_size == 0 {
            return 4;
        }
        let target = (16 * 1024 * 1024) / db_size;
        target.clamp(2, 8)
    }

    /// Set the default tree-sitter language for validation of extensionless files.
    #[cfg(feature = "validate")]
    pub fn set_default_language(&mut self, lang: tree_sitter::Language) {
        self.default_language = Some(lang);
    }

    /// Create a writable adapter from raw bytes (for FUSE write-back).
    pub fn new_writable(data: &[u8]) -> Result<Self> {
        let graph = SqliteGraph::from_bytes_writable(data)?;
        let adapter = Self::new(graph);
        adapter.ensure_errors_table()?;
        Ok(adapter)
    }

    pub fn from_arena(control_path: &Path) -> Result<Self> {
        let graph = SqliteGraph::from_arena(control_path)?;
        Ok(Self::new(graph))
    }

    /// `from_arena` with the load pulled through the verify-on-fault gate
    /// (bead `ley-line-open-b6a4dd`): every 1 KiB page of the arena payload
    /// is proof-verified against `ctrl.current_root` by
    /// [`crate::verified::VerifiedArena`] on its way into SQLite, replacing
    /// the flat whole-buffer hash. Same refusal posture as T2.3: a root
    /// mismatch or a zero-sentinel root over data refuses the load at open;
    /// a page tampered between open and the copy is refused per-page by the
    /// fault gate.
    #[cfg(feature = "verify")]
    pub fn from_arena_verified(control_path: &Path) -> Result<Self> {
        let arena = crate::verified::VerifiedArena::open(control_path)?;
        let graph = SqliteGraph::from_bytes(&arena.read_all()?)?;
        Ok(Self::new(graph))
    }

    /// Writable sibling of [`Self::from_arena_verified`] — the daemon's
    /// write-back mount, loaded through the same per-page gate.
    #[cfg(feature = "verify")]
    pub fn from_arena_writable_verified(control_path: &Path) -> Result<Self> {
        let arena = crate::verified::VerifiedArena::open(control_path)?;
        let graph = SqliteGraph::from_bytes_writable(&arena.read_all()?)?;
        let adapter = Self::new(graph);
        adapter.ensure_errors_table()?;
        Ok(adapter)
    }

    /// Create a writable adapter from an arena (for daemon with write-back).
    ///
    /// **T2.3 verification:** same content-addressed pin as
    /// `SqliteGraph::from_arena` — refuses to load on `current_root`
    /// mismatch. Only `data_size == 0` (fresh arena) skips the hash.
    pub fn from_arena_writable(control_path: &Path) -> Result<Self> {
        let controller = Controller::open_or_create(control_path)?;
        let arena_path = controller.arena_path();

        let file = std::fs::File::open(&arena_path)?;
        let mmap = mmap_read(&file)?;

        let header_slice = &mmap[..std::mem::size_of::<ArenaHeader>()];
        let header: &ArenaHeader = bytemuck::from_bytes(header_slice);

        let file_size = mmap.len() as u64;
        let offset = header
            .validate_header(file_size)
            .context("arena header validation failed")?;
        let buf_size = ArenaHeader::buffer_size(file_size);

        let buf = &mmap[offset as usize..(offset + buf_size) as usize];
        // T2.3: hash buf[..data_size] against current_root, deserialize
        // exactly the verified slice (no padded-suffix asymmetry).
        let verified = crate::verify_arena_root(&controller, header, buf)?;
        let graph = SqliteGraph::from_bytes_writable(verified)?;
        let adapter = Self::new(graph);
        adapter.ensure_errors_table()?;
        Ok(adapter)
    }

    /// Ensure the `_errors` table exists for storing validation errors.
    ///
    /// This table stays keyed by the DISPLAY PATH while the rest of the
    /// projection moves to integer nids (projection-v5). It is not a
    /// projection table — it is owned by this crate and read back by the
    /// mount — and its rows must be able to name a path that has NO node:
    /// `rename_node` records a validation failure against the DESTINATION
    /// path precisely when it refuses to create it, so there is no nid to key
    /// on. Re-keying would force either a fabricated nid or dropping that
    /// diagnostic entirely.
    fn ensure_errors_table(&self) -> Result<()> {
        let guard = self.writer.lock();
        guard.conn().execute_batch(
            "CREATE TABLE IF NOT EXISTS _errors (
                node_id   TEXT PRIMARY KEY,
                line      INTEGER NOT NULL,
                col       INTEGER NOT NULL,
                message   TEXT NOT NULL,
                timestamp INTEGER NOT NULL
            )",
        )?;
        Ok(())
    }

    /// Serialize the current in-memory DB for arena flush.
    pub fn serialize(&self) -> Result<Vec<u8>> {
        let guard = self.writer.lock();
        guard.serialize()
    }

    /// Borrow a reader from the pool, or create one on the fly.
    /// Readers stamped with a stale generation are discarded on pop.
    fn with_reader<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&SqliteGraph) -> Result<R>,
    {
        let current_gen = self.reader_gen.load(Ordering::Acquire);
        let (reader, rgen) = loop {
            match self.readers.pop() {
                Some((r, g)) if g == current_gen => break (r, current_gen),
                Some(_) => continue, // discard stale reader
                None => {
                    let bytes = self.reader_bytes.lock();
                    break (SqliteGraph::from_bytes(&bytes)?, current_gen);
                }
            }
        };
        let result = f(&reader);
        // Only return to pool if still current generation
        if rgen == self.reader_gen.load(Ordering::Acquire) {
            let _ = self.readers.push((reader, rgen));
        }
        result
    }

    /// After a write, bump generation and update cached bytes so new readers
    /// see the mutation. Stale readers are discarded lazily by `with_reader`.
    fn refresh_readers(&self) -> Result<()> {
        let writer = self.writer.lock();
        let bytes = writer.serialize()?;
        self.reader_gen.fetch_add(1, Ordering::Release);
        // Drain is best-effort; stale stragglers are caught by generation check
        while self.readers.pop().is_some() {}
        *self.reader_bytes.lock() = bytes;
        Ok(())
    }

    fn write_content_traced(
        &self,
        id: &str,
        data: &[u8],
        offset: u64,
    ) -> Result<(usize, WriteRefreshOutcome)> {
        let refresh = {
            let guard = self.writer.lock();
            let now = now_nanos();

            // A write to a node the projection cannot resolve wrote nowhere
            // pre-v5 too (`UPDATE ... WHERE id = ?` matched no row) but
            // reported success. Name it instead: every caller reaches here
            // through a lookup or a create, so an unresolvable id is a defect,
            // not a condition to absorb.
            let nid = leyline_schema::resolve_path(guard.conn(), id)?
                .with_context(|| format!("write to unknown node '{id}'"))?;

            // Read existing content, patch in the new data
            let existing: Option<String> = guard
                .conn()
                .query_row("SELECT record FROM nodes WHERE nid = ?1", [nid], |row| {
                    row.get(0)
                })
                .ok()
                .flatten();

            let mut content = existing.map(|s| s.into_bytes()).unwrap_or_default();
            #[cfg(feature = "cdc")]
            let old_len = content.len();
            #[cfg(feature = "cdc")]
            let previous = crate::chunked::capture_chunked_content(guard.conn(), nid)?;
            let off = usize::try_from(offset).context("write offset exceeds usize")?;
            let write_end = off
                .checked_add(data.len())
                .context("write offset + length overflow")?;
            #[cfg(feature = "cdc")]
            let edit_offset = off.min(old_len);
            #[cfg(feature = "cdc")]
            let old_edit_end = write_end.min(old_len);

            // Extend if writing past current end
            if write_end > content.len() {
                content.resize(write_end, 0);
            }
            content[off..write_end].copy_from_slice(data);

            // Validate via tree-sitter if a language is known for this node.
            // Flash-clear pattern: always clear stale error first, then validate.
            #[cfg(feature = "validate")]
            {
                let lang = crate::validate::language_for_node(id, self.default_language.as_ref());
                if let Some(lang) = lang {
                    // Flash clear: remove any previous error for this node
                    guard
                        .conn()
                        .execute(
                            "DELETE FROM _errors WHERE node_id = ?1",
                            rusqlite::params![id],
                        )
                        .ok();

                    if !content.is_empty()
                        && let Err(e) = crate::validate::validate(&content, &lang)
                    {
                        // Write structured error to SQLite
                        guard.conn().execute(
                            "INSERT OR REPLACE INTO _errors (node_id, line, col, message, timestamp) VALUES (?1, ?2, ?3, ?4, ?5)",
                            rusqlite::params![id, e.line, e.column, e.message, now],
                        ).ok();

                        // Restore shadow copy if truncate wiped content before this write
                        if let Some(old) = self.shadow.lock().remove(id) {
                            guard
                                .conn()
                                .execute(
                                    "UPDATE nodes SET record = ?1, size = ?2, mtime = ?3 WHERE nid = ?4",
                                    rusqlite::params![&old, old.len() as i64, now, nid],
                                )
                                .ok();
                            log::info!("restored shadow copy for {id} after validation failure");
                        }

                        log::warn!("validation failed for {id}: {e}");
                        // Drop writer lock and refresh readers so shadow
                        // restore (if any) is visible to subsequent reads.
                        drop(guard);
                        self.refresh_readers()?;
                        return Err(anyhow::anyhow!("{e}"));
                    }

                    // Validation passed — clear shadow (no longer needed)
                    self.shadow.lock().remove(id);
                }
            }

            // Validation passed (or skipped) — commit the write
            let new_str = String::from_utf8_lossy(&content);
            guard.conn().execute(
                "UPDATE nodes SET record = ?1, size = ?2, mtime = ?3 WHERE nid = ?4",
                rusqlite::params![new_str.as_ref(), content.len() as i64, now, nid],
            )?;

            // The edit coordinates were computed against `content` (the raw
            // bytes), but the stored stream is `new_str` — the LOSSY UTF-8
            // conversion of it. When the write landed mid-way through a
            // multi-byte sequence, lossy replacement rewrites bytes OUTSIDE
            // the edit interval, so the coordinates would describe an edit
            // that is not the edit that happened and the incremental path's
            // kept prefix could carry stale hashes (types-friend F6). A
            // truthful Edit exists only when the conversion was the
            // identity; otherwise drop the old manifest and re-chunk in
            // full — correctness costs one full pass exactly when the
            // content stopped being valid UTF-8.
            #[cfg(feature = "cdc")]
            let (previous, edit) = if new_str.as_bytes() == content.as_slice() {
                (
                    previous,
                    leyline_cdc::Edit::parse(edit_offset, old_edit_end, old_len)
                        .context("write-derived edit coordinates")?,
                )
            } else {
                (
                    None,
                    leyline_cdc::Edit::parse(0, old_len, old_len)
                        .context("full-replacement edit coordinates")?,
                )
            };

            // No-op unless this arena already has the chunk schema — writing
            // through a foreign arena must not silently upgrade it.
            #[cfg(feature = "cdc")]
            let refresh = match crate::chunked::refresh_chunked_content_after_edit(
                guard.conn(),
                nid,
                new_str.as_ref().as_bytes(),
                previous,
                edit,
                old_len,
            )? {
                crate::chunked::RefreshOutcome::Skipped => WriteRefreshOutcome::Skipped,
                crate::chunked::RefreshOutcome::Full { bytes_scanned } => {
                    WriteRefreshOutcome::Full { bytes_scanned }
                }
                crate::chunked::RefreshOutcome::Incremental(stats) => {
                    WriteRefreshOutcome::Incremental {
                        prefix_kept: stats.prefix_kept,
                        tail_reused: stats.tail_reused,
                        rehashed: stats.rehashed,
                        bytes_scanned: stats.bytes_scanned,
                    }
                }
            };
            #[cfg(not(feature = "cdc"))]
            let refresh = WriteRefreshOutcome::Disabled;

            // Mark for splice on flush if this node has AST tracking
            #[cfg(feature = "splice")]
            {
                let is_ast: bool = guard
                    .conn()
                    .query_row("SELECT 1 FROM _ast WHERE nid = ?1", [nid], |_| Ok(true))
                    .unwrap_or(false);
                if is_ast {
                    self.pending_splice.lock().insert(id.to_string());
                }
            }
            refresh
        };
        self.refresh_readers()?;
        Ok((data.len(), refresh))
    }

    /// Decode a `(name, kind, size, mtime)` row into a [`Node`] whose `id` is
    /// the display path the caller addressed it by.
    ///
    /// The id is composed from the caller's own parent path rather than
    /// re-derived from the row: `node_path` would walk the whole parent chain
    /// per child, turning one listing into O(children x depth) queries for a
    /// string the caller already holds the prefix of.
    fn row_to_node(
        row: &rusqlite::Row<'_>,
        id: String,
    ) -> std::result::Result<Node, rusqlite::Error> {
        let name: String = row.get("name")?;
        let kind: i64 = row.get("kind")?;
        let size: i64 = row.get("size")?;
        let mtime: i64 = row.get("mtime")?;
        Ok(Node {
            id,
            name,
            is_dir: kind == 1,
            size: size.max(0) as u64,
            mtime_nanos: mtime,
        })
    }

    /// Join a parent display path and a child name into the child's id.
    /// An empty parent is the root, whose children are bare names.
    fn join_id(parent_id: &str, name: &str) -> String {
        if parent_id.is_empty() {
            name.to_string()
        } else {
            format!("{parent_id}/{name}")
        }
    }

    /// Delete `nid`'s `nodes` row and every row beneath it, replacing the
    /// pre-v5 `DELETE ... WHERE id = ?1 OR id LIKE ?2` prefix cascade.
    ///
    /// The three descent shapes mirror
    /// [`crate::chunked::invalidate_chunked_content_subtree`] — a directory
    /// recurses `dirs`, a file root is one nid range, an interior AST node
    /// recurses `parent_nid`. Interning rows in `dirs`/`files` are
    /// deliberately NOT deleted: those tables are append-only, and reusing a
    /// `file_id` would re-bind a dead file's whole nid range to an unrelated
    /// path. A vacated row simply goes stale.
    fn delete_subtree(conn: &rusqlite::Connection, nid: i64) -> Result<()> {
        if let Some(dir_id) = leyline_schema::nid_dir_id(nid) {
            // Every file interned under this directory or any below it, by
            // nid range, plus the directory rows themselves.
            const DESCENDANT_DIRS: &str = "WITH RECURSIVE sub(dir_id) AS ( \
                     SELECT ?1 \
                     UNION ALL \
                     SELECT d.dir_id FROM dirs d JOIN sub s ON d.parent_dir_id = s.dir_id)";
            conn.execute(
                &format!(
                    "DELETE FROM nodes WHERE nid >= 0 AND (nid >> 24) IN (\
                     {DESCENDANT_DIRS} SELECT f.file_id FROM files f JOIN sub s ON f.dir_id = s.dir_id)"
                ),
                rusqlite::params![dir_id],
            )?;
            conn.execute(
                &format!(
                    "DELETE FROM nodes WHERE -nid IN ({DESCENDANT_DIRS} SELECT dir_id FROM sub)"
                ),
                rusqlite::params![dir_id],
            )?;
            return Ok(());
        }
        let ordinal =
            leyline_schema::nid_ordinal(nid).context("non-negative nid has an ordinal")?;
        if ordinal == 0 {
            let file_id =
                leyline_schema::nid_file_id(nid).context("non-negative nid has a file_id")?;
            let (lo, hi) = leyline_schema::file_nid_range(file_id);
            conn.execute(
                "DELETE FROM nodes WHERE nid BETWEEN ?1 AND ?2",
                rusqlite::params![lo, hi],
            )?;
            return Ok(());
        }
        conn.execute(
            "DELETE FROM nodes WHERE nid IN ( \
                 WITH RECURSIVE sub(nid) AS ( \
                     SELECT ?1 \
                     UNION ALL \
                     SELECT n.nid FROM nodes n JOIN sub s ON n.parent_nid = s.nid) \
                 SELECT nid FROM sub)",
            rusqlite::params![nid],
        )?;
        Ok(())
    }
}

impl Graph for SqliteGraphAdapter {
    fn get_node(&self, id: &str) -> Result<Option<Node>> {
        // Root is a synthetic directory
        if id.is_empty() {
            return Ok(Some(Node {
                id: String::new(),
                name: String::new(),
                is_dir: true,
                size: 0,
                mtime_nanos: 0,
            }));
        }
        self.with_reader(|reader| {
            let conn = reader.conn();
            let Some(nid) = leyline_schema::resolve_path(conn, id)? else {
                return Ok(None);
            };
            let result = conn.query_row(
                "SELECT v.name AS name, n.kind, n.size, n.mtime \
                   FROM nodes n JOIN v_node_name v ON v.nid = n.nid \
                  WHERE n.nid = ?1",
                [nid],
                |row| Self::row_to_node(row, id.to_string()),
            );
            match result {
                Ok(node) => Ok(Some(node)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
    }

    fn lookup_child(&self, parent_id: &str, name: &str) -> Result<Option<Node>> {
        // Resolving the JOINED path is the whole lookup: `resolve_path` walks
        // `dirs`/`files` by interned name and falls through to the AST
        // `{raw_kind}[_{k}]` scheme, which is exactly the child-by-name
        // question. Doing it as `parent_nid = ? AND name = ?` would instead
        // render every sibling's display name to compare one of them.
        let id = Self::join_id(parent_id, name);
        self.with_reader(|reader| {
            let conn = reader.conn();
            let Some(nid) = leyline_schema::resolve_path(conn, &id)? else {
                return Ok(None);
            };
            let result = conn.query_row(
                "SELECT v.name AS name, n.kind, n.size, n.mtime \
                   FROM nodes n JOIN v_node_name v ON v.nid = n.nid \
                  WHERE n.nid = ?1",
                [nid],
                |row| Self::row_to_node(row, id.clone()),
            );
            match result {
                Ok(node) => Ok(Some(node)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
    }

    fn list_children(&self, parent_id: &str) -> Result<Vec<Node>> {
        self.with_reader(|reader| {
            let conn = reader.conn();
            // An unresolvable parent lists as empty — the same answer the
            // pre-v5 `WHERE parent_id = ?` gave for an unknown id.
            let Some(parent_nid) = leyline_schema::resolve_path(conn, parent_id)? else {
                return Ok(Vec::new());
            };
            let mut stmt = conn.prepare_cached(
                "SELECT v.name AS name, n.kind, n.size, n.mtime \
                   FROM nodes n JOIN v_node_name v ON v.nid = n.nid \
                  WHERE n.parent_nid = ?1 ORDER BY v.name",
            )?;
            let rows = stmt.query_map([parent_nid], |row| {
                let name: String = row.get("name")?;
                Self::row_to_node(row, Self::join_id(parent_id, &name))
            })?;
            let mut children = Vec::new();
            for row in rows {
                children.push(row?);
            }
            Ok(children)
        })
    }

    fn all_file_contents(&self) -> Result<Vec<(String, String)>> {
        self.with_reader(|reader| {
            // `v_node_path` renders every row's path in one pass. This is the
            // one caller that genuinely wants the whole mapping, which is the
            // case the bulk view exists for — a per-row `node_path` here
            // would re-walk the parent chain once per file.
            let mut stmt = reader.conn().prepare_cached(
                "SELECT p.path, n.record \
                   FROM nodes n JOIN v_node_path p ON p.nid = n.nid \
                  WHERE n.kind = 0 AND n.record IS NOT NULL AND length(n.record) > 0",
            )?;
            let rows = stmt.query_map([], |row| {
                let id: String = row.get(0)?;
                let record: String = row.get(1)?;
                Ok((id, record))
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    /// Serve a byte range.
    ///
    /// With `--features cdc` this delegates to [`crate::chunked::read_content_at`],
    /// which uses the node's chunk manifest when the arena has one and only
    /// touches the chunks overlapping the range. Without the feature — or for
    /// an arena written by another runtime, which has no chunk tables — it is
    /// the `nodes.record` path: load the whole file, return a slice of it.
    ///
    /// Both branches go through one accessor so the two storage generations
    /// cannot drift into different range semantics.
    fn read_content(&self, id: &str, buf: &mut [u8], offset: u64) -> Result<usize> {
        self.with_reader(|reader| {
            // An unresolvable id reads as empty, matching the pre-v5 miss on
            // `WHERE id = ?` — a read of a vanished node is not an error.
            let Some(nid) = leyline_schema::resolve_path(reader.conn(), id)? else {
                return Ok(0);
            };
            #[cfg(feature = "cdc")]
            {
                crate::chunked::read_content_at(reader.conn(), nid, buf, offset)
            }
            #[cfg(not(feature = "cdc"))]
            {
                let record: Option<String> = reader
                    .conn()
                    .query_row("SELECT record FROM nodes WHERE nid = ?1", [nid], |row| {
                        row.get(0)
                    })
                    .ok();
                let Some(data) = record else {
                    return Ok(0);
                };
                let bytes = data.as_bytes();
                let off = offset as usize;
                if off >= bytes.len() {
                    return Ok(0);
                }
                let end = (off + buf.len()).min(bytes.len());
                let n = end - off;
                buf[..n].copy_from_slice(&bytes[off..end]);
                Ok(n)
            }
        })
    }

    fn write_content(&self, id: &str, data: &[u8], offset: u64) -> Result<usize> {
        self.write_content_traced(id, data, offset)
            .map(|(written, _)| written)
    }

    /// Create a mount-visible node, interning it into the projection's
    /// `dirs`/`files` tables so it has a nid to be addressed by.
    ///
    /// Directories land in negative nid space (`-dir_id`); files take ordinal
    /// 0 of their `file_id` range, which is also where a later parse would put
    /// the file's AST root — so a created-then-parsed file keeps one identity.
    fn create_node(&self, parent_id: &str, name: &str, is_dir: bool) -> Result<String> {
        let id = Self::join_id(parent_id, name);
        {
            let guard = self.writer.lock();
            let conn = guard.conn();
            let now = now_nanos();
            let name_id = leyline_schema::intern_name(conn, name)?;

            // `ensure_dir_nodes` interns the chain of its argument's PARENT
            // and materializes a `nodes` row per link, so passing the new
            // node's own path prepares its ancestry either way; the leaf it
            // returns is this node's parent directory.
            let parent_dir_id = leyline_schema::ensure_dir_nodes(conn, &id, now)?;
            let parent_nid = leyline_schema::dir_nid(parent_dir_id);

            let nid = if is_dir {
                conn.execute(
                    "INSERT OR IGNORE INTO dirs (parent_dir_id, name_id) VALUES (?1, ?2)",
                    rusqlite::params![parent_dir_id, name_id],
                )?;
                let dir_id: i64 = conn.query_row(
                    "SELECT dir_id FROM dirs WHERE parent_dir_id = ?1 AND name_id = ?2",
                    rusqlite::params![parent_dir_id, name_id],
                    |r| r.get(0),
                )?;
                leyline_schema::dir_nid(dir_id)
            } else {
                leyline_schema::file_nid(leyline_schema::ensure_file_id(conn, &id)?, 0)
            };

            let kind: i64 = if is_dir { 1 } else { 0 };
            // Raw INSERT rather than `leyline_schema::insert_node`: that helper
            // writes `record` as TEXT, and a fresh node's record must be NULL.
            // The difference is load-bearing — CDC activation selects on
            // `record IS NOT NULL`, so an empty string would enroll a
            // never-written node.
            conn.execute(
                "INSERT INTO nodes (nid, parent_nid, name_id, kind, ord, size, mtime, record) \
                 VALUES (?1, ?2, ?3, ?4, 0, 0, ?5, NULL)",
                rusqlite::params![nid, parent_nid, name_id, kind, now],
            )?;
        }
        self.refresh_readers()?;
        Ok(id)
    }

    fn remove_node(&self, id: &str) -> Result<()> {
        {
            let guard = self.writer.lock();
            let conn = guard.conn();
            // A remove of something that is not there is not an error — the
            // pre-v5 `DELETE ... WHERE id = ?` deleted nothing and returned Ok.
            let Some(nid) = leyline_schema::resolve_path(conn, id)? else {
                return Ok(());
            };

            // Invalidate BEFORE deleting. The manifest has no FK to `nodes`,
            // so deleting the rows would leave it behind — and the subtree
            // descent for an interior AST node walks `nodes.parent_nid`, which
            // the delete would have already erased.
            //
            // The orphan is not merely stale: `files` is append-only, so a
            // file re-created at this path re-binds to the same `file_id` and
            // therefore the same nids, and `has_chunked_content` would serve
            // the DELETED file's bytes to a brand-new, never-written node.
            #[cfg(feature = "cdc")]
            crate::chunked::invalidate_chunked_content_subtree(conn, nid)?;

            Self::delete_subtree(conn, nid)?;
        }
        self.refresh_readers()?;
        Ok(())
    }

    fn truncate(&self, id: &str) -> Result<()> {
        {
            let guard = self.writer.lock();
            let conn = guard.conn();
            let now = now_nanos();
            let Some(nid) = leyline_schema::resolve_path(conn, id)? else {
                return Ok(());
            };

            // Save shadow copy before truncating (for validation rollback)
            #[cfg(feature = "validate")]
            {
                let lang = crate::validate::language_for_node(id, self.default_language.as_ref());
                if lang.is_some() {
                    let old_content: Option<String> = conn
                        .query_row("SELECT record FROM nodes WHERE nid = ?1", [nid], |row| {
                            row.get(0)
                        })
                        .ok()
                        .flatten();
                    if let Some(content) = old_content {
                        self.shadow.lock().insert(id.to_string(), content);
                    }
                }
            }

            conn.execute(
                "UPDATE nodes SET record = NULL, size = 0, mtime = ?1 WHERE nid = ?2",
                rusqlite::params![now, nid],
            )?;

            // Without this, truncate is a silent NO-OP for chunk-backed nodes:
            // `record` becomes NULL but the manifest still describes the old
            // bytes, and the chunked read path keeps serving them.
            #[cfg(feature = "cdc")]
            crate::chunked::invalidate_chunked_content(conn, nid)?;
        }
        self.refresh_readers()?;
        Ok(())
    }

    fn flush_node(&self, id: &str) -> Result<()> {
        #[cfg(feature = "splice")]
        {
            if !self.pending_splice.lock().contains(id) {
                return Ok(());
            }
            let guard = self.writer.lock();
            let Some(nid) = leyline_schema::resolve_path(guard.conn(), id)? else {
                return Ok(());
            };
            let record: Option<String> = guard
                .conn()
                .query_row("SELECT record FROM nodes WHERE nid = ?1", [nid], |r| {
                    r.get(0)
                })
                .ok()
                .flatten();
            let Some(text) = record.filter(|s| !s.is_empty()) else {
                return Ok(());
            };
            leyline_ts::splice::splice_and_reproject(guard.conn(), nid, &text)?;

            // Reproject rewrote `nodes.record` for the spliced node AND every
            // other node of the same source, from inside leyline-ts — which
            // knows nothing about chunk tables. Any manifest still on those
            // nodes now describes the PRE-splice bytes, and the chunked read
            // path would serve them without complaint. Invalidate so those
            // reads fall back to `record`: slower, but correct. Reads
            // re-chunk lazily on the next write through this crate.
            //
            // projection-v5 makes "every node of this source" a nid RANGE —
            // one delete over the file's `(file_id << 24) | ordinal` span,
            // replacing the pre-v5 `_ast` self-join that enumerated ids and
            // invalidated them one at a time. The range holds across the
            // reproject because `files` is append-only: the re-projected file
            // re-binds to the same `file_id`.
            #[cfg(feature = "cdc")]
            {
                let file_id = leyline_schema::nid_file_id(nid)
                    .context("spliced node must be a file-scoped nid")?;
                crate::chunked::invalidate_chunked_content_subtree(
                    guard.conn(),
                    leyline_schema::file_nid(file_id, 0),
                )?;
            }

            // Only remove from pending on success — failed attempts retry on next flush
            self.pending_splice.lock().remove(id);
            // Reproject replaced all nodes — shadows are stale
            #[cfg(feature = "validate")]
            self.shadow.lock().clear();
            drop(guard);
            self.refresh_readers()?;
        }
        let _ = id;
        Ok(())
    }

    #[cfg(feature = "splice")]
    fn batch_splice(&self, edits: &[(String, Option<String>)]) -> Result<()> {
        if edits.is_empty() {
            return Ok(());
        }

        let guard = self.writer.lock();
        let conn = guard.conn();
        let now = now_nanos();

        // Classify edits into AST-tracked (have _ast entry) vs plain data nodes
        struct AstEdit {
            node_id: String,
            text: Option<String>,
            source_id: String,
            start_byte: usize,
            end_byte: usize,
        }

        let mut ast_edits: Vec<AstEdit> = Vec::new();

        for (node_id, text) in edits {
            // A node id that does not resolve has no `_ast` row by
            // construction, so it takes the non-AST arm below — the same
            // place the pre-v5 `QueryReturnedNoRows` sent it.
            let nid = leyline_schema::resolve_path(conn, node_id)?;
            // projection-v5: `_ast` lost its `source_id` column — a node's
            // source IS its file, `nid >> 24`, joined to `_source.file_id`.
            let ast_info = match nid {
                Some(nid) => conn.query_row(
                    "SELECT s.id, a.start_byte, a.end_byte \
                       FROM _ast a JOIN _source s ON s.file_id = (a.nid >> 24) \
                      WHERE a.nid = ?1",
                    [nid],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, i64>(1)? as usize,
                            r.get::<_, i64>(2)? as usize,
                        ))
                    },
                ),
                None => Err(rusqlite::Error::QueryReturnedNoRows),
            };

            match ast_info {
                Ok((source_id, start, end)) => {
                    ast_edits.push(AstEdit {
                        node_id: node_id.clone(),
                        text: text.clone(),
                        source_id,
                        start_byte: start,
                        end_byte: end,
                    });
                }
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    // Non-AST node — direct record update or delete.
                    //
                    // These write `nodes` directly and are NOT covered by the
                    // post-reproject invalidation below: that loop walks
                    // `_ast`-derived node ids, and by definition these nodes
                    // have no `_ast` row. Each arm must invalidate its own
                    // manifest or the read path serves pre-splice bytes (update
                    // arm) or a deleted node's bytes to whatever reuses the
                    // path (delete arm).
                    let Some(nid) = nid else {
                        continue;
                    };
                    match text {
                        Some(t) => {
                            conn.execute(
                                "UPDATE nodes SET record = ?1, size = ?2, mtime = ?3 WHERE nid = ?4",
                                rusqlite::params![t, t.len() as i64, now, nid],
                            )?;
                            #[cfg(feature = "cdc")]
                            crate::chunked::invalidate_chunked_content(conn, nid)?;
                        }
                        None => {
                            // Invalidate first: the subtree descent for an
                            // interior node reads `nodes.parent_nid`, which
                            // the delete is about to erase.
                            #[cfg(feature = "cdc")]
                            crate::chunked::invalidate_chunked_content_subtree(conn, nid)?;
                            Self::delete_subtree(conn, nid)?;
                        }
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }

        if ast_edits.is_empty() {
            drop(guard);
            self.refresh_readers()?;
            return Ok(());
        }

        // Group AST edits by source_id
        let mut groups: HashMap<String, Vec<&AstEdit>> = HashMap::new();
        for edit in &ast_edits {
            groups.entry(edit.source_id.clone()).or_default().push(edit);
        }

        for (source_id, mut group) in groups {
            // Check for byte-range overlaps (parent-child trap)
            for i in 0..group.len() {
                for j in (i + 1)..group.len() {
                    let (si, ei) = (group[i].start_byte, group[i].end_byte);
                    let (sj, ej) = (group[j].start_byte, group[j].end_byte);
                    if (si <= sj && ej <= ei) || (sj <= si && ei <= ej) {
                        anyhow::bail!(
                            "overlapping edits: {} [{}, {}) and {} [{}, {})",
                            group[i].node_id,
                            si,
                            ei,
                            group[j].node_id,
                            sj,
                            ej
                        );
                    }
                }
            }

            // Sort by start_byte DESC (bottom-up: splice later offsets first)
            group.sort_by_key(|a| std::cmp::Reverse(a.start_byte));

            // Read original source
            let source: Vec<u8> = conn
                .query_row(
                    "SELECT content FROM _source WHERE id = ?1",
                    [&source_id],
                    |r| r.get(0),
                )
                .with_context(|| format!("source '{source_id}' not found"))?;

            // Apply splices bottom-up, tracking each edit's post-splice byte range
            struct SpliceRange {
                node_id: String,
                start: usize,
                end: usize,
            }
            let mut modified = source;
            let mut ranges: Vec<SpliceRange> = Vec::new();
            for edit in &group {
                let replacement = edit.text.as_deref().unwrap_or("");
                let mut result = Vec::with_capacity(
                    edit.start_byte + replacement.len() + (modified.len() - edit.end_byte),
                );
                result.extend_from_slice(&modified[..edit.start_byte]);
                result.extend_from_slice(replacement.as_bytes());
                result.extend_from_slice(&modified[edit.end_byte..]);
                ranges.push(SpliceRange {
                    node_id: edit.node_id.clone(),
                    start: edit.start_byte,
                    end: edit.start_byte + replacement.len(),
                });
                modified = result;
            }

            // Validate + reproject (looks up language from _source internally)
            leyline_ts::splice::reproject_source(conn, &source_id, &modified).map_err(|e| {
                // Attempt to attribute the error to a specific node
                let msg = e.to_string();
                // Parse "error at byte N..M" from reproject error
                if let Some(pos) = msg.find("error at byte ") {
                    let rest = &msg[pos + 14..];
                    if let Some(dot_pos) = rest.find("..")
                        && let Ok(err_byte) = rest[..dot_pos].parse::<usize>()
                    {
                        // Find which edit's post-splice range contains the error byte
                        for r in &ranges {
                            if err_byte >= r.start && err_byte < r.end {
                                return anyhow::anyhow!("{e} (attributed to node '{}')", r.node_id);
                            }
                        }
                    }
                }
                // No attribution possible — list all nodes in the group
                let node_ids: Vec<&str> = group.iter().map(|e| e.node_id.as_str()).collect();
                anyhow::anyhow!("{e} (source '{}', edited nodes: {:?})", source_id, node_ids)
            })?;

            // Reproject rewrote `nodes.record` for EVERY node of this source
            // from inside leyline-ts. Unlike the `write_content` path, nothing
            // here re-chunks, so any manifest on those nodes now describes
            // pre-splice bytes and the chunked read path would serve them.
            // Invalidate the whole source: reads fall back to `record` until
            // the next write refreshes them.
            //
            // "The whole source" is now one nid range — `_source.file_id`
            // names the file, and every node of it lives in that file's span.
            #[cfg(feature = "cdc")]
            {
                let file_id: Option<i64> = conn
                    .query_row(
                        "SELECT file_id FROM _source WHERE id = ?1",
                        [&source_id],
                        |r| r.get(0),
                    )
                    .optional()?
                    .flatten();
                if let Some(file_id) = file_id {
                    crate::chunked::invalidate_chunked_content_subtree(
                        conn,
                        leyline_schema::file_nid(file_id, 0),
                    )?;
                }
            }
        }

        // Clear pending splice set
        self.pending_splice.lock().clear();
        #[cfg(feature = "validate")]
        self.shadow.lock().clear();

        drop(guard);
        self.refresh_readers()?;
        Ok(())
    }

    fn serialize(&self) -> Result<Vec<u8>> {
        self.serialize()
    }

    fn rename_node(&self, id: &str, new_parent_id: &str, new_name: &str) -> Result<()> {
        {
            let guard = self.writer.lock();
            let conn = guard.conn();
            let new_id = Self::join_id(new_parent_id, new_name);
            let old_nid = leyline_schema::resolve_path(conn, id)?
                .with_context(|| format!("rename of unknown node '{id}'"))?;

            // Validate content against destination language (catches sed -i pattern:
            // write to temp file → rename over validated file)
            #[cfg(feature = "validate")]
            {
                let dest_lang =
                    crate::validate::language_for_node(&new_id, self.default_language.as_ref());
                if let Some(lang) = dest_lang {
                    let content: Option<String> = conn
                        .query_row(
                            "SELECT record FROM nodes WHERE nid = ?1",
                            [old_nid],
                            |row| row.get(0),
                        )
                        .ok()
                        .flatten();
                    if let Some(ref src) = content
                        && !src.is_empty()
                        && let Err(e) = crate::validate::validate(src.as_bytes(), &lang)
                    {
                        let now = now_nanos();
                        conn.execute(
                            "INSERT OR REPLACE INTO _errors (node_id, line, col, message, timestamp) VALUES (?1, ?2, ?3, ?4, ?5)",
                            rusqlite::params![&new_id, e.line, e.column, e.message, now],
                        ).ok();
                        log::warn!("validation failed on rename to {new_id}: {e}");
                        anyhow::bail!("{e}");
                    }
                }
            }

            // Clear both sides BEFORE moving rows. The manifest has no FK to
            // `nodes`, so a move would leave it keyed to the vacated nids
            // (orphaned on a path a later create can re-bind to — the same
            // leak as `remove_node`), while the destination nids would carry
            // whatever a previous occupant left. The subtree descent also
            // reads `nodes`, which the move is about to rewrite.
            //
            // Invalidate rather than re-key: carrying the manifest across the
            // rename would preserve chunked reads (the content is unchanged),
            // but re-keying onto a nid that already has a manifest violates
            // the `(nid, seq)` primary key. Correctness first; re-keying with
            // conflict handling is an available optimization.
            #[cfg(feature = "cdc")]
            {
                crate::chunked::invalidate_chunked_content_subtree(conn, old_nid)?;
                if let Some(dest) = leyline_schema::resolve_path(conn, &new_id)? {
                    crate::chunked::invalidate_chunked_content_subtree(conn, dest)?;
                }
            }

            let now = now_nanos();
            let new_name_id = leyline_schema::intern_name(conn, new_name)?;
            let new_parent_dir_id = leyline_schema::ensure_dir_nodes(conn, &new_id, now)?;
            let new_parent_nid = leyline_schema::dir_nid(new_parent_dir_id);

            if let Some(dir_id) = leyline_schema::nid_dir_id(old_nid) {
                // THE projection-v5 payoff: a directory rename is two rows for
                // a subtree of any size. Descendants reference the directory
                // by id, not by path, so every path beneath it re-renders from
                // the changed link — where pre-v5 this rewrote one TEXT id per
                // descendant. `dirs` keeps its `UNIQUE(parent_dir_id,
                // name_id)`, so renaming onto an occupied name still conflicts
                // exactly as the pre-v5 primary key did.
                conn.execute(
                    "UPDATE dirs SET parent_dir_id = ?1, name_id = ?2 WHERE dir_id = ?3",
                    rusqlite::params![new_parent_dir_id, new_name_id, dir_id],
                )?;
                conn.execute(
                    "UPDATE nodes SET parent_nid = ?1, name_id = ?2 WHERE nid = ?3",
                    rusqlite::params![new_parent_nid, new_name_id, old_nid],
                )?;
            } else {
                // A file's identity IS `(dir_id, name_id)`, so a renamed file
                // is a different `file_id` and therefore a different nid
                // range. Move the whole range in one statement — the file's
                // own row at ordinal 0 plus every AST node under it — keeping
                // ordinals, and rebase the internal `parent_nid` links that
                // pointed into the old range.
                let old_file_id = leyline_schema::nid_file_id(old_nid)
                    .context("a non-directory nid has a file_id")?;
                let new_file_id = leyline_schema::ensure_file_id(conn, &new_id)?;
                if new_file_id != old_file_id {
                    let (old_lo, old_hi) = leyline_schema::file_nid_range(old_file_id);
                    let new_lo = leyline_schema::file_nid(new_file_id, 0);
                    conn.execute(
                        "UPDATE nodes \
                            SET nid = nid - ?1 + ?3, \
                                parent_nid = CASE \
                                    WHEN parent_nid BETWEEN ?1 AND ?2 \
                                    THEN parent_nid - ?1 + ?3 \
                                    ELSE parent_nid END \
                          WHERE nid BETWEEN ?1 AND ?2",
                        rusqlite::params![old_lo, old_hi, new_lo],
                    )?;
                }
                conn.execute(
                    "UPDATE nodes SET parent_nid = ?1, name_id = ?2 WHERE nid = ?3",
                    rusqlite::params![
                        new_parent_nid,
                        new_name_id,
                        leyline_schema::file_nid(new_file_id, 0)
                    ],
                )?;
            }
        }
        self.refresh_readers()?;
        Ok(())
    }
}

fn now_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as i64
}

/// Thread-safe wrapper that re-opens the inner graph when the control block's
/// `current_root` changes (T2.4 — content-addressed hot-swap on arena update).
///
/// **T2.4 reader-side polling shape.** Pre-T2.4 keyed on `generation`; that
/// public field is gone. Identity is `current_root` (BLAKE3 of arena bytes).
/// Same root → no swap (idempotent re-publish). Different root → swap.
pub struct HotSwapGraph {
    inner: RwLock<Arc<dyn Graph>>,
    control_path: PathBuf,
    last_root: Mutex<[u8; 32]>,
    writable: bool,
    /// Default tree-sitter language for extensionless files (e.g. `source`).
    #[cfg(feature = "validate")]
    default_language: Option<tree_sitter::Language>,
    /// Route every (re)load through the verify-on-fault gate and advance
    /// `current_root` incrementally on flush (bead `ley-line-open-b6a4dd`).
    /// Runtime opt-in via [`HotSwapGraph::with_verify_on_fault`], so the
    /// compiled-in feature changes nothing until a call site asks.
    #[cfg(feature = "verify")]
    verify_on_fault: bool,
    /// Writer half of the outboard seam: the previous flush's payload and
    /// its tree, so the next flush re-hashes only the dirty span instead of
    /// the whole buffer. One extra payload copy — the same order of memory
    /// the adapter's reader-bytes cache already holds.
    #[cfg(feature = "verify")]
    flip_state: Mutex<Option<(Vec<u8>, leyline_core::outboard::Outboard)>>,
}

impl HotSwapGraph {
    pub fn new(control_path: PathBuf) -> Result<Self> {
        let ctrl = Controller::open_or_create(&control_path)?;
        let root = ctrl.current_root();

        // Zero-root sentinel = no data published yet → serve empty graph.
        let initial_graph: Arc<dyn Graph> = if root == [0u8; 32] {
            Arc::new(MemoryGraph::new())
        } else {
            Arc::new(SqliteGraphAdapter::from_arena(&control_path)?)
        };

        Ok(Self {
            inner: RwLock::new(initial_graph),
            control_path,
            last_root: Mutex::new(root),
            writable: false,
            #[cfg(feature = "validate")]
            default_language: None,
            #[cfg(feature = "verify")]
            verify_on_fault: false,
            #[cfg(feature = "verify")]
            flip_state: Mutex::new(None),
        })
    }

    /// Enable verify-on-fault mode (bead `ley-line-open-b6a4dd`): every
    /// (re)load pulls the arena through
    /// [`crate::verified::VerifiedArena`]'s per-page gate, and
    /// [`HotSwapGraph::flush_to_arena`] advances `current_root` via the
    /// outboard's incremental update instead of a full re-hash.
    ///
    /// Re-opens the inner graph through the gate if already loaded.
    /// Unlike `with_writable`'s builder shape, a failed re-open here is an
    /// ERROR, not a silent keep-the-old-graph: swallowing it would leave an
    /// unverified graph serving under a mode that promised the gate — the
    /// exact verify-fallback smell the chunked module documents.
    #[cfg(feature = "verify")]
    pub fn with_verify_on_fault(mut self) -> Result<Self> {
        self.verify_on_fault = true;
        let cached_root = *self.last_root.lock();
        if cached_root != [0u8; 32] {
            let new_graph = self.build_adapter(&self.control_path)?;
            *self.inner.write() = new_graph;
        }
        Ok(self)
    }

    /// Enable writable mode with optional validation language for extensionless files.
    /// Re-opens the inner graph as writable if already loaded.
    #[cfg(feature = "validate")]
    pub fn with_validation(mut self, default_language: Option<tree_sitter::Language>) -> Self {
        self.writable = true;
        self.default_language = default_language;
        let cached_root = *self.last_root.lock();
        if cached_root != [0u8; 32]
            && let Ok(new_graph) = self.build_adapter(&self.control_path)
        {
            *self.inner.write() = new_graph;
        }
        self
    }

    /// Enable writable mode (no validation).
    /// Re-opens the inner graph as writable if already loaded.
    pub fn with_writable(mut self) -> Self {
        self.writable = true;
        let cached_root = *self.last_root.lock();
        if cached_root != [0u8; 32]
            && let Ok(new_graph) = self.build_adapter(&self.control_path)
        {
            *self.inner.write() = new_graph;
        }
        self
    }

    /// Build an adapter for the current root.
    fn build_adapter(&self, control_path: &Path) -> Result<Arc<dyn Graph>> {
        #[cfg(feature = "verify")]
        if self.verify_on_fault {
            #[allow(unused_mut)]
            let mut adapter = if self.writable {
                SqliteGraphAdapter::from_arena_writable_verified(control_path)?
            } else {
                SqliteGraphAdapter::from_arena_verified(control_path)?
            };
            #[cfg(feature = "validate")]
            if self.writable
                && let Some(ref lang) = self.default_language
            {
                adapter.set_default_language(lang.clone());
            }
            return Ok(Arc::new(adapter));
        }
        if self.writable {
            #[allow(unused_mut)]
            let mut adapter = SqliteGraphAdapter::from_arena_writable(control_path)?;
            #[cfg(feature = "validate")]
            if let Some(ref lang) = self.default_language {
                adapter.set_default_language(lang.clone());
            }
            Ok(Arc::new(adapter))
        } else {
            Ok(Arc::new(SqliteGraphAdapter::from_arena(control_path)?))
        }
    }

    /// T2.4: serialize + publish via content-addressed root advance.
    /// Computes BLAKE3 of the serialized bytes, writes them into the
    /// inactive arena buffer, then atomic-publishes the new
    /// `current_root` via `set_arena_with_root`. Polling readers
    /// detect the change by comparing roots.
    pub fn flush_to_arena(&self) -> Result<()> {
        let inner = self.inner.read().clone();
        let bytes = inner.serialize()?;

        let ctrl = Controller::open_or_create(&self.control_path)?;
        let arena_path = ctrl.arena_path();
        let arena_size = ctrl.arena_size();

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&arena_path)?;
        let mut mmap = mmap_write(&file)?;
        leyline_core::layout::write_to_arena(&mut mmap, &bytes)?;

        // σ via the substrate's ContentAddressed impl (Σ §3.4 locks BLAKE3).
        // Retrofitted from inline `blake3::hash` per bead
        // `ley-line-open-32201a`. graph.rs:1449's blake3::hash call stays
        // inline — that's a #[test] oracle, not a production bypass.
        //
        // Verify-on-fault mode advances the root through the outboard tree
        // instead (bead `ley-line-open-b6a4dd`): same BLAKE3 root, bit for
        // bit (the outboard module's pinned identity), but the flip
        // re-hashes only the span this flush actually changed.
        #[cfg(feature = "verify")]
        let new_root: [u8; 32] = if self.verify_on_fault {
            self.advance_root_incrementally(&bytes)
        } else {
            *bytes.as_slice().hash().as_bytes()
        };
        #[cfg(not(feature = "verify"))]
        let new_root: [u8; 32] = *bytes.as_slice().hash().as_bytes();
        let mut ctrl = Controller::open_or_create(&self.control_path)?;
        ctrl.set_arena_with_root(&arena_path, arena_size, new_root)?;

        // Acknowledge our own publish without re-opening.
        *self.last_root.lock() = new_root;
        log::info!(
            "arena flush: root advanced to {} ({} bytes)",
            hex_short(&new_root),
            bytes.len()
        );
        Ok(())
    }

    /// The writer half of the outboard seam (bead `ley-line-open-b6a4dd`):
    /// maintain the tree across flushes so `current_root` advances by
    /// re-hashing only the dirty span. First flush of a session builds the
    /// tree (O(n) — the cost the flat hash paid anyway); every later flush
    /// pays `Outboard::update` over `dirty_span(prev, next)` plus O(log n)
    /// merges. The result is bit-identical to `blake3::hash(bytes)` — the
    /// outboard module pins that identity, and the verified-writer test
    /// here re-checks it against the T2.3 loader as an independent oracle.
    #[cfg(feature = "verify")]
    fn advance_root_incrementally(&self, bytes: &[u8]) -> [u8; 32] {
        use leyline_core::outboard::Outboard;
        let mut state = self.flip_state.lock();
        let outboard = match state.take() {
            Some((prev, mut outboard)) => {
                let dirty = crate::verified::dirty_span(&prev, bytes);
                match outboard.update(bytes, dirty) {
                    Ok(_) => outboard,
                    Err(e) => {
                        // dirty_span is total over its inputs, so update's
                        // range check cannot fire here — but if it ever
                        // does, a half-updated tree must not become the
                        // published root. Rebuild from scratch: correct by
                        // construction, merely un-incremental.
                        log::warn!("incremental root update refused ({e:#}); rebuilding tree");
                        Outboard::build(bytes)
                    }
                }
            }
            None => Outboard::build(bytes),
        };
        let root = *outboard.root().as_bytes();
        *state = Some((bytes.to_vec(), outboard));
        root
    }

    /// T2.4: poll `current_root`; swap if it differs from cached.
    /// Same root → no-op (idempotent re-publish, or no change since
    /// last poll). Different root → reload via `from_arena` (which
    /// internally verifies BLAKE3 — see T2.3).
    fn maybe_swap(&self) -> Result<Arc<dyn Graph>> {
        let ctrl = Controller::open_or_create(&self.control_path)?;
        let current_root = ctrl.current_root();
        let cached_root = *self.last_root.lock();

        if current_root != cached_root {
            // Zero-root sentinel = no data; serve empty graph.
            let new_graph: Arc<dyn Graph> = if current_root == [0u8; 32] {
                Arc::new(MemoryGraph::new())
            } else {
                self.build_adapter(&self.control_path)?
            };
            let mut w = self.inner.write();
            *w = new_graph.clone();
            *self.last_root.lock() = current_root;
            Ok(new_graph)
        } else {
            Ok(self.inner.read().clone())
        }
    }
}

/// 8-character hex prefix for log lines.
fn hex_short(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(8);
    use std::fmt::Write;
    for b in &bytes[..4] {
        let _ = write!(s, "{b:02x}");
    }
    s
}

impl Graph for HotSwapGraph {
    fn get_node(&self, id: &str) -> Result<Option<Node>> {
        self.maybe_swap()?.get_node(id)
    }

    fn lookup_child(&self, parent_id: &str, name: &str) -> Result<Option<Node>> {
        self.maybe_swap()?.lookup_child(parent_id, name)
    }

    fn list_children(&self, parent_id: &str) -> Result<Vec<Node>> {
        self.maybe_swap()?.list_children(parent_id)
    }

    fn read_content(&self, id: &str, buf: &mut [u8], offset: u64) -> Result<usize> {
        self.maybe_swap()?.read_content(id, buf, offset)
    }

    fn write_content(&self, id: &str, data: &[u8], offset: u64) -> Result<usize> {
        self.maybe_swap()?.write_content(id, data, offset)
    }

    fn create_node(&self, parent_id: &str, name: &str, is_dir: bool) -> Result<String> {
        self.maybe_swap()?.create_node(parent_id, name, is_dir)
    }

    fn remove_node(&self, id: &str) -> Result<()> {
        self.maybe_swap()?.remove_node(id)
    }

    fn truncate(&self, id: &str) -> Result<()> {
        self.maybe_swap()?.truncate(id)
    }

    fn rename_node(&self, id: &str, new_parent_id: &str, new_name: &str) -> Result<()> {
        self.maybe_swap()?.rename_node(id, new_parent_id, new_name)
    }

    fn flush_node(&self, id: &str) -> Result<()> {
        self.maybe_swap()?.flush_node(id)
    }

    fn batch_splice(&self, edits: &[(String, Option<String>)]) -> Result<()> {
        self.maybe_swap()?.batch_splice(edits)
    }

    fn serialize(&self) -> Result<Vec<u8>> {
        self.inner.read().serialize()
    }

    fn flush_to_arena(&self) -> Result<()> {
        HotSwapGraph::flush_to_arena(self)
    }
}

/// In-memory graph for unit tests (no arena/SQLite needed).
pub struct MemoryGraph {
    nodes: HashMap<String, Node>,
    /// parent_id -> list of child node IDs
    children: HashMap<String, Vec<String>>,
    /// node ID -> content bytes
    content: Mutex<HashMap<String, Vec<u8>>>,
}

impl Default for MemoryGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryGraph {
    pub fn new() -> Self {
        let mut nodes = HashMap::new();
        // Synthetic root directory — matches SqliteGraphAdapter behavior
        nodes.insert(
            String::new(),
            Node {
                id: String::new(),
                name: String::new(),
                is_dir: true,
                size: 0,
                mtime_nanos: 0,
            },
        );
        Self {
            nodes,
            children: HashMap::new(),
            content: Mutex::new(HashMap::new()),
        }
    }

    pub fn add_node(&mut self, node: Node, parent_id: &str, content: Option<Vec<u8>>) {
        let id = node.id.clone();
        self.nodes.insert(id.clone(), node);
        // Don't register root as its own child
        if id != parent_id {
            self.children
                .entry(parent_id.to_string())
                .or_default()
                .push(id.clone());
        }
        if let Some(data) = content {
            self.content.lock().insert(id, data);
        }
    }
}

impl Graph for MemoryGraph {
    fn get_node(&self, id: &str) -> Result<Option<Node>> {
        Ok(self.nodes.get(id).cloned())
    }

    fn lookup_child(&self, parent_id: &str, name: &str) -> Result<Option<Node>> {
        let Some(child_ids) = self.children.get(parent_id) else {
            return Ok(None);
        };
        for cid in child_ids {
            if let Some(node) = self.nodes.get(cid)
                && node.name == name
            {
                return Ok(Some(node.clone()));
            }
        }
        Ok(None)
    }

    fn list_children(&self, parent_id: &str) -> Result<Vec<Node>> {
        let Some(child_ids) = self.children.get(parent_id) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for cid in child_ids {
            if let Some(node) = self.nodes.get(cid) {
                out.push(node.clone());
            }
        }
        Ok(out)
    }

    fn read_content(&self, id: &str, buf: &mut [u8], offset: u64) -> Result<usize> {
        let guard = self.content.lock();
        let Some(data) = guard.get(id) else {
            return Ok(0);
        };
        let off = offset as usize;
        if off >= data.len() {
            return Ok(0);
        }
        let end = (off + buf.len()).min(data.len());
        let n = end - off;
        buf[..n].copy_from_slice(&data[off..end]);
        Ok(n)
    }
}

/// Fixture builders shared by this crate's tests.
///
/// A node's PATH is not stored under projection-v5, so a fixture has to
/// intern the components that render it — which is more than an
/// `INSERT INTO nodes` and is worth writing exactly once.
#[cfg(test)]
pub(crate) mod fixtures {
    // Which fixtures a build needs depends on its feature set; an
    // unused one here is a feature combination, not dead weight.
    #![allow(dead_code)]

    use super::*;
    use rusqlite::Connection;

    /// The nid a display path resolves to. Tests that reach past the [`Graph`]
    /// trait into the nid-keyed chunk layer translate here, exactly as the
    /// trait methods do.
    pub(crate) fn nid_of(conn: &Connection, path: &str) -> i64 {
        leyline_schema::resolve_path(conn, path)
            .unwrap()
            .unwrap_or_else(|| panic!("fixture path {path:?} must resolve"))
    }

    /// Insert a directory node at `path`, interning its whole ancestry.
    /// Returns its nid.
    ///
    /// The projection-v5 shape of what a fixture used to write as one
    /// `INSERT INTO nodes (id, name, ...)`: a node's PATH is no longer stored,
    /// so a fixture has to intern the components that render it.
    pub(crate) fn put_dir(conn: &Connection, path: &str, mtime: i64) -> Result<i64> {
        let name = path.rsplit('/').next().unwrap_or(path);
        let name_id = leyline_schema::intern_name(conn, name)?;
        let parent_dir_id = leyline_schema::ensure_dir_nodes(conn, path, mtime)?;
        conn.execute(
            "INSERT OR IGNORE INTO dirs (parent_dir_id, name_id) VALUES (?1, ?2)",
            rusqlite::params![parent_dir_id, name_id],
        )?;
        let dir_id: i64 = conn.query_row(
            "SELECT dir_id FROM dirs WHERE parent_dir_id = ?1 AND name_id = ?2",
            rusqlite::params![parent_dir_id, name_id],
            |r| r.get(0),
        )?;
        let nid = leyline_schema::dir_nid(dir_id);
        conn.execute(
        "INSERT OR REPLACE INTO nodes (nid, parent_nid, name_id, kind, ord, size, mtime, record) \
         VALUES (?1, ?2, ?3, 1, 0, 0, ?4, NULL)",
        rusqlite::params![nid, leyline_schema::dir_nid(parent_dir_id), name_id, mtime],
    )?;
        Ok(nid)
    }

    /// Insert a file node at `path` carrying `record` (`None` writes a NULL
    /// record, which is what an unwritten node has and what CDC activation
    /// skips on). Returns its nid.
    pub(crate) fn put_file(
        conn: &Connection,
        path: &str,
        mtime: i64,
        record: Option<&str>,
    ) -> Result<i64> {
        let name = path.rsplit('/').next().unwrap_or(path);
        let name_id = leyline_schema::intern_name(conn, name)?;
        let parent_dir_id = leyline_schema::ensure_dir_nodes(conn, path, mtime)?;
        let file_id = leyline_schema::ensure_file_id(conn, path)?;
        let nid = leyline_schema::file_nid(file_id, 0);
        conn.execute(
        "INSERT OR REPLACE INTO nodes (nid, parent_nid, name_id, kind, ord, size, mtime, record) \
         VALUES (?1, ?2, ?3, 0, 0, ?4, ?5, ?6)",
        rusqlite::params![
            nid,
            leyline_schema::dir_nid(parent_dir_id),
            name_id,
            record.map(|r| r.len() as i64).unwrap_or(0),
            mtime,
            record
        ],
    )?;
        Ok(nid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `nid_of` translates a display path for the nid-keyed chunk layer,
    // which only the `cdc` tests reach into.
    #[cfg(feature = "cdc")]
    use crate::graph::fixtures::nid_of;
    use crate::graph::fixtures::{put_dir, put_file};
    use leyline_schema::create_schema;
    use rusqlite::Connection;

    #[test]
    fn memory_graph_round_trip() {
        let mut g = MemoryGraph::new();

        // Root directory
        g.add_node(
            Node {
                id: "".into(),
                name: "".into(),
                is_dir: true,
                size: 0,
                mtime_nanos: 0,
            },
            "",
            None,
        );

        // A child file
        g.add_node(
            Node {
                id: "file1".into(),
                name: "file1".into(),
                is_dir: false,
                size: 5,
                mtime_nanos: 1000,
            },
            "",
            Some(b"hello".to_vec()),
        );

        // A child dir
        g.add_node(
            Node {
                id: "subdir".into(),
                name: "subdir".into(),
                is_dir: true,
                size: 0,
                mtime_nanos: 2000,
            },
            "",
            None,
        );

        let root = g.get_node("").unwrap().unwrap();
        assert!(root.is_dir);

        let children = g.list_children("").unwrap();
        assert_eq!(children.len(), 2);

        let found = g.lookup_child("", "file1").unwrap().unwrap();
        assert_eq!(found.id, "file1");
        assert!(!found.is_dir);

        let mut buf = [0u8; 64];
        let n = g.read_content("file1", &mut buf, 0).unwrap();
        assert_eq!(&buf[..n], b"hello");

        // Offset read
        let n = g.read_content("file1", &mut buf, 3).unwrap();
        assert_eq!(&buf[..n], b"lo");

        // Missing node
        assert!(g.get_node("nope").unwrap().is_none());
        assert!(g.lookup_child("", "nope").unwrap().is_none());
    }

    #[test]
    fn sqlite_adapter_round_trip() -> Result<()> {
        let source = Connection::open_in_memory()?;
        create_schema(&source)?;
        put_dir(&source, "vulns", 1000)?;
        put_dir(&source, "vulns/CVE-2024-0001", 2000)?;
        put_file(
            &source,
            "vulns/CVE-2024-0001/source",
            3000,
            Some("{\"severity\":\"critical\"}"),
        )?;

        let data = source.serialize("main")?;
        let graph = SqliteGraph::from_bytes(data.as_ref())?;
        let adapter = SqliteGraphAdapter::new(graph);

        // Root is synthetic
        let root = adapter.get_node("")?.unwrap();
        assert!(root.is_dir);

        // Lookup by ID
        let vulns = adapter.get_node("vulns")?.unwrap();
        assert!(vulns.is_dir);
        assert_eq!(vulns.name, "vulns");

        // List children of root
        let root_children = adapter.list_children("")?;
        assert_eq!(root_children.len(), 1);
        assert_eq!(root_children[0].id, "vulns");

        // Lookup child by name
        let child = adapter.lookup_child("vulns", "CVE-2024-0001")?.unwrap();
        assert_eq!(child.id, "vulns/CVE-2024-0001");
        assert!(child.is_dir);

        // Read file content (record column)
        let leaf = adapter.get_node("vulns/CVE-2024-0001/source")?.unwrap();
        assert!(!leaf.is_dir);
        // `size` tracks the record's byte length — the invariant every writer
        // maintains and CDC activation refuses to proceed without.
        assert_eq!(leaf.size, 23);

        let mut buf = [0u8; 256];
        let n = adapter.read_content("vulns/CVE-2024-0001/source", &mut buf, 0)?;
        let content = std::str::from_utf8(&buf[..n])?;
        assert!(content.contains("critical"));

        // Missing node
        assert!(adapter.get_node("nope")?.is_none());

        Ok(())
    }

    /// Helper: create a writable adapter with a `nodes` table for write tests.
    fn writable_adapter() -> Result<SqliteGraphAdapter> {
        let source = Connection::open_in_memory()?;
        create_schema(&source)?;
        put_dir(&source, "docs", 1000)?;
        put_file(&source, "docs/readme", 2000, Some("hello"))?;
        let data = source.serialize("main")?;
        SqliteGraphAdapter::new_writable(data.as_ref())
    }

    #[test]
    fn write_content_updates_record() -> Result<()> {
        let adapter = writable_adapter()?;

        // Write new content
        let n = adapter.write_content("docs/readme", b"world", 0)?;
        assert_eq!(n, 5);

        // Read it back
        let mut buf = [0u8; 64];
        let n = adapter.read_content("docs/readme", &mut buf, 0)?;
        assert_eq!(&buf[..n], b"world");

        // Size updated
        let node = adapter.get_node("docs/readme")?.unwrap();
        assert_eq!(node.size, 5);

        Ok(())
    }

    #[test]
    fn write_content_with_offset() -> Result<()> {
        let adapter = writable_adapter()?;

        // Write at offset — extends content
        let n = adapter.write_content("docs/readme", b"XY", 3)?;
        assert_eq!(n, 2);

        let mut buf = [0u8; 64];
        let n = adapter.read_content("docs/readme", &mut buf, 0)?;
        assert_eq!(&buf[..n], b"helXY");

        Ok(())
    }

    #[test]
    fn create_and_remove_node() -> Result<()> {
        let adapter = writable_adapter()?;

        // Create a file
        let id = adapter.create_node("docs", "notes.txt", false)?;
        assert_eq!(id, "docs/notes.txt");

        let node = adapter.get_node("docs/notes.txt")?.unwrap();
        assert!(!node.is_dir);
        assert_eq!(node.name, "notes.txt");

        // Visible as child
        let children = adapter.list_children("docs")?;
        assert!(children.iter().any(|c| c.id == "docs/notes.txt"));

        // Create a dir at root
        let id2 = adapter.create_node("", "src", true)?;
        assert_eq!(id2, "src");
        assert!(adapter.get_node("src")?.unwrap().is_dir);

        // Remove the file
        adapter.remove_node("docs/notes.txt")?;
        assert!(adapter.get_node("docs/notes.txt")?.is_none());

        Ok(())
    }

    #[test]
    fn truncate_clears_content() -> Result<()> {
        let adapter = writable_adapter()?;

        // Verify content exists
        let mut buf = [0u8; 64];
        let n = adapter.read_content("docs/readme", &mut buf, 0)?;
        assert_eq!(n, 5);

        // Truncate
        adapter.truncate("docs/readme")?;

        // Content gone, size 0
        let n = adapter.read_content("docs/readme", &mut buf, 0)?;
        assert_eq!(n, 0);
        let node = adapter.get_node("docs/readme")?.unwrap();
        assert_eq!(node.size, 0);

        Ok(())
    }

    #[test]
    fn rename_node_cascades_children() -> Result<()> {
        let adapter = writable_adapter()?;

        // Starting state: docs/ contains docs/readme
        assert!(adapter.get_node("docs/readme")?.is_some());

        // Rename "docs" → "notes" under root
        adapter.rename_node("docs", "", "notes")?;

        // Old IDs gone
        assert!(adapter.get_node("docs")?.is_none());
        assert!(adapter.get_node("docs/readme")?.is_none());

        // New IDs present
        let notes = adapter.get_node("notes")?.unwrap();
        assert!(notes.is_dir);
        assert_eq!(notes.name, "notes");

        let readme = adapter.get_node("notes/readme")?.unwrap();
        assert_eq!(readme.name, "readme");
        assert!(!readme.is_dir);

        // Child's parent_id updated
        let children = adapter.list_children("notes")?;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id, "notes/readme");

        // Content still readable
        let mut buf = [0u8; 64];
        let n = adapter.read_content("notes/readme", &mut buf, 0)?;
        assert_eq!(&buf[..n], b"hello");

        Ok(())
    }

    /// T2.4: HotSwapGraph at zero-root sentinel serves empty graph;
    /// after a publish (set_arena_with_root with non-zero root), next
    /// query hot-swaps to the real arena data. Same test as before
    /// the breaking version bump, just keyed on root not generation.
    #[test]
    fn hotswap_zero_root_serves_empty_then_swaps_on_publish() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let ctrl_path = dir.path().join("test.ctrl");
        let arena_path = dir.path().join("test.arena");

        // Create control block with arena path set, zero root sentinel
        // The projection carries its interning tables, indexes, and display
        // views alongside `nodes`, so even a two-node fixture serializes past
        // 32 KiB. Sized off the page count the schema actually needs.
        let arena_size: u64 = 4096 + 131_072 * 2;
        let mut ctrl = Controller::open_or_create(&ctrl_path)?;
        ctrl.set_arena(arena_path.to_str().unwrap(), arena_size)?;
        let _mmap = leyline_core::layout::create_arena(&arena_path, arena_size)?;

        // HotSwapGraph at zero-root sentinel should serve empty root
        let graph = HotSwapGraph::new(ctrl_path.clone())?;
        let root = graph.get_node("")?.unwrap();
        assert!(root.is_dir);
        let children = graph.list_children("")?;
        assert!(children.is_empty());

        // Now publish real data
        let source = Connection::open_in_memory()?;
        create_schema(&source)?;
        put_dir(&source, "docs", 1000)?;
        put_file(&source, "docs/readme", 2000, Some("hello"))?;
        let db_bytes = source.serialize("main")?;

        // Write db to arena
        let mut mmap = leyline_core::layout::create_arena(&arena_path, arena_size)?;
        leyline_core::layout::write_to_arena(&mut mmap, db_bytes.as_ref())?;

        // Publish via set_arena_with_root — root advances away from sentinel
        let mut ctrl = Controller::open_or_create(&ctrl_path)?;
        let new_root: [u8; 32] = blake3::hash(db_bytes.as_ref()).into();
        ctrl.set_arena_with_root(arena_path.to_str().unwrap(), arena_size, new_root)?;

        // Next query should hot-swap to real data
        let children = graph.list_children("")?;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].name, "docs");

        let mut buf = [0u8; 64];
        let n = graph.read_content("docs/readme", &mut buf, 0)?;
        assert_eq!(&buf[..n], b"hello");

        Ok(())
    }

    #[test]
    fn extra_columns_safe() -> Result<()> {
        // Mache's nodes table has `record_id TEXT` and `source_file TEXT` columns.
        // Verify SqliteGraphAdapter queries work with the full shared schema.
        let source = Connection::open_in_memory()?;
        create_schema(&source)?;
        put_dir(&source, "funcs", 1000)?;
        let validate = put_dir(&source, "funcs/Validate", 2000)?;
        let sources = put_file(
            &source,
            "funcs/Validate/source",
            3000,
            Some("func Validate(){}"),
        )?;
        // The columns this test is about: mache's lazy-resolution flow writes
        // both, ley-line's own writers leave both NULL.
        source.execute(
            "UPDATE nodes SET record_id = 'rec-1' WHERE nid = ?1",
            [validate],
        )?;
        source.execute(
            "UPDATE nodes SET source_file = 'validate.go' WHERE nid = ?1",
            [sources],
        )?;

        let data = source.serialize("main")?;
        let graph = SqliteGraph::from_bytes(data.as_ref())?;
        let adapter = SqliteGraphAdapter::new(graph);

        // All standard queries should work despite extra `record_id` column
        let node = adapter.get_node("funcs/Validate")?.unwrap();
        assert!(node.is_dir);
        assert_eq!(node.name, "Validate");

        let children = adapter.list_children("funcs")?;
        assert_eq!(children.len(), 1);

        let child = adapter.lookup_child("funcs", "Validate")?.unwrap();
        assert_eq!(child.id, "funcs/Validate");

        let mut buf = [0u8; 256];
        let n = adapter.read_content("funcs/Validate/source", &mut buf, 0)?;
        assert_eq!(&buf[..n], b"func Validate(){}");

        Ok(())
    }

    #[test]
    fn mtime_nanoseconds() -> Result<()> {
        // Go's time.UnixNano() returns int64 nanoseconds.
        // Verify large nanosecond values survive the round trip.
        let go_mtime: i64 = 1_700_000_000_000_000_000; // ~2023 in nanos

        let source = Connection::open_in_memory()?;
        create_schema(&source)?;
        put_file(&source, "f", go_mtime, None)?;

        let data = source.serialize("main")?;
        let graph = SqliteGraph::from_bytes(data.as_ref())?;
        let adapter = SqliteGraphAdapter::new(graph);

        let node = adapter.get_node("f")?.unwrap();
        assert_eq!(node.mtime_nanos, go_mtime);

        Ok(())
    }

    #[test]
    fn serialize_round_trip() -> Result<()> {
        let adapter = writable_adapter()?;

        // Write some content
        adapter.write_content("docs/readme", b"modified", 0)?;
        adapter.create_node("docs", "new.txt", false)?;

        // Serialize and re-open
        let bytes = adapter.serialize()?;
        let adapter2 = SqliteGraphAdapter::new_writable(&bytes)?;

        // Verify writes survived
        let mut buf = [0u8; 64];
        let n = adapter2.read_content("docs/readme", &mut buf, 0)?;
        assert_eq!(&buf[..n], b"modified");

        let node = adapter2.get_node("docs/new.txt")?.unwrap();
        assert_eq!(node.name, "new.txt");

        Ok(())
    }

    /// Helper: create a writable adapter with Go source files for validation tests.
    #[cfg(feature = "validate")]
    fn writable_go_adapter() -> Result<SqliteGraphAdapter> {
        let source = Connection::open_in_memory()?;
        create_schema(&source)?;
        let go_src = "package main\n\nfunc main() {\n}\n";
        put_dir(&source, "functions", 1000)?;
        put_dir(&source, "functions/main", 2000)?;
        put_file(&source, "functions/main/source", 3000, Some(go_src))?;
        put_dir(&source, "docs", 1000)?;
        put_file(&source, "docs/readme.txt", 2000, Some("hello"))?;
        put_dir(&source, "src", 1000)?;
        put_file(&source, "src/main.go", 3000, Some(go_src))?;
        let data = source.serialize("main")?;
        let mut adapter = SqliteGraphAdapter::new_writable(data.as_ref())?;
        let go_lang: tree_sitter::Language = tree_sitter_go::LANGUAGE.into();
        adapter.set_default_language(go_lang);
        Ok(adapter)
    }

    #[cfg(feature = "validate")]
    #[test]
    fn write_valid_code_clears_error() -> Result<()> {
        let adapter = writable_go_adapter()?;

        // Write valid Go — should succeed and no error in _errors
        let valid = b"package main\n\nfunc main() {\n\tprintln(\"hello\")\n}\n";
        adapter.write_content("functions/main/source", valid, 0)?;

        // Verify content updated
        let mut buf = [0u8; 256];
        let n = adapter.read_content("functions/main/source", &mut buf, 0)?;
        assert_eq!(&buf[..n], valid.as_slice());

        // Verify no error stored
        let guard = adapter.writer.lock();
        let count: i64 = guard.conn().query_row(
            "SELECT COUNT(*) FROM _errors WHERE node_id = ?1",
            ["functions/main/source"],
            |row| row.get(0),
        )?;
        assert_eq!(count, 0, "no error should be stored for valid write");

        Ok(())
    }

    #[cfg(feature = "validate")]
    #[test]
    fn write_invalid_code_stores_error() -> Result<()> {
        let adapter = writable_go_adapter()?;

        // Write broken Go — should fail
        let invalid = b"package main\n\nfunc {{{ bad\n";
        let result = adapter.write_content("functions/main/source", invalid, 0);
        assert!(result.is_err(), "write should fail for invalid Go");

        // Verify content was NOT updated (old content preserved)
        let mut buf = [0u8; 256];
        let n = adapter.read_content("functions/main/source", &mut buf, 0)?;
        let content = std::str::from_utf8(&buf[..n])?;
        assert!(
            content.contains("package main"),
            "old content should be preserved"
        );

        // Verify structured error stored in _errors table
        let guard = adapter.writer.lock();
        let (line, _col, message): (i64, i64, String) = guard.conn().query_row(
            "SELECT line, col, message FROM _errors WHERE node_id = ?1",
            ["functions/main/source"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert!(line >= 2, "error should be on line >= 2, got {line}");
        assert_eq!(message, "syntax error");
        drop(guard);

        // Now write valid code — error should be cleared
        let valid = b"package main\n\nfunc main() {\n}\n";
        adapter.write_content("functions/main/source", valid, 0)?;

        let guard = adapter.writer.lock();
        let count: i64 = guard.conn().query_row(
            "SELECT COUNT(*) FROM _errors WHERE node_id = ?1",
            ["functions/main/source"],
            |row| row.get(0),
        )?;
        assert_eq!(count, 0, "error should be cleared after valid write");

        Ok(())
    }

    #[cfg(feature = "validate")]
    #[test]
    fn write_skips_validation_unknown_extension() -> Result<()> {
        let adapter = writable_go_adapter()?;

        // .txt file — no tree-sitter language, should pass without validation
        let garbage = b"this is not valid code in any language {{{!!!";
        adapter.write_content("docs/readme.txt", garbage, 0)?;

        // Verify content updated (no validation blocked it)
        let mut buf = [0u8; 256];
        let n = adapter.read_content("docs/readme.txt", &mut buf, 0)?;
        assert_eq!(&buf[..n], garbage.as_slice());

        Ok(())
    }

    #[cfg(feature = "validate")]
    #[test]
    fn write_uses_fallback_language_for_extensionless() -> Result<()> {
        let adapter = writable_go_adapter()?;

        // "source" has no extension — should use default_language (Go)
        let invalid = b"func {{{ totally broken";
        let result = adapter.write_content("functions/main/source", invalid, 0);
        assert!(
            result.is_err(),
            "extensionless file should be validated via fallback"
        );

        Ok(())
    }

    #[cfg(feature = "validate")]
    #[test]
    fn write_validates_by_extension() -> Result<()> {
        let adapter = writable_go_adapter()?;

        // main.go has .go extension — should validate as Go regardless of fallback
        let invalid = b"func {{{ broken go";
        let result = adapter.write_content("src/main.go", invalid, 0);
        assert!(result.is_err(), ".go file should be validated as Go");

        // Valid Go
        let valid = b"package main\n\nfunc main() {}\n";
        adapter.write_content("src/main.go", valid, 0)?;

        Ok(())
    }

    #[test]
    #[cfg(feature = "validate")]
    fn truncate_then_invalid_write_restores_shadow() -> Result<()> {
        let adapter = writable_go_adapter()?;

        // Write valid Go to the extensionless "source" file (uses fallback language)
        let valid = b"package main\n\nfunc main() { println(\"hello\") }\n";
        adapter.write_content("functions/main/source", valid, 0)?;

        // Verify content is there
        let mut buf = [0u8; 256];
        let n = adapter.read_content("functions/main/source", &mut buf, 0)?;
        assert_eq!(&buf[..n], valid);

        // Truncate (simulates first half of `echo 'x' > file`)
        adapter.truncate("functions/main/source")?;

        // Content is gone after truncate
        let n = adapter.read_content("functions/main/source", &mut buf, 0)?;
        assert_eq!(n, 0);

        // Write INVALID Go (simulates second half of `echo 'x' > file`)
        let invalid = b"func {{{ broken";
        let result = adapter.write_content("functions/main/source", invalid, 0);
        assert!(result.is_err(), "invalid write should be rejected");

        // Shadow copy should have restored the old valid content
        let n = adapter.read_content("functions/main/source", &mut buf, 0)?;
        assert_eq!(
            &buf[..n],
            valid,
            "old valid content should be restored after failed write"
        );

        Ok(())
    }

    #[test]
    #[cfg(feature = "validate")]
    fn rename_invalid_content_to_validated_path_rejected() -> Result<()> {
        let adapter = writable_go_adapter()?;

        // Create a temp file with invalid Go content (simulates sed -i temp file)
        // Use .tmp extension so write_content skips validation (unrecognized ext)
        adapter.create_node("src", "main.go.tmp", false)?;
        let invalid = b"func {{{ broken";
        adapter.write_content("src/main.go.tmp", invalid, 0)?;

        // Rename temp over main.go — destination has .go extension → validate
        let result = adapter.rename_node("src/main.go.tmp", "src", "main.go");
        assert!(
            result.is_err(),
            "renaming invalid content to .go path should fail"
        );

        // Temp file should still exist (rename was rejected)
        let temp = adapter.get_node("src/main.go.tmp")?;
        assert!(
            temp.is_some(),
            "temp file should still exist after rejected rename"
        );

        // Original main.go should still have its old content
        let mut buf = [0u8; 256];
        let n = adapter.read_content("src/main.go", &mut buf, 0)?;
        assert!(n > 0, "original .go file should still exist with content");

        Ok(())
    }

    #[test]
    #[cfg(feature = "validate")]
    fn rename_valid_content_to_validated_path_succeeds() -> Result<()> {
        let adapter = writable_go_adapter()?;

        // Create temp file with valid Go
        adapter.create_node("src", "main.go.tmp", false)?;
        let valid = b"package main\n\nfunc main() { println(\"updated\") }\n";
        adapter.write_content("src/main.go.tmp", valid, 0)?;

        // sed -i removes the original before renaming temp over it
        adapter.remove_node("src/main.go")?;

        // Rename temp → main.go — valid content, should succeed
        adapter.rename_node("src/main.go.tmp", "src", "main.go")?;

        // main.go now has the new content
        let mut buf = [0u8; 256];
        let n = adapter.read_content("src/main.go", &mut buf, 0)?;
        assert_eq!(&buf[..n], valid);

        Ok(())
    }

    /// Helper: create a writable adapter from an HTML parse with source tracking.
    #[cfg(feature = "splice")]
    fn writable_ast_adapter(html: &[u8]) -> Result<SqliteGraphAdapter> {
        let db_bytes = leyline_ts::parse_with_source(
            html,
            leyline_ts::languages::TsLanguage::Html,
            "test.html",
        )?;
        SqliteGraphAdapter::new_writable(&db_bytes)
    }

    #[test]
    #[cfg(feature = "splice")]
    fn splice_write_and_flush_triggers_reproject() -> Result<()> {
        let html = b"<p>hello</p>";
        let adapter = writable_ast_adapter(html)?;

        // The text leaf is at "test.html/element/text" in the projected
        // tree: a node's path is rooted at its source FILE, which is the
        // node the parse hangs the AST root on.
        let node_id = "test.html/element/text";
        let node = adapter.get_node(node_id)?;
        assert!(
            node.is_some(),
            "test.html/element/text node should exist after HTML parse"
        );

        // Write new content to the node's record
        adapter.write_content(node_id, b"world", 0)?;

        // Verify pending splice was marked
        assert!(
            adapter.pending_splice.lock().contains(node_id),
            "node should be marked for pending splice"
        );

        // Flush triggers splice_and_reproject
        adapter.flush_node(node_id)?;

        // Pending splice should be cleared
        assert!(
            !adapter.pending_splice.lock().contains(node_id),
            "pending splice should be cleared after successful flush"
        );

        // Source should be updated
        let guard = adapter.writer.lock();
        let source: Vec<u8> = guard.conn().query_row(
            "SELECT content FROM _source WHERE id = 'test.html'",
            [],
            |r| r.get(0),
        )?;
        let source_str = String::from_utf8_lossy(&source);
        assert!(
            source_str.contains("world"),
            "_source should contain spliced text, got: {source_str}"
        );

        Ok(())
    }

    #[test]
    #[cfg(feature = "splice")]
    fn splice_non_ast_node_ignored() -> Result<()> {
        let html = b"<p>hello</p>";
        let adapter = writable_ast_adapter(html)?;

        // Create a plain node (no _ast entry)
        adapter.create_node("", "plain.txt", false)?;
        adapter.write_content("plain.txt", b"just text", 0)?;

        // Should not be pending
        assert!(
            !adapter.pending_splice.lock().contains("plain.txt"),
            "non-AST node should not be marked for splice"
        );

        // Flush is a no-op
        adapter.flush_node("plain.txt")?;

        // Plain content unchanged
        let mut buf = [0u8; 64];
        let n = adapter.read_content("plain.txt", &mut buf, 0)?;
        assert_eq!(&buf[..n], b"just text");

        Ok(())
    }

    #[test]
    #[cfg(feature = "splice")]
    fn splice_partial_then_complete() -> Result<()> {
        let html = b"<div>original</div>";
        let adapter = writable_ast_adapter(html)?;

        let node_id = "test.html/element/text";

        // Write broken HTML that produces syntax errors when spliced into source
        // <div>original</div> → <div><broken</div> → tree-sitter error
        adapter.truncate(node_id)?;
        adapter.write_content(node_id, b"<broken", 0)?;

        // Flush should fail (syntax error) but not panic
        let result = adapter.flush_node(node_id);
        if result.is_err() {
            // Node should still be pending for retry
            assert!(
                adapter.pending_splice.lock().contains(node_id),
                "failed flush should keep node pending"
            );
        }
        // Note: tree-sitter may be lenient with HTML — if it passes, that's OK too.

        // Write valid replacement content
        adapter.truncate(node_id)?;
        adapter.write_content(node_id, b"replaced", 0)?;

        // This flush should succeed
        adapter.flush_node(node_id)?;

        assert!(
            !adapter.pending_splice.lock().contains(node_id),
            "successful flush should clear pending"
        );

        // Verify source updated
        let guard = adapter.writer.lock();
        let source: Vec<u8> = guard.conn().query_row(
            "SELECT content FROM _source WHERE id = 'test.html'",
            [],
            |r| r.get(0),
        )?;
        let source_str = String::from_utf8_lossy(&source);
        assert!(
            source_str.contains("replaced"),
            "_source should contain final text, got: {source_str}"
        );

        Ok(())
    }

    /// End-to-end for the write-then-flush path: write, splice, read back.
    ///
    /// NOTE on what this does and does not prove. It passes with or without
    /// the manifest invalidation in `flush_node`, because `write_content`
    /// re-chunks the node before the flush ever runs — so this node is never
    /// stale by the time it is read. Verified by removing the invalidation:
    /// still green. The genuine staleness case is
    /// `batch_splice_does_not_leave_a_stale_chunk_manifest`, which reaches
    /// `reproject_source` without any `write_content` refresh in front of it.
    /// Kept as a path-integration check, not as the staleness falsifier.
    #[test]
    #[cfg(all(feature = "splice", feature = "cdc"))]
    fn write_then_flush_serves_post_splice_bytes() -> Result<()> {
        let adapter = writable_ast_adapter(b"<p>hello</p>")?;
        let node_id = "test.html/element/text";

        // Give the arena chunk storage, then populate this node's manifest so
        // reads are genuinely being served from chunks before the splice.
        {
            let guard = adapter.writer.lock();
            crate::chunked::create_chunked_content_schema(guard.conn())?;
            crate::chunked::store_content_chunked(
                guard.conn(),
                nid_of(guard.conn(), node_id),
                b"hello",
            )?;
            assert!(crate::chunked::has_chunked_content(
                guard.conn(),
                nid_of(guard.conn(), node_id)
            )?);
        }
        adapter.refresh_readers()?;

        let mut buf = vec![0u8; 32];
        let n = adapter.read_content(node_id, &mut buf, 0)?;
        assert_eq!(
            &buf[..n],
            b"hello",
            "precondition: chunked read serves old text"
        );

        // Write + flush: leyline-ts reprojects and rewrites `record` directly.
        adapter.write_content(node_id, b"world", 0)?;
        adapter.flush_node(node_id)?;

        // The read must NOT return the stale chunked bytes.
        let mut buf = vec![0u8; 32];
        let n = adapter.read_content(node_id, &mut buf, 0)?;
        assert_eq!(
            &buf[..n],
            b"world",
            "read served stale pre-splice bytes from an invalidated-but-kept manifest"
        );
        Ok(())
    }

    /// The write path must keep the manifest current, so a mount that writes
    /// then reads still gets chunked reads rather than silently degrading to
    /// the whole-record path forever after the first write.
    #[test]
    #[cfg(feature = "cdc")]
    fn write_refreshes_the_chunk_manifest() -> Result<()> {
        let adapter = writable_adapter()?;
        {
            let guard = adapter.writer.lock();
            crate::chunked::create_chunked_content_schema(guard.conn())?;
        }

        let body = "x".repeat(300_000);
        adapter.write_content("docs/readme", body.as_bytes(), 0)?;

        let guard = adapter.writer.lock();
        assert!(
            crate::chunked::has_chunked_content(guard.conn(), nid_of(guard.conn(), "docs/readme"))?,
            "a write into a chunk-enabled arena must populate the manifest"
        );
        drop(guard);

        let mut buf = vec![0u8; 100];
        let n = adapter.read_content("docs/readme", &mut buf, 150_000)?;
        assert_eq!(&buf[..n], &body.as_bytes()[150_000..150_100]);
        Ok(())
    }

    /// `batch_splice` (the ADR-007 commit path) calls `reproject_source`
    /// DIRECTLY — it never goes through `write_content`, so nothing re-chunks.
    /// A manifest populated before the splice therefore describes pre-splice
    /// bytes, and the chunked read path would serve them: correct-looking
    /// output that is silently the wrong content.
    ///
    /// This is the falsifying case for the invalidation in `batch_splice`.
    /// Remove that invalidation and this test fails; every other test passes.
    #[test]
    #[cfg(all(feature = "splice", feature = "cdc"))]
    fn batch_splice_does_not_leave_a_stale_chunk_manifest() -> Result<()> {
        let adapter = writable_ast_adapter(b"<p>hello</p>")?;
        let node_id = "test.html/element/text";

        {
            let guard = adapter.writer.lock();
            crate::chunked::create_chunked_content_schema(guard.conn())?;
            // Manifest describes the CURRENT content, "hello".
            crate::chunked::store_content_chunked(
                guard.conn(),
                nid_of(guard.conn(), node_id),
                b"hello",
            )?;
        }
        adapter.refresh_readers()?;

        let mut buf = vec![0u8; 32];
        let n = adapter.read_content(node_id, &mut buf, 0)?;
        assert_eq!(&buf[..n], b"hello", "precondition: served from chunks");

        // Commit path: no write_content, so no re-chunk anywhere.
        adapter.batch_splice(&[(node_id.to_string(), Some("world".to_string()))])?;
        adapter.refresh_readers()?;

        let mut buf = vec![0u8; 32];
        let n = adapter.read_content(node_id, &mut buf, 0)?;
        assert_eq!(
            &buf[..n],
            b"world",
            "batch_splice left a stale manifest — the read served pre-splice bytes"
        );
        Ok(())
    }

    /// Helper: writable adapter whose arena has chunk storage enabled.
    #[cfg(feature = "cdc")]
    fn chunked_adapter() -> Result<SqliteGraphAdapter> {
        let adapter = writable_adapter()?;
        {
            let guard = adapter.writer.lock();
            crate::chunked::create_chunked_content_schema(guard.conn())?;
        }
        Ok(adapter)
    }

    #[cfg(feature = "cdc")]
    fn cdc_body(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                b'a' + (state % 26) as u8
            })
            .collect()
    }

    #[cfg(feature = "cdc")]
    fn manifest_tuples(
        adapter: &SqliteGraphAdapter,
        node_id: &str,
    ) -> Result<Vec<(Vec<u8>, usize, usize)>> {
        let guard = adapter.writer.lock();
        let nid = nid_of(guard.conn(), node_id);
        let mut statement = guard.conn().prepare(
            "SELECT chunk_hash, byte_offset, byte_len
               FROM content_manifest
              WHERE nid = ?1
              ORDER BY seq",
        )?;
        let rows = statement.query_map([nid], |row| {
            Ok((
                row.get(0)?,
                row.get::<_, i64>(1)? as usize,
                row.get::<_, i64>(2)? as usize,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    #[cfg(feature = "cdc")]
    fn assert_chunked_content_matches(
        adapter: &SqliteGraphAdapter,
        node_id: &str,
        model: &[u8],
    ) -> Result<()> {
        let expected_manifest: Vec<_> = leyline_cdc::chunk(model)
            .into_iter()
            .map(|chunk| (chunk.hash.as_bytes().to_vec(), chunk.offset, chunk.len))
            .collect();
        assert_eq!(manifest_tuples(adapter, node_id)?, expected_manifest);

        let mut reconstructed = vec![0; model.len()];
        let read = adapter.read_content(node_id, &mut reconstructed, 0)?;
        assert_eq!(&reconstructed[..read], model);
        Ok(())
    }

    #[test]
    #[cfg(feature = "cdc")]
    fn graph_write_incrementally_matches_full_chunk_oracle() -> Result<()> {
        let adapter = chunked_adapter()?;
        let node_id = "docs/readme";
        let mut model = cdc_body(0xC0FFEE, 4_000_000);
        adapter.write_content(node_id, &model, 0)?;

        let initial_len = model.len();
        let cases: [(&str, usize, &[u8]); 5] = [
            ("deep overwrite", initial_len / 2, b"XYZ"),
            (
                "boundary overwrite",
                leyline_cdc::MAX_CHUNK - 2,
                b"boundary",
            ),
            ("append", initial_len, b"append"),
            ("beyond EOF", initial_len + b"append".len() + 97, b"tail"),
            ("empty write", initial_len / 3, b""),
        ];

        for (name, edit_offset, edit) in cases {
            let write_end = edit_offset + edit.len();
            if write_end > model.len() {
                model.resize(write_end, 0);
            }
            model[edit_offset..write_end].copy_from_slice(edit);

            let (_, outcome) = adapter.write_content_traced(node_id, edit, edit_offset as u64)?;
            assert_chunked_content_matches(&adapter, node_id, &model)
                .with_context(|| format!("{name} diverged from full-chunk oracle"))?;

            let WriteRefreshOutcome::Incremental {
                prefix_kept,
                tail_reused,
                bytes_scanned,
                ..
            } = outcome
            else {
                panic!("{name}: expected incremental refresh, got {outcome:?}");
            };
            if name == "deep overwrite" {
                assert!(prefix_kept > 0);
                assert!(tail_reused > 0);
                assert!(bytes_scanned <= 4 * leyline_cdc::MAX_CHUNK);
            }
        }
        Ok(())
    }

    #[test]
    #[cfg(feature = "cdc")]
    fn graph_write_full_chunks_when_manifest_is_missing_or_stale() -> Result<()> {
        for stale in [false, true] {
            let adapter = chunked_adapter()?;
            let node_id = "docs/readme";
            if stale {
                let guard = adapter.writer.lock();
                crate::chunked::store_content_chunked(
                    guard.conn(),
                    nid_of(guard.conn(), node_id),
                    b"hello",
                )?;
                // Staleness means the RECORD moved on (a foreign writer
                // replacing the bytes bumps the generation via trigger) —
                // same length on purpose: this is exactly the same-shape
                // replacement the old (size, mtime) witness could not see.
                // An mtime-only touch deliberately no longer invalidates —
                // the generation witness keys on mutation, not metadata
                // (ley-line-open-b82f56).
                let nid = nid_of(guard.conn(), node_id);
                guard
                    .conn()
                    .execute("UPDATE nodes SET record = 'holla' WHERE nid = ?1", [nid])?;
            }

            let edit = b"XY";
            let (_, outcome) = adapter.write_content_traced(node_id, edit, 1)?;
            assert_eq!(
                outcome,
                WriteRefreshOutcome::Full { bytes_scanned: 5 },
                "stale={stale}"
            );
            // The stale branch's write lands on the FOREIGN bytes — the
            // whole point is that the tampered record, not the stale
            // manifest, is what the write path reads and re-chunks.
            let model: &[u8] = if stale { b"hXYla" } else { b"hXYlo" };
            assert_chunked_content_matches(&adapter, node_id, model)?;
        }
        Ok(())
    }

    #[test]
    #[cfg(feature = "cdc")]
    fn graph_write_does_not_add_chunk_schema_to_foreign_arena() -> Result<()> {
        let adapter = writable_adapter()?;
        let (_, outcome) = adapter.write_content_traced("docs/readme", b"XY", 1)?;
        assert_eq!(outcome, WriteRefreshOutcome::Skipped);

        let guard = adapter.writer.lock();
        let chunk_tables: i64 = guard.conn().query_row(
            "SELECT count(*) FROM sqlite_master
              WHERE type = 'table'
                AND name IN ('chunks', 'content_manifest', 'content_manifest_meta')",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(chunk_tables, 0);
        Ok(())
    }

    /// `truncate` must actually truncate a chunk-backed node. Without
    /// invalidation it sets `record = NULL` while the manifest still describes
    /// the old bytes, so the chunked read path keeps serving them — truncate
    /// becomes a silent no-op. (Found by adversarial review; reproduced before
    /// fixing: returned 5 bytes of "world" after truncate.)
    #[test]
    #[cfg(feature = "cdc")]
    fn truncate_invalidates_the_chunk_manifest() -> Result<()> {
        let adapter = chunked_adapter()?;
        adapter.write_content("docs/readme", b"world", 0)?;

        let mut buf = [0u8; 64];
        let n = adapter.read_content("docs/readme", &mut buf, 0)?;
        assert_eq!(&buf[..n], b"world", "precondition: content is readable");

        adapter.truncate("docs/readme")?;

        let mut buf = [0u8; 64];
        let n = adapter.read_content("docs/readme", &mut buf, 0)?;
        assert_eq!(
            n, 0,
            "truncate was a no-op — stale manifest still served {n} bytes"
        );
        Ok(())
    }

    /// THE severe one. Node ids are PATHS and paths get reused. A manifest that
    /// outlives its `nodes` row is not merely stale — it attaches to whatever
    /// node is created at that path next, so a brand-new, never-written file
    /// serves a DELETED file's content. Cross-generation content leak.
    /// (Found by adversarial review; reproduced before fixing: the fresh node
    /// returned "secret-old-bytes".)
    #[test]
    #[cfg(feature = "cdc")]
    fn recreating_a_removed_path_does_not_leak_the_old_content() -> Result<()> {
        let adapter = chunked_adapter()?;
        adapter.write_content("docs/readme", b"secret-old-bytes", 0)?;
        adapter.remove_node("docs/readme")?;

        let fresh = adapter.create_node("docs", "readme", false)?;
        let mut buf = [0u8; 64];
        let n = adapter.read_content(&fresh, &mut buf, 0)?;
        assert_eq!(
            n,
            0,
            "a newly created file leaked {} bytes of a deleted file: {:?}",
            n,
            String::from_utf8_lossy(&buf[..n])
        );
        Ok(())
    }

    /// `remove_node` cascades over descendants (`id LIKE 'x/%'`), so
    /// invalidation must cascade identically or a child's manifest outlives
    /// its row and leaks into a recreated child path.
    #[test]
    #[cfg(feature = "cdc")]
    fn removing_a_directory_invalidates_descendant_manifests() -> Result<()> {
        let adapter = chunked_adapter()?;
        adapter.write_content("docs/readme", b"child-secret", 0)?;

        adapter.remove_node("docs")?;

        let guard = adapter.writer.lock();
        assert!(
            !crate::chunked::has_chunked_content(
                guard.conn(),
                nid_of(guard.conn(), "docs/readme")
            )?,
            "descendant manifest survived removal of its parent directory"
        );
        Ok(())
    }

    /// `batch_splice`'s non-AST arm writes `nodes` directly. The post-reproject
    /// invalidation walks `_ast`-derived ids, so by definition it never sees
    /// these nodes — each arm must invalidate itself.
    #[test]
    #[cfg(all(feature = "splice", feature = "cdc"))]
    fn batch_splice_plain_node_arm_invalidates() -> Result<()> {
        let adapter = writable_ast_adapter(b"<p>hello</p>")?;
        {
            let guard = adapter.writer.lock();
            crate::chunked::create_chunked_content_schema(guard.conn())?;
        }
        // A node with no `_ast` row.
        let plain = adapter.create_node("", "plain.txt", false)?;
        adapter.write_content(&plain, b"world", 0)?;
        {
            let guard = adapter.writer.lock();
            let is_ast: bool = guard
                .conn()
                .query_row(
                    "SELECT 1 FROM _ast WHERE nid = ?1",
                    [nid_of(guard.conn(), &plain)],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            assert!(!is_ast, "fixture must be a NON-AST node");
        }

        adapter.batch_splice(&[(plain.clone(), Some("REPLACED".to_string()))])?;
        adapter.refresh_readers()?;

        let mut buf = [0u8; 64];
        let n = adapter.read_content(&plain, &mut buf, 0)?;
        assert_eq!(
            &buf[..n],
            b"REPLACED",
            "plain-node splice served stale bytes from a live manifest"
        );
        Ok(())
    }

    /// `rename_node` moves the `nodes` row but the manifest is keyed by the old
    /// id with no FK, so it orphans onto a path that may be reused.
    #[test]
    #[cfg(feature = "cdc")]
    fn rename_does_not_orphan_a_manifest_on_the_vacated_path() -> Result<()> {
        let adapter = chunked_adapter()?;
        adapter.write_content("docs/readme", b"pre-rename-bytes", 0)?;
        adapter.rename_node("docs/readme", "docs", "moved")?;

        let guard = adapter.writer.lock();
        assert!(
            !crate::chunked::has_chunked_content(
                guard.conn(),
                nid_of(guard.conn(), "docs/readme")
            )?,
            "manifest orphaned on the vacated path — a new file there would \
             read the pre-rename content"
        );
        drop(guard);

        // And the renamed node still reads correctly (via the record path).
        let mut buf = [0u8; 64];
        let n = adapter.read_content("docs/moved", &mut buf, 0)?;
        assert_eq!(&buf[..n], b"pre-rename-bytes");
        Ok(())
    }

    #[test]
    #[cfg(feature = "cdc")]
    fn post_open_chunk_corruption_fails_closed() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let ctrl_path = dir.path().join("corrupt.ctrl");
        let arena_path = dir.path().join("corrupt.arena");

        let source = Connection::open_in_memory()?;
        create_schema(&source)?;
        crate::chunked::create_chunked_content_schema(&source)?;
        let data = vec![b'x'; 256 * 1024];
        let record = String::from_utf8(data.clone())?;
        let nid = put_file(&source, "docs/readme", 1, Some(&record))?;
        crate::chunked::store_content_chunked(&source, nid, &data)?;
        let db_bytes = source.serialize("main")?;

        let arena_size = 4096 + 1024 * 1024 * 2;
        let mut mmap = leyline_core::layout::create_arena(&arena_path, arena_size)?;
        leyline_core::layout::write_to_arena(&mut mmap, db_bytes.as_ref())?;
        let root: [u8; 32] = blake3::hash(db_bytes.as_ref()).into();
        Controller::open_or_create(&ctrl_path)?.set_arena_with_root(
            arena_path.to_str().unwrap(),
            arena_size,
            root,
        )?;

        let adapter = SqliteGraphAdapter::from_arena_writable(&ctrl_path)?;
        let offset = {
            let guard = adapter.writer.lock();
            let (hash, offset, mut bytes): (Vec<u8>, i64, Vec<u8>) = guard.conn().query_row(
                "SELECT m.chunk_hash, m.byte_offset, c.chunk_bytes \
                   FROM content_manifest m JOIN content_chunks c USING (chunk_hash) \
                  WHERE m.nid = ?1 ORDER BY m.byte_offset LIMIT 1",
                [nid],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            bytes[0] ^= 0xff;
            guard.conn().execute(
                "UPDATE content_chunks SET chunk_bytes = ?1 WHERE chunk_hash = ?2",
                rusqlite::params![bytes, hash],
            )?;
            u64::try_from(offset)?
        };
        adapter.refresh_readers()?;

        let before = vec![0xa5; 4096];
        let mut out = before.clone();
        let err = adapter
            .read_content("docs/readme", &mut out, offset)
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("integrity violation"),
            "{err:#}"
        );
        assert_eq!(out, before, "failed read partially modified destination");
        Ok(())
    }

    /// Publish a minimal nodes-schema SQLite image into a fresh on-disk
    /// arena the way the producer does, returning the control path and the
    /// one file node's content.
    #[cfg(feature = "verify")]
    fn publish_nodes_arena(dir: &Path) -> Result<(PathBuf, String)> {
        let source = Connection::open_in_memory()?;
        create_schema(&source)?;
        let content = "verify-on-fault must serve exactly these bytes".to_string();
        put_file(&source, "docs/readme", 1, Some(&content))?;
        let db_bytes = source.serialize("main")?;

        let arena_path = dir.join("verified.arena");
        let ctrl_path = dir.join("verified.ctrl");
        let arena_size = 4096 + 2 * 1024 * 1024;
        let mut mmap = leyline_core::layout::create_arena(&arena_path, arena_size)?;
        leyline_core::layout::write_to_arena(&mut mmap, db_bytes.as_ref())?;
        let root: [u8; 32] = blake3::hash(db_bytes.as_ref()).into();
        Controller::open_or_create(&ctrl_path)?.set_arena_with_root(
            arena_path.to_str().unwrap(),
            arena_size,
            root,
        )?;
        Ok((ctrl_path, content))
    }

    /// Flip one byte of the ACTIVE payload via a direct file write — the
    /// tamper the verified load path must refuse.
    #[cfg(feature = "verify")]
    fn flip_active_payload_byte(ctrl_path: &Path, at: u64) -> Result<()> {
        use std::io::{Read, Seek, SeekFrom, Write};
        let controller = Controller::open_or_create(ctrl_path)?;
        let arena_path = PathBuf::from(controller.arena_path());
        let bytes = std::fs::read(&arena_path)?;
        let header: &ArenaHeader =
            bytemuck::from_bytes(&bytes[..std::mem::size_of::<ArenaHeader>()]);
        let offset = header
            .validate_header(bytes.len() as u64)
            .context("arena header validation failed")?;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&arena_path)?;
        file.seek(SeekFrom::Start(offset + at))?;
        let mut b = [0u8; 1];
        file.read_exact(&mut b)?;
        file.seek(SeekFrom::Start(offset + at))?;
        file.write_all(&[b[0] ^ 0x01])?;
        file.flush()?;
        Ok(())
    }

    /// The verified load call-site: byte-identical service through the
    /// per-page gate, and a pre-load tamper refused at open (bead
    /// `ley-line-open-b6a4dd`; the post-load per-page refusal is pinned in
    /// `verified::tests`).
    #[test]
    #[cfg(feature = "verify")]
    fn verified_load_serves_identical_content_and_refuses_tamper() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let (ctrl, content) = publish_nodes_arena(dir.path())?;

        let adapter = SqliteGraphAdapter::from_arena_verified(&ctrl)?;
        let mut buf = [0u8; 128];
        let n = adapter.read_content("docs/readme", &mut buf, 0)?;
        assert_eq!(&buf[..n], content.as_bytes());

        // Byte 100 sits in the SQLite header page; any flipped payload byte
        // must fail the load-time root check.
        flip_active_payload_byte(&ctrl, 100)?;
        // `.err()` rather than `unwrap_err()`: the adapter has no Debug impl
        // (a live SQLite pool has nothing useful to print).
        let err = SqliteGraphAdapter::from_arena_verified(&ctrl)
            .err()
            .expect("tampered arena must be refused at load");
        assert!(
            format!("{err:#}").contains("root mismatch at load"),
            "{err:#}"
        );
        Ok(())
    }

    /// The FUSE serving path, gated: `LeylineFuse::read` calls exactly
    /// `Graph::read_content` on the mounted graph — this is that call on a
    /// `HotSwapGraph` in verify-on-fault mode.
    #[test]
    #[cfg(feature = "verify")]
    fn verified_hot_swap_graph_serves_the_fuse_read_path() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let (ctrl, content) = publish_nodes_arena(dir.path())?;

        let graph = HotSwapGraph::new(ctrl)?.with_verify_on_fault()?;
        let mut buf = [0u8; 128];
        let n = graph.read_content("docs/readme", &mut buf, 0)?;
        assert_eq!(&buf[..n], content.as_bytes());
        Ok(())
    }

    /// The builder must fail closed: enabling verify-on-fault on a graph
    /// whose arena was tampered AFTER the (unverified) initial load is an
    /// error — keeping the already-loaded unverified graph and returning Ok
    /// would be the verify-fallback smell wearing a builder costume. This
    /// is also the only observable that proves the builder actually
    /// re-loads through the gate rather than skipping the reload.
    #[test]
    #[cfg(feature = "verify")]
    fn with_verify_on_fault_refuses_a_tampered_arena() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let (ctrl, _content) = publish_nodes_arena(dir.path())?;

        // Loads clean (the arena is untampered at construction time)...
        let graph = HotSwapGraph::new(ctrl.clone())?;
        let root_before = Controller::open_or_create(&ctrl)?.current_root();
        // ...then the file is tampered behind its back...
        flip_active_payload_byte(&ctrl, 100)?;
        // ...so the gate must refuse, not shrug and serve the copy.
        let err = graph
            .with_verify_on_fault()
            .err()
            .expect("verify-on-fault over a tampered arena must refuse");
        assert!(
            format!("{err:#}").contains("root mismatch at load"),
            "{err:#}"
        );

        // --- post-fault state (bead `ley-line-open-5e6ce6`) ---
        //
        // The builder consumes `self`, so there is no surviving graph to
        // probe — the post-fault question here is not "is it still usable"
        // but "did the refusal change anything it should not have".
        //
        // The dangerous outcome is a refusal that *repairs*: a failed
        // verified load which wrote the observed root into `current_root`
        // would make the second attempt succeed. That is a silent trust
        // downgrade — the tamper becomes the new baseline — and the
        // assertion above cannot see it, because it only ever runs one
        // attempt. Stopping at "it returned an error" is exactly the gap
        // this bead exists to close.

        // 1. The published root is untouched. A refusal must observe, not
        //    reconcile.
        assert_eq!(
            Controller::open_or_create(&ctrl)?.current_root(),
            root_before,
            "a refused verified load must not rewrite current_root — \
             adopting the tampered root would make the tamper canonical"
        );

        // 2. The refusal is stable. Retrying must fail, not succeed because
        //    the first attempt cached or repaired something.
        //
        //    Note where it fails: `HotSwapGraph::new` itself refuses now,
        //    before the builder is reached. Two independent gates cover a
        //    tampered arena — the T2.3 loader's root check at deserialize
        //    time, and verify-on-fault's check at load — and the assertion
        //    above only exercises the second because the graph was
        //    constructed *before* the tamper. So this chains both: whichever
        //    gate catches it, a retry must not get a working graph.
        let retry = HotSwapGraph::new(ctrl.clone())
            .and_then(HotSwapGraph::with_verify_on_fault)
            .err()
            .expect("a second attempt over the same tampered arena must also refuse");
        assert!(
            format!("{retry:#}").contains("root mismatch"),
            "the refusal must be stable across attempts, got: {retry:#}"
        );
        Ok(())
    }

    /// The writer half: `flush_to_arena` in verify-on-fault mode advances
    /// `current_root` through the outboard (build on the first flush,
    /// incremental update on the second) and the result must be
    /// bit-identical to the reference hash — checked directly AND through
    /// the T2.3 flat-hash loader as an independent oracle, which refuses
    /// the arena outright if the incremental root ever diverges.
    #[test]
    #[cfg(feature = "verify")]
    fn verified_writer_advances_root_incrementally_and_stays_bit_identical() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let (ctrl, _content) = publish_nodes_arena(dir.path())?;

        let graph = HotSwapGraph::new(ctrl.clone())?
            .with_writable()
            .with_verify_on_fault()?;

        for (flush, edit) in ["first edit", "second, rather longer edit body"]
            .iter()
            .enumerate()
        {
            graph.write_content("docs/readme", edit.as_bytes(), 0)?;
            graph.flush_to_arena()?;

            let bytes = graph.serialize()?;
            let published = Controller::open_or_create(&ctrl)?.current_root();
            assert_eq!(
                published,
                *blake3::hash(&bytes).as_bytes(),
                "flush {flush}: incremental root diverged from the reference hash"
            );
            SqliteGraphAdapter::from_arena(&ctrl)
                .with_context(|| format!("flush {flush}: T2.3 loader refused the arena"))?;
        }
        Ok(())
    }
}
