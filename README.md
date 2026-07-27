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
    ├── SQLite projections for local queries
    ├── Cap'n Proto records for cross-runtime interchange
    └── BLAKE3 identities for verification and snapshot history
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

- `nodes`, `_ast`, `_source`, and related SQLite tables are local query
  projections, not the cross-runtime wire contract.
- Cap'n Proto schemas under `rs/ll-core/schema-capnp/schemas/` are the typed
  interchange contract. Go bindings live in
  [`clients/go/leyline-schema`](clients/go/leyline-schema/).
- The daemon serves line-delimited JSON over its Unix socket and MCP HTTP when
  enabled. Typed fixtures test the Rust↔Go response surface.
- Arenas publish consistent SQLite snapshots. The generalized arena↔consumer
  Cap'n Proto handoff is tracked by `ley-line-open-50be73`.
- CDC is an explicit, derived read optimization. `nodes.record` remains
  authoritative; activate and collect CDC manifests with `leyline cdc`.

## Install and build

```bash
task install              # released/default feature set
task install:full         # portable full feature set, no mount backend
task install:full+mount   # add FUSE/NFS support
task ci                   # check, clippy, formatting, tests, FFI gates
```

The current release is `v0.10.4`. It publishes platform binaries, FFI
staticlibs, and the Apache-2.0 Go schema module at
`clients/go/leyline-schema/v0.10.4`. See
[releases/latest](https://github.com/agentic-research/ley-line-open/releases/latest)
for assets and [GETTING-STARTED.md](GETTING-STARTED.md) for download commands.

## OCI image

`task image` builds a local distroless OCI image tagged
`localhost/leyline:0.10.4` (equivalently `ley-line-open:0.10.4`). It uses
krust/cargo-zigbuild for the static binary
and `cgr.dev/chainguard/static:latest` as the runtime base. The image is not
automatically pushed to GHCR; `ghcr.io/agentic-research/ley-line-open:0.10.4`
is a registry reference only when an operator has published that image.

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
- `docs/ARCHITECTURE.md` — normative ownership and invariants.
- `docs/TABLE_CONTRACT.md` — SQL projection contract.
- `docs/TECHNICAL_OVERVIEW.md` — detailed concepts and glossary.

## License

The schema and wire-contract crates are Apache-2.0. The implementation crates
are AGPL-3.0-or-later. See [LICENSE-APACHE](LICENSE-APACHE) and
[LICENSE](LICENSE).
