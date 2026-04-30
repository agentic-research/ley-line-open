//! MCP Streamable HTTP transport for the leyline daemon.
//!
//! Wraps the existing UDS op dispatch table (`ops::handle_base_op`) in an
//! MCP JSON-RPC shape so cloister (or any MCP-aware client) can route tool
//! calls into LLO without speaking the line-delimited UDS protocol.
//!
//! Single endpoint:
//! - `POST /mcp` — JSON-RPC requests (`initialize`, `tools/list`, `tools/call`)
//! - `GET /mcp`  — SSE stream for server→client notifications (stub for now)
//!
//! Tool surface = base daemon ops, 1:1. The tool's `name` is the op's name;
//! the tool's `arguments` object is the request JSON the op already expects.
//! No new protocol; this is a transport wrapper.
//!
//! Mirrors the pattern in `rosary/src/serve/mod.rs::run_http`.
//!
//! Per-tool authorization, mTLS termination, and identity scoping are
//! intentionally out of scope here — those live in the cloister gateway and
//! notme-proxy. This module assumes localhost-only or already-attested
//! traffic.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::DaemonContext;

// ---------------------------------------------------------------------------
// Tool registry — static descriptions of every op exposed over MCP.
// ---------------------------------------------------------------------------

/// One entry in `tools/list`. Names must match the `op` field that
/// `ops::handle_base_op` already dispatches on.
struct McpTool {
    name: &'static str,
    description: &'static str,
    schema: Value,
}

fn tool_registry() -> Vec<McpTool> {
    #[allow(unused_mut)]
    let mut tools = vec![
        McpTool {
            name: "status",
            description: "Daemon lifecycle status: phase, head_sha, last_reparse_at_ms, per-pass enrichment.",
            schema: json!({"type": "object", "properties": {}}),
        },
        McpTool {
            name: "snapshot",
            description: "Force a snapshot of the living db into the arena.",
            schema: json!({"type": "object", "properties": {}}),
        },
        McpTool {
            name: "reparse",
            description: "Re-run tree-sitter parsing. Pass `files: [paths]` to scope to specific files (preferred for hooks). `source` may be a directory or a single file path; a file is auto-rewritten to (parent, scope=[basename]) so PostToolUse hooks can forward `tool_input.file_path` directly.",
            schema: json!({
                "type": "object",
                "properties": {
                    "source": {"type": "string", "description": "Source dir or file; falls back to daemon --source"},
                    "files":  {"type": "array", "items": {"type": "string"}, "description": "Scope to these files (relative to source dir)"},
                    "lang":   {"type": "string", "description": "Optional language filter"}
                }
            }),
        },
        McpTool {
            name: "enrich",
            description: "Run an enrichment pass (e.g. `lsp`, `embed`) optionally scoped to specific files.",
            schema: json!({
                "type": "object",
                "properties": {
                    "pass":  {"type": "string"},
                    "files": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["pass"]
            }),
        },
        McpTool {
            name: "query",
            description: "Run an arbitrary SQL query against the living db.",
            schema: json!({
                "type": "object",
                "properties": {"sql": {"type": "string"}},
                "required": ["sql"]
            }),
        },
        McpTool {
            name: "list_children",
            description: "List child nodes of a given parent id.",
            schema: json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"]
            }),
        },
        McpTool {
            name: "read_content",
            description: "Read a node's source content (the `record` column).",
            schema: json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"]
            }),
        },
        McpTool {
            name: "find_callers",
            description: "Find references of a token (queries node_refs).",
            schema: json!({
                "type": "object",
                "properties": {"token": {"type": "string"}},
                "required": ["token"]
            }),
        },
        McpTool {
            name: "find_defs",
            description: "Find definitions of a token (queries node_defs).",
            schema: json!({
                "type": "object",
                "properties": {"token": {"type": "string"}},
                "required": ["token"]
            }),
        },
        McpTool {
            name: "get_node",
            description: "Fetch a single node by id.",
            schema: json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"]
            }),
        },
        // ── LSP ops (always exposed; auto-enrichment kicks in lazily) ────
        McpTool {
            name: "lsp_hover",
            description: "Position-based LSP hover; resolves (file, line, col) to the node and returns hover text.",
            schema: position_schema(),
        },
        McpTool {
            name: "lsp_defs",
            description: "Position-based LSP definitions.",
            schema: position_schema(),
        },
        McpTool {
            name: "lsp_refs",
            description: "Position-based LSP references.",
            schema: position_schema(),
        },
        McpTool {
            name: "lsp_symbols",
            description: "Document symbols for a file.",
            schema: file_schema(),
        },
        McpTool {
            name: "lsp_diagnostics",
            description: "Diagnostics for a file (enriched on demand if missing).",
            schema: file_schema(),
        },
    ];

    #[cfg(feature = "vec")]
    tools.push(McpTool {
        name: "vec_search",
        description: "KNN search over the sidecar VectorIndex; embeds the query via the active Embedder.",
        schema: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "k":     {"type": "integer", "default": 10}
            },
            "required": ["query"]
        }),
    });

    tools
}

