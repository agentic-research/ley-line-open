# ADR-0030 — Sheaf over embeddings: making δ⁰ load-bearing

**Status:** Proposed (2026-07-16)
**Bead:** `ley-line-open-d4b72e`
**Related:**
- `ley-line-open-716c69` (necessity audit — δ⁰ reduces to a hash-gated reverse-dep BFS on all OSS-live inputs)
- `ley-line-open-71b20c` (Rung 0 — degeneracy tripwire test)
- `ley-line-open-20988a` / ADR-adjacent (extraction epoch — the exact-correctness floor this ADR rides on)
- `ley-line-open-a3f764`, `ley-line-open-d03e7d` (prior sheaf-liveness audits)
- decade `merkle-cas-substrate`, decade `hdc-translation-layer` (`ley-line-open-a660ab`)

---

## Context

The `leyline-sheaf` crate presents an incremental cache invalidation layer in the
vocabulary of cellular sheaves: stalks assigned to regions, a coboundary operator
δ⁰ measuring disagreement across adjacent regions, invalidation driven by the
δ⁰ cascade. A read-only necessity audit (`716c69`, 2026-07-15) established, in
code, that on **every input the open-source path can construct** this reduces to a
plain primitive:

- **Live stalks are SHA-256** (`sheaf/src/merkle.rs`). A cryptographic hash is an
  *avalanche* function: one input-bit change flips ~half the output bits. Two
  almost-identical regions therefore produce two maximally-distant stalks. The
  metric carries no signal — the only reachable states are "identical" (distance
  0) and "different" (distance ≈ random-large). There is no sub-EPS continuum.
- **Live restriction maps are always `RestrictionMap::project_dim_range`** — a
  static, axis-aligned coordinate mask, identical on both endpoints of every edge.
  `compose` / `weighted` / `RestrictionMap::new` have zero non-test callers.
- **`on_change` is a single-pass worklist BFS** (`sheaf/src/cache.rs`), O(V+E),
  each edge evaluated once — not an iteration to a fixed point.

Consequently `δ⁰_edge = ‖P·(x_v − x_u)‖²` with hash-byte stalks collapses to
"did the shared-boundary hash change," and the cascade collapses to "walk reverse
dependencies from the changed nodes, gated by hash equality." The δ⁰/Čech/H¹
machinery is real code but **off the invalidation path** — it feeds only a health
metric (`Σ‖δ⁰‖²`) and the agreement/`h0_dimension` diagnostics. `holonomy` does
not exist in `rs/` at all. The advertised "91×" precision is a filename→region
label index, not the cohomology.

None of this is a correctness defect. `node_hash` (content address) plus the
extraction epoch (`20988a`) are the exact-correctness floor; the sheaf is an
advisory eviction optimizer on top. The in-crate docstrings are already candid
("heuristic proxy for δ⁰ … NOT the Čech coboundary operator"). The gap is between
that honesty and the design-doc framing that treats the cohomology as the moat.

## The observation this ADR acts on

The ingredient that would make δ⁰ non-degenerate is **already in the workspace,
one crate away.** `rs/ll-open/hdc` produces *locality-preserving* representations:
Charikar signed random projection (SimHash) and HDC hypervectors
(`bytes_to_hv`, majority-bundled), where similar content maps to *close* vectors
by cosine. `hdc/src/schema.rs` already records the fork in a comment: *"algebra
doesn't pay rent and we'd ship il+LSH instead."*

The distinction is not "hash vs vector" — a SHA-256 *is* a vector. It is
**avalanche (locality-destroying) vs locality-preserving.** The sheaf is wired to
the wrong one.

## Decision

Pursue — **gated by a falsification ladder, not committed up front** — replacing
the sheaf's cryptographic stalks with locality-preserving ones (HDC hypervector
or SimHash of the region boundary), so δ⁰ measures genuine *semantic* disagreement
between overlapping regions.

If it survives the ladder, this yields **semantic incremental invalidation**: an
edit whose region embedding moves less than EPS is skipped without re-derivation —
a capability no content-hash system can offer, because a hash cannot express
"close but not identical." That, not the label index, would be a real moat.

The decision is explicitly *conditional*. Each rung below can kill the idea
cheaply; we do not build rung N+1 until rung N survives.

## Correctness architecture (why a lossy optimizer is safe here)

A semantic optimizer can be **wrong**: call two genuinely-different regions
"close," skip, and serve stale facts. It therefore must not be the correctness
authority. It does not have to be:

- `node_hash` (exact, content-addressed) + the extraction epoch remain the
  correctness **floor**.
- Sheaf-over-embeddings rides on top as a **performance** layer that skips work
  when confident, with the hash as the net when it is wrong (Rung 3).

