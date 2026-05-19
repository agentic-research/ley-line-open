# ADR-0020 — Observation flow over a learned CellComplex

**Status:** Proposed (2026-05-19)
**Decade:** `ley-line-open-9d30ac` (Σ Merkle-CAS substrate) — consumer-facing layer, parallel to ADR-0016
**Thread:** `T10/observation-lattice`
**Bead:** `ley-line-open-8bf731`

**Sibling artifacts:**

- `docs/adr/0014-capnp-as-protocol.md` — wire encoding (capnp-json over UDS / MCP HTTP). Settled.
- `docs/adr/0015-lazy-on-access-ingestion.md` — when ingestion happens. Settled.
- `docs/adr/0016-ai-native-query-surface.md` — agent-shaped query ops. Settled.
- `rs/ll-open/sheaf/` — `leyline-sheaf` ships `CellComplex`, `CoChangeTracker`, `SheafCache`, δ⁰ machinery. **Load-bearing**: this ADR's substrate uses the engine for its designed purpose (numerical δ⁰ over a learned complex of co-changing entities), not as decorative citation.
- `rs/ll-open/cli-lib/src/daemon/enrichment.rs` — `EnrichmentPass` chain. New passes emit observation rows.
- `ley-line-open-503971` — typed payload registry (capnp schemas keyed by `payload_kind`).
- `ley-line-open-783d72` — `BlobStore` trait + default. Observation bodies above an inline threshold live here, referenced by BLAKE3 hash.
- `rosary-7023a9` — observation-lattice substrate (parent design in rosary).

---

## Context

### What changed (and why this ADR is short)

A prior draft of ADR-0020 hand-authored a much larger model: an `entity` table with hierarchical addressing (`org:repo:src:PATH:sym:NAME@OFFSET`), three named restriction policies (Atomic / Inherited / Local), three reference lenses (`code_archaeology` / `developer` / `em`), an unhomed-observation re-homing lifecycle, and a sheaf-on-a-lattice framing that cited `leyline-sheaf` as the math layer.

Three review passes (`theoretical-foundations-analyst`, `paradigm-assessor`, author self-scrutiny) converged on three corrections:

1. **The sheaf-on-a-lattice framing was rhetorical.** The cited engine (`CellComplex`, `δ⁰`) operates on `Vec<f32>` stalks; the prior draft stored opaque capnp BLOBs with no feature-encoder bridge. The math citation could not be honored mechanically.
2. **Byte-offset addressing made `cargo fmt` fatal.** Hand-authored `sym:NAME@BYTE_OFFSET` ids change on any whitespace edit, orphaning every prior observation in a file because the re-homer's exact-id / mention-resolution policies don't recover homed-then-orphaned rows.
3. **Lenses, restriction policies, and entity addresses were prescription ahead of pressure-test.** No in-tree consumer was demanding `lens.developer` or `lens.em`. The "three restriction policies" enumeration was a hand-authored taxonomy without a workload forcing it.

