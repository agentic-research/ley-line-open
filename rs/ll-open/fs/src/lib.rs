pub mod fuse;
pub mod graph;
pub mod nfs;
pub mod staging;
pub mod validate;
#[cfg(feature = "vec")]
pub mod vector;

use anyhow::{Context, Result};
use leyline_core::{ArenaHeader, Controller};
use memmap2::Mmap;
use rusqlite::{Connection, DatabaseName};
use serde::Serialize;
use std::ffi::CStr;
use std::io::Cursor;
use std::os::raw::c_char;
use std::path::Path;

/// A read-only SQLite database deserialized from an arena buffer.
///
/// Mache writes a complete SQLite `.db` file into each half of the
/// double-buffered arena. This struct opens the active buffer in-place
/// using `sqlite3_deserialize`, avoiding a full copy to a temp file.
pub struct SqliteGraph {
    conn: Connection,
}

impl SqliteGraph {
    /// Open the active buffer from a ley-line arena as a read-only SQLite DB.
    pub fn from_arena(control_path: &Path) -> Result<Self> {
        let controller = Controller::open_or_create(control_path)?;
        let arena_path = controller.arena_path();

        let file = std::fs::File::open(&arena_path).context("open arena file")?;
        let mmap = unsafe { Mmap::map(&file)? };

        let header_slice = &mmap[..std::mem::size_of::<ArenaHeader>()];
        let header: &ArenaHeader = bytemuck::from_bytes(header_slice);

        let file_size = mmap.len() as u64;
        let offset = header
            .active_buffer_offset(file_size)
            .context("invalid arena header")?;
        let buf_size = ArenaHeader::buffer_size(file_size);

        let buf = &mmap[offset as usize..(offset + buf_size) as usize];
        Self::from_bytes(buf)
    }

