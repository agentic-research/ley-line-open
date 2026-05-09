# Decade: HDC as a Unified Translation Layer for Language Representation

**Status:** Proposed (V0 falsifiability experiment locked, pending bead dispatch)
**Author:** James Gardner + Claude + Gemini cross-review
**Date:** 2026-05-09
**Channel (per BDR/Harmony lattice):** `analysis` — decade-level architectural reasoning
**Sibling docs:**
- `docs/decades/T8/hdc-translation-layer-falsifiability.md` — math-friend's falsifiability analysis (input)
- `rs/ll-open/hdc/README.md` — existing HDC primitives (substrate)
- `docs/decades/2026-merkle-cas-substrate.md` — Σ decade (parallel architectural primitive; this decade does not depend on Σ)

---

## 0. One-line claim

> A single HDC hypervector — generated via deterministic algebra over BPE-tokenized text, **no learned parameters** — supports both dense semantic-similarity readouts and symbolic compositional-decomposition readouts at moderate quality. If true, this is a unification of two pipeline branches (dense embedding pipelines vs. symbolic NLP pipelines) that today exist in mutual exclusion.

This decade is independent of the Σ Merkle-CAS decade. Σ governs how state advances atomically across hosts; this decade governs how *language itself* is represented before it enters any pipeline. Both are substrate primitives; neither subsumes the other.

---

## 1. The Core Hypothesis

Today's pipelines force a choice:

- **Dense pipelines (SBERT, sentence-transformers, GPT hidden states):** good for semantic similarity. Compositional structure is irreversibly absorbed into the dense vector at training time and cannot be queried back out without a separate model.
- **Symbolic pipelines (ASTs, parse trees, morphological analyzers):** good for exact compositional decomposition. Dense semantic similarity is unavailable except via re-embedding.

**The claim:** HDC provides a representation primitive where a *single* hypervector supports *both* readouts from *the same bytes*, with **zero learned parameters in the encoding step**. The two readouts are deterministic functions of the same HDC vector.

Why this matters if true:
- Compositional decomposition becomes free for any dense embedding pipeline that adds an HDC layer.
- Small models with HDC structural input can match larger models on compositional tasks (the structure is in the algebra, not the parameters).
- Neurosymbolic architectures gain a training-free symbolic substrate.

Why this might be false:
- Hash-based codebooks have **zero semantic prior** — `HV("cat")` and `HV("feline")` are mathematically orthogonal. Only sub-word token overlap drives dense similarity. SBERT's contrastive training shapes "cat" ≈ "feline"; we cannot reproduce that without training.
- Bundle capacity at D=10000 caps composition depth. Beyond ~D/16 components, noise dominates signal.

---

## 2. Ex-Ante Commitments

To avoid the garden of forking paths, every architectural and evaluation choice is locked **before any code runs**. Math-friend's prior analysis (`docs/decades/T8/hdc-translation-layer-falsifiability.md`) called this out as the load-bearing discipline; we comply.

### 2.1 Vector primitives

| Knob | Locked value |
|---|---|
| Dimensionality `D` | 10000 |
| Element type | Bipolar `{-1, +1}` |
| Bind operation | Element-wise multiplication |
| Bundle operation | Element-wise sum followed by sign-thresholding (majority vote) |
| Permutation `Π` | Cyclic shift by integer offset |
| Similarity metric | Cosine (equivalent to normalized dot product on bipolar) |

### 2.2 Codebook

| Knob | Locked value |
|---|---|
| Tokenizer | GPT-2 base BPE (50,257 tokens) — matches `sentence-transformers/all-MiniLM-L6-v2`'s scope |
| Codebook generation | `BLAKE3(BPE_token_utf8_bytes) → seed → bipolar HV ∈ {-1, +1}^D` |
| Learned parameters | **Zero**. Codebook is fully determined by the token text + BLAKE3 + seed expansion. |
| Reproducibility | Bit-exact. A fresh checkout produces the identical codebook. |

### 2.3 V0 encoder (α-family: positional cyclic-shift at sub-word, BoW above)

```text
sub-word level:   HV(word)     = Σ_j Π^j ( HV(bpe_j) )
sentence level:   HV(sentence) = Σ_i HV(word_i)              ← BoW; no Π
document level:   HV(doc)      = Σ_k HV(sentence_k)          ← BoW
```

Mechanism:
- Permutation lives **only at the sub-word level**. It captures morphological order ("cat" ≠ "act") without imposing absolute sentence positions that would tank shift-equivalent paraphrase similarity (an analysis-killer for STS-B; see Forfeits §4).
- Sentence and document use plain bundling (Bag-of-Words). Cosine of two sentence HVs reflects sub-word token overlap statistics.

### 2.4 Falsifiability thresholds

The experiment succeeds **only if both thresholds pass simultaneously, from the same HDC encoding pass**.

