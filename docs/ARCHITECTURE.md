# Architecture — ley-line-open

Canonical architecture overview for ley-line-open (LLO). Companion to the [root README](../README.md) (which frames the project) and the [rs/ workspace README](../rs/README.md) (which maps the crate layout). This doc records the **architectural layers + their contracts**, the **runtime model**, the **consumer-facing surfaces**, and the **load-bearing ADRs** each layer pins.

---

## Status

| Field | Value |
|---|---|
| LLO version | v0.10.3 |
| Last verified | 2026-07-24 |
| Source of truth files | `rs/ll-core/`, `rs/ll-open/`, `rs/ll-open/cli-lib/src/daemon/`, `docs/adr/*.md`, `docs/decades/*.md`, `CHANGELOG.md` |

This file is canonical for the architectural shape. Per-crate detail lives in each crate's `README.md`. Per-decision detail lives in `docs/adr/`. Per-table contract lives in [`TABLE_CONTRACT.md`](TABLE_CONTRACT.md). Per-decade design lives in [`docs/decades/`](decades/).

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
| `leyline-core` | Arena primitives. mmap'd files + control block + generation counter for hot-reload. `ContentAddressed` trait = the σ substrate entry point (BLAKE3-locked per Σ §3.4) | `ArenaHeader`, `Controller`, `ContentAddressed`, `Hash` |
| `leyline-schema` | Shared SQLite schema for the `nodes` table + indexes | `create_schema`, `insert_node` |
| `leyline-public-schema` | Capnp wire schema for the daemon UDS + MCP transport. Source of truth for every base op's request/response shape with `$Json.name` annotations for camel↔snake | `capnp/daemon.capnp` |
| `leyline-schema-capnp` | Capnp schemas for the Σ event log (`AstNode`, `SourceFile`, `BindingRecord`, `Head`, `AstNodeList`). Decade `ley-line-open-9d30ac` | Generated Rust bindings |
| `leyline-schema-spec` | Vendor-neutral IDL crate. Ships per-capability specs (`credential-isolation/v1`, `confinement/v1`, `build-cache/v1`, `mcp-tool/v1`) with canonical test vectors + integrity/identity pins. Verified by `verify_vectors_sha256` (SHA-256 integrity) + `verify_confinement_digest` (BLAKE3-256 identity) + `capability_mapping_coverage` + `version_bump_on_vector_change` cargo tests | Non-code artifact — spec dirs + pin files |

**Contract:** Σ substrate is **BLAKE3-locked**. Every content-addressed primitive uses BLAKE3 (`leyline-core::substrate::ContentAddressed for [u8]`). SHA-256 appears in exactly two places: (a) the OCI ecosystem boundary, and (b) `VECTORS.sha256` content-integrity pins under `schema-spec/*/v*/` (distinct concern from BLAKE3-256 identity digests pinned in `CONFINEMENT_DIGESTS.blake3` — see bead `ley-line-open-193170`).

### Layer 2: Projection engine (`rs/ll-open/`)

Where source becomes structure. Parses, enriches, signs, presents.

