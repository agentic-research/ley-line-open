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
task runtime:test        # embedded libkrun runtime contract tests (fake worker)
```

### Embedded execution runtime

The experimental `leyline-runtime` crate owns the execution/v1 lifecycle and
the first-party `libkrun` and native-`nono` worker backends. Both verify a
content-addressed rootfs and copy it into a private per-run userspace volume;
the native backend applies `nono` before launching the guest-relative
executable, while the libkrun backend enters a microVM. Neither worker invokes
`krunvm`, Taskfile, or repository scripts. The immutable CAS source remains
outside the guest's writable boundary. Embedders select a backend explicitly
when constructing `run_execution_daemon`; the ordinary daemon remains backend-
free for compatibility.

The portable runtime tests use a fake worker and do not require libkrun or
Hypervisor.framework. The real Apple Silicon guest-write proof is ignored by
default because it requires a locally installed libkrun/libkrunfw toolchain and
the Hypervisor.framework entitlement; see
[`libkrun_guest_write.rs`](rs/ll-open/runtime/tests/libkrun_guest_write.rs) and
bead `ley-line-open-16a994` for the CI/release gate.

The execution/v1 API is exposed through the runtime crate, the explicit
`run_execution_daemon` UDS entry point, first-party CLI client, and MCP
registry. Its schema version
(`cloister/execution/v1`) is deliberately independent of the repository and
crate release version; consumers must negotiate API compatibility rather than
infer it from `v0.14.0`. The ordinary open `leyline daemon` command remains a
substrate daemon with no execution backend; an embedding application must opt
into `run_execution_daemon` after constructing a trusted resolver/backend
handler. The service currently proves schema binding,
provisioning, ordered lifecycle events, cancellation, cleanup, and
content-addressed receipt assembly. Native backend conformance and mmap-backed
CAS projection remain separate follow-on gates in beads
`ley-line-open-f81567` and `ley-line-open-16c953`.

For embedders that keep an explicit artifact catalog, `CatalogResolver` binds
the authorized executable artifact and workspace graph identities to a
content-addressed rootfs digest and guest-relative entrypoint. It rejects
unknown or duplicate identities, workspace drift, path traversal, and output
limits that the backend cannot enforce; it never accepts host paths from the
execution wire request.

The open binary can host the native surface directly when those resources are
provisioned explicitly:

```bash
leyline execution-daemon \
  --cas-root /var/lib/leyline/cas \
  --run-root /var/lib/leyline/runs \
  --worker /usr/libexec/leyline-native-worker \
  --catalog /etc/leyline/execution-catalog.json
```

The catalog is local trusted configuration; it is not a substitute for the
signed `RunSpec`/`RunGrant` wire contract. Signet/NotMe/Interlace trust roots
remain embedding-owned: production callers must pass an `EvidenceVerifier` to
`start_authorized_with_verifier`, which resolves each content-addressed
evidence reference and verifies its signed envelope or certificate chain. The
default first-party CLI rejects execution unless an embedding verifier is
installed. `--allow-unverified-evidence` exists only for local fixtures and is
an explicit downgrade. The legacy `start_authorized` path uses a metadata-only
fixture verifier and is not a production trust boundary. The daemon owns the
worker and UDS lifecycle, while callers provide logical intent and the
embedding verifier.

LLO also ships `CasDsseEvidenceVerifier`, which verifies APAS
`application/vnd.in-toto+json` envelopes from an embedding-provided
content-addressed store against embedding-provided Signet/NotMe trust keys.
Cloister may use that adapter or supply its own verifier for Interlace
certificate/lease evidence; key distribution and rotation remain outside LLO.

To select the embedded VM path, use `--backend micro-vm --libkrun
/path/to/libkrun` and repeat `--device` only for explicitly granted device
paths. This selects LLO's `KrunWorkerBackend`; it does not invoke the
`krunvm` CLI.

Mutation testing remains a separate hardening pass: use the repository's
`task mutants:diff DIFF=<path>` gate after the runtime edge-case tests are
expanded. The initial local runtime mutation run exposed survivor cases that
are tracked in `ley-line-open-ce0cf0`; they are intentionally not presented as
covered by this vertical slice.

The current release is `v0.14.0`. It publishes platform binaries, FFI
staticlibs, and the Apache-2.0 Go schema module at
`clients/go/leyline-schema/v0.14.0`. See
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
curl -fsSLO https://github.com/agentic-research/ley-line-open/releases/download/v0.14.0/capnpc-schema-bridge-zod-darwin-arm64
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
`localhost/leyline:v0.14.0` (equivalently `ley-line-open:v0.14.0`). It uses
krust/cargo-zigbuild for the static binary
and `cgr.dev/chainguard/static:latest` as the runtime base.

Pushing a `v*` tag publishes the multi-arch image (linux/amd64 + linux/arm64)
to `ghcr.io/agentic-research/ley-line-open:v0.14.0`, with a
[build-provenance attestation](https://docs.github.com/actions/security-guides/using-artifact-attestations)
recorded against the pushed digest:

```bash
docker pull ghcr.io/agentic-research/ley-line-open:v0.14.0
gh attestation verify oci://ghcr.io/agentic-research/ley-line-open:v0.14.0 \
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
- `rs/ll-open/runtime` — execution/v1 lifecycle, private rootfs materialization,
  native nono and embedded libkrun worker backends, and confinement policy.
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