fn position_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "file": {"type": "string"},
            "line": {"type": "integer", "description": "Zero-based line."},
            "col":  {"type": "integer", "description": "Zero-based column."}
        },
        "required": ["file", "line", "col"]
    })
}

fn file_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"file": {"type": "string"}},
        "required": ["file"]
    })
}

// ---------------------------------------------------------------------------
// JSON-RPC types — minimal subset we need for MCP Streamable HTTP.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    method: String,
    #[serde(default)]
    params: Value,
    id: Option<Value>,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

impl JsonRpcResponse {
    fn ok(id: Option<Value>, result: Value) -> Self {
        Self { jsonrpc: "2.0", id, result: Some(result), error: None }
    }
    fn err(id: Option<Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError { code, message: message.into(), data: None }),
        }
    }
}

// ---------------------------------------------------------------------------
// Server entry point
// ---------------------------------------------------------------------------

/// Spawn the MCP HTTP server bound to `127.0.0.1:port`. Returns a join
/// handle so the caller can keep it alive (or `.abort()` it on shutdown).
pub fn spawn(ctx: Arc<DaemonContext>, port: u16) -> Result<tokio::task::JoinHandle<()>> {
    let app = Router::new()
        .route("/mcp", post(handle_post).get(handle_get))
        .with_state(ctx);

    let addr: SocketAddr = format!("127.0.0.1:{port}").parse()?;
    let std_listener = std::net::TcpListener::bind(addr)
        .with_context(|| format!("bind MCP HTTP on {addr}"))?;
    std_listener
        .set_nonblocking(true)
        .context("set TCP listener non-blocking")?;
    let listener = tokio::net::TcpListener::from_std(std_listener)
        .context("convert TCP listener")?;

    eprintln!("MCP HTTP server listening on http://{addr}/mcp");

    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            log::error!("MCP HTTP server exited: {e:#}");
        }
    });
    Ok(handle)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `POST /mcp` — JSON-RPC requests.
async fn handle_post(
    State(ctx): State<Arc<DaemonContext>>,
    body: String,
) -> Response {
    let request: JsonRpcRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            return json_response(JsonRpcResponse::err(
                None,
                -32700,
                format!("parse error: {e}"),
            ));
        }
    };

    if request.jsonrpc != "2.0" {
        return json_response(JsonRpcResponse::err(
            request.id,
            -32600,
            "jsonrpc must be \"2.0\"",
        ));
    }

    // Notifications (no id) — accept silently.
    let id = match request.id {
        Some(v) => v,
        None => return StatusCode::ACCEPTED.into_response(),
    };

    let response = match request.method.as_str() {
        "initialize" => JsonRpcResponse::ok(
            Some(id),
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "leyline", "version": env!("CARGO_PKG_VERSION")}
            }),
        ),
        "tools/list" => handle_tools_list(id),
        "tools/call" => handle_tools_call(&ctx, id, &request.params),
        "ping" => JsonRpcResponse::ok(Some(id), json!({})),
        other => JsonRpcResponse::err(Some(id), -32601, format!("method not found: {other}")),
    };

    json_response(response)
}

/// `GET /mcp` — SSE for server→client notifications. v1 stub: 405.
///
/// A future commit can pipe `EventRouter` events out here.
async fn handle_get() -> StatusCode {
    StatusCode::METHOD_NOT_ALLOWED
}

fn json_response(resp: JsonRpcResponse) -> Response {
    (StatusCode::OK, axum::Json(resp)).into_response()
}

fn handle_tools_list(id: Value) -> JsonRpcResponse {
    let tools: Vec<Value> = tool_registry()
        .into_iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.schema,
            })
        })
        .collect();
    JsonRpcResponse::ok(Some(id), json!({"tools": tools}))
}