    /// Deserialize an arbitrary byte slice as a read-only SQLite database.
    /// This is the core primitive — the bytes must be a valid SQLite file.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        Self::deserialize(data, true)
    }

    /// Deserialize an arbitrary byte slice as a writable in-memory SQLite database.
    pub fn from_bytes_writable(data: &[u8]) -> Result<Self> {
        Self::deserialize(data, false)
    }

    fn deserialize(data: &[u8], readonly: bool) -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        let cursor = Cursor::new(data);
        conn.deserialize_read_exact(DatabaseName::Main, cursor, data.len(), readonly)
            .context("sqlite3_deserialize failed")?;
        Ok(SqliteGraph { conn })
    }

    /// Access the underlying connection for arbitrary queries.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Serialize the in-memory database to bytes (for flushing back to arena).
    pub fn serialize(&self) -> Result<Vec<u8>> {
        let data = self.conn.serialize(DatabaseName::Main)?;
        Ok(data.to_vec())
    }

    /// List all table names in the database.
    pub fn tables(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        let mut names = Vec::new();
        for name in rows {
            names.push(name?);
        }
        Ok(names)
    }

    /// Count rows in a table.
    pub fn row_count(&self, table: &str) -> Result<u64> {
        if !table.chars().all(|c| c.is_alphanumeric() || c == '_') {
            anyhow::bail!("invalid table name: {}", table);
        }
        let sql = format!("SELECT COUNT(*) FROM \"{}\"", table);
        let count: u64 = self.conn.query_row(&sql, [], |row| row.get(0))?;
        Ok(count)
    }

    /// Query a row as JSON by id from the results table.
    pub fn get_node_json(&self, id: &str) -> Result<Option<String>> {
        let result = self.conn.query_row(
            "SELECT * FROM results WHERE rowid = ?1 OR id = ?1 LIMIT 1",
            [id],
            |row| {
                let col_count = row.as_ref().column_count();
                let mut map = serde_json::Map::new();
                for i in 0..col_count {
                    let name = row.as_ref().column_name(i).unwrap_or("?").to_string();
                    let val: String = row.get::<_, String>(i).unwrap_or_default();
                    map.insert(name, serde_json::Value::String(val));
                }
                Ok(serde_json::Value::Object(map).to_string())
            },
        );

        match result {
            Ok(json) => Ok(Some(json)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

// ---------------------------------------------------------------------------
// C FFI — context-handle based API for the `nodes` table
//
// Mirrors the Graph trait (graph.rs) as a C-compatible API.
// Consumers: tree-sitter (C), jujutsu (Rust lib or C), LSP servers, AI agents.
// Go (mache) reads the shared arena directly — no CGO needed.
//
// Each handle owns a SqliteGraphAdapter with its own Connection behind a Mutex,
// so multiple threads can query in parallel without contention.
//
// Return conventions:
//   >= 0  success (bytes written)
//   -1    error (query failed, buffer too small, not found, invalid UTF-8)
//   -2    null context handle
// ---------------------------------------------------------------------------

use graph::SqliteGraphAdapter;

/// JSON output format for C FFI node responses.
#[derive(Serialize)]
struct NodeJson {
    id: String,
    name: String,
    kind: u8,
    size: u64,
    mtime: i64,
}

impl NodeJson {
    fn from_node(node: &graph::Node) -> Self {
        Self {
            id: node.id.clone(),
            name: node.name.clone(),
            kind: if node.is_dir { 1 } else { 0 },
            size: node.size,
            mtime: node.mtime_nanos,
        }
    }
}

/// Opaque handle to a ley-line graph context.
/// Wraps SqliteGraphAdapter which queries the `nodes` table.
pub struct LeylineCtx {
    adapter: SqliteGraphAdapter,
}

/// Open the arena and return an opaque handle.
///
/// Returns null on failure. Caller must call `leyline_close` to free.
///
/// # Safety
/// `path` must be a valid null-terminated C string pointing to the control file.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn leyline_open(path: *const c_char) -> *mut LeylineCtx {
    let c_path = unsafe { CStr::from_ptr(path) };
    let r_path = match c_path.to_str() {
        Ok(s) => Path::new(s),
        Err(_) => return std::ptr::null_mut(),
    };

    match SqliteGraphAdapter::from_arena(r_path) {
        Ok(adapter) => Box::into_raw(Box::new(LeylineCtx { adapter })),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Close a handle, freeing the underlying connection.
///
/// # Safety
/// `ctx` must be a valid pointer from `leyline_open`, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn leyline_close(ctx: *mut LeylineCtx) {
    if !ctx.is_null() {
        drop(unsafe { Box::from_raw(ctx) });
    }
}

/// Helper: extract a Rust str from a C string pointer, returning -1 on failure.
unsafe fn cstr_to_str<'a>(ptr: *const c_char) -> Result<&'a str, i32> {
    let c = unsafe { CStr::from_ptr(ptr) };
    c.to_str().map_err(|_| -1)
}

/// Helper: write bytes into an output buffer, returning byte count or -1 if too large.
unsafe fn write_out(data: &[u8], out_buf: *mut u8, len: usize) -> i32 {
    if data.len() > len {
        return -1;
    }
    unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), out_buf, data.len()) };
    data.len() as i32
}

