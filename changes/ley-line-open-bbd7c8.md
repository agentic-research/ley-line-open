Section: Fixed

- **A `_tree` directory row that carries a `blob_hash` is refused** (bead
  `ley-line-open-bbd7c8`). `tree::decode` read such a row as a directory
  and silently ignored the hash. It is now refused, as a file row without
  a `blob_hash` already was, with the typed `tree::MalformedRow`
  (`DirectoryWithBlob` / `FileWithoutBlob`, naming the path) on every
  `_tree` read, so `storeRoot`, `leyline verify-head` and `leyline rebuild`
  fail closed on either row.
