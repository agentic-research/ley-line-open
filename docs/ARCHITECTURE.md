# Architecture — ley-line-open

Canonical architecture overview for ley-line-open (LLO). Companion to the [root README](../README.md) (which frames the project) and the [rs/ workspace README](../rs/README.md) (which maps the crate layout). This doc records the **architectural layers + their contracts**, the **runtime model**, the **consumer-facing surfaces**, and the **load-bearing ADRs** each layer pins.

---

## Status

| Field | Value |
|---|---|
| LLO version | v0.5.0 |
| Last verified | 2026-06-25 |
| Source of truth files | `rs/ll-core/`, `rs/ll-open/`, `rs/ll-open/cli-lib/src/daemon/`, `docs/adr/*.md`, `CHANGELOG.md` |

This file is canonical for the architectural shape. Per-crate detail lives in each crate's `README.md`. Per-decision detail lives in `docs/adr/`. Per-table contract lives in [`TABLE_CONTRACT.md`](TABLE_CONTRACT.md).

---

## The three layers

LLO is structured into three architectural layers:

```
┌──────────────────────────────────────────────────────────────────┐
│  CONSUMER SURFACE (UDS + MCP HTTP)                               │
│  daemon ops · capnp wire · MCP JSON-RPC                          │
└──────────────────────────────────────────────────────────────────┘
                            ▲
┌──────────────────────────────────────────────────────────────────┐
│  PROJECTION ENGINE  (ll-open/)                                   │
│  parse · LSP ingest · enrichment passes (HDC, vec, sheaf)        │
│  filesystem presentation (FUSE/NFS) · sign · vcs · text-search   │
└──────────────────────────────────────────────────────────────────┘
                            ▲
┌──────────────────────────────────────────────────────────────────┐
│  INFRASTRUCTURE  (ll-core/)                                      │
│  arena (mmap'd Σ Merkle-CAS) · BLAKE3 content-addressing         │
│  SQLite schema (nodes + sidecars) · capnp wire schemas           │
└──────────────────────────────────────────────────────────────────┘
```

### Layer 1: Infrastructure (`rs/ll-core/`)

The Σ substrate — content-addressed storage primitives every other crate is built on.

| Crate | Purpose | Key types |
|---|---|---|
| `leyline-core` | Arena primitives. mmap'd files + control block + generation counter for hot-reload | `ArenaHeader`, `Controller`, `ContentAddressed` |
| `leyline-schema` | Shared SQLite schema for the `nodes` table + indexes | `create_schema`, `insert_node` |
| `leyline-public-schema` | Capnp wire schema for the daemon UDS + MCP transport. Source of truth for every base op's request/response shape with `$Json.name` annotations for camel↔snake | `capnp/daemon.capnp` |
| `leyline-schema-capnp` | Capnp schemas for the Σ event log (T8, decade `ley-line-open-9d30ac`) | Generated Rust bindings |

**Contract:** Σ substrate is **BLAKE3-locked**. Every content-addressed primitive uses BLAKE3 (`leyline-core::substrate::ContentAddressed for [u8]`). SHA-256 is deliberately not a substrate commitment — it appears only at the OCI ecosystem boundary.

### Layer 2: Projection engine (`rs/ll-open/`)

Where source becomes structure. Parses, enriches, signs, presents.

