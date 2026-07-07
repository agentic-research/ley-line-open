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

`leyline parse` materializes a **merkle-AST content address** (`node_hash`) in
the **same parse pass**, in the **same transaction** as the existing
`_ast`/`node_defs`/`node_refs` projections. The content layer is a deduped
`node_content` table + a `node_child` git-tree object; the occurrence tables
that already exist additively carry a `node_hash` pointer. Referential
integrity is enforced at write time.

### 1. `node_hash` — the merkle-AST content address (the `be6136` cure)

`node_hash` is a **bottom-up (POST-ORDER)** fold over the tree, computed via the
locked σ surface (`ContentAddressed::hash` — BLAKE3, `lint:blake3`-gated), NOT
`blake3::hash` directly. It is a new **preimage**, not a new hash function:

```
P  =  "llo/ast/v1" ‖ 0x00                     # domain + version tag (git-object style)
   ‖  node_tag                                # 0x00 = leaf(terminal), 0x01 = internal
   ‖  uvarint(len(kind)) ‖ kind               # CANONICAL κ kind, length-prefixed
   leaf:      ‖ uvarint(len(token)) ‖ token          # terminal UTF-8 text, verbatim
   internal:  ‖ uvarint(child_count) ‖ child_hash[0..n]   # 32B each, SOURCE ORDER
node_hash = ContentAddressed::hash(P)         # 32-byte BLOB
```

Decisive choices:

- **Fold ALL non-`extra` children — named AND anonymous — in source order.**
  This fixes a real bug: the old walk recursed only over `is_named()` children,
  so operators (`+`, `-`, `==`) — which are ANONYMOUS tokens — were invisible
  and `a+b` hashed identically to `a-b`. Anonymous tokens are leaves whose text
  is the token, so the leaf rule captures them with no special case.
- **Length-prefix (uvarint), NOT a 0x00 delimiter.** Token text (string/char
  literals) can contain NUL; only length-prefixing is unambiguous once token
  text enters σ.
- **Canonical κ kind, not raw tree-sitter kind, in the preimage.** Insulates
  identity from grammar churn and enables cross-grammar dedup. `raw_kind` is
  kept as a CONTENT COLUMN, NOT hashed.
- **No language tag in σ** — a blob is language-agnostic; κ + structure
  disambiguate.
- **Normalization (permanent):** whitespace dropped (we hash tree + token, never
  raw bytes → a gofmt reflow changes nothing); comments/`extra` EXCLUDED from
  the fold; identifiers/literals/operators VERBATIM, NOT alpha-normalized
  (find_definition / GetCallers / v_refs resolve on the identifier string —
  collapsing Add/Sub would break "who calls Add?"). A rename is a new node_hash.
- **IN σ:** domain/version tag, κ kind, terminal token text, ordered child
  node_hashes. **OUT:** every positional field — start/end_byte, row/col,
  source_id/path, parse-run node_id, whitespace, comments. Containment is
  therefore INSIDE the hash (children are folded in).

**The path MUST NOT enter the address.** This is the `be6136` fix stated as an
invariant: because the fragile, mutable, path-shaped string is absent from the
content address, the `be6136` class of silent JOIN miss cannot recur by
construction. Two consequences fall out:

1. **Parse-run invariance.** An unchanged subtree yields a byte-identical
   `node_hash` across parse runs — and across files: two byte-identical
   functions in different files share a `node_hash` and are stored once.
2. **`node_id` is demoted to a locator.** The slash-path `node_id`
   (`_ast.node_id`, e.g. `main.go/function_declaration`) remains the
   human/tool-facing address and the parse-run-local pointer into `_ast`. It is
   **never a cross-fact join key.**

**The one-to-many invariant (load-bearing in code):** a reference's resolved
target is a def OCCURRENCE (`node_id`), NEVER a `node_hash`. `node_hash` is
one-to-many; keying resolution on it would silently collapse two distinct
callees with identical bodies (the mirror of `be6136`).

**Caveat (stated explicitly, mirroring ADR-0023):** content-addressed identity
*changes when bytes change*. "The same function across edits" is therefore a
**separate `lineage` edge** produced by a future diff pass, not `node_hash`
equality. `node_hash` is not overloaded to mean identity-over-time.

### 2. Fail-loud referential integrity

- `_ast.node_hash`, `node_defs.node_hash`, `node_refs.node_hash`, and both
  `node_child` endpoints are declared `REFERENCES node_content(node_hash)`.
