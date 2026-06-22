//! Request-side typed dispatch for the daemon UDS + MCP wire.
//!
//! Post-b0ea2e (the capnp-json adoption), the **response** side of the
//! wire is generated entirely by `capnp_json::to_json` against the
//! typed builders in `leyline_public_schema::daemon_capnp::*`. There is
//! no hand-written response mirror anymore — the schema in
//! `rs/ll-core/public-schema/capnp/daemon.capnp` is the load-bearing
//! contract for response shape, enforced at compile time through the
//! typed builder API.
//!
//! What stays in this module:
//!
//! - `BaseRequest`: the serde tagged enum that decodes incoming wire
//!   lines into one of the base ops. Lives here rather than going
//!   through capnp-json on the request side because the dispatch enum
//!   is cleaner as a serde-driven tagged union (`#[serde(tag = "op",
//!   rename_all = "snake_case")]`) than as a capnp union — the request
//!   shape is small, the args are heterogeneous per op, and serde's
//!   `from_value` already handles missing-field and unknown-op errors
//!   with structured messages.
//! - `BASE_OP_NAMES`: the canonical string-list of every known op
//!   name. Single source of truth for `is_known_base_op`, the
//!   `base_op_names()` test helper, and the `mcp::tool_registry` drift
//!   check. Adding a new op means editing this list AND adding a
//!   `BaseRequest` variant (the two are tied together by drift tests
//!   in `ops.rs`).
//! - `LspPosition` / `LspFile`: small typed-args structs the LSP ops
//!   consume.
//! - `Ref` / `TokenMapEntry`: intermediate data types used inside
//!   `ops.rs` while building capnp List populations. Not serde-
//!   serialized today (no `to_wire` callers); kept as plain data
//!   structs.
//!
//! Beads:
//! - `ley-line-open-b0ea2e` (the wire.rs codegen / capnp-json adoption thread)
//! - `ley-line-open-b632ee` (collapse of 4 op-name SoTs into BASE_OP_NAMES)

use serde::Deserialize;

// ---------------------------------------------------------------------------
// Canonical op-name list.
//
// This is THE source of truth for "is this op recognized by the
// dispatcher?" Every entry must correspond to a `BaseRequest` variant
// (with the snake_case-of-the-variant-name as its serde tag) and a
// `dispatch_typed` match arm in `ops.rs`. A drift test
// (`every_canonical_name_resolves_as_base_request_tag` in `ops.rs`)
// catches the case where a name is added here without a matching
// variant; the compiler catches the inverse (a variant without a
// dispatch arm) via the exhaustive match in `dispatch_typed`.
//
// Adding a new op means:
//   1. Add a `BaseRequest` variant below.
//   2. Add the snake_case name to `BASE_OP_NAMES`.
//   3. Add a match arm in `ops.rs::dispatch_typed` (compiler-enforced).
//   4. Optionally expose it via `tool_registry()` in `daemon::mcp`
//      (drift test enforces tool_registry ⊆ BASE_OP_NAMES).
//
// Down from 4-5 hand-maintained lists (pre-b632ee) to 2 paired
// structures with bidirectional drift tests.
// ---------------------------------------------------------------------------

/// The canonical set of op names the daemon's UDS dispatcher recognizes.
/// Feature-gated entries appear conditionally; the consts below pin the
/// expected counts so a refactor that drops a name without updating the
/// gates is caught at compile time.
pub const BASE_OP_NAMES: &[&str] = &[
    "status",
    "flush",
    "load",
    "query",
    "reparse",
    "snapshot",
    "enrich",
    "list_roots",
    "list_children",
    "read_content",
    "find_callers",
    "find_callees",
    "find_defs",
    "get_node",
    "get_refs_map",
    "get_defs_map",
    "get_schema",
    "get_db_path",
    "lsp_hover",
    "lsp_defs",
    "lsp_refs",
    "lsp_symbols",
    "lsp_diagnostics",
    "sheaf_set_topology",
    "sheaf_update_topology",
    "sheaf_invalidate",
    "sheaf_defect",
    "sheaf_stalks",
    "sheaf_status",
    "sheaf_learned_weights",
    "sheaf_reap",
    "leyline_version",
    #[cfg(feature = "vec")]
    "vec_search",
    #[cfg(feature = "text-search")]
    "text_search",
    #[cfg(feature = "validate")]
    "validate",
];

