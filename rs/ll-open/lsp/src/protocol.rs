//! JSON-RPC framing types + re-exports from `lsp_types`.
//!
//! LSP data types (DocumentSymbol, Diagnostic, Location, Hover, etc.) come from
//! the `lsp-types` crate. We only hand-roll the JSON-RPC envelope types since
//! `lsp-types` doesn't include those.

use serde::{Deserialize, Serialize};

// Re-export the types we use from lsp-types so consumers can import from one place.
pub use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionResponse, Diagnostic, DiagnosticSeverity,
    DocumentSymbol, GotoDefinitionResponse, Hover, HoverContents, Location, MarkedString,
    MarkupContent, MarkupKind, Position, Range, SymbolKind, Url,
};

// ── JSON-RPC framing ────────────────────────────────────────────

#[derive(Serialize)]
pub struct Request {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'static str,
    pub params: serde_json::Value,
}

impl Request {
    pub fn new(id: u64, method: &'static str, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method,
            params,
        }
    }
}

#[derive(Serialize)]
pub struct Notification {
    pub jsonrpc: &'static str,
    pub method: &'static str,
    pub params: serde_json::Value,
}

impl Notification {
    pub fn new(method: &'static str, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0",
            method,
            params,
        }
    }
}

#[derive(Deserialize, Debug)]
pub struct Response {
    pub id: Option<u64>,
    pub result: Option<serde_json::Value>,
    pub error: Option<ResponseError>,
    pub method: Option<String>,
    pub params: Option<serde_json::Value>,
}

#[derive(Deserialize, Debug)]
pub struct ResponseError {
    pub code: i64,
    pub message: String,
}

// ── Helpers ─────────────────────────────────────────────────────

/// LSP SymbolKind → human-readable name.
pub fn symbol_kind_name(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::FILE => "file",
        SymbolKind::MODULE => "module",
        SymbolKind::NAMESPACE => "namespace",
        SymbolKind::PACKAGE => "package",
        SymbolKind::CLASS => "class",
        SymbolKind::METHOD => "method",
        SymbolKind::PROPERTY => "property",
        SymbolKind::FIELD => "field",
        SymbolKind::CONSTRUCTOR => "constructor",
        SymbolKind::ENUM => "enum",
        SymbolKind::INTERFACE => "interface",
        SymbolKind::FUNCTION => "function",
        SymbolKind::VARIABLE => "variable",
        SymbolKind::CONSTANT => "constant",
        SymbolKind::STRING => "string",
        SymbolKind::NUMBER => "number",
        SymbolKind::BOOLEAN => "boolean",
        SymbolKind::ARRAY => "array",
        SymbolKind::OBJECT => "object",
        SymbolKind::KEY => "key",
        SymbolKind::NULL => "null",
        SymbolKind::ENUM_MEMBER => "enum_member",
        SymbolKind::STRUCT => "struct",
        SymbolKind::EVENT => "event",
        SymbolKind::OPERATOR => "operator",
        SymbolKind::TYPE_PARAMETER => "type_parameter",
        _ => "unknown",
    }
}

/// Diagnostic severity → name.
pub fn severity_name(severity: Option<DiagnosticSeverity>) -> &'static str {
    match severity {
        Some(DiagnosticSeverity::ERROR) => "error",
        Some(DiagnosticSeverity::WARNING) => "warning",
        Some(DiagnosticSeverity::INFORMATION) => "info",
        Some(DiagnosticSeverity::HINT) => "hint",
        _ => "unknown",
    }
}