/// Get a node by ID. Writes JSON `{"id":"...","name":"...","kind":0|1,"size":N,"mtime":N}`.
///
/// # Safety
/// `ctx` from `leyline_open`. `id` null-terminated. `out_buf` has `len` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn leyline_get_node(
    ctx: *const LeylineCtx,
    id: *const c_char,
    out_buf: *mut u8,
    len: usize,
) -> i32 {
    let ctx = match unsafe { ctx.as_ref() } {
        Some(c) => c,
        None => return -2,
    };
    let id_str = match unsafe { cstr_to_str(id) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    use graph::Graph;
    match ctx.adapter.get_node(id_str) {
        Ok(Some(node)) => {
            let json = match serde_json::to_string(&NodeJson::from_node(&node)) {
                Ok(j) => j,
                Err(_) => return -1,
            };
            unsafe { write_out(json.as_bytes(), out_buf, len) }
        }
        Ok(None) => -1,
        Err(_) => -1,
    }
}

/// List children of a node. Writes JSON array of node objects.
///
/// # Safety
/// `ctx` from `leyline_open`. `parent_id` null-terminated. `out_buf` has `len` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn leyline_list_children(
    ctx: *const LeylineCtx,
    parent_id: *const c_char,
    out_buf: *mut u8,
    len: usize,
) -> i32 {
    let ctx = match unsafe { ctx.as_ref() } {
        Some(c) => c,
        None => return -2,
    };
    let pid = match unsafe { cstr_to_str(parent_id) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    use graph::Graph;
    match ctx.adapter.list_children(pid) {
        Ok(children) => {
            let nodes: Vec<NodeJson> = children.iter().map(NodeJson::from_node).collect();
            let json = match serde_json::to_string(&nodes) {
                Ok(j) => j,
                Err(_) => return -1,
            };
            unsafe { write_out(json.as_bytes(), out_buf, len) }
        }
        Err(_) => -1,
    }
}

/// Look up a child by name under a parent. Writes JSON node object.
///
/// # Safety
/// `ctx` from `leyline_open`. `parent_id` and `name` null-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn leyline_lookup_child(
    ctx: *const LeylineCtx,
    parent_id: *const c_char,
    name: *const c_char,
    out_buf: *mut u8,
    len: usize,
) -> i32 {
    let ctx = match unsafe { ctx.as_ref() } {
        Some(c) => c,
        None => return -2,
    };
    let pid = match unsafe { cstr_to_str(parent_id) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let name_str = match unsafe { cstr_to_str(name) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    use graph::Graph;
    match ctx.adapter.lookup_child(pid, name_str) {
        Ok(Some(node)) => {
            let json = match serde_json::to_string(&NodeJson::from_node(&node)) {
                Ok(j) => j,
                Err(_) => return -1,
            };
            unsafe { write_out(json.as_bytes(), out_buf, len) }
        }
        Ok(None) => -1,
        Err(_) => -1,
    }
}

/// Read file content (the `record` column) for a node.
///
/// # Safety
/// `ctx` from `leyline_open`. `id` null-terminated. `out_buf` has `len` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn leyline_read_content(
    ctx: *const LeylineCtx,
    id: *const c_char,
    out_buf: *mut u8,
    len: usize,
    offset: u64,
) -> i32 {
    let ctx = match unsafe { ctx.as_ref() } {
        Some(c) => c,
        None => return -2,
    };
    let id_str = match unsafe { cstr_to_str(id) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    use graph::Graph;
    let mut buf = vec![0u8; len];
    match ctx.adapter.read_content(id_str, &mut buf, offset) {
        Ok(n) => {
            unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr(), out_buf, n) };
            n as i32
        }
        Err(_) => -1,
    }
}

/// Legacy alias for `leyline_get_node`. Queries the `nodes` table (not the old `results` table).
///
/// # Safety
/// Same contract as `leyline_get_node`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn leyline_query(
    ctx: *const LeylineCtx,
    id: *const c_char,
    out_buf: *mut u8,
    len: usize,
) -> i32 {
    unsafe { leyline_get_node(ctx, id, out_buf, len) }
}

