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
//!   lines into one of the 23 base ops. Lives here rather than going
//!   through capnp-json on the request side because the dispatch enum
//!   is cleaner as a serde-driven tagged union (`#[serde(tag = "op",
//!   rename_all = "snake_case")]`) than as a capnp union — the request
//!   shape is small, the args are heterogeneous per op, and serde's
//!   `from_value` already handles missing-field and unknown-op errors
//!   with structured messages.
//! - `LspPosition` / `LspFile`: small typed-args structs the LSP ops
//!   consume.
//! - `Ref` / `TokenMapEntry`: intermediate data types used inside
//!   `ops.rs` while building capnp List populations. Not serde-
//!   serialized today (no `to_wire` callers); kept as plain data
//!   structs.
//!
//! Bead: ley-line-open-b0ea2e (the wire.rs codegen / capnp-json
//! adoption thread).

use serde::Deserialize;

// ---------------------------------------------------------------------------
// Request enum — typed dispatch surface for socket.rs.
//
// Each variant corresponds to one op in `is_known_base_op`. Args land in
// the variant's named fields with serde's `tag = "op"` rename pulling
// the `op` JSON field as the discriminator. `#[serde(default)]` on
// optional fields keeps the request small for ops that don't use them.
//
// ops::handle_base_op_value tries to deserialize the already-parsed
// Value as `BaseRequest`. On success → typed dispatch into
// `dispatch_typed`. On failure → returns an ErrorResponse on the wire.
// Adding a new op means:
//   1. New variant here.
//   2. Match arm in `dispatch_typed`.
//   3. New name in `is_known_base_op`.
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
}

#[cfg(feature = "vec")]
fn default_vec_k() -> u32 {
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
