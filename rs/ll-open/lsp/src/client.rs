//! Minimal LSP client — spawns a language server and speaks JSON-RPC over stdio.

use anyhow::{Context, Result, bail};
use lsp_types::PublishDiagnosticsParams;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

/// Buffer for the channel that carries server→client responses + notifs.
/// 64 is enough for a typical request/response burst (initialize +
/// didOpen + symbols + diagnostics) without blocking the reader task.
const RESPONSE_CHANNEL_BUFFER: usize = 64;

/// How long `drain_notifications` waits before checking the response
/// channel — gives the server a moment to publish diagnostics after a
/// didOpen/didChange before we move on.
const DIAGNOSTIC_DRAIN_DELAY: std::time::Duration = std::time::Duration::from_millis(200);

/// Maximum time `shutdown()` waits for a graceful exit before killing.
/// Some servers (notably terraform-ls) hang on `exit` — keep this
/// short so a daemon shutdown doesn't stall on a misbehaving language.
const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

use crate::protocol::{
    CompletionItem, CompletionResponse, Diagnostic, DocumentSymbol, GotoDefinitionResponse, Hover,
    Location, Notification, Request, Response,
};

pub struct LspClient {
    child: Child,
    stdin: tokio::process::ChildStdin,
    rx: mpsc::Receiver<Response>,
    next_id: u64,
    /// Diagnostics received via notifications (server pushes these).
    pub diagnostics: Vec<(String, Vec<Diagnostic>)>,
    /// Readiness derived from the protocol's own structure — `$/progress`
    /// token begin/end pairing, and rust-analyzer's
    /// `experimental/serverStatus` where offered. `await_ready` polls
    /// this. Never flips for servers that emit no progress at all —
    /// those callers should pass `wait: false` instead of calling
    /// `await_ready` at all. See [`ReadinessTracker`] for the contract
    /// (bead `ley-line-open-fb7d73`).
    readiness: ReadinessTracker,
}

impl LspClient {
    /// Spawn a language server and perform the LSP handshake. Sends no
    /// per-server `initializationOptions`. Equivalent to
    /// `start_with_options(command, args, root_uri, None)`.
    ///
    /// Callers that need server-specific init options (e.g. gopls's
    /// build configuration, pyright's analysis settings) should use
    /// `start_with_options` directly.
    pub async fn start(command: &str, args: &[&str], root_uri: &str) -> Result<Self> {
        Self::start_with_options(command, args, root_uri, None).await
    }

