# Getting started with ley-line-open

Most users don't run `leyline` directly. **mache** (the Go consumer) auto-spawns the daemon and runs `leyline parse` transparently when its consumers (Claude Code, scripts, agents) ask for code-intelligence answers. The path most users want:

```bash
# 1. Install mache (this is the agent-facing surface — see https://github.com/agentic-research/mache)
cd ~/path/to/mache && task install

# 2. Install leyline (so mache's auto-spawn finds it)
cd ~/path/to/ley-line-open && task install

# 3. Use mache. It auto-spawns leyline when needed.
mache find-smells --db /path/to/your.db --rule '<rule_id>'
mache serve  # MCP server agents can connect to
```

If you're using mache, **you don't run `leyline parse` yourself** — `mache/cmd/serve.go::autoInvokeLeylineParse` shells out to `leyline parse <source> -o <tmp.db>` on-demand, then `DiscoverOrStart` spawns a daemon if one isn't already on `~/.mache/default.ctrl.sock`. The whole leyline lifecycle is mache's responsibility for the mache-consumer path.

This document covers two paths:

1. **Use it directly** — when you want to drive `leyline` without mache (writing your own consumer, debugging, profiling parse perf, or generating a .db for offline analysis).
2. **Working on LLO** — when you want to send PRs back.

Skip the rest of this doc on first read if mache is your consumer; `mache install` + this `task install` is the whole story.

---

## Use it directly

### Install

#### Pre-built binary (recommended)

Every tagged release attaches four `leyline` binaries — one per platform mache's auto-downloader probes. Grab the one for your host:

```bash
# macOS Apple Silicon
curl -L https://github.com/agentic-research/ley-line-open/releases/latest/download/leyline-darwin-arm64 \
  -o ~/.local/bin/leyline && chmod +x ~/.local/bin/leyline

# macOS Intel
curl -L https://github.com/agentic-research/ley-line-open/releases/latest/download/leyline-darwin-amd64 \
  -o ~/.local/bin/leyline && chmod +x ~/.local/bin/leyline

# Linux x86_64
curl -L https://github.com/agentic-research/ley-line-open/releases/latest/download/leyline-linux-amd64 \
  -o ~/.local/bin/leyline && chmod +x ~/.local/bin/leyline

# Linux aarch64
curl -L https://github.com/agentic-research/ley-line-open/releases/latest/download/leyline-linux-arm64 \
  -o ~/.local/bin/leyline && chmod +x ~/.local/bin/leyline
```

Ensure `~/.local/bin` is on your `PATH` (or drop the binary anywhere else on `PATH`). That's the whole install — the binary is statically linked, no runtime deps for `leyline parse` / `leyline daemon`.

**Feature set of the released binary**: default features only (`lsp` +
`validate` + `hdc` + `cdc`). Enough for structural analysis, validation, HDC
similarity search, and explicit chunk activation. The `vec` (fastembed) and
`mount` (FUSE / NFS) features are gated OFF in the release binary so it
doesn't drag libfuse or an ONNX model download onto every consumer. If you
need those, build from source (below).

The release also attaches the FFI staticlibs (`libleyline_fs-{darwin,linux}-*.a` + `leyline_fs.h`) for C consumers that link the arena reader directly.

#### From source (needed for `vec` or `mount`)

```bash
git clone https://github.com/agentic-research/ley-line-open
cd ley-line-open
task install:full   # everything portable (--features all, no mount)
```

