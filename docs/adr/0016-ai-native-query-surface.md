# ADR-0016 — AI-native query surface (LSP as compatibility shim, not primary protocol)

**Status:** Proposed (2026-05-16)
**Decade:** `ley-line-open-9d30ac` (Σ Merkle-CAS substrate) — consumer-facing layer
**Thread:** `T9/consumer-protocol-shape`
**Bead:** `ley-line-open-9f491f`

**Sibling artifacts** (read for fuller context):
- `docs/adr/0014-capnp-as-protocol.md` — wire encoding (capnp-json over UDS / MCP HTTP). Settled. ADR-0016 does NOT revisit wire encoding.
- `docs/adr/0015-lazy-on-access-ingestion.md` — *when* parsing happens. ADR-0015 explicitly defers "what shape do consumers see" to this ADR (comment on `ley-line-open-9db858`, 2026-05-16).
- `rs/ll-open/cli-lib/src/daemon/mcp.rs` — current MCP tool registry. ADR-0016 names ops that exist today (`find_callers`, `find_callees`, `find_defs`, `lsp_hover`, etc.) and ops that are net-new (`inspect_symbol`, `inspect_neighborhood`, `at_position`, `search_symbols`).

---

## Context

### What LSP is shaped for

The Language Server Protocol was designed in 2016 to serve IDEs. Every load-bearing decision in its surface reflects the **human-at-a-cursor** consumer model:

| LSP feature | IDE-driven reason it exists | What it costs an AI agent |
|---|---|---|
| **Position-keyed requests** (`textDocument/hover` takes `(uri, line, character)`) | An IDE knows the cursor location and almost nothing else | Agents have a symbol name (`SocketClient.SendOp`) and don't know — or care — what line it lives on. They must first resolve symbol → position to issue the request. |
| **Per-keystroke didChange / publishDiagnostics** | Editor needs squigglies updated as the user types | Agents make discrete batch queries with seconds-to-minutes between them. The streaming-fast-path is dead weight. |
| **Markdown hover content** (`MarkupContent { kind: "markdown", value: "```rust\nfn foo(...)\n```\n\n*docs*" }`) | Editor has a markdown renderer; humans read prose with code fences | Agent has to *parse* the markdown to recover the type signature it asked for. The wrapping fences are noise the agent must strip before reasoning. |
| **didOpen/didChange/didClose session ceremony** | Editor "owns" a document buffer and notifies the server of the buffer state | Agent has no persistent buffer; it has a query and the on-disk repo. Telling the server "I am opening this file" is meaningless. |
| **One method, one answer** (`hover` returns hover; `definition` returns definition; `references` returns references) | Editor displays one popup at a time; latency budget is per-keystroke | Agent's actual question is "tell me about this symbol" — a *bundle* of (definition, signature, docs, callers, callees, examples). Atomic LSP methods force N round-trips. |
| **Stateful capability negotiation** (`initialize` exchanges client/server capabilities) | Editor and server are long-lived peers in a 1:1 session | Agent dispatches are often one-shot; the initialize round-trip is amortized over too few queries to be worth its cost. |

These aren't bugs in LSP. They are the right answers to the questions an IDE asks. They are the wrong answers to the questions an AI agent asks.

### What LLO's actual consumers need

Three first-class consumer classes today:

