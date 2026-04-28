//! Extension trait for ley-line (private) to register additional daemon ops
//! and lifecycle hooks.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use super::enrichment::EnrichmentPass;
use super::events::{EventEmitter, EventRouter};

/// Extension point for private daemon ops and lifecycle hooks.
///
/// The base daemon handles: status, flush, load, query, reparse, subscribe,
/// unsubscribe, emit. Any unrecognized op is passed to the extension.
///
/// Lifecycle hooks let the extension spawn background tasks (receiver,
/// inference, TCP control, etc.) without the base daemon knowing about them.
pub trait DaemonExt: Send + Sync {
    /// Handle a synchronous UDS op.
    /// Return `Some(json_string)` if handled, `None` to fall through.
    fn handle_op(&self, op: &str, req: &serde_json::Value) -> Option<String> {
        let _ = (op, req);
        None
    }

    /// Handle an async UDS op (e.g., LSP tool invocation).
    fn handle_op_async<'a>(
        &'a self,
        op: &'a str,
        req: &'a serde_json::Value,
    ) -> Option<Pin<Box<dyn Future<Output = String> + Send + 'a>>> {
        let _ = (op, req);
        None
    }

    /// Called after event router is created but before UDS socket + mount.
    ///
    /// Use to initialize extension state (sheaf cache, inference engine, etc.)
    /// that needs an event emitter. The returned emitter can be used to emit
    /// events from background tasks.
    fn on_init(&self, _emitter: EventEmitter) {}

    /// Called after mount is complete. Spawn background tasks here.
    ///
    /// Receives the control path (for arena access) and event router (for
    /// pub/sub). Background tasks should be spawned via `tokio::spawn` —
    /// they run until the daemon shuts down.
    fn on_post_mount(&self, _ctrl_path: &Path, _router: &Arc<EventRouter>) {}

    /// Register additional enrichment passes.
    ///
    /// The base daemon provides `TreeSitterPass`. Extensions add LSP,
    /// embeddings, sheaf, etc. Each pass must own a disjoint set of tables.
    fn enrichment_passes(&self) -> Vec<Box<dyn EnrichmentPass>> {
        vec![]
    }

    /// Called when the source repo's HEAD commit changes (e.g. branch switch,
    /// new commit). Use to invalidate VCS-keyed caches in the extension.
    ///
    /// `old_sha` may be empty on first transition. The same information is
    /// also emitted as a `daemon.head.changed` event — extensions can use
    /// either pattern.
    fn on_head_changed(&self, _old_sha: &str, _new_sha: &str) {}

    /// Called when the source repo's dirty file set changes (file edits,
    /// adds, removes). `paths` is the new dirty set in full — not a delta.
    ///
    /// Mirrors the `daemon.files.changed` event payload.
    fn on_files_changed(&self, _paths: &[String]) {}
}

/// No-op extension that rejects all ops. Used when no private extension is registered.
pub struct NoExt;
impl DaemonExt for NoExt {}