    /// Spawn a language server and perform the LSP handshake with
    /// optional per-server `initializationOptions`.
    ///
    /// Bead `ley-line-open-661727` / mache-6584a0 (gopls cold-start):
    /// some servers need both `workspaceFolders` AND
    /// `initializationOptions` to load the workspace properly.
    /// rust-analyzer infers from `rootUri`; gopls strongly prefers
    /// `workspaceFolders` (without it, gopls loads files but doesn't
    /// analyze the module — hover returns empty even after the
    /// server's progress signals fire). Sending both is harmless for
    /// servers that only care about one.
    pub async fn start_with_options(
        command: &str,
        args: &[&str],
        root_uri: &str,
        initialization_options: Option<serde_json::Value>,
    ) -> Result<Self> {
        let mut child = Command::new(command)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn {command}"))?;

        let stdin = child
            .stdin
            .take()
            .expect("stdin piped in Command builder above");
        let stdout = child
            .stdout
            .take()
            .expect("stdout piped in Command builder above");

        // Reader task: parse LSP messages from stdout, forward to channel
        let (tx, rx) = mpsc::channel::<Response>(RESPONSE_CHANNEL_BUFFER);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_message(&mut reader).await {
                    Ok(Some(msg)) => {
                        if tx.send(msg).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => break, // EOF
                    Err(e) => {
                        log::debug!("LSP read error: {e}");
                        break;
                    }
                }
            }
        });

        let mut client = Self {
            child,
            stdin,
            rx,
            next_id: 1,
            diagnostics: Vec::new(),
            readiness: ReadinessTracker::default(),
        };

        // Initialize handshake.
        //
        // `window.workDoneProgress` — opt-in for `$/progress`
        // notifications. Without this rust-analyzer (and most servers)
        // won't emit the begin/report/end lifecycle that signals when
        // the workspace is indexed; we'd block forever on indexing
        // queries. Bead `ley-line-open-661727` chased this through
        // when v0.5.3 surfaced `[lsp] documentSymbol returned 25 / 0
        // hovers / 0 defs / 0 refs` — the queries fired before
        // rust-analyzer finished loading the cargo project model.
        //
        // `experimental.serverStatusNotification` — rust-analyzer-
        // specific notification that signals `quiescent: true` when
        // the server is done indexing + analysis. Richer than
        // `$/progress` (quiescence is one bit covering ALL of the
        // server's work, with no per-token bookkeeping), so
        // `ReadinessTracker` treats it as authoritative when offered.
        // Derive a workspace folder from rootUri. gopls (and many other
        // modern LSP servers) prefer `workspaceFolders` for module /
        // package detection — `rootUri` is the deprecated single-folder
        // signal and gopls treats it as a fallback. The folder's name is
        // the basename of the root path; servers display it in
        // workspace-aware UI but otherwise ignore it.
        let workspace_name = root_uri
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or("workspace")
            .to_string();

        let mut init_params = serde_json::json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "workspaceFolders": [{
                "uri": root_uri,
                "name": workspace_name,
            }],
            "capabilities": {
                "window": {
                    "workDoneProgress": true
                },
                "experimental": {
                    "serverStatusNotification": true
                },
                "workspace": {
                    "workspaceFolders": true,
                    "configuration": true
                },
                "textDocument": {
                    "synchronization": { "didSave": true },
                    "documentSymbol": {
                        "hierarchicalDocumentSymbolSupport": true
                    },
                    "publishDiagnostics": {},
                    "hover": { "contentFormat": ["plaintext", "markdown"] },
                    "references": {},
                    "definition": {},
                    "completion": {
                        "completionItem": {
                            "documentationFormat": ["plaintext", "markdown"]
                        }
                    }
                }
            }
        });

        // Stitch in per-server initialization options if provided.
        // gopls cares about `build.expandWorkspaceToModule`,
        // `directoryFilters`; pyright cares about `python.analysis.*`;
        // rust-analyzer cares about `cargo.*` + `procMacro.enable`.
        // The map is owned by `LspEnrichmentPass::run` in the daemon's
        // lsp_pass.rs so each server's tuning lives next to its
        // language-server invocation.
        if let Some(opts) = initialization_options {
            init_params["initializationOptions"] = opts;
        }

        let _init_result = client.request("initialize", init_params).await?;
        client.notify("initialized", serde_json::json!({})).await?;

        Ok(client)
    }

    /// Send a request and wait for the response.
    pub async fn request(
        &mut self,
        method: &'static str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = self.next_id;
        self.next_id += 1;

        let req = Request::new(id, method, params);
        self.send(&serde_json::to_string(&req)?).await?;

        // Read messages until we get our response, dispatching everything
        // else along the way: notifications (handle), and — crucially —
        // server→CLIENT requests, which we MUST answer or the server
        // blocks. gopls sends `workspace/configuration` and
        // `window/workDoneProgress/create` requests early; without our
        // reply it never resolves config or emits progress, so hover/def/
        // ref stay empty and the per-file budget is wasted (mache-6584a0).
        // rust-analyzer doesn't depend on these, which is why only gopls hung.
        loop {
            let msg = self
                .rx
                .recv()
                .await
                .context("LSP server closed connection")?;

            match (msg.id, msg.method.as_deref()) {
                // Server→client REQUEST (has both id and method): answer it.
                (Some(req_id), Some(method)) => {
                    let method = method.to_string();
                    self.answer_server_request(req_id, &method, msg.params.as_ref())
                        .await?;
                }
                // Notification (method, no id): handle (diagnostics, progress, status).
                (None, Some(_)) => self.handle_notification(&msg),
                // Response to one of OUR requests.
                (Some(resp_id), None) if resp_id == id => {
                    if let Some(err) = msg.error {
                        bail!("LSP error {}: {}", err.code, err.message);
                    }
                    return Ok(msg.result.unwrap_or(serde_json::Value::Null));
                }
                // Stale response for a different id, or malformed — skip.
                _ => {}
            }
        }
    }

    /// Answer a server→client request with a minimal "use defaults" / "ack"
    /// response. Without this the server (gopls especially) blocks waiting
    /// for the reply and never finishes loading the workspace (mache-6584a0).
    async fn answer_server_request(
        &mut self,
        id: u64,
        method: &str,
        params: Option<&serde_json::Value>,
    ) -> Result<()> {
        let result = server_request_result(method, params);
        let resp = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
        self.send(&serde_json::to_string(&resp)?).await
    }

    /// Send a notification (no response expected).
    pub async fn notify(&mut self, method: &'static str, params: serde_json::Value) -> Result<()> {
        let notif = Notification::new(method, params);
        self.send(&serde_json::to_string(&notif)?).await
    }

    /// Open a file for analysis.
    pub async fn open_file(&mut self, uri: &str, language_id: &str, text: &str) -> Result<()> {
        self.notify(
            "textDocument/didOpen",
            serde_json::json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": text
                }
            }),
        )
        .await
    }

    /// Request document symbols (hierarchical).
    pub async fn document_symbols(&mut self, uri: &str) -> Result<Vec<DocumentSymbol>> {
        let result = self
            .request(
                "textDocument/documentSymbol",
                serde_json::json!({
                    "textDocument": { "uri": uri }
                }),
            )
            .await?;

        let symbols: Vec<DocumentSymbol> = serde_json::from_value(result).unwrap_or_default();
        Ok(symbols)
    }

    /// Go-to-definition: resolve the definition location(s) for a position.
    pub async fn definition(
        &mut self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Vec<Location>> {
        let result = self
            .request(
                "textDocument/definition",
                serde_json::json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": line, "character": character }
                }),
            )
            .await?;

        if result.is_null() {
            return Ok(vec![]);
        }
        match serde_json::from_value::<GotoDefinitionResponse>(result) {
            Ok(GotoDefinitionResponse::Scalar(loc)) => Ok(vec![loc]),
            Ok(GotoDefinitionResponse::Array(locs)) => Ok(locs),
            Ok(GotoDefinitionResponse::Link(links)) => Ok(links
                .into_iter()
                .map(|l| Location {
                    uri: l.target_uri,
                    range: l.target_selection_range,
                })
                .collect()),
            Err(e) => {
                // Don't fail the whole pass on a single malformed
                // server response — but make it visible so operators
                // can see "this LSP server is sending us garbage".
                log::warn!("LSP definition response parse failed: {e}");
                Ok(vec![])
            }
        }
    }

    /// Find all references to the symbol at a position.
    pub async fn references(
        &mut self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Vec<Location>> {
        let result = self
            .request(
                "textDocument/references",
                serde_json::json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": line, "character": character },
                    "context": { "includeDeclaration": true }
                }),
            )
            .await?;

        if result.is_null() {
            return Ok(vec![]);
        }
        let locations: Vec<Location> = serde_json::from_value(result).unwrap_or_default();
        Ok(locations)
    }

    /// Hover: get type info / documentation for a position.
    pub async fn hover(&mut self, uri: &str, line: u32, character: u32) -> Result<Option<Hover>> {
        let result = self
            .request(
                "textDocument/hover",
                serde_json::json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": line, "character": character }
                }),
            )
            .await?;

        if result.is_null() {
            return Ok(None);
        }
        Ok(serde_json::from_value(result).ok())
    }

    /// Completion: get completion items at a position.
    pub async fn completion(
        &mut self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Vec<CompletionItem>> {
        let result = self
            .request(
                "textDocument/completion",
                serde_json::json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": line, "character": character }
                }),
            )
            .await?;

        if result.is_null() {
            return Ok(vec![]);
        }
        match serde_json::from_value::<CompletionResponse>(result) {
            Ok(CompletionResponse::Array(items)) => Ok(items),
            Ok(CompletionResponse::List(list)) => Ok(list.items),
            Err(e) => {
                log::warn!("LSP completion response parse failed: {e}");
                Ok(vec![])
            }
        }
    }

    /// Drain any pending diagnostic notifications.
    pub async fn drain_notifications(&mut self) {
        // Give the server a moment to send notifications
        tokio::time::sleep(DIAGNOSTIC_DRAIN_DELAY).await;
        while let Ok(msg) = self.rx.try_recv() {
            match (msg.id, msg.method.as_deref()) {
                (Some(req_id), Some(method)) => {
                    let method = method.to_string();
                    let _ = self
                        .answer_server_request(req_id, &method, msg.params.as_ref())
                        .await;
                }
                (None, Some(_)) => self.handle_notification(&msg),
                _ => {}
            }
        }
    }

    /// Shut down the server gracefully (with timeout for misbehaving servers).
    pub async fn shutdown(mut self) -> Result<()> {
        let graceful = async {
            let _ = self.request("shutdown", serde_json::Value::Null).await;
            let _ = self.notify("exit", serde_json::Value::Null).await;
            let _ = self.child.wait().await;
        };
        // Some servers (terraform-ls) hang on shutdown — don't block forever
        if tokio::time::timeout(SHUTDOWN_TIMEOUT, graceful)
            .await
            .is_err()
        {
            log::debug!("LSP shutdown timed out, killing process");
            let _ = self.child.kill().await;
        }
        Ok(())
    }

    fn handle_notification(&mut self, msg: &Response) {
        let Some(method) = &msg.method else {
            return;
        };

        match method.as_str() {
            "textDocument/publishDiagnostics" => {
                if let Some(params) = &msg.params
                    && let Ok(diag) =
                        serde_json::from_value::<PublishDiagnosticsParams>(params.clone())
                {
                    self.diagnostics
                        .push((diag.uri.to_string(), diag.diagnostics));
                }
            }
            "$/progress" => {
                // Generic LSP work-done progress, tracked per TOKEN — the
                // protocol's own structure — never by matching the
                // human-readable `title`, which is some upstream server's
                // UI prose and renames without notice. See
                // [`ReadinessTracker`]; beads `ley-line-open-661727`,
                // `ley-line-open-fb7d73`.
                if let Some(params) = &msg.params {
                    self.readiness.on_progress(params);
                }
            }
            "experimental/serverStatus" => {
                // rust-analyzer-specific (`experimental.serverStatusNotification`
                // capability declared in `start`). When `quiescent: true`
                // the server has finished its current analysis sweep —
                // hover/definition/references are now backed by the
                // resolved project model.
                if let Some(params) = &msg.params {
                    self.readiness.on_server_status(params);
                }
            }
            _ => {}
        }
    }

    /// Wait for the language server to signal readiness for semantic
    /// queries (hover / definition / references). Polls the
    /// [`ReadinessTracker`] (fed by `$/progress` token lifecycles and
    /// rust-analyzer's `experimental/serverStatus`) up to `timeout`,
    /// with a bounded tick — poll-until-condition with a deadline, not
    /// a sleep standing in for a signal. Returns `true` if the ready
    /// state was reached, `false` on timeout. Callers should still
    /// issue queries on timeout — the server may have skipped progress
    /// notifications (older language servers, or one that doesn't
    /// index).
    ///
    /// Bead `ley-line-open-661727`: documentSymbol is syntactic and
    /// returns immediately, but hover/def/refs need the workspace's
    /// project model loaded. Issuing them before the indexing cycle
    /// completes is the root cause of "25 symbols, 0 hovers/defs/refs."
    pub async fn await_ready(&mut self, timeout: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        let poll_interval = std::time::Duration::from_millis(50);
        loop {
            // Drain pending messages (no waiting). Each tick may advance
            // the readiness state via $/progress or experimental/serverStatus
            // — but only if we ANSWER the server's requests: gopls won't emit
            // progress until its `window/workDoneProgress/create` request is
            // acked, so answering server→client requests here is what lets
            // the readiness signal arrive at all (mache-6584a0).
            while let Ok(msg) = self.rx.try_recv() {
                match (msg.id, msg.method.as_deref()) {
                    (Some(req_id), Some(method)) => {
                        let method = method.to_string();
                        let _ = self
                            .answer_server_request(req_id, &method, msg.params.as_ref())
                            .await;
                    }
                    (None, Some(_)) => self.handle_notification(&msg),
                    _ => {}
                }
            }
            if self.readiness.is_ready() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(poll_interval).await;
        }
    }

    /// Test-only: peek at the readiness state. Production callers use
    /// `await_ready` which drains notifications first.
    #[cfg(test)]
    pub fn is_server_ready(&self) -> bool {
        self.readiness.is_ready()
    }
}

