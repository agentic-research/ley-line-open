//! Embedding enrichment pass — drives `VectorIndex` from the living db.
//!
//! Public-shipping default uses [`ZeroEmbedder`], a no-op model that returns
//! zero vectors. Private extensions override it via [`DaemonExt::embedder`].
//!
//! Schema partition: this pass writes to the **sidecar** [`VectorIndex`] only.
//! It does not own any tables in the living db (vec0 cannot survive
//! `sqlite3_serialize`/`deserialize`). It reads `nodes`/`_source` for file
//! contents.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::Connection;

use super::enrichment::{EnrichmentPass, EnrichmentStats};
use super::vec_index::VectorIndex;
use super::DaemonContext;

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

// ---------------------------------------------------------------------------
// Priority queue for MCP-driven re-embedding
// ---------------------------------------------------------------------------

/// One unit of re-embedding work. Higher `priority` drains first
/// (max-heap on `priority`).
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct EmbedTask {
    pub priority: u64,
    pub node_id: String,
}

impl Ord for EmbedTask {
    fn cmp(&self, other: &Self) -> Ordering {
        // Tie-break on node_id so we have a total order; both a and b's
        // priorities being equal shouldn't make `BinaryHeap` think they
        // are the same element.
        self.priority
            .cmp(&other.priority)
            .then_with(|| self.node_id.cmp(&other.node_id))
    }
}

impl PartialOrd for EmbedTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Type alias for the shared embed-task queue stored on `DaemonContext`.
pub type EmbedQueue = Arc<Mutex<BinaryHeap<EmbedTask>>>;

/// Monotonic counter for embed-task priorities.
///
/// Earlier revisions used wall-clock micros, but NTP steps (or DST jumps,
/// or the user moving the laptop's clock) could reorder tasks
/// unpredictably. A monotonic counter guarantees promotion ordering is
/// strictly newest-first regardless of wall-clock skew. The counter is
/// process-local; restarts reset to zero, which is fine because the
/// queue itself doesn't survive restart.
static EMBED_PRIORITY_COUNTER: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Allocate the next priority value. Strictly increasing across the
/// process lifetime; safe to call from any thread.
fn next_priority() -> u64 {
    // Relaxed is fine — we only need monotonicity within a single
    // process, not synchronization with anything else.
    EMBED_PRIORITY_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Promote `node_id` to the front of the embed queue. Called from query ops
/// when a node is touched, so its embedding gets refreshed soon.
pub fn promote(queue: &EmbedQueue, node_id: &str) {
    let priority = next_priority();
    if let Ok(mut q) = queue.lock() {
        q.push(EmbedTask {
            priority,
            node_id: node_id.to_string(),
        });
    }
}

/// Spawn the background embed-queue drainer. Every 1s it pops up to 32 tasks
/// (highest priority first), looks up each node's source content from the
/// living db, embeds it, and writes the vector to the sidecar index. Emits
/// `daemon.embed.complete` after each non-empty batch.
pub fn start_drain(ctx: Arc<DaemonContext>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        // Don't burst on catch-up — we'd rather smoothly drain than spike.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let emitter = ctx.router.emitter();
        loop {
            tick.tick().await;
            let batch: Vec<EmbedTask> = {
                let mut q = match ctx.embed_queue.lock() {
                    Ok(g) => g,
                    Err(_) => continue,
                };
                let mut out = Vec::with_capacity(32);
                for _ in 0..32 {
                    match q.pop() {
                        Some(t) => out.push(t),
                        None => break,
                    }
                }
                out
            };
            if batch.is_empty() {
                continue;
            }

            let mut count = 0u64;
            for task in batch {
                let content = {
                    let conn = match ctx.live_db.lock() {
                        Ok(g) => g,
                        Err(_) => continue,
                    };
                    conn.query_row(
                        "SELECT record FROM nodes WHERE id = ?1",
                        [&task.node_id],
                        |r| r.get::<_, Option<String>>(0),
                    )
                    .ok()
                    .flatten()
                };
                let Some(content) = content else { continue };
                if content.is_empty() {
                    continue;
                }
                let v = match ctx.embedder.embed(&content) {
                    Ok(v) => v,
                    Err(e) => {
                        log::debug!("embed failed for {}: {e:#}", task.node_id);
                        continue;
                    }
                };
                if let Err(e) = ctx.vec_index.insert(&task.node_id, &v) {
                    log::debug!("vec insert failed for {}: {e:#}", task.node_id);
                    continue;
                }
                count += 1;
            }

            if count > 0 {
                emitter.emit(
                    "daemon.embed.complete",
                    "leyline",
                    serde_json::json!({ "count": count }),
                );
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::vec_index::register_vec;

    // ── Priority counter invariants ────────────────────────────────────
    //
    // These tests establish that promote() ordering is wall-clock-independent
    // (monotonic counter, not SystemTime). The previous implementation could
    // reorder tasks under NTP step / DST jump / user clock change.

    #[test]
    fn next_priority_is_strictly_monotonic() {
        let a = next_priority();
        let b = next_priority();
        let c = next_priority();
        assert!(a < b, "next_priority must be strictly increasing: {a} < {b}");
        assert!(b < c, "next_priority must be strictly increasing: {b} < {c}");
    }

    #[test]
    fn promote_orders_newer_first_in_queue() {
        let queue: EmbedQueue = Arc::new(Mutex::new(BinaryHeap::new()));
        promote(&queue, "first");
        promote(&queue, "second");
        promote(&queue, "third");

        // BinaryHeap is max-heap on priority; later promotions get higher
        // priority numbers, so they pop out first.
        let mut q = queue.lock().unwrap();
        assert_eq!(q.pop().map(|t| t.node_id), Some("third".to_string()));
        assert_eq!(q.pop().map(|t| t.node_id), Some("second".to_string()));
        assert_eq!(q.pop().map(|t| t.node_id), Some("first".to_string()));
    }

    #[test]
    fn promote_concurrent_does_not_collide() {
        // Spawn 4 threads, each promoting 100 ids. All 400 priorities must
        // be unique — a guarantee the previous wall-clock impl could not
        // make if two threads called within the same microsecond.
        let queue: EmbedQueue = Arc::new(Mutex::new(BinaryHeap::new()));
        let mut handles = vec![];
        for t in 0..4 {
            let q = queue.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..100 {
                    promote(&q, &format!("t{t}-{i}"));
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let q = queue.lock().unwrap();
        let priorities: std::collections::HashSet<u64> =
            q.iter().map(|t| t.priority).collect();
        assert_eq!(
            priorities.len(),
            q.len(),
            "all 400 priorities should be unique under contention",
        );
    }

    #[test]
    fn embed_task_ordering_matches_priority() {
        // Direct test of the Ord impl: the heap is keyed on priority, with
        // node_id as tie-breaker. Two tasks with identical priorities must
        // never compare Equal — that would let one shadow the other in
        // BinaryHeap. Since promote() always allocates a fresh priority,
        // the tie-break path is only exercised by direct construction
        // (tests / edge cases).
        let a = EmbedTask { priority: 5, node_id: "a".into() };
        let b = EmbedTask { priority: 5, node_id: "b".into() };
        assert!(a < b, "tie-broken on node_id ascending");

        let high = EmbedTask { priority: 10, node_id: "z".into() };
        let low  = EmbedTask { priority: 1,  node_id: "a".into() };
        assert!(low < high, "lower priority compares less");

        let mut heap: BinaryHeap<EmbedTask> = BinaryHeap::new();
        heap.push(low);
        heap.push(high);
        assert_eq!(heap.pop().unwrap().node_id, "z", "max-heap pops highest");
    }

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
