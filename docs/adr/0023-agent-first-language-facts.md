# ADR-0023 — Agent-first language facts: analyzer-as-library, not LSP-wire

**Status:** Proposed (2026-06-24)
**Bead:** (to be filed on acceptance)
**Related:** ADR-0014 (capnp-as-protocol), ADR-0015 (lazy-on-access-ingestion), ADR-0016 (AI-native query surface), ADR-0020 (entity-observation-lattice); cloister LSP removal (`src/backends/lsp.ts`, `src/tool-schemas/lsp.ts`, `test/lsp-backend.test.ts`)

---

## Context

The current LLO ingestion pipeline produces language-level facts by running per-language LSP servers (rust-analyzer, gopls, pyright, etc.), normalizing their wire-protocol responses, and writing the normalized records into the Σ substrate as `BindingRecord` rows. mache reads those rows and exposes agent-facing queries via MCP (`find_definition`, `find_callers`, `get_type_info`, `find_callees`, `get_overview`, `get_architecture`, `get_communities`, `get_impact`, `find_smells`, plus the write surface).

This architecture has two layers of mismatch:

**Mismatch 1 — LSP wire is editor-shaped.** The Language Server Protocol was designed for one human-paced editor talking to one analyzer server. Its message shapes carry editor-tuned assumptions: hover responses are markdown strings sized for tooltips; diagnostics are severity-tagged for squiggly underlines; references are paginated for "find usages" panels; code actions assume a menu render. None of that shape is wrong for editors. All of it is lossy when the consumer is an agent that wants a structured graph at a snapshot.

**Mismatch 2 — analyzers compute far more than LSP exposes.** rust-analyzer's salsa-based incremental DB tracks fine-grained dependency edges between every name resolution, type inference, and trait lookup; the LSP wire surface exposes none of that graph, only per-cursor query results. gopls runs `go/analysis` analyzers and a packages graph internally; the LSP wire surface exposes only the editor-visible subset. pyright maintains a full type-checker graph; LSP returns hover strings and diagnostic messages. The wire protocol is structurally a per-cursor request/response over the analyzer's much richer internal state, and consuming that protocol is at best ~20% of what the analyzer already computed.

The downstream consequence is that LLO's BindingRecord schema is the union of what each language's LSP wire happens to expose, and mache's MCP tools are correspondingly shaped by the lowest-common-denominator LSP shape. Adding agent-native queries (community-aware search, callgraph reachability, type-graph traversal, change-impact analysis at the salsa-edge level) requires inventing a parallel ingestion path per language, because the LSP wire doesn't carry the underlying graph data.

Cloister already removed its LSP backend (`src/backends/lsp.ts`, etc., deleted in recent commits). LLO is currently the only repo still consuming LSP wire protocol responses as a first-class data source. That status is an artifact of incremental migration, not a deliberate design choice.

### What this ADR is NOT

This is **not** an "implement our own Language Server Protocol" proposal. The LSP wire format is fine for what it was designed for (editor-paced human interaction) and we have no interest in replacing it for editor use cases. This ADR proposes a different category of integration: **link the analyzer as a library and consume its internal graph data directly**, bypassing the editor-shaped wire protocol entirely.

LSP servers remain a legitimate ingestion source for languages whose analyzers we cannot link as libraries (the "tier 2" path below). We are not deprecating LSP; we are demoting it from primary to fallback.

## Decision

LLO adopts a three-tier ingestion model for language facts. Each language is placed in exactly one tier:

### Tier 1 — analyzer-as-library (primary)

For languages whose analyzer ships as a consumable library, LLO/mache imports the analyzer and emits ACI-shaped records directly into the Σ substrate. No LSP wire protocol, no JSON-RPC, no editor abstractions in the path.

The contract:

- The analyzer is depended on at a pinned version (semver-locked for stability; analyzer internals are not API-stable).
- The integration extracts the structured graph the analyzer already computed (packages graph, type graph, callgraph, salsa edges, name-resolution tables).
- The integration emits new capnp record kinds (extension of `BindingRecord`) that mache consumes.
- The integration runs in the same process tier where the analyzer's native runtime lives: Go analyzers in mache (Go), Rust analyzers in LLO (Rust), JS-based analyzers via a Node sidecar.

