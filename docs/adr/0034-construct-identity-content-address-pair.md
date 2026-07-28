# ADR-0034 — Construct identity is a pair: `node_hash` (content) + qualified token (address)

**Status:** Accepted (2026-07-27)
**Bead:** `ley-line-open-2037b4`
**Problem decomposition:** [`docs/problems/symbol-identity.md`](../problems/symbol-identity.md) — 38-row evidence matrix, refs pinned to `abd5b86`
**Related:**
- ADR-0027 (unified code-fact IR producer — defines `node_hash`; deleted the `symbol_id` this ADR replaces)
- ADR-0013 (LSP enrichment — the positional `_ast` JOIN this ADR must reconcile)
- mache ADR-0023 (unified code-fact IR — the consumer side that asked for `symbol_id`)
- `mache-e64f36` (mache reached the same conclusion independently)
- `ley-line-open-23377a` (the address half), `25811f` (κ NULL), `470290` (cross-file JOIN bug)
- `ley-line-open-4ec276` (schema-bridge — the publication path), `vigil-4b304d` (declare-vs-generate)

---

## Context

Identifying a code construct requires two independent answers. LLO ships one.

| half | question | today |
| --- | --- | --- |
| **content** | *what is this code?* | `node_hash` — ships, durable, correct |
| **address** | *which `walk` do I mean?* | **no durable answer** |

`node_hash` (ADR-0027 §1) is a BLAKE3 merkle fold over the AST. It is reflow-invariant,
survives insertion, and is **deliberately not unique per occurrence** — `node_content` is
keyed `node_hash PRIMARY KEY` so identical subtrees dedup to one row. Correct for dedup,
CDC and memoization; disqualifying for addressing.

Nothing else addresses durably. `node_id` is positional and **silently rebinds** — the
same id, at the same byte span, denotes a different function across generations. `token`
qualifies by *receiver* but never by enclosing named lexical scope, so five languages
collide. `container_node_id` means *nearest enclosing κ container* and is correctly NULL
at top level — not a fallback.

ADR-0027 deleted `compute_symbol_id`, and **that deletion was correct**: its preimage was
`(whole-file content_hash, span_start, span_end, kind, name)`, so any edit anywhere in a
file rewrote every `symbol_id` in it. The ADR named the cost in its own Consequences —
*"span left symbol identity"* — and explained why nobody noticed: *"Zero mache blast
radius — mache reads NONE of the deleted tables."* True then. Consumers have since grown
into needing it, at a measured cost of 733 Rust constructs and 16.9% of Go `init()`
bodies keyed on rendered names (mache).

**The deletion was right; the replacement was never scoped.** This ADR scopes it.

---

## Decision

### D1 — The address is `(source_id, qualified token)`, in the language's own syntax

Not a new opaque key. The address is the pair already present on every `node_defs` row,
with `token` fully qualified by its enclosing named scopes.

**Qualification uses the language's native separator, extending the convention already
shipped.** Emission is already language-native — `S::f` (Rust), `A.init` (Go), `C.walk`
(TS), `Greet::bye` (trait default). Introducing a second, uniform syntax would mean two
naming authorities, which is the failure this ADR exists to end. So:

```
rust    alpha::beta::walk_and_insert        mod alpha { mod beta { fn walk_and_insert } }
rust    alpha::Handler::run                 mod alpha { impl Handler { fn run } }
java    Outer1.Builder.build                nested types
python  Outer1.Helper.run                   nested classes; also nested defs (decorators)
cpp     alpha::helper                       namespace alpha { int helper() }
ts      A.walk                              namespace A { export function walk }
go      A.init                              unchanged — Go is already correct
```

**The qualified token is a lookup key, not a parseable structure.** `a::b::f` does not
encode whether `b` is a module or a type, and must not be relied on to. Disambiguation
comes from `canonical_kind` and `node_id` on the same row. This is deliberate: making the
token self-describing would require escaping rules, and escaping rules are how
canonicalization schemes acquire dialects.

**Bare aliases are preserved, always.** `node_defs` is explicitly a many-to-one alias
index — `open` and `FsBlobStore::open` are two rows pointing at one node. Qualification
**adds** rows; it never replaces the bare ones. Every existing unqualified
`find_definition` keeps working, by construction.

