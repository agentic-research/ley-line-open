# ley-line-open

Ley-line-open (LLO) is a continuously updated code-index database and query
server. It turns source code into structured facts that agents and tools can
query without repeatedly reparsing a repository.

Most people use it through [mache](https://github.com/agentic-research/mache):
mache installs or starts `leyline` for you. Start with
[GETTING-STARTED.md](GETTING-STARTED.md). This README is the short orientation;
the implementation-level contract is in
[docs/TECHNICAL_OVERVIEW.md](docs/TECHNICAL_OVERVIEW.md).

## The useful mental model

```text
source files
    │ tree-sitter + language servers
    ▼
structured code facts
    ├── SQLite projection — queried in-process and by same-machine consumers
    ├── Cap'n Proto records for typed cross-runtime interchange
    └── BLAKE3 roots for snapshot integrity and parse history
             │
             ▼
       leyline daemon ── UDS / MCP HTTP ── mache, agents, other consumers
```

The normal path is:

```bash
cd /path/to/mache && task install
cd /path/to/ley-line-open && task install
mache serve
```

You generally do not need to run `leyline parse` yourself. For direct use or
development, see [GETTING-STARTED.md](GETTING-STARTED.md).

## What is stable

- `nodes`, `_ast`, `_source`, and related SQLite tables are the **SQL projection
  ABI**: a stable contract about which tables, columns, and indexes you may rely
  on. `nodes.record` is read directly across runtimes — mache reads that column
  out of the projection with no cgo. The SQL projection ABI is a contract, not a
  content identity: it has no root hash and will not be given one.
- Cap'n Proto schemas under `rs/ll-core/schema-capnp/schemas/` are the typed
  interchange contract. Go bindings live in
  [`clients/go/leyline-schema`](clients/go/leyline-schema/).
- The daemon serves line-delimited JSON over its Unix socket and MCP HTTP when
  enabled. Typed fixtures test the Rust↔Go response surface.
- Arenas publish consistent SQLite snapshots, each named by a BLAKE3 hash of its
  serialized byte image (`current_root`). That hash is authoritative for byte
  integrity only — it says nothing about logical content. The generalized
  arena↔consumer Cap'n Proto handoff is tracked by `ley-line-open-50be73`.
- CDC is an explicit, derived read optimization. `nodes.record` remains
  authoritative; activate and collect CDC manifests with `leyline cdc`.
- The `content_chunks`, `content_manifest`, and `content_manifest_meta` tables
  are private derived indexes. They never replace the authoritative record, no
  consumer reads them, and changes to them do not bump `leyline-schema`.

Which identity is authoritative for what is settled by
[ADR-0032](docs/adr/0032-declared-decompositions.md) §D4 and applied in
[docs/ARCHITECTURE.md § Authority model](docs/ARCHITECTURE.md#authority-model).
Those two are the arbiters; this list is a summary of them.

## Install and build

```bash
task install              # released/default feature set
task install:full         # portable full feature set, no mount backend
task install:full+mount   # add FUSE/NFS support
task ci                   # check, clippy, formatting, tests, FFI gates
```

The current release is `v0.13.0`. It publishes platform binaries, FFI
staticlibs, and the Apache-2.0 Go schema module at
`clients/go/leyline-schema/v0.13.0`. See
[releases/latest](https://github.com/agentic-research/ley-line-open/releases/latest)
for assets and [GETTING-STARTED.md](GETTING-STARTED.md) for download commands.

## Generator binaries

LLO ships code generators that downstream repos **run**. Every release publishes
them per platform, so consuming one is a download rather than a Rust build:

| binary | what it emits |
|---|---|
| `capnpc-schema-bridge-zod` | capnp → zod TypeScript validators |
| `capnpc-schema-bridge-go` | capnp → Go types |
| `capnpc-schema-bridge-jsonschema` | capnp → JSON Schema |
| `capnpc-schema-bridge-tooldefs` | capnp → MCP `tools/list` definitions |
| `leyline-mcp-descriptor` | descriptor JSON → `server.json`, with coverage validation |

```bash
# capnp resolves `-o<plugin>` by PATH-searching `capnpc-<plugin>`, so the
# schema-bridge assets must be installed under their unsuffixed names.
curl -fsSLO https://github.com/agentic-research/ley-line-open/releases/download/v0.13.0/capnpc-schema-bridge-zod-darwin-arm64
install -m 0755 capnpc-schema-bridge-zod-darwin-arm64 ~/.local/bin/capnpc-schema-bridge-zod
```

Verify against the release's `SHA256SUMS` before installing.

The four `capnpc-*` binaries are capnp plugins. `leyline-mcp-descriptor` is a
plain filter — descriptor JSON on stdin, `server.json` on stdout, exit 1 with
**empty stdout** on failure, so `leyline-mcp-descriptor < in.json > server.json`
cannot truncate a good artifact when validation fails.

Prior releases published none of these, so consumers had to SHA-pin a git
dependency and build a build-tool from source — which meant a fix could be
*tagged* without being *obtainable* (`ley-line-open-e44960`).

## OCI image

`task image` builds a local distroless OCI image tagged
`localhost/leyline:v0.13.0` (equivalently `ley-line-open:v0.13.0`). It uses
krust/cargo-zigbuild for the static binary
and `cgr.dev/chainguard/static:latest` as the runtime base.

Pushing a `v*` tag publishes the multi-arch image (linux/amd64 + linux/arm64)
to `ghcr.io/agentic-research/ley-line-open:v0.13.0`, with a
[build-provenance attestation](https://docs.github.com/actions/security-guides/using-artifact-attestations)
recorded against the pushed digest:

```bash
docker pull ghcr.io/agentic-research/ley-line-open:v0.13.0
gh attestation verify oci://ghcr.io/agentic-research/ley-line-open:v0.13.0 \
  --repo agentic-research/ley-line-open
```

The image tag is **`v`-prefixed**, matching `server.json`'s
`packages[0].version`. That is not cosmetic: cloister derives the image as
`<identifier>:<version>` from `server.json`, so the published tag and that
field must be the same string or the address resolves to nothing. `task
image:verify-published` asserts exactly that, reading the address out of the
committed `server.json` rather than reconstructing it.

```bash
task image
task image:smoke
```

The image exposes MCP on container port 8384. Publish it to host loopback (for
example `-p 127.0.0.1:18384:8384`) unless an authenticated proxy is in front.

## Repository map

- `rs/ll-core` — arena, hashes, schemas, and shared infrastructure.
- `rs/ll-open/ts` — tree-sitter parsing and AST projections.
- `rs/ll-open/lsp` — language-server enrichment.
- `rs/ll-open/fs` — SQLite graph, arena reader, CDC, FFI, and mount adapters.
- `rs/ll-open/cli-lib` — daemon lifecycle and UDS/MCP dispatch.
- `clients/go/leyline-schema` — generated Go contract bindings.
- `docs/ARCHITECTURE.md` — normative vocabulary, ownership, and the authority
  model (which identity domain is authoritative for what).
- `docs/TABLE_CONTRACT.md` — the SQL projection ABI, table by table.
- `docs/TECHNICAL_OVERVIEW.md` — detailed concepts and glossary.

## License

The schema and wire-contract crates are Apache-2.0. The implementation crates
are AGPL-3.0-or-later. See [LICENSE-APACHE](LICENSE-APACHE) and
[LICENSE](LICENSE).