| Crate | Purpose | Key entry points |
|---|---|---|
| `leyline-ts` | Tree-sitter AST projection + bidirectional splice | `parse`, `splice` |
| `leyline-lsp` | LSP client for ingesting language-server analysis into SQLite | `LspClient`, `document_symbols`, `hover` |
| `leyline-hdc` | Hyperdimensional computing — D=8192 hypervectors via bundle composition + seeded leaves; popcount-Hamming distance (ADR-0024) | `EncoderNode`, `encode_fresh`, `Hypervector` |
| `leyline-sheaf` | Čech-cohomology engine; structural cache + δ⁰-driven invalidation (ADR-0020) | `CellComplex`, `SheafCache` |
| `leyline-fs` | Filesystem presentation — mounts arena as FUSE or NFS | `SqliteGraph`, `SqliteGraphAdapter` |
| `leyline-vcs` | jj sidecar — automatic versioning of arena snapshots | `VersionedGraph`, `.leyline/` virtual dir |
| `leyline-sign` | CMS signing primitives + gpgsm-compatible binary for jj commit signing (verifier-only; host code lives in cloister per ADR-0019) | Certificate, Signature |
| `leyline-cas-ffi` | Wasm32-callable FFI for BLAKE3-substrate hash. Consumed by cloister via workerd's cdylib loader | `leyline_hash_bytes` |
| `leyline-text-search` | Unstructured-text semantic search backend abstraction. `NullEngine` default; `WitchcraftEngine` (XTR-WARP) behind feature flag | `TextSearchEngine` trait |
| `leyline-chat-embed` | CLI binary: semantic search over Claude Code chat databases (mache's `claude-chats` ingest) via fastembed/MiniLM | `chat-embed` binary |
| `leyline-cli-lib` | The daemon. Owns the living db + UDS control socket + MCP HTTP transport; hosts all enrichment passes | `cmd_daemon`, `daemon::ops` |
| `leyline-cli` | The `leyline` binary. Thin wrapper around `leyline-cli-lib` | `parse`, `daemon`, `serve`, `inspect` |

**Contract:** Every enrichment pass writes into a sidecar (`_lsp*`, `_hdc*`, etc.) — the `nodes` + `_ast` + `_source` core tables are the canonical substrate; sidecars are derived. Text-search engines are sidecar by construction (storage path lives OUTSIDE the arena; no capnp segments emitted; re-indexing never advances `current_root`).

### Layer 3: Consumer surface

Two wire transports, one tool surface.

| Transport | Path | Used by |
|---|---|---|
| **UDS** | `~/.mache/<arena>.ctrl.sock` (default) — local-process IPC | mache (Go), other same-machine consumers |
| **MCP HTTP** | `:8384` default (`--mcp-port`) — JSON-RPC tool surface for agents (token-gated per ADR-0022) | Claude Code (MCP plugin), cloister (proxy via Cloudflare Access) |

Both transports dispatch to the same op registry — see `rs/ll-open/cli-lib/src/daemon/` for the dispatcher. Adding an op = capnp variant + Rust arm + entry in `is_known_base_op`. The op surface is ~23 base ops grouped by purpose (lifecycle, navigation, graph queries, introspection, LSP, bulk SQL, embedding search).

---

## The Σ substrate — runtime model

LLO's daemon runs a **living SQLite database in memory** with an **arena snapshot loop**:

1. **Parse phase**: `leyline-ts` walks tree-sitter ASTs into the in-memory db.
2. **Enrichment phase**: each registered pass (LSP → `_lsp*` tables; HDC → `_hdc*` tables; etc.) writes sidecar rows.
3. **Snapshot**: serialize current db state → arena buffer → BLAKE3-hash → advance `current_root` on the controller's generation counter.
4. **Readers**: mmap the arena via `SqliteGraph` (zero-copy via `sqlite3_deserialize`); detect generation change → hot-swap to new buffer.

The arena is double-buffered: a writer flip advances the controller; readers see atomic transitions via the generation counter. Multiple readers share a lock-free pool (`SqliteGraphAdapter`), 2-8 readers auto-sized.

```
                  ┌─────────┐
                  │ writer  │
                  └────┬────┘
                       ▼ writes
        ┌──────────────────────────────┐
        │ in-memory living db (SQLite) │
        └──────────────┬───────────────┘
                       ▼ snapshot
        ┌────────────────────────────────┐
        │  arena (mmap'd, double-buffered)│
        │  ┌──────────────┬──────────────┐│
        │  │ buffer A     │ buffer B     ││
        │  └──────────────┴──────────────┘│
        │  controller: current_root, gen  │
        └─────┬──────────────┬────────────┘
              ▼              ▼
        ┌──────────┐    ┌──────────┐
        │ reader 1 │    │ reader N │  (SqliteGraph pool)
        └──────────┘    └──────────┘
```

---

## Cross-runtime consumers

The substrate is consumed across language runtimes:

- **mache (Go)** — primary consumer. Reads the arena via pure-Go capnp deserialization (`clients/go/leyline-schema`) + opens the SQLite projection directly. **No cgo.** Exposes its own MCP surface for code-intel tools.
- **cloister (TS / workerd)** — agent execution + network topology layer. Consumes leyline-cas-ffi via `cloister-cas.wasm` for substrate-aligned hashing. Calls LLO's MCP over HTTP through Cloudflare Access (per ADR-0022's Mode B).
- **Control-room (Swift, future)** — consumes the same FFI surface as cloister via the C ABI.

