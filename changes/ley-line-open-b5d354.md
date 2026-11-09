Section: Breaking

- **`node_content` drops `lang` and `raw_kind`: projection-v12 (ADR-0047 M2,
  D-iii, bead `ley-line-open-b5d354`).** Neither is in `node_hash`'s
  preimage, so one row served every file and language holding the same
  subtree, kept whichever occurrence landed first, and kept a replaced file's
  value after a write (a Go and a Rust file sharing the `"` leaf left
  `lang = 'go'` after the Go string was deleted). Every `node_content` column
  is now a function of `node_hash`. Read a node's language and grammar kind
  per occurrence through `_ast.kind_id → kinds(lang, raw_kind)`;
  `docs/TABLE_CONTRACT.md` gives the query for a fielded child's grammar kind
  (an anonymous child's is its `kind`, since the merkle-ast-v3 fold applies
  κ to named nodes only).
  Injected-language subtrees have no occurrence rows, so their language and
  grammar kind are no longer in the projection (`ley-line-open-cc5c55`).
  `leyline rebuild`'s logical-row gate compares every `node_content` column;
  the exclusion it carried for these two is gone. `PROJECTION_SCHEMA_VERSION`
  is `projection-v12`; a v11 arena is refused at parse open.