/// Readiness derived from the LSP protocol's structure, not from any
/// server's prose (bead `ley-line-open-fb7d73`).
///
/// The predecessor substring-matched `$/progress` titles against
/// "indexing" / "loading" / "workspace" / "ready" — copies of
/// rust-analyzer's, gopls's and pyright's UI strings, held without a
/// contract. A patch-release rename breaks that silently: queries return
/// empty and it reads as "cold index", not "our matcher stopped
/// matching". It was also loose in the other direction: with no per-token
/// bookkeeping, the FIRST `end` of any token flipped ready while other
/// work was still in flight.
///
/// What the protocol itself guarantees is the token lifecycle: every
/// work-done progress token emits `begin`, then `report`s, then exactly
/// one `end`. So readiness is structural:
///
///   ready ⇔ at least one cycle has completed AND no begun token is
///           still in flight
///
/// — for any token, under any title, in any language. A fresh `begin`
/// (rust-analyzer reindexing on a Cargo.toml change) makes the state
/// not-ready again without needing to recognize the token's name.
///
/// rust-analyzer's `experimental/serverStatus` is richer — `quiescent`
/// is one bit covering all of the server's work — so when a server
/// offers it at all, its latest value is authoritative over the
/// progress-derived state. That notification is a negotiated capability
/// (declared in `start`), not a UI string, so depending on it is
/// depending on a contract.
#[derive(Debug, Default)]
struct ReadinessTracker {
    /// Tokens with a `begin` and no `end` yet. LSP tokens are
    /// `number | string`; the canonical JSON rendering keeps `1` and
    /// `"1"` distinct, as the protocol does.
    in_flight: std::collections::HashSet<String>,
    /// At least one progress cycle has completed. An `end` for a token
    /// whose `begin` we never saw still counts: it is structural
    /// evidence of a completed cycle, and progress subscriptions can
    /// race the first `begin`.
    saw_cycle_end: bool,
    /// Latest `experimental/serverStatus` quiescent value — `None`
    /// until the server demonstrates it speaks that contract.
    quiescent: Option<bool>,
}