Naming rule (cross-repo design beads `cloister-5e4402` / `ley-line-open-5e05e6`): **anything named `leyline-*` lives in LLO**. Cloister hosts `cloister-*` bridge crates that depend on LLO primitives — never forks or symlink-plus-extensions.

---

## Load-bearing ADRs

Architectural decisions that shape the substrate today:

| ADR | Subject | Status |
|---|---|---|
| [ADR-0014](adr/0014-capnp-as-protocol.md) | Capnp as the wire protocol | Accepted |
| [ADR-0015](adr/0015-lazy-on-access-ingestion.md) | Lazy-on-access ingestion | Accepted |
| [ADR-0016](adr/0016-ai-native-query-surface.md) | AI-native query surface | Accepted |
| [ADR-0020](adr/0020-entity-observation-lattice.md) | Entity-observation lattice (sheaf-driven) | Accepted |
| [ADR-0021](adr/0021-cache-lockfile-schema.md) | Cache lockfile schema | Accepted |
| [ADR-0022](adr/0022-mcp-wire-auth-shared-secret.md) | MCP wire auth: shared-secret token (local); cloister-proxied (remote) | Accepted |
| [ADR-0023](adr/0023-agent-first-language-facts.md) | Agent-first language facts (analyzer-as-library, not LSP-wire) | Proposed |
| [ADR-0024](adr/0024-hdc-substrate-identity-rewrite.md) | HDC substrate-identity rewrite (bundle composition, seeded leaves, fp-quantize) | Accepted (shipped v0.5.0) |
| [ADR-0025](adr/0025-hdc-compositional-validation.md) | HDC compositional-vs-distance use modes (validate or remove) | Proposed |

ADRs 0017-0019 are cloister-side and live in `~/remotes/art/cloister/docs/adr/`.

---

## Build + release

| Surface | Built via |
|---|---|
| `leyline` binary | `task build` (debug, headless) / `task release` (release, headless) / `task release:mount` (release with FUSE) |
| Distroless OCI image | `task image` — produces `ley-line-open:0.5.0` (~20 MB) via krust + cargo-zigbuild static musl; image default CMD is `daemon --mcp-port 8384 --mcp-bind 0.0.0.0` |
| FFI staticlibs + header | `task release:fs-static` / `task release:cas-ffi` — published as GitHub release artifacts (linux + darwin × amd64+arm64; macOS amd64 staticlib currently absent) |
| Go schema client | `clients/go/leyline-schema` — nested Go module, tag-published as `clients/go/leyline-schema/v<version>` alongside the root release tag |

Release flow is on-tag-push: `task readme:version-check` gates README version-pin drift in CI (mirroring the `compat:check` + `gen:server-json:check` pattern).

---

## What this doc does NOT cover

- **Per-crate API detail.** Lives in each crate's `README.md`.
- **Per-table schema.** Lives in [`TABLE_CONTRACT.md`](TABLE_CONTRACT.md).
- **Cloister, mache, control-room internals.** Lives in those repos.
- **Decade-level / strategic problem-statement docs.** Live in [`docs/decades/`](decades/) and [`docs/problems/`](problems/).
- **Research / red-team output.** Lives in [`docs/research/`](research/) and [`docs/audits/`](audits/).

This doc is the structural skeleton; the per-area detail is one click away.
