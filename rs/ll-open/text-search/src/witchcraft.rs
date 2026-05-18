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

/// Total distinct documents in the witchcraft `document` table.
/// Issued via the DB's own connection (no parallel rusqlite open),
/// so the count reads the same byte-state writes are landing in.
fn count_documents(db: &DB) -> Result<usize> {
    let mut stmt = db
        .query("SELECT COUNT(*) FROM document")
        .map_err(|e| anyhow::anyhow!("witchcraft count_documents prepare: {e}"))?;
    // rusqlite 0.39 dropped `FromSql for u64`; read via `i64` then cast.
    // `COUNT(*)` is non-negative so the cast is total.
    let count: i64 = stmt
        .query_row([], |row| row.get(0))
        .map_err(|e| anyhow::anyhow!("witchcraft count_documents query: {e}"))?;
    Ok(count.max(0) as usize)
}

/// Probe whether a given uuid is already in the `document` table.
/// One indexed point-lookup; cheap relative to the embedding work
/// `add_doc` triggers, so paying it on every upsert keeps `len()`
/// exact across replace-by-id without changing the trait contract.
///
/// Uses `COUNT(*)` rather than `SELECT 1 ... LIMIT 1` so the result
/// is always exactly one row (`0` or `1`) — no `OptionalExtension`
/// needed on the rusqlite side, and the crate stays off our direct
/// dep list (witchcraft pulls it transitively).
fn document_contains(db: &DB, uuid: &Uuid) -> Result<bool> {
    let mut stmt = db
        .query("SELECT COUNT(*) FROM document WHERE uuid = ?1")
        .map_err(|e| anyhow::anyhow!("witchcraft document_contains prepare: {e}"))?;
    let count: i64 = stmt
        .query_row([uuid.to_string()], |row| row.get(0))
        .map_err(|e| anyhow::anyhow!("witchcraft document_contains query: {e}"))?;
    Ok(count > 0)
}

