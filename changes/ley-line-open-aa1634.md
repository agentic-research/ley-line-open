Section: Fixed

- **Comments are `_ast` occurrences** (bead `ley-line-open-aa1634`).
  `leyline parse` skipped every tree-sitter `extra` node, so `_ast` held no
  comment and doc comments were unavailable to consumers. Every comment is
  now an `_ast` occurrence and a leaf `nodes` row with its grammar kind
  (`comment`; `line_comment`/`block_comment` in Rust and Java) and span, in
  source order under the node tree-sitter attached it to. Its `content_id`
  names a leaf `node_content` row whose `token` is the comment text.
  Comments stay out of every other node's `node_hash`, so a comment-only edit
  changes no code `node_hash`, and they add no def, ref or import rows. Other
  extras (Python's `line_continuation`) still have no row. A doc comment is
  the comment run directly above a definition; `docs/TABLE_CONTRACT.md` gives
  the query. `EXTRACTION_EPOCH` moves 6 → 7, which moves `parserId` and makes
  an existing arena re-derive on the next parse. Code occurrences keep their
  `nid`s; comments take the ordinals after them, so a comment edit moves no
  code `nid`, while `nodes.ord` counts comment siblings in source order and
  injected ids sit past the comments. `projection-v12` and `merkle-ast-v3` are
  unchanged. `Head.rootHash` moves for any file with a comment, because the
  comment's `AstNode` record joins the `.ast.capnp` segment it folds.
