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
    /// Set when the server has signalled quiescence — either via
    /// rust-analyzer's `experimental/serverStatus` notification with
    /// `quiescent: true`, or via `$/progress` end for an indexing token
    /// (any token whose title contains "indexing" / "loading" / "ready",
    /// case-insensitive). `await_ready` polls this. Always-true for
    /// servers that don't emit indexing signals — those callers should
    /// pass `wait: false` instead of calling `await_ready` at all.
    server_ready: bool,
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

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

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
            server_ready: false,
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
        // the server is done indexing + analysis. Cheaper than waiting
        // for `$/progress` (which we'd have to parse the title of to
        // distinguish "Indexing" from "Discovering tests" etc.).
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

        // Read messages until we get our response (collecting notifications along the way)
        loop {
            let msg = self
                .rx
                .recv()
                .await
                .context("LSP server closed connection")?;

            // Server notification (e.g. publishDiagnostics)
            if msg.id.is_none() {
                self.handle_notification(&msg);
                continue;
            }

            if msg.id == Some(id) {
                if let Some(err) = msg.error {
                    bail!("LSP error {}: {}", err.code, err.message);
                }
                return Ok(msg.result.unwrap_or(serde_json::Value::Null));
            }
            // Response for a different ID — skip (shouldn't happen in serial usage)
        }
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
            if msg.id.is_none() {
                self.handle_notification(&msg);
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
                // Generic LSP work-done progress. Indexing/loading tokens
                // signal ready when they emit `kind: "end"`. We don't
                // track per-token state — any "end" of a recognized
                // indexing-ish token flips the ready flag. False positives
                // (a non-indexing token whose title matches "loading"
                // ends early) are harmless: subsequent queries either
                // succeed (server actually was ready) or return empty
                // (caller falls back to per-symbol retry). Bead
                // `ley-line-open-661727`.
                if let Some(params) = &msg.params
                    && let Some(value) = params.get("value")
                    && let Some(kind) = value.get("kind").and_then(|v| v.as_str())
                {
                    match kind {
                        "begin" => {
                            if let Some(title) = value.get("title").and_then(|v| v.as_str())
                                && is_readiness_token(title)
                            {
                                // Reset on begin in case a fresh indexing
                                // cycle starts after we already flipped
                                // ready (rust-analyzer reindexes on
                                // Cargo.toml change).
                                self.server_ready = false;
                            }
                        }
                        "end" => {
                            // Without per-token bookkeeping we can't tell
                            // which token just ended. Conservative read:
                            // any `end` flips ready true. The pass's
                            // `await_ready` callers verify via subsequent
                            // empty-results retry anyway.
                            self.server_ready = true;
                        }
                        _ => {}
                    }
                }
            }
            "experimental/serverStatus" => {
                // rust-analyzer-specific (`experimental.serverStatusNotification`
                // capability declared in `start`). When `quiescent: true`
                // the server has finished its current analysis sweep —
                // hover/definition/references are now backed by the
                // resolved project model. This is strictly cheaper than
                // parsing $/progress titles.
                if let Some(params) = &msg.params
                    && let Some(quiescent) = params.get("quiescent").and_then(|v| v.as_bool())
                    && quiescent
                {
                    self.server_ready = true;
                }
            }
            _ => {}
        }
    }

    /// Wait for the language server to signal readiness for semantic
    /// queries (hover / definition / references). Polls
    /// `server_ready` (flipped by `$/progress` indexing-token end and
    /// rust-analyzer's `experimental/serverStatus quiescent: true`) up
    /// to `timeout`. Returns `true` if ready signal arrived, `false`
    /// on timeout. Callers should still issue queries on timeout — the
    /// server may have skipped progress notifications (older language
    /// servers, or one that doesn't index).
    ///
    /// Bead `ley-line-open-661727`: documentSymbol is syntactic and
    /// returns immediately, but hover/def/refs need the workspace's
    /// project model loaded. Issuing them before the indexing cycle
    /// completes is the root cause of "25 symbols, 0 hovers/defs/refs."
    pub async fn await_ready(&mut self, timeout: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        let poll_interval = std::time::Duration::from_millis(50);
        loop {
            // Drain any pending notifications first (no waiting); each
            // tick may flip server_ready via $/progress or
            // experimental/serverStatus.
            while let Ok(msg) = self.rx.try_recv() {
                if msg.id.is_none() {
                    self.handle_notification(&msg);
                }
            }
            if self.server_ready {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(poll_interval).await;
        }
    }

    /// Test-only: peek at the readiness flag. Production callers use
    /// `await_ready` which drains notifications first.
    #[cfg(test)]
    pub fn is_server_ready(&self) -> bool {
        self.server_ready
    }
}

/// Recognize indexing-related `$/progress` titles. Conservative match
/// on common substrings; case-insensitive. rust-analyzer uses
/// `"rust-analyzer/Indexing"`, gopls uses `"setting up workspace"`,
/// pyright uses `"Indexing"`. Match what's there empirically + don't
/// over-match (a non-indexing token title containing "loading" would
/// flip ready prematurely; the cost is one wasted retry, not
/// correctness loss).
fn is_readiness_token(title: &str) -> bool {
    let t = title.to_ascii_lowercase();
    t.contains("indexing")
        || t.contains("loading")
        || t.contains("workspace")
        || t.contains("ready")
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

    #[test]
    fn is_readiness_token_matches_known_titles() {
        // rust-analyzer
        assert!(is_readiness_token("rust-analyzer/Indexing"));
        assert!(is_readiness_token("Indexing"));
        // pyright
        assert!(is_readiness_token("Pyright: Indexing"));
        // gopls
        assert!(is_readiness_token("Setting up workspace"));
        assert!(is_readiness_token("Loading packages"));
        // generic
        assert!(is_readiness_token("Server ready"));
    }

    #[test]
    fn is_readiness_token_rejects_non_indexing_titles() {
        // Common non-indexing progress titles.
        assert!(!is_readiness_token("Run cargo test"));
        assert!(!is_readiness_token("Diagnostics published"));
        assert!(!is_readiness_token(""));
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
                server_ready: false,
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
        assert!(was_ready, "quiescent: true must flip server_ready");
        assert!(client.is_server_ready());
    }

    #[tokio::test]
    async fn await_ready_returns_true_on_progress_end_for_indexing_token() {
        let (mut client, tx) = fake_client_for_test();
        // rust-analyzer-style $/progress lifecycle for an indexing token.
        tx.send(Response {
            id: None,
            method: Some("$/progress".into()),
            params: Some(serde_json::json!({
                "token": "rustAnalyzer/Indexing",
                "value": {"kind": "begin", "title": "rust-analyzer/Indexing"}
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
        assert!(was_ready, "$/progress end must flip server_ready");
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
        assert!(!was_ready, "quiescent: false must NOT flip server_ready");
    }
}
