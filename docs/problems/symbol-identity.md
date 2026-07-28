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

- **`token` is under-qualified, in five languages.** `node_defs` qualifies by
  *receiver* (impl type, trait, Go receiver, class) but never by enclosing **named
  lexical scope**. Seven distinct `fn walk_and_insert` across seven `mod` blocks in
  LLO's own `rs/ll-open/ts/src/refs.rs` carry one identical token.

  Measured with `leyline parse`, grouping `node_defs` by `(token, source_id)`:

  ```
  python   Helper x2  Helper.run x2  run x2    nested classes; every decorator makes a nested def
  java     Builder x2 Builder.build x2 build x2  nested types ARE named in-file scopes
  cpp      helper x2                            in-namespace defs emit bare
  rust     f x3 across mod a::b / mod a / mod c  plus Handler::run across mods
  ts       walk x2 across namespace A / B
  go       (none)                               the only measured-clean language
  ```

  The cause is uniform: every extractor qualifies exactly **one** ancestor level —
  `python_enclosing_class` `refs.rs:685`, `java_enclosing_type` `:964`,
  `cpp_enclosing_class` `:1117`, `rust_impl_receiver` `:531`. One level is complete
  for receivers, which do not nest; it is incomplete for lexical scopes, which do.

  Ruby, PHP, Kotlin, Scala and C# have **no extractor at all** — no dispatch arm in
  `refs.rs`, so zero `node_defs` rows. "Unaffected" there is vacuous, not safe.

- **`container_node_id` is not a fallback, and is not a defect.** Its semantics are
  *nearest enclosing κ container*, so NULL at top level is **by design**, and a
  partial index is the correct shape for a sparse column (mache corpus: 91/6,126 Rust
  rows non-NULL, 383/4,707 Go). It records enclosure by a *definition*, not membership
  in a type or module — which matters because the obvious "just populate the column
  that already exists" fix would overload a column that already means something else.

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
tree-sitter (ts/src/project.rs:106)    x.rs/mod_item_0/declaration_list/function_item  ← ORDINAL
lsp         (lsp/src/project.rs:1028)  symbols/alpha/walk                              ← NAME
```

Note the LSP root: the literal string `"symbols"` (`lsp/src/project.rs:217`, `:957`).
**No file segment enters the id at all** — which is why the mechanism is prior art and
not a usable address; see below.

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

Impl blocks *are* nested as their own container symbols, so same-named methods across
different impls get distinct parents. But four things disqualify this as the address:

1. **The container name is the raw impl header text**, embedding the trait path exactly
   as written. Rewriting `fmt::Debug` to `std::fmt::Debug` changes the address without
   changing the code's meaning. Name-keyed, yet syntactically derived.
2. **No file segment**, so ids collide *across files* in the only multi-file artifact —
   and `project_hover` is `INSERT OR REPLACE` on that id (`lsp/src/project.rs:783-786`),
   so the last file processed wins.
3. **It is not in the artifact consumers read.** `walk_symbol` runs only in the
   standalone single-file path (`project_lsp_into` `:198`); the merged/daemon path
   (`merge_lsp_into_ast` `:288`) keys `_lsp` by positional AST `node_id` instead.
4. **The DocumentSymbol tree is not the AST.** LSP segments exist only at symbol levels,
   while tree-sitter paths thread through `declaration_list`/`block`; and AST scope nodes
   are frequently *nameless* — `impl_item` has no name field (rust-analyzer synthesises
   "impl Handler" server-side), C++ has anonymous namespaces, TS `internal_module` emits
   no def and has no κ mapping.

So point 4 is the real cost: adopting the keying still requires per-language selection of
scope-bearing node kinds and **name synthesis for nameless scopes** — which *is* the
canonicalization the ADR must own. What exists in-tree is a name-keyed walk *mechanism*,
not an address.

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
   *positionally*, via `_ast` JOINs on line/char (ADR-0013 Step 1) — the `be6136`
   surface. Reconciling them is the actual deliverable. Note one of those JOINs is
   outright broken today: `merge_symbol` (`lsp/src/project.rs:1089-1099`) has **no
   `source_id` predicate**, so in the daemon a symbol can bind to a node in a
   *different file* at the same coordinates. Its sibling `lookup_referrer_node_id`
   (`:755-772`) filters correctly. Filed separately; the ADR should assume it is fixed,
   not design around it.
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

**Phase 0 — unblock consumers.** These are real defects now, but the two steps are
**ordered**, and the order is load-bearing:

1. `25811f` first — `function_signature_item` emits `canonical_kind = NULL`. Trait
   signatures fall out of a partial index *and* collide with the documented
   pre-migration escape. Decide `'function'` vs a distinct κ value; NULL is not an
   option. Audit sibling languages for the same bodyless-declaration hole.
2. *Then* tell consumers to adopt `node_hash` for dedup/CDC/memo, never for addressing.

**The ordering is not cosmetic.** The `node_hash` preimage hashes
`canonical_kind(raw).unwrap_or(raw)` (`cli-lib/src/cmd_parse.rs:2978`, `:3213`), so
**fixing `25811f` rewrites every trait-signature `node_hash`.** Telling a consumer to
pin to hashes first and fixing κ second would move the hashes under them. This
sequencing error was made once already and corrected on `mache-e64f36`.

Two further durability conditions follow from the same line, and the ADR must state
both: most raw kinds are *unmapped* and fall through to `raw`, so a tree-sitter grammar
bump that renames productions silently rewrites hashes; and Rust attributes are
*siblings* of the item, so `#[inline]` / `#[cfg(...)]` change the parent's hash but not
the function's own — a real hole for function-granularity memoization.

