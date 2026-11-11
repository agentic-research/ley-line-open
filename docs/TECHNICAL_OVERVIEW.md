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
   arenas, the arena snapshot root (`current_root`), and reader-safe
   publication.
2. **Projection** parses and enriches source files, then materializes tables
   such as `nodes`, `_ast`, and `_source`.
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

The writer constructs a complete SQLite snapshot, writes it into the arena's
inactive buffer and flips the active index (or, when the snapshot outgrows the
arena, writes a larger arena file beside it and renames it into place), and
publishes the snapshot's BLAKE3 `current_root` under the control block's
seqlock. The substrate API exposes no generation number; `current_root` is the
arena's identity. Readers validate the
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

- **Cap'n Proto segment root** (`Head.rootHash`): identifies the segments one
  parse run wrote.
- **Store root** (`Head.storeRoot`): the parser-free directory Merkle tree over
  the store's files (path, git mode, blob hash) and directories.
- **Parser identity** (`Head.parserId`): the derivation the projection was
  computed under, bound beside the store root, never folded into it.
- **SQLite arena root** (`current_root`): identifies one complete published
  database snapshot.
- **Node hash** (`node_hash`): identifies one deduplicated syntax subtree,
  independent of where it occurs.
- **Blob/chunk hash**: identifies an individual payload or CDC chunk.
- **Head digest** (head digest v3): the signed claim that binds the generation,
  the segment root and the previous one, the store root, the parser identity
  and the previous claim.
- **Rootfs digest**: identifies one execution rootfs, as BLAKE3 over the exact
  bytes of its `rootfs.manifest.json`. An OCI image's SHA-256 digests are
  provenance and are never compared with it.
- **Run id** (`run_id`): identifies one authorized run from its `RunSpec`
  digest, grant id and replay key.
- **SQL projection ABI**: identifies the table/column compatibility promise; it
  is not a hash.

Arena-local integers such as `nid` and `content_id` are locators, not
identities: they name a row only inside one arena. `AGENTS.md` lists every
identity with its construction.

Content-addressed storage (CAS) uses deterministic bytes and a BLAKE3 digest as
an identity. A signed head (head digest v3) binds the segment root, store root
and parser identity to the previous head's digest; it does not bind the arena
snapshot root. Integrity answers “did these bytes change?”; signatures and the
identity layer answer “who is allowed to publish or consume them?”

CDC is an optimization at the BlobStore boundary for deduplication and partial
reads. It does not replace the arena snapshot contract or change SQLite page
semantics: SQLite still reads pages from the verified serialized database
slice.

## Runtime boundaries

The daemon serves local clients over a Unix-domain socket (UDS) and exposes
optional MCP endpoints over HTTP and UDS, which serve only the advertised tools
inside the Cloister groups the listener grants (`--mcp-groups`). Its `query` op
runs one read-only `SELECT` and refuses anything else with a typed refusal. The Go `daemon/wire` package is the typed JSON
consumer boundary. Event fields must follow the live wire contract (including
numeric `seq` values); compatibility handling belongs at that boundary rather
than in every caller.

Execution is a separate, opt-in boundary. `leyline execution-daemon` serves
the `execution/v1` lifecycle from `leyline-runtime`: a signed grant authorizes
a run (`--allow-unverified-evidence`, for local fixtures, checks no signature), a resolver picks a rootfs by digest from a content-addressed store, and
a first-party backend (a native `nono` sandbox or a libkrun microVM) runs it
under a `confinement/v1` manifest whose digest the grant commits to. The
ordinary `leyline daemon` has no execution backend. `leyline rootfs
import-oci` fills the rootfs store from a digest-pinned OCI image.

Identity responsibilities are layered: LLO can produce local, unsigned
observations; Cloister defines portable observation envelopes and ledger
receipts; notme supplies stable principals and capabilities; canonical-hours
projects observations; Rosary composes work and lifecycle records.

## Optional facilities

HDC, sheaf analysis, vector/text search, the FUSE/NFS host mount, and
CDC-backed distribution are
additional projections or transport optimizations. They are not prerequisites
for parsing, SQLite querying, snapshot publication, or daemon operation.

## Glossary

| Term | Meaning |
| --- | --- |
| Arena | File/buffer containing a published SQLite snapshot |
| Arena flip | Atomic switch of the arena's active buffer (or rename of a grown arena into place), published as a new `current_root` |
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
