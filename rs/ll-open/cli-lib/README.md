# leyline-cli-lib

The ley-line daemon — living SQLite database + arena snapshot loop + UDS/MCP serving surface. The `leyline` binary (`rs/ll-open/cli`) is a thin wrapper around this crate.

## What's here

- **`cmd_daemon`** — daemon entry point. Owns the living db (in-memory SQLite), the Σ snapshot loop (arena flip + BLAKE3 root advance), and lifecycle phases (`Initializing` → `Parsing` → `Enriching` → `Ready`).
- **`daemon::ops`** — request dispatch. 23 base ops grouped by purpose: lifecycle (status, flush, load, snapshot, reparse, enrich), navigation (list_roots, list_children, get_node, read_content), graph queries (find_callers, find_callees, find_defs, get_refs_map, get_defs_map), introspection (get_schema, get_db_path), LSP (lsp_hover, lsp_defs, lsp_refs, lsp_symbols, lsp_diagnostics), bulk SQL (query), and embedding search (vec_search, feature-gated). Adding a new op: variant in `BaseRequest`, arm in `dispatch_typed`, entry in `is_known_base_op`.
- **`daemon::wire`** — request-side serde dispatch (`BaseRequest` tagged enum + `LspPosition`/`LspFile` typed args). The response side is generated from `daemon.capnp` via the capnp-json codec; there is no hand-written response mirror after b0ea2e.
- **`daemon::socket`** — UDS read loop, dispatch chain (event ops → base ops → extension async → extension sync → unknown).
- **`daemon::mcp`** — MCP HTTP transport. Maps each base op to an `McpTool` and routes through the same `handle_base_op_value` entry as the UDS path.
- **`daemon::events`** — pub/sub event router. Handlers emit `daemon.{op}` events for state-changing ops (reparse, snapshot, enrich, load); the router fans out to subscribed UDS connections.
- **`daemon::enrichment`** — extension-pass orchestration. LSP, embeddings, HDC each register as `EnrichmentPass`; `run_pass` resolves dependencies and runs them in order. Stats reported via the `enrich` op.
- **`daemon::ext`** — extension trait. Private LLO consumers (ley-line proper) implement `DaemonExt` to add custom ops and enrichment passes without forking.
- **`cmd_parse`** — tree-sitter parse pass. Walks source dirs, emits AST + source segment files + populates `nodes`/`_ast`/`_source`/`node_refs`/`node_defs`.
- **`cmd_load`** — restore from arena: BLAKE3-verify the buffer, `sqlite3_deserialize` into the living db.

## Wire protocol

Two transports, single dispatch table:
- **UDS** (`default.sock`) — line-delimited JSON; shell-debuggable (`echo '{"op":"status"}' | nc -U`).
- **MCP HTTP** (`:8384/mcp`) — JSON-RPC; consumed by cloister + agentic clients.

Every base-op response except `query`/`lsp_*`/`vec_search` is typed against `daemon.capnp` via [`daemon/wire.rs`](src/daemon/wire.rs) and emitted by `capnp_json::to_json`. The three carve-outs emit ad-hoc JSON (row payloads with method-specific shapes — typing them would add ceremony without buying drift detection beyond what the fixture gate already provides). See [ADR-0014's interim status](../../../docs/adr/0014-capnp-as-protocol.md) for the JSON-as-carrier doctrine; bead `ley-line-open-40df83` tracks the planned binary-capnp wire as the natural step 4.

## Used by

- `leyline-cli` — the `leyline` binary
- ley-line (private) — registers extension passes via `DaemonExt`

## Drift gates

- `tests/fixtures/daemon-protocol.json` + `tests/integration.rs::daemon_protocol_gate_handlers_emit_required_keys` — runtime handler ↔ schema parity
- `clients/go/leyline-schema/daemon/daemon_protocol_test.go` — cross-runtime: every fixture decodes into typed Go bindings under strict-unmarshal

Both gates are load-bearing; `wire.rs` is hand-written (request enum + LSP args + intermediate plain data) and the response side is codegen'd at runtime by capnp-json from the typed `daemon.capnp` schema.
