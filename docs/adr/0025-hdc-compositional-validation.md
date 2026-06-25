# ADR-0025 — HDC compositional-vs-distance use modes: validate or remove

**Status:** Proposed (2026-06-25)
**Bead:** (to be filed on acceptance)
**Related:** ADR-0024 (HDC substrate-identity rewrite — v0.5.0); `rs/ll-open/cli-lib/tests/phase_0b_real_ground_truth.rs`

---

## Context

ADR-0024 documented the v0.5.0 substrate-identity rewrite (bundle composition, drop `content_role`, fp-quantize, seeded leaves) and the Phase 0B-real empirical validation. That work resolved the *substrate-identity* question: under graded distance retrieval, HDC is a real signal (2.30× random) that adds **+7.7%** over vec-alone under weighted score-fusion (kernel-RBF, α=0.40).

The question ADR-0024 *did not* answer — and this ADR is here to: **is HDC actually useful beyond a vec-shaped distance retriever?**

### The honest framing of current HDC use

Today the substrate uses HDC as **one of three Kanerva-canonical operations and ignores the other two**:

| Kanerva operation | What it does | Used in v0.5.0? |
|---|---|---|
| **Bundle** (majority vote) | Set-like composite that stays similar to members | ✓ — drives bundle composition |
| **Bind** (XOR) | Associate variable-value pairs; *invertible* via unbind | **REMOVED in v0.5.0** (content_role retirement) |
| **Permute** (rotation) | Encode order; sequences via composed permutes | partially — used for child position only, not for sequences |

Plus the canonical post-encode operations native HDC depends on:

- **Item memory / clean-up codebook** — decode noisy composites by lookup against known archetype HVs. **Not used.**
- **Compositional query** — agent constructs query HVs at runtime via bind+bundle, retrieves by similarity to encoded composites. **Not used** (no bind algebra to build queries against).
- **Sequence retrieval** — encode call sequences via permute, retrieve by sequence-HV similarity. **Not used** (no sequence encoding today).

The v0.5.0 rewrite optimized HDC to be a **better distance-retrieval embedding** — Phase 1's graded distances confirm that worked. But by doing so it sacrificed the operations that make HDC genuinely *different* from a dense vector embedding. Phase 0B-real measures the weakest HDC mode (popcount-Hamming retrieval over flat composites), and the +7.7% lift is the lower bound of HDC's value-add under that mode.

The +7.7% is **not rock bottom**. It's a single point on a 5-dimensional measurement space:

| Axis | What Phase 0B-real tests | Coverage |
|---|---|---|
| Combination architecture | RRF, score-fusion, kernel-RBF, prototype-classify, HDC→vec, vec→HDC + α-sweep | well-explored |
| Ground truth shape | function-name prefix (lexical) | 1 value |
| Corpus | LLO daemon ~400 functions | 1 value |
| HDC feature richness | v0.5.0 encoder (bundle + seeded leaves) | 1 value |
| **HDC use mode** | **distance retrieval over flat composites** | **1 value (the weakest one)** |

The fifth axis is the one this ADR exists to test.

## Decision

A four-phase research arc to falsify "HDC has compositional value beyond distance retrieval." Each phase has a deliverable; each has explicit go/no-go criteria; the final phase has pre-registered falsification thresholds.

### Phase α — Restore bind for explicit roles, layered on the current substrate

Re-introduce explicit role hypervectors (`ROLE_KIND`, `ROLE_RETURNS`, `ROLE_NAME`, `ROLE_PARENT`, `ROLE_BODY`, etc.) as sparse-random constants, but layered:

