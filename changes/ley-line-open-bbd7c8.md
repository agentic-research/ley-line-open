Section: Fixed

- **A `_tree` directory row that carries a `blob_hash` is refused** (bead
  `ley-line-open-bbd7c8`). `tree::decode` read such a row as a directory
  and silently ignored the hash. It is now refused, as a file row without
  a `blob_hash` already was, with the typed `tree::MalformedRow`
  (`DirectoryWithBlob` / `FileWithoutBlob`, naming the path) by
  `tree::decode`, which every `_tree` reader now goes through, the storefs
  filesystem's lookup and directory listing included (they read rows
  directly and served a directory with a blob as a directory). So
  `storeRoot`, `leyline verify-head`, `leyline rebuild` and the storefs
  share fail closed (EIO for the share) on either row.