A δ⁰ false-negative thus degrades to *unnecessary revalidation the hash catches*,
never *stale served*. This session's epoch/hash work is precisely what makes it
safe to let the optimizer be lossy.

## Proof ladder (falsifiable, cheapest-first)

**Rung 0 — degeneracy (baseline; `71b20c`).** Run δ⁰ `on_change` and a hash-gated
reverse-dep BFS on the current SHA stalks over the same inputs; assert identical
invalidation sets on every input. Establishes that the math adds zero today.

**Rung 1 — divergence / kill-switch (`d4e605`).** Swap in an HV/SimHash stalk in a
test harness; construct one cosmetic edit (rename a local, reflow whitespace) that
preserves region meaning; assert `δ⁰(HV) < EPS` while `hash(bytes)` differs — the
two decisions diverge. **If no such divergence can be constructed even with
locality-preserving stalks, the thesis is dead — stop and ship the honest BFS.**
~1 day, binary.

**Rung 2 — value / the real proof (`d50164`).** Ground truth is free: define
"meaningful change to a region" as "the derived facts (`node_defs` / `node_refs` /
CFG) actually changed," computable by re-derivation. Replay N real edits from git
history; per touched region record the δ⁰-skip decision (HV stalks) and the oracle
(did re-derived facts change?). Sweep EPS; tabulate:
- **true-skip** (δ⁰ skip, facts unchanged) = work saved — the payoff number;
- **false-negative** (δ⁰ skip, facts changed) = would-serve-stale — must be ≈ 0 at
  a useful EPS (and is caught by Rung 3);
- baseline SHA gate = 0 false-negatives, 0 true-skips.

Success = an EPS band with meaningful true-skip and near-zero FN → the math pays
rent, with a number. **Falsification** = the ROC is a diagonal: skip decision
uncorrelated with fact-stability → surface-embedding distance does not predict
derived-fact stability → decoration confirmed, ship the BFS. This rung tests the
one question the whole ADR hinges on.

**Rung 3 — safety invariant (`d53329`). HOLDS (2026-07-15).** Establish and test
that `node_hash` is checked under the δ⁰ skip decision; inject a deliberate δ⁰
false-negative and assert the consumer still receives correct facts. Makes Rung
2's FN rate a performance cost, not a correctness bug. Parallel with Rung 1/2.

*Result (measured, guard-path trace in bead d53329 + `cli-lib/tests/sheaf_skip_safety.rs`):*
the floor is stronger than "underneath" — the δ⁰ skip is entirely **off** the
fact-derivation and fact-serving paths. `SheafCache::on_change` / `reap` are
consumed only by `daemon/sheaf_ops.rs`, which emits an advisory
`daemon.sheaf.invalidate` event; `cmd_parse` re-derives on `epoch + mtime + size`
and content-addresses every fact by `node_hash` (`_source.content_hash`,
`_ast.node_hash`, `node_content` PK), reading nothing from the sheaf; query
commands read facts straight from the node_hash-keyed tables. There is no code
path where a δ⁰ skip can suppress a re-derivation whose `node_hash` would differ,
so a δ⁰ false-negative degrades to unnecessary revalidation the hash catches,
never stale-served facts. `sheaf_skip_safety.rs` pins this: it injects a real
`SheafCache` δ⁰ false-negative (sub-`DELTA0_EPS` boundary-embedding move while the
merkle/node-hash stalk changes) and asserts the node_hash floor still delivers
the re-derived facts; the test fails if the floor term is removed from the
consumer's refresh decision. No correctness gap found.

## The open question this exists to settle

**Does surface-embedding distance predict derived-fact stability for code
regions?** An HV of a region's *boundary* encodes surface content; the *facts*
depend on structure. They may not correlate — a small HV distance may say nothing
about whether the CFG changed. Nobody knows. Rung 2 is the experiment that
answers it, and it is the most likely place the idea dies.

## Consequences

- If it survives: `leyline-sheaf` becomes load-bearing as *general* behavior (not
  an edge case) in an embedding-stalk regime, and the δ¹/H¹/weighted-restriction
  machinery that currently has no callers acquires a purpose. Non-axis-aligned
  restriction maps (projections onto region subspaces — orthogonal decomposition)
  become the natural follow-on.
- If it dies at Rung 1 or 2: adopt `716c69`'s recommendation — rename the cascade
  to honest primitives, re-gate the moat claim on the label index, keep the health
  metric and agreement ops (which are real and separate). No loss; the audit
  already did that work.

## Non-goals

- Not proposing to remove the sheaf math (the health metric / agreement /
  `h0_dimension` ops are real and stay regardless).
- Not changing the correctness model — `node_hash` + epoch remain the floor.
- Not committing to non-axis-aligned restriction maps in this ADR; that is a
  distinct, harder follow-on gated on this surviving.