impl ReadinessTracker {
    /// Feed a `$/progress` notification's params.
    fn on_progress(&mut self, params: &serde_json::Value) {
        let Some(kind) = params
            .get("value")
            .and_then(|v| v.get("kind"))
            .and_then(|k| k.as_str())
        else {
            return;
        };
        let token = params
            .get("token")
            .map(|t| t.to_string())
            .unwrap_or_default();
        match kind {
            "begin" => {
                self.in_flight.insert(token);
            }
            "end" => {
                self.in_flight.remove(&token);
                self.saw_cycle_end = true;
            }
            // "report" — the token is still in flight; nothing changes.
            _ => {}
        }
    }

    /// Feed an `experimental/serverStatus` notification's params.
    fn on_server_status(&mut self, params: &serde_json::Value) {
        if let Some(q) = params.get("quiescent").and_then(|v| v.as_bool()) {
            self.quiescent = Some(q);
        }
    }

    fn is_ready(&self) -> bool {
        match self.quiescent {
            // The server speaks the serverStatus contract: its word wins,
            // in both directions.
            Some(q) => q,
            None => self.saw_cycle_end && self.in_flight.is_empty(),
        }
    }
}

/// Build the `result` payload for a server→client request we must answer.
/// The server blocks until these are answered (mache-6584a0); the replies
/// are deliberately minimal:
///   - `workspace/configuration` → one `null` per requested item, i.e.
///     "no override, use your defaults". gopls requests config for each
///     workspace folder/scope right after `initialized`; an unanswered
///     request stalls its workspace load.
///   - everything else (`window/workDoneProgress/create`,
///     `client/registerCapability`, `client/unregisterCapability`, …) →
///     `null`, a bare ack so the server proceeds. In particular gopls will
///     not emit `$/progress` until its `window/workDoneProgress/create`
///     request is acked, so without this `await_ready` waits the full
///     per-file budget and the file is skipped with zero hovers.
fn server_request_result(method: &str, params: Option<&serde_json::Value>) -> serde_json::Value {
    match method {
        "workspace/configuration" => {
            let n = params
                .and_then(|p| p.get("items"))
                .and_then(|i| i.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            serde_json::Value::Array(vec![serde_json::Value::Null; n])
        }
        _ => serde_json::Value::Null,
    }
}

impl LspClient {
    async fn send(&mut self, body: &str) -> Result<()> {
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        self.stdin.write_all(header.as_bytes()).await?;
        self.stdin.write_all(body.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }
}

/// Read a single LSP message from a buffered reader.
async fn read_message<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<Option<Response>> {
    // Read headers
    let mut content_length: Option<usize> = None;
    loop {
        let mut header_line = String::new();
        let n = reader.read_line(&mut header_line).await?;
        if n == 0 {
            return Ok(None); // EOF
        }
        let trimmed = header_line.trim();
        if trimmed.is_empty() {
            break; // End of headers
        }
        if let Some(val) = trimmed.strip_prefix("Content-Length: ") {
            content_length = Some(val.parse()?);
        }
    }

    let len = content_length.context("missing Content-Length header")?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;

    let msg: Response = serde_json::from_slice(&buf)?;
    Ok(Some(msg))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shorthand for a `$/progress` params object.
    fn progress(token: serde_json::Value, kind: &str) -> serde_json::Value {
        serde_json::json!({ "token": token, "value": { "kind": kind } })
    }

    #[test]
    fn tracker_readiness_ignores_titles_entirely() {
        // The regression this type exists to prevent: readiness used to
        // substring-match progress titles against copies of upstream UI
        // strings. A title no matcher has ever heard of must work
        // identically — the token lifecycle is the contract, not the prose.
        let mut t = ReadinessTracker::default();
        t.on_progress(&serde_json::json!({
            "token": 7,
            "value": { "kind": "begin", "title": "Reticulating splines" }
        }));
        assert!(!t.is_ready(), "a begun token means work is in flight");
        t.on_progress(&progress(serde_json::json!(7), "report"));
        assert!(!t.is_ready(), "report keeps the token in flight");
        t.on_progress(&progress(serde_json::json!(7), "end"));
        assert!(t.is_ready(), "the cycle completed; no title was consulted");
    }

    #[test]
    fn tracker_waits_for_every_in_flight_token() {
        // The predecessor flipped ready on the FIRST end of any token,
        // while other work was mid-flight — premature readiness that read
        // as "cold index" downstream. All begun tokens must end.
        let mut t = ReadinessTracker::default();
        t.on_progress(&progress(serde_json::json!("a"), "begin"));
        t.on_progress(&progress(serde_json::json!("b"), "begin"));
        t.on_progress(&progress(serde_json::json!("a"), "end"));
        assert!(!t.is_ready(), "token b is still in flight");
        t.on_progress(&progress(serde_json::json!("b"), "end"));
        assert!(t.is_ready());
    }

    #[test]
    fn tracker_goes_unready_when_a_new_cycle_begins() {
        // rust-analyzer reindexes on Cargo.toml change. The predecessor
        // could only reset if the new token's TITLE matched its string
        // list; structurally, any fresh begin means not-ready.
        let mut t = ReadinessTracker::default();
        t.on_progress(&progress(serde_json::json!(1), "begin"));
        t.on_progress(&progress(serde_json::json!(1), "end"));
        assert!(t.is_ready());
        t.on_progress(&progress(serde_json::json!(2), "begin"));
        assert!(!t.is_ready(), "a fresh cycle re-arms the wait");
        t.on_progress(&progress(serde_json::json!(2), "end"));
        assert!(t.is_ready());
    }

    #[test]
    fn tracker_counts_an_end_whose_begin_was_missed() {
        // Progress subscriptions can race the first begin; a lone end is
        // still structural evidence of a completed cycle (and is what the
        // predecessor's any-end behavior got right).
        let mut t = ReadinessTracker::default();
        t.on_progress(&progress(serde_json::json!("never-begun"), "end"));
        assert!(t.is_ready());
    }

    #[test]
    fn tracker_keeps_number_and_string_tokens_distinct() {
        // LSP tokens are number | string; 1 and "1" are different tokens
        // and an end for one must not retire the other.
        let mut t = ReadinessTracker::default();
        t.on_progress(&progress(serde_json::json!(1), "begin"));
        t.on_progress(&progress(serde_json::json!("1"), "end"));
        assert!(!t.is_ready(), "token 1 (number) is still in flight");
    }

    #[test]
    fn server_status_is_authoritative_when_offered() {
        // quiescent is one bit covering ALL server work — richer than
        // token bookkeeping, and a negotiated capability rather than a UI
        // string. Once a server demonstrates it speaks the contract, its
        // word wins in both directions.
        let mut t = ReadinessTracker::default();
        // Progress fully drained, but the server says not quiescent.
        t.on_progress(&progress(serde_json::json!(1), "begin"));
        t.on_progress(&progress(serde_json::json!(1), "end"));
        t.on_server_status(&serde_json::json!({ "quiescent": false }));
        assert!(!t.is_ready(), "the server's own status outranks progress");
        // And the reverse: quiescent true while a token is in flight.
        t.on_progress(&progress(serde_json::json!(2), "begin"));
        t.on_server_status(&serde_json::json!({ "quiescent": true }));
        assert!(t.is_ready());
    }

    #[test]
    fn workspace_configuration_response_has_one_null_per_item() {
        // gopls requests config per workspace scope; the LSP spec requires
        // the response array length to match the requested `items` length.
        // Replying [null, null] = "no override, use defaults" (mache-6584a0).
        let params = serde_json::json!({
            "items": [{"section": "gopls"}, {"section": "gopls"}, {}]
        });
        assert_eq!(
            server_request_result("workspace/configuration", Some(&params)),
            serde_json::json!([null, null, null])
        );
        // No items / missing → empty array, still a valid response.
        assert_eq!(
            server_request_result("workspace/configuration", None),
            serde_json::json!([])
        );
    }

    #[test]
    fn other_server_requests_ack_with_null() {
        // window/workDoneProgress/create MUST be acked or gopls never emits
        // $/progress → await_ready waits the full budget → 0 hovers.
        for m in [
            "window/workDoneProgress/create",
            "client/registerCapability",
            "client/unregisterCapability",
            "some/unknownServerRequest",
        ] {
            assert_eq!(server_request_result(m, None), serde_json::Value::Null);
        }
    }

    /// Construct a `LspClient` from a fake child process for tests.
    /// We can't actually spawn a server, so this synthesizes the
    /// struct directly and lets tests feed notifications via the rx
    /// channel through a paired tx (which the helper returns).
    fn fake_client_for_test() -> (LspClient, mpsc::Sender<Response>) {
        let (tx, rx) = mpsc::channel::<Response>(RESPONSE_CHANNEL_BUFFER);
        // SAFETY: we never touch `child` / `stdin` in await_ready tests.
        // The fields are required by the struct shape but the test only
        // drives `handle_notification` + `await_ready`'s polling loop.
        let child = tokio::process::Command::new("true")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn /usr/bin/true");
        let mut child = child;
        let stdin = child.stdin.take().expect("stdin");
        (
            LspClient {
                child,
                stdin,
                rx,
                next_id: 1,
                diagnostics: Vec::new(),
                readiness: ReadinessTracker::default(),
            },
            tx,
        )
    }

    #[tokio::test]
    async fn await_ready_returns_true_on_quiescent_status() {
        let (mut client, tx) = fake_client_for_test();
        // Feed an experimental/serverStatus notification with quiescent: true.
        tx.send(Response {
            id: None,
            method: Some("experimental/serverStatus".into()),
            params: Some(serde_json::json!({"quiescent": true})),
            result: None,
            error: None,
        })
        .await
        .unwrap();
        let was_ready = client
            .await_ready(std::time::Duration::from_millis(500))
            .await;
        assert!(was_ready, "quiescent: true must report ready");
        assert!(client.is_server_ready());
    }

    #[tokio::test]
    async fn await_ready_returns_true_when_progress_drains() {
        let (mut client, tx) = fake_client_for_test();
        // A full $/progress lifecycle under a title nothing recognizes —
        // readiness is the token pairing, not the prose.
        tx.send(Response {
            id: None,
            method: Some("$/progress".into()),
            params: Some(serde_json::json!({
                "token": "rustAnalyzer/Indexing",
                "value": {"kind": "begin", "title": "Reticulating splines"}
            })),
            result: None,
            error: None,
        })
        .await
        .unwrap();
        tx.send(Response {
            id: None,
            method: Some("$/progress".into()),
            params: Some(serde_json::json!({
                "token": "rustAnalyzer/Indexing",
                "value": {"kind": "end"}
            })),
            result: None,
            error: None,
        })
        .await
        .unwrap();
        let was_ready = client
            .await_ready(std::time::Duration::from_millis(500))
            .await;
        assert!(was_ready, "a drained progress cycle must report ready");
    }

    #[tokio::test]
    async fn await_ready_returns_false_on_timeout_without_signals() {
        let (mut client, _tx) = fake_client_for_test();
        let start = tokio::time::Instant::now();
        let was_ready = client
            .await_ready(std::time::Duration::from_millis(120))
            .await;
        let elapsed = start.elapsed();
        assert!(!was_ready, "no signals ⇒ timeout returns false");
        assert!(
            elapsed >= std::time::Duration::from_millis(100),
            "timeout must actually wait; elapsed={elapsed:?}"
        );
    }

    #[tokio::test]
    async fn quiescent_false_does_not_flip_ready() {
        let (mut client, tx) = fake_client_for_test();
        tx.send(Response {
            id: None,
            method: Some("experimental/serverStatus".into()),
            params: Some(serde_json::json!({"quiescent": false, "health": "warning"})),
            result: None,
            error: None,
        })
        .await
        .unwrap();
        let was_ready = client
            .await_ready(std::time::Duration::from_millis(120))
            .await;
        assert!(!was_ready, "quiescent: false must NOT report ready");
    }
}
