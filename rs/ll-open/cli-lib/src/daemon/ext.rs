//! Extension trait for ley-line (private) to register additional daemon ops.

use std::future::Future;
use std::pin::Pin;

/// Extension point for private daemon ops.
///
/// The base daemon handles: status, flush, load, query, reparse, subscribe,
/// unsubscribe, emit. Any unrecognized op is passed to the extension.
///
/// Return `Some(json_string)` if handled, `None` to fall through to "unknown op".
pub trait DaemonExt: Send + Sync {
    fn handle_op(&self, op: &str, req: &serde_json::Value) -> Option<String> {
        let _ = (op, req);
        None
    }

    fn handle_op_async<'a>(
        &'a self,
        op: &'a str,
        req: &'a serde_json::Value,
    ) -> Option<Pin<Box<dyn Future<Output = String> + Send + 'a>>> {
        let _ = (op, req);
        None
    }
}

/// No-op extension that rejects all ops. Used when no private extension is registered.
pub struct NoExt;
impl DaemonExt for NoExt {}
