@0x9e1e4e1af2b578d9;

# Go codegen annotations (inert for capnpc-rust; consumed by capnpc-go).
using Go = import "/go.capnp";
$Go.package("ast");
$Go.import("github.com/agentic-research/ley-line-open/clients/go/leyline-schema/ast");

# AstNode — tree-sitter AST node projection.
#
# T8.3 (decade ley-line-open-9d30ac, thread T8/capnp-as-protocol).
# Producer: rs/ll-open/cli-lib/src/cmd_parse.rs::parse_into_conn.
# Local SQL projection: `_ast` table.
#
# Snapshot semantics: each parse run produces a fresh AstNode log
# (truncate-and-rewrite). The log captures the *current* AST for the
# repo at that parse generation. T8.5 hashes the log into the Σ root.

using Common = import "common.capnp";

struct AstNode {
  # RETIRED (written empty since Phase A of bead ley-line-open-17c271).
  # The slot is kept because persisted `capnp_blobs` pin capnp ordinals —
  # removing @0 would renumber the struct and break decoding of every
  # stored blob. A node's address inside an `AstNodeList` is its ordinal
  # (`_ast_pointer.offset_in_blob`); the SQL projection's `_ast.node_id`
  # remains the queryable locator. Keeping the locator OUT of the hashed
  # preimage is what lets the projection re-key (integer nids, Phase B)
  # without moving every `blob_hash`.
  nodeId @0 :Text;

  # The `_source.id` (relative path) the node belongs to.
  sourceId @1 :Text;

  # tree-sitter node kind (e.g. `function_declaration`,
  # `identifier`, `block`). The CONSTRUCT_KINDS list in
  # rs/ll-open/lsp/src/project.rs filters this set for find_callers
  # UX.
  nodeKind @2 :Text;

  # Byte and (line, column) position of the node in the source
  # file. Both are stored — bytes are canonical, lines/cols are
  # query-friendly.
  range @3 :Common.Range;
}

# AstNodeList — per-file aggregation of AstNode records.
#
# ADR-0026 Phase 1 (bead ley-line-open-3e87ad): the pointer-store blob unit
# is per-file. A single AstNodeList message contains every AstNode for one
# source file; the canonical bytes of that message are hashed with BLAKE3 to
# yield `capnp_blobs.blob_hash`, and each `_ast_pointer.offset_in_blob`
# indexes into the `nodes` list. The wrapper struct exists so the ADR's
# literal spec — `blob_hash = BLAKE3(canonical(List(AstNode)))` — has a
# concrete capnp root (canonical form requires a single-root message).
struct AstNodeList {
  nodes @0 :List(AstNode);
}
