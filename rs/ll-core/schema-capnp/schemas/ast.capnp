@0x9e1e4e1af2b578d9;
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
  # Stable per-parse-run node identifier — path-shaped, e.g.
  # `pkg/auth.go/function_declaration/block/statement_list/...`.
  # Matches `_ast.node_id` in the SQL projection.
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
