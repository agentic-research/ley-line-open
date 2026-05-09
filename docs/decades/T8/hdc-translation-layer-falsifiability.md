# HDC as a training-free translation layer — falsifiability analysis

**Status:** Working analysis. Drives the design of the Rust experiment in
`rs/ll-open/hdc-translate/` (proposed crate, not yet landed).
**Decade:** T8 (post-Σ Merkle-CAS substrate)
**Date:** 2026-05-08
**Author:** theoretical-foundations-analyst (synthesis target: James Gardner)
**Source primitives:** `rs/ll-open/hdc/src/{util,encoder,sheaf,codebook,canonical}.rs`
**Source data:** `~/github/jamestexas/lossless/{enhanced_lshape_results.json, lshape_manifesto_results.json}`

> **Read me first.** This document does two things: (a) gives the claim a
> falsifiable shape so the Rust experiment is unambiguous, and (b) refuses to
> hand-wave the parts that don't go through. Sections 1–4 are constructive.
> Section 5 is the brake — there are concrete reasons VSA hasn't replaced
> learned embeddings in 30 years and they apply here. Section 6 lists the
> things I cannot decide without you.

---

## 1. Formalize the hypothesis

### 1.1 Setting

Let `E_src : ℝ^d → 𝔹^D` be the "input encoder" — a deterministic, training-free
random projection from the upstream embedding space (e.g. 384-d from
`all-MiniLM-L6-v2`) to a binary D-bit hypervector. In this repo, the canonical
construction is

```
E_src(x) := expand_seed(blake3_seed(quantize(x)))   // util.rs
```

i.e. a Johnson-Lindenstrauss-style random projection followed by a sign
threshold — but operationally any *fixed* deterministic map from `ℝ^d` to
balanced ±1 (equiv. binary 0/1) hypervectors will do, as long as
`E_src(x) ≠ E_src(y)` for `x ≠ y` and the bit balance is ~D/2.

Let `R = {r₁, …, r_k}` be a set of role hypervectors (`tagged_seed_vector` per
relationship type), with the only constraint that they are pairwise nearly
orthogonal — i.e. `Hamming(r_i, r_j) ≈ D/2` for `i ≠ j`. With D=8192 this is
trivially satisfied by independent SplitMix64 draws (see
`tagged_seed_vector_distinct_for_distinct_tags` test).

Let `bind(a, b) = a ⊕ b` (XOR — `xor_into` in `util.rs`).
Let `cos_HD(a, b) := 1 − 2·popcount_distance(a, b)/D` ∈ [−1, 1] — the cosine of
the corresponding bipolar vectors. This is *exactly* equivalent to the cosine
on `±1`-valued sign vectors derived from the binary HVs, no approximation.

Let `(x, y, t) ∈ Pairs_i` denote a labeled pair from relationship class `i`
(e.g. SIMILAR, HIERARCHICAL, DISSIMILAR, …) with `t = ±1` indicating
"in-class" vs "cross-class".

### 1.2 The naïve form of the hypothesis (which fails immediately)

The user's first-cut formulation looks like

> H₀ (naïve): for each relationship type `i`, there exist roles `r_i` such that
> `cos_HD(bind(E_src(x), r_i), bind(E_src(y), r_i)) ≥ τ_i` for in-class pairs.

This is **trivially true and trivially uninformative.** XOR is its own inverse,
so

```
cos_HD(E_src(x) ⊕ r_i, E_src(y) ⊕ r_i)
  = 1 − 2·Hamming(E_src(x) ⊕ r_i, E_src(y) ⊕ r_i)/D
  = 1 − 2·Hamming(E_src(x), E_src(y))/D
  = cos_HD(E_src(x), E_src(y)).
```

Same-role binding is similarity-preserving: the role drops out entirely. The
"role" plays *no* discriminative function under this construction. Any threshold
that holds without the role holds with the role. This is the **XOR identity
trap**, and it kills the simple version of the claim before any data is
collected.

### 1.3 The substantive form

For HDC to do real work, the *role* has to enter the comparison asymmetrically
or the *structure* has to live in a bundle, not a single bind. Three formally
distinct hypotheses follow, in order of strength.

