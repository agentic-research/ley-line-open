# Coupling audit (2026-06-17)

**Trigger:** queued by handoff bead `ley-line-open-5f92fa` (2026-05-19). LLO grew fast across the v0.4.4 + v0.4.5 cycle; the substrate primitives compose, but the coupling discipline was never audited. This doc is the audit.

**Scope:** five questions from the handoff:

1. `cli-lib` bloat trajectory — `daemon/ops.rs` dispatch growth (~2k LOC last touched, now 3026)
2. `DaemonContext` accretion in `daemon/mod.rs`
3. `ll-core` (Tier 1) ⟶ `ll-open` (Tier 2) inverse boundary audit
4. `chat-embed` vs `text-search` vs planned `agent-corpus` overlap
5. ADR-0014 §3 capnp triplet pin alignment

**Method:** structural + grep-based enumeration, no dispatched-agent judgment. Findings classified `action` (file a sub-bead) | `noted` (audit-only, no follow-up needed) | `confirmed` (existing plan is sound).

---

## F1 — `daemon/ops.rs` op-name has four sources of truth (action)

`ops.rs` is 3026 LOC. The body is dominated by 32 per-op handler implementations (each ~50-100 LOC); the dispatch overhead itself is small (~130 LOC across three structures). The real coupling cost is **the op-name list is replicated in four places**:

1. `base_op_names()` in `ops.rs:47-87` — `#[cfg(test)]` `Vec<&str>` of 30 names + 2 feature-gated `.push`es
2. `is_known_base_op()` in `ops.rs:151-188` — production `matches!` pattern, 29 string literals + 2 feature-gated `||` branches
3. `dispatch_typed()` in `ops.rs:190-251` — typed match on 32 `BaseRequest` enum variants
4. `tool_registry()` in `mcp.rs:61-326` — MCP tool definitions; a drift test (`mcp.rs:882`) enforces ⊆ `base_op_names()`

`STATE_CHANGING_OPS` (`ops.rs:27`) adds a fifth list of 5 names — disjoint enough to be acceptable.

**Drift tests that exist (`ops.rs:2587-2600`, `ops.rs:3018-3022`):**

- `handle_base_op_dispatches_every_canonical_name` — name in `base_op_names()` implies dispatch arm + `is_known_base_op` recognize it
- `state_changing_ops_subset_of_base_op_names` — `STATE_CHANGING_OPS ⊆ base_op_names()`
- `mcp::tool_registry ⊆ base_op_names()` (`mcp.rs:882`)

**Drift tests that don't exist:**

- Nothing enforces `base_op_names() == is_known_base_op()`'s matched set (the test catches the asymmetric case where `base_op_names` has a name `is_known_base_op` doesn't — adding one in the other direction is undetected).
- Nothing enforces `dispatch_typed` covers every `BaseRequest` variant. Compiler does this for free via exhaustive match, so this is fine.

**Cost:** the load-bearing comment on `ops.rs:95-99` says "Adding a new op is a 3-step process the compiler enforces." Counting reality: wire variant + dispatch arm + `is_known_base_op` + `base_op_names()` + optionally `tool_registry` = **4-5 places**, not 3. The comment is wrong. New contributors will miss `is_known_base_op` (its presence is justified at `ops.rs:148-150` as avoiding test-cfg coupling — defensible but expensive).

**Action:** sub-bead — collapse to one source of truth. Two candidate shapes:

- *Cheapest:* `is_known_base_op` switches to `BaseRequest::deserialize` probe (try-parse on `{"op":<name>}`); `base_op_names()` derives from a const slice or a `serde_introspect`-style helper. Reduces from 4 hand-maintained lists to 1.
- *Cleanest:* a `BaseOp` enum tags every variant with a stable `&'static str` name; `base_op_names()` becomes `BaseOp::ALL.iter().map(|b| b.name())`; `is_known_base_op` is `BaseOp::from_name(op).is_some()`; `dispatch_typed` keeps its typed match. Centralizes name-string ownership on the wire enum itself.

Either approach is a small, focused refactor (~200 LOC delta) and pays off with every new op added thereafter.

## F2 — `DaemonContext` accretion is disciplined (noted)