/// CompletionItemKind → human-readable name.
pub fn completion_kind_name(kind: Option<CompletionItemKind>) -> &'static str {
    match kind {
        Some(CompletionItemKind::TEXT) => "text",
        Some(CompletionItemKind::METHOD) => "method",
        Some(CompletionItemKind::FUNCTION) => "function",
        Some(CompletionItemKind::CONSTRUCTOR) => "constructor",
        Some(CompletionItemKind::FIELD) => "field",
        Some(CompletionItemKind::VARIABLE) => "variable",
        Some(CompletionItemKind::CLASS) => "class",
        Some(CompletionItemKind::INTERFACE) => "interface",
        Some(CompletionItemKind::MODULE) => "module",
        Some(CompletionItemKind::PROPERTY) => "property",
        Some(CompletionItemKind::UNIT) => "unit",
        Some(CompletionItemKind::VALUE) => "value",
        Some(CompletionItemKind::ENUM) => "enum",
        Some(CompletionItemKind::KEYWORD) => "keyword",
        Some(CompletionItemKind::SNIPPET) => "snippet",
        Some(CompletionItemKind::COLOR) => "color",
        Some(CompletionItemKind::FILE) => "file",
        Some(CompletionItemKind::REFERENCE) => "reference",
        Some(CompletionItemKind::FOLDER) => "folder",
        Some(CompletionItemKind::ENUM_MEMBER) => "enum_member",
        Some(CompletionItemKind::CONSTANT) => "constant",
        Some(CompletionItemKind::STRUCT) => "struct",
        Some(CompletionItemKind::EVENT) => "event",
        Some(CompletionItemKind::OPERATOR) => "operator",
        Some(CompletionItemKind::TYPE_PARAMETER) => "type_parameter",
        _ => "unknown",
    }
}

/// Extract plain text from LSP hover contents.
pub fn hover_to_plaintext(hover: &Hover) -> String {
    match &hover.contents {
        HoverContents::Scalar(MarkedString::String(s)) => s.clone(),
        HoverContents::Scalar(MarkedString::LanguageString(ls)) => ls.value.clone(),
        HoverContents::Markup(m) => m.value.clone(),
        HoverContents::Array(arr) => arr
            .iter()
            .map(|ms| match ms {
                MarkedString::String(s) => s.clone(),
                MarkedString::LanguageString(ls) => ls.value.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Extract plain text from completion documentation.
pub fn completion_doc_text(doc: &lsp_types::Documentation) -> String {
    match doc {
        lsp_types::Documentation::String(s) => s.clone(),
        lsp_types::Documentation::MarkupContent(m) => m.value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_factory_sets_jsonrpc_2_0() {
        // Request::new bakes in jsonrpc: "2.0". The LSP server
        // dispatches on this exact string per the JSON-RPC 2.0 spec;
        // a typo or version bump would break every server we talk to.
        // Sister pin to mcp::JsonRpcResponse factory contract.
        let req = Request::new(7, "initialize", serde_json::json!({"x": 1}));
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.id, 7);
        assert_eq!(req.method, "initialize");
        let json = serde_json::to_value(&req).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(obj.get("jsonrpc").and_then(|v| v.as_str()), Some("2.0"));
        // id is u64 in our framing.
        assert_eq!(obj.get("id").and_then(|v| v.as_u64()), Some(7));
        assert_eq!(obj.get("method").and_then(|v| v.as_str()), Some("initialize"));
        assert!(obj.get("params").is_some());
    }

    #[test]
    fn notification_factory_sets_jsonrpc_2_0_no_id() {
        // Sister pin: Notification differs from Request in lacking
        // the `id` field. The LSP spec uses absence of `id` to
        // distinguish notifications from requests; misencoding here
        // would cause the server to wait for our reply forever.
        let n = Notification::new("textDocument/didOpen", serde_json::json!({}));
        assert_eq!(n.jsonrpc, "2.0");
        let json = serde_json::to_value(&n).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(obj.get("jsonrpc").and_then(|v| v.as_str()), Some("2.0"));
        assert!(!obj.contains_key("id"), "Notification must NOT carry an id");
        assert_eq!(obj.get("method").and_then(|v| v.as_str()), Some("textDocument/didOpen"));
    }
}
