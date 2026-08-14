# leyline-mcp-descriptor

MCP Registry `server.json` emitter, shared across ART producers. Validates tool/group coverage, then renders.

## Why this is a crate and not a per-repo script

Four repos publish an MCP `server.json` and each solved it differently (bead `ley-line-open-4ec276`):

| Repo | Approach |
|---|---|
| ley-line-open | generates from the in-code tool registry, drift-gated |
| mache | generates from `internal/mcpregistry`, drift-gated (Go) |
| rosary | hand-maintained manifest + a `server-json:check` assertion |
| canonical-hours | a script that string-patches the version in place |

Three maturity levels of one rule. Per `vigil-4b304d`: *regenerable deterministically → GENERATE*, because then drift is unrepresentable rather than merely detected.

## What's here

- **`ServerMeta` / `PackageMeta` / `TransportMeta` / `ArtifactMeta`** — the typed shape of a `server.json`.
- **`ToolRef` / `GroupRef`** — tool and tool-group coverage declarations, validated against the emitting repo's actual registry before rendering.
- **`render`** — produces the final `server.json` bytes from validated metadata.

## Used by

- **`server-json-gen`** (`tools/server-json-gen`) — LLO's own `cargo run -p server-json-gen > server.json` generation path.

Not linked into the daemon (`leyline-cli-lib`) — this is build/release-time tooling, not a runtime dependency.