14 fields. 4 are feature-gated (`vec_index`, `embedder`, `embed_queue` under `vec`; `text_search` under `text-search`). 10 always-present.

Each field has a docstring justifying its presence. Notable cases:

- `enrich_inflight` (lines 134-149) — LSP-specific HashSet, always allocated regardless of `lsp` feature. Cheap (empty until LSP hits it) but technically a leak. Could be feature-gated; marginal win. Not flagged.
- `sheaf` (lines 181-187) — `Arc<SheafState>`, always present. Sheaf ops are unconditionally compiled (`pub mod sheaf_ops;` in `mod.rs:14`, no cfg). Consistent with the substrate's "sheaf-cache as first-class" framing; consistent with the unconditional sheaf op entries in `tool_registry` and `base_op_names`.
- `live_db` (lines 113-133) — extensive docstring explaining `std::sync::Mutex` choice over `tokio::sync::Mutex`. Load-bearing reasoning; an example of disciplined documentation.
- `state` (lines 156-159) — `Arc<RwLock<DaemonState>>`. Needed because `cmd_daemon`'s background tasks (snapshot timer, etc.) and the run loop both mutate it.

**Verdict:** no dead fields, no rotting comments, no accretion that needs surgery. The 14-field count is high but each is load-bearing. Audit-clean.

## F3 — `ll-core` ⟶ `ll-open` inverse boundary is clean (noted)

For every crate in `rs/ll-open/`, Cargo.toml `[dependencies]` ll-core entries were enumerated and grepped for use sites in `src/`. Result (per Explore subagent enumeration):

| ll-open crate | ll-core deps declared | use sites | verdict |
|---|---|---|---|
| `cas-ffi` | `leyline-core` | 2 (`ContentAddressed::hash`) | essential |
| `chat-embed` | — | 0 | clean |
| `cli` | — | 0 | clean |
| `cli-lib` | `leyline-core`, `leyline-schema`, `leyline-schema-capnp`, `leyline-public-schema` | 14 | essential |
| `fs` | `leyline-core`, `leyline-schema` | 4 | essential |
| `hdc` | — | 0 | clean |
| `lsp` | `leyline-schema`, `leyline-schema-capnp` | 8 | essential |
| `sheaf` | — | 0 | clean |
| `sign` | — | 0 | clean |
| `text-search` | — | 0 | clean |
| `ts` | `leyline-schema` | 2 + re-exports | essential |
| `vcs` | `leyline-core` | 1 (`Controller`) | minimal-but-essential |

**Dead deps:** none. **Minimal deps:** `vcs` pulls only `Controller` (one use site in `src/lib.rs`), but `Controller` is the load-bearing arena abstraction — not a dependency-bloat candidate.

**Verdict:** every declared ll-core dep is actually used. The Tier 1 ⟶ Tier 2 inverse direction is clean. No `task tier:trim-inverse` gate needs to land.

## F4 — ADR-0014 §3 capnp pin language is stale (action — small)

ADR-0014 §3 declares an *exact* pin language: `capnpc = "=0.20.0"` exact (with a parenthetical "currently 0.20 semver-range — TIGHTEN"). Actual code in 2026-06-17:

| Crate | `capnp` | `capnpc` | `capnp-json` |
|---|---|---|---|
| `rs/ll-core/schema-capnp` | `=0.25.0` | `=0.25.0` | — |
| `rs/ll-core/public-schema` | `=0.25.0` | `=0.25.0` | `=0.1.0` |
| `rs/ll-open/cli-lib` | `=0.25.0` | (via deps) | `=0.1.0` |
| `rs/ll-open/lsp` | `=0.25.0` | (via deps) | — |

`Cargo.lock` resolves all to `0.25.0`. **Rust intra-workspace alignment is exact and consistent.** The ADR's text is out of date.

Go side: `clients/go/leyline-schema/go.mod` pulls `capnproto.org/go/capnp/v3 v3.1.0-alpha.2`. `mache`'s go.mod pulls the same alpha tag. The cross-runtime fixtures (`schema-capnp/tests/cross_runtime_fixtures.rs`, extended in PR #53 to cover cache.capnp) assert byte-equality between Rust and Go encoders — those tests currently pass with the alpha tag, but **alpha versioning violates the "exact deterministic" reproducibility goal** the ADR sets. Either bump to the stable 0.25.x line on Go (if available) or document the alpha as the deliberate cross-runtime pin.

