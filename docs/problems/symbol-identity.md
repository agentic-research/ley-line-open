# Problem decomposition — `symbol-identity`

> **Decomposed:** 2026-07-27
> **Status:** Draft — for review
> **Beads:** `ley-line-open-2037b4` (design, P1), `ley-line-open-23377a` (bug), `ley-line-open-25811f` (bug)
> **Cross-repo:** `mache-e64f36` (design), `mache-e3d9bb` (P0 bug)
> **Refresh after:** 2026-10-27

## Problem statement

**LLO ships half of a construct identity, and consumers are inventing the other half badly.**

Identifying a code construct requires answering two independent questions:

| half | question | LLO today |
| --- | --- | --- |
| **content** | *what is this code?* | `node_hash` — ships, durable, correct |
| **address** | *which `walk` do I mean?* | **no durable answer** |

`node_hash` is a BLAKE3 merkle fold over the AST (ADR-0027). It is reflow-invariant
and survives insertion. It is also, by design, **not unique per occurrence**: two
identical-bodied functions in `mod alpha` and `mod beta` share one hash, because
`node_content` is keyed `node_hash PRIMARY KEY` for intrinsic dedup. That is correct
for dedup, CDC, and memoization, and disqualifying for addressing.

Nothing else in the projection addresses a construct durably:

- **`node_id` is positional and silently rebinds.** Not merely unstable — the same id,
  with the same byte span, denotes a *different function* across generations:

  ```
  gen1: x.rs/mod_item_0  bytes 0..26  =>  mod alpha { fn walk() {} }
  gen2: x.rs/mod_item_0  bytes 0..26  =>  mod aaa   { fn walk() {} }
  ```

  The id's *shape* also changes when a singleton gains a sibling
  (`mod_item` → `mod_item_0`/`mod_item_1`, `project.rs:106`). A consumer diffing
  generations on `node_id` sees "unchanged" for a node that became something else.

- **`token` is under-qualified.** `node_defs` qualifies by *receiver* (impl type,
  trait, Go receiver, class) but never by enclosing **named lexical scope**. Seven
  distinct `fn walk_and_insert` across seven `mod` blocks in LLO's own
  `rs/ll-open/ts/src/refs.rs` carry one identical token. TS `namespace` has the same
  defect. Go/Java/C do not — they have no named nestable in-file scope, so
  `source_id` genuinely addresses their free functions.

- **`container_node_id` tracks a different relationship.** It is populated, but only
  for *function-local* items — consts and nested defs declared inside a fn body
  (mache corpus: 91/6,126 Rust rows = 1.5%, 383/4,707 Go = 8.1%):

  ```
  WRITES_PER_THREAD -> .../blob_store.rs/mod_item/declaration_list/function_item_30
  ```

  So it records lexical enclosure *by a definition*, not membership in a type or
  module. It is still no fallback for qualification — but the mechanism is wired, not
  absent, which matters because the obvious "just populate the column that already
  exists" fix would overload a column that already means something else.

The cost is not theoretical. mache measured **733 Rust constructs** and **16.9% of Go
`init()` bodies** lost to keying on rendered names — the third, weaker identity it
invented because neither durable half was usable.

### Why the gap exists

ADR-0027 deleted `symbols`, `fact_edges`, `compute_symbol_id` and the UNIQUE
`symbol_id` index. **That deletion was correct.** `symbol_id` was location-keyed, and
incident `be6136` was a one-byte path-canonicalization mismatch that made every
`_lsp_refs × _ast` JOIN silently miss. The cure — *"path never enters the address"* —
produced `node_hash`.

The ADR names the cost in its own Consequences: *"the `head.capnp` root shape changes
(span left symbol identity)"*. And it explains why nobody noticed: *"Zero mache blast
radius — mache reads NONE of the deleted tables."* True at the time. mache has since
grown into needing it.

**The deletion was right. The replacement was never scoped.** That is this problem.

### The correction that makes this cheap

An earlier framing of this problem claimed *"an address must be nominal; you cannot
escape naming for addressing."* That was the wrong abstraction, and it made the work
look bigger than it is.

**It is a walk either way.** Both projections traverse the same containment tree; they
differ only in what keys each step:

```
tree-sitter (project.rs:106)   x.rs/mod_item_0/declaration_list/function_item   ← ORDINAL
lsp         (project.rs:1028)  x.rs/alpha/walk                                  ← NAME
```

`walk_symbol` and `flatten_symbols` in `rs/ll-open/lsp/src/project.rs` already build
`format!("{parent_id}/{}", sym.name)` — name-keyed, recursive, over the hierarchical
`DocumentSymbol` tree (`client.rs:171` requests `hierarchicalDocumentSymbolSupport`).

**The keying already exists in LLO** — in the LSP projection, while the tree-sitter
projection uses ordinals for the same traversal. So the work is not "design an identity
scheme" from nothing; it is "key the tree-sitter walk by name, as the sibling projection
already does," plus reconciling the two namespaces.

**But the LSP address is not a finished article, and must not be adopted verbatim.**
Measured against rust-analyzer on an `impl Display` / `impl Debug` fixture:

```
symbols/impl fmt::Debug for Hash/fmt      method
symbols/impl fmt::Display for Hash/fmt    method
```