**Phase 1 — the address half.**

- `23377a` — key the tree-sitter walk by name, as a **generic enclosing-named-scope
  accumulation with a per-language audit**, not a Rust/TS patch. Java, Python, C++,
  Rust and TS are all measured-broken; **only Go is clean**, and languages with no
  extractor (Ruby, PHP, Kotlin, Scala, C#) must be recorded as out of scope rather than
  silently "passing". Requires an ancestor *walk*, since scopes nest and every extractor
  qualifies one level. Requires name synthesis for nameless scopes (`impl_item`,
  anonymous namespaces). Must **add** qualified rows without removing the bare aliases —
  `node_defs` is explicitly a many-to-one alias index and unqualified lookup depends on
  them.
- **This phase makes ADR decision #1 de facto.** The path *syntax* — separator, depth,
  how receiver and scope compose (`alpha::Handler::run` vs `alpha/Handler/run`) — *is*
  the single canonicalization authority. Once qualified tokens land in `node_defs.token`
  and a consumer keys on them, changing the scheme is a breaking re-emission. The
  *carrier* (column vs derived `symbol_id`) is genuinely deferrable; the canonicalization
  is not. Decide the syntax before emitting, even if the carrier waits.
- Open, and **not** to be presumed either way: `project_hover` is `INSERT OR REPLACE`
  on the LSP id (`lsp/src/project.rs:783-786`). Two `cfg`-paired `impl Handler` blocks in
  one file would render as two same-named DocumentSymbols, so Rust is *not* safe by
  assumption. C++ overloads are the other candidate. Needs a live-server fixture.

**Phase 2 — the ADR and reconciliation.**

- Choose (a)/(b)/(c), name the single canonicalization authority, pin the stability
  contract, and ship conformance vectors — mache and cloister will key on this.
- Reconcile the tree-sitter and LSP namespaces, replacing or hardening the positional
  JOIN that `be6136` exploited.

**Phase 3 — release.**

- Land Phase 0 + 1 in the upcoming release. Coordinate the `leyline-schema` pin bump
  with it (see below) rather than shipping consumers against an interim shape.

## Consumer impact

Consumers should adopt `node_hash` for dedup/CDC/memoization — durable today, does not
wait on the ADR — while never using it to address. **But adopt it after `25811f`**, per
the Phase 0 ordering: fixing that bead rewrites every trait-signature hash.

The address half is what they must not hand-roll in the meantime; a rendered-name key
entrenches exactly the failure this problem describes.

Release coupling: `nodes.record`'s declared type changes `JSON` → `TEXT` in
`ley-line-open-f7966d` (`JSON` carried NUMERIC affinity, so `'007'` was stored as `7`).
Downstreams pinning `leyline-schema v0.10.3` need the bump coordinated with the release
carrying Phase 0 + 1, not landed against an interim shape.

## Publication — why fixing this alone is not enough

Whatever the ADR decides must be **published through a generated consumer surface**, or
consumers hand-mirror it and the class returns. That is how mache arrived at
rendered-name keying, and how `cloister/src/routes/vault-proxy.ts:21` came to hand-write
an `InjectionStrategy` union mirroring a wire spec that lives here.

This is `vigil-4b304d`'s declare-vs-generate rule: *regenerable deterministically →
GENERATE.* capnp schemas are deterministically regenerable, so identity **types** belong
in the GENERATE arm. `cloister-86ce1f` states the stronger form — generate both halves
from one source and disagreement is not caught, it is *unrepresentable*; the check
ceases to exist rather than passing.

Note the boundary this does **not** cross. A reference implementation — LLO's
`schema-spec/credential-isolation/v1/ref-impl-py/` — is deliberately *not* generated. Its
purpose is to be an independent second statement of the spec's semantics, so
spec-vs-implementation disagreement remains detectable. Types generated, semantics
declared independently; `vigil-4b304d` carves out exactly this case ("declare
independently when you want two statements that are ALLOWED to disagree, because the
disagreement is the signal").

**LLO already hosts the generator, and does not run it.** `rs/ll-open/schema-bridge`
(`leyline-schema-bridge`, ADR-0036 Phase 2) emits zod TS / Go / JSON Schema from capnp
and hard-errors on any unmapped construct rather than silently emitting `z.unknown()`.
LLO tests the crate — `schema-bridge/tests/integration.rs` runs in the workspace sweep —
but invokes it as a generator **nowhere**: zero `schema-bridge` references in
`Taskfile.yml` and zero in `.github/workflows/`.

Downstream is where it actually runs. cloister's Taskfile carries `cluster:zod`,
`cluster:zod:verify` and `cluster:zod:check-drift` against LLO's plugin binaries today.
So the generator is exercised — just not by the repo that owns it. The gap is therefore
narrower than "nobody runs it", and differently shaped: **LLO can regress generation
semantics and only a downstream repo finds out.** One (schema, format) pair of LLO's own
in `task ci`, with a drift check, closes it.

Related: `ley-line-open-4ec276` (finish generalizing schema-bridge; its acceptance
criteria already ask for adoption beads downstream), `cloister-871aed` (the cloister
adoption bead that answers it), `cloister-5e4402` (`leyline-*` primitives belong in LLO),
`e7f466` (TS surface), `41867b` (Go consumer surface, done).

## Open questions

- Does the `_lsp` `INSERT OR REPLACE` collision reproduce in any shipped language?
  Rust is not safe by assumption — see Phase 1.
- Which κ value should a bodyless declaration carry (`25811f`): `'function'`, or a
  distinct value that keeps declarations separable from projectable constructs?
## Evidence

Every claim above, with how it was checked. **Refs are pinned to `abd5b86`** — line
numbers drift, so a stale ref means "re-locate the symbol," not "the claim is false."
Verdicts use: `VERIFIED` (checked directly), `CORRECTED` (an earlier draft of this doc
was wrong; the row states the correct claim), `CROSS-REPO` (measured by a consumer, not
independently reproducible here), `UNVERIFIED` (stated as open, not relied upon).

### Content half — `node_hash`

| # | Claim | Method | Verdict | Ref |
|---|---|---|---|---|
| 1 | `node_content` is keyed `node_hash PRIMARY KEY`, so identical subtrees dedup to one row | read DDL | VERIFIED | `rs/ll-open/ts/src/schema.rs:302`; ADR-0027:153 |
| 2 | Two identical-bodied fns in different `mod`s share one `node_hash` | `leyline parse` on a two-mod fixture; both `function_item` rows hashed `C64793AC…` | VERIFIED | fixture `idtest/gen1` |
| 3 | Inserting a `mod` above others leaves an unrelated fn's `node_hash` unchanged | two-generation parse, hashes compared | VERIFIED | fixtures `idtest/gen1`, `gen2` |
| 4 | Comment insertion does not change `node_hash` (`extra` excluded from the fold) | doc-comment + inner-comment fixture, hash byte-identical | VERIFIED (Fable) | `rs/ll-open/cli-lib/src/cmd_parse.rs:2782` |
| 5 | **Durability is conditional on the κ map.** The preimage hashes `canonical_kind(raw).unwrap_or(raw)`, so changing a kind's κ mapping rewrites every `node_hash` of that kind | read preimage construction | VERIFIED | `cmd_parse.rs:2978`, `:3213` |
| 6 | **Durability is conditional on grammar version.** Most raw kinds are unmapped and fall through to `raw`, so a tree-sitter grammar bump that renames productions silently rewrites hashes | follows from #5's `unwrap_or(raw)` | VERIFIED | `cmd_parse.rs:2978` |
| 7 | Rust attributes are *siblings* of the item, so `#[inline]` / `#[cfg(...)]` change the parent's hash but **not the function's own** | `#[inline]` fixture; fn hash unchanged | VERIFIED (Fable) | — |

Rows 5–7 are why "adopt `node_hash` now" carries conditions. In particular **row 5 means
fixing `25811f` rewrites every trait-signature hash**, so that fix must land *before*
consumers pin to hashes, not after.

### Address half — what does not work

| # | Claim | Method | Verdict | Ref |
|---|---|---|---|---|
| 8 | `node_id` ordinals are positional among same-kind siblings of one parent | read construction | VERIFIED | `rs/ll-open/ts/src/project.rs:106` |
| 9 | `node_id` **silently rebinds**: same id, same span, different function across generations | two-generation parse; `x.rs/mod_item_0` = `mod alpha` in gen1, `mod aaa` in gen2, both bytes 0..26 | VERIFIED | fixtures `idtest/gen1`, `gen2` |
| 10 | `node_id` *shape* changes when a singleton gains a sibling (`mod_item` → `mod_item_0`/`_1`) | two-generation parse | VERIFIED | fixtures `shape/a`, `shape/b` |
| 11 | Seven distinct `fn walk_and_insert` in `refs.rs` share one token | `grep -c "fn walk_and_insert"` → 7, in seven test mods | VERIFIED | `rs/ll-open/ts/src/refs.rs` |
| 12 | `mod_item` reaches the receiver match and is excluded by `_ => return None` | read `rust_impl_receiver` | VERIFIED | `refs.rs:531-549`, arm at `:541` |
| 13 | **CORRECTED — the collision is not Rust+TS only.** An earlier draft said "Go/Java/C need nothing." Java, Python, and C++ all collide | `leyline parse` per language, group by `(token, source_id)` having count>1 | CORRECTED | see table below |
| 14 | Only **Go** is measured-clean | mache corpus: 0 `(token, source_id)` collisions | CROSS-REPO | mache-e64f36 |
| 15 | Ruby/PHP/Kotlin/Scala/C# have **no extractor** — zero `node_defs` rows, so "unaffected" is vacuous, not safe | no dispatch arm in `refs.rs` | VERIFIED (Fable) | `refs.rs` dispatch |

Measured collisions (`leyline parse`, group by `(token, source_id)`):

```
python   Helper x2   Helper.run x2   build/run x2     nested classes; every decorator makes a nested def
java     Builder x2  Builder.build x2  build x2       nested types are named in-file scopes
cpp      helper x2                                    in-namespace defs emit bare
rust     f x3 across mod a::b / mod a / mod c         plus Handler::run across mods
ts       walk x2 across namespace A / B
go       (none)
```

Each per-language extractor qualifies exactly **one** ancestor level:
`python_enclosing_class` `refs.rs:685`, `java_enclosing_type` `:964`,
`cpp_enclosing_class` `:1117`, `rust_impl_receiver` `:531`.

| # | Claim | Method | Verdict | Ref |
|---|---|---|---|---|
| 16 | **CORRECTED — `container_node_id` is not a defect.** An earlier draft called it "NULL on every def row, despite an index that assumes otherwise." Its semantics are *nearest enclosing κ container*: NULL for top-level defs **by design**, populated where a container exists, and a partial index is the correct shape for a sparse column | read semantics; mache corpus 91/6,126 Rust, 383/4,707 Go non-NULL | CORRECTED | `ts/src/schema.rs:154`, `cmd_parse.rs:2915-2925`; mache-4b8a42 |

The original error was generalizing from three-function fixtures that contained no
function-local items. The operative point survives — it is not a qualification fallback —
but "kill it or fill it" was the wrong question and has been removed.

### The LSP walk — prior art, not an address

| # | Claim | Method | Verdict | Ref |
|---|---|---|---|---|
| 17 | The LSP walk is name-keyed and recursive: `format!("{parent_id}/{}", sym.name)` | read | VERIFIED | `rs/ll-open/lsp/src/project.rs:1028` (`walk_symbol`), `:925` (`flatten_symbols`) |
| 18 | **CORRECTED — the id has no file segment.** An earlier draft wrote `x.rs/alpha/walk`. The walk is rooted at the literal string `"symbols"`, so ids are `symbols/alpha/walk` | read call sites | CORRECTED | `project.rs:217`, `:957` |
| 19 | Consequently ids collide **across files** in the only multi-file artifact, and `_lsp_hover` is `INSERT OR REPLACE` on that id — last file wins | read | VERIFIED (Fable) | `project.rs:783-786` |
| 20 | `walk_symbol` runs only in the standalone single-file path; the merged/daemon path keys `_lsp` by positional AST `node_id` instead | traced both entry points | VERIFIED | `project_lsp_into` `:198` vs `merge_lsp_into_ast` `:288` |
| 21 | rust-analyzer nests impl blocks as their own container symbols, so same-named methods across impls get distinct parents | `leyline lsp --server rust-analyzer` on an `impl Display`/`impl Debug` fixture | VERIFIED | output: `symbols/impl fmt::Debug for Hash/fmt`, `symbols/impl fmt::Display for Hash/fmt` |
| 22 | The container name is the **raw impl header text**, so rewriting `fmt::Debug` → `std::fmt::Debug` changes the address without changing meaning | same run | VERIFIED | same output |
| 23 | The DocumentSymbol tree is **not** the AST. LSP segments exist only at symbol levels; AST paths thread through `declaration_list`/`block`, and AST scope nodes are often nameless (`impl_item` has no name — rust-analyzer synthesises one server-side) | read | VERIFIED (Fable) | — |

Rows 18–23 are why the claim is **"a name-keyed walk mechanism exists in-tree,"** not
"the address half exists." Row 23 is the real cost: adopting the keying still requires
per-language selection of scope-bearing node kinds and name synthesis for nameless
scopes — which *is* the canonicalization the ADR must own.

| # | Claim | Method | Verdict | Ref |
|---|---|---|---|---|
| 24 | **Unflagged bug:** `merge_symbol`'s `_ast` JOIN has **no `source_id` predicate**, so in the daemon (one connection, all files) a symbol can bind to a node in a *different file* at the same coordinates, then `INSERT OR REPLACE` cross-contaminates | read; contrasted with the sibling lookup that filters correctly | VERIFIED | `project.rs:1089-1099` (no filter) vs `lookup_referrer_node_id` `:755-772` (`WHERE source_id = ?1`) |

### History — why `symbol_id` was deleted

| # | Claim | Method | Verdict | Ref |
|---|---|---|---|---|
| 25 | The deleted `compute_symbol_id` hashed `(content_hash, span_start, span_end, kind, name)` — and `content_hash` is the **whole-file byte hash** (it is the value stored in `_source.content_hash`), so *any* edit anywhere in a file rewrote **every** `symbol_id` in that file. Stronger than "location-keyed" | read the deleted impl and its call site; traced `content_hash` to the `_source` insert | VERIFIED | `git show 34ed903` — `compute_symbol_id` body + call sites; `cmd_parse.rs:312` at that rev |
| 25a | It was short-lived: added in `34ed903` ("produce unified code-fact IR"), removed by ADR-0027's implementation weeks later | `git show 34ed903 --stat` | VERIFIED | `34ed903` |
| 26 | ADR-0027 states "span left symbol identity" and "Zero mache blast radius" | read | VERIFIED | ADR-0027:258, :241 |
| 27 | The deletion was **also** cost-driven, not only `be6136`-principle-driven — the ADR's Costs section targets "the earlier location-keyed design's ~3× insert / ~1.0 GB regression" | read | VERIFIED (Fable) | ADR-0027 Costs |
| 28 | ADR-0027's status is still **Proposed**, not Accepted | read frontmatter | VERIFIED | ADR-0027 header |
| 29 | `_source.id` **is** the relative path, so the occurrence key is path-bearing | query a projection: `id = "x.rs"` | VERIFIED | fixture `idtest/gen1.db` |
| 30 | ADR-0027 made `be6136`'s class loud rather than eliminating it (fail-loud FK integrity, `unbound_facts`) | read | VERIFIED | ADR-0027 §2, §3 |

### κ / `canonical_kind`

| # | Claim | Method | Verdict | Ref |
|---|---|---|---|---|
| 31 | `function_signature_item` has no arm in the Rust κ match and falls to `_ => None` | read | VERIFIED | `rs/ll-open/ts/src/languages.rs:342-356` |
| 32 | Trait signatures therefore emit `canonical_kind = NULL` | `leyline parse` on a trait fixture: `Greet::hello` → NULL, `Greet::bye` → `function` | VERIFIED | fixture `sig.db` |
| 33 | `idx_defs_canonical_kind` is partial (`WHERE canonical_kind IS NOT NULL`), so those rows are out of the index | read | VERIFIED | `ts/src/schema.rs:238` |
| 34 | NULL is *also* the documented pre-migration escape, so the two states are indistinguishable | read | VERIFIED | `DEFS_TABLE_DDL` doc block, `schema.rs:205-230` |

### Claims relied on but not independently verifiable here

| # | Claim | Status |
|---|---|---|
| 35 | 733 Rust constructs and 16.9% of Go `init()` bodies lost to rendered-name keying | CROSS-REPO — mache's measurement (`mache-e64f36`); not reproducible from LLO. Cited as consumer-reported impact, not as an LLO fact. |
| 36 | `_lsp` `INSERT OR REPLACE` collision reachable in a shipped language | UNVERIFIED. Rust is *not* safe by assumption: two `impl Handler` blocks in one file (e.g. `cfg`-paired) render as two same-named DocumentSymbols. Needs a live-server fixture. C++ overloads are the other candidate. |
| 37 | Whether `container_node_id` should carry the qualified path | OPEN — but see row 16; it already means something else, so overloading it is a design choice, not a fill-in. |