**H₁ (Asymmetric-role separation — Plate's HRR pattern, weakest non-trivial).**
There exist roles `R = {r₁, …, r_k}` and a deterministic relation-encoder
`Φ_i(x, y) = bind(E_src(x), r_i^L) ⊕ bind(E_src(y), r_i^R)` (left-role / right-role
distinct per relationship `i`) such that for some readout vector `q_i` and
margin `μ_i > 0`:

> `cos_HD(Φ_i(x, y), q_i) − cos_HD(Φ_i(x, y'), q_i) ≥ μ_i`

for in-class pairs `(x, y) ∈ Pairs_i` versus distractor pairs `(x, y')`,
**without any learned parameters in `E_src`, R, or `q_i`** (they are all derived
from `tagged_seed_vector` of fixed strings). The readout `q_i` is built by
*bundling* a small held-out set of in-class examples — call this set the
"prototype kit", size `m`, fixed `m ≤ 32` per relationship.

This is testable with the L-shape relationship dataset. The falsification
condition is in §2.1.

**H₂ (Geometry-preservation — strongest informational claim).** The map
`E_src` followed by `Φ_i` defines a function `T : (ℝ^d)² × {1,…,k} → 𝔹^D` whose
*relationship-class accuracy* (top-1 retrieval of the correct partner from a
candidate pool) on a held-out test set is within ε of the L-shape encoder's
accuracy on the *same task with the same evaluation protocol*. The L-shape's
existing evaluation reports cosine similarity, which is the wrong metric (see
§2.4 on mode collapse); H₂ commits to a discriminative metric (top-k retrieval
or AUC of in-class vs cross-class).

**H₃ (Geometric-translation — what would actually justify the "translation
layer" framing).** For each non-Euclidean target geometry G ∈ {Hyperbolic,
Tropical, Symbolic} there exists a deterministic readout `R_G : 𝔹^D → G`
constructed from HDC primitives only (no training) such that some downstream
task metric on `R_G(E_src(x))` matches the metric on a SOTA learned encoder for
G to within ε. This is the "SMOGE-without-training" claim. **This is the
hypothesis the user actually cares about**, but it is *much* stronger than
H₁/H₂ and is not directly probed by the L-shape dataset.

### 1.4 What "training-free" means precisely

I am being deliberately strict. "Training-free" in this analysis means:

1. **No gradient descent.** No backprop on any parameter that touches the HDC
   layer.
2. **The prototype kit is permitted.** Bundling a small held-out set of
   in-class examples to form `q_i` is *not* training in the gradient sense; it
   is a closed-form constructor (Plate 1995, Kanerva 2009). It does, however,
   require labels for those `m` examples, so the protocol must report `m`
   prominently and ablate over it (m=1, 4, 16, 32).
3. **The roles are fixed by the protocol.** They come from
   `tagged_seed_vector("relation-CONCEPTUAL_METAPHOR-left", 0)` etc. No
   per-task tuning, no role selection on the eval set.
4. **`E_src` is fixed.** Either the literal `expand_seed(blake3_seed(...))` of
   a quantized embedding, or a fixed sparse-binary projection (Charikar
   simhash, already wired in this repo via `LayerKind::Semantic`). No PCA fit,
   no learned thresholds.

If at any point the protocol relaxes these to make a number look better, the
"training-free" claim is forfeit.

### 1.5 Status of each hypothesis

| Hypothesis | What it says | Strength | Decidable from L-shape data alone? |
|---|---|---|---|
| H₀ (same-role bind) | Cosine threshold under same-role binding | **Vacuous.** XOR identity trap. | N/A — refuted on the math. |
| H₁ (asymmetric-role + readout) | HDC matches L-shape's reported preservation, with selectivity not collapse | Non-trivial, falsifiable | Yes |
| H₂ (top-k retrieval parity) | HDC matches a discriminative metric within ε | Strong | Yes, but the L-shape baseline must be re-measured |
| H₃ (cross-geometry translation) | HDC is a free SMOGE | Very strong | No — needs SMOGE's tasks (AGNews, TREC) |

The Rust experiment in §4 covers H₁ and H₂. H₃ is a separate program of work.

---

## 2. Falsification conditions

### 2.1 Falsifying H₁

**Measurement.** For each of the 9 L-shape relationship classes, holding out
80% of pairs as test:

1. Build the relation-encoder Φ_i over **(x, y)** = **(left-anchor,
   right-anchor)** of each pair.
2. Build prototype `q_i` by bundling Φ_i over the m=16 training pairs of class i
   (`bundle_majority` in `sheaf.rs` if implemented for general bundling, else
   the bipolar-form majority — XOR-bundle is wrong here; see §5.5).
3. Compute discriminative score `s(x, y, i) = cos_HD(Φ_i(x, y), q_i)`.
4. Report:
   - **Within-class top-1 retrieval accuracy** (given x, find y from the pool
     of all candidates such that `argmax_y s(x, y, i)` is the true partner).
   - **AUC** of `s(·, ·, i)` separating in-class from cross-class pairs, per i.
   - **Selectivity** (analog of `lshape_selectivity` from `lshape_manifesto_results.json`):
     mean cross-class score minus mean in-class score, normalized.

**Falsification predicate.** H₁ is **refuted** if the median per-class
top-1 retrieval accuracy is within ±5pp of *random retrieval* (i.e.
1/|pool|), or if AUC < 0.55 on more than 5 of 9 classes. This is a deliberately
weak bar — we are not asking HDC to be *better* than the L-shape; we are
asking it to do non-trivially better than chance on a test labeled the same
way. If it doesn't clear that bar, the "training-free translation layer"
framing is empirically dead.

**Null hypothesis H₀.** Learned compression is necessary; the L-shape's 60%
compression with the relationship preservation it claims cannot be reproduced
by closed-form HDC primitives. Under H₀, any deterministic role-bind +
prototype-bundle scheme will give AUC indistinguishable from random.

**Failure modes that look like success.**

1. **Test-train leakage via prototype kit.** If the m=16 prototype examples
   overlap with the test set, the readout vector q_i contains the test
   examples and AUC will be artificially inflated. Mitigation: enforce a
   prototype/test split with stable hashing of pair identifiers.
2. **L-shape's degenerate baseline.** The L-shape reports 0.95+ similarity on
   *every* class including DISSIMILAR. That's mode collapse (see §2.4). If
   the experiment reports cosine similarity rather than discrimination, HDC
   will *also* trivially "match" the L-shape — both encoders are putting
   everything close to everything else, in different ways. Mitigation: never
   compare on cosine similarity alone; always compare on top-k retrieval and
   AUC.
3. **Class imbalance in the L-shape dataset.** From the table: CONCEPTUAL_METAPHOR
   has 1 pair, SHAPE_SIMILARITY has 1, MECHANICAL_RELATION has 1, USAGE_RELATION
   has 1. With n=1 you cannot distinguish anything. SIMILAR has 90 and
   HIERARCHICAL has 109; those are the only classes with statistical mass.
   Mitigation: either restrict the eval to {SIMILAR, HIERARCHICAL, DISSIMILAR
   (n=30)} or augment with a synthetic dataset that has balanced cardinality.
4. **Role-tuning on the eval split.** The roles `r_i^L`, `r_i^R` must be
   committed before any test data is touched. The simplest contract is:
   `tagged_seed_vector("relation-{NAME}-left", 0)` and `…-right`, fixed by
   the relationship name string. Any "search over role indices for the best
   one" is gradient descent in disguise.

**Statistical power.** For AUC tests, with n=90 in-class and n≥90 cross-class,
the standard-error on AUC is roughly `0.08–0.10` under H₀. To detect a true
AUC of 0.65 against the H₀ value of 0.5 with α=0.05 and power 0.8, n≥40 per
class suffices. Three of nine classes (SIMILAR, HIERARCHICAL, DISSIMILAR)
clear that bar in the existing dataset. The other six are statistically
underpowered and any per-class report on them is descriptive, not inferential.

### 2.2 Falsifying H₂

**Measurement.** Re-evaluate the L-shape model itself with the *same*
discriminative metric used for HDC: top-k retrieval and AUC. (The published
L-shape numbers are cosine similarities, which are not directly comparable.)
Then compare HDC AUC vs L-shape AUC per class; ε-band of ±0.05.

**Falsification predicate.** H₂ is **refuted** if the L-shape's discriminative
metric exceeds HDC's by more than 0.05 AUC on the majority of statistically
powered classes (i.e. SIMILAR, HIERARCHICAL, DISSIMILAR — see §2.1 power
calc).

**Null hypothesis.** The L-shape's *learned* compression genuinely encodes
relationship structure that closed-form binding cannot reproduce.

**Failure mode.** If the L-shape's AUC is *also* near 0.5 (which the
selectivity = -0.0017 figure strongly suggests it will be), then "HDC matches
L-shape" reduces to "two encoders both fail at the discriminative task," which
is true but not informative. The protocol must report a third reference point:
**raw `all-MiniLM-L6-v2` cosine similarity, no compression at all**. If the
raw embeddings already achieve high AUC and *both* HDC and L-shape degrade
it, the entire compression program has misframed the problem.