Two useful facts. First, impl blocks *are* nested as their own container symbols, so
same-named methods across different impls get distinct parents — the `INSERT OR REPLACE`
collision is unreachable for Rust through impls, since duplicate method names within one
impl are not legal. (C++ overloads remain the open risk.) Second, and more important for
the ADR: **the container name is the raw impl header text**, embedding the trait path
exactly as written. Rewriting `fmt::Debug` to `std::fmt::Debug` changes the address
without changing the code's meaning. The LSP walk is therefore name-keyed and
collision-free but *syntactically derived* — prior art for the keying, not a canonical
semantic path. The ADR must define the canonical form; it cannot simply adopt this one.

## What must be decided (the ADR)

`ley-line-open-2037b4`. Three options:

- **(a) qualified `token` + `source_id`** — cheap, already indexed, no new table.
  Sufficient for Go/Java/C today (measured: 0 `(token, source_id)` collisions in Go).
  Insufficient alone for Rust `mod` / TS `namespace` until (23377a) lands.
- **(b) restored `symbol_id`** per mache ADR-0023 — one opaque join key; consumers need
  no naming rules.
- **(c) both, with `symbol_id` DERIVED from `(source_id, qualified token)`** so there is
  exactly one naming authority.

The ADR must also state:

1. **The stability contract per half** — what survives insertion, reflow, and rename;
   and what is *allowed* to collide. `node_hash` colliding on identical bodies is
   correct behaviour and must be documented as such, not filed as a bug.
2. **Namespace reconciliation.** Two id namespaces exist and are reconciled
   *positionally* — `enrich_symbols` resolves `referrer_node_id` via an `_ast` JOIN on
   line/char (ADR-0013 Step 1). **That JOIN is where `be6136` happened.** Reconciling
   them is the actual deliverable.
3. **The scope of the `be6136` cure.** *"Path never enters the address"* is true of
   `node_hash` only. `_source.id` **is** the relative path, so the occurrence key
   `(token, node_id, source_id)` is path-bearing. ADR-0027 handled that by making the
   failure **loud** (fail-loud FK integrity §2, `unbound_facts` §3), not by eliminating
   it. Any design here keeps that posture rather than claiming a path-free occurrence
   layer.
4. **Layer separation.** content / address / **provenance** are three things. Commit
   SHA and jj change-id are provenance and belong on the signed Head. Folding them into
   an address would change identity every commit, destroying cross-generation tracking,
   CDC dedup, and incremental memo — the opposite of the goal. Equally, a content hash
   cannot serve as the address: `_source.content_hash` changes on any edit to the file.
5. **LSP's role.** LSP is an enrichment pass requiring a live language server, so it is
   not reproducible from source alone and its coverage is per-language. It therefore
   cannot *be* the substrate identity — but it can be the authority the tree-sitter walk
   is validated against. This is consistent with
   [`agent-first-semantic-surface`](agent-first-semantic-surface.md): *"LSP becomes one
   such backend, not the substrate."*

## Plan

**Phase 0 — unblock consumers (no ADR dependency).**
Nothing below waits on the ADR; these are real defects now.

- `25811f` — `function_signature_item` emits `canonical_kind = NULL`. Trait signatures
  fall out of a partial index *and* collide with the documented pre-migration escape.
  Decide `'function'` vs a distinct kappa value; NULL is not an option. Audit sibling
  languages for the same bodyless-declaration hole.
- Tell mache to adopt `node_hash` **now** for dedup/CDC/memo, and never for addressing.

**Phase 1 — the address half.**

- `23377a` — key the tree-sitter walk by name. Requires an ancestor *walk* accumulating
  the path, not a match arm: modules nest, and `rust_impl_receiver` returns one level
  (correct for impls, which do not nest). Must cover TS `namespace`. Must leave
  Go/Java/C emission byte-identical. Must **add** qualified rows without removing the
  bare aliases — `node_defs` is explicitly a many-to-one alias index, and unqualified
  lookup depends on the bare rows.
- Verify the open question first: `walk_symbol` does `INSERT OR REPLACE INTO _lsp
  (node_id, ...)`. If two symbols in one file produce the same `parent_id/name`, the
  second silently replaces the first. Rust likely cannot hit it; C++ overloads and
  multiple trait impls may. **Unverified — needs a fixture, not an assumption.**

**Phase 2 — the ADR and reconciliation.**

- Choose (a)/(b)/(c), name the single canonicalization authority, pin the stability
  contract, and ship conformance vectors — mache and cloister will key on this.
- Reconcile the tree-sitter and LSP namespaces, replacing or hardening the positional
  JOIN that `be6136` exploited.

**Phase 3 — release.**

- Land Phase 0 + 1 in the upcoming release. Coordinate the `leyline-schema` pin bump
  with it (see below) rather than shipping consumers against an interim shape.

## Consumer impact

Consumers should adopt `node_hash` immediately for dedup/CDC/memoization — it is
durable today and does not wait on the ADR — while never using it to address. The
address half is what they must not hand-roll in the meantime; a rendered-name key
entrenches exactly the failure this problem describes.

Release coupling: `nodes.record`'s declared type changes `JSON` → `TEXT` in
`ley-line-open-f7966d` (`JSON` carried NUMERIC affinity, so `'007'` was stored as `7`).
Downstreams pinning `leyline-schema v0.10.3` need the bump coordinated with the release
carrying Phase 0 + 1, not landed against an interim shape.

## Open questions

- Does `schema-bridge` relate to this, or to `ley-line-open-e7f466` (TS bindings)?
  **Not investigated.** Flagged rather than guessed.
- Does the `_lsp` `INSERT OR REPLACE` collision reproduce in any shipped language?
- Should `container_node_id` be populated as part of Phase 1, or does the qualified
  token make it redundant? The ADR should kill it or fill it, not leave it NULL behind
  an index that implies otherwise.
