Section: Fixed

- **The daemon's reader pool has sqlite-vec's modules** (bead
  `ley-line-open-3bf5dc`). With the `vec` feature the daemon registered
  sqlite-vec (a process-wide `sqlite3_auto_extension`, which reaches only
  connections opened afterwards) while building its vector index, after the
  reader pool had opened every connection. A `query` naming `vec_each` or
  another sqlite-vec module failed with "no such table". The daemon now
  registers it before the living database and its readers open.
