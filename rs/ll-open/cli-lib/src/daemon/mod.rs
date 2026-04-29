//! Open daemon: UDS socket + event router + extensible ops.

#[cfg(feature = "vec")]
pub mod embed;
pub mod enrichment;
pub mod ext;
pub mod events;
#[cfg(feature = "lsp")]
pub mod lsp_pass;
pub mod mcp;
pub mod ops;
pub mod socket;
#[cfg(feature = "vec")]
pub mod vec_index;

use std::collections::HashMap;
#[cfg(feature = "vec")]
use std::collections::BinaryHeap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

pub use ext::{DaemonExt, NoExt};
pub use events::EventRouter;

/// Wall-clock millis since UNIX_EPOCH. Used by the daemon's `last_*_ms`
/// state fields, embed-priority records, and pass-outcome timestamps —
/// anywhere we need a comparable timestamp that can survive a JSON
/// round-trip.
///
/// Returns 0 if the system clock is before 1970 (effectively impossible
/// in practice; the fallback exists so callers don't have to plumb a
/// Result through cold paths). Callers that need *monotonic* ordering
/// should use a separate counter — wall-clock is wrong for ordering
/// across NTP steps. See `daemon::embed::next_priority` for the
/// monotonic-counter pattern.
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Lifecycle phase of the daemon.
#[derive(Debug, Clone, PartialEq)]
pub enum DaemonPhase {
    /// Bringing up the living db (warm-start deserialize or cold-start open).
    Initializing,
    /// Running the tree-sitter parse pass.
    Parsing,
    /// Running enrichment passes (LSP, embeddings, etc.).
    Enriching,
    /// Idle and ready to serve queries.
    Ready,
    /// Stuck — last lifecycle step failed. Message describes the failure.
    Error(String),
}

impl DaemonPhase {
    /// Short stable string for JSON serialization.
    pub fn as_str(&self) -> &str {
        match self {
            DaemonPhase::Initializing => "initializing",
            DaemonPhase::Parsing => "parsing",
            DaemonPhase::Enriching => "enriching",
            DaemonPhase::Ready => "ready",
            DaemonPhase::Error(_) => "error",
        }
    }
}

/// Status of one enrichment pass: when it last ran, the basis (parse_version it
/// ran against), and any recent error.
#[derive(Debug, Clone, Default)]
pub struct PassStatus {
    /// Wall-clock millis-since-epoch when the pass last completed.
    pub last_run_at_ms: Option<i64>,
    /// `parse_version` the pass last ran against (causal basis).
    pub basis: Option<u64>,
    /// Last error message, cleared on next successful run.
    pub error: Option<String>,
}

/// Snapshot of daemon lifecycle state, returned by `op_status`.
#[derive(Debug, Clone)]
pub struct DaemonState {
    pub phase: DaemonPhase,
    pub head_sha: Option<String>,
    pub last_reparse_at_ms: Option<i64>,
    pub enrichment: HashMap<String, PassStatus>,
}

impl DaemonState {
    pub fn initializing() -> Self {
        Self {
            phase: DaemonPhase::Initializing,
            head_sha: None,
            last_reparse_at_ms: None,
            enrichment: HashMap::new(),
        }
    }
}

/// Shared state passed to all op handlers.
pub struct DaemonContext {
    pub ctrl_path: PathBuf,
    pub ext: Arc<dyn DaemonExt>,
    pub router: Arc<EventRouter>,
    /// The living database — owned for the daemon's lifetime.
    /// All queries go through this. `:memory:` SQLite, crash-recovered from arena.
    pub live_db: Mutex<rusqlite::Connection>,
    /// Source directory being tracked (if --source was given).
    pub source_dir: Option<PathBuf>,
    /// Language filter for parsing.
    pub lang_filter: Option<String>,
    /// Registered enrichment passes (tree-sitter + extension passes).
    pub enrichment_passes: Vec<Box<dyn enrichment::EnrichmentPass>>,
    /// Lifecycle state (phase, head_sha, last_reparse_at, per-pass status).
    /// Shared via `Arc` so background tasks and the run_daemon scope can both
    /// mutate it.
    pub state: Arc<RwLock<DaemonState>>,
    /// Sidecar vector index used by `op_vec_search`. Populated when the
    /// `vec` feature is enabled.
    #[cfg(feature = "vec")]
    pub vec_index: Arc<vec_index::VectorIndex>,
    /// Embedder used to vectorize text (queries + node content). Defaults to
    /// `ZeroEmbedder`; private extensions override via `DaemonExt::embedder`.
    #[cfg(feature = "vec")]
    pub embedder: Arc<dyn embed::Embedder>,
    /// Priority queue of node ids to re-embed. Query ops push when a node is
    /// touched; the background drainer pops batches and refreshes the
    /// VectorIndex.
    #[cfg(feature = "vec")]
    pub embed_queue: Arc<Mutex<BinaryHeap<embed::EmbedTask>>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_ms_is_positive_after_unix_epoch() {
        // We are well past 1970 — now_ms() should always be a large
        // positive number. The unwrap_or(0) fallback only fires if
        // SystemTime::now() is *before* UNIX_EPOCH (impossible
        // outside a deliberately tampered system clock).
        let t = now_ms();
        assert!(t > 1_700_000_000_000, "now_ms should be > 2023, got {t}");
    }

    #[test]
    fn now_ms_is_monotonic_within_a_call_burst() {
        // Wall-clock isnt strictly monotonic across NTP steps, but two
        // calls in immediate succession on the same thread should not
        // observe a backwards step. If this test ever flakes it is
        // either an NTP step in the middle of CI (rare) or a serious
        // platform issue.
        let a = now_ms();
        let b = now_ms();
        let c = now_ms();
        assert!(a <= b && b <= c, "now_ms went backwards: {a} {b} {c}");
    }
}

