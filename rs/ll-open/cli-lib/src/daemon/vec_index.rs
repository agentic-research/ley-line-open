//! Isolated vector store for KNN search over graph nodes.
//!
//! Holds its own SQLite connection (in-memory or disk-backed) with a
//! `vec0` virtual table. Sidecar by design: vec0 cannot survive
//! `sqlite3_serialize`/`deserialize`, so the index never enters the
//! living db or the arena. The EmbeddingPass writes here, and the
//! `vec_search` op reads here.

use anyhow::{Result, ensure};
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::Mutex;

/// Register the sqlite-vec extension for all future connections.
/// Must be called once before any `Connection` is opened.
/// Idempotent — safe to call multiple times.
pub fn register_vec() {
    use rusqlite::ffi::sqlite3_auto_extension;
    use sqlite_vec::sqlite3_vec_init;
    unsafe {
        sqlite3_auto_extension(Some(std::mem::transmute::<
            *const (),
            unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *mut std::os::raw::c_char,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> std::os::raw::c_int,
        >(sqlite3_vec_init as *const ())));
    }
}

/// An isolated vector index backed by its own SQLite connection.
///
/// Uses `vec0` virtual tables for KNN search. The index is completely
/// separate from the arena's SQLite database — vectors never enter
/// serialization or buffer swaps.
pub struct VectorIndex {
    conn: Mutex<Connection>,
    dimensions: usize,
}