/// KNN search over the attached VectorIndex.
///
/// Returns a heap-allocated JSON C string: `[{"id":"...","distance":0.023}, ...]`.
/// Returns null if no VectorIndex is attached, ctx is null, dim mismatches, or on error.
/// Caller **must** free the returned string with `leyline_free_string`.
///
/// # Safety
/// `ctx` from `leyline_open`. `query_floats` points to `dim` contiguous floats.
#[cfg(feature = "vec")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn leyline_knn_search(
    ctx: *const LeylineCtx,
    query_floats: *const f32,
    dim: usize,
    k: usize,
) -> *mut std::os::raw::c_char {
    let ctx = match unsafe { ctx.as_ref() } {
        Some(c) => c,
        None => return std::ptr::null_mut(),
    };
    let vectors = match ctx.adapter.vectors() {
        Some(v) => v,
        None => return std::ptr::null_mut(),
    };
    if query_floats.is_null() || dim == 0 {
        return std::ptr::null_mut();
    }
    let query = unsafe { std::slice::from_raw_parts(query_floats, dim) };
    let results = match vectors.search(query, k) {
        Ok(r) => r,
        Err(_) => return std::ptr::null_mut(),
    };
    let json_entries: Vec<serde_json::Value> = results
        .iter()
        .map(|(id, distance)| serde_json::json!({"id": id, "distance": distance}))
        .collect();
    let json = match serde_json::to_string(&json_entries) {
        Ok(j) => j,
        Err(_) => return std::ptr::null_mut(),
    };
    match std::ffi::CString::new(json) {
        Ok(cs) => cs.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Free a string returned by `leyline_knn_search`.
///
/// # Safety
/// `ptr` must be a pointer returned by `leyline_knn_search`, or null.
#[cfg(feature = "vec")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn leyline_free_string(ptr: *mut std::os::raw::c_char) {
    if !ptr.is_null() {
        drop(unsafe { std::ffi::CString::from_raw(ptr) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leyline_schema::create_schema;

    /// Create a minimal SQLite database in memory, serialize it to bytes,
    /// then verify SqliteGraph can deserialize and query it.
    #[test]
    fn round_trip_deserialize() -> Result<()> {
        let source = Connection::open_in_memory()?;
        source.execute_batch(
            "CREATE TABLE results (id TEXT PRIMARY KEY, name TEXT, value TEXT);
             INSERT INTO results VALUES ('CVE-2024-0001', 'Test Vuln', 'critical');
             INSERT INTO results VALUES ('CVE-2024-0002', 'Another', 'high');",
        )?;

        let data = source.serialize(DatabaseName::Main)?;
        let bytes = data.as_ref();
        assert!(!bytes.is_empty(), "serialized DB should not be empty");

        let graph = SqliteGraph::from_bytes(bytes)?;

        let tables = graph.tables()?;
        assert!(tables.contains(&"results".to_string()));

        assert_eq!(graph.row_count("results")?, 2);

        let name: String = graph.conn().query_row(
            "SELECT name FROM results WHERE id = ?1",
            ["CVE-2024-0001"],
            |row| row.get(0),
        )?;
        assert_eq!(name, "Test Vuln");

        Ok(())
    }

    /// Verify get_node_json returns the expected JSON.
    #[test]
    fn get_node_json_round_trip() -> Result<()> {
        let source = Connection::open_in_memory()?;
        source.execute_batch(
            "CREATE TABLE results (id TEXT PRIMARY KEY, name TEXT, value TEXT);
             INSERT INTO results VALUES ('node-1', 'Alpha', '42');",
        )?;

        let data = source.serialize(DatabaseName::Main)?;
        let graph = SqliteGraph::from_bytes(data.as_ref())?;

        let json = graph.get_node_json("node-1")?;
        assert!(json.is_some());
        let json = json.unwrap();
        assert!(json.contains("\"id\":\"node-1\""));
        assert!(json.contains("\"name\":\"Alpha\""));

        // Missing node returns None
        assert!(graph.get_node_json("missing")?.is_none());

        Ok(())
    }

    /// Verify that querying a garbage-deserialized DB fails gracefully.
    /// Note: sqlite3_deserialize accepts any buffer; errors surface on query.
    #[test]
    fn rejects_invalid_bytes_on_query() {
        let garbage = b"this is not a sqlite database at all!!";
        let graph = SqliteGraph::from_bytes(garbage);
        // Deserialization may succeed (sqlite accepts the buffer lazily),
        // but querying must fail.
        if let Ok(g) = graph {
            let result = g.tables();
            assert!(result.is_err(), "querying garbage DB should fail");
        }
        // If from_bytes itself fails, that's also acceptable.
    }

    /// Simulate a full arena layout: [Header][Buffer0][Buffer1]
    /// and verify SqliteGraph can read the active buffer.
    #[test]
    fn arena_buffer_extraction() -> Result<()> {
        let source = Connection::open_in_memory()?;
        source.execute_batch(
            "CREATE TABLE results (id TEXT PRIMARY KEY, data TEXT);
             INSERT INTO results VALUES ('node-1', 'hello from buffer 1');",
        )?;
        let serialized = source.serialize(DatabaseName::Main)?;
        let db_bytes = serialized.as_ref();

        // Build a fake arena: 4096-byte header + two equal buffers
        let buf_size = db_bytes.len().max(4096);
        let mut arena = vec![0u8; 4096 + buf_size * 2];

        // Write header (active_buffer = 1, so buffer 1 is live)
        let header = ArenaHeader {
            magic: ArenaHeader::MAGIC,
            version: ArenaHeader::VERSION,
            active_buffer: 1,
            padding: [0; 2],
            sequence: 42,
        };
        let header_bytes: &[u8; std::mem::size_of::<ArenaHeader>()] =
            bytemuck::bytes_of(&header).try_into().unwrap();
        arena[..header_bytes.len()].copy_from_slice(header_bytes);

        // Write DB into buffer 1 (offset = 4096 + buf_size)
        let buf1_offset = 4096 + buf_size;
        arena[buf1_offset..buf1_offset + db_bytes.len()].copy_from_slice(db_bytes);

        // Extract buffer 1 and deserialize
        let parsed_header: &ArenaHeader =
            bytemuck::from_bytes(&arena[..std::mem::size_of::<ArenaHeader>()]);
        let file_size = arena.len() as u64;
        let offset = parsed_header.active_buffer_offset(file_size).unwrap();
        let bsz = ArenaHeader::buffer_size(file_size);
        let active_buf = &arena[offset as usize..(offset + bsz) as usize];

        let graph = SqliteGraph::from_bytes(active_buf)?;
        let data: String =
            graph
                .conn()
                .query_row("SELECT data FROM results WHERE id = 'node-1'", [], |row| {
                    row.get(0)
                })?;
        assert_eq!(data, "hello from buffer 1");

        Ok(())
    }

    /// Helper: create an in-memory DB with the `nodes` table and return a LeylineCtx pointer.
    fn make_test_ctx() -> (*mut LeylineCtx, rusqlite::Connection) {
        let source = Connection::open_in_memory().unwrap();
        create_schema(&source).unwrap();
        source
            .execute_batch(
                "INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('vulns', '', 'vulns', 1, 0, 1000, NULL);
                INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('vulns/CVE-1', 'vulns', 'CVE-1', 0, 23, 2000, '{\"severity\":\"critical\"}');
                INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('vulns/CVE-2', 'vulns', 'CVE-2', 0, 10, 3000, '{\"severity\":\"high\"}');",
            )
            .unwrap();

        let data = source.serialize(DatabaseName::Main).unwrap();
        let graph = SqliteGraph::from_bytes(data.as_ref()).unwrap();
        let adapter = graph::SqliteGraphAdapter::new(graph);
        let ctx = Box::into_raw(Box::new(LeylineCtx { adapter }));
        (ctx, source)
    }

    /// Verify the C FFI context-handle lifecycle: open, query, close.
    #[test]
    fn ffi_context_handle_lifecycle() -> Result<()> {
        let (ctx, _source) = make_test_ctx();

        let mut buf = [0u8; 512];
        let id = std::ffi::CString::new("vulns/CVE-1").unwrap();

        // leyline_get_node
        let n = unsafe { leyline_get_node(ctx, id.as_ptr(), buf.as_mut_ptr(), buf.len()) };
        assert!(n > 0, "get_node should succeed");
        let json = std::str::from_utf8(&buf[..n as usize])?;
        assert!(json.contains("\"id\":\"vulns/CVE-1\""));
        assert!(json.contains("\"kind\":0"));
        assert!(json.contains("\"size\":23"));

        // leyline_query (legacy alias) should give same result
        let n2 = unsafe { leyline_query(ctx, id.as_ptr(), buf.as_mut_ptr(), buf.len()) };
        assert_eq!(n, n2, "legacy alias should match get_node");

        // Null ctx returns -2
        let n =
            unsafe { leyline_get_node(std::ptr::null(), id.as_ptr(), buf.as_mut_ptr(), buf.len()) };
        assert_eq!(n, -2);

        // Clean up
        unsafe { leyline_close(ctx as *mut _) };

        Ok(())
    }

    /// Verify leyline_list_children returns a JSON array.
    #[test]
    fn ffi_list_children() -> Result<()> {
        let (ctx, _source) = make_test_ctx();

        let mut buf = [0u8; 1024];
        let parent = std::ffi::CString::new("vulns").unwrap();

        let n = unsafe { leyline_list_children(ctx, parent.as_ptr(), buf.as_mut_ptr(), buf.len()) };
        assert!(n > 0, "list_children should succeed");
        let json = std::str::from_utf8(&buf[..n as usize])?;
        assert!(json.starts_with('['));
        assert!(json.ends_with(']'));
        assert!(json.contains("CVE-1"));
        assert!(json.contains("CVE-2"));

        unsafe { leyline_close(ctx as *mut _) };
        Ok(())
    }

    /// Verify leyline_lookup_child finds a child by name.
    #[test]
    fn ffi_lookup_child() -> Result<()> {
        let (ctx, _source) = make_test_ctx();

        let mut buf = [0u8; 512];
        let parent = std::ffi::CString::new("vulns").unwrap();
        let name = std::ffi::CString::new("CVE-1").unwrap();

        let n = unsafe {
            leyline_lookup_child(
                ctx,
                parent.as_ptr(),
                name.as_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
            )
        };
        assert!(n > 0, "lookup_child should succeed");
        let json = std::str::from_utf8(&buf[..n as usize])?;
        assert!(json.contains("\"id\":\"vulns/CVE-1\""));

        // Missing child returns -1
        let missing = std::ffi::CString::new("nope").unwrap();
        let n = unsafe {
            leyline_lookup_child(
                ctx,
                parent.as_ptr(),
                missing.as_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
            )
        };
        assert_eq!(n, -1);

        unsafe { leyline_close(ctx as *mut _) };
        Ok(())
    }

    /// Verify JSON escaping handles \n, \t, \, and " correctly via serde_json.
    #[test]
    fn ffi_json_escaping() -> Result<()> {
        let source = Connection::open_in_memory().unwrap();
        // Use schema helper, then parameterized inserts for special chars
        create_schema(&source).unwrap();
        source
            .execute_batch("INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('root', '', 'root', 1, 0, 1000, NULL);")
            .unwrap();

        // Insert nodes with real special characters via params
        source
            .execute(
                "INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES (?1, 'root', ?2, 0, 0, 2000, NULL)",
                rusqlite::params!["root/tricky", "line1\nline2"],
            )
            .unwrap();
        source
            .execute(
                "INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES (?1, 'root', ?2, 0, 0, 3000, NULL)",
                rusqlite::params!["root/bs", "back\\slash"],
            )
            .unwrap();
        source
            .execute(
                "INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES (?1, 'root', ?2, 0, 0, 4000, NULL)",
                rusqlite::params!["root/qt", "has\"quote"],
            )
            .unwrap();
        source
            .execute(
                "INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES (?1, 'root', ?2, 0, 0, 5000, NULL)",
                rusqlite::params!["root/tab", "col1\tcol2"],
            )
            .unwrap();

        let data = source.serialize(DatabaseName::Main).unwrap();
        let graph = SqliteGraph::from_bytes(data.as_ref()).unwrap();
        let adapter = graph::SqliteGraphAdapter::new(graph);
        let ctx = Box::into_raw(Box::new(LeylineCtx { adapter }));

        let mut buf = [0u8; 512];

        // Test node with newline in name
        let id = std::ffi::CString::new("root/tricky").unwrap();
        let n = unsafe { leyline_get_node(ctx, id.as_ptr(), buf.as_mut_ptr(), buf.len()) };
        assert!(n > 0, "get_node should succeed for tricky name");
        let json = std::str::from_utf8(&buf[..n as usize])?;
        // serde_json escapes \n as \\n in the JSON string
        assert!(json.contains(r#"\n"#), "newline should be escaped: {json}");
        // Verify it parses as valid JSON and round-trips
        let parsed: serde_json::Value = serde_json::from_str(json)?;
        assert_eq!(parsed["name"], "line1\nline2");

        // Test node with backslash in name
        let id = std::ffi::CString::new("root/bs").unwrap();
        let n = unsafe { leyline_get_node(ctx, id.as_ptr(), buf.as_mut_ptr(), buf.len()) };
        assert!(n > 0);
        let json = std::str::from_utf8(&buf[..n as usize])?;
        let parsed: serde_json::Value = serde_json::from_str(json)?;
        assert_eq!(parsed["name"], "back\\slash");

        // Test node with quote in name
        let id = std::ffi::CString::new("root/qt").unwrap();
        let n = unsafe { leyline_get_node(ctx, id.as_ptr(), buf.as_mut_ptr(), buf.len()) };
        assert!(n > 0);
        let json = std::str::from_utf8(&buf[..n as usize])?;
        let parsed: serde_json::Value = serde_json::from_str(json)?;
        assert_eq!(parsed["name"], "has\"quote");

        // Test node with tab in name
        let id = std::ffi::CString::new("root/tab").unwrap();
        let n = unsafe { leyline_get_node(ctx, id.as_ptr(), buf.as_mut_ptr(), buf.len()) };
        assert!(n > 0);
        let json = std::str::from_utf8(&buf[..n as usize])?;
        let parsed: serde_json::Value = serde_json::from_str(json)?;
        assert_eq!(parsed["name"], "col1\tcol2");

        // Test list_children also produces valid JSON with special chars
        let parent = std::ffi::CString::new("root").unwrap();
        let n = unsafe { leyline_list_children(ctx, parent.as_ptr(), buf.as_mut_ptr(), buf.len()) };
        assert!(n > 0);
        let json = std::str::from_utf8(&buf[..n as usize])?;
        let parsed: serde_json::Value = serde_json::from_str(json)?;
        assert!(parsed.is_array());
        assert_eq!(parsed.as_array().unwrap().len(), 4);

        unsafe { leyline_close(ctx as *mut _) };
        Ok(())
    }

    /// Verify leyline_read_content reads the record column.
    #[test]
    fn ffi_read_content() -> Result<()> {
        let (ctx, _source) = make_test_ctx();

        let mut buf = [0u8; 256];
        let id = std::ffi::CString::new("vulns/CVE-1").unwrap();

        let n = unsafe { leyline_read_content(ctx, id.as_ptr(), buf.as_mut_ptr(), buf.len(), 0) };
        assert!(n > 0, "read_content should succeed");
        let content = std::str::from_utf8(&buf[..n as usize])?;
        assert!(content.contains("critical"));

        // Offset read
        let n2 = unsafe { leyline_read_content(ctx, id.as_ptr(), buf.as_mut_ptr(), buf.len(), 5) };
        assert!(n2 > 0);
        assert!(n2 < n, "offset read should return fewer bytes");

        unsafe { leyline_close(ctx as *mut _) };
        Ok(())
    }

    /// Verify KNN search via FFI returns valid JSON results.
    #[cfg(feature = "vec")]
    #[test]
    fn ffi_knn_search() -> Result<()> {
        crate::vector::register_vec();

        let (ctx, _source) = make_test_ctx();

        // Attach a VectorIndex with test embeddings
        let idx = crate::vector::VectorIndex::new(4, None)?;
        idx.insert("vulns/CVE-1", &[1.0, 0.0, 0.0, 0.0])?;
        idx.insert("vulns/CVE-2", &[0.0, 1.0, 0.0, 0.0])?;
        unsafe { &mut *(ctx as *mut LeylineCtx) }
            .adapter
            .attach_vectors(idx);

        // Search nearest to [1, 0, 0, 0]
        let query = [1.0f32, 0.0, 0.0, 0.0];
        let ptr = unsafe { leyline_knn_search(ctx, query.as_ptr(), 4, 5) };
        assert!(!ptr.is_null(), "knn_search should return non-null");

        let c_str = unsafe { std::ffi::CStr::from_ptr(ptr) };
        let json_str = c_str.to_str()?;
        let results: Vec<serde_json::Value> = serde_json::from_str(json_str)?;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["id"], "vulns/CVE-1");
        assert!(results[0]["distance"].as_f64().unwrap() < f64::EPSILON);

        unsafe { leyline_free_string(ptr) };
        unsafe { leyline_close(ctx as *mut _) };
        Ok(())
    }

    /// Verify KNN search returns null when no VectorIndex is attached.
    #[cfg(feature = "vec")]
    #[test]
    fn ffi_knn_search_no_index() {
        let (ctx, _source) = make_test_ctx();

        let query = [1.0f32, 0.0, 0.0, 0.0];
        let ptr = unsafe { leyline_knn_search(ctx, query.as_ptr(), 4, 5) };
        assert!(ptr.is_null(), "should return null without VectorIndex");

        unsafe { leyline_close(ctx as *mut _) };
    }

    /// Verify KNN search returns null on null ctx.
    #[cfg(feature = "vec")]
    #[test]
    fn ffi_knn_search_null_ctx() {
        let query = [1.0f32, 0.0, 0.0, 0.0];
        let ptr = unsafe { leyline_knn_search(std::ptr::null(), query.as_ptr(), 4, 5) };
        assert!(ptr.is_null(), "should return null for null ctx");
    }

    /// Verify leyline_free_string handles null safely.
    #[cfg(feature = "vec")]
    #[test]
    fn ffi_free_string_null_safe() {
        unsafe { leyline_free_string(std::ptr::null_mut()) };
        // No crash = pass
    }
}