- Current bundle composition stays as the *structural channel* → drives distance retrieval (preserves Phase 1's graded distances + Phase 0B-real's +7.7% fusion lift).
- New role-bind builds a *compositional channel* → enables compositional query.
- Each encoded function gets TWO hypervectors in the substrate: `dist_hv` (today's) + `compositional_hv` (new).

The pre-v0.5.0 single-HV-with-content_role was wrong because it tried to do both jobs with one HV; the unbind leaked content into the structural axis. Two channels keep them separable.

**Deliverable.** Updated `leyline-hdc` encoder emitting both HVs per node; capnp schema extension (`compositional_hv` field on the existing HV record); migration path for v0.5.x consumers (compositional_hv is optional, no break to distance-retrieval consumers).

**Go/no-go.** Phase 1 distance gates stay green; Phase 0B-real fusion-sweep still clears vec-alone by ≥ 0.02. If either regresses, the dual-channel design is wrong and Phase α restarts.

### Phase β — Item memory / archetype codebook

Curate a small set of canonical function archetypes (parser, scanner, request-handler, validator, state-mutator, factory, etc.) as deliberately-constructed archetype HVs. Add a `classify_archetype(function_hv) → (archetype_id, confidence)` operation: find the archetype HV closest to the input, with confidence derived from margin to second-nearest.

This is the test Phase 0B-real's prototype-classify *should* have been. The bag-of-exemplars version died at 44.4% group-pick accuracy; a curated codebook with explicit archetype HVs + clean-up margin is the actual native-HDC classification pattern.

**Deliverable.** `rs/ll-open/hdc/src/codebook/archetypes.rs` with the initial archetype set + the classify operation; tests against a small hand-labeled held-out set.

**Go/no-go.** Classify accuracy on held-out labeled set is significantly above vec-semantic-search-for-archetype-name baseline. Pre-register threshold: archetype-classify ≥ vec-baseline + 15 points absolute on a held-out set of N ≥ 100 labeled functions.

### Phase γ — Sequence encoding via permute

Encode call sequences `foo→bar→baz` as `bundle(permute(foo_hv, 0), permute(bar_hv, 1), permute(baz_hv, 2))`. Add a `find_sequence(seq_hv, k) → matching_callers` query that returns functions whose call sequences are HDC-similar to the query.

**This is the operation vec structurally cannot express.** Sequences aren't expressible as cosine over flat embeddings without losing order or losing length-invariance. If HDC adds value anywhere uniquely, sequence retrieval is the most likely place.

**Deliverable.** Sequence-encoding pass in LLO (alongside the existing per-function encoding); MCP tool `find_sequence` on mache exposing the query.

**Go/no-go.** Sequence retrieval beats vec-cosine-over-concatenated-call-strings by a significant margin on a held-out test set of known call-sequence patterns. Pre-register threshold: recall@10 ≥ vec-baseline × 1.30 on a held-out set of N ≥ 50 canonical call sequences.

### Phase δ — Agent-facing compositional query API

MCP tool `compositional_query(role: str, kind: str, returns_type: str, ...) → matches`. The handler constructs the query HV at runtime via bind+bundle of the role HVs, retrieves against `compositional_hv` (Phase α's new channel).

**Deliverable.** New tool on mache; tests on ambiguous-fixture corpus showing the query disambiguates by composite where lexical similarity alone is ambiguous.

**Go/no-go.** On a corpus where the lexical signal is structurally insufficient (e.g., functions sharing role+kind but with distinct vocabulary), compositional_query recovers the correct composite at significantly higher precision than vec-semantic-search. Pre-register threshold: precision@10 ≥ vec-baseline + 20 points absolute on a held-out set of N ≥ 50 compositional queries.

### Phase ε — Decision

After Phases α-δ have shipped (or any of β/γ/δ has failed its go/no-go), the substrate makes a load-bearing decision based on the empirical record:

1. **Compositional value confirmed** (β AND/OR γ AND/OR δ passes its threshold)
   → **Keep HDC, invest fully.** ADR-0025 amendment documents which compositional uses earned their keep; v0.6 substrate ships with the compositional surface as a first-class agent capability.

2. **No compositional value, fusion lift holds** (Phase 0B-real's +7.7% still measurable on the dual-channel encoder)
   → **Keep HDC as the cheap second voice in fusion.** Position explicitly: "vec is primary; HDC adds modest lift at near-zero per-query cost." Drop the compositional surface; keep the encoder + the distance retrieval; deprecate any roadmap items premised on compositional value.

3. **Neither compositional nor fusion value** (β/γ/δ all null AND fusion no longer measurable)
   → **Clean removal.** Delete `leyline-hdc` crate, the codebook, Phase 0/0B/0C/1/0B-real test suite. ADR-0026 documents the removal + the post-mortem. Vec-alone becomes the substrate's only retrieval path. Honest blog story: tested it, didn't earn its place.

The decision is *pre-committed* to the empirical record. No "well, but the architecture is interesting" rationalization. The thresholds in β/γ/δ are pre-registered before running; if they don't clear, the substrate goes to outcome 2 or 3 mechanically.

## Cost / value framing

Why removal is acceptable if Phase ε is null: HDC carries real maintenance cost in the substrate (the `leyline-hdc` crate, the codebook, the Phase 0/0B/0C/1/0B-real test discipline, the substrate-identity ADR-0024 work). That cost is justified ONLY by value HDC delivers that vec cannot. Without compositional value, the +7.7% fusion lift is the only thing HDC measurably adds.

**Per-item cost comparison** (current measurements):

| | HDC | vec (fastembed/MiniLM-L6) |
|---|---|---|
| Per-item storage | 8192 bits = 1 KB | 384 floats × 4 = 1.5 KB |
| Encoding cost | ~μs (tree walk + bundle) | ~10-100 ms (model inference) |
| Distance per comparison | ~100 ns (popcount-Hamming) | ~μs (cosine over 384 floats) |
| Model artifact | 0 MB | ~100 MB |
| Cold start | 0 ms (deterministic) | 1-3 s (model load) |
| External dependency | none | model download / ship in binary |

The cost gap is real (~10-100× cheaper encoding, ~10× cheaper comparison, +100MB-less model footprint). But a cheap delivery vehicle for nothing is still nothing — cost savings only matter if HDC delivers value vec cannot.

**Where cost has more weight than this ADR is giving it:** wasm / edge / embedded deployments. If mache, LLO, or downstream consumers ever ship into workerd, control-room Swift, or any environment where 100MB of model + ONNX runtime is a non-starter, HDC-only retrieval becomes the deployment-pragmatic choice at -29% recall vs vec. As of 2026-06-25 no such consumer is queued; the cost argument is conditional on that future case.

## Phasing constraints

- **Phase α is the foundation; β, γ, δ all depend on it.** The dual-channel encoder must ship before any compositional test can run.
- **Phase β is cheapest** (codebook curation + classify; no schema changes beyond α). Run it first to maximize information-per-unit-effort.
- **Phase γ requires ingestion-side work** (call-sequence extraction); higher cost.
- **Phase δ requires agent-side work** (new MCP tool + query construction); moderate cost.
- **Phase ε is the decision; not a build.**

Suggested order: α → β → (decide whether to continue if β is null) → γ → (decide) → δ → ε.

## Rejected alternatives

### Stay at v0.5.0; declare +7.7% the final answer

Rejected. Phase 0B-real explicitly noted the +7.7% as a single-axis floor; ADR-0024 closed *with* an open-questions list naming higher-granularity classify, structural ground truth, and richer features as untested. Treating +7.7% as authoritative would be motivated reasoning against ADR-0024's own caveats.

### Skip the validation; just remove HDC now

Rejected. The +7.7% fusion lift is real and reproducible. Removing without testing the compositional axis throws away both (a) the existing fusion value and (b) the untested compositional capability. Phase ε's null outcome (option 3) is what justifies removal; without running the test, removal would be on aesthetic grounds.

### Skip the encoder change; just add an item memory layer on the current HVs

Rejected. Without bind for explicit roles, the item-memory classify is just bag-of-exemplars — which Phase 0B-real already ran and which already died at 44.4%. The encoder change is the prerequisite that makes a real codebook test possible.

### Add a fifth axis to Phase 0B-real (HDC use mode) without the encoder change

Rejected for the same reason as above; can't measure compositional value without the operations to build compositional queries.

## Open questions

- **Bind primitive choice.** XOR is the canonical HDC bind, but circular convolution and component-wise multiplication are alternatives with different distributivity properties. Phase α picks XOR for symmetry with the original `content_role`; revisit if the algebra blocks something.
- **Archetype curation effort.** Phase β depends on labeled archetype data. Bootstrapping the labeled set may need its own ADR. Conservative estimate: 1-2 weeks to curate ~100 archetypes + held-out labeled set.
- **Call-sequence extraction.** Phase γ needs call-sequence data from LLO's parse pass. Today the bindings log has call edges; extracting sequences (vs single edges) is its own ingestion-side question.
- **Per-query α in compositional queries.** Phase δ might also explore query-conditional α for fusion (the "query as section of sheaf" reframe from the 2026-06-24 design discussion). Out of scope for this ADR; flag if it surfaces in Phase δ.
- **Cross-language generalization.** All phases tested initially on the LLO daemon corpus (single-language, single-domain). Generalization to TS/Python/Rust at substrate level is its own test set per ADR-0023's phasing.

## Acceptance criteria

This ADR (the spec) is accepted when:
- The phase plan is reviewed and the falsification thresholds in β/γ/δ are confirmed.
- The bead thread is filed: one bead per phase, with dependencies, in mache + LLO as appropriate.
- The implementation order is committed (α → β → … unless overridden).

Phase α's implementation acceptance is in its own PR. The decision in Phase ε is documented in ADR-0026 (the empirical outcome + the keep/de-feature/remove call).

This ADR records the decision contract; per-phase deliverables live in their own PRs. The discipline is: thresholds are pre-registered HERE, before implementation, so the Phase ε decision can be mechanical against the record.
