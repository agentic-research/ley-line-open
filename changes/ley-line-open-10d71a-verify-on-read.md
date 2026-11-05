Section: Fixed

- **`source_blobs` reads verify their bytes against their address** (bead
  `ley-line-open-10d71a`). `tree::blob_bytes`, the reader behind `read_source`,
  the store filesystem and `leyline rebuild`, returned whatever bytes a row
  held under a blob hash. A row changed in place is now refused (the store
  filesystem answers `EIO`) instead of being served as the file the hash names:
  Σ (CA) as verify-on-read, the contract `BlobStore::get` already states.
