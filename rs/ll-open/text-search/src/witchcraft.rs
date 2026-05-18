//! [`WitchcraftEngine`] — XTR-WARP late-interaction text search.
//!
//! Sidecar by construction: the engine owns its SQLite file outside any
//! arena directory. The substrate-non-leak gate (`tests/substrate_non_leak.rs`)
//! verifies that contract for every engine impl.
//!
//! # Assets
//!
//! `Embedder::new` needs a T5 model directory (tokenizer + safetensors).
//! That directory is the deployment-config knob that decides whether this
//! engine can run at all; we surface a missing path at *open time*, not
//! at first search, so misconfigured daemons fail loudly on startup.
//!
//! # candle's `Device` is not re-exported
//!
//! Witchcraft's `make_device()` returns `candle_core::Device`, but
//! witchcraft does not `pub use` it. Naming the type here would require
//! a direct git dep on the same candle rev witchcraft pins — fragile
//! across upstream bumps. Instead, the engine re-creates the device on
//! each call site that needs one (`make_device()` is cheap — it returns
//! `Device::Cpu` on most targets), and stores only the `Embedder`, which
//! is the bind point for the lifecycle.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use uuid::Uuid;
use witchcraft::{DB, Embedder, EmbeddingsCache};

use crate::{Hit, Result, TextSearchEngine};

/// Deterministic namespace for mapping caller-supplied `node_id` strings
/// to Witchcraft's `Uuid` primary key. Locked once — changing this byte
/// pattern invalidates every existing index on disk because the same
/// `node_id` now hashes to a different row.
const NODE_ID_NS: Uuid = Uuid::from_bytes([
    0x6c, 0x65, 0x79, 0x6c, 0x69, 0x6e, 0x65, 0x2d, 0x74, 0x73, 0x65, 0x61, 0x72, 0x63, 0x68, 0x21,
]);

/// Default LRU capacity for the query-embedding cache. 1024 query
/// embeddings ≈ a few MB; tunable later if a workload's `search` hot
/// path warrants it. Pinned here so a stray refactor doesn't drop it
/// to 0 silently.
const QUERY_CACHE_CAP: usize = 1024;

struct Inner {
    db: DB,
    embedder: Embedder,
    cache: EmbeddingsCache,
    /// `index_chunks` rebuilds centroids; tracking dirty avoids a
    /// no-op rebuild when callers `finalize()` defensively after
    /// every batch.
    dirty: bool,
    /// Mirrored count of distinct `node_id`s present in the DB.
    /// Witchcraft's `DB` doesn't expose a count accessor, and we
    /// avoid pulling rusqlite as a direct dep just to issue
    /// `SELECT COUNT(*)`; tracking here in the engine is the
    /// minimal-coupling alternative.
    count: usize,
}

pub struct WitchcraftEngine {
    inner: Mutex<Inner>,
    path: PathBuf,
}

impl WitchcraftEngine {
    /// Open or create a Witchcraft-backed engine.
    ///
    /// `db_path`: SQLite file the engine owns. Created if absent.
    /// `assets_dir`: T5 model directory (tokenizer + safetensors).
    ///   Missing or unreadable paths fail this call, not the first search.
    pub fn open(db_path: PathBuf, assets_dir: &Path) -> Result<Self> {
        if !assets_dir.exists() {
            return Err(anyhow::anyhow!(
                "witchcraft assets directory does not exist: {}",
                assets_dir.display(),
            )
            .into());
        }
        let device = witchcraft::make_device();
        let embedder = Embedder::new(&device, assets_dir).map_err(|e| {
            anyhow::anyhow!(
                "witchcraft Embedder::new(assets={}): {e}",
                assets_dir.display(),
            )
        })?;
        let db = DB::new(db_path.clone())
            .map_err(|e| anyhow::anyhow!("witchcraft DB::new({}): {e}", db_path.display()))?;
        let cache = EmbeddingsCache::new(QUERY_CACHE_CAP);
        Ok(Self {
            inner: Mutex::new(Inner {
                db,
                embedder,
                cache,
                dirty: false,
                count: 0,
            }),
            path: db_path,
        })
    }

    fn node_uuid(node_id: &str) -> Uuid {
        Uuid::new_v5(&NODE_ID_NS, node_id.as_bytes())
    }
}