impl VectorIndex {
    /// Create a new vector index.
    ///
    /// - `dimensions`: embedding vector length (e.g. 384 for MiniLM-L6-v2)
    /// - `path`: if `Some`, opens a disk-backed SQLite file (persists across restarts).
    ///   if `None`, uses an in-memory database (ephemeral).
    ///
    /// `register_vec()` MUST have been called before this.
    pub fn new(dimensions: usize, path: Option<PathBuf>) -> Result<Self> {
        let conn = match path {
            Some(ref p) => Connection::open(p)?,
            None => Connection::open_in_memory()?,
        };
        let ddl = format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS node_embeddings USING vec0(\
                node_id TEXT PRIMARY KEY,\
                embedding float[{dimensions}]\
            );"
        );
        conn.execute_batch(&ddl)?;
        Ok(Self {
            conn: Mutex::new(conn),
            dimensions,
        })
    }

    /// The dimensionality of vectors in this index.
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// Insert or replace an embedding for a node.
    ///
    /// vec0 virtual tables do not support `INSERT OR REPLACE`, so we
    /// delete any existing row first, then insert.
    pub fn insert(&self, node_id: &str, embedding: &[f32]) -> Result<()> {
        ensure!(
            embedding.len() == self.dimensions,
            "expected {}-dim embedding, got {}",
            self.dimensions,
            embedding.len()
        );
        let bytes: &[u8] = bytemuck::cast_slice(embedding);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM node_embeddings WHERE node_id = ?1",
            rusqlite::params![node_id],
        )?;
        conn.execute(
            "INSERT INTO node_embeddings(node_id, embedding) VALUES (?1, ?2)",
            rusqlite::params![node_id, bytes],
        )?;
        Ok(())
    }

    /// Retrieve an embedding for a specific node by ID.
    pub fn get(&self, node_id: &str) -> Result<Option<Vec<f32>>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT embedding FROM node_embeddings WHERE node_id = ?1")?;
        let mut rows = stmt.query(rusqlite::params![node_id])?;

        if let Some(row) = rows.next()? {
            let bytes: Vec<u8> = row.get(0)?;
            let floats: &[f32] = bytemuck::cast_slice(&bytes);
            Ok(Some(floats.to_vec()))
        } else {
            Ok(None)
        }
    }

    /// KNN search: find the `k` nearest nodes to the query embedding.
    /// Returns `(node_id, distance)` pairs sorted by distance.
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<(String, f64)>> {
        ensure!(
            query.len() == self.dimensions,
            "expected {}-dim query, got {}",
            self.dimensions,
            query.len()
        );
        let bytes: &[u8] = bytemuck::cast_slice(query);
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT node_id, distance FROM node_embeddings \
             WHERE embedding MATCH ?1 ORDER BY distance LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![bytes, k as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Drop all embeddings (e.g., on generation change).
    pub fn clear(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM node_embeddings", [])?;
        Ok(())
    }

    /// Number of stored embeddings.
    pub fn len(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM node_embeddings", [], |r| r.get(0))?;
        Ok(count as usize)
    }

    /// Returns true if the index contains no embeddings.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() {
        register_vec();
    }

    #[test]
    fn insert_and_search() -> Result<()> {
        setup();
        let idx = VectorIndex::new(4, None)?;

        idx.insert("node-a", &[1.0, 0.0, 0.0, 0.0])?;
        idx.insert("node-b", &[0.0, 1.0, 0.0, 0.0])?;
        idx.insert("node-c", &[0.9, 0.1, 0.0, 0.0])?;

        // Query closest to [1, 0, 0, 0] — should be node-a, then node-c
        let results = idx.search(&[1.0, 0.0, 0.0, 0.0], 3)?;
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].0, "node-a");
        assert_eq!(results[1].0, "node-c");
        assert_eq!(results[2].0, "node-b");

        // Distance to self should be 0
        assert!((results[0].1) < f64::EPSILON);

        Ok(())
    }

    #[test]
    fn search_empty_index() -> Result<()> {
        setup();
        let idx = VectorIndex::new(4, None)?;
        let results = idx.search(&[1.0, 0.0, 0.0, 0.0], 10)?;
        assert!(results.is_empty());
        Ok(())
    }

    #[test]
    fn clear_removes_all() -> Result<()> {
        setup();
        let idx = VectorIndex::new(4, None)?;
        idx.insert("node-a", &[1.0, 0.0, 0.0, 0.0])?;
        idx.insert("node-b", &[0.0, 1.0, 0.0, 0.0])?;
        assert_eq!(idx.len()?, 2);

        idx.clear()?;
        assert_eq!(idx.len()?, 0);
        assert!(idx.is_empty()?);
        Ok(())
    }

    #[test]
    fn insert_replaces_existing() -> Result<()> {
        setup();
        let idx = VectorIndex::new(4, None)?;

        idx.insert("node-a", &[1.0, 0.0, 0.0, 0.0])?;
        idx.insert("node-a", &[0.0, 1.0, 0.0, 0.0])?;
        assert_eq!(idx.len()?, 1);

        // Search should find the updated embedding
        let results = idx.search(&[0.0, 1.0, 0.0, 0.0], 1)?;
        assert_eq!(results[0].0, "node-a");
        assert!(results[0].1 < f64::EPSILON);
        Ok(())
    }

    #[test]
    fn dimension_mismatch_rejected() -> Result<()> {
        setup();
        let idx = VectorIndex::new(4, None)?;

        // Wrong dimension on insert
        let result = idx.insert("node-a", &[1.0, 0.0]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("expected 4-dim"));

        // Wrong dimension on search
        let result = idx.search(&[1.0, 0.0, 0.0], 5);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("expected 4-dim"));

        Ok(())
    }

    #[test]
    fn disk_backed_persistence() -> Result<()> {
        setup();
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("vectors.db");

        // Create, insert, drop
        {
            let idx = VectorIndex::new(4, Some(db_path.clone()))?;
            idx.insert("node-a", &[1.0, 0.0, 0.0, 0.0])?;
            idx.insert("node-b", &[0.0, 1.0, 0.0, 0.0])?;
            assert_eq!(idx.len()?, 2);
        }

        // Reopen — data should survive
        {
            let idx = VectorIndex::new(4, Some(db_path))?;
            assert_eq!(idx.len()?, 2);
            let results = idx.search(&[1.0, 0.0, 0.0, 0.0], 1)?;
            assert_eq!(results[0].0, "node-a");
        }

        Ok(())
    }

    #[test]
    fn custom_dimensions() -> Result<()> {
        setup();
        let idx = VectorIndex::new(768, None)?;
        assert_eq!(idx.dimensions(), 768);

        let mut embedding = vec![0.0f32; 768];
        embedding[0] = 1.0;
        idx.insert("node-a", &embedding)?;

        let mut query = vec![0.0f32; 768];
        query[0] = 1.0;
        let results = idx.search(&query, 1)?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "node-a");
        assert!(results[0].1 < f64::EPSILON);
        Ok(())
    }
}
