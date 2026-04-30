//! Copy-on-Write staging overlay for atomic multi-node edits (ADR-007).
//!
//! `StagingGraph` wraps a live `Graph` and intercepts writes into a shadow
//! SQLite database. Reads check the shadow first, then fall through to the
//! live graph. Deletions are tracked as tombstones. On commit, all staged
//! changes are batch-spliced into the live graph atomically.

use anyhow::Result;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::graph::{Graph, Node};

/// CoW overlay graph for staging multi-node edits.
///
/// Writes go to a shadow SQLite DB; reads check shadow first, then
/// fall through to the live graph. Tombstones mark deleted nodes.
pub struct StagingGraph {
    live: Arc<dyn Graph>,
    shadow: Mutex<Connection>,
}

impl StagingGraph {
    /// Create a new staging overlay wrapping the given live graph.
    pub fn new(live: Arc<dyn Graph>) -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE shadow_nodes (
                id TEXT PRIMARY KEY,
                parent_id TEXT NOT NULL,
                name TEXT NOT NULL,
                kind INTEGER NOT NULL,
                size INTEGER DEFAULT 0,
                mtime INTEGER NOT NULL,
                record TEXT,
                tombstone INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX idx_shadow_parent ON shadow_nodes(parent_id, name);",
        )?;
        Ok(Self {
            live,
            shadow: Mutex::new(conn),
        })
    }

    /// List IDs of all dirty (modified, non-tombstone) nodes in staging.
    pub fn dirty_nodes(&self) -> Result<Vec<String>> {
        let conn = self.shadow.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id FROM shadow_nodes WHERE tombstone = 0")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// List IDs of all tombstoned (deleted) nodes in staging.
    pub fn tombstone_nodes(&self) -> Result<Vec<String>> {
        let conn = self.shadow.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id FROM shadow_nodes WHERE tombstone = 1")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// Read the staged record for a dirty node (used by batch splice).
    pub fn staged_record(&self, id: &str) -> Result<Option<String>> {
        let conn = self.shadow.lock().unwrap();
        let result = conn.query_row(
            "SELECT record FROM shadow_nodes WHERE id = ?1 AND tombstone = 0",
            [id],
            |r| r.get::<_, Option<String>>(0),
        );
        match result {
            Ok(record) => Ok(record),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Check if a node is tombstoned (deleted in staging).
    pub fn is_tombstone(&self, id: &str) -> bool {
        let conn = self.shadow.lock().unwrap();
        conn.query_row(
            "SELECT tombstone FROM shadow_nodes WHERE id = ?1",
            [id],
            |r| Ok(r.get::<_, i64>(0)? != 0),
        )
        .unwrap_or(false)
    }

    /// Clear all staged changes (dirty nodes and tombstones).
    pub fn discard(&self) -> Result<()> {
        let conn = self.shadow.lock().unwrap();
        conn.execute("DELETE FROM shadow_nodes", [])?;
        Ok(())
    }

    /// Access the shadow Connection (for batch splice in commit).
    pub fn shadow_conn(&self) -> &Mutex<Connection> {
        &self.shadow
    }

    /// Access the live graph.
    pub fn live(&self) -> &Arc<dyn Graph> {
        &self.live
    }

    /// Commit all staged changes to the live graph via batch splice.
    ///
    /// Collects dirty nodes and tombstones from shadow, builds an edit list,
    /// and calls `batch_splice` on the live graph. On success, clears staging.
    /// On failure, staging is preserved for retry.
    pub fn commit(&self) -> Result<()> {
        let mut edits: Vec<(String, Option<String>)> = Vec::new();

        let conn = self.shadow.lock().unwrap();

        // Collect dirty nodes (modified, non-tombstone)
        {
            let mut stmt =
                conn.prepare("SELECT id, record FROM shadow_nodes WHERE tombstone = 0")?;
            let rows = stmt.query_map([], |r| {
                let id: String = r.get(0)?;
                let record: Option<String> = r.get(1)?;
                Ok((id, record))
            })?;
            for row in rows {
                let (id, record) = row?;
                edits.push((id, record));
            }
        }

        // Collect tombstones (deleted nodes) — splice byte range with ""
        {
            let mut stmt = conn.prepare("SELECT id FROM shadow_nodes WHERE tombstone = 1")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for row in rows {
                edits.push((row?, None));
            }
        }
        drop(conn);

        if edits.is_empty() {
            return Ok(());
        }

        // Delegate to the live graph's batch splice
        self.live.batch_splice(&edits)?;

        // Success — clear staging
        self.discard()?;
        Ok(())
    }

    /// Derive parent_id from a node ID (everything before the last '/').
    fn parent_id_of(id: &str) -> &str {
        match id.rfind('/') {
            Some(pos) => &id[..pos],
            None => "",
        }
    }

    /// CoW: ensure a node exists in the shadow DB by copying from live if absent.
    /// Returns true if the node now exists in shadow, false if not found in live.
    fn cow_into_shadow(&self, id: &str) -> Result<bool> {
        {
            let conn = self.shadow.lock().unwrap();
            let exists: bool = conn
                .query_row("SELECT 1 FROM shadow_nodes WHERE id = ?1", [id], |_| {
                    Ok(true)
                })
                .unwrap_or(false);
            if exists {
                return Ok(true);
            }
        }
        // Read from live (no shadow lock held — avoids deadlock)
        let node = self.live.get_node(id)?;
        let Some(node) = node else {
            return Ok(false);
        };

        let kind: i64 = if node.is_dir { 1 } else { 0 };
        let parent_id = Self::parent_id_of(id);

        let record = if !node.is_dir && node.size > 0 {
            let mut buf = vec![0u8; node.size as usize + 1];
            let n = self.live.read_content(id, &mut buf, 0)?;
            buf.truncate(n);
            if n > 0 {
                Some(String::from_utf8_lossy(&buf).into_owned())
            } else {
                None
            }
        } else {
            None
        };

        let conn = self.shadow.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO shadow_nodes \
             (id, parent_id, name, kind, size, mtime, record, tombstone) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
            rusqlite::params![
                id,
                parent_id,
                node.name,
                kind,
                node.size as i64,
                node.mtime_nanos,
                record
            ],
        )?;
        Ok(true)
    }
}

fn now_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as i64
}

impl Graph for StagingGraph {
    fn get_node(&self, id: &str) -> Result<Option<Node>> {
        if id.is_empty() {
            return Ok(Some(Node {
                id: String::new(),
                name: String::new(),
                is_dir: true,
                size: 0,
                mtime_nanos: 0,
            }));
        }

        let conn = self.shadow.lock().unwrap();
        let result = conn.query_row(
            "SELECT id, name, kind, size, mtime, tombstone FROM shadow_nodes WHERE id = ?1",
            [id],
            |r| {
                let tombstone: i64 = r.get(5)?;
                if tombstone != 0 {
                    Ok(None)
                } else {
                    Ok(Some(Node {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        is_dir: r.get::<_, i64>(2)? == 1,
                        size: r.get::<_, i64>(3)?.max(0) as u64,
                        mtime_nanos: r.get(4)?,
                    }))
                }
            },
        );
        drop(conn);

        match result {
            Ok(opt) => Ok(opt),
            Err(rusqlite::Error::QueryReturnedNoRows) => self.live.get_node(id),
            Err(e) => Err(e.into()),
        }
    }

    fn lookup_child(&self, parent_id: &str, name: &str) -> Result<Option<Node>> {
        let conn = self.shadow.lock().unwrap();
        let result = conn.query_row(
            "SELECT id, name, kind, size, mtime, tombstone FROM shadow_nodes \
             WHERE parent_id = ?1 AND name = ?2",
            rusqlite::params![parent_id, name],
            |r| {
                let tombstone: i64 = r.get(5)?;
                if tombstone != 0 {
                    Ok(None)
                } else {
                    Ok(Some(Node {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        is_dir: r.get::<_, i64>(2)? == 1,
                        size: r.get::<_, i64>(3)?.max(0) as u64,
                        mtime_nanos: r.get(4)?,
                    }))
                }
            },
        );
        drop(conn);

        match result {
            // Found in shadow (either live node or tombstone)
            Ok(opt) => Ok(opt),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                // Not in shadow — check live, but verify not tombstoned by ID
                let live = self.live.lookup_child(parent_id, name)?;
                if let Some(ref node) = live
                    && self.is_tombstone(&node.id)
                {
                    return Ok(None);
                }
                Ok(live)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn list_children(&self, parent_id: &str) -> Result<Vec<Node>> {
        // Collect shadow entries for this parent
        let shadow_map: HashMap<String, Option<Node>>;
        {
            let conn = self.shadow.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT id, name, kind, size, mtime, tombstone \
                 FROM shadow_nodes WHERE parent_id = ?1",
            )?;
            let rows = stmt.query_map([parent_id], |r| {
                let id: String = r.get(0)?;
                let tombstone: i64 = r.get(5)?;
                if tombstone != 0 {
                    Ok((id, None))
                } else {
                    Ok((
                        id.clone(),
                        Some(Node {
                            id,
                            name: r.get(1)?,
                            is_dir: r.get::<_, i64>(2)? == 1,
                            size: r.get::<_, i64>(3)?.max(0) as u64,
                            mtime_nanos: r.get(4)?,
                        }),
                    ))
                }
            })?;
            shadow_map = rows.collect::<std::result::Result<_, _>>()?;
        }

        // Get live children, apply shadow overlay
        let live_children = self.live.list_children(parent_id)?;
        let mut result = Vec::new();
        let mut seen = HashSet::new();

        for child in live_children {
            let id = child.id.clone();
            if let Some(entry) = shadow_map.get(&id) {
                if let Some(node) = entry {
                    result.push(node.clone());
                }
            } else {
                result.push(child);
            }
            seen.insert(id);
        }

        // Add shadow-only children (created in staging, not in live)
        for (id, entry) in &shadow_map {
            if !seen.contains(id)
                && let Some(node) = entry
            {
                result.push(node.clone());
            }
        }

        Ok(result)
    }

    fn read_content(&self, id: &str, buf: &mut [u8], offset: u64) -> Result<usize> {
        let conn = self.shadow.lock().unwrap();
        let result = conn.query_row(
            "SELECT record, tombstone FROM shadow_nodes WHERE id = ?1",
            [id],
            |r| {
                let tombstone: i64 = r.get(1)?;
                if tombstone != 0 {
                    Ok(None) // tombstoned
                } else {
                    Ok(r.get::<_, Option<String>>(0)?) // record (may be NULL)
                }
            },
        );
        drop(conn);

        match result {
            Ok(Some(data)) => {
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
            Ok(None) => Ok(0), // tombstone or NULL record (truncated)
            Err(rusqlite::Error::QueryReturnedNoRows) => self.live.read_content(id, buf, offset),
            Err(e) => Err(e.into()),
        }
    }

    fn write_content(&self, id: &str, data: &[u8], offset: u64) -> Result<usize> {
        self.cow_into_shadow(id)?;

        let conn = self.shadow.lock().unwrap();
        let now = now_nanos();

        // Read existing shadow content, patch in the new data
        let existing: Option<String> = conn
            .query_row(
                "SELECT record FROM shadow_nodes WHERE id = ?1 AND tombstone = 0",
                [id],
                |r| r.get(0),
            )
            .ok()
            .flatten();

        let mut content = existing.map(|s| s.into_bytes()).unwrap_or_default();
        let off = offset as usize;
        if off + data.len() > content.len() {
            content.resize(off + data.len(), 0);
        }
        content[off..off + data.len()].copy_from_slice(data);

        let new_str = String::from_utf8_lossy(&content);
        conn.execute(
            "UPDATE shadow_nodes SET record = ?1, size = ?2, mtime = ?3 WHERE id = ?4",
            rusqlite::params![new_str.as_ref(), content.len() as i64, now, id],
        )?;

        Ok(data.len())
    }

    fn create_node(&self, parent_id: &str, name: &str, is_dir: bool) -> Result<String> {
        let id = if parent_id.is_empty() {
            name.to_string()
        } else {
            format!("{parent_id}/{name}")
        };

        let conn = self.shadow.lock().unwrap();
        let now = now_nanos();
        let kind: i64 = if is_dir { 1 } else { 0 };

        conn.execute(
            "INSERT INTO shadow_nodes \
             (id, parent_id, name, kind, size, mtime, record, tombstone) \
             VALUES (?1, ?2, ?3, ?4, 0, ?5, NULL, 0)",
            rusqlite::params![id, parent_id, name, kind, now],
        )?;

        Ok(id)
    }

    fn remove_node(&self, id: &str) -> Result<()> {
        let now = now_nanos();

        // Collect live node + all descendants for cascading tombstone
        let mut to_tombstone: Vec<(String, String, String, i64, i64)> = Vec::new();
        let mut stack = vec![id.to_string()];
        while let Some(pid) = stack.pop() {
            if let Some(node) = self.live.get_node(&pid)? {
                to_tombstone.push((
                    pid.clone(),
                    Self::parent_id_of(&pid).to_string(),
                    node.name,
                    if node.is_dir { 1 } else { 0 },
                    node.size as i64,
                ));
                if node.is_dir {
                    for child in self.live.list_children(&pid)? {
                        stack.push(child.id);
                    }
                }
            }
        }

        // Insert/update tombstones in shadow
        let conn = self.shadow.lock().unwrap();
        for (nid, parent, name, kind, size) in &to_tombstone {
            conn.execute(
                "INSERT INTO shadow_nodes \
                 (id, parent_id, name, kind, size, mtime, record, tombstone) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 1) \
                 ON CONFLICT(id) DO UPDATE SET tombstone = 1, mtime = ?6",
                rusqlite::params![nid, parent, name, kind, size, now],
            )?;
        }

        // Also cascade to any shadow-only descendants (created in staging)
        let prefix = format!("{id}/%");
        conn.execute(
            "UPDATE shadow_nodes SET tombstone = 1, mtime = ?1 \
             WHERE id LIKE ?2 AND tombstone = 0",
            rusqlite::params![now, prefix],
        )?;

        Ok(())
    }

    fn truncate(&self, id: &str) -> Result<()> {
        self.cow_into_shadow(id)?;

        let conn = self.shadow.lock().unwrap();
        let now = now_nanos();
        conn.execute(
            "UPDATE shadow_nodes SET record = NULL, size = 0, mtime = ?1 WHERE id = ?2",
            rusqlite::params![now, id],
        )?;
        Ok(())
    }

    fn flush_node(&self, _id: &str) -> Result<()> {
        Ok(()) // No-op — splice happens in batch on commit
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::MemoryGraph;

    fn test_live() -> Arc<dyn Graph> {
        let mut g = MemoryGraph::new();
        g.add_node(
            Node {
                id: "docs".into(),
                name: "docs".into(),
                is_dir: true,
                size: 0,
                mtime_nanos: 1000,
            },
            "",
            None,
        );
        g.add_node(
            Node {
                id: "docs/readme".into(),
                name: "readme".into(),
                is_dir: false,
                size: 5,
                mtime_nanos: 2000,
            },
            "docs",
            Some(b"hello".to_vec()),
        );
        g.add_node(
            Node {
                id: "docs/notes".into(),
                name: "notes".into(),
                is_dir: false,
                size: 5,
                mtime_nanos: 3000,
            },
            "docs",
            Some(b"world".to_vec()),
        );
        Arc::new(g)
    }

    #[test]
    fn staging_creates_shadow_parent_index() -> Result<()> {
        // Scale-problem pin parallel to the schema/lsp/ts index pins.
        // idx_shadow_parent is what makes lookup_child / list_children
        // O(log N) on the staging shadow rather than full-table scan.
        // At scale (a daemon mid-edit on a 50k-file repo with hundreds
        // of dirty nodes), losing the index would push every staging
        // query to scan every dirty row. A refactor that DROP'd the
        // index from the DDL would still pass behavior tests on small
        // fixtures. Pin existence directly via sqlite_master against
        // the shadow connection.
        let live = test_live();
        let staging = StagingGraph::new(live)?;
        let conn = staging.shadow.lock().unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='index' AND name=?1",
                ["idx_shadow_parent"],
                |r| r.get(0),
            )
            .unwrap();
        assert!(exists, "missing idx_shadow_parent");
        Ok(())
    }

    #[test]
    fn read_through_to_live() -> Result<()> {
        let live = test_live();
        let staging = StagingGraph::new(live)?;

        // get_node reads through to live
        let node = staging.get_node("docs/readme")?.unwrap();
        assert_eq!(node.name, "readme");

        // read_content reads through to live
        let mut buf = [0u8; 64];
        let n = staging.read_content("docs/readme", &mut buf, 0)?;
        assert_eq!(&buf[..n], b"hello");

        // list_children reads through to live
        let children = staging.list_children("docs")?;
        assert_eq!(children.len(), 2);

        // lookup_child reads through to live
        let found = staging.lookup_child("docs", "readme")?.unwrap();
        assert_eq!(found.id, "docs/readme");

        Ok(())
    }

    #[test]
    fn cow_write_shadows_live() -> Result<()> {
        let live = test_live();
        let staging = StagingGraph::new(live.clone())?;

        // Write to staging
        staging.write_content("docs/readme", b"staged", 0)?;

        // Staging returns modified content
        let mut buf = [0u8; 64];
        let n = staging.read_content("docs/readme", &mut buf, 0)?;
        assert_eq!(&buf[..n], b"staged");

        // Live is unchanged
        let n = live.read_content("docs/readme", &mut buf, 0)?;
        assert_eq!(&buf[..n], b"hello");

        // dirty_nodes lists the modified node
        let dirty = staging.dirty_nodes()?;
        assert!(dirty.contains(&"docs/readme".to_string()));

        // staged_record returns the new content
        let record = staging.staged_record("docs/readme")?;
        assert_eq!(record, Some("staged".to_string()));

        Ok(())
    }

    #[test]
    fn cow_write_with_offset() -> Result<()> {
        let live = test_live();
        let staging = StagingGraph::new(live)?;

        // Write at offset — extends content
        staging.write_content("docs/readme", b"XY", 3)?;

        let mut buf = [0u8; 64];
        let n = staging.read_content("docs/readme", &mut buf, 0)?;
        assert_eq!(&buf[..n], b"helXY");

        Ok(())
    }

    #[test]
    fn tombstone_hides_live_node() -> Result<()> {
        let live = test_live();
        let staging = StagingGraph::new(live)?;

        staging.remove_node("docs/readme")?;

        // get_node returns None for tombstoned node
        assert!(staging.get_node("docs/readme")?.is_none());

        // list_children excludes tombstoned node
        let children = staging.list_children("docs")?;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id, "docs/notes");

        // lookup_child returns None for tombstoned node
        assert!(staging.lookup_child("docs", "readme")?.is_none());

        // tombstone_nodes lists it
        let tombs = staging.tombstone_nodes()?;
        assert!(tombs.contains(&"docs/readme".to_string()));

        // is_tombstone returns true
        assert!(staging.is_tombstone("docs/readme"));

        Ok(())
    }

    #[test]
    fn tombstone_cascades_to_children() -> Result<()> {
        let live = test_live();
        let staging = StagingGraph::new(live)?;

        // Remove the parent directory — should cascade to children
        staging.remove_node("docs")?;

        assert!(staging.get_node("docs")?.is_none());
        assert!(staging.get_node("docs/readme")?.is_none());
        assert!(staging.get_node("docs/notes")?.is_none());

        // Root listing should not include docs
        let root_children = staging.list_children("")?;
        assert!(root_children.is_empty());

        Ok(())
    }

    #[test]
    fn create_node_in_staging() -> Result<()> {
        let live = test_live();
        let staging = StagingGraph::new(live)?;

        let id = staging.create_node("docs", "new.txt", false)?;
        assert_eq!(id, "docs/new.txt");

        let node = staging.get_node("docs/new.txt")?.unwrap();
        assert_eq!(node.name, "new.txt");
        assert!(!node.is_dir);

        // Visible in list_children (merged with live)
        let children = staging.list_children("docs")?;
        assert_eq!(children.len(), 3);

        Ok(())
    }

    #[test]
    fn truncate_in_staging() -> Result<()> {
        let live = test_live();
        let staging = StagingGraph::new(live)?;

        staging.truncate("docs/readme")?;

        // Content is gone
        let mut buf = [0u8; 64];
        let n = staging.read_content("docs/readme", &mut buf, 0)?;
        assert_eq!(n, 0);

        // Size is 0
        let node = staging.get_node("docs/readme")?.unwrap();
        assert_eq!(node.size, 0);

        // staged_record returns None (NULL record)
        let record = staging.staged_record("docs/readme")?;
        assert!(record.is_none());

        Ok(())
    }

    #[test]
    fn discard_clears_staging() -> Result<()> {
        let live = test_live();
        let staging = StagingGraph::new(live)?;

        staging.write_content("docs/readme", b"staged", 0)?;
        staging.remove_node("docs/notes")?;
        staging.create_node("docs", "new.txt", false)?;

        assert!(!staging.dirty_nodes()?.is_empty());
        assert!(!staging.tombstone_nodes()?.is_empty());

        staging.discard()?;

        // Everything cleared
        assert!(staging.dirty_nodes()?.is_empty());
        assert!(staging.tombstone_nodes()?.is_empty());

        // Back to live state
        let mut buf = [0u8; 64];
        let n = staging.read_content("docs/readme", &mut buf, 0)?;
        assert_eq!(&buf[..n], b"hello");

        // Tombstoned node is back
        let node = staging.get_node("docs/notes")?.unwrap();
        assert_eq!(node.name, "notes");

        // Created node is gone
        assert!(staging.get_node("docs/new.txt")?.is_none());

        Ok(())
    }

    #[test]
    fn unmodified_staging_is_clean() -> Result<()> {
        let live = test_live();
        let staging = StagingGraph::new(live)?;

        assert!(staging.dirty_nodes()?.is_empty());
        assert!(staging.tombstone_nodes()?.is_empty());

        Ok(())
    }

    #[test]
    fn write_then_truncate_then_rewrite() -> Result<()> {
        let live = test_live();
        let staging = StagingGraph::new(live)?;

        // Write
        staging.write_content("docs/readme", b"first", 0)?;
        let mut buf = [0u8; 64];
        let n = staging.read_content("docs/readme", &mut buf, 0)?;
        assert_eq!(&buf[..n], b"first");

        // Truncate
        staging.truncate("docs/readme")?;
        let n = staging.read_content("docs/readme", &mut buf, 0)?;
        assert_eq!(n, 0);

        // Rewrite
        staging.write_content("docs/readme", b"second", 0)?;
        let n = staging.read_content("docs/readme", &mut buf, 0)?;
        assert_eq!(&buf[..n], b"second");

        Ok(())
    }

    #[test]
    fn tombstone_then_recreate() -> Result<()> {
        let live = test_live();
        let staging = StagingGraph::new(live)?;

        // Remove
        staging.remove_node("docs/readme")?;
        assert!(staging.get_node("docs/readme")?.is_none());

        // Recreate at same path (INSERT OR REPLACE semantics)
        // The shadow still has the tombstone — we need to un-tombstone it
        let conn = staging.shadow.lock().unwrap();
        conn.execute(
            "UPDATE shadow_nodes SET tombstone = 0, record = NULL, size = 0, mtime = ?1 \
             WHERE id = 'docs/readme'",
            rusqlite::params![now_nanos()],
        )?;
        drop(conn);

        let node = staging.get_node("docs/readme")?.unwrap();
        assert_eq!(node.name, "readme");

        Ok(())
    }

    #[test]
    fn shadow_only_child_listed() -> Result<()> {
        let live = test_live();
        let staging = StagingGraph::new(live)?;

        // Create a new file that doesn't exist in live
        staging.create_node("docs", "staging_only.txt", false)?;
        staging.write_content("docs/staging_only.txt", b"new content", 0)?;

        let children = staging.list_children("docs")?;
        let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"staging_only.txt"));
        assert!(names.contains(&"readme"));
        assert!(names.contains(&"notes"));
        assert_eq!(children.len(), 3);

        Ok(())
    }

    // --- Batch splice tests (require `splice` feature + real SqliteGraphAdapter) ---

    #[cfg(feature = "splice")]
    mod batch_splice {
        use super::*;
        use crate::graph::SqliteGraphAdapter;

        /// Create a writable SqliteGraphAdapter from parsed HTML with AST tables.
        fn ast_adapter(html: &[u8]) -> Result<Arc<SqliteGraphAdapter>> {
            let db_bytes = leyline_ts::parse_with_source(
                html,
                leyline_ts::languages::TsLanguage::Html,
                "test.html",
            )?;
            let adapter = SqliteGraphAdapter::new_writable(&db_bytes)?;
            Ok(Arc::new(adapter))
        }

        #[test]
        fn commit_single_node_edit() -> Result<()> {
            let live = ast_adapter(b"<p>hello</p>")?;
            let staging = StagingGraph::new(live.clone())?;

            // Stage an edit to the text node
            staging.write_content("element/text", b"world", 0)?;

            // Verify staging has the edit
            let dirty = staging.dirty_nodes()?;
            assert!(dirty.contains(&"element/text".to_string()));

            // Commit — triggers batch splice on live
            staging.commit()?;

            // Staging is cleared
            assert!(staging.dirty_nodes()?.is_empty());

            // Live source is updated
            let mut buf = [0u8; 256];
            let n = live.read_content("element/text", &mut buf, 0)?;
            assert_eq!(&buf[..n], b"world");

            Ok(())
        }

        #[test]
        fn commit_multiple_sibling_edits() -> Result<()> {
            // Two sibling elements — edit both atomically
            let live = ast_adapter(b"<div><p>aaa</p><p>bbb</p></div>")?;

            // Find the text nodes (disambiguated siblings)
            let children = live.list_children("element")?;
            let text_nodes: Vec<String> = children
                .iter()
                .flat_map(|c| {
                    let id = c.id.clone();
                    // List children of each element_N to find text nodes
                    live.list_children(&id)
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|n| !n.is_dir)
                        .map(|n| n.id)
                        .collect::<Vec<_>>()
                })
                .collect();

            assert!(
                text_nodes.len() >= 2,
                "expected at least 2 text nodes, got: {text_nodes:?}"
            );

            let staging = StagingGraph::new(live.clone())?;

            // Stage edits to both text nodes
            for text_id in &text_nodes {
                staging.write_content(text_id, b"edited", 0)?;
            }

            // Commit — all edits applied atomically
            staging.commit()?;

            // All text nodes updated in live
            for text_id in &text_nodes {
                let mut buf = [0u8; 64];
                let n = live.read_content(text_id, &mut buf, 0)?;
                assert_eq!(&buf[..n], b"edited");
            }

            Ok(())
        }

        #[test]
        fn commit_empty_staging_is_noop() -> Result<()> {
            let live = ast_adapter(b"<p>hello</p>")?;
            let staging = StagingGraph::new(live)?;

            // Commit with nothing staged — should succeed silently
            staging.commit()?;

            Ok(())
        }

        #[test]
        fn failed_commit_preserves_staging() -> Result<()> {
            let live = ast_adapter(b"<p>hello</p>")?;
            let staging = StagingGraph::new(live)?;

            // Stage an overlapping edit — edit both parent and child
            // element contains element/text, so their byte ranges overlap
            staging.write_content("element", b"<div>replaced</div>", 0)?;
            staging.write_content("element/text", b"also replaced", 0)?;

            // Commit should fail (overlapping byte ranges)
            let result = staging.commit();
            assert!(result.is_err(), "commit should fail with overlapping edits");

            // Staging should still have the edits
            let dirty = staging.dirty_nodes()?;
            assert!(
                dirty.len() >= 2,
                "staging should be preserved after failed commit"
            );

            Ok(())
        }
    }
}
