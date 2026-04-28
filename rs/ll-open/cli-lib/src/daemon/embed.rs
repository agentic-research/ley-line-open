//! Embedding enrichment pass — drives `VectorIndex` from the living db.
//!
//! Public-shipping default uses [`ZeroEmbedder`], a no-op model that returns
//! zero vectors. Private extensions override it via [`DaemonExt::embedder`].
//!
//! Schema partition: this pass writes to the **sidecar** [`VectorIndex`] only.
//! It does not own any tables in the living db (vec0 cannot survive
//! `sqlite3_serialize`/`deserialize`). It reads `nodes`/`_source` for file
//! contents.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use rusqlite::Connection;

use super::enrichment::{EnrichmentPass, EnrichmentStats};
use super::vec_index::VectorIndex;

/// An object that can produce a fixed-dimension embedding for text input.
///
/// LLO ships [`ZeroEmbedder`] as the default. Private repos provide a real
/// model implementation through [`DaemonExt::embedder`].
pub trait Embedder: Send + Sync {
    /// Produce an embedding for the given text. Length must equal
    /// `dimensions()`.
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
    /// Embedding length. The [`VectorIndex`] is sized to match.
    fn dimensions(&self) -> usize;
}

/// No-op embedder that returns a zero vector of fixed dimension. Useful for
/// shape testing and CI without a model dependency. Private extensions
/// replace this via [`DaemonExt::embedder`].
pub struct ZeroEmbedder {
    pub dim: usize,
}

impl Embedder for ZeroEmbedder {
    fn embed(&self, _text: &str) -> Result<Vec<f32>> {
        Ok(vec![0.0_f32; self.dim])
    }
    fn dimensions(&self) -> usize {
        self.dim
    }
}

/// Enrichment pass that materializes file embeddings into the sidecar
/// [`VectorIndex`].
///
/// Depends on `tree-sitter` (needs `nodes` populated). Writes to no
/// living-db tables — embeddings live in the vec0 sidecar.
pub struct EmbeddingPass {
    pub index: Arc<VectorIndex>,
    pub embedder: Arc<dyn Embedder>,
}

impl EmbeddingPass {
    pub fn new(index: Arc<VectorIndex>, embedder: Arc<dyn Embedder>) -> Self {
        Self { index, embedder }
    }
}

impl EnrichmentPass for EmbeddingPass {
    fn name(&self) -> &str {
        "embed"
    }

    fn depends_on(&self) -> &[&str] {
        &["tree-sitter"]
    }

    fn reads(&self) -> &[&str] {
        &["nodes", "_source"]
    }

    fn writes(&self) -> &[&str] {
        // Sidecar VectorIndex is disjoint from the living db's table set.
        &[]
    }

    fn run(
        &self,
        conn: &Connection,
        _source_dir: &Path,
        changed_files: Option<&[String]>,
    ) -> Result<EnrichmentStats> {
        let start = Instant::now();

        // Iterate file nodes (kind = 0) with non-empty source content.
        // When `changed_files` scopes the run, only those source paths are
        // considered — this maps to the same file set the parser saw.
        let scope: Option<HashSet<&str>> =
            changed_files.map(|s| s.iter().map(|p| p.as_str()).collect());

        let mut stmt = conn.prepare(
            "SELECT id, record FROM nodes \
             WHERE kind = 0 AND record IS NOT NULL AND record <> ''",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;

        let mut files_processed = 0u64;
        let mut items_added = 0u64;

        for row in rows {
            let (id, content) = row?;
            if let Some(set) = &scope
                && !set.contains(id.as_str())
            {
                continue;
            }

            files_processed += 1;
            let vec = self.embedder.embed(&content)?;
            self.index.insert(&id, &vec)?;
            items_added += 1;
        }

        Ok(EnrichmentStats {
            pass_name: "embed".to_string(),
            files_processed,
            items_added,
            duration_ms: start.elapsed().as_millis() as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::vec_index::register_vec;

    /// Set up a minimal living-db with a couple of file nodes, then run
    /// EmbeddingPass with the default ZeroEmbedder. Verify the index has
    /// the right node ids and vectors of the right shape.
    #[test]
    fn embedding_pass_zero_embedder_populates_index() -> Result<()> {
        register_vec();
        let conn = Connection::open_in_memory()?;
        // Minimal schema — only the nodes table is needed.
        conn.execute_batch(
            "CREATE TABLE nodes (
                id TEXT PRIMARY KEY,
                parent_id TEXT,
                name TEXT,
                kind INTEGER,
                size INTEGER,
                mtime INTEGER,
                record TEXT
            );
            INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record)
                VALUES ('src/a.go', '', 'a.go', 0, 12, 1, 'package main');
            INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record)
                VALUES ('src/b.go', '', 'b.go', 0, 13, 2, 'package other');
            INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record)
                VALUES ('src', '', 'src', 1, 0, 0, NULL);",
        )?;

        let dim = 4;
        let index = Arc::new(VectorIndex::new(dim, None)?);
        let embedder: Arc<dyn Embedder> = Arc::new(ZeroEmbedder { dim });

        let pass = EmbeddingPass::new(index.clone(), embedder);
        let stats = pass.run(&conn, Path::new("/tmp"), None)?;

        assert_eq!(stats.files_processed, 2, "should embed 2 files");
        assert_eq!(stats.items_added, 2);
        assert_eq!(index.len()?, 2);

        // Verify zero vector shape.
        let v = index.get("src/a.go")?.expect("a.go embedding present");
        assert_eq!(v.len(), dim);
        assert!(v.iter().all(|&x| x == 0.0));
        Ok(())
    }

    /// Scoped run only embeds files in the changed set.
    #[test]
    fn embedding_pass_scope_limits_files() -> Result<()> {
        register_vec();
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE nodes (
                id TEXT PRIMARY KEY,
                parent_id TEXT,
                name TEXT,
                kind INTEGER,
                size INTEGER,
                mtime INTEGER,
                record TEXT
            );
            INSERT INTO nodes VALUES ('a.go', '', 'a.go', 0, 1, 1, 'package a');
            INSERT INTO nodes VALUES ('b.go', '', 'b.go', 0, 1, 1, 'package b');
            INSERT INTO nodes VALUES ('c.go', '', 'c.go', 0, 1, 1, 'package c');",
        )?;

        let index = Arc::new(VectorIndex::new(4, None)?);
        let pass = EmbeddingPass::new(index.clone(), Arc::new(ZeroEmbedder { dim: 4 }));
        let stats = pass.run(
            &conn,
            Path::new("/tmp"),
            Some(&["a.go".to_string(), "c.go".to_string()]),
        )?;

        assert_eq!(stats.items_added, 2);
        assert_eq!(index.len()?, 2);
        assert!(index.get("a.go")?.is_some());
        assert!(index.get("c.go")?.is_some());
        assert!(index.get("b.go")?.is_none());
        Ok(())
    }
}
