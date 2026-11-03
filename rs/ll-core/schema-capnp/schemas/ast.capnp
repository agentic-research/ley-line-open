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
  # The slot is kept because capnp ordinals are never reused: removing @0
  # would renumber the struct and break decoding of every segment record
  # already written. The SQL projection's `_ast.nid` is the queryable
  # locator. Keeping the locator OUT of the hashed preimage is what lets
  # the projection re-key (integer nids, Phase B) without moving the
  # segment root.
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

# `AstNodeList` (the ADR-0026 per-file pointer-store blob) was removed at
# projection-v8 with the `capnp_blobs` table it filled (ADR-0040 D5, bead
# ley-line-open-f30fdf). No segment ever carried it.
