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
}

impl LspClient {
    /// Spawn a language server and perform the LSP handshake.
    pub async fn start(command: &str, args: &[&str], root_uri: &str) -> Result<Self> {
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
        };

        // Initialize handshake
        let init_params = serde_json::json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
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
            Err(_) => Ok(vec![]),
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
            Err(_) => Ok(vec![]),
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
        if let Some(method) = &msg.method
            && method == "textDocument/publishDiagnostics"
            && let Some(params) = &msg.params
            && let Ok(diag) = serde_json::from_value::<PublishDiagnosticsParams>(params.clone())
        {
            self.diagnostics
                .push((diag.uri.to_string(), diag.diagnostics));
        }
    }

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
