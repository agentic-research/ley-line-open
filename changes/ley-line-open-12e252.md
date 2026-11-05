Section: Added

- **Arena-local locators are their own types** (bead `ley-line-open-12e252`,
  ADR-0047 step M0). `leyline-schema` defines `Nid`, `FileId`, `DirId`,
  `NameId`, `KindId` and `ContentId`, each a `repr(transparent)` newtype over
  `i64` with SQL impls (`ToSql`/`FromSql`) and nothing else: no `serde` or
  Cap'n Proto impl, and no conversion between locators, from an integer or
  from a content address except the explicit `from_raw`/`get`. The schema
  API (`file_nid`, `dir_nid`, `ensure_file_id`, `intern_name`, `intern_kind`,
  `insert_node`, `node_path`, `resolve_path`, `file_nid_range`, …) and the
  projection writers in `leyline-ts`, `leyline-lsp`, `leyline-fs` and
  `leyline-cli-lib` take and return them, so passing a nid where a `file_id`
  belongs, swapping a `name_id` and a `kind_id`, or serializing a nid onto a
  wire is a compile error (compile-fail doc tests in
  `leyline_schema::locator`). Bead `80f037` F3 refuses a bare nid on the wire
  at runtime; this makes it a type error. Nothing moves: the stored integers,
  the nid encoding and every column are unchanged.
- **`leyline_derive_key` in `leyline-cas-ffi`**, BLAKE3
  `derive_key(context, input)` over the C ABI and in the wasm build, with a C
  header (`rs/ll-open/cas-ffi/include/leyline_cas_ffi.h`). It is the
  prerequisite of run-id v2 (ADR-0047 D-i, M3): cloister derives `run_id`
  through this FFI. It delegates to the new `leyline_core::substrate::derive_key`
  and is pinned by `deriveKey` cases in
  `schema-spec/sigma/v1/test-vectors/constructions.json`, taken from BLAKE3's
  own test vectors and checked from Rust, through the C ABI and against the
  wasm export surface. Additive: `leyline_hash_bytes` and every Σ address are
  unchanged.
