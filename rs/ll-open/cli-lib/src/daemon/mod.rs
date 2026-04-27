//! Open daemon: UDS socket + event router + extensible ops.

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
    /// Cached arena SQLite connection — invalidated when generation changes.
    /// (generation, Connection). Mutex because ops are called from async tasks.
    pub arena_conn: Mutex<Option<(u64, rusqlite::Connection)>>,
}