| Crate | Purpose | Key entry points |
|---|---|---|
| `leyline-ts` | Tree-sitter AST projection + bidirectional splice | `parse`, `splice` |
| `leyline-lsp` | LSP client for ingesting language-server analysis into SQLite | `LspClient`, `document_symbols`, `hover` |
| `leyline-hdc` | Hyperdimensional computing — D=8192 hypervectors via bundle composition + seeded leaves; popcount-Hamming distance (ADR-0024) | `EncoderNode`, `encode_fresh`, `Hypervector` |
| `leyline-sheaf` | Čech-cohomology engine; structural cache + δ⁰-driven invalidation (ADR-0020) | `CellComplex`, `SheafCache` |
| `leyline-fs` | Filesystem presentation — mounts arena as FUSE or NFS; optional CDC-derived chunk manifests for bounded range reads | `SqliteGraph`, `SqliteGraphAdapter`, `activate_chunked_content` |
| `leyline-vcs` | jj sidecar — automatic versioning of arena snapshots | `VersionedGraph`, `.leyline/` virtual dir |
| `leyline-sign` | Ed25519 `RootSigner` that signs the at-rest Σ `Head` (S1) + `verify_head` verify-on-load (S2) + the canonical key id `kid = lowercasehex(SHA-256(SPKI)[:16])` (S3, signet ADR-012); plus CMS/gpgsm verify primitives for jj commit signing (interactive host signing stays cloister-side per ADR-0019) | `Ed25519RootSigner`, `verify_head`, `canonical_kid`, Certificate, Signature |
| `leyline-cas-ffi` | Wasm32-callable FFI for BLAKE3-substrate hash. Consumed by cloister via workerd's cdylib loader | `leyline_hash_bytes` |
| `leyline-text-search` | Unstructured-text semantic search backend abstraction. `NullEngine` default; `WitchcraftEngine` (XTR-WARP) behind feature flag | `TextSearchEngine` trait |
| `leyline-chat-embed` | CLI binary: semantic search over Claude Code chat databases (mache's `claude-chats` ingest) via fastembed/MiniLM | `chat-embed` binary |
| `leyline-cli-lib` | The daemon. Owns the living db + UDS control socket + MCP HTTP transport; hosts all enrichment passes | `cmd_daemon`, `daemon::ops` |
| `leyline-cli` | The `leyline` binary. Thin wrapper around `leyline-cli-lib` | `parse`, `daemon`, `serve`, `inspect` |

**Contract:** Every enrichment pass writes into a sidecar (`_lsp*`, `_hdc*`, `_cfg`/`_cfg_edge`, etc.) — the `nodes` + `_ast` + `_source` + `node_content` core tables are the canonical substrate; sidecars are derived. Text-search engines are sidecar by construction (storage path lives OUTSIDE the arena; no capnp segments emitted; re-indexing never advances `current_root`).

**In-flight (analysis-substrate decade, `docs/decades/analysis-substrate.md`):** The `_cfg` / `_dfg` / `_taint` fact tables — three projections of one differential-dataflow computation over the existing `_ast` / `node_content` / `node_defs` / `node_refs` EDB, driven by `daemon.sheaf.invalidate` as the epoch tracker. v0.7.2 shipped T1's schema + κ CFG-kind vocabulary + reflow-invariant CFG builder + F1_cfg_reflow_stable gate. T1.b3-followup (bead `a0fadd`) wires `_cfg` population into `cmd_parse`; T2 / T3 / T4 still open. `docs/decades/analysis-substrate.md` §4.1 names the sub-file staging layer as a decade-level open question to resolve before T3.b3 (bead `c25128`).

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
3. **Optional CDC activation**: `daemon --cdc` resumably builds private chunk
   manifests from authoritative readable `nodes.record` leaves before the
   first snapshot. Missing or stale manifests fall back to authoritative
   records; no schema-client or wire contract changes. Long-lived writable
   projections run explicit `leyline cdc gc` off the hot path; one IMMEDIATE
   transaction deletes only chunk rows unreachable from every manifest and
   reports rows and bytes before, unreachable, deleted, and remaining.
4. **Snapshot**: serialize current db state → arena buffer → BLAKE3-hash → advance `current_root` on the controller's generation counter.
5. **Readers**: mmap the arena via `SqliteGraph` (zero-copy via `sqlite3_deserialize`); detect generation change → hot-swap to new buffer.

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
| [ADR-0026](adr/0026-content-addressed-pointer-store.md) | Content-addressed pointer store (`_ast_pointer` + `capnp_blobs`; Phase 1 dual-write) | Accepted (Phase 1 shipped) |
| [ADR-0027](adr/0027-unified-code-fact-ir-producer.md) | Unified code-fact IR: merkle-AST `node_hash` (κ kind + terminal + child hashes) + `node_content` / `node_child` git-tree object | Accepted (shipped v0.6.0) |
| [ADR-0028](adr/0028-content-addressed-source-blobs.md) | Content-addressed source blobs (`source_blobs`; F-git compat with `git cat-file blob`) | Accepted (Phase 1 shipped) |
| [ADR-0029](adr/0029-cas-backed-workspace.md) | CAS-backed workspace (mount driver + manifest; alternative to git-worktree flow) | Proposed (Phase 1 mount driver bead `de3f81`) |

ADRs 0017-0019 are cloister-side and live in `~/remotes/art/cloister/docs/adr/`. Mache's ADR-0024 (`incremental-dataflow-taint-as-substrate-queries`) is a separate document in the mache repo whose producer-side lives in LLO's `dataflow-substrate` decade — see the decade doc for the mapping.

---

## Build + release

| Surface | Built via |
|---|---|
| `leyline` binary (default features) | `task build` (debug, headless) / `task release` (release, headless) / `task install` (release + codesign + install to `~/.local/bin`). Default features are `lsp` + `validate` + `hdc` + `cdc` — the structural-analysis core plus explicit CDC activation |
| `leyline` binary (recommended for downstream consumers) | `task install:full` — `--features all` (adds `vec` + `text-search`). Portable — no libfuse-t/libfuse runtime dep |
| `leyline` binary (everything including mount) | `task install:full+mount` — `--features full`. Requires libfuse-t (macOS `brew install fuse-t`) or libfuse (Linux `apt install libfuse-dev`) at runtime |
| Distroless OCI image | `task image` — produces `ley-line-open:0.10.3` (~20 MB) via krust + cargo-zigbuild static musl; image default CMD is `daemon --mcp-port 8384 --mcp-bind 0.0.0.0` |
| FFI staticlibs + header | `task release:fs-static:target` — builds the mache-facing `leyline-fs` staticlib with explicit CDC support and publishes it as a verified GitHub release artifact (linux amd64/arm64 + darwin arm64; macOS amd64 staticlib currently absent) |
| Go schema client | `clients/go/leyline-schema` — nested Go module, tag-published as `clients/go/leyline-schema/v<version>` only when the public schema changes; binary-only/private-storage releases keep advertising the latest schema tag |

Release flow is on-tag-push: `task readme:version-check` gates README version-pin drift in CI (mirroring the `compat:check` + `gen:server-json:check` pattern). See [README.md § Build](../README.md#build) for the recommended install path per user type.

---

## What this doc does NOT cover

- **Per-crate API detail.** Lives in each crate's `README.md`.
- **Per-table schema.** Lives in [`TABLE_CONTRACT.md`](TABLE_CONTRACT.md).
- **Cloister, mache, control-room internals.** Lives in those repos.
- **Decade-level / strategic problem-statement docs.** Live in [`docs/decades/`](decades/) and [`docs/problems/`](problems/).
- **Research / red-team output.** Lives in [`docs/research/`](research/) and [`docs/audits/`](audits/).

This doc is the structural skeleton; the per-area detail is one click away.