Three install shapes to choose from (see [README §Install](README.md#install) for the full matrix):

- **`task install`** — same feature set as the released binary (`lsp` + `validate` + `hdc` + `cdc`). Use the released binary instead unless you need a specific commit.
- **`task install:full`** — everything portable (`--features all`, no mount). Adds `vec` (fastembed model, ~100MB cache at `~/.cache/fastembed/` on first `vec_search`) and `text-search`.
- **`task install:full+mount`** — everything including FUSE/NFS mount. Requires `libfuse-t` (macOS) or `libfuse` (Linux) at runtime.

All three produce `~/.local/bin/leyline` (codesigned on macOS).

Runtime deps:
- **None** for the `leyline parse` + `leyline daemon` happy path (daemon default is UDS-only on the control socket; MCP HTTP is opt-in via `--mcp-port`).
- **None** for the default mount path on macOS under `install:full+mount` — `leyline daemon --mount /path` uses the kernel's native NFS client (`leyline serve --backend nfs` is the macOS default; `--backend fuse` is opt-in and requires `fuse-t`).
- **libfuse3** on Linux for `install:full+mount` builds — Linux's default backend is FUSE (`--backend fuse`); NFS is opt-in.
- **A fastembed model cache** at `~/.cache/fastembed/` (~100 MB, downloaded on first `vec_search` use, only present under `install:full` and up). Skip if you don't use `vec_search`.

### Parse a corpus

```bash
leyline parse ./path/to/your/code -o /tmp/my-code.db
# → SQLite db with nodes / _ast / _source / _file_index populated
```

`-l <lang>` filters to a single language (`go`, `rust`, `python`, `proto`, `hcl`, ...). The default parses every recognized file extension. Built-in languages: html, markdown, json, yaml, go, python, elixir, hcl/terraform, rust, protobuf, javascript, typescript.

To enable chunk-backed range reads for an existing projection:

```bash
leyline cdc enable --db /tmp/my-code.db
```

Activation is explicit, bounded, resumable, and idempotent. It keeps
`nodes.record` authoritative while building private derived chunk tables, so
budget for both representations. For a long-lived writable projection, inspect
and collect unreachable chunk history explicitly:

```bash
leyline cdc gc --db /tmp/my-code.db --dry-run --json
leyline cdc gc --db /tmp/my-code.db
```

The sweep is transactional and preserves chunks referenced by any manifest.

### Run the daemon

```bash
# UDS-only (default) — control socket at ~/.mache/default.ctrl.sock,
# no HTTP surface. This is what mache's auto-spawn uses.
leyline daemon --source ./path/to/your/code

# Opt in to CDC activation before the first arena snapshot is published.
leyline daemon --source ./path/to/your/code --cdc

# Add HTTP MCP transport on localhost — opt-in via --mcp-port.
leyline daemon --source ./path/to/your/code --mcp-port 8384
# → also listens on 127.0.0.1:8384/mcp (JSON-RPC) alongside the UDS socket
```

The daemon hosts a living SQLite database with an arena snapshot loop — parses on startup, optionally activates CDC with `--cdc`, watches the source dir, re-parses on change, snapshots into the `.arena` file, advances the Σ root (`current_root` — BLAKE3-32 of the active arena buffer, per T2.4). Tool surface is documented in `server.json` (regenerated by `task gen:server-json`).

**MCP HTTP transport is opt-in** — pass `--mcp-port <port>` to enable it. Without that flag the daemon runs UDS-only, which is what mache's auto-spawn expects. When you DO enable it, the token gate is on by default (ADR-0022): the token lives at `~/.local/share/leyline/daemon.token` on Linux (`~/Library/Application Support/leyline/daemon.token` on macOS). Pass `--mcp-no-auth` to disable for localhost-only debugging. Public-bind (`0.0.0.0` etc.) is refused unless you also pass `--mcp-allow-public`.

### Quick query against a .db

```bash
sqlite3 /tmp/my-code.db "SELECT a.kind, COUNT(*) FROM _ast a GROUP BY a.kind ORDER BY 2 DESC LIMIT 10;"
sqlite3 /tmp/my-code.db "SELECT language, COUNT(*) FROM _source GROUP BY language;"
```

For agent-facing queries (find_definition / find_callers / find_callees / hdc_search / vec_search / `query`), use the daemon's MCP/UDS surface or — preferably — let mache mediate (mache's MCP tools wrap LLO's daemon ops with a smarter consumer-facing API surface).

---

## Working on LLO

You're here if you want to send PRs back.

### Prereqs

```bash
brew install capnp go-task        # capnp ≥1.3.0 (build.rs codegen); go-task is the build entry point
rustup install stable             # current toolchain
brew install fuse-t               # OPTIONAL — only if you want --backend fuse on macOS (default is --backend nfs, needs nothing extra)
brew install sccache              # Optional; Taskfile auto-detects + uses as RUSTC_WRAPPER for faster rebuilds (bead 488440)
```

Linux equivalents: `apt-get install capnproto libfuse3-dev` + `cargo install task` (or whatever your distro packages). FUSE is the Linux default backend, so libfuse3 is the load-bearing mount-time dep there.

### Build + test

```bash
git clone https://github.com/agentic-research/ley-line-open
cd ley-line-open
task ci      # check + clippy + fmt:check + lint:blake3 + smells + build:fs-static + tier:isolation + test + sign:host:{build,test} + compat:check + gen:server-json:check + readme:version-check (~5min cold, much faster with sccache)
task install # release build + macOS codesign + cp to ~/.local/bin (~50s warm; sccache helps)
```

The `task ci` target IS what CI runs — passing it locally guarantees the PR's CI gate will pass for the same set of checks. Per the project's standing discipline: **always run `task ci` before push.** Selective `cargo check` / `cargo test` misses workspace-level gates (`compat:check`, `gen:server-json:check`, `readme:version-check`, etc.).

### Where things live

- **`rs/ll-core/`** — infrastructure tier: arena, capnp schemas, schema, control block (`current_root`).
- **`rs/ll-open/`** — projection engine: parse (`leyline-ts`), LSP ingest (`leyline-lsp`), HDC, sheaf, FUSE/NFS mount, CMS sign, the daemon (`leyline-cli-lib`), the binary (`leyline-cli`).
- **`clients/go/leyline-schema/`** — nested Go module for mache + other Go consumers (pure-Go capnp deserialization, no cgo).
- **`docs/adr/`** — architectural decisions, numbered. Worth reading: [ADR-0014](docs/adr/0014-capnp-as-protocol.md) (wire), [ADR-0016](docs/adr/0016-ai-native-query-surface.md) (query surface), [ADR-0022](docs/adr/0022-mcp-wire-auth-shared-secret.md) (wire auth), [ADR-0024](docs/adr/0024-hdc-substrate-identity-rewrite.md) (HDC), [ADR-0025](docs/adr/0025-hdc-compositional-validation.md) (HDC next).
- **`docs/ARCHITECTURE.md`** — three-layer overview (infra / projection / consumer surface), runtime model, cross-runtime consumer pattern.
- **`docs/TABLE_CONTRACT.md`** — per-table schema contract.

### Bead tracking

Work tracking lives in `.beads/beads.db` (Dolt-backed via [rsry](https://github.com/agentic-research/rosary)). The `bd` CLI talks to it; `rsry_*` MCP tools talk to it. New work starts with a bead — `rsry_bead_search` to dedup, `rsry_bead_create` if new. Commit messages include the bead ID: `[ley-line-open-XXXXXX] type(scope): description`.

### Conventions

- Commit messages: `[bead-id] type(scope): description`. Type is `feat` / `fix` / `chore` / `docs` / `test` / `refactor` / `perf`.
- `task ci` MUST pass before push.
- New ADRs go in `docs/adr/` with the next number. ADRs in `Proposed` status can be drafted in a PR; promotion to `Accepted` is its own decision.
- Don't close a bead with "tiny follow-up needed" — either fix the loose end in the same PR or keep the bead open with narrower scope ("never partial close" discipline).

---

## Project arc

LLO is the open-source data-plane substrate extracted from the private `ley-line` repo (mid-2026). Mache (Go) is the primary consumer; cloister (TypeScript on workerd) consumes via Cap'n Proto FFI; future control-room (Swift) is on the same FFI path. Σ — the Merkle-CAS substrate — is the unifying primitive: BLAKE3-rooted content-addressed bytes that every consumer reads, all SQL projections (`nodes`, `_ast`, `_source`, `_file_index`) derived from it.

Where to read next:

- **Project framing + architecture mermaid**: [README.md](README.md)
- **Architectural overview**: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)
- **ADRs**: [docs/adr/](docs/adr/)
- **Crate-level READMEs**: [rs/README.md](rs/README.md) (workspace map) + per-crate `README.md` in each `rs/ll-*/<crate>/`

---

## Distribution status (the honest version)

- **GitHub releases** (primary channel, actively used): cut on every `v*` tag via [.github/workflows/release.yml](.github/workflows/release.yml). Each release attaches the same 8-asset matrix: 4 `leyline-{darwin,linux}-{amd64,arm64}` binaries (default features — `lsp` + `validate` + `hdc`, no mount) + 3 `libleyline_fs-*.a` FFI staticlibs + `leyline_fs.h`. Latest at [releases/latest](https://github.com/agentic-research/ley-line-open/releases/latest). Mache's auto-downloader pulls `leyline-{GOOS}-{GOARCH}` from `/releases/latest/download/` — that's the load-bearing consumer path.
- **Distroless OCI image**: `task image` builds `ley-line-open:<VERSION>` locally (~20 MB, headless MCP daemon on `:8384`) via krust+docker. **Not auto-published** to a registry today — the `ghcr.io/agentic-research/ley-line-open:VER` references in `README.md` / `server.json` describe the tag the local build produces, not a pushed image. A container-deploy consumer builds it themselves. Auto-push to ghcr on tag is open work.
- **Homebrew**: legacy `homebrew-tap/Formula/leyline.rb` exists but points at the pre-LLO-extraction private repo (v0.2.0, private URLs, `license :cannot_represent`). Not actively maintained; the tap source (`kiln`) has been archived. **Don't `brew install`** today; use the binary-download install above.
- **crates.io**: LLO is a workspace of internal crates, not published individually. Consumers link to the binary or the FFI staticlib, not the source crates.

Open distribution work: homebrew formula update, ghcr auto-push on tag. Pick this up if you want LLO to install via package-manager UX.

---

## I'm stuck

- **`mache` can't find leyline** — check `which leyline`. mache's `DiscoverOrStart` looks on PATH first, then `~/.mache/bin/leyline`. If `task install` put leyline at `~/.local/bin/leyline`, ensure that's on PATH or symlink to `~/.mache/bin/leyline`.
- **`leyline parse` says "language X not supported"** — check the feature flags in [`rs/ll-open/ts/Cargo.toml`](rs/ll-open/ts/Cargo.toml). Daemon ships with `html / markdown / json / yaml / go / python / elixir / hcl / rust / proto / javascript / typescript` enabled; other languages need a feature flag at build time.
- **MCP client can't connect** — token gate is on by default (ADR-0022). Either pass `--mcp-no-auth` (localhost only) or fetch `~/.local/share/leyline/daemon.token` and send as `x-leyline-token` header.
- **Daemon won't warm-start** — `cmd_daemon.rs::try_warm_start_from_arena` logs `warn!` lines for unreadable controller / arena. Check the log; it distinguishes "no arena yet" (silent fall-through to cold start, expected) from "arena exists but unreadable" (visible warn, real failure).
- **Anything else** — file an issue or a bead (`rsry_bead_create scope:repo:ley-line-open`).