struct Inner {
    db: DB,
    embedder: Embedder,
    cache: EmbeddingsCache,
    /// `index_chunks` rebuilds centroids; tracking dirty avoids a
    /// no-op rebuild when callers `finalize()` defensively after
    /// every batch.
    dirty: bool,
    /// Cached count of distinct `node_id`s present in the `document`
    /// table. Seeded at `open()` from `SELECT COUNT(*)` and kept in
    /// sync by probing `document_contains` before each
    /// upsert/remove so it stays exact across replace-by-id and
    /// remove-of-absent. Avoids a `SELECT COUNT(*)` per `len()` call
    /// (which would otherwise dominate the call's cost).
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
        // Seed the count cache from the existing table. A daemon
        // restart opens against the same SQLite file; without this
        // load, `len()` reports zero until the first upsert lands —
        // which silently breaks any consumer that gates on it
        // (e.g. "skip reindex if non-empty").
        let count = count_documents(&db)?;
        let cache = EmbeddingsCache::new(QUERY_CACHE_CAP);
        Ok(Self {
            inner: Mutex::new(Inner {
                db,
                embedder,
                cache,
                dirty: false,
                count,
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
        // `add_doc` is INSERT OR REPLACE on the uuid PK, so repeated
        // upserts of the same node_id correctly overwrite the row. To
        // keep `len()` exact across replace-by-id we probe first and
        // only increment when the row is genuinely new. One indexed
        // point-lookup per upsert; cheap relative to the embedding
        // work `add_doc` already triggers.
        let was_present = document_contains(&inner.db, &uuid)?;
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
        // Probe so we decrement only when there was an actual row to
        // remove. Pre-fix `saturating_sub(1)` silently hid bugs where
        // a caller emit'd a stray remove against a never-upserted id —
        // the floor masked the underflow but `len()` would diverge
        // from reality on the next `upsert`-then-remove cycle.
        let was_present = document_contains(&inner.db, &uuid)?;
        inner
            .db
            .remove_doc(&uuid)
            .map_err(|e| anyhow::anyhow!("witchcraft remove_doc({node_id}): {e}"))?;
        if was_present {
            inner.count -= 1;
        }
        // Don't bump `dirty`: index_chunks is over chunks, and an
        // orphan chunk from a deleted doc stays in the index until a
        // full reindex anyway. Witchcraft's `search` filters by current
        // doc rows, so orphans don't surface as hits.
        Ok(())
    }

    fn finalize(&self) -> Result<()> {
        // Known constraint, not a TODO: this lock is held across
        // `embed_chunks` (candle T5 forward pass) + `index_chunks`
        // (centroid rebuild). Any concurrent `search` on this engine
        // blocks behind it. Today's idiom is *batch* finalize after a
        // reindex pass, where blocking is acceptable. If a future
        // workload needs interleaved write+search at steady state,
        // the fix is a reader-writer split — move `db` + `cache`
        // behind `RwLock` and move write-side state behind a separate
        // `Mutex`. Punting until measured is the right call (premature
        // R/W split adds non-trivial dispatch complexity for no
        // observed contention).
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

    // ── Count-contract tests ─────────────────────────────────────────
    //
    // These exercise `count_documents` / `document_contains` against a
    // bare `witchcraft::DB` so they run in every CI invocation — no T5
    // assets needed. The full `WitchcraftEngine` lifecycle requires an
    // `Embedder` (assets-gated, only runs when `WITCHCRAFT_ASSETS_DIR`
    // is staged); these tests cover the count bookkeeping that drives
    // the `TextSearchEngine::len()` contract without that dependency.

    /// `count_documents` against a freshly-created DB returns 0. Pins
    /// the "empty DB doesn't mis-seed" invariant — pre-fix the engine
    /// would also report 0 here, but for the wrong reason (no DB read).
    #[test]
    fn count_documents_on_empty_db_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let db = witchcraft::DB::new(tmp.path().join("empty.db")).expect("DB::new");
        let count = count_documents(&db).expect("count_documents");
        assert_eq!(count, 0);
    }

    /// `count_documents` after add_doc returns the actual row count.
    /// Pins the `open()`-seed contract: opening an engine against a
    /// populated DB must reflect the existing row count, not 0.
    #[test]
    fn count_documents_reflects_actual_rows() {
        let tmp = TempDir::new().unwrap();
        let mut db = witchcraft::DB::new(tmp.path().join("seeded.db")).expect("DB::new");
        let uuid_a = WitchcraftEngine::node_uuid("a");
        let uuid_b = WitchcraftEngine::node_uuid("b");
        db.add_doc(&uuid_a, None, "{}", "body a", None)
            .expect("add_doc a");
        db.add_doc(&uuid_b, None, "{}", "body b", None)
            .expect("add_doc b");
        assert_eq!(count_documents(&db).expect("count"), 2);

        // INSERT-OR-REPLACE on the same uuid must not inflate the count.
        // This is the load-bearing invariant for `upsert` — without the
        // `document_contains` probe, the engine's mirrored count would
        // diverge from this answer.
        db.add_doc(&uuid_a, None, "{}", "body a v2", None)
            .expect("add_doc replace");
        assert_eq!(
            count_documents(&db).expect("count after replace"),
            2,
            "INSERT OR REPLACE must keep distinct uuid count steady",
        );
    }

    /// `document_contains` matches the actual presence of a uuid.
    /// Pins the probe used by `upsert`/`remove` so a future refactor
    /// of the SQL doesn't silently start returning false-negatives
    /// (which would re-introduce the inflated-count bug).
    #[test]
    fn document_contains_matches_actual_presence() {
        let tmp = TempDir::new().unwrap();
        let mut db = witchcraft::DB::new(tmp.path().join("probe.db")).expect("DB::new");
        let uuid = WitchcraftEngine::node_uuid("known");
        let absent = WitchcraftEngine::node_uuid("absent");

        assert!(
            !document_contains(&db, &uuid).expect("probe before insert"),
            "fresh DB must report no presence for any uuid",
        );

        db.add_doc(&uuid, None, "{}", "body", None)
            .expect("add_doc");
        assert!(
            document_contains(&db, &uuid).expect("probe after insert"),
            "inserted uuid must surface as present",
        );
        assert!(
            !document_contains(&db, &absent).expect("probe absent"),
            "an unrelated uuid must NOT surface as present",
        );

        db.remove_doc(&uuid).expect("remove_doc");
        assert!(
            !document_contains(&db, &uuid).expect("probe after remove"),
            "removed uuid must report not-present",
        );
    }
}
