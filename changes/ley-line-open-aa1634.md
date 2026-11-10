Section: Breaking

- **Comments are `_ast` occurrences; `projection-v13`** (bead
  `ley-line-open-aa1634`). `leyline parse` skipped every tree-sitter `extra`
  node, so `_ast` held no comment and doc comments were unavailable to
  consumers. Every comment is now an `_ast` occurrence and a leaf `nodes` row
  with its grammar kind (`comment`, `line_comment`, `block_comment`, ...;
  match `raw_kind LIKE '%comment%'`) and span, in source order under the
  node tree-sitter attached it to. Its `content_id` names a leaf
  `node_content` row whose `token` is the comment text. Comments stay out of
  every other node's `node_hash`, so a comment-only edit changes no code
  `node_hash`, and they add no def, ref or import rows. Other extras
  (Python's `line_continuation`) still have no row; Swift's member-separator
  `multiline_comment` and Scala's shebang `comment` are not extras and stay
  folded, as before. A doc comment is the comment run directly above a
  definition; `docs/TABLE_CONTRACT.md` gives the query. **Breaking:** no
  table or column moves, but two consumer invariants do: a comment's `_ast`
  row has no `node_edge` from its parent, and `nid` order is no longer
  document order, because a file's comments take the ordinals after its code
  (code `nid`s are unchanged, and a comment edit moves none of them;
  `nodes.ord` counts comment siblings in source order; injected ids sit past
  the comments). `PROJECTION_SCHEMA_VERSION` moves to `projection-v13`, and a
  `projection-v12` arena is refused at parse open: delete it and reparse
  cold. A consumer written against v12 must refuse v13. `EXTRACTION_EPOCH`
  moves 6 → 7. Both move `parserId`. `merkle-ast-v3` and `storeRoot` are
  unchanged; `Head.rootHash` moves for any file with a comment, because the
  comment's `AstNode` record joins the `.ast.capnp` segment it folds.