**Nameless scopes get synthesised names, and the synthesis is part of this contract.**
`impl_item` has no `name` field; C++ has anonymous namespaces; TS `internal_module` emits
no def today. Where a scope has no name, the emitter uses the receiver text already
computed by `rust_impl_receiver` and its siblings; where there is none at all
(anonymous namespace), the scope contributes **no segment** rather than a placeholder —
a placeholder would be a naming rule consumers must learn, which D1 exists to avoid.

### D2 — There is exactly one canonicalization authority, and it is the emitter

The path syntax in D1 is owned by `rs/ll-open/ts/src/refs.rs` and nothing else. No
consumer may reconstruct it, and no consumer needs to: the qualified token is emitted.

**This decision is forced by `23377a`, not by this ADR.** Once qualified tokens land in
`node_defs.token` and a consumer keys on them, changing the scheme is a breaking
re-emission. That is why D1 is decided here and now, before `23377a` emits, even though
D3's carrier question is deferrable.

### D3 — No `symbol_id` column, for now

Option (b) from the decomposition — a restored opaque `symbol_id` — is **rejected for
this pass**, and option (c) is deferred.

`(source_id, qualified token)` is already stored, already indexed (`idx_defs_token`),
already the ADR-0027 §5 occurrence key, and requires no migration. An opaque key adds a
second identity that must be kept consistent with the first, which is a new drift surface
in an ADR about eliminating drift.

If a single-column join key is later needed, it is **derived** —
`H(domain_tag ‖ source_id ‖ qualified_token)` — so the emitter stays the only authority.
It is not to be independently computed.

### D4 — The stability contract, stated per half

Consumers must be told what each half survives, and what it is *allowed* to collide on.

**`node_hash` (content).** Survives: reflow, comment insertion (`extra` nodes are excluded
from the fold), insertion of unrelated siblings. **Deliberately collides** on identical
subtrees — that is dedup, not a bug, and must never be filed as one.

Conditional on two things neither previously stated:

1. **The κ map.** The preimage hashes `canonical_kind(raw).unwrap_or(raw)`
   (`cmd_parse.rs:2978`, `:3213`), so remapping a kind rewrites every `node_hash` of that
   kind. **Concretely: fixing `25811f` rewrites every trait-signature hash.** Therefore
   `25811f` lands before consumers pin to hashes. A κ-map change is a **generation-lineage
   event** and must be announced as one.
2. **The grammar version.** Most raw kinds are unmapped and fall through to `raw`, so a
   tree-sitter grammar bump that renames productions silently rewrites hashes.

And one honest hole: **Rust attributes are siblings of the item, not children**, so
`#[inline]` / `#[cfg(...)]` change the parent's hash but not the function's own.
Function-granularity memoization keyed on `node_hash` misses attribute-only changes,
which are semantically meaningful. Consumers memoizing at function granularity must
include the parent hash or accept the miss.

**Qualified token (address).** Survives: reflow, insertion, edits to the body. **Changes**
when the construct is renamed or moved between scopes — which is correct, because that
*is* a different address. Collides where two constructs genuinely share a name in one
scope in one file, which C++, TypeScript and Python all permit.

