Section: Fixed

- **Comments that contradicted the code now state what it does, and
  `task tier:isolation` builds every ll-core crate** (bead
  `ley-line-open-b42256`). The run-id σ-axiom test names M3 as
  `ley-line-open-ab2093`; the `leyline-cas-ffi` module doc lists
  `leyline_derive_key`'s empty or non-UTF-8 context refusal; the
  `verify_vectors_sha256` doc stops counting two pin files (there are six)
  and a new test fails when a `VECTORS.sha256` file is missing from its
  list; the daemon auth doc names the socket `<ctrl>.sock` beside the
  control file. `tier:isolation` built three of the eight ll-core crates
  (not `leyline-schema-capnp`); it now builds all eight and fails when their
  dependency tree reaches an ll-open crate.
