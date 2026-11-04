Section: Fixed

- **A range read of the empty blob returns 0 bytes** (bead
  `ley-line-open-bc7df6`). `leyline_fs::blob_chunked::read_blob_range`
  refused BLAKE3("") with "no chunk manifest" although the empty blob tiles as
  zero spans and `has_blob_chunked` already reads it as complete, so an empty
  file in a `source_blobs`-activated arena could not be read through the CDC
  target. It now reads back as 0 bytes for any range; any other blob without
  a manifest is still refused. Empty files are now pinned end to end: propose
  (create, replace to and from empty, delete and recreate), `read_source`
  (`content: ""`, present on the wire), the cold parse and reparse in Go, Rust,
  Python and TypeScript, CDC activation and GC, checkout materialize, ingest
  and conflict, the three-way merge with an empty side, `treeRoot` (an empty
  file and an empty directory at one path give different roots), and the NFS
  host mount (read, `st_size` 0, truncate to empty, a zero-length write, an
  editor's save of an empty buffer).