| Readout | Metric | Threshold | Justification |
|---|---|---|---|
| Dense | Spearman ρ on STS-B test split | `ρ ≥ 0.553` | `0.7 × all-MiniLM-L6-v2`'s `ρ = 0.79`. Proves training-free algebra captures a mathematically significant fraction of supervised contrastive geometry without claiming parity. |
| Symbolic | Top-1 BPE-token recovery at depth-3 | `acc ≥ 70%` | Chance ≈ `1/50257 ≈ 0.00002`. 70% is dramatically above chance and well below the theoretical ceiling (~99% at D=10000 for k=3 bundles). Defensible floor. |
| Same-bytes invariant | Both readouts produced from one `Vec<i8>` per text input | Bit-exact | Falsifies the "two systems in a trenchcoat" failure mode. Encoder runs once; both readouts are pure functions of its output. |

### 2.5 Evaluation corpora

| Readout | Corpus | Held-out |
|---|---|---|
| Dense | STS-B test split (1379 sentence pairs, public benchmark) | N/A — we never train; the test split is just the evaluation set |
| Symbolic | 1000 depth-3 BPE compositions sampled from CC-100 English | Yes — sampled with a deterministic seed; codebook never sees them during construction (it's hash-based, so this is automatic) |

Comparison rig: mirror the `~/github/jamestexas/lossless/` evaluation harness so we inherit the same SBERT-as-baseline computation. Re-using their fixture eliminates one common source of "did you measure differently from the baseline" critique.

---

## 3. Falsifiability Conditions

For each threshold from §2.4, what would refute the claim:

**Dense (`ρ < 0.553`):** algebraic encoding fails to capture enough distributional signal to even approach SBERT. Refutes the unified-rep claim's dense leg. Likely cause: hash codebook's zero semantic prior is too weak; SBERT's training is necessary, not redundant.

**Symbolic (`acc < 70%` at depth-3):** bundle capacity at D=10000 is insufficient or codebook collisions are too frequent. Refutes the unified-rep claim's symbolic leg. Likely cause: bundle noise floor dominates earlier than theory predicts.

**Same-bytes (separate encodings for each readout):** trivially refutes the architectural claim — it would mean we proved two unrelated systems work, not one unified primitive.

**Failure modes that look like success but aren't** (each is gated explicitly):

| Failure mode | Gate |
|---|---|
| p-hacking on encoder design (try 100, report best) | Encoder is locked in §2.3 before any measurement |
| p-hacking on STS-B (eval on training-adjacent data) | Use STS-B test split only, never the dev split during development |
| Symbolic recovery via accidental codebook proximity | Held-out compositions sampled from outside the natural depth-3 space |
| "We just trained the projection" | Projection is `Random R` per JL lemma, fixed at decade publication, **never** updated during evaluation |
| L-shape-style mode collapse | Effective rank of encoded sentence matrix `≥ 2048` (≥ 20% of D) |

---

## 4. Explicit Forfeits (V0 Limitations)

V0 is **deliberately narrow** to maximize falsifiability. The following are accepted limitations:

1. **Sentence-level word order is invisible.** "Dog bites man" and "Man bites dog" produce identical HVs. V1 layers in shift-invariant bigram binding to address this; V0 does not.
2. **No word-level positional recovery.** Symbolic queries are restricted to "the j-th BPE token within word i" (sub-word position). Sentence-level "the 3rd word" is unavailable.
3. **No syntactic role recovery.** V0 cannot isolate grammatical roles (subject, verb, object). That requires Plate-style role-filler binding which depends on a parser. Out of scope; V2 territory.
4. **No semantic similarity beyond surface BPE overlap.** "cat" and "feline" are orthogonal in our codebook. SBERT-style synonym similarity is unattainable without training. Accepted.
5. **No generation.** This is a representation primitive, not a language model. We do not claim HDC generates text.

If V0 passes both thresholds, V1 (bigram extension) and V2 (parser-aware role-filler) become legitimate next steps. If V0 fails either threshold, layering more complexity on top is unlikely to save it — the failure indicates HDC algebra cannot carry the claim, not that we need more terms.

---

## 5. Implementation

### 5.1 Location

New crate: `rs/ll-open/hdc-translate/`

Builds on `leyline-hdc` (existing primitives at `rs/ll-open/hdc/`):
- `Hypervector` type (D=10000, bipolar)
- `bind`, `bundle`, `permute`, `cosine` operations
- `HvCell` sheaf-stalk types (not used in V0, but already in the crate)

New code in `hdc-translate/`:
- `codebook.rs` — BLAKE3-seeded BPE codebook. Deterministic.
- `tokenizer.rs` — GPT-2 BPE wrapper (Rust crate `tokenizers`).
- `encoder.rs` — V0 encoder (sub-word permutation + sentence/document BoW).
- `readout_dense.rs` — JL projection ℝ^D → ℝ^d (d=384, matching MiniLM output dim) + STS-B harness.
- `readout_symbolic.rs` — depth-3 composition recovery harness against CC-100 sample.
- `bin/falsify.rs` — single binary that runs both readouts on a corpus and exits non-zero if either threshold is missed.

### 5.2 CI integration

- `cargo test -p leyline-hdc-translate` runs unit tests on encoder operations (round-trip, Hamming distances, bundle capacity).
- `cargo test -p leyline-hdc-translate --test falsify` runs the actual experiment. Exits non-zero if any threshold fails. Treated as part of `task ci` once committed.
- Test fixtures (STS-B test split + 1000 depth-3 compositions) committed under `rs/ll-open/hdc-translate/tests/fixtures/` for bit-exact reproducibility.

### 5.3 Reproducibility

- **No randomness anywhere except codebook seed.** Codebook seed locked at 0 for V0.
- Fresh checkout → identical codebook → identical encoder output → identical readouts → bit-equal pass/fail.
- The `falsify` binary's output (Spearman ρ, recovery acc, effective rank) committed as a single JSON file per run; comparison against threshold is purely numerical.

### 5.4 What is *not* implemented in V0

- No tie-in with `Controller::current_root` or Σ root advance. HDC encodings are not part of the substrate event log. (V3 territory: hash HDC encodings as Σ events.)
- No tie-in with `interrupt.rs::REDIRECT` for inference-time injection. (V4 territory: prove the claim *also* helps a running LLM.)
- No multi-language. English only. CC-100 has multilingual data but we restrict to `en` for V0.

---

## 6. Bead Arc

The decade decomposes into three threads, each with one bead-pack. Beads filed at decade publication; only T1 dispatches immediately, T2 gates on T1 success, T3 gates on T2 success.

### T1 — Build V0 encoder + readouts

- T1.1 — `hdc-translate` crate scaffold + BLAKE3-seeded BPE codebook + unit tests
- T1.2 — V0 encoder (sub-word permutation + sentence/document BoW)
- T1.3 — Dense readout: JL projection + STS-B harness + Spearman computation
- T1.4 — Symbolic readout: depth-3 BPE recovery harness against CC-100 sample
- T1.5 — `falsify` binary tying everything together; test fixtures committed
- T1.6 — Effective-rank invariant test (mode-collapse regression pin)

Acceptance: `cargo test -p leyline-hdc-translate` green; `cargo test --test falsify` runs end-to-end (pass or fail; we don't pre-judge).

### T2 — Falsification run

- T2.1 — Run `falsify` on STS-B; record `ρ`
- T2.2 — Run `falsify` on CC-100 depth-3 sample; record `acc`
- T2.3 — Verify same-bytes invariant via instrumentation
- T2.4 — Write up results: pass/fail per threshold, effective rank, comparisons against random-projection baseline (sanity check that we beat random)
- T2.5 — File followup beads for V1 if both pass; file `closed: refuted` write-up if either fails

### T3 — V1 (conditional on T2 pass)

- T3.1 — Add bigram binding layer at sentence level
- T3.2 — Re-run STS-B with the bigram extension; ensure τ_dense still passes
- T3.3 — Add new symbolic claim: word-bigram-partner recovery; threshold TBD
- T3.4 — Decide V2 (parser-aware role-filler binding) or stop

If T2 falsifies, T3 does not run. The decade publishes the negative result.

---

## 7. Out-of-scope (deliberate non-goals)

- Replacing transformers. HDC has trivial holonomy; transformers have non-trivial curvature (BREAD). HDC is a representation primitive, not a model.
- Documentation storage / knowledge graphs without embeddings. Adjacent topic, separate decade if we pursue it. The author's note: this experiment is the proof-of-concept *that would license* such a downstream.
- Cross-runtime fixtures (mache, control-room). V0 is Rust-only. If V0 passes and V1 ships, we revisit cross-runtime.
- Generation. HDC isn't generative natively.

---

## 8. References

- Plate, T. (1994). *Holographic Reduced Representations*. Distributed representations for cognitive structures.
- Kanerva, P. (1996). *Hyperdimensional Computing*. Foundational vector-symbolic-architecture paper.
- Bojanowski, P. et al. (2016). *Enriching Word Vectors with Subword Information* (FastText). The non-trained-but-not-this-experiment baseline.
- Reimers, N. & Gurevych, I. (2019). *Sentence-BERT: Sentence Embeddings using Siamese BERT-Networks*. The trained baseline.
- `docs/decades/T8/hdc-translation-layer-falsifiability.md` — math-friend's prior analysis. This decade implements a V0 narrower than what that doc proposes; intentional, per the (b) Moderate threshold commitment.
- `~/github/jamestexas/lossless/LSHAPE_ANALYSIS_REPORT.md` — the L-shape work this experiment supersedes.
- Johnson, W. B. & Lindenstrauss, J. (1984). The lemma that licenses the dense readout's projection step.

---

## Status / Authorship trail

- 2026-05-09: V0 ex-ante commitments locked (this document)
- Cross-reviewed by Gemini (encoder strategy: pushed back on absolute permutation at sentence level; correct catch, BoW adopted)
- Cross-reviewed by Claude/math-friend (falsifiability framing: pushed back on cosine-as-primary-metric; AUC + retrieval-class metrics adopted in earlier draft, dropped after Gemini's Jaccard-overlap argument re-prioritized cosine for THIS specific encoder choice)
- Pending: math-friend re-review against this final V0 commitment
