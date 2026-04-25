//! Extension trait for ley-line (private) to register additional daemon ops
//! and lifecycle hooks.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

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
}

/// No-op extension that rejects all ops. Used when no private extension is registered.
pub struct NoExt;
impl DaemonExt for NoExt {}