The deeper reframe (user, 2026-05-19): **we don't manually author this.** `leyline-sheaf` already ships `CoChangeTracker`, which *learns* restriction edge weights from observed co-change patterns. The substrate's structural layer should be **built from the observation flow**, not specified ahead of it. That dissolves all three corrections: the framework becomes load-bearing (the observation stream is the bridge to `CellComplex`); addressing becomes whatever-the-observer-wrote (no hand-spec'd grammar to break); and lenses become *queries* against the learned complex, not first-class APIs.

This ADR captures the small substrate that the reframe permits.

---

## Decision

### 1 — Storage: one table

```sql
CREATE TABLE observation (
    id            INTEGER PRIMARY KEY,
    source        TEXT NOT NULL,             -- "tree-sitter" | "git" | "claude-code" | "agent-edit" | ...
    payload_kind  TEXT NOT NULL,             -- capnp schema name from ley-line-open-503971 registry
    payload_inline BLOB,                     -- inline capnp bytes when small (< INLINE_THRESHOLD)
    payload_hash  BLOB,                      -- BlobStore BLAKE3 hash when large; NULL if inline
    mentions      TEXT NOT NULL,             -- JSON array of stable tokens this observation references
    observed_at   INTEGER NOT NULL           -- epoch ms (event time, not insert time)
);

CREATE INDEX observation_by_kind     ON observation(payload_kind, observed_at DESC);
CREATE INDEX observation_by_mentions ON observation(mentions);  -- json_each lookup
```

**No `entity` table.** No `parent` foreign key. No hand-authored address grammar. The "entity set" is the projected union of every `mentions` token ever observed.

**Stable token formats emerge from observers**, not from this ADR. Today's observers already use stable forms: tree-sitter cites `repo-relative-path:sym:NAME`, git cites commit SHAs, rosary cites `bead:ID`, claude-code cites `session:UUID:turn:N`. If a future workload surfaces ambiguity (two distinct symbols sharing a token), the observer that introduces the ambiguity also fixes it — by emitting a richer token (e.g., a content-addressed AST-shape hash). The ADR does not pre-spec the disambiguation rule because no current observer needs one.

**Inline vs hash placement.** `payload_inline` for capnp payloads below `INLINE_THRESHOLD` (proposed: 4096 bytes); `payload_hash` referencing `BlobStore` (ley-line-open-783d72) for larger. Threshold is a tunable, not a wire contract.

**`payload_kind` is the registry key** to ley-line-open-503971's capnp typed-payload registry. Schema migration is type-registry work; the table itself is opaque to payload kind.

### 2 — Structure: learned, not authored

A daemon enrichment pass (`ComplexBuildPass`, new) periodically:

1. Scans `observation` rows since the last pass run.
2. Constructs `CellComplex` nodes from unique mention-tokens.
3. Constructs edges from **co-occurrence in observations**: when an observation's `mentions` array cites two tokens, an edge between those tokens is reinforced. The edge weight is a function of co-occurrence frequency + source diversity (a single source emitting the same co-occurrence repeatedly weighs less than two independent sources agreeing).
4. Feeds the complex to `CoChangeTracker::observe(invalidated, edges)` — already in `leyline-sheaf` — to learn weights from temporal co-change.
5. Snapshots via `SheafCache`'s existing generation machinery.

This is what `CellComplex` was designed for: a sparse structural representation with restriction edges whose weights are observable from the data. `CoChangeTracker` is the existing mechanism that does the observing. The bridge that the prior draft's review found missing — `payload_kind → Vec<f32>` feature map per observation — is replaced by **the observation stream itself**: nodes and edges are derived from `mentions` co-occurrence, not from a hand-authored feature encoder.

### 3 — Query surface: three primitives

```capnp
struct NeighborhoodRequest { token @0 :Text;  k @1 :UInt32; }
struct NeighborhoodResponse { hits @0 :List(Hit); }  # (token, weight, hops) tuples

struct AgreementRequest { token @0 :Text;  payload_kind @1 :Text; }
struct AgreementResponse { observations @0 :List(Observation);  defects @1 :List(Defect); }

struct CoChangedRequest { token @0 :Text;  window_ms @1 :UInt64; }
struct CoChangedResponse { peers @0 :List(Peer); }  # tokens that co-changed, with weights
```

- **`neighborhood(token, k)`** — k-hop neighbors in the learned complex, ranked by edge weight. Built on `CellComplex` traversal.
- **`agreement(token, payload_kind)`** — observations on this token from all sources for the given payload kind, with disagreement scores between sources. Disagreement is computed via `CellComplex::detect_violations` against a degenerate two-node complex (one node per source, identity restriction maps) — same engineering trick paradigm-assessor named, now made explicit. The term "δ⁰" is reserved for the cochain operator in `leyline-sheaf`; this op's disagreement field is `coherence_defect`, not δ⁰.
- **`co_changed_with(token, window)`** — direct query against `CoChangeTracker`'s learned edge weights, filtered to the requested time window.

**No `lens.developer`, no `lens.em`, no `lens.code_archaeology` as first-class ops.** Useful query patterns emerge as compositions of the three primitives; if any composition repeats often enough to be worth promoting to a stable op, it gets its own bead at that time. Today's three primitives are the minimum surface.

---

## Falsifiability gates

The ADR is "done" when these four gates are met. Until then it stays Proposed and downstream substrate beads (`79a37c`, `79c6ab`, `79fd04`) run against the table shape in §1 only.

1. **One pass writes observations.** Implement the session enrichment pass (ley-line-open-79a37c) that walks Claude Code JSONLs and emits observation rows with `payload_kind = "agent.session_turn"` and `mentions` populated with cited paths/symbols/beads. Fixture test: ingest a fixture session, verify row count + `mentions` extraction.
2. **`ComplexBuildPass` invokes `CellComplex`.** The pass takes a small fixture of observations (5–10 rows referencing 3–4 distinct tokens), builds a `CellComplex`, hands it to `CoChangeTracker::observe`. Test: complex is constructed, edges reflect co-occurrence, `CoChangeTracker` updates internal weights. The test fails if the code path doesn't actually call into `leyline-sheaf::CellComplex` — proves the "math is load-bearing" claim mechanically.
3. **`agreement(token, payload_kind)` computes coherence_defect via `detect_violations`.** Test: insert two `code.symbol_def` observations on the same token from different sources with disagreeing fields. Verify the `agreement` op returns a non-empty `defects` field whose computation passes through `CellComplex::detect_violations` against a degenerate 2-node complex.
4. **Property test for the typed-payload registry.** For each registered `payload_kind`, a `proptest!` fixture generates random payloads and verifies round-trip through inline-and-hash encoding. New payload kinds added without a property-test fixture fail the build. Aligns ley-line-open-503971's registry with the substrate.

When all four gates are met, the ADR moves Proposed → Accepted.

---

## What this ADR does NOT settle

- **The dispatch cadence for `ComplexBuildPass`.** Cron-style daemon pass vs query-time materialisation vs both — decided by the first consumer exercising the surface, not pre-authored.
- **Cross-machine federation** (any `org:user:` tokens shared between machines). Out of scope; this ADR is per-machine. Notme.bot territory.
- **Lens-style aggregations** (`developer` / `em` / `code_archaeology`). Not first-class APIs in this ADR. If a query pattern composes neighborhood + agreement + co_changed_with repeatedly, it gets a follow-up bead at that point — not before.
- **Address-disambiguation rules** for tokens. Today's observers use stable forms; the moment one introduces ambiguity, that observer fixes it. The ADR refuses to pre-author a disambiguation grammar.
- **Re-homing of orphaned tokens** (the `cargo fmt` failure mode the prior draft tripped over). Under this ADR there is no entity_id to orphan — observations reference whatever tokens the observer cited. If a tree-sitter pass later finds that `sym:foo` at byte 1240 is now at byte 1450, that's not a re-homing problem; it's the tree-sitter pass emitting a fresh observation with the new byte range. The two observations of `sym:foo` coexist; lens queries see both.

---

## What the prior draft got wrong (for the record)

Recoverable from git history if a future reader wants the long form. Headline corrections:

| Prior draft | Correction |
|---|---|
| `entity` + `parent` tables with hand-authored addressing | One `observation` table; tokens are observer-emitted strings |
| Three restriction policies (Atomic / Inherited / Local) as an enumeration | No prescribed policies; restriction structure is what `CoChangeTracker` learns from co-change |
| "Sheaf on a lattice" framing with H¹ / cohomology vocabulary | Honest small claim: numerical δ⁰ on a degenerate complex for coherence_defect; the learned complex is the genuine `CellComplex` consumer |
| Three reference lenses shipped in ADR | No lenses in ADR. Three primitives (`neighborhood`, `agreement`, `co_changed_with`); compositions emerge as needed |
| `sym:NAME@BYTE_OFFSET` disambiguation rule | No rule. Observers emit whatever stable token works; ambiguity is the observer's problem if it arises |
| Unhomed-observation re-homing lifecycle with LLM fallback | No re-homing. Observations are immutable claims by sources; no entity_id to orphan |

The deletion of `lens.developer`, `lens.em`, the restriction-policy enumeration, and the addressing grammar shrinks the ADR from ~800 lines to ~200. The substrate it specifies is materially smaller and survives the failure scenarios the prior draft tripped over.