/// `tools/call` — translate `{ name, arguments }` into the daemon's existing
/// op shape and dispatch through `handle_base_op`.
fn handle_tools_call(ctx: &DaemonContext, id: Value, params: &Value) -> JsonRpcResponse {
    let Some(name) = params.get("name").and_then(|v| v.as_str()) else {
        return JsonRpcResponse::err(Some(id), -32602, "missing `name` in params");
    };

    // Build the op request: clone arguments verbatim, add `op` field.
    let mut req = match params.get("arguments") {
        Some(Value::Object(map)) => Value::Object(map.clone()),
        Some(Value::Null) | None => Value::Object(serde_json::Map::new()),
        Some(other) => {
            return JsonRpcResponse::err(
                Some(id),
                -32602,
                format!("`arguments` must be an object, got {}", type_of(other)),
            );
        }
    };
    if let Value::Object(ref mut map) = req {
        map.insert("op".into(), Value::String(name.to_string()));
    }

    let Some(response) = super::ops::handle_base_op(ctx, name, &req) else {
        return JsonRpcResponse::err(
            Some(id),
            -32601,
            format!("unknown tool: {name}"),
        );
    };

    // `handle_base_op` returns an already-serialized JSON string. MCP tool
    // results must wrap content in `{content: [{type: "text", text: ...}]}`.
    //
    // Surface inner-op failures via `isError: true` per the MCP spec. We
    // detect failure by parsing the response and checking `ok == false` —
    // the convention every base op uses.
    let is_error = serde_json::from_str::<Value>(&response)
        .ok()
        .and_then(|v| v.get("ok").and_then(|b| b.as_bool()))
        .map(|ok| !ok)
        .unwrap_or(false);

    JsonRpcResponse::ok(
        Some(id),
        json!({
            "content": [{"type": "text", "text": response}],
            "isError": is_error,
        }),
    )
}

fn type_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonrpc_response_factories_set_protocol_version() {
        // JsonRpcResponse::ok and ::err embed jsonrpc: "2.0" — the
        // JSON-RPC 2.0 protocol identifier. Clients dispatch on it;
        // a typo (e.g. "2.O") would break MCP interop. ok must set
        // result=Some, error=None; err inverts. id must round-trip
        // verbatim.
        let id = Some(serde_json::json!(7));
        let ok = JsonRpcResponse::ok(id.clone(), serde_json::json!({"x": 1}));
        assert_eq!(ok.jsonrpc, "2.0");
        assert_eq!(ok.id, id);
        assert!(ok.result.is_some());
        assert!(ok.error.is_none());

        let err = JsonRpcResponse::err(id.clone(), -32600, "bad request");
        assert_eq!(err.jsonrpc, "2.0");
        assert_eq!(err.id, id);
        assert!(err.result.is_none());
        let je = err.error.as_ref().unwrap();
        assert_eq!(je.code, -32600);
        assert_eq!(je.message, "bad request");
        assert!(je.data.is_none());
    }

    #[test]
    fn registry_is_non_empty_and_unique() {
        let tools = tool_registry();
        assert!(!tools.is_empty(), "registry should expose at least one tool");
        let mut seen = std::collections::HashSet::new();
        for t in &tools {
            assert!(
                seen.insert(t.name),
                "duplicate tool name in registry: {}",
                t.name,
            );
        }
    }

    #[test]
    fn registry_includes_lsp_ops() {
        let names: std::collections::HashSet<&str> =
            tool_registry().into_iter().map(|t| t.name).collect();
        for op in ["lsp_hover", "lsp_defs", "lsp_refs", "lsp_symbols", "lsp_diagnostics"] {
            assert!(names.contains(op), "missing LSP tool: {op}");
        }
    }

    #[test]
    fn registry_names_are_subset_of_canonical_op_names() {
        // Drift guard: every MCP tool's `name` must correspond to an op
        // that `handle_base_op` dispatches. If you add a tool here but
        // forget the match arm in `daemon::ops`, this test fails — and
        // MCP requests would otherwise silently return "unknown op"
        // errors at runtime.
        let canonical: std::collections::HashSet<&str> =
            crate::daemon::ops::base_op_names().into_iter().collect();
        for tool in tool_registry() {
            assert!(
                canonical.contains(tool.name),
                "MCP tool `{}` is not in base_op_names() — handle_base_op cannot dispatch it",
                tool.name,
            );
        }
    }
}
