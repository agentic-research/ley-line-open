# ADR-0031 — Restriction-addressed derived-view caching over CAS

**Status:** Proposed (2026-07-17) — positive result, verified spike; deployment gated on `f38a86`.
**Bead:** `ley-line-open-f62327`
**Related:**
- ADR-0030 (the NO-GO this neighbors — approximate embedding-distance gating; see its 2026-07-17 addendum)
- `ley-line-open-716c69` (necessity audit — δ⁰ is a hash-gated BFS; **still holds**, this result does not revive the cohomology)
- ADR-0027 (merkle-AST `node_hash` — the stable identity the restriction key must adopt, `f38a86`)
- ADR-0026 / 0028 / 0029 (content-addressed substrate the review facts are read from)
- Experiment: commit `5b5b97c` on `codex-restriction-ast-review-toy`, `rs/ll-open/sheaf/tests/restriction_review_real.rs`, `docs/research/restriction-addressed-review-real-facts.md`

---

## Thesis

> **CAS gives exact identity for stored objects. Restriction-addressed caching
> gives exact identity for consumer-observable derived views.**

A content address (`node_hash`, a source-blob BLAKE3) answers "are these two
*objects* identical?" exactly and cheaply. It cannot answer "is this expensive
*derived view* — a code-review result, a resolved call graph, a lint verdict —
still valid?" without recomputing the view, because the view depends on a
*projection* of the substrate, not the whole object, and often on rows that span
*multiple* objects. Restriction-addressed caching closes that: cache the expensive
view keyed on the **exact hash of its input closure** — the specific substrate rows
the view observes.

## Context — why this exists after ADR-0030

ADR-0030 ran a falsification ladder on using an **approximate** embedding-stalk
*distance* (δ⁰ + EPS threshold) as a cache skip gate and returned a **NO-GO**: the
facts are identifier-keyed, a rename-invariant/lossy stalk can't see the changes
that move them, and no EPS band is both useful and near-zero-false-negative.

That result stands. This ADR is a **different mechanism** — **exact**, not
approximate. It hashes the review's input rows; it never thresholds an embedding.
The two ADRs are both true because they are about different things: ADR-0030
*falsifies the approximate*, this ADR *constructs the exact*.

## The construction (one review family: call-target review of a function `F`)

Three artifacts, kept **structurally separate** (the earlier toy conflated them,
making soundness a tautology — this does not):

1. **Restriction (cheap).** `hash(input_closure(F))` — a resolution-free gather +
   SHA-256 of: `F`'s `container_node_id`; the sorted `(token, qualifier)` of
   `node_refs` contained in `F`; the `_imports` rows those tokens name; and — load-
   bearingly — the `node_defs` rows those tokens resolve to, **across files** (an
   indexed point lookup). ~6 rows touched.
2. **Review result (expensive, distinct).** The resolved call graph — for each
   call token, an *unindexed* cross-corpus join over all `node_defs` plus import
   resolution. 400 → 20,000 rows at scale. (A deliberate lower-bound stand-in for
   a real review, which would be an analysis or LLM pass on top of these edges.)
3. **Oracle.** The review run independently on before/after, outputs compared. It
   never consults the restriction.

Soundness is by construction: the restriction hashes exactly the review's input
closure, and the review is a deterministic function of that closure, so
*restriction-unchanged ⇒ inputs-unchanged ⇒ output-unchanged*. This is memoization
keyed on the input closure — and it is a *win* only because the restriction is
strictly **cheaper** than the review (indexed vs. full scan) and strictly
**tighter** than the whole object (unchanged under edits elsewhere).

## What the experiment proved (verified: re-run + code-reviewed independently)

Nine fixtures over a two-file corpus (4 review-changing, 5 review-preserving):

| policy      | false_skip_rate | true_skip_rate | recompute_saved |
|-------------|-----------------|----------------|-----------------|
| WholeObject | 0/4             | 0/5            | 0               |
| AstShape    | 4/4             | 4/5            | 4 (unsafe)      |
| Restriction | **0/4**         | **5/5**        | **5**           |

