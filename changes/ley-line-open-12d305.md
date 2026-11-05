Section: Fixed

- **Checkout no longer materialises a tampered `source_blobs` row, and every
  `source_blobs` read verifies through one accessor** (bead
  `ley-line-open-12d305`). `checkout`'s materialiser selected `blob_bytes`
  directly and wrote whatever a row held under a blob hash into the working
  directory; a row changed under its key is now refused, the file is left as
  it was, and the reconcile reports the path and the blob hash. The four
  remaining raw readers (`tree::blob_bytes`, checkout, `splice::source_bytes`
  and CDC activation) now read through `leyline_core::source_blob` (new
  `sqlite` feature), which re-derives the key with `Ref<Blob>::verified` and
  refuses a mismatch with a typed `SourceBlobError` naming the blob and, where
  the caller knows it, the path. `task lint:source-blob-reads` (in `task ci`)
  fails on any production `SELECT` of `source_blobs.blob_bytes` outside that
  module.