// ---------------------------------------------------------------------------
// Request enum — typed dispatch surface for socket.rs.
//
// Each variant has a serde tag matching one entry in `BASE_OP_NAMES`.
// Args land in the variant's named fields; `#[serde(default)]` on
// optional fields keeps the request small for ops that don't use them.
//
// ops::handle_base_op_value tries to deserialize the already-parsed
// Value as `BaseRequest`. On success → typed dispatch into
// `dispatch_typed`. On failure → returns an ErrorResponse on the wire.
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
    #[cfg(feature = "text-search")]
    TextSearch {
        query: String,
        #[serde(default = "default_text_search_k")]
        k: u32,
    },
    SheafSetTopology {
        #[serde(default)]
        regions: Vec<crate::daemon::sheaf_ops::SheafStalkInput>,
        #[serde(default)]
        restrictions: Vec<crate::daemon::sheaf_ops::SheafRestrictionInput>,
        #[serde(default)]
        node_stalk_dim: u32,
    },
    SheafUpdateTopology {
        #[serde(default)]
        delta: crate::daemon::sheaf_ops::TopologyDeltaInput,
        #[serde(default)]
        node_stalk_dim: u32,
    },
    SheafInvalidate {
        #[serde(default)]
        regions: Vec<u32>,
        #[serde(default)]
        stalks: Vec<crate::daemon::sheaf_ops::SheafStalkInput>,
    },
    SheafDefect,
    SheafStalks,
    SheafStatus,
    SheafLearnedWeights,
    SheafReap,
    /// Wire-compat handshake. Takes no args; returns the daemon's
    /// version + wire-format identity (bead ley-line-open-cb8960).
    LeylineVersion,
    /// Tree-sitter syntactic validation (ley-line-open-fa8638).
    /// Mirrors mache's `writeback/validate.go` over the UDS so mache can
    /// drop its CGO tree-sitter link. Read-only (NOT in STATE_CHANGING_OPS).
    #[cfg(feature = "validate")]
    Validate(ValidateRequest),
}

#[cfg(feature = "vec")]
fn default_vec_k() -> u32 {
    10
}

#[cfg(feature = "text-search")]
fn default_text_search_k() -> u32 {
    10
}

// ---------------------------------------------------------------------------
// Typed args for the LSP family of ops. The LSP ops emit their row
// payloads via hand-built JSON (see `lsp_rows_response` in ops.rs)
// because the row shape is method-specific (hover content vs symbol
// metadata vs diagnostic ranges) and the daemon.capnp schema doesn't
// model these row variants. The REQUEST side is uniform enough to type:
// position-based ops (hover, defs, refs) take (file, line, col); file-
// level ops (symbols, diagnostics) take just (file).
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
pub struct LspPosition {
    pub file: String,
    pub line: u32,
    #[serde(default)]
    pub col: u32,
}

#[derive(Deserialize, Debug)]
pub struct LspFile {
    pub file: String,
}

// ---------------------------------------------------------------------------
// Validate request. Either `language` (one of the extension keys
// validate.rs recognizes: "go" | "py" | "js" | "ts" | "tsx" | "rs" |
// "ex" | "exs") or `path` (the daemon extracts the extension) is
// required. `content` is UTF-8 source text; callers passing binary
// content should base64-encode then base64-decode upstream — the wire
// uses JSON strings, not raw bytes.
// ---------------------------------------------------------------------------

#[cfg(feature = "validate")]
#[derive(Deserialize, Debug)]
pub struct ValidateRequest {
    /// UTF-8 source text to validate.
    pub content: String,
    /// Language extension key (e.g. "go", "py"). Mutually exclusive with `path`
    /// in practice; if both are supplied, `language` wins.
    #[serde(default)]
    pub language: Option<String>,
    /// Path the daemon infers the language from via the file extension.
    /// Used when the caller has a path but not an explicit language id.
    #[serde(default)]
    pub path: Option<String>,
}

// ---------------------------------------------------------------------------
// Intermediate data types used inside ops.rs while assembling capnp
// List populations. These are NOT serde-serialized — the response wire
// shape comes from capnp_json::to_json on the typed builder, not from
// these structs. They survive so handler code stays readable
// ("collect rows into Vec<WireRef>, then loop and set each capnp slot")
// without polluting the public ops surface with capnp builder lifetimes.
// ---------------------------------------------------------------------------

/// One reference: node_id (where the reference lives) + source_id (the
/// file that contains it). Used by find_callers / find_defs /
/// find_callees handlers as a Vec collected from SQL rows before the
/// capnp List(Ref) population step.
#[derive(Debug, Clone)]
pub struct Ref {
    pub node_id: String,
    pub source_id: String,
}

/// Bulk token-map entry — one token plus the list of nodes it
/// references (or defines). Used by op_get_token_map's intermediate
/// Vec before the capnp List(TokenMapEntry) population.
#[derive(Debug, Clone)]
pub struct TokenMapEntry {
    pub token: String,
    pub node_ids: Vec<String>,
}
