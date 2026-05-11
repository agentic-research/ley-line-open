//! Typed Rust serde mirrors of `rs/ll-core/public-schema/capnp/daemon.capnp`.
//!
//! Each struct corresponds 1:1 to a capnp message in the schema. The
//! schema's camelCase field names are mapped to the snake_case JSON wire
//! via `#[serde(rename = "...")]` per the JSON-as-carrier doctrine
//! (cloister `interlace-spec/0.1.0/README.md` lines 92-133): the typed
//! contract is the schema; the carrier-format naming is a per-side tag.
//!
//! `ops.rs` handlers build these structs and serialize via
//! `serde_json::to_string(&typed_response)?` instead of hand-building
//! JSON via the `json!({...})` macro. That makes the schema genuinely
//! load-bearing — adding a field to the schema requires touching this
//! file (compile error), and forgetting to wire a handler to its typed
//! struct can't silently emit drift.
//!
//! Bead: ley-line-open-b69606 (A-3).

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Request enum — typed dispatch surface for socket.rs.
//
// Each variant corresponds to one op in `base_op_names`. Args land in the
// variant's named fields with serde's `tag = "op"` rename pulling the `op`
// JSON field as the discriminator. `#[serde(default)]` on optional fields
// keeps the request small for ops that don't use them.
//
// socket.rs tries to deserialize the incoming line as `BaseRequest`. On
// success → typed dispatch into `handle_base_op`. On failure → falls
// through to event ops (subscribe/unsubscribe/emit) and extension
// dispatch with the raw `serde_json::Value`. Adding a new op means:
//   1. New variant here.
//   2. Match arm in `handle_base_op`.
//   3. Match arm in `base_op_names` test list (compile error names it).
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BaseRequest {
    Status,
    Flush,
    Load {
        /// Base64-encoded .db bytes.
        db: String,
    },
    Query {
        sql: String,
        #[serde(default)]
        limit: Option<usize>,
    },
    Reparse {
        #[serde(default)]
        source: Option<String>,
        #[serde(default)]
        lang: Option<String>,
        #[serde(default)]
        files: Option<Vec<String>>,
    },
    Snapshot,
    Enrich {
        pass: String,
        #[serde(default)]
        files: Option<Vec<String>>,
    },
    ListRoots,
    ListChildren {
        #[serde(default)]
        id: Option<String>,
    },
    ReadContent {
        id: String,
    },
    FindCallers {
        token: String,
    },
    FindCallees {
        id: String,
    },
    FindDefs {
        token: String,
    },
    GetNode {
        id: String,
    },
    GetRefsMap,
    GetDefsMap,
    GetSchema,
    GetDbPath,
    LspHover(LspPosition),
    LspDefs(LspPosition),
    LspRefs(LspPosition),
    LspSymbols(LspFile),
    LspDiagnostics(LspFile),
    #[cfg(feature = "vec")]
    VecSearch {
        query: String,
        #[serde(default = "default_vec_k")]
        k: u32,
    },
}

/// Position-based LSP request args (lsp_hover, lsp_defs, lsp_refs).
#[derive(Deserialize, Debug)]
pub struct LspPosition {
    pub file: String,
    pub line: u32,
    #[serde(default)]
    pub col: u32,
}

/// File-scoped LSP request args (lsp_symbols, lsp_diagnostics).
#[derive(Deserialize, Debug)]
pub struct LspFile {
    pub file: String,
}

#[cfg(feature = "vec")]
fn default_vec_k() -> u32 {
    10
}

// ---------------------------------------------------------------------------
// Shared payload types — used inside multiple response variants.
// ---------------------------------------------------------------------------

/// Full node row. Matches `Node` in daemon.capnp (id, parentId, name,
/// kind, size, record). Wire emits snake_case; struct field names follow
/// Rust convention.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: String,
    #[serde(rename = "parent_id")]
    pub parent_id: String,
    pub name: String,
    pub kind: i32,
    pub size: i64,
    pub record: String,
}

/// A token reference, used by find_callers / find_defs / find_callees.
/// Matches `Ref` in daemon.capnp.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    #[serde(rename = "node_id")]
    pub node_id: String,
    #[serde(rename = "source_id")]
    pub source_id: String,
}

/// Bulk token-map entry — one token plus the list of nodes it
/// references (or defines, depending on which map). Matches
/// `TokenMapEntry` in daemon.capnp. Used by get_refs_map / get_defs_map.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TokenMapEntry {
    pub token: String,
    #[serde(rename = "node_ids")]
    pub node_ids: Vec<String>,
}

/// One tier in LLO's layer-ownership topology. Matches `SchemaTier` in
/// daemon.capnp.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SchemaTier {
    pub name: String,
    pub crates: Vec<String>,
}