**Action:** sub-bead — update ADR-0014 §3 with the actual pin (=0.25.0), and add a section on the Go alpha-pin decision (whether deliberate or a deferred-bump artifact per handoff bead). Small, focused doc fix; ~20 line delta.

## F5 — `chat-embed` / `text-search` / `agent-corpus`: zero code overlap, plan is sound (confirmed)

**chat-embed** (`rs/ll-open/chat-embed/src/main.rs`, 574 LOC) is a **binary-only** tool. No library crate. Three subcommands (`index`, `query`, `stats`). Reads `mache ingest claude-chats` SQLite output; embeds session intent via fastEmbed MiniLM-L6-V2 (384-dim); writes to sidecar `chat_embeddings` table; brute-force cosine top-k on query. No external importers — the workspace contains zero `use chat_embed::` sites.

**text-search** (`rs/ll-open/text-search/src/lib.rs`) is a trait with 8 methods (`upsert`, `remove`, `finalize`, `search`, `len`, `is_empty`, `clear`, `storage_path`). Two impls: `NullEngine` (returns `NotImplemented`, default in `DaemonContext`); `WitchcraftEngine` (XTR-WARP + BM25 hybrid, behind feature `engine-witchcraft`). Seven importers (4 production: `cmd_daemon.rs`, `daemon/mod.rs`, `daemon/ext.rs`, `daemon/ops.rs`; 3 test infrastructure files).

**agent-corpus** (planned per bead `ley-line-open-79a37c`) introduces `AgentSourceParser` trait + `Watermark` module + `pump_into_engine(parser, engine, watermark)` driver + `ClaudeCode` source impl. Lifted from Witchcraft's `pickbrain`. The bead's body explicitly notes: when this lands, `chat-embed` becomes either a thin CLI wrapper over `ClaudeCodeParser + WitchcraftEngine` or is **deleted entirely** (bead `ley-line-open-79fd04` tracks this decision).

**Overlap analysis:**

- *Code overlap*: zero. Different crates, different importers, no shared abstractions.
- *Design overlap*: real. Both chat-embed and (agent-corpus + WitchcraftEngine) do "index Claude Code sessions semantically + top-k query." The latter has the trait surface; the former is hardcoded.

**Verdict:** the existing plan (bead 79fd04) is sound. No new bead. When agent-corpus lands, the decision-tree is already in place: refactor chat-embed to a thin wrapper, or delete. The audit confirms there's no hidden coupling that would block either path.

---

## Action items

| # | Action | Priority | Estimated scope |
|---|---|---|---|
| 1 | Sub-bead: collapse `ops.rs` op-name 4-list to 1 source of truth (per F1) | P1 | small refactor, ~200 LOC delta + drift-test updates |
| 2 | Sub-bead: update ADR-0014 §3 capnp pin language (per F4) | P2 | doc fix, ~20 LOC delta |
| 3 | (rolled into existing 0.25.4 triplet deferred work): Go capnp v3.1.0-alpha.2 → stable decision | P2 | already deferred per handoff bead |

**Items deliberately not actioned:** F2, F3, F5 — audit-clean or covered by existing beads.

## Bibliography

- `rs/ll-open/cli-lib/src/daemon/mod.rs:109-188` — DaemonContext struct
- `rs/ll-open/cli-lib/src/daemon/ops.rs:27,47,151,190` — four op-name structures
- `rs/ll-open/cli-lib/src/daemon/ops.rs:2587-2600,3018-3022` — drift tests
- `rs/ll-open/cli-lib/src/daemon/mcp.rs:61,882` — MCP tool registry + drift test
- `docs/adr/0014-capnp-as-protocol.md` §3 — stale pin language
- `rs/ll-open/text-search/src/lib.rs:96-141` — TextSearchEngine trait
- `rs/ll-open/chat-embed/src/main.rs` — binary tool
- Beads: `ley-line-open-79a37c` (agent-corpus), `ley-line-open-79fd04` (chat-embed refactor/delete), `ley-line-open-5f92fa` (handoff bead that queued this audit)
