# ADR-0026 — Unified code-fact IR: the producer contract

**Status:** Proposed (2026-07-06)
**Bead:** `ley-line-open-7ed023`
**Related:**
- mache ADR-0023 (Unified Code-Fact IR — the consumer/design side; this ADR is its producer mirror)
- ADR-0014 (capnp-as-protocol — the `be6136` post-mortem this generalizes; `set_root_canonical`, `head.capnp` chain)
- ADR-0016 (AI-native query surface — the query surface that sits on this IR)
- `ley-line-open-be6136` (the one-byte path-mismatch JOIN-miss incident that motivates the content-addressed key)

---

## Context

mache ADR-0023 defines a **unified code-fact IR** — a property graph over a
content-addressed symbol set, encoded as two SQLite tables (`symbols`,
`fact_edges`) plus a narrow attribute table, keyed on a parse-run-invariant
`symbol_id`. mache is the consumer: it re-expresses its `v_refs`/`v_defs` smell
views as plain SQL over `fact_edges` and materializes containment closure.

**`leyline parse` is the only place that can produce it.** The parse pass in
`rs/ll-open/cli-lib/src/cmd_parse.rs` is where the AST, the source bytes, and
(via later enrichment) the LSP graph are all in hand. Per ADR-0023's
implementation strategy, the producer is the critical path: "nothing downstream
works until the tables exist." This ADR is the producer-side contract mache
ADR-0023 explicitly asks for ("this ADR needs a ley-line-open ADR mirror … the
0013/0014 pattern … before mache builds a reader").

The `be6136` incident is the load-bearing motivation. A one-byte
canonicalization mismatch in `_source.path` made every `_lsp_refs × _ast` JOIN
miss silently — the SQL schema was acting as a cross-process protocol with a
mutable, path-shaped join key and no integrity constraint. ADR-0014 fixed the
binding arm by moving it onto typed capnp; the *join-key fragility* remains
wherever facts relate by path-shaped strings. This ADR removes that fragility
by construction for the new IR.

---

## Decision

`leyline parse` materializes two new SQLite tables in the **same parse pass**,
in the **same transaction** as the existing `_ast`/`node_defs`/`node_refs`
projections, keyed on a content-addressed `symbol_id`, with referential
integrity enforced at write time.

### 1. `symbol_id` — the content-addressed join key (the `be6136` cure)

```
symbol_id = BLAKE3( source.contentHash ‖ canonical_span ‖ kind ‖ name )
```

- `source.contentHash` — BLAKE3-32 of the file bytes (`source.capnp`, field
  `contentHash @3`). **This ADR wires it**: the contentHash was previously left
  empty at `cmd_parse.rs` (the "T8.5 wires BLAKE3" TODO). It is now computed
  in-worker from the bytes already read for parsing, populated into `_source`,
  and fed into `symbol_id`. This lands as its own reviewable commit.
- `canonical_span` — the node's byte range (`start_byte`, `end_byte`),
  little-endian `u64` pair. Byte offsets are content-relative, not path-relative.
- `kind` — the **canonical** cross-language kind (κ below), not the raw
  tree-sitter kind.
- `name` — the symbol's identifier text, or empty for anonymous nodes.

**The path MUST NOT enter the key.** This is the `be6136` fix stated as an
invariant: because the fragile, mutable, path-shaped string is absent from the
content address, the `be6136` class of silent JOIN miss cannot recur by
construction. Two consequences fall out:

1. **Parse-run invariance.** An unchanged file yields byte-identical
   `symbol_id`s across parse runs. The IR is diffable and generation-keyed
   materialization is cheap.
2. **`node_id` is demoted to a locator.** The existing slash-path `node_id`
   (`_ast.node_id`, e.g. `main.go/function_declaration`) remains the
   human/tool-facing address and the parse-run-local pointer into `_ast`. It is
   **never a cross-fact join key.** Resolution `node_id → symbol_id` happens
   once, here, at parse time.

**Caveat (stated explicitly, mirroring ADR-0023):** content-addressed identity
*changes when bytes change*. "The same function across edits" is therefore a
**separate `lineage` edge** produced by a future diff pass, not `symbol_id`
equality. `symbol_id` is not overloaded to mean identity-over-time.

### 2. Fail-loud referential integrity

- `fact_edges.src` and `.dst` are declared `REFERENCES symbols(symbol_id)`.
- `PRAGMA foreign_keys = ON` is set for the parse transaction. A dangling edge
  is therefore an **insert error in the producer**, not a silently-zeroed row in
  the consumer. `be6136` would have failed the parse loudly instead of degrading
  a downstream query to zero rows.
- When a ref legitimately cannot be resolved (cross-repo target, file deleted
  mid-pass), the edge is emitted with `dst = NULL` (allowed) and the
  `unbound_facts` counter is incremented.

### 3. `unbound_facts` counter in `head.capnp`

A new field on `Head` (appended at the next free ordinal, per the ADR-0014
additive-evolution contract — unset stays byte-stable under canonical encoding):

```capnp
# Count of fact_edges emitted this generation with dst = NULL — a ref the
# producer could not resolve to a symbol. Monotone per generation; the W5
# ratchet gate asserts unbound_facts <= baseline.
unboundFacts @4 :UInt64;
```

"N facts failed to bind this generation" becomes a first-class, monotonic
number. `be6136` would have surfaced as this counter jumping to ~100% — a red
gate on the first CI run.

### 4. The schema

```sql
CREATE TABLE symbols (
  symbol_id   BLOB    NOT NULL,   -- BLAKE3(contentHash ‖ span ‖ kind ‖ name)
  gen         INTEGER NOT NULL,   -- parse generation (head.capnp clock)
  source_id   TEXT    NOT NULL,   -- repo-relative path (locator, not key)
  node_id     TEXT    NOT NULL,   -- parse-run locator into _ast (NOT a join key)
  kind        TEXT    NOT NULL,   -- CANONICAL cross-language kind (κ)
  raw_kind    TEXT    NOT NULL,   -- tree-sitter kind (function_declaration/…)
  lang        TEXT    NOT NULL,
  name        TEXT,
  span_start  INTEGER NOT NULL,   -- canonical byte offsets
  span_end    INTEGER NOT NULL,
  PRIMARY KEY (symbol_id, gen)
);

CREATE TABLE fact_edges (
  src        BLOB    NOT NULL REFERENCES symbols(symbol_id),  -- FK = fail-loud
  dst        BLOB             REFERENCES symbols(symbol_id),  -- NULL = unbound (counted)
  kind       TEXT    NOT NULL,   -- contains|calls|references|defines|binds|imports|has_type|lineage
  fidelity   TEXT    NOT NULL,   -- mention|binding|reachability
  gen        INTEGER NOT NULL,
  token      TEXT,               -- lemma at the ref site (mention arm)
  qualifier  TEXT,               -- selector LHS (binding arm)
  span_start INTEGER, span_end INTEGER
);
```

Both tables are written inside the existing parse transaction, after the
`_ast`/`node_defs`/`node_refs` batches, so their inputs are all in hand.

### 5. Source-to-edge mapping (this pass)

| Source projection | `symbols` / `fact_edges` emission | fidelity |
| --- | --- | --- |
| `_ast` node | one `symbols` row; `contains` edge parent→child | mention |
| `node_defs` | `defines` edge (def-site symbol → named symbol) | mention |
| `node_refs` | `references`/`calls` edge, `token` set, `dst` resolved or NULL | mention |

`fact_edges` is `v_refs` generalized: mache's
`v_refs ≡ SELECT … FROM fact_edges WHERE kind IN ('calls','references','binds')`.
The `fidelity` discriminator that was a runtime UNION in mache's TEMP view lifts
to a materialized column here.

**Deferred to a follow-up (out of scope for this producer slice):**

- **`binds` edges** (`fidelity='binding'`, resolved `dst`, `qualifier` set).
  Binding events are produced by the LSP-enrichment path
  (`rs/ll-open/lsp/…`), not the parse pass. They layer on once that path is
  traced.
- **`has_type` / `lineage` / `imports`** edge kinds — later passes.
- **`symbol_attrs`** (EAV for open LSP metadata: hover, diagnostics) — enrichment.

### 6. `κ` — cross-language kind collapse

`κ: (lang, raw_kind) → kind` lives in the language registry
(`rs/ll-open/ts/src/languages.rs`), mirroring mache's `internal/lang` single
source of truth. Base kinds (closed set): `function`, `method`, `type`, `field`,
`variable`, `constant`, `module`/`file`, `import`, `parameter`. Examples:
Go `function_declaration`, Rust `function_item`, Python `function_definition` →
`function`. Anything unmapped keeps `kind = raw_kind` (open-world escape hatch);
`raw_kind` is always retained so language-specific rules can still discriminate.

This producer slice ships κ for the actively-used grammars and the base-kind
collapse; full κ coverage across every registered language is a follow-up
per-language review surface (same cadence as `internal/lang`).

---

## Consequences

**Positive:**

- `be6136`'s failure *class* is closed by construction (path never enters the
  key) and made loud (FK integrity + `unbound_facts`), not silent.
