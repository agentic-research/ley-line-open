Section: Added

- **`status` reports the daemon's `--source` directory as `source_root`**
  (bead `ley-line-open-246526`). A `propose`'s and a `read_source`'s paths
  are relative to that tree, so a client attached to a daemon it did not
  start reads the root from `status` instead of from the arena's owner
  sentinel. The value is canonical (symlinks resolved, the spelling the
  owner sentinel records); the field is absent when the daemon runs without
  `--source`. Additive: Cap'n Proto `StatusResponse.sourceRoot @11`, and the
  Go `daemon/wire` `StatusResponse.SourceRoot`. A JSON decoder that rejects
  unknown fields must allow `source_root`.