### Tier 2 — LSP wire (fallback)

Languages we cannot integrate at the library level run their LSP server and consume the wire protocol the same way LLO does today. Records are written into the same capnp schema as tier 1, with explicit provenance marking ("source=lsp-wire") so agents can reason about which facts are graph-complete and which are editor-shaped.

### Tier 3 — tree-sitter only

The existing tree-sitter pass remains the universal floor. Any language LLO can parse with tree-sitter gets structural facts (AST shape, identifier extraction, comment association) regardless of analyzer availability. Tier 3 facts are emitted unconditionally for all supported languages; tier 1/2 facts layer on top when the analyzer is available.

## Per-language scope and phasing

Priority order is driven by the user's day-to-day use cases (Go and Terraform are the active day-job surface; Rust is the tooling layer; TypeScript is downstream consumer territory).

### Phase 1 — Go (tier 1, mache-side)

Go's analyzer ships as standard-library + `golang.org/x/tools` packages, directly importable from mache:

- `go/packages` — load a module, get the full packages graph with type-checked syntax trees.
- `go/analysis` — run any `Analyzer` in the public analyzer set against the loaded packages. The `passes/*` directory contains the analyzers gopls runs internally.
- `go/types` — full type information per package, traversable.
- `go/ast` + `go/token` — AST with position info.

mache already imports these packages for some of its `find_callers` / `find_definition` implementations. Phase 1 expands that consumption to emit ACI-shaped records covering:

- Module/package graph with import edges
- Type graph (struct fields, method sets, interface satisfaction edges)
- Full callgraph at the module level
- `go/analysis` findings as first-class records (not LSP diagnostics strings)
- Vendoring + go.work + build-constraint resolution at snapshot

Implementation home: **mache** (Go). The records are written into the same SQLite projection LLO emits today; the schema gets `_go_*` extension tables for facts LLO's current BindingRecord can't carry.

### Phase 2 — Terraform (tier 1, mache-side)

Terraform's analyzer is less library-friendly than Go's, but the pieces exist:

- `github.com/hashicorp/hcl/v2` — HCL parser, library-consumable, stable.
- `github.com/hashicorp/terraform-config-inspect` — lightweight inspection of module structure (resources, variables, outputs, providers) without running plan.
- For deeper analysis (variable resolution across modules, provider-schema-aware resource graphs, plan-time dependency edges), the options are:
  - **(a)** Vendor parts of `hashicorp/terraform-ls` internals (the modules indexing layer is the relevant subset). High churn risk but rich data.
  - **(b)** Drive `terraform` CLI with `-json` output for plan/graph commands and parse. Stable interface, requires plan run.
  - **(c)** Build a focused HCL+provider-schema evaluator that doesn't depend on terraform-ls or terraform-core. Most work, most stability.

Phase 2 ships with HCL + terraform-config-inspect for structural facts immediately. The deeper analysis (option a/b/c) is its own follow-up decision once the structural surface is in production.

Records to emit:

- Module hierarchy (root, called modules with source URLs and versions)
- Resource/data declarations with `for_each`/`count` markers
- Variable + output graph (declarations and resolutions where statically determinable)
- Provider declarations and pinned versions
- Module-call argument graph (which variables flow to which child modules)

Implementation home: **mache** (Go). HCL is Go-native.

### Phase 3 — Rust (tier 1, LLO-side)

rust-analyzer publishes its internals as `ra_ap_*` crates on crates.io (`ra_ap_ide`, `ra_ap_hir`, `ra_ap_ide_db`, etc.). LLO can depend on `ra_ap_ide` directly:

- Salsa DB exposing incremental type/name-resolution edges
- Full HIR with trait resolution
- Cross-crate name resolution table (queryable, not per-request)
- Inlay-hint data with origin metadata (the inference reason, not just the rendered string)
- Borrow/lifetime analysis structured facts

Caveats:

