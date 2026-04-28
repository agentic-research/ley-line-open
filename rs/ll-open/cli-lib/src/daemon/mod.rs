//! Open daemon: UDS socket + event router + extensible ops.

pub mod enrichment;
pub mod ext;
pub mod events;
pub mod ops;
pub mod socket;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub use ext::{DaemonExt, NoExt};
pub use events::EventRouter;

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
}
