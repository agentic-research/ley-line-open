# leyline-mcp-protocol-schema

MCP wire-protocol facts, **generated at build time** from a digest-pinned schema (bead `ley-line-open-60f0d3`) — the JSON-RPC method names (`tools/call`, `server/discover`, notifications, …) that the daemon and tests would otherwise hand-encode as string literals.

## Why generated, not recorded

LLO used to hand-mirror MCP protocol facts in several places, and one went stale within days: SEP-2567 / SEP-2575 removed sessions and `initialize` from the spec, and nothing noticed. Recording the corrected fact anywhere by hand — a constant, a comment, a bead — is the same failure with a delay: a plausible value with nothing checking it against the source.

## How the pin is enforced

`build.rs` reads the vendored `schema/mcp/protocol.<REV>.json`, hashes it, and **fails compilation** on a digest mismatch — not a test failure, a hard build error. Every exported constant derives from the verified bytes; the only fact in this crate typed by a human is the sha256 pin in `build.rs` itself.

## What's here

- **`vendored_relative_path`** — the path to the vendored schema file the digest check reads.
- Generated constants for MCP JSON-RPC method names, consumed wherever LLO's daemon or tests would otherwise hardcode a protocol string.

## Used by

- **`leyline-cli-lib`** — the daemon's MCP HTTP transport (`daemon/mcp.rs`).