- `ra_ap_*` versions track rust-analyzer releases; the API is not stability-guaranteed. We pin to a known version and accept periodic upgrade work.
- The salsa DB is heavy at process startup. LLO needs a long-lived analyzer process per workspace, not per-request.
- License: `ra_ap_*` is MIT/Apache-2.0 (same as rust-analyzer). Compatible with LLO's mixed license model.

Records to emit:

- Crate dependency graph
- Trait implementation graph
- Type graph at module granularity
- Salsa-edge-derived "what changes if I edit X" answers (structural, not just textual)

Implementation home: **LLO** (Rust). New crate `rs/ll-open/lang-rust/` that vendors the `ra_ap_*` consumer.

### Phase 4 — TypeScript (tier 1, sidecar)

The TypeScript Compiler API (`typescript` npm package) is the analyzer, exposed as a JavaScript library:

- Full AST + type-checker
- Project graph (tsconfig references and includes)
- Symbol resolution across the project
- Import graph

We have two options for hosting:

- **(a)** Node sidecar — a small Node.js process LLO spawns that imports `typescript` and emits ACI records over a UDS or shared file. Pros: directly use the Compiler API; stable interface. Cons: introduces a Node runtime dependency for LLO users on TS projects.
- **(b)** SWC (Rust) — the SWC parser is a fast Rust port of TS's parser, library-consumable from LLO. Pros: no Node dependency, native Rust. Cons: SWC has parser-level semantic analysis but not a full type-checker; tier-1 coverage of TS type facts is reduced.

Phase 4 picks (a) Node sidecar as the wedge; (b) SWC remains an option if the Node runtime dependency becomes a deployment concern.

Implementation home: **LLO** spawns the sidecar; sidecar is a separate small package shipped alongside the binary.

### Tier-2 holdovers (not in scope for this ADR)

Python, Ruby, Java, C/C++, and any language whose analyzer is not library-consumable stay on the LSP-wire ingestion path. They get tier-3 tree-sitter floor coverage immediately; tier-1 upgrade is a per-language follow-up ADR if and when an analyzer becomes library-extractable.

## Substrate emission format

The Σ substrate gets new capnp record kinds, sibling to `BindingRecord`. Sketch:

```capnp
struct LangFact {
  source @0 :Source;            # which tier / which analyzer / which version
  snapshot @1 :Data;            # the rootHash at which this fact was computed
  kind @2 :Kind;
  payload @3 :Data;             # tier-specific structured payload
  union {
    typeFact @4 :TypeFact;
    callFact @5 :CallFact;
    importFact @6 :ImportFact;
    moduleFact @7 :ModuleFact;
    ...
  }
}

enum Source {
  treeSitter @0;
  lspWire @1;
  goPackages @2;
  hcl @3;
  raApIde @4;
  tsCompiler @5;
}
```

