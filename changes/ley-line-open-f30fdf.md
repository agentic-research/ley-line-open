Section: Breaking

- **projection-v8: the pointer store leaves the projection** (ADR-0040 D5,
  bead `ley-line-open-f30fdf`). `capnp_blobs` (one canonical `AstNodeList`
  blob per file) and `_ast_blob` (`file_id → blob_hash`) are no longer
  created or written. They were the second stored copy of the syntax tree
  and had no reader in LLO or mache. On the kibana slice bead
  `ley-line-open-17c271` pins (8,000 `.ts` files, 36.3 MB of source), a cold
  parse goes from 2,284.6 MB to 1,471.1 MB (66.0x to 42.5x source); peak
  RSS is unchanged (4.53 GB before, 4.58 GB after). A `projection-v7` arena
  is refused at parse open; rebuild it with a cold reparse. No identity
  moves: `Head.rootHash` folds the segment files, and the kibana parse
  writes byte-identical `.ast.capnp` and `.source.capnp` segments and the
  same `rootHash` before and after. `parserId` and `treeRoot` move once
  because `parserId` folds the projection label. Nothing produces the
  `AstNodeList` struct any more; it leaves the published schema at the next
  schema release (bead `ley-line-open-438772`). The dead
  `daemon::granularity_router` (no caller; it recommended reads from
  `capnp_blobs`) and `leyline_schema::FileKey::FileId` (whose only table was
  `_ast_blob`) are removed. ADR-0026 is amended.
