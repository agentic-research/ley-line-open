# leyline-schema-spec

Vendor-neutral capability specifications for the ley-line substrate. Each
capability lives under `<capability>/v<n>/` and defines a wire-shape contract
that any second implementation can conform to.

## What's here

- **`_traits.capnp`** — canonical Cap'n Proto trait annotations
  (`$Sensitive`, `$Op(...)`, `$SinceVersion(...)`, …) used across every
  capability spec's schema. Downstream emitters (zod today, Rust later)
  read these annotations and honor them at codegen time.
- **`_capability-mapping.md`** — the three-lane identifier scheme
  (signet URN / WIMSE URI / cloister interface name) that keys every
  capability to its concrete implementation.
- **`_traits.md`** — human-readable documentation of the trait
  annotations declared in `_traits.capnp`.
- **`LAYOUT.md`** — the on-disk layout contract every capability spec
  MUST follow.
- **`credential-isolation/v1/`** — credential-proxy wire protocol
  (envelope shape, receipt commitment, error responses, injection
  strategies). Ships a Python reference implementation under
  `ref-impl-py/` and 10 conformance vectors under `test-vectors/`.
- **`build-cache/v1/`** — OCI-shaped remote build cache transport
  (manifest shape, digest encoding, push/pull protocols). Ships a
  5-file vector bundle under `vectors/`.
- **`mcp-tool/v1/`** — MCP tool meta-groups wire contract with two
  example vectors.

## Conformance vectors and `VECTORS.sha256`

Two spec dirs pin their vector bundles with a `VECTORS.sha256` file:

- `credential-isolation/v1/VECTORS.sha256` — 10 vectors under `test-vectors/`
- `build-cache/v1/vectors/VECTORS.sha256` — 5 vectors alongside it

The crate's `verify_vectors_sha256` test walks both files and re-hashes
every listed vector, asserting SHA-256 equality. Any drift between the
pinned digests and the committed bytes fails `cargo test -p
leyline-schema-spec`.

## History

Content moved verbatim from `cloister/cloister-spec/` (see bead
`ley-line-open-729a7e` and ADR-0029). Byte-identity was verified with
`cmp -s` at move time. The cloister-side follow-up (deleting cloister's
copy + depending on this crate) is tracked separately; until it lands,
both trees co-exist byte-for-byte.
