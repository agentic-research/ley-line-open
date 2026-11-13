# ley-line-open

Ley-line-open (LLO) keeps data as immutable, content-addressed values: each
version of a tree of files under a directory Merkle root (`storeRoot`), an
image's root filesystem named by its rootfs digest, signed evidence by its
BLAKE3 digest. It serves projections of those values:

- **structured facts**: a tree is parsed into a queryable SQLite projection,
  with tree-sitter and language servers for the files that have a grammar,
  which is what [mache](https://github.com/agentic-research/mache) consumes;
- **a filesystem**: a store mounted (NFS or FUSE, in a build with the `mount`
  feature), or shared into a microVM as its workspace, where a write to an
  authored path is a proposal merged back into the store;
- **a run**: a root filesystem executed under a signed grant in a libkrun
  microVM or a native `nono` sandbox, which is what
  [cloister](https://github.com/agentic-research/cloister) drives.

A signed head attests which inputs a projection was computed from (`storeRoot`
and `parserId`); a run receipt attests what ran, from which inputs, under which
confinement. LLO supplies the mechanism; cloister owns the policy that decides what a run
may do ([ADR-0035](docs/adr/0035-confinement-manifest-and-attested-enforcement.md)).

Pick your path:

- **Through mache.** mache installs or starts `leyline` and runs the parse for
  you; see [GETTING-STARTED.md](GETTING-STARTED.md).
- **Through cloister.** cloister issues `RunGrant`s and drives the execution
  daemon, or links `leyline-runtime` in-process. The operating guide is
  [rs/ll-open/runtime/README.md](rs/ll-open/runtime/README.md#running-the-execution-daemon);
  the wire contract is
  [execution/v1](rs/ll-core/schema-spec/execution/v1/README.md).
- **Directly.** Parse a tree, run the daemon, and query it:
  [GETTING-STARTED.md § Use it directly](GETTING-STARTED.md#use-it-directly).
- **Developing on LLO.** [GETTING-STARTED.md § Working on LLO](GETTING-STARTED.md#working-on-llo),
  [AGENTS.md](AGENTS.md) for the working contract, and
  [docs/ROADMAP.md](docs/ROADMAP.md) for where it is going and in what order.

The implementation-level contract is
[docs/TECHNICAL_OVERVIEW.md](docs/TECHNICAL_OVERVIEW.md); vocabulary, ownership
and the authority model are in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## The mental model

```text
   mache, agents                          cloister (policy, issuer)
        │ UDS line-delimited JSON, MCP          │ execution/v1 over UDS
        ▼                                       ▼
  leyline daemon: code facts          leyline execution-daemon: confined runs
  tree-sitter parse + LSP             RunSpec + signed RunGrant verified
  SQL projection (projection-v13)     confinement/v1 digest checked before start
  query, read_source, propose         libkrun microVM │ native nono
        │ proposals                        │ microVM       │ rootfs by digest
        │                                  │ workspace     ▼
        ▼                                  │ share     rootfs CAS (--cas-root):
  ┌─ store (one per daemon) ────────────┐  │ within    trees named by their
  │ source blobs by blob hash (BLAKE3)  │◄─┘ writeScope rootfs digest
  │ _tree → storeRoot                   │
  │ head: storeRoot + parserId + …,     │   evidence store (--evidence-store):
  │   signed as head digest v3          │   envelopes by blake3-256 digest
  │ SQLite arena snapshots by           │
  │   current_root · CDC index          │
  └─────────────────────────────────────┘
```

**Store and identity.** Source bytes are blobs named by their BLAKE3 blob hash.
`_tree` lists every path with its git mode and blob hash, and `Head.storeRoot`
is the parser-free directory Merkle root over it. `Head.parserId` names the
derivation the projection was computed under, beside the store root and never
folded into it; a signed head binds both (head digest v3), and
`leyline verify-head` recomputes them. The head digest also binds the
generation, the Cap'n Proto segment root and the previous head. A write is a proposal: checked against
the base its author read and merged three-way into the store
([ADR-0042](docs/adr/0042-proposals.md),
[ADR-0045](docs/adr/0045-git-shaped-store-and-merged-proposals.md)); one that
cannot land is refused with a typed reason (`stale`, or a conflict), which a
mount or workspace share reports as `ESTALE`. The daemon publishes SQLite arena snapshots, each
named by its SQLite arena snapshot root (`current_root`, BLAKE3 over the
serialized byte image). CDC is an explicit, derived chunk index over the
whole-file `source_blobs`.

**Code facts (for mache).** `leyline parse` and the daemon project the
tree-sitter parse into the SQL projection ABI (`nodes`, `_ast`, `_source`,
`_tree`, `_parse_diagnostics` and the interning tables, at `projection-v13`).
Language servers enrich it (`lsp_*` operations). The daemon serves
line-delimited JSON on a Unix socket and, when enabled, MCP over HTTP; its
surface includes a read-only `query` that runs one `SELECT`, definition and
caller lookups, `read_source`, `propose`, and the optional HDC, vector and text
search.

**Confined execution (for cloister).** `leyline execution-daemon` serves the
`execution/v1` lifecycle from `leyline-runtime`. It hosts its own substrate
daemon, so a workspace share and that daemon's socket propose through one
writer; give it the source tree rather than pointing a second daemon at the
same store. A `RunSpec` names the work, a
`RunGrant` signed by the issuer authorizes it, and a `RunReceipt` records what
ran, bound to the spec, grant, confinement digest and input roots. Two backends
exist: an embedded libkrun microVM (the worker links LLO's vendored libkrun,
upstream v1.19.6 plus LLO's patch series, `rs/third_party/libkrun-patches/`)
and a native `nono` sandbox (Seatbelt on macOS, Landlock on Linux). Each run
applies one `confinement/v1` manifest whose digest the grant commits to, and
the worker compares it with the grant before the workload starts. On the
microVM tier a run's workspace is a share served from the store
([ADR-0044](docs/adr/0044-userspace-data-plane.md)); a writing grant names a
`writeScope`, and the share refuses a write outside it. The native tier refuses
a grant that names a workspace. Root filesystems come from OCI images:
`leyline rootfs import-oci` reads an OCI image layout or pulls a digest-pinned
image from a registry, verifies every blob, and writes rootfs manifest v2 into
a CAS root, which can be a case-sensitive volume (`leyline rootfs
provision-cas`). The microVM guest receives argv and environment byte for byte
from an exec config file, and a run the guest init refuses, or a guest that
records no exit status, ends `Failed` with exit status 125.
`cloister/connections/v1` (declared connections, ADR-0046 Slice 1) is
specified, and its `RunGrant` and `RunReceipt` fields are on the wire; the
broker that serves them is planned for 0.31.0, and until then a grant carrying
connections is refused at start.

**Deployment configuration.** The `config.leyline/v1` specification (one
versioned document per daemon kind,
[ADR-0049](docs/adr/0049-deployment-configuration.md)) and its library resolver
shipped in 0.30.0. The daemons adopt it in 0.31.0, which also removes their
runtime `LEYLINE_*`/`LLO_*` environment configuration; until then they are
configured by flags and those variables.

**Platforms.** macOS on Apple silicon is first-class: the release ships the
microVM worker for it, ad-hoc signed with the Hypervisor.framework entitlement,
and an in-guest end-to-end run is a manual gate on every release. Linux
binaries ship for x86_64 and aarch64, and the native backend confines with
Landlock there. Linux/KVM microVM tests are an open v1 criterion
(`ley-line-open-5c2040`); until they run, Linux keeps TSI hijack off and
nothing claims Linux microVM evidence. v1 is one daemon owning one store on one
machine, the same on macOS and Linux ([docs/ROADMAP.md](docs/ROADMAP.md)).

## What is stable

Each contract has its own version, independent of the release version.

- **SQL projection ABI.** `nodes`, `_ast`, `_source`, `_tree` and related
  SQLite tables are a contract about which tables, columns and indexes a
  consumer may rely on, versioned as `projection-v13`
  ([docs/TABLE_CONTRACT.md](docs/TABLE_CONTRACT.md)). `nodes.record` is read
  directly across runtimes; mache reads that column out of the projection with
  no cgo. The generalized arena↔consumer Cap'n Proto handoff is tracked by
  `ley-line-open-50be73`. The SQL projection ABI is a contract, not a content identity: it has
  no root hash and will not be given one.
- **Cap'n Proto.** The schemas under `rs/ll-core/schema-capnp/schemas/` are the
  typed interchange contract, and the Cap'n Proto segment root (`Head.rootHash`)
  names the segments one parse run wrote. Go bindings live in
  [`clients/go/leyline-schema`](clients/go/leyline-schema/); the daemon's typed
  JSON fixtures test the Rust↔Go response surface.
- **execution/v1** (`cloister/execution/v1`), **confinement/v1** and
  **connections/v1** live under
  [`rs/ll-core/schema-spec/`](rs/ll-core/schema-spec/README.md) with vectors
  and, where the spec says so, a Python reference. A consumer negotiates
  execution/v1 compatibility (`llo_execution_capabilities` lists what a daemon
  supports) rather than inferring it from the release version. connections/v1 is specified, with no broker yet (above).
- **config.leyline/v1**
  ([spec](rs/ll-core/schema-spec/deployment-config/v1/README.md)): the
  specification, vectors and library resolver have shipped; the daemons do not
  read it yet.
- **Identities.** [AGENTS.md](AGENTS.md#identity-and-authority-vocabulary)
  lists every identity LLO ships with its construction, and which are proposed
  only. The SQLite arena snapshot root (`current_root`) is authoritative for
  byte integrity only and says nothing about logical content. The store root
  names the source tree independently of the parser; the pair
  `(storeRoot, parserId)` is the parser-qualified identity, bound by head
  digest v3 and `_meta.derived_from`. A blob hash names one payload
  and is a root for nothing. The rootfs digest is BLAKE3 over a
  `rootfs.manifest.json`'s exact bytes; an OCI image's SHA-256 digests are
  provenance and never compared with it.
- **CDC.** `content_chunks` and `blob_manifest` are private derived indexes.
  `source_blobs` remains authoritative, no consumer reads the chunk tables, and
  changes to them do not bump `leyline-schema`. Activate and collect CDC with
  `leyline cdc`.

Which identity is authoritative for what is settled by
[ADR-0032](docs/adr/0032-declared-decompositions.md) §D4 and applied in
[docs/ARCHITECTURE.md § Authority model](docs/ARCHITECTURE.md#authority-model).
Those two are the arbiters; this list is a summary of them.

## Install and build

The common path, with mache:

```bash
cd /path/to/mache && task install
cd /path/to/ley-line-open && task install
mache serve
```

Building and testing:

```bash
task install              # released/default feature set
task install:full         # portable full feature set, no mount backend
task install:full+mount   # add NFS and FUSE mounts (FUSE links libfuse)
task ci                   # check, clippy, formatting, tests, FFI gates, cargo-deny audit
task runtime:test         # runtime, krun-worker and agent-in-guest tests (fake worker; real guests when LEYLINE_LIBKRUNFW_PATH is set)
task krun-worker:install  # build, sign and install the microVM worker (needs zig on macOS)
```

The execution daemon's flags, evidence verifiers, trust keys, image import,
the CAS volume, the microVM worker and mutation testing are documented in
[rs/ll-open/runtime/README.md § Running the execution daemon](rs/ll-open/runtime/README.md#running-the-execution-daemon).

The current release is `v0.30.0`. It publishes platform binaries, the macOS
microVM worker, FFI staticlibs, and the Apache-2.0 Go schema module at
`clients/go/leyline-schema/v0.30.0`. See
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
curl -fsSLO https://github.com/agentic-research/ley-line-open/releases/download/v0.30.0/capnpc-schema-bridge-zod-darwin-arm64
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
`localhost/leyline:v0.30.0`. It uses
`cargo zigbuild` for the static musl binary
and `cgr.dev/chainguard/static:latest` as the runtime base.

Pushing a `v*` tag publishes the multi-arch image (linux/amd64 + linux/arm64)
to `ghcr.io/agentic-research/ley-line-open:v0.30.0`, with a
[build-provenance attestation](https://docs.github.com/actions/security-guides/using-artifact-attestations)
recorded against the pushed digest:

```bash
docker pull ghcr.io/agentic-research/ley-line-open:v0.30.0
gh attestation verify oci://ghcr.io/agentic-research/ley-line-open:v0.30.0 \
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

Store, identity and contracts (`rs/ll-core`):

- `core` — BLAKE3 content addressing, the `BlobStore` contract, arena layout
  and the head digest.
- `schema` — the SQL projection ABI's table definitions and locator types.
- `schema-capnp`, `public-schema` — Cap'n Proto schemas and their generated
  Rust bindings.
- `schema-spec` — execution/v1, confinement/v1, connections/v1,
  config.leyline/v1 and the other vendor-neutral specs, with vectors.
- `mcp-descriptor`, `mcp-protocol-schema`, `ffi-helpers` — `server.json`
  generation, MCP wire facts, C-boundary helpers.

Code facts (`rs/ll-open`):

- `ts` — tree-sitter parsing and the AST projection.
- `lsp` — language-server enrichment.
- `fs` — the SQLite graph, arena reader, CDC range reads, GC and FFI.
- `cdc` — content-defined chunking.
- `cli-lib`, `cli` — the daemon (snapshot loop, store, proposals, UDS/MCP
  dispatch) and the `leyline` binary.
- `cache`, `hdc`, `sheaf`, `text-search`, `chat-embed` — derived-view cache and
  the optional analysis and search facilities.
- `vcs` — a jj sidecar that versions arena snapshots.

Confined execution (`rs/ll-open`):

- `runtime` — execution/v1 lifecycle, authorization and evidence verification,
  confinement, OCI import, the CAS volume, and the native and libkrun backends.
- `krun-worker` — the microVM worker binary; the only crate that links the
  vendored libkrun (`rs/third_party/libkrun`).
- `storefs` — the store-backed filesystem behind the virtio-fs workspace share
  and the host NFS/FUSE mount.
- `sign`, `envelope` — Ed25519 signing, the canonical key id, DSSE and in-toto
  envelopes.
- `cas-ffi` — wasm-callable C FFI for the CAS primitives.

Other:

- `rs/ll-open/schema-bridge` — the capnp code generators above.
- `rs/ll-open/perf-sample` — test-only timing support.
- `clients/go/leyline-schema` — generated Go contract bindings.
- `docs/ARCHITECTURE.md` — normative vocabulary, ownership, and the authority
  model (which identity domain is authoritative for what).
- `docs/TABLE_CONTRACT.md` — the SQL projection ABI, table by table.
- `docs/TECHNICAL_OVERVIEW.md` — detailed concepts and glossary.
- `docs/ROADMAP.md` — the plan to v1 and what each release shipped.

## License

The schema and wire-contract crates are Apache-2.0. The implementation crates
are AGPL-3.0-or-later. See [LICENSE-APACHE](LICENSE-APACHE) and
[LICENSE](LICENSE). Each crate's `Cargo.toml` declares which side it is on, and
`task lint:license-direction` (part of `task ci`) fails when an Apache-2.0
crate gains a normal or build dependency, direct or transitive, on AGPL code.
