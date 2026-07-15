# leyline-net/v1 test vectors

Pinned conformance vectors for the leyline-net generic wire frames
(bead `ley-line-open-083344`). 12 values × 2 byte-forms:

- `fixtures.capnp` — the value definitions (capnp consts), a
  value-for-value mirror of cloister's `wire/cross-check-fixtures.capnp`.
- `reference/<name>.bin` — reference-encoder bytes (`capnp eval -b` /
  plain `write_message`); byte-equal to cloister's committed
  `test/wire/fixtures/canonical.ts`.
- `canonical/<name>.bin` — strict canonical form (`set_root_canonical`).
- `digests.json` — BLAKE3 + SHA-256 + size for every vector, both forms.
- `VECTORS.sha256` — sha256sum manifest over every load-bearing file
  here (vectors, digests.json, fixtures.capnp; README prose is
  errata-editable and deliberately unpinned).

Binary carrier, not JSON: unlike credential-isolation/confinement
(JSON-as-carrier), these vectors ARE the wire bytes — the thing being
specified is a capnp encoding, so the fixture format is the encoding.

Regenerate (deliberate spec change only):

```
cargo run -p leyline-schema-capnp --example gen_leyline_net_vectors -- \
    rs/ll-core/schema-spec/leyline-net/v1/test-vectors
```

Drift gates: `rs/ll-core/schema-capnp/tests/leyline_net_vectors.rs`
(BLAKE3 pins + byte-equality + decode + capnp-eval cross-checks) and
`rs/ll-core/schema-spec/tests/verify_vectors_sha256.rs` (SHA-256 pins).
A vector byte change without a version bump in `../README.md` is spec
breakage by definition.