> **AMENDED 2026-07-28** (`ley-line-open-5d3cb6`). This clause previously read
> "…the pair is insufficient and `node_id` disambiguates." That was wrong, and it
> conceded more stability than is necessary. `node_id` is positional, so it rebinds on
> reorder — but **overloads are discernible**, and a discernible construct must not be
> separated by position. The corrected two-tier rule:
>
> 1. **δ — a syntactic discriminator.** For overload languages, a hash of the parameter
>    *type-position* tokens, **excluding parameter names** — the Itanium mangling rule.
>    It therefore survives a parameter rename, and it is locally derivable because the
>    type tokens are in the subtree. Its failure mode is a spurious *split* (two
>    spellings of one type), never a collapse. For Rust `#[cfg]`-paired twins, fold the
>    attribute **siblings** into δ — not into `node_hash`; the content layer is
>    unchanged. That turns D4's own "honest hole" (attributes outside the fold) into an
>    asset.
> 2. **cohort-ordinal — only within a cohort δ cannot separate.** Position enters the
>    address exactly where position is all that exists, and nowhere else.
>
> The second tier is **forced by a theorem**, not chosen. No address can be
> simultaneously unique, stable under edits that do not move or rename the construct, and
> derivable from the file alone, once the domain admits byte-identical twins in one scope
> — and it does: C/C++ redeclaration, Python `def f()` twice, TS declaration merging,
> Rust `#[cfg]`-paired identical bodies. Proof by confluence: inserting an identical copy
> *above* or *below* an existing one yields **the same file, byte for byte**, so a
> snapshot-local function cannot tell the two positions apart, yet stability demands both
> keep the original address — contradicting uniqueness.
>
> The consequence for this ADR is larger than the clause: **an address is a presentation
> of a construct in one generation, not the construct.** Identity-over-time of
> indiscernibles lives in the edit trace, not in any snapshot. Cross-generation stability
> therefore belongs to the `lineage` edge ADR-0027 already reserved (§Consequences,
> "not overloaded to mean identity-over-time"), recovered pairwise between adjacent
> generations the way git recovers renames — computed, not stored.
>
> clangd's `detail` remains disqualified as *substrate* identity per D5 (server-supplied,
> not reproducible from source), but is the ideal **validator** for δ, which is precisely
> the role D5 already assigns to LSP.

### D5 — Namespace reconciliation is the deliverable, and one JOIN is broken

Two id namespaces exist:

```
tree-sitter   x.rs/mod_item_0/declaration_list/function_item     positional
lsp           symbols/alpha/walk                                 name-keyed, no file segment
```

They are reconciled **positionally**, by `_ast` JOINs on line/char — the `be6136` surface.

**`merge_symbol` (`lsp/src/project.rs:1089-1099`) has no `source_id` predicate**, so in
the daemon an LSP symbol can bind to a node in a *different file* at the same coordinates.
Its sibling `lookup_referrer_node_id` (`:755-772`) filters correctly. This is the `be6136`
class but **invisible to `be6136`'s instrumentation**: a wrong-file match resolves
successfully, so it is not an unbound fact and fail-loud FK integrity does not fire.
Filed as `ley-line-open-470290`; this ADR assumes it fixed.

**The LSP namespace is not adopted.** It is rooted at the literal string `"symbols"`, so
it carries no file segment and collides across files — where `project_hover` is
`INSERT OR REPLACE` on that id (`:783-786`), last file wins. And `walk_symbol` runs only
in the standalone single-file path (`project_lsp_into` `:198`); the merged path keys
`_lsp` positionally instead. Its container names are raw syntax (`impl fmt::Debug for
Hash`), so rewriting `fmt::Debug` to `std::fmt::Debug` would change the address without
changing meaning.

**LSP's role is validator, not substrate.** It requires a live language server, so it is
not reproducible from source alone and its coverage is per-language. It can be the
authority the tree-sitter walk is *checked against*. Consistent with
[`agent-first-semantic-surface`](../problems/agent-first-semantic-surface.md): *"LSP
becomes one such backend, not the substrate."*

### D6 — Content, address and provenance are three layers; do not fold them

| layer | answers | cardinality | where it lives |
| --- | --- | --- | --- |
| content | what the code *is* | many-to-one | `node_hash` |
| address | *which* construct | many-to-one lookup | `(source_id, qualified token)` |
| **locator** | *which row, this generation* | **one-to-one, per generation** | `node_id` / `nodes.id` |
| provenance | *which generation* | — | signed `Head` / Σ root |

> **AMENDED 2026-07-28** (`ley-line-open-5d3cb6`). The **locator** row is new. Its absence
> was not cosmetic: `nodes.id` and `_lsp.node_id` are `PRIMARY KEY`s, so they are
> definitionally one-to-one, while the LSP walk built a many-to-one value and stored it
> through `INSERT OR REPLACE` — destroying rows. clangd emitted three symbols for three
> overloads of `add`; one survived.
>
> "The address is many-to-one" is a coherent statement about `node_defs`, which is an
> alias index. It is **evasive** about `nodes`, because a primary key cannot be
> many-to-one and the code was not treating it as such. A locator must satisfy uniqueness
> and local derivability; it owes nothing across generations, and that is what the
> `lineage` edge is for.