The exact shape is a v0.1 design subject to revision as Phase 1 ships. What matters for this ADR is the principle: facts carry their source tier explicitly, so agents (and mache's query layer) can reason about which facts are graph-complete vs editor-shaped.

The existing `BindingRecord` schema stays as-is for backward compatibility. `LangFact` is additive.

## Consequences

### What this buys us

- **Agents get analyzer-native data**, not editor-reshaped data. Salsa edges, package graphs, type graphs become first-class queryable.
- **New MCP tools become possible** that weren't possible on the LSP wire: `get_callgraph_at_snapshot`, `find_impact_via_salsa`, `find_trait_implementors`, `find_type_uses_across_crates`.
- **Provenance is explicit.** Every fact knows what tier produced it; agents can rank or filter accordingly.
- **LSP becomes a fallback**, not a critical path. Languages we don't fully integrate still work via tier 2.

### What this costs

- **Vendor lock-in per analyzer.** rust-analyzer's `ra_ap_*` API changes between releases; we accept periodic upgrade work.
- **Runtime weight.** Tier-1 analyzers (especially salsa DB) want long-lived processes. LLO's process model grows.
- **Build complexity.** Phase 3 adds a heavy Rust dependency (rust-analyzer's crate graph is large). Phase 4 introduces a Node sidecar.
- **Per-language schema growth.** Each tier-1 integration adds extension records. Schema discipline matters more.

### Migration path

This ADR does **not** remove the existing LSP-wire path. The order is:

1. Ship tier-3 tree-sitter coverage for Go, Terraform, Rust, TypeScript (Phase 0 — already largely done in LLO; gap-fill as needed).
2. Phase 1: Go analyzer-as-library lands as additive ingestion. LSP-Go remains running in parallel for validation.
3. Once Phase 1 is stable and mache's queries prefer tier-1 facts for Go, retire LSP-Go.
4. Same pattern for Phases 2, 3, 4.

Each retirement is its own commit/ADR-amendment, not a Big Bang switch.

## Rejected alternatives

### "Build our own LSP" (NIH a wire-protocol replacement)

Rejected. The LSP wire protocol is editor-shaped by design; replacing it with an "agent LSP" wire would either (a) reproduce LSP's request/response shape with different message names (cosmetic), or (b) require inventing a graph-shaped wire protocol whose only consumer is mache (no ecosystem). Library-level integration with existing analyzers gets us the graph data without inventing a protocol no one else speaks.

### "Run LSP wire and re-shape harder downstream"

Rejected. The fundamental data isn't on the wire; no amount of downstream reshaping recovers what the LSP wire doesn't send. rust-analyzer's salsa edges are computed and discarded before they reach the wire. We can't reshape what was never serialized.

### "Tree-sitter only; skip analyzers entirely"

Rejected. Tree-sitter is the right floor (and stays in tier 3), but it's structural-only. Type resolution, name resolution, trait satisfaction, callgraph — none of those are reachable from tree-sitter alone. Agents that need semantic facts (impact analysis, refactoring planning, type-aware search) need analyzer-level data.

### "Stick with LSP wire for everything; add agent-native MCP tools that hide the limitation"

Rejected. This is the status quo, and it's why `find_callers` is paginated, `get_type_info` returns markdown strings, and there's no `get_callgraph_at_snapshot` tool. The wire is the bottleneck.

## Open questions

- **Analyzer version drift.** When rust-analyzer ships a breaking `ra_ap_*` change, what's the upgrade cadence? Quarterly? On-demand? Pinned to a specific Rust toolchain? Phase 3 needs an upgrade-policy answer before it merges.
- **Snapshot consistency across tiers.** Tier-1 analyzers maintain their own incremental state. The Σ substrate has its own snapshot/rootHash discipline. How do we reconcile a salsa-DB snapshot with the substrate's rootHash? Probably: the analyzer process re-runs against the substrate-pinned source content, salsa caches what it can, and the fact's `snapshot` field names the substrate rootHash, not the analyzer's internal version. To be confirmed in Phase 3 implementation.
- **Per-workspace vs per-project analyzer process.** rust-analyzer wants a long-lived process per workspace. Multiple workspaces open simultaneously → multiple processes → memory cost. Out of scope for this ADR; tracked as a Phase 3 implementation concern.
- **Terraform plan-time facts.** Phase 2 ships without plan-time data. If agent queries need "what will happen if I apply this change" answers, that's a fundamentally different ingestion (running `terraform plan`) and probably its own ADR.
- **Schema evolution.** `LangFact` is sketched here; the real schema needs design once Phase 1 emission lands. The capnp evolution rules (additive fields only, no renumbering) constrain what we can do mid-flight.

## Acceptance criteria

- Phase 1 (Go) ships with at least three agent-native query types that weren't available on the LSP-wire path (e.g. full module callgraph at snapshot, `go/analysis` finding records, package import graph).
- mache's MCP tool surface gains corresponding queries that prefer tier-1 facts when available.
- LSP-Go ingestion runs in parallel for one release cycle for validation; retire once tier-1 covers the same surface.
- Tier-1 facts are clearly tagged in the substrate; agents can filter on source tier.

Phase acceptance for Terraform/Rust/TypeScript is defined in their respective implementation PRs; this ADR records the framework, not the per-phase exit criteria.
