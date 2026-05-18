//! Unstructured-text semantic search backend for ley-line-open.
//!
//! This crate is the abstraction layer between the daemon and whatever
//! retrieval engine is configured. It exists so that ley-line's existing
//! single-vector KNN op (`vec_search`, backed by `sqlite-vec`) can stay
//! untouched while a richer text-shaped retrieval surface — XTR-WARP via
//! the `witchcraft` crate — is added alongside.
//!
//! ## Substrate contract
//!
//! Every engine is **sidecar by construction**: it owns its own SQLite (or
//! other) storage and MUST NOT touch the Σ Merkle-CAS substrate. That means:
//!
//! 1. The engine's storage path lives outside the arena directory.
//! 2. The engine never writes a `*.bindings.capnp` segment.
//! 3. Re-indexing a corpus never advances `current_root`.
//!
//! `tests/substrate_non_leak.rs` asserts (1) and (3) directly; (2) is
//! structurally guaranteed by the engine not having a capnp dependency.
//!
//! ## Engines shipped
//!
//! - [`null::NullEngine`] — default. Every op returns [`Error::NotImplemented`].
//!   Used so the daemon op surface compiles and clients see a structured
//!   error instead of an "unknown op" 404 when no real engine is wired in.
//! - [`witchcraft::WitchcraftEngine`] — feature-gated under `engine-witchcraft`.
//!   XTR-WARP late-interaction retrieval via the upstream `witchcraft` crate.
//!   Constructor needs a T5 assets directory; see the module docstring for
//!   the deployment-time knob.

use std::path::Path;

pub mod null;

#[cfg(feature = "engine-witchcraft")]
pub mod witchcraft;

/// One search hit. `node_id` is the caller-supplied identifier passed to
/// [`TextSearchEngine::upsert`].
///
/// **`score` is engine-defined and NOT normalized across engines.** The
/// only contract is "higher is better within the same engine instance."
/// Witchcraft returns late-interaction similarity (mixed with BM25 via
/// RRF in hybrid mode); a future FTS5 backend would return BM25 raw
/// scores; a future single-vector backend would return cosine in
/// `[-1, 1]`. Consumers MUST treat **rank order** as the load-bearing
/// signal — score-threshold filters tuned against today's Witchcraft
/// hybrid will produce wildly different recall under a swapped backend.
/// If a backend swap is in your future, gate on rank (e.g. top-k cutoff),
/// not the raw score value.
///
/// The daemon wire surface (`op_text_search` response) exposes the raw
/// score so consumers can sort or weight; the same caveat applies across
/// the wire.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub node_id: String,
    pub score: f32,
}

/// Errors returned by engines. The [`Error::NotImplemented`] variant is the
/// distinguished case — it's how the [`null::NullEngine`] signals "this op
/// has no real backend yet" without conflating that with a real engine
/// failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("text-search engine has no backend wired in: {0}")]
    NotImplemented(&'static str),
    #[error("text-search engine error: {0}")]
    Engine(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Abstract text-search backend.
///
/// All methods use interior mutability — the engine owns its own locking so
/// the daemon can hand `Arc<dyn TextSearchEngine>` to many readers without
/// a caller-side `Mutex`. This matches the pattern `VectorIndex` already
/// uses (one `Mutex<Connection>` inside).
///
/// ## Lifecycle
///
/// Callers `upsert` documents in any order, then call `finalize` to build
/// internal index structures, then `search`. Engines that index incrementally
/// (e.g. a `vec0`-style backend) implement `finalize` as a no-op; engines
/// that require an explicit build step (Witchcraft / XTR-WARP centroids) do
/// real work in `finalize`. `search` is allowed before `finalize` but may
/// return empty results.
pub trait TextSearchEngine: Send + Sync {
    /// Insert or replace the text content associated with `node_id`. The
    /// engine may defer embedding work until [`finalize`].
    fn upsert(&self, node_id: &str, content: &str) -> Result<()>;

    /// Remove `node_id` from the index. Idempotent — no error if absent.
    fn remove(&self, node_id: &str) -> Result<()>;

    /// Build or rebuild internal index structures over everything upserted
    /// since the last `finalize`. Idempotent; safe to call repeatedly.
    fn finalize(&self) -> Result<()>;

    /// Top-`k` hits for `query`. Returns an empty `Vec` (not an error) when
    /// the index is empty or `query` is empty; this is the contract that
    /// lets the daemon op return `{ok: true, results: []}` cleanly on a
    /// freshly-created database.
    fn search(&self, query: &str, k: usize) -> Result<Vec<Hit>>;

    /// Number of distinct `node_id`s currently in the index. Must be
    /// exact (no overcount on replace, no decrement on remove-of-absent)
    /// — callers gate on `len() == 0` to decide whether to skip a
    /// reindex pass, and on `len() > N` for batch-size policy, so a
    /// best-effort counter can break either decision silently.
    fn len(&self) -> Result<usize>;

    /// `true` iff `len() == 0`.
    fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Drop every upserted document and reset internal state.
    fn clear(&self) -> Result<()>;

    /// Path on disk where the engine's storage lives, if any.
    ///
    /// **Contract**: if the engine writes any state to disk, this MUST
    /// return `Some(path)` pointing at that location. The
    /// substrate-non-leak gate in `tests/substrate_non_leak.rs` asserts
    /// that path lies outside the arena directory; an engine that
    /// returns `None` despite having on-disk storage silently bypasses
    /// the gate, and a future audit can no longer prove the substrate
    /// invariant for that engine. Only engines that are fully in-memory
    /// (or that delegate persistence to a caller-owned handle) may
    /// return `None`.
    fn storage_path(&self) -> Option<&Path>;
}
