use anyhow::{Context, Result};
use crossbeam_queue::ArrayQueue;
use std::collections::HashMap;
#[cfg(feature = "splice")]
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use leyline_core::{ArenaHeader, Controller};

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
/// Queries the optimized `nodes` table schema written by mache:
/// ```sql
/// CREATE TABLE nodes (
///     id TEXT PRIMARY KEY,
///     parent_id TEXT,
///     name TEXT NOT NULL,
///     kind INTEGER NOT NULL,   -- 0=file, 1=dir
///     size INTEGER DEFAULT 0,
///     mtime INTEGER NOT NULL,
///     record JSON
/// );
/// ```
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

    /// Create a writable adapter from an arena (for daemon with write-back).
    pub fn from_arena_writable(control_path: &Path) -> Result<Self> {
        let controller = Controller::open_or_create(control_path)?;
        let arena_path = controller.arena_path();

        let file = std::fs::File::open(&arena_path)?;
        let mmap = unsafe { memmap2::Mmap::map(&file)? };

        let header_slice = &mmap[..std::mem::size_of::<ArenaHeader>()];
        let header: &ArenaHeader = bytemuck::from_bytes(header_slice);

        let file_size = mmap.len() as u64;
        let offset = header
            .active_buffer_offset(file_size)
            .context("invalid arena header")?;
        let buf_size = ArenaHeader::buffer_size(file_size);

        let buf = &mmap[offset as usize..(offset + buf_size) as usize];
        let graph = SqliteGraph::from_bytes_writable(buf)?;
        let adapter = Self::new(graph);
        adapter.ensure_errors_table()?;
        Ok(adapter)
    }

    /// Ensure the `_errors` table exists for storing validation errors.
    fn ensure_errors_table(&self) -> Result<()> {
        let guard = self.writer.lock().unwrap();
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
        let guard = self.writer.lock().unwrap();
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
                    let bytes = self.reader_bytes.lock().unwrap();
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
        let writer = self.writer.lock().unwrap();
        let bytes = writer.serialize()?;
        self.reader_gen.fetch_add(1, Ordering::Release);
        // Drain is best-effort; stale stragglers are caught by generation check
        while self.readers.pop().is_some() {}
        *self.reader_bytes.lock().unwrap() = bytes;
        Ok(())
    }

    fn row_to_node(row: &rusqlite::Row<'_>) -> std::result::Result<Node, rusqlite::Error> {
        let id: String = row.get("id")?;
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
            let result = reader.conn().query_row(
                "SELECT id, name, kind, size, mtime FROM nodes WHERE id = ?1",
                [id],
                Self::row_to_node,
            );
            match result {
                Ok(node) => Ok(Some(node)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
    }

    fn lookup_child(&self, parent_id: &str, name: &str) -> Result<Option<Node>> {
        self.with_reader(|reader| {
            let result = reader.conn().query_row(
                "SELECT id, name, kind, size, mtime FROM nodes WHERE parent_id = ?1 AND name = ?2",
                rusqlite::params![parent_id, name],
                Self::row_to_node,
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
            let mut stmt = reader.conn().prepare_cached(
                "SELECT id, name, kind, size, mtime FROM nodes WHERE parent_id = ?1",
            )?;
            let rows = stmt.query_map([parent_id], Self::row_to_node)?;
            let mut children = Vec::new();
            for row in rows {
                children.push(row?);
            }
            Ok(children)
        })
    }

    fn all_file_contents(&self) -> Result<Vec<(String, String)>> {
        self.with_reader(|reader| {
            let mut stmt = reader.conn().prepare_cached(
                "SELECT id, record FROM nodes WHERE kind = 0 AND record IS NOT NULL AND length(record) > 0",
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

    fn read_content(&self, id: &str, buf: &mut [u8], offset: u64) -> Result<usize> {
        self.with_reader(|reader| {
            let record: Option<String> = reader
                .conn()
                .query_row("SELECT record FROM nodes WHERE id = ?1", [id], |row| {
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
        })
    }

    fn write_content(&self, id: &str, data: &[u8], offset: u64) -> Result<usize> {
        {
            let guard = self.writer.lock().unwrap();
            let now = now_nanos();

            // Read existing content, patch in the new data
            let existing: Option<String> = guard
                .conn()
                .query_row("SELECT record FROM nodes WHERE id = ?1", [id], |row| {
                    row.get(0)
                })
                .ok()
                .flatten();

            let mut content = existing.map(|s| s.into_bytes()).unwrap_or_default();
            let off = offset as usize;

            // Extend if writing past current end
            if off + data.len() > content.len() {
                content.resize(off + data.len(), 0);
            }
            content[off..off + data.len()].copy_from_slice(data);

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
                        if let Some(old) = self.shadow.lock().unwrap().remove(id) {
                            guard
                                .conn()
                                .execute(
                                    "UPDATE nodes SET record = ?1, size = ?2, mtime = ?3 WHERE id = ?4",
                                    rusqlite::params![&old, old.len() as i64, now, id],
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
                    self.shadow.lock().unwrap().remove(id);
                }
            }

            // Validation passed (or skipped) — commit the write
            let new_str = String::from_utf8_lossy(&content);
            guard.conn().execute(
                "UPDATE nodes SET record = ?1, size = ?2, mtime = ?3 WHERE id = ?4",
                rusqlite::params![new_str.as_ref(), content.len() as i64, now, id],
            )?;

            // Mark for splice on flush if this node has AST tracking
            #[cfg(feature = "splice")]
            {
                let is_ast: bool = guard
                    .conn()
                    .query_row("SELECT 1 FROM _ast WHERE node_id = ?1", [id], |_| Ok(true))
                    .unwrap_or(false);
                if is_ast {
                    self.pending_splice.lock().unwrap().insert(id.to_string());
                }
            }
        }
        self.refresh_readers()?;
        Ok(data.len())
    }

    fn create_node(&self, parent_id: &str, name: &str, is_dir: bool) -> Result<String> {
        let id = {
            let guard = self.writer.lock().unwrap();
            let now = now_nanos();

            let id = if parent_id.is_empty() {
                name.to_string()
            } else {
                format!("{}/{}", parent_id, name)
            };

            let kind: i64 = if is_dir { 1 } else { 0 };
            guard.conn().execute(
                "INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES (?1, ?2, ?3, ?4, 0, ?5, NULL)",
                rusqlite::params![id, parent_id, name, kind, now],
            )?;
            id
        };
        self.refresh_readers()?;
        Ok(id)
    }

    fn remove_node(&self, id: &str) -> Result<()> {
        {
            let guard = self.writer.lock().unwrap();
            // Delete the node and all descendants (cascading by prefix)
            guard.conn().execute(
                "DELETE FROM nodes WHERE id = ?1 OR id LIKE ?2",
                rusqlite::params![id, format!("{id}/%")],
            )?;
        }
        self.refresh_readers()?;
        Ok(())
    }

    fn truncate(&self, id: &str) -> Result<()> {
        {
            let guard = self.writer.lock().unwrap();
            let now = now_nanos();

            // Save shadow copy before truncating (for validation rollback)
            #[cfg(feature = "validate")]
            {
                let lang = crate::validate::language_for_node(id, self.default_language.as_ref());
                if lang.is_some() {
                    let old_content: Option<String> = guard
                        .conn()
                        .query_row("SELECT record FROM nodes WHERE id = ?1", [id], |row| {
                            row.get(0)
                        })
                        .ok()
                        .flatten();
                    if let Some(content) = old_content {
                        self.shadow.lock().unwrap().insert(id.to_string(), content);
                    }
                }
            }

            guard.conn().execute(
                "UPDATE nodes SET record = NULL, size = 0, mtime = ?1 WHERE id = ?2",
                rusqlite::params![now, id],
            )?;
        }
        self.refresh_readers()?;
        Ok(())
    }

    fn flush_node(&self, id: &str) -> Result<()> {
        #[cfg(feature = "splice")]
        {
            if !self.pending_splice.lock().unwrap().contains(id) {
                return Ok(());
            }
            let guard = self.writer.lock().unwrap();
            let record: Option<String> = guard
                .conn()
                .query_row("SELECT record FROM nodes WHERE id = ?1", [id], |r| r.get(0))
                .ok()
                .flatten();
            let Some(text) = record.filter(|s| !s.is_empty()) else {
                return Ok(());
            };
            leyline_ts::splice::splice_and_reproject(guard.conn(), id, &text)?;
            // Only remove from pending on success — failed attempts retry on next flush
            self.pending_splice.lock().unwrap().remove(id);
            // Reproject replaced all nodes — shadows are stale
            #[cfg(feature = "validate")]
            self.shadow.lock().unwrap().clear();
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

        let guard = self.writer.lock().unwrap();
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
            let ast_info = conn.query_row(
                "SELECT source_id, start_byte, end_byte FROM _ast WHERE node_id = ?1",
                [node_id.as_str()],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)? as usize,
                        r.get::<_, i64>(2)? as usize,
                    ))
                },
            );

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
                    // Non-AST node — direct record update or delete
                    match text {
                        Some(t) => {
                            conn.execute(
                                "UPDATE nodes SET record = ?1, size = ?2, mtime = ?3 WHERE id = ?4",
                                rusqlite::params![t, t.len() as i64, now, node_id],
                            )?;
                        }
                        None => {
                            conn.execute(
                                "DELETE FROM nodes WHERE id = ?1 OR id LIKE ?2",
                                rusqlite::params![node_id, format!("{node_id}/%")],
                            )?;
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
            group.sort_by(|a, b| b.start_byte.cmp(&a.start_byte));

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
                    if let Some(dot_pos) = rest.find("..") {
                        if let Ok(err_byte) = rest[..dot_pos].parse::<usize>() {
                            // Find which edit's post-splice range contains the error byte
                            for r in &ranges {
                                if err_byte >= r.start && err_byte < r.end {
                                    return anyhow::anyhow!(
                                        "{e} (attributed to node '{}')",
                                        r.node_id
                                    );
                                }
                            }
                        }
                    }
                }
                // No attribution possible — list all nodes in the group
                let node_ids: Vec<&str> = group.iter().map(|e| e.node_id.as_str()).collect();
                anyhow::anyhow!("{e} (source '{}', edited nodes: {:?})", source_id, node_ids)
            })?;
        }

        // Clear pending splice set
        self.pending_splice.lock().unwrap().clear();
        #[cfg(feature = "validate")]
        self.shadow.lock().unwrap().clear();

        drop(guard);
        self.refresh_readers()?;
        Ok(())
    }

    fn serialize(&self) -> Result<Vec<u8>> {
        self.serialize()
    }

    fn rename_node(&self, id: &str, new_parent_id: &str, new_name: &str) -> Result<()> {
        {
            let guard = self.writer.lock().unwrap();
            let new_id = if new_parent_id.is_empty() {
                new_name.to_string()
            } else {
                format!("{new_parent_id}/{new_name}")
            };

            // Validate content against destination language (catches sed -i pattern:
            // write to temp file → rename over validated file)
            #[cfg(feature = "validate")]
            {
                let dest_lang =
                    crate::validate::language_for_node(&new_id, self.default_language.as_ref());
                if let Some(lang) = dest_lang {
                    let content: Option<String> = guard
                        .conn()
                        .query_row("SELECT record FROM nodes WHERE id = ?1", [id], |row| {
                            row.get(0)
                        })
                        .ok()
                        .flatten();
                    if let Some(ref src) = content
                        && !src.is_empty()
                        && let Err(e) = crate::validate::validate(src.as_bytes(), &lang)
                    {
                        let now = now_nanos();
                        guard.conn().execute(
                            "INSERT OR REPLACE INTO _errors (node_id, line, col, message, timestamp) VALUES (?1, ?2, ?3, ?4, ?5)",
                            rusqlite::params![&new_id, e.line, e.column, e.message, now],
                        ).ok();
                        log::warn!("validation failed on rename to {new_id}: {e}");
                        anyhow::bail!("{e}");
                    }
                }
            }

            let old_prefix = format!("{id}/");
            let new_prefix = format!("{new_id}/");

            // Rename the node itself
            guard.conn().execute(
                "UPDATE nodes SET id = ?1, parent_id = ?2, name = ?3 WHERE id = ?4",
                rusqlite::params![new_id, new_parent_id, new_name, id],
            )?;

            // Cascade to descendants
            let descendants: Vec<(String, String)> = {
                let mut stmt = guard
                    .conn()
                    .prepare("SELECT id, parent_id FROM nodes WHERE id LIKE ?1")?;
                let rows = stmt.query_map(rusqlite::params![format!("{old_prefix}%")], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                rows.collect::<std::result::Result<_, _>>()?
            };

            for (old_child_id, old_child_parent) in descendants {
                let new_child_id = format!("{new_prefix}{}", &old_child_id[old_prefix.len()..]);
                let new_child_parent = if old_child_parent == id {
                    new_id.clone()
                } else {
                    format!("{new_prefix}{}", &old_child_parent[old_prefix.len()..])
                };
                guard.conn().execute(
                    "UPDATE nodes SET id = ?1, parent_id = ?2 WHERE id = ?3",
                    rusqlite::params![new_child_id, new_child_parent, old_child_id],
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

/// Thread-safe wrapper that re-opens the inner graph when the control block
/// generation changes (hot-swap on arena update).
pub struct HotSwapGraph {
    inner: RwLock<Arc<dyn Graph>>,
    control_path: PathBuf,
    last_generation: AtomicU64,
    writable: bool,
    /// Default tree-sitter language for extensionless files (e.g. `source`).
    #[cfg(feature = "validate")]
    default_language: Option<tree_sitter::Language>,
}

impl HotSwapGraph {
    pub fn new(control_path: PathBuf) -> Result<Self> {
        let ctrl = Controller::open_or_create(&control_path)?;
        let generation = ctrl.generation();

        // Gen 0 means no data loaded yet — serve an empty graph
        let initial_graph: Arc<dyn Graph> = if generation == 0 {
            Arc::new(MemoryGraph::new())
        } else {
            Arc::new(SqliteGraphAdapter::from_arena(&control_path)?)
        };

        Ok(Self {
            inner: RwLock::new(initial_graph),
            control_path,
            last_generation: AtomicU64::new(generation),
            writable: false,
            #[cfg(feature = "validate")]
            default_language: None,
        })
    }

    /// Enable writable mode with optional validation language for extensionless files.
    /// Re-opens the inner graph as writable if already loaded.
    #[cfg(feature = "validate")]
    pub fn with_validation(mut self, default_language: Option<tree_sitter::Language>) -> Self {
        self.writable = true;
        self.default_language = default_language;
        let current_gen = self.last_generation.load(Ordering::Acquire);
        if current_gen > 0
            && let Ok(new_graph) = self.build_adapter(&self.control_path)
        {
            *self.inner.write().unwrap() = new_graph;
        }
        self
    }

    /// Enable writable mode (no validation).
    /// Re-opens the inner graph as writable if already loaded.
    pub fn with_writable(mut self) -> Self {
        self.writable = true;
        let current_gen = self.last_generation.load(Ordering::Acquire);
        if current_gen > 0
            && let Ok(new_graph) = self.build_adapter(&self.control_path)
        {
            *self.inner.write().unwrap() = new_graph;
        }
        self
    }

    /// Build an adapter for the current generation.
    fn build_adapter(&self, control_path: &Path) -> Result<Arc<dyn Graph>> {
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

    /// Serialize the in-memory SQLite, write to the inactive arena buffer,
    /// flip the header, and bump the control block generation.
    /// Updates `last_generation` without re-opening to avoid losing in-flight writes.
    pub fn flush_to_arena(&self) -> Result<()> {
        let inner = self.inner.read().unwrap().clone();
        let bytes = inner.serialize()?;

        let ctrl = Controller::open_or_create(&self.control_path)?;
        let arena_path = ctrl.arena_path();
        let arena_size = ctrl.arena_size();
        let current_gen = ctrl.generation();

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&arena_path)?;
        let mut mmap = unsafe { memmap2::MmapMut::map_mut(&file)? };
        leyline_core::layout::write_to_arena(&mut mmap, &bytes)?;

        let new_gen = current_gen + 1;
        let mut ctrl = Controller::open_or_create(&self.control_path)?;
        ctrl.set_arena(&arena_path, arena_size, new_gen)?;

        // Acknowledge generation bump without re-opening
        self.last_generation.store(new_gen, Ordering::Release);
        log::info!(
            "arena flush: gen {current_gen} -> {new_gen} ({} bytes)",
            bytes.len()
        );
        Ok(())
    }

    /// Check generation and swap the inner graph if stale.
    fn maybe_swap(&self) -> Result<Arc<dyn Graph>> {
        let ctrl = Controller::open_or_create(&self.control_path)?;
        let current_gen = ctrl.generation();
        let cached_gen = self.last_generation.load(Ordering::Acquire);

        if current_gen != cached_gen {
            // Gen 0 means empty; any nonzero gen means real data in the arena
            let new_graph: Arc<dyn Graph> = if current_gen == 0 {
                Arc::new(MemoryGraph::new())
            } else {
                self.build_adapter(&self.control_path)?
            };
            let mut w = self.inner.write().unwrap();
            *w = new_graph.clone();
            self.last_generation.store(current_gen, Ordering::Release);
            Ok(new_graph)
        } else {
            Ok(self.inner.read().unwrap().clone())
        }
    }
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
        self.inner.read().unwrap().serialize()
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
            self.content.lock().unwrap().insert(id, data);
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
        let guard = self.content.lock().unwrap();
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

#[cfg(test)]
mod tests {
    use super::*;
    use leyline_schema::create_schema;
    use rusqlite::{Connection, DatabaseName};

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
        source.execute_batch(
            "INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('vulns', '', 'vulns', 1, 0, 1000, NULL);
            INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('vulns/CVE-2024-0001', 'vulns', 'CVE-2024-0001', 1, 0, 2000, NULL);
            INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('vulns/CVE-2024-0001/source', 'vulns/CVE-2024-0001', 'source', 0, 42, 3000, '{\"severity\":\"critical\"}');",
        )?;

        let data = source.serialize(DatabaseName::Main)?;
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
        assert_eq!(leaf.size, 42);

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
        source.execute_batch(
            "INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('docs', '', 'docs', 1, 0, 1000, NULL);
            INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('docs/readme', 'docs', 'readme', 0, 5, 2000, 'hello');",
        )?;
        let data = source.serialize(DatabaseName::Main)?;
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

    #[test]
    fn hotswap_generation_zero_serves_empty() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let ctrl_path = dir.path().join("test.ctrl");
        let arena_path = dir.path().join("test.arena");

        // Create control block at gen 0 with arena path set
        // Buffers need to hold a serialized SQLite DB (~12KB minimum)
        let arena_size: u64 = 4096 + 32768 * 2;
        let mut ctrl = Controller::open_or_create(&ctrl_path)?;
        ctrl.set_arena(arena_path.to_str().unwrap(), arena_size, 0)?;

        // Create the arena file (needed for later, but gen 0 = empty graph)
        let _mmap = leyline_core::layout::create_arena(&arena_path, arena_size)?;

        // HotSwapGraph at gen 0 should serve empty root
        let graph = HotSwapGraph::new(ctrl_path.clone())?;
        let root = graph.get_node("")?.unwrap();
        assert!(root.is_dir);
        // No children at gen 0
        let children = graph.list_children("")?;
        assert!(children.is_empty());

        // Now write real data to arena and bump generation
        let source = Connection::open_in_memory()?;
        create_schema(&source)?;
        source.execute_batch(
            "INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('docs', '', 'docs', 1, 0, 1000, NULL);
            INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('docs/readme', 'docs', 'readme', 0, 5, 2000, 'hello');",
        )?;
        let db_bytes = source.serialize(DatabaseName::Main)?;

        // Write db to arena via write_to_arena
        let mut mmap = leyline_core::layout::create_arena(&arena_path, arena_size)?;
        leyline_core::layout::write_to_arena(&mut mmap, db_bytes.as_ref())?;

        // Bump control block to gen 1
        let mut ctrl = Controller::open_or_create(&ctrl_path)?;
        ctrl.set_arena(arena_path.to_str().unwrap(), arena_size, 1)?;

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
        source.execute_batch(
            "INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record_id, record) VALUES ('funcs', '', 'funcs', 1, 0, 1000, NULL, NULL);
            INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record_id, record) VALUES ('funcs/Validate', 'funcs', 'Validate', 1, 0, 2000, 'rec-1', NULL);
            INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record, source_file) VALUES ('funcs/Validate/source', 'funcs/Validate', 'source', 0, 18, 3000, 'func Validate(){}', 'validate.go');",
        )?;

        let data = source.serialize(DatabaseName::Main)?;
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
        source.execute_batch(&format!(
            "INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('f', '', 'f', 0, 0, {go_mtime}, NULL);"
        ))?;

        let data = source.serialize(DatabaseName::Main)?;
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
        source.execute_batch(
            "INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('functions', '', 'functions', 1, 0, 1000, NULL);
            INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('functions/main', 'functions', 'main', 1, 0, 2000, NULL);
            INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('functions/main/source', 'functions/main', 'source', 0, 37, 3000, 'package main\n\nfunc main() {\n}\n');
            INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('docs', '', 'docs', 1, 0, 1000, NULL);
            INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('docs/readme.txt', 'docs', 'readme.txt', 0, 5, 2000, 'hello');
            INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('src', '', 'src', 1, 0, 1000, NULL);
            INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('src/main.go', 'src', 'main.go', 0, 37, 3000, 'package main\n\nfunc main() {\n}\n');",
        )?;
        let data = source.serialize(DatabaseName::Main)?;
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
        let guard = adapter.writer.lock().unwrap();
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
        let guard = adapter.writer.lock().unwrap();
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

        let guard = adapter.writer.lock().unwrap();
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

        // The text leaf is at "element/text" in the projected tree
        let node_id = "element/text";
        let node = adapter.get_node(node_id)?;
        assert!(
            node.is_some(),
            "element/text node should exist after HTML parse"
        );

        // Write new content to the node's record
        adapter.write_content(node_id, b"world", 0)?;

        // Verify pending splice was marked
        assert!(
            adapter.pending_splice.lock().unwrap().contains(node_id),
            "node should be marked for pending splice"
        );

        // Flush triggers splice_and_reproject
        adapter.flush_node(node_id)?;

        // Pending splice should be cleared
        assert!(
            !adapter.pending_splice.lock().unwrap().contains(node_id),
            "pending splice should be cleared after successful flush"
        );

        // Source should be updated
        let guard = adapter.writer.lock().unwrap();
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
            !adapter.pending_splice.lock().unwrap().contains("plain.txt"),
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

        let node_id = "element/text";

        // Write broken HTML that produces syntax errors when spliced into source
        // <div>original</div> → <div><broken</div> → tree-sitter error
        adapter.truncate(node_id)?;
        adapter.write_content(node_id, b"<broken", 0)?;

        // Flush should fail (syntax error) but not panic
        let result = adapter.flush_node(node_id);
        if result.is_err() {
            // Node should still be pending for retry
            assert!(
                adapter.pending_splice.lock().unwrap().contains(node_id),
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
            !adapter.pending_splice.lock().unwrap().contains(node_id),
            "successful flush should clear pending"
        );

        // Verify source updated
        let guard = adapter.writer.lock().unwrap();
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
}