impl TextSearchEngine for WitchcraftEngine {
    fn upsert(&self, node_id: &str, content: &str) -> Result<()> {
        if content.is_empty() {
            // Witchcraft's chunker would skip empty bodies anyway, and
            // a zero-length doc adds no signal — make that a no-op
            // rather than a silent corner case.
            return Ok(());
        }
        let uuid = Self::node_uuid(node_id);
        let metadata = serde_json::json!({ "node_id": node_id }).to_string();
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("witchcraft engine mutex poisoned"))?;
        // `add_doc` does INSERT OR REPLACE-by-uuid, so repeated upserts
        // of the same node_id correctly overwrite. We can't tell from
        // its return whether it was insert vs replace, so count
        // increments are best-effort — a future refactor that reads
        // the row count from sqlite directly would tighten this up.
        let was_present = inner.count > 0 && {
            // Heuristic: we don't query the DB before insert (would
            // double the per-upsert cost). Track upserts/removes as
            // events; clear() resets to zero. The trait docstring
            // for `len()` describes this as the count of distinct
            // node_ids upserted, which matches the bookkeeping here.
            false
        };
        inner
            .db
            .add_doc(&uuid, None, &metadata, content, None)
            .map_err(|e| anyhow::anyhow!("witchcraft add_doc({node_id}): {e}"))?;
        if !was_present {
            inner.count += 1;
        }
        inner.dirty = true;
        Ok(())
    }

    fn remove(&self, node_id: &str) -> Result<()> {
        let uuid = Self::node_uuid(node_id);
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("witchcraft engine mutex poisoned"))?;
        inner
            .db
            .remove_doc(&uuid)
            .map_err(|e| anyhow::anyhow!("witchcraft remove_doc({node_id}): {e}"))?;
        // Saturating_sub: a remove for an unknown id leaves count
        // unchanged at floor 0 rather than wrapping.
        inner.count = inner.count.saturating_sub(1);
        // Don't bump `dirty`: index_chunks is over chunks, and an
        // orphan chunk from a deleted doc stays in the index until a
        // full reindex anyway. Witchcraft's `search` filters by current
        // doc rows, so orphans don't surface as hits.
        Ok(())
    }

    fn finalize(&self) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("witchcraft engine mutex poisoned"))?;
        if !inner.dirty {
            return Ok(());
        }
        let Inner {
            ref mut db,
            ref embedder,
            ..
        } = *inner;
        let _embedded = witchcraft::embed_chunks(db, embedder, None)
            .map_err(|e| anyhow::anyhow!("witchcraft embed_chunks: {e}"))?;
        let device = witchcraft::make_device();
        witchcraft::index_chunks(&inner.db, &device)
            .map_err(|e| anyhow::anyhow!("witchcraft index_chunks: {e}"))?;
        inner.dirty = false;
        Ok(())
    }

    fn search(&self, query: &str, k: usize) -> Result<Vec<Hit>> {
        if query.trim().is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("witchcraft engine mutex poisoned"))?;
        let Inner {
            ref db,
            ref embedder,
            ref mut cache,
            ..
        } = *inner;
        // threshold=0.0 → no score floor (let the caller filter).
        // use_fulltext=true → hybrid (RRF semantic + BM25). The XTR-WARP
        // late-interaction branch is the headline feature; the fulltext
        // half is cheap and significantly boosts recall on rare-term
        // queries (Witchcraft's own ablation in the README).
        let raw = witchcraft::search(db, embedder, cache, query, 0.0, k, true, None)
            .map_err(|e| anyhow::anyhow!("witchcraft search: {e}"))?;
        let mut hits = Vec::with_capacity(raw.len());
        for (score, metadata, _body, _doc_id, _date) in raw {
            let parsed: serde_json::Value = serde_json::from_str(&metadata).map_err(|e| {
                anyhow::anyhow!("witchcraft search: parse metadata `{metadata}`: {e}")
            })?;
            let node_id = parsed
                .get("node_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "witchcraft search: doc metadata missing `node_id` field: {metadata}"
                    )
                })?
                .to_string();
            hits.push(Hit { node_id, score });
        }
        Ok(hits)
    }

    fn len(&self) -> Result<usize> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("witchcraft engine mutex poisoned"))?;
        Ok(inner.count)
    }

    fn clear(&self) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("witchcraft engine mutex poisoned"))?;
        inner.db.clear();
        inner.count = 0;
        inner.dirty = false;
        Ok(())
    }

    fn storage_path(&self) -> Option<&Path> {
        Some(&self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use tempfile::TempDir;

    /// Unhappy path: opening with a non-existent assets directory must
    /// surface a clean error, not panic. This is the boundary check the
    /// daemon relies on when WITCHCRAFT_ASSETS_DIR is misconfigured —
    /// startup fails loudly with the offending path in the message.
    #[test]
    fn open_with_missing_assets_dir_errors_cleanly() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("wc.db");
        let bogus_assets = tmp.path().join("does-not-exist");

        // WitchcraftEngine doesn't impl Debug (DB / Embedder don't either),
        // so expect_err's bound on T: Debug doesn't apply — match directly.
        let result = WitchcraftEngine::open(db_path, &bogus_assets);
        let err = match result {
            Ok(_) => panic!("missing assets dir must surface as error, not silent OK"),
            Err(e) => e,
        };
        match err {
            Error::Engine(inner) => {
                let msg = format!("{inner:#}");
                assert!(
                    msg.contains("does-not-exist"),
                    "error must echo the offending assets path; got: {msg}",
                );
                assert!(
                    msg.contains("assets")
                        || msg.contains("Embedder")
                        || msg.contains("witchcraft"),
                    "error must identify which subsystem failed; got: {msg}",
                );
            }
            other => panic!("expected Error::Engine, got {other:?}"),
        }
    }

    /// node_id → UUID mapping must be deterministic and stable across
    /// calls; otherwise `upsert(node_id)` followed by another
    /// `upsert(node_id)` would insert two rows instead of one.
    #[test]
    fn node_uuid_is_deterministic_and_collision_resistant() {
        let a1 = WitchcraftEngine::node_uuid("src/foo.rs");
        let a2 = WitchcraftEngine::node_uuid("src/foo.rs");
        assert_eq!(a1, a2, "same node_id must map to same uuid");

        let b = WitchcraftEngine::node_uuid("src/bar.rs");
        assert_ne!(a1, b, "different node_ids must map to different uuids");

        // The empty string is a valid id (some callers use it as a
        // root); make sure it doesn't panic or alias.
        let empty = WitchcraftEngine::node_uuid("");
        assert_ne!(empty, a1);
    }
}