// ---------------------------------------------------------------------------
// Per-op response shapes.
//
// Convention: `ok` is always first. Optional fields use `Option<T>` with
// `#[serde(skip_serializing_if = "Option::is_none")]` so absent fields
// don't bloat the wire. Each field has a `#[serde(rename = "...")]` if
// the JSON wire name differs from the Rust field name.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StatusResponse {
    pub ok: bool,
    pub phase: String,
    #[serde(rename = "current_root")]
    pub current_root: String,
    #[serde(rename = "arena_path")]
    pub arena_path: String,
    #[serde(rename = "arena_size")]
    pub arena_size: u64,
    /// JSON-encoded per-pass enrichment status map. Schema declares
    /// `enrichment: Text`; here we carry an opaque JSON Value so
    /// callers can serialize a typed map into the field without
    /// going through Text marshaling. Always present; empty object
    /// when no passes have run.
    pub enrichment: serde_json::Value,
    #[serde(rename = "head_sha", skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    #[serde(rename = "last_reparse_at_ms", skip_serializing_if = "Option::is_none")]
    pub last_reparse_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FlushResponse {
    pub ok: bool,
    #[serde(rename = "current_root")]
    pub current_root: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SnapshotResponse {
    pub ok: bool,
    #[serde(rename = "current_root")]
    pub current_root: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LoadResponse {
    pub ok: bool,
    #[serde(rename = "current_root")]
    pub current_root: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReparseResponse {
    pub ok: bool,
    #[serde(rename = "current_root")]
    pub current_root: String,
    pub parsed: u64,
    pub unchanged: u64,
    pub deleted: u64,
    pub errors: u64,
    #[serde(rename = "changed_files")]
    pub changed_files: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EnrichResponse {
    pub ok: bool,
    #[serde(rename = "current_root")]
    pub current_root: String,
    /// Schema declares `passes: List(EnrichmentStats)`; we carry it
    /// as opaque JSON for now since the handler computes a Vec of
    /// serde-friendly structs already and conversion to a typed
    /// `EnrichmentStats` Vec is mechanical (deferred to a follow-up).
    pub passes: serde_json::Value,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ListChildrenResponse {
    pub ok: bool,
    pub children: Vec<Node>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReadContentResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GetNodeResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<Node>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FindCallersResponse {
    pub ok: bool,
    pub callers: Vec<Ref>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FindDefsResponse {
    pub ok: bool,
    pub defs: Vec<Ref>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FindCalleesResponse {
    pub ok: bool,
    pub callees: Vec<Ref>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GetRefsMapResponse {
    pub ok: bool,
    pub entries: Vec<TokenMapEntry>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GetDefsMapResponse {
    pub ok: bool,
    pub entries: Vec<TokenMapEntry>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GetSchemaResponse {
    pub ok: bool,
    pub tiers: Vec<SchemaTier>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GetDbPathResponse {
    pub ok: bool,
    #[serde(rename = "db_path")]
    pub db_path: String,
    #[serde(rename = "ctrl_path")]
    pub ctrl_path: String,
    #[serde(rename = "bindings_path")]
    pub bindings_path: String,
    #[serde(rename = "ast_path")]
    pub ast_path: String,
    #[serde(rename = "source_path")]
    pub source_path: String,
    #[serde(rename = "head_path")]
    pub head_path: String,
}

/// Common error envelope. Matches `ErrorResponse` in daemon.capnp.
/// Used when an op returns `ok: false` and the only meaningful field
/// is `error`. Distinct from per-op error variants (like
/// `ReadContentResponse.error`) which still belong inside their
/// op-specific struct.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ErrorResponse {
    pub ok: bool,
    pub error: String,
}

impl ErrorResponse {
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: error.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// JSON helpers — every op handler uses these instead of `json!({...})`.
// ---------------------------------------------------------------------------

/// Serialize a typed response into the JSON wire shape. Panics only on
/// allocator failure (serde_json::to_string is infallible for the types
/// in this module — all derive Serialize and have no recursive Cycle).
pub fn to_wire<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("typed wire serialization is infallible")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sanity: status response round-trips through JSON with the
    /// expected snake_case field names. This is the load-bearing
    /// contract — if anyone renames a field's serde tag, this test
    /// notices.
    #[test]
    fn status_response_wire_shape() {
        let resp = StatusResponse {
            ok: true,
            phase: "ready".into(),
            current_root: "0".repeat(64),
            arena_path: "/tmp/test".into(),
            arena_size: 1024,
            enrichment: serde_json::json!({}),
            head_sha: Some("abc123".into()),
            last_reparse_at_ms: Some(42i64),
            error: None,
        };
        let wire = to_wire(&resp);
        // Required keys present, snake_case.
        assert!(wire.contains("\"ok\":true"));
        assert!(wire.contains("\"current_root\":"));
        assert!(wire.contains("\"arena_path\":"));
        assert!(wire.contains("\"arena_size\":1024"));
        assert!(wire.contains("\"head_sha\":\"abc123\""));
        assert!(wire.contains("\"last_reparse_at_ms\":42"));
        // Optional unset field is elided.
        assert!(!wire.contains("\"error\""));
    }

    #[test]
    fn ref_wire_uses_snake_case() {
        let r = Ref {
            node_id: "n".into(),
            source_id: "s".into(),
        };
        let wire = to_wire(&r);
        assert_eq!(wire, r#"{"node_id":"n","source_id":"s"}"#);
    }

    #[test]
    fn token_map_entry_wire_uses_node_ids_snake() {
        let e = TokenMapEntry {
            token: "t".into(),
            node_ids: vec!["a".into(), "b".into()],
        };
        let wire = to_wire(&e);
        assert_eq!(wire, r#"{"token":"t","node_ids":["a","b"]}"#);
    }

    #[test]
    fn error_response_constructor() {
        let e = ErrorResponse::new("not found");
        let wire = to_wire(&e);
        assert_eq!(wire, r#"{"ok":false,"error":"not found"}"#);
    }
}
