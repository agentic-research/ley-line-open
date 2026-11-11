Section: Added

- **`cloister/connections/v1`, the connection broker's input document** (bead
  `ley-line-open-e9d940`, ADR-0046 Slice 1 S2). New spec directory
  `rs/ll-core/schema-spec/connections/v1/`: README, JSON Schema, canonical form,
  `connectionsDigest` (BLAKE3-256 over the canonical bytes) pinned for a one- and a
  three-connection document, 11 accepted cases and 69 refusals, each refusal pinned by
  connection, field and reason code. `leyline_schema_spec::connections` is the
  parser and canonicalizer the runtime will share. It is the crate's first Rust module
  and has no dependencies. An independent Python reference (standard library only,
  with its own reader and BLAKE3) must reach every pinned digest and refusal under
  `cargo test -p leyline-schema-spec`. Slice 1 accepts only `art.tcp` from a UNIX
  endpoint to a loopback IP literal. Credentials, TLS, names and every other D2 member
  are refused by name. Nothing reads the document yet: authorization and the broker
  arrive with `ley-line-open-521deb` and `ley-line-open-52b5b3`.