Cost (deterministic rows touched; wall time swept over corpus scale, release):
205 defs → 6 vs 416 rows; 2005 → 6 vs 4016; 10005 → 6 vs 20016 (up to 3336× fewer
rows, ~18× faster wall-time at 10k defs). Honest footnote (printed by the test, not
hidden): in an unoptimized debug build the smallest scale *inverts* — SHA-256
constant overhead beats a 416-row compare — so restriction gating pays only when
the gated review costs more than ~1µs, which any real review clears by orders of
magnitude.

The five falsifiable properties, all held:

```
restriction != result
restriction < object
restriction cheaper than review at scale
restriction spans multiple objects
restriction unchanged => review output unchanged
```

- **`AstShape` false-skips all four review-changing edits** — the ADR-0030 blind
  spot reproduced exactly (call-target changes are identifier-text changes).
- The two edits the earlier toy was *missing* — a **local variable rename** and a
  **body arithmetic change** (`a+1` → `a*3`) — both came out **sound skip**
  (restriction unchanged, review confirmed unchanged) while `WholeObject`
  wastefully recomputed. These are the proof of *useful* skipping beyond whitespace.
- **The strongest fixture — cross-file def rename.** The callee's `node_def` is
  renamed in a *different* file; `F`'s file is byte-identical. The restriction
  *correctly invalidates* only because it reaches across the corpus boundary into
  the resolved-def rows the review observes. **This is where it stops being a
  "normalized AST cache key" and becomes genuinely restriction/sheaf-shaped: the
  key for `F` spans multiple objects.**

## The claim — and the intellectual-honesty prize

> **Restriction maps earned rent. Cohomology did not.**
>
> The sheaf-shaped restriction/topology structure is load-bearing for multi-object
> derived-view caching; δ⁰/H⁰/cohomological machinery remains **unused** for this
> cache-soundness result.

The experiment succeeds by **discarding** the cohomology and keeping only the
topology (a restriction that projects onto — and spans — the objects a consumer
observes). `716c69`'s finding that the δ⁰ machinery is decoration **still holds**.
What earned rent is the **restriction-map / multi-object input-closure** idea,
computed by **exact hashing**, not the "cohomology as invalidation oracle" idea
ADR-0030 killed.

## Decision

Adopt restriction-addressed caching as the model for caching expensive derived
views over the CAS substrate, **per consumer / per review family**, each with its
own restriction key and its own false-skip measurement. Do **not** reintroduce
δ⁰/embedding distance; the restriction is exact by design.

Deployment is gated on the caveats below (chiefly `f38a86`).

## Caveats (proven vs. asserted vs. preconditioned)

1. **Stable container identity is a precondition (`f38a86`).** The experiment keys
   containers by *name* (`fn:score`), not the daemon's *positional* `node_id`. A
   positional id shifts on any line change above `F` → the restriction degenerates
   to whole-file sensitivity (stays *sound*, loses the true-skips). Deployment
   requires keying on stable, reflow-invariant identity — ADR-0027's `node_hash`.
2. **The review is a lower-bound stand-in.** The cost win is *measured* in work
   and wall-time-at-scale, and *asserted* to grow for a real expensive review
   (LLM / heavy lint). The join is deliberately cheap; a real review widens the gap.
3. **Fixture proof, not yet replay (`f3a81e`).** Nine hand-picked fixtures plus a
   structural sound-superset argument. Exactness softens the small-N concern (unlike
   ADR-0030's approximate stalk, this needs no statistical false-negative budget),
   but a git-replay over real edits would stress superset-*correctness* harder — and
   the experiment already found one near-miss (cross-file def rows are load-bearing).

## Next moves (beads)

- `ley-line-open-f38a86` — re-key the restriction on `node_hash` / stable container
  identity (the deployment precondition).
- `ley-line-open-f3a81e` — git-replay stress test for superset-correctness (the
  rung-2 analog for the positive claim).
- `ley-line-open-f463aa` — second review family (public-API review keyed on
  `node_defs`) to prove the per-family pattern generalizes; then import review →
  `_imports`, unsafe/unwrap review → a token/operator restriction.
- Land the experiment (currently `5b5b97c` on `codex-restriction-ast-review-toy`)
  on `main`.

## Non-goals

- Not reviving δ⁰/H⁰ cohomology (it stays unused; `716c69` stands).
- Not an approximate/embedding skip gate (that is ADR-0030's NO-GO).
- Not a production daemon integration yet (gated on `f38a86` + `f3a81e`).
