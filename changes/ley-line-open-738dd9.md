Section: Breaking

- **projection-v10: the occurrence tables point at their subtree by
  `content_id`** (bead `ley-line-open-738dd9`). `_ast.node_hash`,
  `node_refs.node_hash` and `node_defs.node_hash` (a 32-byte BLOB on every
  occurrence) become `content_id INTEGER REFERENCES node_content(content_id)`,
  and `idx_ast_node_hash` becomes `idx_ast_content` on `_ast(content_id)`.
  `node_content.node_hash` stays the identity, stored once per distinct
  subtree; `content_id` stays arena-local and never leaves the database. The
  read-only views `v_ast_hash`, `v_node_refs_hash` and `v_node_defs_hash` carry
  the v9 column sets, `node_hash` included, so a v9 read runs unchanged against
  the view; on the right of a `LEFT JOIN`, join `node_content` on `content_id`
  instead (TABLE_CONTRACT gives the rewrites). `_cfg` keeps its `node_hash`
  key. On this repository's `rs/` tree a cold parse goes from 321.19 MiB to
  243.44 MiB (26.2x to 19.9x source): `_ast` 76.75 to 39.88, its hash index
  55.06 to 18.25, `node_refs` 11.13 to 7.81, `node_defs` 2.81 to 2.06. The
  `_ast`, `node_content`, edge, def, ref and def-to-ref join sets are
  unchanged by hash (each `EXCEPT` of the v9 and v10 arenas is empty both
  ways), and `node_hash` and `Head.rootHash` are byte-identical. `treeRoot`
  moves only because `parserId` folds the projection label, as at every bump:
  the v10 arena's `_tree` recomputed under the v9 `parserId` gives the v9
  `treeRoot`. A `projection-v9` arena is refused at parse open.