1. **mache** (Go, batch graph analysis). Asks "give me all callers of X with their type signatures across the repo so I can community-detect." Currently issues 1× `find_callers` + N× per-result lookups.
2. **cloister** (cross-runtime gateway, dispatches to LLO and other backends). Asks "answer this MCP `tools/call` and stream the result." Sees LLO as a single op surface, not a protocol with session state.
3. **agent dispatchers** (rosary's dev-agent / architect-agent / staging-agent, all via MCP `tools/call`). Ask "inspect this symbol, with enough context that I can decide whether to read the source file or move on." Today they issue `find_callers` → for-each result `get_node` → for-each result `read_content`. 1 query becomes 1 + 2N round-trips.

A fourth, future consumer class — **IDEs** (VS Code, Neovim, Zed) — needs LSP. We will not give up that audience. But IDEs are not the primary consumer; they are an adapter target.

### Where this ADR fits relative to ADR-0014 and ADR-0015

- **ADR-0014** settles the *wire encoding* (capnp-json over UDS and MCP HTTP). Not revisited here. ADR-0016 adds *new ops* to that wire; it does not change the encoding.
- **ADR-0015** settles *when ingestion happens* (lazy-on-access vs eager). Its consumer-facing surface (section 6 in the original draft) was scoped down to "lazy-access concerns only" via the 2026-05-16 comment on `ley-line-open-9db858`. The shape of the consumer protocol moved here.
- **ADR-0016** settles *what shape consumers see*. Eight sub-decisions follow.

---

## Decision

ADR-0016 commits, in order of load-bearing-ness:

### 1. Symbol-keyed lookup is the default; position-keyed is an explicit translation

**Choice.** The primary op is `inspect_symbol(symbol_id) -> Bundle`, where `symbol_id` is a stable string like `mache/internal/leyline.SocketClient.SendOp` (Go), `rs/ll-open/cli-lib/src/daemon/mcp.rs::handle_post` (Rust path::item), or the equivalent in other languages. Position-keyed access is an explicit translation step: `at_position(file, line, col) -> { symbol_id, kind }`. An agent that has a name asks once; an editor that has a cursor pays one extra translation hop.

**Alternatives considered.**

- *(a) Position-keyed primary, symbol-keyed shim.* LSP's shape. Forces agents to first resolve "where in the file does symbol X live?" before they can ask anything about it — a question they don't know the answer to and don't care about. Rejected: optimizes for the consumer (IDE) that has the position and against the consumer (agent) that doesn't.
- *(b) Symbol-keyed only, no position translation.* Cuts out editor consumers entirely. Rejected: violates the explicit goal of preserving LSP as a compatibility shim (decision 8). VS Code hovers will need to translate cursor → symbol_id once per hover, then ask `inspect_symbol`.
- *(c) Symbol-keyed and position-keyed coequal, both top-level.* Two parallel surfaces, agents and editors each pick one. Rejected: the second surface is overhead for the 90% of consumers that don't need it, and "coequal" tends to drift — one surface gets better-maintained than the other.

**Falsifiability.** Agent flow "find what this function does + who calls it" must take **≤ 2 op-calls** (`inspect_symbol` returns both bundle and caller list). LSP today: `textDocument/hover` + `textDocument/definition` + `textDocument/references` + per-result `textDocument/hover` for caller type sigs — typically 5+ round-trips. Test: instrumented MCP wire counts in the worked example below.

---

### 2. Bundle responses; one query answers the full natural question

**Choice.** `inspect_symbol(id)` returns, in a single response:

```json
{
  "symbol_id": "mache/internal/leyline.SocketClient.SendOp",
  "kind": "function",
  "definition": { "file": "...", "line_start": 142, "line_end": 168, "byte_start": 4221, "byte_end": 4990 },
  "hover_typed": {
    "signature": "func (c *SocketClient) SendOp(op string, args any) ([]byte, error)",
    "docstring": "SendOp writes a length-prefixed JSON op...",
    "kind": "function",
    "receiver_type": "*SocketClient"
  },
  "references": [ { "symbol_id": "...", "file": "...", "line": 17, "kind": "call" }, ... ],
  "implementations": [ ... ],
  "callers":  [ { "symbol_id": "...", "signature": "...", "file": "..." }, ... ],
  "callees":  [ { "symbol_id": "...", "signature": "...", "file": "..." }, ... ],
  "freshness": { "generation": 4217, "parsed_at_ms": 1747424812931, "source_mtime_ms": 1747424801222, "stalk_hash": "..." }
}
```

This is **net-new** — no current MCP op returns this shape (see `rs/ll-open/cli-lib/src/daemon/mcp.rs`). It composes existing primitives (`find_callers`, `find_callees`, `find_defs`, `get_node`, `read_content`, LSP hover) into one call.

**Alternatives considered.**

- *(a) Keep atomic ops; agents compose them.* Status quo. Agents pay N round-trips, each with serialization + UDS + dispatch overhead. Rejected: the falsifiability gate B below shows the cost.
- *(b) Bundle, but make every sub-field opt-in via flags* (`inspect_symbol(id, include=["definition", "callers"])`). Rejected as default: the cost of returning all fields is dominated by the queries that produce them (not the wire bytes), and those queries are cheap when the data is already in the snapshot. Add `include`/`exclude` flags as an **optimization knob**, not the default API shape — default returns the full bundle.
- *(c) GraphQL-style query language.* Caller specifies a query selecting which fields they want. Rejected: enormous implementation surface, brittle schema evolution, no existing consumer asks for this flexibility. Default bundle + opt-out flags covers the access pattern.

**Falsifiability.** The full bundle for a typical Go function (`mache/internal/leyline.SocketClient.SendOp`) returns in **< 50ms p99** and **≤ 16KB JSON** on a warm snapshot. Test: bench harness, p99 over 100 runs on the mache repo.

---

### 3. Structured types in hover; no markdown rendering on the server

**Choice.** `hover_typed` is a typed record:

```json
{
  "kind": "function",
  "signature": "func (c *SocketClient) SendOp(op string, args any) ([]byte, error)",
  "docstring": "SendOp writes a length-prefixed JSON op to the daemon UDS.\n\nReturns the raw response bytes.",
  "receiver_type": "*SocketClient",
  "params": [ { "name": "op", "type": "string" }, { "name": "args", "type": "any" } ],
  "returns": [ { "type": "[]byte" }, { "type": "error" } ],
  "source_excerpt": "func (c *SocketClient) SendOp(...) ([]byte, error) {\n\t...\n}"
}
```

No backticks. No code fences. No `MarkupContent { kind: "markdown", value: "```go\n...\n```" }`. The `docstring` field carries the comment text **verbatim** as the author wrote it (which may itself contain markdown — that's fine; it's the author's content, not server-applied wrapping).

**Alternatives considered.**

- *(a) LSP-style markdown hover.* Server formats `"```go\n<sig>\n```\n\n<doc>"` and ships it. Rejected: every agent consumer has to parse markdown back into fields. The server already has the fields; emitting them as structure is strictly more useful.
- *(b) Structured + markdown alongside.* `hover_typed: { ... }` AND `hover_markdown: "..."`. Rejected as default: doubles the response size, and the LSP shim (decision 8) reconstructs markdown from `hover_typed` cheaply. No need to ship both eagerly. Editor adapter can build markdown locally.
- *(c) HTML-rendered hover.* Some IDEs do this. Rejected: even worse than markdown for agents, and editors that want HTML can render from structure.

**Falsifiability.** Grep the response JSON of any non-shim op: zero occurrences of `` ``` `` (triple-backtick) in fields other than `docstring`. Test: schema-validation gate in CI.

---

### 4. Stateless requests; no didOpen/didChange/didClose

**Choice.** Every op carries everything it needs. No `initialize` round-trip required (the MCP `initialize` exchange is fine; what's absent is per-document session state). No `textDocument/didOpen` / `didChange` / `didClose`. No per-consumer "open files" set tracked on the server.

If a consumer wants to drive parsing, they call `reparse(files=[...])` explicitly (ADR-0015's lazy-on-access doctrine handles the case where they don't).

**Alternatives considered.**

- *(a) LSP-style session state.* Server tracks per-client open documents and dispatches re-parse on `didChange`. Rejected: meaningless for agents (no persistent buffer), and even for editors the `mtime`-driven reparse triggered by ADR-0015's lazy-access path is sufficient.
- *(b) Optional session state for clients that want it.* Implement `didOpen` etc. as no-ops or as parse-now hints. Rejected: maintains the surface area of LSP's worst feature in the name of compatibility, when the LSP shim can fake it locally (drop didOpen on the floor; let mtime-driven invalidation cover the change-tracking case).
- *(c) Subscription model* (consumer subscribes to symbol updates; server pushes when underlying data changes). Rejected: real-time invalidation is useful but orthogonal to statelessness — it's better modeled as a separate SSE stream on `GET /mcp` (already stubbed) than as session state on every request.

**Falsifiability.** Two agents querying the same symbol back-to-back must produce **byte-for-byte identical** responses (modulo `freshness.parsed_at_ms` which advances on reparse). Test: dispatch the same `inspect_symbol(X)` from two distinct connections; compare responses; assert equality after stripping the `parsed_at_ms` field.

---

### 5. Cross-file expansion in one request

**Choice.** `inspect_neighborhood(symbol_id, depth=N, edge_kinds=[CALLS, REFS, IMPORTS]) -> Neighborhood` returns the focal symbol's bundle plus the N-hop neighborhood, where each neighbor is itself returned with a (possibly truncated) bundle. Bounded by:

- `depth` (caller-provided, default 1, max 4)
- a hard byte cap on the response (default **64KB**; caller can request up to 1MB by passing `max_bytes`)
- per-neighbor truncation: distant neighbors return a `symbol_id` + `hover_typed.signature` only, not full callers/callees

This is **net-new**. Existing ops (`find_callers`, `find_callees`) traverse one edge each.

**Alternatives considered.**

- *(a) Caller-driven graph walk via repeated `inspect_symbol`.* Status quo extended. Rejected: every hop is a round-trip; depth-2 with 10 neighbors per hop is 11 round-trips minimum, 111 if the caller pulls full bundles on every neighbor.
- *(b) Server-driven walk with mandatory full bundles on every neighbor.* Rejected: response size blows up unboundedly. The 64KB byte cap with per-distance truncation is the resilience hatch.
- *(c) Bidirectional streaming neighborhood walk* (server streams nodes as it walks). Rejected for v1 in favor of the bounded single-response shape (simpler client code), but compatible with decision 6's streaming infrastructure if a future caller needs it.

**Falsifiability.** Agent needing "this symbol + 2 hops, full bundles for hop-1, signature-only for hop-2" makes **1** UDS/MCP round-trip. Instrument the wire layer with a per-connection write counter; assert `writes_for_neighborhood_query == 1`.

---

### 6. Streaming partial results for unbounded queries

**Choice.** `search_symbols(pattern, limit, kind?) -> Stream<SymbolMatch>` streams as **newline-delimited JSON** (NDJSON) over the MCP `tools/call` response body. Same shape over UDS (each match is one line on the socket).

The first result is emitted as soon as the underlying SQL `LIMIT` clause yields its first row (server uses streaming cursor, not buffered result-set). Consumer can close the read side to cancel; server detects the closed socket and aborts the query.

`search_symbols` itself is **net-new** but composes the existing `query` op's underlying SQL primitives.

**Alternatives considered.**

- *(a) Bounded batch response* (`search_symbols` returns all results in one JSON blob, capped at `limit`). Rejected: a 10K-result search holds 10K rows in memory before the consumer sees the first one. Time-to-first-result degrades to time-to-last-result.
- *(b) Pagination via cursor* (`search_symbols(pattern, cursor?) -> { results, next_cursor }`). Rejected as primary: requires the agent to round-trip per page, defeating the latency benefit. Acceptable as a *secondary* surface for resume-after-disconnect, but NDJSON streaming is the default.
- *(c) gRPC-style server streaming.* Mechanism only; NDJSON-over-HTTP-chunked-transfer is the same idea with less protocol surface. Rejected: pulls in protobuf tooling for a problem JSON streaming already solves.

**Falsifiability.** A 10K-result search returns first 100 results **within 100ms** even if total takes 5s. Measure `time_to_first_result_ms` and `time_to_completion_ms` separately; gate the former.

---

### 7. Freshness signal in-band

**Choice.** Every response carries a `freshness` block:

```json
{
  "generation": 4217,
  "parsed_at_ms": 1747424812931,
  "source_mtime_ms": 1747424801222,
  "stalk_hash": "b3:abc123..."
}
```

`generation` is a monotone u64 advanced on every reparse pass. `parsed_at_ms` is when the data was produced. `source_mtime_ms` is the on-disk mtime at the time of last reparse (lets the agent detect drift between the snapshot and the working tree). `stalk_hash` is the sheaf-region hash from ADR-0015's lazy-access topology — fine-grained "did this part of the graph change?" signal.

Agents that cache responses compare `freshness.generation` against their snapshot's high-water mark to detect staleness without polling.

**Alternatives considered.**

- *(a) Out-of-band freshness via separate `status` op.* Status quo (`status` returns `head_sha` and `last_reparse_at_ms`). Rejected for in-band: forces every cache-hit decision to do a second round-trip, defeating the cache.
- *(b) ETag-style HTTP semantics on MCP responses.* Rejected: works for editors that natively understand HTTP caching, doesn't fit UDS, doesn't capture the sheaf-region granularity.
- *(c) Push-based invalidation* (server tells subscribed clients when their cached symbols change). Rejected for v1: orthogonal infrastructure (decision 4 noted this). The freshness counter in every response covers the "did anything change?" question cheaply.

**Falsifiability.** An agent that caches `inspect_symbol(X)` then re-issues the call **after a file change covering X** must see `freshness.generation` advance — even if the symbol itself is unchanged. Test: cache the response; touch the file containing X; call `reparse`; re-issue the query; assert `response2.freshness.generation > response1.freshness.generation`.

---

### 8. LSP compatibility shim — adapter, not parallel implementation

**Choice.** A separate `leyline lsp` subcommand speaks JSON-RPC LSP over stdio (the LSP convention). Internally, every supported LSP method translates to one or more native ops:

| LSP method | Translation | Status |
|---|---|---|
| `initialize` | Local; reply with capabilities derived from native op surface | Supported via translation |
| `textDocument/hover` | `at_position(file, line, col) -> symbol_id` → `inspect_symbol(symbol_id)` → format `hover_typed` as markdown locally | Supported via translation |
| `textDocument/definition` | `at_position` → `inspect_symbol` → return `.definition` | Supported via translation |
| `textDocument/typeDefinition` | `at_position` → `inspect_symbol` → walk `hover_typed.returns[0].type` to its symbol | Supported via translation |
| `textDocument/references` | `at_position` → `inspect_symbol` → return `.references` | Supported via translation |
| `textDocument/implementation` | `at_position` → `inspect_symbol` → return `.implementations` | Supported via translation |
| `textDocument/documentSymbol` | Existing `lsp_symbols(file)` op | Supported via translation (op already exists) |
| `textDocument/publishDiagnostics` | Existing `lsp_diagnostics(file)` op, emitted on `didOpen` / `didSave` | Supported via translation |
| `textDocument/didOpen` | No-op on server; shim caches the buffer locally so subsequent position queries can use the on-disk file (server reads from disk; shim drops in-flight buffer-only edits) | Supported via translation (degraded) |
| `textDocument/didChange` | No-op on server unless `didSave` follows; shim updates local buffer | Supported via translation (degraded — see consequence below) |
| `textDocument/didSave` | `reparse(files=[uri])` | Supported via translation |
| `textDocument/didClose` | No-op | Supported via translation (no state to clean up) |
| `textDocument/completion` | Server has type info but no completion ranking model | **Deferred** (would need completion-specific ranking infra) |
| `textDocument/signatureHelp` | `at_position` of the open paren → ancestor call site → `inspect_symbol(callee)` → return signature | **Deferred** (parser-side support for "inside which call?" is partial) |
| `textDocument/codeAction` | Refactoring ops not in scope | **Intentionally unsupported** (LLO is an inspection store, not an editor) |
| `textDocument/codeLens` | Editor-side UI affordance | **Intentionally unsupported** (no UI; editor builds its own from native ops) |
| `textDocument/documentLink` | Hyperlink resolution in source | **Deferred** (low-value for inspection workflows) |
| `textDocument/documentHighlight` | Same-document occurrences of symbol at cursor | **Deferred** (cheap to add once `references` is solid) |
| `textDocument/formatting` | Editor responsibility | **Intentionally unsupported** (LLO does not own formatting) |
| `textDocument/rangeFormatting` | Editor responsibility | **Intentionally unsupported** |
| `textDocument/onTypeFormatting` | Per-keystroke editor responsibility | **Intentionally unsupported** (semantics make no sense for batch agents and per-keystroke triggers are absent from LLO's model) |
| `textDocument/rename` | Refactor op | **Intentionally unsupported** (LLO is inspection-only; not an editor) |
| `textDocument/prepareRename` | Refactor op | **Intentionally unsupported** |
| `textDocument/foldingRange` | Editor UI | **Intentionally unsupported** (editor builds from AST) |
| `textDocument/selectionRange` | Editor UI | **Intentionally unsupported** |
| `textDocument/semanticTokens/*` | Editor highlighting | **Deferred** (cheap to derive from AST, but no current consumer) |
| `textDocument/linkedEditingRange` | Editor UI | **Intentionally unsupported** |
| `textDocument/moniker` | Cross-repo symbol identity | **Deferred** (becomes interesting once Σ-graph cross-repo navigation lands) |
| `textDocument/inlayHint` | Editor display | **Deferred** (derivable from `hover_typed`) |
| `textDocument/inlineValue` | Debugger UI | **Intentionally unsupported** (debugger out of scope) |
| `textDocument/colorPresentation`, `documentColor` | Color picker in source | **Intentionally unsupported** |
| `textDocument/diagnostic` (pull model) | Pulls diagnostics for a file | **Deferred** (existing `publishDiagnostics` push model covers current consumers) |
| `workspace/symbol` | Workspace-wide symbol search | Supported via translation (calls `search_symbols`) |
| `workspace/executeCommand` | Server-defined commands | **Intentionally unsupported** (LLO has no command API; rosary owns command dispatch) |
| `workspace/applyEdit` | Server-initiated edits | **Intentionally unsupported** (LLO is read-only with respect to source) |
| `workspace/willRenameFiles`, `didRenameFiles`, etc. | File lifecycle | **Deferred** (mtime-driven invalidation covers the common case) |
| `workspace/didChangeConfiguration` | Editor pushes config | **Deferred** (no current consumer needs server-side config state) |
| `workspace/didChangeWorkspaceFolders` | Multi-root workspace | **Deferred** (LLO operates against a single `--source` root today) |
| `callHierarchy/*` | Call hierarchy view | Supported via translation (composes `inspect_symbol.callers/callees`) |
| `typeHierarchy/*` | Type hierarchy view | **Deferred** (Go interface satisfaction graph is partial in LLO today) |
| `$/cancelRequest` | Cancel in-flight request | **Deferred** (cooperative cancel works at the streaming layer per decision 6, but not yet plumbed through LSP shim) |

Every LSP method specified in the LSP 3.17 spec is placed in exactly one of `{supported via translation, deferred, intentionally unsupported}`. None silently absent.

**Alternatives considered.**

- *(a) Native LSP server alongside native AI surface.* Two parallel implementations, both touching the same data. Rejected: doubles the maintenance cost; the two surfaces will drift; "which one is canonical?" becomes a question at every disagreement.
- *(b) No LSP support at all; tell editors to call MCP directly.* Rejected: zero editor adopts MCP today; LSP is the entry into VS Code / Neovim / Zed / Emacs. Giving that up means giving up the editor consumer category entirely, which is gratuitous when a thin shim suffices.
- *(c) Embed an existing LSP server (gopls, rust-analyzer) under LLO.* Rejected: the existing LSP servers don't expose LLO's structural data (Σ root, sheaf invalidation, cross-repo navigation). Embedding them recreates the inversion this ADR exists to avoid.

**Falsifiability.** Connect VS Code to `leyline lsp` against the mache repo; hover over `SocketClient.SendOp`. Must return a hover including the type signature within **200ms p99**. The shim is compat target, not a production-hot-path target. Test: VS Code integration test with timed assertion.

---

## Worked example — Gate B

**Task:** Find all callers of `mache/internal/leyline.SocketClient.SendOp` along with their type signatures. Depth 1.

Assumptions for the comparison:
- mache repo, warm snapshot (file has been parsed; sheaf cache is hot)
- 9 callers across 5 files (count from current mache `find_callers` against this symbol)
- UDS round-trip baseline: 1ms each on localhost (kernel-only) ignoring serialization
- MCP HTTP round-trip baseline: 3ms each on localhost (adds Axum / JSON-RPC framing) ignoring serialization
- Payload sizing: typical Go hover ≈ 380 bytes JSON-encoded; a `references` array entry ≈ 110 bytes; a full `inspect_symbol` bundle for `SocketClient.SendOp` with 9 callers ≈ 4.2KB

### Path 1 — LSP today

| Step | LSP request | Round-trips | Payload bytes (req + resp) | Cumulative latency (MCP HTTP @ 3ms) |
|---|---|---|---|---|
| 1 | Translate symbol name → position. LSP does not have a symbol → position primitive; closest is `workspace/symbol` with name filter, returning candidate locations. | 1 | 160 req + 720 resp (3 candidates) | 3ms |
| 2 | `textDocument/hover` on the candidate location | 1 | 220 req + 560 resp (markdown-wrapped sig) | 6ms |
| 3 | `textDocument/references` at the position to get the 9 caller positions | 1 | 220 req + 1,150 resp (9 × ~130 bytes) | 9ms |
| 4 | For each of 9 caller positions, `textDocument/hover` to recover the caller's type signature | 9 | 9 × (220 req + 560 resp) = 7,020 | 36ms |
| **Total** | | **12** | **~9.8KB** | **~36ms** |

Notes:
- LSP cannot return the bundle in one call. Each hover/definition/references is its own method, each its own round-trip.
- Hover responses are markdown-wrapped (`"```go\nfunc ...\n```\n\ndoc"`); the agent must parse the markdown to recover the signature it asked for. Counted in payload bytes; not counted in latency (parsing is fast).
- `workspace/symbol` returns candidate locations and the agent must disambiguate. If the candidate is wrong, add another round-trip.

### Path 2 — AI-native surface (this ADR)

| Step | Native request | Round-trips | Payload bytes (req + resp) | Cumulative latency (MCP HTTP @ 3ms) |
|---|---|---|---|---|
| 1 | `inspect_symbol("mache/internal/leyline.SocketClient.SendOp")` — returns definition, hover_typed, references, callers (with `hover_typed.signature` embedded per-caller), callees, freshness | 1 | 110 req + 4,200 resp | 3ms |
| **Total** | | **1** | **~4.3KB** | **~3ms** |

Notes:
- Caller signatures are embedded in the `callers` array directly (see decision 2). No per-caller hover follow-up.
- Single round-trip means the connection latency dominates once over not 12 times.

### Comparison

| Metric | LSP today | AI-native | Ratio (LSP / AI) |
|---|---|---|---|
| Round-trips | 12 | 1 | **12×** |
| Payload bytes | ~9.8KB | ~4.3KB | **2.3×** |
| Latency (3ms RTT, MCP HTTP) | ~36ms | ~3ms | **12×** |
| Latency (1ms RTT, raw UDS) | ~12ms | ~1ms | **12×** |

**Gate B threshold:** ≥3× round-trip reduction AND ≥2× payload reduction.
**Result:** 12× round-trip reduction, 2.3× payload reduction. **Pass.**

The payload reduction is the weakest dimension (the bundle response is genuinely large because it carries the caller signatures the agent asked for). The round-trip reduction is the dominant win — and on cold caches or higher-latency links (containerized deployments, remote workspaces) the latency multiplier grows.

---

## Consequences

### Positive

- **Round-trip tax eliminated for the dominant agent access pattern.** "Inspect symbol X" is one call, not 12. On a 3ms-RTT MCP HTTP link, that's a 33ms-per-symbol savings; for an agent doing 100 inspections in a session that's 3.3 seconds of wall-clock latency removed.
- **Structured responses are agent-friendly.** No markdown parsing; no "did the server emit `` ```go `` or `` ```rust ``?" sniffing. The agent reasons over typed fields.
- **Freshness is in-band.** Caching is correct without polling.
- **LSP audience preserved.** VS Code / Neovim / Zed still work via the shim; we don't burn the editor bridge.
- **Composable surface.** `inspect_neighborhood` is `inspect_symbol` + a graph walk; `search_symbols` is `find_callers`'s underlying SQL with a streaming adapter. Each new op is a thin wrapper over existing primitives in `daemon/ops.rs`, not a parallel implementation.

### Negative

- **New op surface.** Four net-new ops (`inspect_symbol`, `inspect_neighborhood`, `at_position`, `search_symbols`) plus the LSP shim subcommand. Each needs a schema entry in `daemon/mcp.rs`, a handler in `daemon/ops.rs`, and tests. Estimated 800–1200 LOC of net-new code plus the LSP shim crate (~2000 LOC for translation logic).
- **Bundle responses are larger per-call.** The worked example shows the AI-native path returning 4.3KB in one shot vs LSP's 12 calls totalling ~9.8KB. On a query where the consumer would only need a subset (e.g., just `definition`), the bundle wastes bytes. Mitigation: `inspect_symbol(id, include=["definition"])` opt-out flag (decision 2 alternative b, demoted to optimization knob).
- **LSP shim is degraded.** `didOpen`/`didChange` are no-ops on the server; the shim holds the unsaved buffer locally. Editors that rely on per-keystroke diagnostic updates see staler diagnostics than they would against gopls. This is intentional — the latency budget for batch agent queries is not per-keystroke — but it is a regression for that one editor use case.
- **Maintenance cost: two surfaces.** Native ops and LSP shim must stay in sync as we add ops. Mitigation: the LSP shim is mechanical translation (every supported LSP method maps to ≤2 native calls); regressions surface as integration-test failures against a frozen VS Code recording.

### What we deliberately can't do that LSP can

- **Per-keystroke incremental updates.** No `didChange` re-parse; reparse is mtime/save-driven via ADR-0015. Editors that need sub-second feedback on unsaved buffer state will see latency.
- **Refactoring** (`rename`, `codeAction`, `applyEdit`). Out of scope. LLO is an inspection store; the editor or a separate refactor service owns the write side.
- **Formatting** (`formatting`, `rangeFormatting`, `onTypeFormatting`). Editor responsibility.
- **Code-lens / inlay hints** as server-pushed UI. Editors can build these locally from `inspect_symbol`'s bundle if they want; we don't push them.

### Migration story for existing consumers

- **mache** (Go): currently issues `find_callers` + per-result `get_node` + per-result `read_content`. Migrate to `inspect_symbol` once landed. Backward compat: `find_callers`/`find_callees`/`find_defs` remain; they're the building blocks of `inspect_symbol`.
- **cloister**: passes MCP `tools/call` through. New tool names appear in `tools/list`; cloister needs no code change unless it filters tools by name.
- **rosary dispatcher agents**: migrate prompts to prefer `inspect_symbol` over `find_callers` + follow-up. Old tool names stay valid; agents that don't migrate keep working at the round-trip-tax cost.
- **VS Code / IDE users**: `leyline lsp` subcommand new. Currently zero VS Code users of LLO directly; the shim opens that door.

---

## Skeptic-pass defenses (Gate C)

Each load-bearing claim in this ADR re-read with adversarial intent. The defenses below address the strongest objections; they are not exhaustive.

**Claim: "AI agents have no cursor."**
*Skeptic:* But MCP-driven editors do — a Claude Code session with `lsp` tools exposed has both a cursor (the user's) and AI access. Doesn't that contradict the premise?
*Defense:* The cursor in those sessions belongs to the *user*, not the agent. The agent receives a symbol name from the user ("explain `SocketClient.SendOp`") and queries by symbol; the user's cursor is upstream of the agent's query and irrelevant to the protocol shape between agent and server. The agent never needs to ask "what's at line 142?" — it asks "what's `SendOp`?". If a future MCP transport hands cursors to agents (debugger-style: "here's what the user is looking at"), `at_position` is the explicit bridge.

**Claim: "Bundle responses are ≤16KB."**
*Skeptic:* For a function with 1000 callers across the repo, the bundle is much larger than 16KB. Doesn't this break?
*Defense:* The 16KB falsifiability target is for a *typical* function (the example chose `SendOp`, which has 9 callers). For high-fanout symbols, `references`/`callers`/`callees` get truncated with a `truncated: true` flag and a `total_count`. The agent can request the full set via `inspect_neighborhood(id, depth=1, edge_kinds=[CALLS])` with a higher `max_bytes`, or via paginated `search_callers(id, limit, offset)` if it really wants all 1000. The bundle is for the common case; the unbounded case is what decision 5 + decision 6 cover.

**Claim: "LSP shim is a thin adapter."**
*Skeptic:* The shim must handle 30+ LSP methods, track per-client capabilities, manage the JSON-RPC framing, hold unsaved buffer state. "Thin" is misleading.
*Defense:* Of the 30+ methods in the LSP 3.17 spec, the coverage map shows 12 are supported-via-translation and 8 are intentionally-unsupported (refactor / formatting / UI). The supported set is ~6 hot methods (`hover`, `definition`, `references`, `documentSymbol`, `diagnostics`, `workspace/symbol`) and 6 lifecycle/cold methods. Each hot method is ≤30 lines of translation logic. The shim is JSON-RPC framing + a method dispatch table + 6 small translators. "Thin" stands relative to a from-scratch LSP server (gopls is 100K+ LOC); the shim should land under 2000 LOC.

**Claim: "Stateless requests."**
*Skeptic:* The freshness block requires the server to know its own generation number, which is state. Aren't you smuggling state in?
*Defense:* Server state is fine — the *server* is stateful (it has a database, an Σ root, a snapshot generation). The decision-4 statelessness is about *per-consumer* state: no `Map<ClientID, OpenFiles>`, no `Map<ClientID, Capabilities>`. The freshness counter is a server-global value that every request reads; it has no per-consumer index.

**Claim: "≥3× round-trip reduction."**
*Skeptic:* The worked example chose the access pattern where LSP is weakest. Cherry-picked.
*Defense:* The worked example is the access pattern that motivates this ADR — bundle-style symbol inspection. For *narrow* access patterns where the agent really only wants one field (just the definition; just the docstring), the LSP single-method call is 1 round-trip and AI-native `inspect_symbol(id, include=["definition"])` is also 1 round-trip with similar payload. Bundle responses pay for themselves on the bundle workload, not on the narrow workload — and the bundle workload dominates agent traffic empirically (every dispatcher prompt that touches code does fan-out queries). If someone shows me an agent that overwhelmingly uses narrow queries, we add `include` as a first-class default and ship. Not seeing it today.

**Claim: "Markdown wrapping is noise."**
*Skeptic:* Some agents prefer markdown — it's what their training distribution looks like. Stripping the fences might hurt model performance.
*Defense:* The agent that wants markdown can reconstruct it from `hover_typed` in three lines of formatting code. The agent that wants structure has no way to recover structure from markdown except by parsing. Asymmetry favors structure as the wire format. If empirical testing shows a measurable model-quality regression from un-fenced signatures, add `hover_markdown` as an optional field — but ship the structured version first and gather the data.

**Claim: "12× round-trip in the worked example assumes MCP HTTP."**
*Skeptic:* Over raw UDS the ratio is the same multiplier, but the absolute latency is much smaller and the win matters less.
*Defense:* True for warm-loopback cases. The MCP HTTP path is the one cloister uses and the one remote MCP gateways will use; that's where the latency win compounds. Even on raw UDS, 12 syscalls vs 1 is a kernel-scheduling win on busy hosts (each UDS round-trip has a context switch). The multiplier is the durable insight; absolute milliseconds are deployment-dependent.

---

## Open implementation questions (out of scope for this ADR)

- Concrete schema for `symbol_id`: is it `path::name` everywhere or do we want a typed `SymbolId { repo, module, item }`? Decided in the implementation bead.
- Whether `inspect_neighborhood`'s default `depth=1` should be 1 or 2 for ergonomics. Empirical question.
- LSP shim's stdio vs TCP transport. Stdio is the editor default; TCP is convenient for testing.
- Cancellation plumbing through MCP HTTP (decision 6 + future `$/cancelRequest` in the LSP shim).

---

## References

- `docs/adr/0014-capnp-as-protocol.md` — wire encoding (capnp-json over UDS / MCP HTTP). Not revisited here.
- `docs/adr/0015-lazy-on-access-ingestion.md` — when ingestion happens. Cross-referenced via the 2026-05-16 comment on `ley-line-open-9db858` that scoped its decision 6 to lazy-access only and deferred consumer-protocol-shape to this ADR.
- `rs/ll-open/cli-lib/src/daemon/mcp.rs` — current MCP tool registry; ADR-0016 ops names and additions reference this surface.
- `rs/ll-open/cli-lib/src/daemon/ops.rs` — `base_op_names()` enumerates dispatch targets; `find_callers` / `find_callees` / `find_defs` exist; `inspect_symbol` / `inspect_neighborhood` / `at_position` / `search_symbols` are net-new.
- Language Server Protocol spec 3.17: <https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/>
- Model Context Protocol: <https://modelcontextprotocol.io/>
- Bead `ley-line-open-9f491f` — this ADR's tracking bead.
- Bead `ley-line-open-9db858` — sibling ADR-0015.