- `PRAGMA foreign_keys = ON` is set for the parse transaction. A `node_hash`
  pointer that doesn't resolve to a real content row is therefore an **insert
  error in the producer**, not a silently-zeroed row in the consumer. The
  post-order fold emits children before parents, and `node_content` is flushed
  before every referencing table, so the FK is always satisfiable at write time.
- Containment carries no fail-loud edge because it is intrinsic: a parent's
  children are folded into its `node_hash`, and `node_child` records the tree
  structure. A "dangling parent→child" state is unrepresentable.
- An unresolved reference (cross-repo target, builtin, not-yet-parsed file) is
  simply a `node_refs` row whose `token` matches no `node_defs` row; it feeds
  the `unbound_facts` counter.

### 3. `unbound_facts` counter in `head.capnp`

A new field on `Head` (appended at the next free ordinal, per the ADR-0014
additive-evolution contract — unset stays byte-stable under canonical encoding):

```capnp
# Count of node_refs emitted this generation whose token resolves to no
# node_defs row — a ref the producer could not bind to a definition.
# Monotone per generation; the W5 ratchet gate asserts unbound_facts <= baseline.
unboundFacts @4 :UInt64;
```

"N facts failed to bind this generation" becomes a first-class, monotonic
number. `be6136` would have surfaced as this counter jumping to ~100% — a red
gate on the first CI run. (The field ordinal and name are unchanged from the
retired `fact_edges` shape — the count is now sourced from unresolved
`node_refs` targets, which is the exact parity image of the old
`fact_edges WHERE dst IS NULL AND kind IN ('references','calls')`.)

### 4. The schema

Net change is mostly deletion + one deduped content table + one git-tree
object + a `node_hash` column on tables that already exist.

```sql
CREATE TABLE node_content (        -- one row per UNIQUE subtree (~0.32·N rows)
  node_hash BLOB PRIMARY KEY,      -- 32B σ address; a real single-column PK
  node_tag  INTEGER NOT NULL,      -- 0 = leaf, 1 = internal
  kind      TEXT    NOT NULL,      -- CANONICAL κ (the hashed kind)
  raw_kind  TEXT    NOT NULL,      -- grammar kind (content column, NOT hashed)
  lang      TEXT    NOT NULL,
  token     TEXT,                  -- leaf text; NULL for internal
  arity     INTEGER NOT NULL
);  -- INSERT OR IGNORE on the PK == intrinsic dedup

CREATE TABLE node_child (          -- the git-tree object; deduped per unique parent
  parent_hash BLOB    NOT NULL REFERENCES node_content(node_hash),
  ordinal     INTEGER NOT NULL,
  child_hash  BLOB    NOT NULL REFERENCES node_content(node_hash),
  field       TEXT,                -- tree-sitter field ("name","body"), NULL if none
  PRIMARY KEY (parent_hash, ordinal)
);

ALTER TABLE _ast      ADD COLUMN node_hash BLOB REFERENCES node_content(node_hash);
CREATE INDEX idx_ast_node_hash ON _ast(node_hash);   -- "every location of this subtree"
ALTER TABLE node_defs ADD COLUMN node_hash BLOB REFERENCES node_content(node_hash);
ALTER TABLE node_refs ADD COLUMN node_hash BLOB REFERENCES node_content(node_hash);
-- DELETED: symbols, fact_edges, and the UNIQUE symbol_id index they needed.
```

`_source`, `nodes`, `_imports` are unchanged. FK order: children before parents
(bottom-up) under `PRAGMA foreign_keys = ON` — the post-order fold gives this
naturally, and `node_content` is flushed before every referencing table. All of
it is written inside the existing parse transaction.

Located containment ("children of THIS node at THIS position") stays via
`nodes.parent_id` / `_ast` — unchanged, no `parent_occ` column needed. Content
descent ("all distinct subtrees in X") is a recursive CTE over `node_child`.

### 5. Content vs occurrence split (this pass)

- **CONTENT** (intrinsic to `node_hash`, deduped once): identity (`node_hash`),
  `kind`/`raw_kind`/`token`/`arity`/`lang`, and CONTAINS (parent→child — the
  children are *inside* the hash, so there is NO stored per-occurrence
  containment edge; `node_child` records the deduped tree structure).