### 2.3 Falsifying H₃

Out of scope for this experiment harness. Would require the AGNews/TREC
classification pipeline from SMOGE, replacing the learned router with HDC
bundle/bind, and reporting accuracy under the same training data exposure
(zero, in HDC's case). Flagged in §6 Open Questions.

### 2.4 The L-shape mode-collapse problem (the analytic load-bearing finding)

From `lshape_manifesto_results.json`:

```
relationship_disentanglement.lshape_selectivity = -0.0017749…
compositional_reasoning.lshape_accuracy        = -0.0112545…
```

Both are essentially zero (or negative). Combined with the
`enhanced_lshape_results` table where every class — *including* DISSIMILAR —
ends up at 0.94–0.99 cosine, the diagnosis is unambiguous: **the L-shape is
collapsing the embedding space onto a low-dimensional manifold near a single
point**, and "high cosine similarity" is the symptom, not the success.

This is *not* an HDC problem. It is a problem with the L-shape's published
metric. The HDC experiment must avoid the same trap by:

1. Reporting AUC and top-k retrieval, not raw cosine.
2. Including DISSIMILAR as a *negative* test: an encoder that says
   "DISSIMILAR pairs look DISSIMILAR" must produce *low* in-class similarity
   for that class — or rather, the per-class AUC discrimination is what's
   evaluated, not a cosine threshold.
3. Reporting effective rank of the encoded space — if HDC outputs collapse
   to low rank, we will see it directly. (Trivially false for binary HVs
   with balanced bits, but worth pinning as a regression test.)

---

## 3. Implication chain

The user asked: *"if it works, neurosymbolic is very different arch, right?
teeny SLMs should work?"* Three causal claims, each weaker than the last
implies. Treat them as independent; do not let H₁ entail H₂ entail H₃ by
vibe.

### 3.1 If H₁ holds — what is *entailed* about neurosymbolic architecture

**Strong claim that does follow.** *Where* a system needs only relational
algebra over distributional embeddings, that algebra is closed-form and
training-free. The "symbolic layer" of a neurosymbolic stack — predicate
binding, role assignment, set composition — does not require gradient descent
when the binding algebra is HRR/VSA. This is **already established** in the
VSA literature (Plate 1995; Eliasmith's Spaun, 2012; Schlegel et al. 2022).
H₁ would simply *replicate* that finding on a new dataset. It does **not** say
anything new about the *neural* layer.

**Strong claim that does NOT follow.** "Therefore neurosymbolic systems are
cheaper to construct in general." The expensive parts of a neurosymbolic
architecture are typically (a) the perception encoder feeding the symbolic
layer (still neural, still trained), and (b) the *grounding* — making the
symbols correspond to the right things in the world. HDC bind makes step (c)
— the *algebra over already-grounded symbols* — free, but (a) and (b)
remain. The savings are real but bounded.

**Concrete bound.** If a neurosymbolic system has a perception encoder with N
parameters and a symbolic-reasoning module with M parameters, and HDC replaces
*the entire symbolic module*, the parameter saving is at most M/(N+M). For
typical systems where the perception encoder dwarfs the reasoner (BERT-base
110M vs reasoning module ≤1M), this saving is ≤1%. For systems where the
reasoner is the bulk (e.g. logic-heavy KG completion), the saving can be
substantial. Don't sell the saving as architectural-revolutionary unless the
target system is in the second regime.

### 3.2 If H₂ holds — small models routed through HDC

**The conditional claim.** A small language model (call it θ_S, e.g. ~270M
params like Gemma-3-270M) augmented with an HDC bus that performs
relational composition can match a larger end-to-end model (call it θ_L, e.g.
7B+) **on the subset of tasks whose difficulty lives in the relational
composition step, not the perception or memorization step**.

**For which tasks?** The user's question deserves a precise answer. Three
task classes:

1. **Pure compositional generalization** (out-of-distribution composition of
   known primitives — SCAN-like, COGS-like, gSCAN). Here H₂ predicts the
   small-θ-S+HDC model **should** match θ_L, because the failure mode of θ_L
   on these tasks is well-known to be *compositional*, not capacity-bound
   (Lake & Baroni 2018). Empirical bet: HDC bus closes 50–80% of the
   compositional gap.
2. **Memorization-bound tasks** (factual QA, named-entity-heavy retrieval).
   Here H₂ predicts **no improvement**. The bottleneck is parametric memory,
   which HDC bind does not provide. Small-θ-S+HDC will still trail θ_L.
3. **Mixed reasoning** (math word problems, multi-hop QA). Predicts
   **partial improvement** — the relational structure helps, the parametric
   memory still hurts. The Tri-Brain pattern from BREAD's README is the
   correct architectural model: HDC for the relational scaffold, neural for
   the perception, symbolic verifier for ground truth. The user has already
   built one instance of this pattern.

**The boundary is sharp and pre-registerable.** Before the experiment, commit
to: *H₂ predicts gain on compositional benchmarks with effect-size > 0.1
accuracy, and predicts no gain (effect ≤ 0.02) on TriviaQA-class
memorization*. If the small-model+HDC system gains uniformly on both, the
gain is from a confound (e.g. the prototype kit is leaking task signal) and
H₂ is partially refuted.

### 3.3 If H₃ holds — training is "redundant"

**Restate the claim precisely.** "Training is redundant" can mean any of:

(a) *Overparameterization redundancy* — large models have far more capacity
    than they need to fit the compositional structure of language; the
    structure could be encoded in a small fixed algebra (HDC). True; well-
    documented (lottery ticket, sparse fine-tuning, etc). Saying so is not
    new.

(b) *Inductive-bias redundancy* — gradient descent is *re-discovering* an
    algebra (binding, bundling, permutation) that we could have written down
    in closed form. Plausible; this is exactly the Plate (1995) thesis. The
    evidence for it is that the relation between distributional similarity
    and binding algebra is *theorem*-like, not data-dependent.

(c) *End-to-end training redundancy* — given the right closed-form bus, the
    gradients flowing through neural components are smaller and the
    end-to-end loop converges faster (or doesn't need to be end-to-end at
    all). Active research direction; partially supported by adapter-tuning
    literature.

H₃ in its usable form is the conjunction of (b) and (c). Under (b)+(c), what
you save is the *symbolic part* of model capacity, and what you keep is the
*perceptual/memory part*. So:

**The honest statement.** *If H₃ holds, then for any task whose difficulty
decomposes as `(perception, composition, memory)`, the composition component
becomes constructive and training-free; perception and memory still cost
parameters proportional to their irreducible information content.* This is
much weaker than "training is redundant" but it is what the math actually
supports.

### 3.4 What none of H₁ / H₂ / H₃ entail

- That LLMs as currently architected are doing something wrong.
- That gradient descent is unnecessary in general.
- That the "consciousness" framing in semfield is grounded. (User flagged
  this; the analysis takes the flag at face value.)
- That BREAD's holonomy result is wrong. Quite the opposite: BREAD measures
  *non-abelian* curvature in transformer paths; HDC's bind is abelian
  (commutative). The two are complementary measurements of *different*
  structural layers, not competing claims about the same one. See §5.4.

---

## 4. The Rust experiment protocol

A single binary in a new crate `rs/ll-open/hdc-translate/`. No network calls,
no Python prototyping, no test-set peeking.

### 4.1 Crate layout

```
rs/ll-open/hdc-translate/
├── Cargo.toml             # depends on leyline-hdc, ndarray, anyhow, serde, csv
└── src/
    ├── lib.rs             # public API (pure functions, no side effects)
    ├── projection.rs      # E_src: ℝ^d → 𝔹^D
    ├── relation.rs        # Φ_i, prototype kit, score s(·, ·, i)
    ├── readout.rs         # bundle_majority + cleanup-memory readout
    ├── eval.rs            # AUC, top-k, selectivity
    └── bin/
        └── lshape_replay.rs   # the H₁/H₂ harness
```

### 4.2 Inputs

1. **Embedding matrix.** `[N × 384] f32` from
   `~/github/jamestexas/lossless/compressed_embeddings.npy` (or re-emit fresh
   via SentenceTransformers; doesn't matter as long as the projection is
   stable).
2. **Pair labels.** A CSV/Parquet of `(pair_id, anchor_idx, partner_idx,
   relationship_class, split)` derived from
   `enhanced_lshape_results.json`. Splits: `prototype` (m examples per class),
   `test` (rest), enforced disjoint by `pair_id`.
3. **No HDC parameters.** All HDC roles, codebooks, projection seeds are
   string-tagged constants committed in code. The reproducibility contract
   is: same input embeddings + same source rev → same output to the bit.

### 4.3 Algebra steps (one pass per pair)

For each pair `(x_idx, y_idx, class_i)`:

1. `x_hv := E_src(embeddings[x_idx])` via
   `expand_seed(blake3_seed(quantize_to_bytes(embeddings[x_idx])))`.
   (`projection.rs`. Quantize `f32 → i8` first to make the seed stable.)
2. `y_hv := E_src(embeddings[y_idx])` similarly.
3. `r_left_i := tagged_seed_vector("relation-{class_i.name}-left", 0)`.
4. `r_right_i := tagged_seed_vector("relation-{class_i.name}-right", 0)`.
5. `phi := bind(x_hv, r_left_i) ⊕ bind(y_hv, r_right_i)`. (One `xor_into`
   call composed with another. `bind` is `xor_into`.)

For each class i, build the prototype `q_i` from the m=16 prototype-split
pairs by **majority bundle** (not XOR-bundle — see §5.5). The repo already
ships `bundle_majority` for this purpose in `sheaf.rs`; use it. If
`bundle_majority` is internal, expose a `pub fn` or copy the algorithm:

```
q_i_bits[d] = sign(sum over prototype pairs of ((-1)^{phi_p[d]}))
            (i.e. majority over the bipolar projections, broken to 0
             with deterministic tiebreak)
```

### 4.4 Score and metrics (`eval.rs`)

For test pair `(x, y, class_i)`:

- `s_inclass := cos_HD(phi(x, y, i), q_i)`.
- For 31 cross-class distractors `y'` (sampled deterministically by hashing
  the seed `pair_id || y'`): `s_cross := cos_HD(phi(x, y', i), q_i)`.

Per class:

- **AUC** of `s_inclass` vs `s_cross` distributions.
- **Top-1 retrieval rate** over the 32-element pool (1 true + 31 distractors).
- **Selectivity** = mean(s_inclass) − mean(s_cross), divided by the
  pooled std. (Cohen's d.)

Per experiment:

- Macro-AUC across statistically powered classes ({SIMILAR, HIERARCHICAL,
  DISSIMILAR}).
- Effective rank of the matrix `[phi(x_p, y_p, class_p) for all test pairs]`
  (regression test on mode collapse — should be near full rank D, not
  near 1).

### 4.5 Falsifiability gates

The experiment binary exits non-zero if **any** of:

1. Macro-AUC over statistically-powered classes < 0.55. (H₁ refuted.)
2. Mean per-class top-1 retrieval < 1/32 + 0.05 = 0.081. (H₁ refuted.)
3. Effective rank of test-set encoded matrix < D/4 = 2048. (Mode collapse
   regression.)
4. Bit-exact reproducibility: re-run produces non-identical scores. (Pin
   determinism — `expand_seed` and `tagged_seed_vector` are pure functions;
   any drift indicates a code bug.)

The output JSON schema:

```json
{
  "git_rev": "...",
  "n_pairs_per_class": { "SIMILAR": 90, ... },
  "n_prototype": 16,
  "results_per_class": [
    { "class": "SIMILAR", "auc": 0.72, "top1": 0.41, "selectivity": 0.83, "n_test": 74 },
    ...
  ],
  "macro_auc": 0.68,
  "effective_rank": 7892,
  "gates_passed": true
}
```

### 4.6 What this experiment does NOT cover

- It does not test H₃ (cross-geometry translation). That requires the SMOGE
  task suite.
- It does not stand up an SLM and route it through HDC. The user mentioned
  an "inference interruptor in llo" — I could not locate it in this
  repository; see §6.
- It does not measure latency / parameter count vs the L-shape encoder. Add
  if needed; trivial.

---

## 5. Adversarial / failure analysis

### 5.1 Why hasn't HRR/VSA dethroned learned embeddings in 30 years?

Plate's HRR (1994) and Kanerva's binary spatter codes (1996) are older than
word2vec (2013) and BERT (2018). They have not displaced learned
distributional embeddings. The reasons matter for the user's claim.

1. **Capacity per dimension.** HRR/VSA stores ~D/(2·log D) cleanly retrievable
   role-filler bindings before crosstalk dominates. At D=8192 that is roughly
   **300 bindings**. A learned embedding at d=384 with full f32 precision has
   ~10^9 distinguishable points by quantization, vastly more capacity for
   *content*. VSA wins on *structure* per parameter (it is essentially free)
   but loses on *content* per dimension. The right comparison is: VSA + a
   small content table vs. learned dense embeddings; not VSA-only vs.
   learned.
2. **Cleanup memory cost.** Querying a VSA bundle requires a "cleanup" step
   that compares against a codebook of stored items. This is a O(N·D)
   operation per query (N items, D bits), competitive with FAISS-style
   approximate nearest neighbor on dense vectors but not obviously faster.
   The user's own popcount Hamming primitives benchmark at ~16 cycles per u64
   (per the comments in `util.rs`); fast, but not free.
3. **Compositionality vs. similarity is empirically subtle.** Plate himself
   showed (1995) that HRR composition produces vectors whose similarity
   structure tracks the surface composition, not the deep semantic
   composition, unless you stack multiple bind-bundle layers — at which
   point the *role* vectors themselves need to be tuned, and tuning them is a
   rediscovery of the learned-embedding problem one level up. The literature
   resolves this with hybrid systems (HD-LLM, HRR-LSTM, Eliasmith's NEF).
   None are training-free.
4. **The benchmarks were not built around it.** GLUE, SuperGLUE, MMLU, etc.,
   are built to reward parametric memory and surface-form generalization.
   They penalize compositionality and reward scale. VSA's natural strengths
   (compositional generalization, OOD systematicity) are tested by SCAN,
   COGS, gSCAN, CLEVR-CoGenT — much smaller benchmarks with much less
   ecosystem pull.

The honest reading: VSA has been *correct but irrelevant* for most of NLP
because most of NLP's measured task is content retrieval, not composition.
The user's claim is interesting precisely because it points at the
compositional layer specifically. Don't sell H₁/H₂ as "VSA wins"; sell them as
"on the composition-bound subset of tasks, VSA matches learned compression at
zero training cost." That is a true and useful claim.

### 5.2 Where Johnson-Lindenstrauss breaks for this use case

The JL lemma says a random orthogonal projection preserves pairwise
*Euclidean* distances up to `(1±ε)` with target dimension `D ≥ O(log N / ε²)`.
For the HDC use case we are projecting `ℝ^d → 𝔹^D` with sign quantization,
which is not the JL lemma; it is the *SimHash* (Charikar 2002) or
*sign-random-projection* lemma.

SimHash preserves *cosine* similarity (not Euclidean): the Hamming distance
of the binary projections is `D · θ(x, y) / π` in expectation, where
`θ(x, y)` is the angle between `x` and `y`. So `cos_HD ≈ 1 − 2θ/π`. This is
**monotonic** in cosine but **not equal**. For high cosine (≥0.9), the binary
Hamming form *compresses* the dynamic range — pairs that are 0.99 vs 0.95
cosine in real-valued space land at very similar Hamming distances after
projection at D=8192. This is the **resolution ceiling of binary HDC**.

Practical consequence: HDC is poorly suited to discriminating *fine-grained*
similarities at the high end. The L-shape's reported 0.95 vs 0.99 distinctions
will *not* survive a binary HDC encoding cleanly. Two responses:

(a) Accept it. Argue the fine-grained distinctions are themselves artifacts
    of the L-shape's mode collapse (§2.4) and were never real signal.
(b) Increase D. Going from D=8192 to D=65536 buys 3 more bits of resolution
    at 8× memory. Probably not worth it for the experiment, but available.

### 5.3 Information preservation vs downstream utility

The user's framing conflates two different claims and the conflation is
load-bearing.

**Information preservation (true and easy).** Random projection preserves
information up to a `log` factor. Binary HDC is information-preserving in
this sense to the resolution ceiling.

**Downstream utility (separate empirical question).** Whether a *downstream
task* — relationship classification, retrieval, NLI — trained on or routed
through the HDC representation is competitive with a learned dense
representation is a different question. The answer is task-dependent and
threshold-dependent. The H₂ experiment in §4 is the one that tests utility,
not H₁.

The doc-level discipline: **never claim "HDC preserves the relationship"
as evidence for "HDC is useful for the relationship-detection task."** The
two are different empirical questions with different experimental
protocols. The user's previous repos (notably semfield) drift into this
conflation; the falsifiability analysis exists to keep it from happening
again.

### 5.4 BREAD's holonomy result and HDC's representational ceiling

BREAD reports `H ~ n^0.028`, R²=0.98 — transformer paths exhibit a small but
**nonzero**, **nonlinear**, and **path-dependent** holonomy as token positions
vary. The holonomy is the integral of the connection 1-form along the path; it
is provably zero iff the connection is flat (curvature 2-form vanishes
identically). BREAD's measurement is that the connection is *not* flat.

HDC's bind operation, in the binary regime, is `⊕` (XOR), which is
commutative and associative. Its action on hypervectors generates an abelian
group (specifically, the additive group of GF(2)^D, of order 2^D). The
permutation operation `rotate_left(_, k)` for a fixed k commutes with itself
under composition (Z_D action), and rotation-by-k composed with rotation-by-j
is rotation-by-(k+j). Two different rotation amounts commute. Therefore the
HDC group of operations *under bind+rotate* is an abelian semidirect product
GF(2)^D ⋊ Z_D — still abelian.

**Consequence.** The connection that HDC implements has trivially zero
curvature on any closed loop. HDC can therefore represent *only* the
**abelian / locally-linearizable** subspace of whatever transformers
represent. BREAD's measurement says transformers are *not* purely in that
subspace.

**What this rules out.** Compositional structures that genuinely require
non-commutative composition — e.g. word order in non-projective syntactic
constructions, anaphora resolution where the binding depends on traversal
order, certain modal-logic compositions — cannot be exactly represented by
flat HDC. The "training-free" framing therefore has a hard ceiling: the
gradient-descent component of an end-to-end model is doing *at least* the
work of selecting the non-abelian correction that flat HDC cannot supply.

**What this does not rule out.** Most pairwise-similarity tasks, including all
9 of the L-shape's relationship types, are at most weakly non-commutative
(SIMILAR is symmetric; HIERARCHICAL is anti-symmetric; PART_OF is
anti-symmetric; etc.). Anti-symmetric relations *can* be represented in
abelian HDC by using *distinct* left/right roles (Plate's standard
construction; this is precisely what H₁ does). The hard-ceiling tasks are
ones where order matters in a way that doesn't reduce to a single
left/right axis — which the user's L-shape relationship dataset does not
contain. So BREAD's curvature does not refute the experiment; it bounds the
generality of any positive result.

### 5.5 Does bind + bundle carry enough information to distinguish 9 classes?

User's heuristic concern: `bind(x, y) = x ⊕ z = bind(x, z')` where
`z' = z`. The identity is true (XOR is its own inverse), but it doesn't carry
the implication the user is worried about. Here is the precise math.

Two pairs in the *same* class produce `phi(x_a, y_a, i)` and `phi(x_b, y_b, i)`.
Their similarity under `cos_HD` is

```
cos_HD(phi_a, phi_b)
  = cos_HD(x_a ⊕ r_L ⊕ y_a ⊕ r_R, x_b ⊕ r_L ⊕ y_b ⊕ r_R)
  = cos_HD(x_a ⊕ y_a, x_b ⊕ y_b)
```

— the roles cancel for *same-class* comparisons. This is a feature, not a bug:
it means *the in-class signature is a function of `x ⊕ y`*, i.e. the
*difference vector* of the pair. Different classes — using *different*
left/right role pairs — produce signatures that differ by `r_L^i ⊕ r_L^j ⊕
r_R^i ⊕ r_R^j` between class i and class j. With nearly-orthogonal random
role vectors, this XOR has Hamming weight ≈ D — a large gap, easily
discriminable.

**So the algebra works for the prototype-readout architecture in §4.3.** What
it does *not* tell you is whether `x ⊕ y` is a useful per-class signature.
That depends on whether the L-shape's classes are characterized by
direction in the embedding space:

- SIMILAR: `x − y` ≈ 0 (for cosine; small in any norm). XOR ⇒ near-zero
  Hamming. Class signature ≈ `r_L ⊕ r_R`.
- DISSIMILAR: `x − y` large. XOR ⇒ near-D/2 Hamming. Signature is
  near-uniform.
- HIERARCHICAL / PART_OF: directional, `y − x` lives in a class-characteristic
  subspace. XOR is a coarse proxy for that direction.

Whether *XOR* is a good proxy for *vector difference* in `ℝ^d` is the deep
question. SimHash literature says: yes, in expectation, for the bit-
collisions rate; no, in detail, for the geometry of the difference. The H₁
experiment is exactly the test of how well the proxy holds for the L-shape's
relationship classes specifically.

**So the heuristic does *not* refute H₁ but it bounds what success means.**
A positive H₁ result tells you: *the difference of binary projections is a
sufficient class signature for these 9 classes*. It does *not* tell you that
HDC is doing anything beyond random projection followed by majority vote —
which, for what it's worth, would be a perfectly respectable closed-form
baseline, just not a particularly novel one.

### 5.6 Bundle ≠ XOR-bundle (correctness footnote)

XOR-bundle in binary collapses pairs to zero: `a ⊕ a = 0`. Bundling more than
~2 vectors via XOR rapidly destroys information in a way that bipolar
*majority* bundling does not. The repo's `bundle_majority` (in `sheaf.rs`) is
the correct primitive. Any implementation that calls `xor_into` repeatedly
to build the prototype `q_i` is *wrong*; this is a common pitfall in early
HDC code. Pin this in the experiment binary.

---

## 6. Open questions for the user

❓ **Q1 (load-bearing).** *Where is the "inference interruptor in llo"?* I
searched `rs/ll-open/cli-lib/src/{daemon,cmd_lsp.rs}`, `rs/ll-open/lsp/src/`,
and the workspace `Cargo.toml`s for any LLM substrate (candle, llama,
tokenizers) or any string match for `interrupt`/`interruptor`/`intercept`. No
hits in this repository. Possibilities:

1. It's in a different repo (`signet`? `rosary`? a private branch?).
2. It's planned but not yet landed.
3. The phrase refers to a generic LSP cancellation hook, not an LLM
   primitive.

If H₂ is to be tested in this repo, we need either to land that substrate
first (probably via candle + a Gemma-270M GGUF) or to reduce the experiment
to H₁ alone for now. The §4 protocol covers H₁/H₂-classifier-style on the
L-shape data; the *small-LLM-routed-through-HDC* version of H₂ is a separate
experiment.

❓ **Q2.** *Are the 9 L-shape relationship classes the right benchmark for
H₁?* Three of them (SIMILAR, HIERARCHICAL, DISSIMILAR) have statistical mass.
The other six have n=1–3. Do you want me to (a) run on those three only,
(b) augment with synthetic balanced data, (c) move to a bigger compositional
benchmark like the SCAN test split, or (d) all of the above?

❓ **Q3.** *Have you re-measured the L-shape's discriminative metric?* The
published numbers are cosine similarity, which the
`lshape_manifesto_results.json` data confirms is degenerate (selectivity ≈ 0).
H₂ requires the L-shape's AUC and top-k retrieval as a baseline. Do you have
that, or do I include "re-evaluate the L-shape on the discriminative metric"
as part of the protocol? (Adds ~1 day of work; cleanly separates the
analyses.)

❓ **Q4.** *What's your acceptance threshold for "H₃ is worth the next
experiment"?* Concretely: if H₁ reports macro-AUC = 0.65 (well above
chance, well below the L-shape's hypothetical AUC of, say, 0.80), is that
enough to motivate the H₃ / SMOGE-replay experiment? Or do you want the
H₁ bar to be *parity* with the L-shape before investing in H₃?

❓ **Q5.** *On the BREAD-holonomy/HDC-flatness ceiling (§5.4).* I claim HDC
is the abelian approximation of whatever BREAD measures. Do you read this as
(a) "HDC is the linearization of BREAD's connection" (which would suggest
HDC + a small non-abelian residual could replicate transformers), or (b) "HDC
and BREAD are independent measurements of different structural layers"
(which suggests the two should be combined in a Tri-Brain-style architecture
rather than one replacing the other)? The architectural implication of H₃
depends on this read.

❓ **Q6.** *Tangram primitive integration.* You flagged that the tangram
primitive in `semfield/physics_tokenizer/tangram.py` matters but the
surrounding consciousness layer doesn't. Do you want the §4 experiment to
incorporate tangram-style boundary detection (different binding roles for
"global / cluster / local" regions of the embedding), or keep the H₁
experiment minimal and treat tangram-HDC as a separate follow-on? My default
is the latter — H₁ should isolate the bind/bundle/role primitives without
adding regional-binding complexity — but the architectural payoff lives in
combining them.

---

## Appendix A — quick reference for §4 implementer

| Need | Existing primitive | Source |
|---|---|---|
| `E_src` (random projection) | `expand_seed(blake3_seed(bytes))` | `hdc/src/util.rs:151` |
| Bind | `xor_into(&mut hv, &other)` | `hdc/src/util.rs:59` |
| Permute | `rotate_left(&hv, k)` | `hdc/src/util.rs:73` |
| Cosine via Hamming | `popcount_distance(&a, &b)` then `1 - 2*d/D_BITS` | `hdc/src/util.rs:101` |
| Role vectors | `tagged_seed_vector("relation-{name}-left", 0)` | `hdc/src/util.rs:176` |
| Majority bundle | `bundle_majority` (if not pub, expose) | `hdc/src/sheaf.rs` |
| `Restriction::RotateLeft` | for sheaf-style alignment if H₁ extends | `hdc/src/sheaf.rs:170` |

All primitives are deterministic, cross-machine reproducible, and pinned by
existing tests. The experiment can be a thin layer on top.

## Appendix B — citation anchors

- Plate, T. A. (1995). Holographic Reduced Representations. IEEE TNN.
- Kanerva, P. (1996). Binary spatter-coding of ordered K-tuples.
- Charikar, M. (2002). Similarity estimation techniques from rounding
  algorithms (SimHash).
- Eliasmith, C. et al. (2012). A large-scale model of the functioning brain
  (Spaun).
- Lake, B. & Baroni, M. (2018). Generalization without systematicity (SCAN).
- Schlegel, K., Neubert, P., Protzel, P. (2022). A comparison of vector
  symbolic architectures.
- BREAD `experiments/2026-01-14_geometry-of-reasoning-paper/paper.md`
  (`H ~ n^0.028`, R²=0.98).

— end —
