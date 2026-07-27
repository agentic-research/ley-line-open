# Ley-line-open: technical overview

Ley-line-open (LLO) keeps a structured, queryable representation of a source
repository and serves it to local consumers. The normal path is:

```text
source files → tree-sitter + language servers → code facts
           → SQLite query projection + Cap'n Proto interchange records
           → verified snapshot → daemon (UDS/MCP)
```

The project has three layers:

1. **Storage and publication** maintain SQLite snapshots, content hashes,
   arenas, generations, and reader-safe publication.
2. **Projection** parses and enriches source files, then materializes tables
   such as `nodes`, `_ast`, `_source`, and `_file_index`.
3. **Consumer APIs** expose queries over the daemon and typed cross-language
   records. Mache is the primary Go consumer; other runtimes use the same
   durable contracts.

## SQLite and Cap'n Proto have different jobs

SQLite is the local materialized query index: it is optimized for filtering,
joins, indexes, and ad-hoc inspection. Cap'n Proto is the typed interchange
format: it gives Rust, Go, TypeScript, and future consumers a versioned schema
and deterministic record encoding. Neither representation's hash is silently
substituted for the other's.

The SQL table and column shape is a consumer contract documented in
`docs/TABLE_CONTRACT.md`; it is an ABI, not a content identity.

## Snapshot and arena publication

The writer constructs a complete SQLite snapshot, verifies its root, and
publishes it as an active arena buffer with a generation. Readers validate the
published metadata before opening the active slice and keep that mapping alive
for the lifetime of their connection. A reader must never observe a partially
written snapshot.

The arena-facing protocol is being generalized under bead
`ley-line-open-50be73`. That protocol will make the envelope, format/version,
segment descriptors, `current_root`, active-buffer consistency, capability
discovery, and reader handoff explicit. Until that contract is accepted, an
arena implementation detail is not a portable consumer API.

## Separate identities and verification boundaries

These identities are intentionally distinct:

- **Cap'n Proto segment/root**: identifies canonical portable records.
- **SQLite arena root**: identifies one complete published database snapshot.
- **Blob/chunk hash**: identifies an individual payload or CDC chunk.
- **SQL projection ABI**: identifies the table/column compatibility promise; it
  is not a hash.

Content-addressed storage (CAS) uses deterministic bytes and a BLAKE3 digest as
an identity. A Merkle-linked head can additionally bind a snapshot to its
predecessor. Integrity answers “did these bytes change?”; signatures and the
identity layer answer “who is allowed to publish or consume them?”

CDC is an optimization at the BlobStore boundary for deduplication and partial
reads. It does not replace the arena snapshot contract or change SQLite page
semantics: SQLite still reads pages from the verified serialized database
slice.

## Runtime boundaries

The daemon serves local clients over a Unix-domain socket (UDS) and exposes an
optional MCP HTTP endpoint. The Go `daemon/wire` package is the typed JSON
consumer boundary. Event fields must follow the live wire contract (including
numeric `seq` values); compatibility handling belongs at that boundary rather
than in every caller.

Identity responsibilities are layered: LLO can produce local, unsigned
observations; Cloister defines portable observation envelopes and ledger
receipts; notme supplies stable principals and capabilities; canonical-hours
projects observations; Rosary composes work and lifecycle records.

## Optional facilities

HDC, sheaf analysis, vector/text search, FUSE, and CDC-backed distribution are
additional projections or transport optimizations. They are not prerequisites
for parsing, SQLite querying, snapshot publication, or daemon operation.

## Glossary

| Term | Meaning |
| --- | --- |
| Arena | File/buffer containing a published SQLite snapshot |
| Arena flip | Atomic reader switch to a newer generation |
| Canonical bytes | Deterministic encoding used for hashing or interchange |
| CAS | Content-addressed storage keyed by a content hash |
| CDC | Content-defined chunking for deduplication and partial reads |
| FFI | Interface callable from another language/runtime |
| Merkle-linked | A root digest commits to related child state and history |
| Substrate | Foundational storage/identity layer; use the specific contract name when possible |
| UDS | Unix-domain socket for local IPC |
| MCP | Model Context Protocol transport for tools and resources |

For normative details, read `docs/ARCHITECTURE.md`, the table contract, and the
relevant ADRs. This page is the orientation layer, not a replacement for those
specifications.