- **OCCURRENCE** (per-location — `_ast` already holds span/row/col/source_id/
  node_id): DEFINES, REFERENCES, IMPORTS. These stay in
  `node_defs`/`node_refs`/`_imports`, keyed by `token`+`node_id`+`source_id`,
  each additively carrying a `node_hash` pointer (NEVER keyed by `node_hash` —
  the one-to-many invariant).

| Source projection | emission | note |
| --- | --- | --- |
| `_ast` node | one `node_content` row (deduped); `node_hash` stamped on the `_ast` row | contains is intrinsic |
| parent→child | one `node_child` row per unique parent | the git-tree object |
| `node_defs` | occurrence row carrying `node_hash` | def site |
| `node_refs` | occurrence row carrying `node_hash`; unresolved token feeds `unbound_facts` | ref site |

mache's `v_defs`/`v_refs` smell views read `node_defs`/`node_refs` unchanged —
the additive `node_hash` column is invisible to them until they opt in.

**Deferred to a follow-up (out of scope for this producer slice):**

- **`binds` (scope-resolved binding) + `has_type` / `lineage`** — produced by
  the LSP-enrichment path (`rs/ll-open/lsp/…`) and a future diff pass, layered
  onto the occurrence tables once those paths are traced.
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
  address) and made loud (FK integrity + `unbound_facts`), not silent.
- The `a+b == a-b` collision is fixed — anonymous operator tokens are folded
  into `node_hash`.
- Content-addressed dedup: a unique subtree is stored once, shared across every
  file and location it appears in.
- **Zero mache blast radius** — mache reads NONE of the deleted tables. Its
  `find_definition`/`find_callers`/`v_defs`/`v_refs` read `node_defs`/`node_refs`
  and `_ast`, all of which keep working (the `node_hash` column is additive).

**Costs (named):**

- **Storage roughly halves the IR portion** — the content layer dedups to
  ~0.32·N `node_content` rows (measured 0.382 on a 28-file / 85 k-`_ast`-row
  Rust corpus), and the eager per-occurrence `contains` edges are eliminated
  (containment is intrinsic to `node_hash`). This replaces the earlier
  location-keyed design's ~3× insert / ~1.0 GB regression.
- **Honest floor:** the total .db still lands at roughly **1.5–1.9× the pre-IR
  insert baseline**, driven by the un-dedupable ~535 k-row `_ast` occurrence
  layer (one row per named node, per location) plus a new `node_content` PK
  index. The dedup wins on the *content* layer, not the occurrence layer.
- **The `head.capnp` root shape changes** (span left symbol identity), so this
  is treated as a new generation lineage / schema-version reset
  (`_meta.ir_schema_version = merkle-ast-v1`). The .db is a rebuildable
  projection — the next parse emits the new shape.
- **Cross-gen tracking is explicit** (a future `lineage` edge), not free.

---

## Implementation phasing (this bead)

1. **B1 — `node_hash` fold** — invert the parse walk to POST-ORDER returning a
   32-byte `node_hash` via `ContentAddressed::hash`; fold ALL non-`extra`
   children (named AND anonymous — fixes `a+b == a-b`); delete `compute_symbol_id`
   and the `node_to_sym` map.
2. **B2 — schema** — add `node_content` + `node_child`; `ALTER` `_ast`/
   `node_defs`/`node_refs` to add `node_hash`; delete the `symbols`/`fact_edges`
   DDL and the UNIQUE `symbol_id` index; wire inserts under
   `PRAGMA foreign_keys=ON` with `node_content` flushed first.
3. **B3 — fact rewire** — contains → intrinsic (`node_child` only);
   defines/references → occurrence rows carrying `node_hash`;
   `head.capnp` `unboundFacts` sourced from unresolved `node_refs` targets;
   delete the eager `contains` loop and the `EdgeBatch`/`SymbolBatch` machinery.

`_source.contentHash` (e251083 — the byte-level file address, complementary to
`node_hash`) and the κ/`canonical_kind` machinery are retained.

Follow-ups: `binds` arm (LSP-enrichment path), full κ coverage,
`has_type`/`lineage`/`imports`, `symbol_attrs`.

## Open questions

- **κ closed-set boundary.** The 9 base kinds cover the current grammars; new
  languages may surface kinds that don't map cleanly (Rust `impl`, TS
  namespaces). The open-world escape prevents breakage but fragments
  cross-language queries — a per-language review surface.
- **Generation GC policy.** Diff/lineage wants ≥2 gens retained; storage wants
  1. Deferred to the lineage follow-up.
