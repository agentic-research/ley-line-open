# ADR-0030 Rung 2 — the value experiment (AST-structural stalk)

**Bead:** `ley-line-open-d50164`
**ADR:** `docs/adr/0030-sheaf-over-embeddings.md`
**Depends on:** Rung 1 (`ley-line-open-d4e605`, PR #235 — byte stalks RULED OUT)
**Status:** RESOLVED — **NO-GO**. The AST-structural stalk carries real but
insufficient signal; the ADR's near-zero-false-negative bar is not met.

---

## The one question this rung exists to settle

> Does a **rename-invariant AST-structural** embedding's distance predict
> whether a code region's derived facts (`node_defs` / `node_refs`) changed?

Rung 1 established that a *byte-trigram* stalk is anti-correlated with fact
stability (a rename moves it FARTHER than a fact-changing edit — byte distance
tracks trigram churn, not structure), and ruled byte stalks out. Rung 2 tests
the representation Rung 1 named as the only survivor candidate: a stalk over the
tree-sitter **node-kind sequence** (identifier text excluded), which is
invariant under renames by construction.

## What was built

- **The rename-invariant stalk** (`sheaf/tests/common/mod.rs`,
  `structural_stalk_*`): parse a region with `leyline-ts`, take the pre-order
  sequence of NAMED node kinds (`node.kind()` — never the identifier text),
  shingle into kind-trigrams, map each to a hypervector via the HDC substrate's
  `bytes_to_hv`, majority-bundle. A rename touches zero node kinds → the stalk
  is invariant. A structural edit (new branch, added statement, changed shape)
  perturbs the sequence → the stalk moves. The δ⁰ distance is the same normalized
  Hamming `d/D` the live sheaf's `edge_violation_squared` computes under the
  axis-aligned restriction mask (established faithful in Rung 1).

- **The free oracle** (`derive_facts*`): re-derive the region's
  `node_defs`/`node_refs`/`_imports` via `leyline_ts::refs::extract_rust` — the
  exact emission the daemon fold persists — and compare the fact SET before/after.
  "Facts changed" = the sets differ. Position-only identity (`node_id`,
  `container_node_id`) is excluded: it is a location, not a fact.

- **Milestone A** (`sheaf/tests/ast_structural_discrimination.rs`): the cheap-kill
  discrimination test, on the same fixtures as Rung 1.

- **Milestone B** (`sheaf/benches/git_replay_invalidation.rs`): the git-replay
  value experiment over LLO's own history.

## Milestone A — discrimination (the gate the byte stalk failed): PASSED

On the Rung-1 fixtures, the AST-structural stalk (`d/D`, EPS = 0.10):

| edit | `d/D` | oracle |
|---|---|---|
| whitespace reflow | **0.0000** | facts unchanged |
| local rename `total→running_sum` | **0.0000** | facts unchanged |
| meaningful (`+` guard branch, callee swap) | **0.1912** | facts changed |
| big-region rename | 0.0756 | facts unchanged |
| big-region meaningful (`±` branch, callee swap) | 0.1071 | facts changed |

Cosmetic edits land below EPS; structural edits above; **meaningful > cosmetic**
in both regions — the discrimination the byte stalk *inverted*. The kill gate is
cleared, so Milestone B is warranted.

**Two honest nuances surfaced (measured, not engineered away):**

1. **Rename-invariance is not absolute.** The big-region rename moves the stalk
   `d/D = 0.076` (not 0) because renaming `total→running` forces Rust's struct
   field shorthand `Summary { total }` to expand to `Summary { total: running }`
   — a genuine grammar-level shape change (`shorthand_field_initializer` →
   `field_initializer`). A rename is structurally invisible only when it does not
   perturb surface syntax the grammar distinguishes.

2. **The blind spot, pinned as a test.** A *pure* callee swap
   (`compute_weight → compute_penalty`, no branch) has `d/D = 0` while the oracle
   reports facts changed — a structural **false negative**. Both sides parse to
   `call_expression > identifier`; a kind-structure embedding cannot see it. This
   is not a defect in the stalk — it is the fundamental limit of a rename-invariant
   representation, and it is exactly what Milestone B quantifies on real edits.

## Milestone B — the value experiment

**Corpus:** LLO's own `rs/` tree, replayed from git history. For every
`function_item` present in both a commit and its parent whose source bytes
changed (a real region-edit), record (a) the structural `d/D` and (b) whether
the re-derived facts changed. Two runs, for stability:

| run | commits touching `rs/` | region-edits | facts changed | facts unchanged |
|---|---|---|---|---|
| N=400 | 400 | 1460 | 738 | 722 |
| N=1500 (full reachable `rs/` history) | 572 | 1960 | 1129 | 831 |

Runtime ≈ 50 s / 68 s respectively. Numbers below are the **N=1960** run
(both runs agree to within noise).

### There is real signal (the ROC is NOT a diagonal)

```
mean structural d/D | facts changed = 0.1110   facts unchanged = 0.0278   (Δ = 0.0832)
```

Fact-changing edits move the structural stalk ~**4×** as far as fact-preserving
edits, on average. Structural distance is genuinely correlated with derived-fact
change — this is the first stalk in the ladder that isn't anti-correlated.

### But the safety bar is not met — confusion matrix over EPS

Skip iff `d/D < EPS`. `skip-rate` = fraction of fact-UNCHANGED edits skipped
(work saved); `FN-rate` = fraction of fact-CHANGING edits wrongly skipped
(would serve stale).

| EPS | true-skip | false-neg | skip-rate | **FN-rate** | wasted-inval |
|----:|----:|----:|----:|----:|----:|
| 0.00 | 0 | 0 | 0.0% | **0.00%** | 831 |
| 0.02 | 499 | 114 | 60.0% | **10.10%** | 332 |
| 0.04 | 624 | 293 | 75.1% | **25.95%** | 207 |
| 0.06 | 702 | 452 | 84.5% | **40.04%** | 129 |
| 0.08 | 759 | 571 | 91.3% | **50.58%** | 72 |
| 0.10 | 783 | 666 | 94.2% | **58.99%** | 48 |
| 0.20 | 822 | 953 | 98.9% | **84.41%** | 9 |
| 0.30 | 828 | 1042 | 99.6% | **92.29%** | 3 |

### The irreducible floor

```
structurally-invisible edits (d/D == 0):
  facts changed  & d==0:  19   (irreducible false-negatives — the blind spot)
  facts unchanged & d==0: 363   (free true-skips at any EPS>0)
```

**19 fact-changing edits are structurally invisible** (the pure callee/arg/literal
swap class). They are skipped at *any* `EPS > 0`. So the false-negative rate has
a hard floor of 19/1129 = **1.7%** — no EPS can drive it to zero except EPS = 0,
which skips nothing. And the descent is steep: the smallest useful threshold
(EPS = 0.02) already captures 60% of skippable work at the cost of **114
would-serve-stale edits (10.1% FN)**.

### Verdict rule outcome

- **strict zero-FN:** EPS = 0.00 → 0.0% skip. (Any nonzero skip crosses the
  1.7% invisible-fact-change floor.)
- **≤1%-FN budget:** unreachable — no EPS holds FN ≤ 1% while skipping anything.
- **baseline SHA gate** over these edits: 0 true-skips, 0 false-negatives.

There is **no EPS band with meaningful true-skip AND near-zero false-negative.**
That is the ADR's own falsification condition, arrived at with a number.

## Why the correlation, though real, cannot pay rent

The ADR's safety model (Rung 3) says a δ⁰ false-negative should degrade to
"unnecessary revalidation the hash catches, never stale served." But every edit
in this corpus changed BYTES, so `node_hash` differs for **100%** of them. That
forces the dilemma the necessity audit (`716c69`) already identified:

- If the δ⁰-skip is trusted *without* re-checking the hash, ~10% of skips (at the
  only useful EPS) serve stale facts — a correctness regression.
- If the hash IS re-checked after a δ⁰-skip, it says "changed" for every edit and
  re-derivation runs anyway — the skip saves nothing.

Either way the hash gate dominates. The structural stalk's genuine 4×
separation is not strong enough to open a safe operating point between these two
failure modes. The blind spot (structure-preserving fact changes: callee swaps,
argument swaps, literal↔identifier) is intrinsic to a rename-invariant
representation and is where the value proposition dies.

## Verdict: NO-GO on ADR-0030

ADR-0030 dies at Rung 2. The progression across the ladder is honest and
monotone: byte stalks are anti-correlated (Rung 1); AST-structural stalks are
positively but weakly correlated and carry an irreducible ~1.7% false-negative
floor from structure-preserving fact changes (Rung 2). Neither delivers the
near-zero-FN semantic skip the ADR needs to be a moat.

**Recommendation** (per ADR-0030's own consequences clause and `716c69`):

- Ship the honest hash-gated reverse-dependency BFS; re-gate the "moat" claim on
  the filename→region label index, not δ⁰.
- Keep the health metric (`Σ‖δ⁰‖²`) and the agreement / `h0_dimension` ops —
  they are real and separate.
- Do **not** wire locality-preserving stalks into the invalidation path, and do
  **not** pursue the non-axis-aligned restriction-map follow-on that was gated on
  this surviving.

The correlation number (Δmean `d/D` ≈ 0.08, 4× class separation) is worth
recording: a *rename-invariant AST structural distance is a real but weak
predictor of derived-fact change* — usable perhaps as a diagnostic ranking
signal, never as a correctness-adjacent skip gate.

## Reproduce

```text
# Milestone A (discrimination + blind-spot pins)
cargo test -p leyline-sheaf --test ast_structural_discrimination -- --nocapture

# Milestone B (git-replay value experiment)
cargo bench -p leyline-sheaf --bench git_replay_invalidation
#   env knobs: RUNG2_N_COMMITS (default 400), RUNG2_MAX_EDITS (default 8000)
```

**Dev/bench deps added to `leyline-sheaf`** (test/bench only — production
`leyline-sheaf` stays HDC- and parser-independent): `leyline-ts` (feature
`rust`, the tree-sitter parse + `extract_rust` oracle) and `tree-sitter`.
`leyline-hdc` was already a Rung-1 dev-dep.
