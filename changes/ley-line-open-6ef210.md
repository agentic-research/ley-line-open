Section: Security

- **The bundled SQLite is 3.54.1, past CVE-2026-119375
  (RUSTSEC-2026-0459)** (bead `ley-line-open-6ef210`). rusqlite moves from
  0.39 to 0.41 and `libsqlite3-sys` from 0.37 (SQLite 3.51.3) to 0.39.1, and
  r2d2_sqlite from 0.34 to 0.36 with it. SQLite before 3.54.0 can return
  uninitialized page-cache memory when it opens a database whose `-wal` file
  has a different page size. witchcraft (the `engine-witchcraft` feature of
  `leyline-text-search`) still requires rusqlite 0.39; a semver-trick crate,
  `rs/shims/rusqlite-0.39`, serves that requirement from rusqlite 0.41 through
  `[patch.crates-io]`, so the workspace links one SQLite.
  `sqlite_version_floor_test` fails if a later dependency change brings back
  a SQLite older than 3.54.0. The daemon's read-only `query` op allows the two
  JSON functions SQLite 3.54 adds, `json_array_insert` and
  `jsonb_array_insert`, which compute a value from their arguments as
  `json_insert` does; every other action is still denied by default. In the
  Rust API, `snapshot_to_arena`, `activate_cdc_and_snapshot` and
  `SqliteGraph::serialize` take `&mut` because rusqlite 0.41 serializes a
  connection only through an exclusive borrow.
