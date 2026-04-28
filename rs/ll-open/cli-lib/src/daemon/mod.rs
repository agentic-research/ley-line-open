//! Open daemon: UDS socket + event router + extensible ops.

pub mod enrichment;
pub mod ext;
pub mod events;
#[cfg(feature = "lsp")]
pub mod lsp_pass;
pub mod ops;
pub mod socket;
#[cfg(feature = "vec")]
pub mod vec_index;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

pub use ext::{DaemonExt, NoExt};
pub use events::EventRouter;

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
}