Commit SHA and jj change-id are **provenance**. Folding them into an address would change
identity every commit, destroying cross-generation tracking, CDC dedup and incremental
memoization — the opposite of the goal. Equally, a content hash cannot serve as an
address: `_source.content_hash` changes on any edit to the file, which is exactly why the
deleted `compute_symbol_id` was unusable.

Note the honest scope of ADR-0027's *"path never enters the address"*: it is true of
`node_hash` only. `_source.id` **is** the relative path, so the occurrence key is
path-bearing. ADR-0027 handled that by making the failure **loud** (§2, §3), not by
eliminating it. This ADR keeps that posture and does not claim a path-free occurrence
layer.

### D7 — The outcome ships through a generated surface, or it does not ship

An identity consumers cannot import is an identity they will hand-mirror — which is how
rendered-name keying happened. Per `vigil-4b304d`: *regenerable deterministically →
GENERATE.*

The qualified-token contract and its κ vocabulary are published through
`rs/ll-open/schema-bridge`, which today is wired into **zero** Taskfile targets and
**zero** workflows (`ley-line-open-4ec276`). A generator nobody runs prevents no drift.
Wiring one (schema, format) pair into `task ci` with a drift check is a precondition for
calling this ADR delivered.

---

## Consequences

**Positive.**

- The address half exists, in a column consumers already read, with no migration.
- One naming authority. Consumers never reconstruct a path, so there is no rule to drift.
- Bare aliases preserved → zero blast radius for existing `find_definition` callers.
- `(source_id, qualified token)` and `node_hash` are independently useful: the first
  addresses, the second dedups. Neither is asked to do the other's job.

**Costs, named.**

- **`node_defs` grows.** Qualification adds one row per definition that has an enclosing
  named scope, on top of the existing bare alias. Bounded by definition count, not by AST
  size; `node_defs` is already a many-to-one alias index, so this is more of what it is.
- **`23377a` is larger than first filed.** Five languages measured broken (Java, Python,
  C++, Rust, TS), only Go clean, plus name synthesis for nameless scopes. Its original
  acceptance criteria required *"Go/Java/C emission byte-identical"*, which would have
  pinned the Java bug; corrected.
- **Five languages have no extractor at all** (Ruby, PHP, Kotlin, Scala, C#) — zero
  `node_defs` rows. They are out of scope here and must be recorded as such rather than
  reading as "passing".
- **The κ/`node_hash` coupling is now a release-sequencing constraint**, permanently. Any
  κ-map change is a generation-lineage event.
- **A qualified token is ambiguous about scope kind** by deliberate choice (D1). Consumers
  needing that distinction join `canonical_kind`.

---

## Implementation phasing

1. **`25811f` first.** `function_signature_item` → non-NULL κ. Must precede any consumer
   pinning to `node_hash` (D4.1). Audit sibling languages for the same bodyless-declaration
   hole.
2. **`470290`.** Add the `source_id` predicate to `merge_symbol` (D5). RED test: two files
   with symbols at identical coordinates.
3. **`23377a`.** Generic enclosing-named-scope accumulation, per-language audited, bare
   aliases preserved, D1 syntax. This is where D1 becomes load-bearing.
4. **`4ec276`.** One (schema, format) pair wired into `task ci` with a drift check (D7).
5. Conformance vectors, since mache and cloister will key on this.

## Open questions

- Which κ value should a bodyless declaration carry (`25811f`) — `'function'`, or a
  distinct value that keeps declarations separable from projectable constructs? mache
  asked for the separation; a distinct value is a κ vocabulary addition and must clear the
  architecture-vocabulary gate.
- Does the `_lsp` `INSERT OR REPLACE` collision reproduce in a shipped language? Rust is
  **not** safe by assumption — two `cfg`-paired `impl Handler` blocks in one file render
  as two same-named DocumentSymbols. Needs a live-server fixture.
- Do the five extractor-less languages get extractors, or an explicit "no def projection"
  declaration? Silence currently reads as coverage.
