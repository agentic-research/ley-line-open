Section: Breaking

- **projection-v9: the merkle-AST edges are interned** (bead
  `ley-line-open-9b7d28`). `node_content` gains `content_id INTEGER PRIMARY
  KEY` (arena-local; not an identity) and keeps `node_hash` as `NOT NULL
  UNIQUE`. The edges move to `node_edge (parent_id, ordinal, child_id,
  field)`, `WITHOUT ROWID`, keyed on content ids instead of two 32-byte
  hashes per edge. `node_child` is now a read-only VIEW with the v8
  columns, so v8 reads run unchanged; on the right side of a `LEFT JOIN`,
  read `node_edge` instead (TABLE_CONTRACT gives the SQL). On bead
  `ley-line-open-17c271`'s kibana slice the edges go from 515.2 MB
  (`node_child` + its autoindex) to 72.9 MB, the arena from 1,471.1 MB to
  1,030.1 MB (42.5x to 28.4x source), and peak parse RSS from 4.58 GB to
  4.09 GB; the edge set is unchanged by hash (4,346,013 rows, an `EXCEPT`
  of the two arenas is empty both ways). On this repository's `rs/` tree:
  168.4 MB to 23.7 MB. No identity moves: `node_hash` and `Head.rootHash`
  are byte-identical. A `projection-v8` arena is refused at parse open.