- One cross-language, DSL-free query surface materialized at parse time;
  consumers become trivial indexed SQL (ADR-0023's "one hard producer, many easy
  consumers" — the same trade ADR-0014 made with capnp).
- The `contentHash` TODO from T8.5 is closed as a side effect.

**Costs (named):**

- **Storage roughly doubles** — the normalized IR is stored alongside raw
  `_ast`/`node_refs`. Bytes for latency; the "materialized not views" trade.
- **Producer complexity grows** — parse now computes `symbol_id`, runs κ,
  resolves-or-counts refs, and enforces FK integrity. This is the intended
  direction.
- **Cross-gen tracking is explicit** (a future `lineage` edge), not free.

---

## Implementation phasing (this bead)

1. **contentHash in the parse pass** (own commit) — compute BLAKE3 in-worker,
   populate `_source.contentHash`, feed `symbol_id`.
2. **`symbols` + `symbol_id` + κ** — table, content-addressed key (path
   excluded), κ collapse in the language registry.
3. **`fact_edges` + FK + `unbound_facts`** — table, `PRAGMA foreign_keys=ON`,
   contains/defines/references arms, `head.capnp` counter.

Follow-ups: `binds` arm (LSP-enrichment path), full κ coverage,
`has_type`/`lineage`/`imports`, `symbol_attrs`.

## Open questions

- **κ closed-set boundary.** The 9 base kinds cover the current grammars; new
  languages may surface kinds that don't map cleanly (Rust `impl`, TS
  namespaces). The open-world escape prevents breakage but fragments
  cross-language queries — a per-language review surface.
- **Generation GC policy.** Diff/lineage wants ≥2 gens retained; storage wants
  1. Deferred to the lineage follow-up.
