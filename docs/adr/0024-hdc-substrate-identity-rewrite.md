# ADR-0024 — HDC substrate-identity rewrite: bundle composition, seeded leaves, fp-quantize

**Status:** Accepted (2026-06-24) — shipped in v0.5.0
**Bead:** `ley-line-open-7b5086` (decision); `ley-line-open-98ac42` (implementation)
**Related:** ADR-0016 (AI-native query surface); `rs/ll-open/hdc/src/encoder.rs`; `rs/ll-open/cli-lib/tests/phase_0_*.rs`, `phase_0b_*.rs`, `phase_0c_*.rs`, `phase_1_*.rs`, `phase_0b_real_ground_truth.rs`

---

## Context

The HDC (Hyperdimensional Computing) encoder in `leyline-hdc` v0.4.x produced hypervectors with several properties that math-friend dual review and Phase-0/0B/0C/1 empirical results converged on as load-bearing failures of the substrate's identity model:

1. **XOR-bind composition** of children into parent hypervectors. XOR is similarity-perfect-transmitting (`d(a⊕c, b⊕c) = d(a, b)` exactly) and commutative. For tree composition this is wrong — two functions whose children share kinds and structure should be MORE similar, not equally distant once they're bound to their parent. XOR-bind made the substrate a binary equality oracle: identical structures collapsed to distance 0; non-identical structures hovered near D/2 (random) with no graded similarity in between.

2. **`content_role` smearing.** Leaves carried their lexical content via an XOR-bound `content_role` hypervector that was supposed to be unbindable for projection-style queries (`unbind_child_at_position`, `explain_cluster_centroid`). In practice the unbind operations leaked the content into the structural axis, mixing semantic and structural similarity in a way neither queryable nor predictable.

3. **`canonical_signature_bytes` over-discrimination.** The fp-quantize hash that grouped canonical-shape nodes included `sorted_child_kinds`. Small AST-shape variations (an extra trailing whitespace child, a different statement ordering) fragmented otherwise-identical canonical groups. The fp-quantize was both too strict (fragmenting equivalent shapes) and structurally redundant (the shape was already encoded in the bundle composition behavior).

4. **Content-free leaves.** Leaves' hypervectors were deterministic from `(kind, identifier_id)` only — they carried no lexical fingerprint of the underlying token text. Two `fn parse_json` and `fn parse_payload` identifiers got distinct random hypervectors that shared no lexical signal. HDC had no lexical channel at all; what little lexical sensitivity Phase-0 retrieval exhibited came from secondary effects, not from any explicit lexical encoding.

### Empirical signatures of the failure (pre-rewrite)

- **Phase 1 (function-granularity retrieval):** distances clustered tightly around D/2 for any pair that wasn't a binary-exact match. The substrate behaved as an equality oracle, not a graded similarity surface.
- **Phase 0B (Jaccard agreement vs vec):** HDC and vec disagreed on which results were top-K, but the disagreement was driven by HDC's near-random behavior on non-identical pairs, not by complementary signal.
- **Math friend dual review** identified all four issues independently. The XOR-bind and content_role critiques were converged on by two separate reviewers as the most load-bearing.

The substrate value-prop (graded structural + lexical similarity available to agents at substrate level, complementary to vec) was structurally not deliverable on the v0.4.x identity.

## Decision

A four-piece rewrite of the HDC encoder's identity, shipped together in v0.5.0:

### 1. Bundle composition replaces XOR-bind for child composition

Children are now combined into parent hypervectors via **majority-vote bundling with random-bit tiebreak**, not XOR. Bundle composition is similarity-DAMPENING: similar children produce similar bundles; dissimilar children produce a bundle that pulls toward majority. The dampening is graded with the number and similarity of inputs.

For tree composition this is the correct shape: a parent node whose children share kinds and structure should produce a hypervector that is graded-similar to other parents with similar children. XOR-bind couldn't do this; bundle composition does it by construction.

Position is preserved by permute (rotation) applied per child position, the same as before. The combination is:

```
parent_hv = majority_bundle_with_tiebreak([
    permute(child_i, position_i) for each child
])
```

### 2. Drop `content_role`

With bundle composition carrying the structural information graded-ly, the role-based unbind operations are no longer load-bearing. The `unbind_child_at_position` and `explain_cluster_centroid` functions are retired along with the `content_role` hypervector itself.

Six obsolete tests in `query.rs` are removed (they tested the unbind algebra which no longer exists).

### 3. fp-quantize: drop `sorted_child_kinds` from `canonical_signature_bytes`

The canonical-shape hash that groups equivalent AST patterns no longer carries `sorted_child_kinds`. The kinds are still encoded in the bundle composition's behavior; including them in the hash was structurally redundant and brittle to small shape variations.

`ModuleCodebook` keeps `child_kinds` inline (different concern — modules don't recurse and need shallow discrimination at the codebook level).

### 4. Seeded leaves: char-trigram bundle of leaf token content

Leaves with content (identifiers, literals) now compose their hypervector from two sources: the deterministic kind+identity vector AND a char-trigram bundle of the actual token text:

```
leaf_hv = bundle([
    base_vector(kind, identity),
    char_trigram_bundle(token_text)   // Kanerva 2009 text encoding
])
```

Empty content falls back to `base_vector` only. The change carries through:

- `EncoderNode::leaf_with_content(kind, bytes)` — new constructor
- `leaf_content: Option<Vec<u8>>` field on `EncoderNode`
- `leaf_content_hv()` — char-trigram bundle helper
- `content_hash` extended with length-prefixed leaf content (deterministic across runs)
- `tree_to_encoder_node` takes `Option<&[u8]>` source so the ingestion pass can populate leaves with their UTF-8 text

This gives HDC its first explicit lexical channel. Identifiers that share substrings (parse_json / parse_payload) now grade as lexically similar at the substrate level.

## Results (Phase-1 + Phase-0B-real validation)

### Phase 1 — graded similarity replaces equality oracle

Distances are now structurally graded. Within-cluster pairs (functions in the same trait-impl family or with shared structural skeleton) cluster at distance ~600-900; cross-cluster pairs at ~1100-1400; random pairs at D/2. The substrate is no longer a binary oracle; it is a continuous similarity surface.

### Phase 0B-real — HDC is complementary signal under weighted fusion

Against function-name-prefix ground truth on a ~400-function LLO daemon corpus (36 groups, K=10):

| Architecture | Recall@10 | Δ vs vec-alone |
|---|---:|---:|
| vec-alone | 0.518 | (baseline) |
| HDC-alone | 0.375 | -27.6% |
| RRF(HDC, vec) | 0.409 | -21.0% |
| HDC→vec (filter N=50 + rerank) | 0.354 | -31.6% |
| vec→HDC (filter N=50 + rerank) | 0.363 | -29.9% |
| **score-fusion α=0.20** | **0.556** | **+7.3%** |
| **kernel-RBF α=0.40, σ=D/4** | **0.557** | **+7.7%** |
| prototype-bundle classify (B) | 0.444 | -14.3% |

The "vec dominates" verdict that naive fusion (RRF, filter-rerank) produced was an artifact of the fusion shape, not the substrate. Under weighted score-fusion, HDC's contribution at α≈0.2-0.4 lifts recall above vec-alone by 7-8%. The complementary-modality claim of the substrate holds.

### Prototype-classify dies even with ground truth

The HDC-native bundle-as-prototype pattern (Kanerva-style classify-then-refine) was tested with leave-one-out prototypes derived from the known ground-truth groups. Pick accuracy 44.4% (16/36); recall after classify+rerank 0.444 < vec-alone. HDC's pure-structural axis does not generalize at function granularity, even when given the correct group structure. This validates the architectural prediction: betting retrieval on one dimension of a two-dimensional relevance target structurally cannot win.

### Test discipline

The rewrite shipped with five phases of empirical gates, all asserting on the v0.5.0 substrate (not back-compatible with v0.4.x):

- Phase 0 — synthetic throughput baseline
- Phase 0B — Jaccard agreement HDC vs vec
- Phase 0C — MI vs null (4.35σ above null = real complementary signal on noise vec correctly suppresses)
- Phase 1 — function-granularity characterization (graded similarity gate)
- Phase 0B-real — function-name ground truth recall@K with score-fusion / kernel-RBF / prototype ablations

The Phase 1 thresholds were calibrated to the v0.5.0 distance distribution (within-cluster < cross-cluster, not absolute distance gates). Several v0.4.x tests' absolute-distance assertions were rewritten as relative-cluster-property gates.

## Consequences

### What this buys

- **Graded similarity at the substrate level.** Agents can rank, threshold, and reason about structural+lexical similarity continuously. The binary-equality-oracle limitation is gone.
- **Complementary-modality value-prop validated.** Score-fusion clears vec-alone by +7.7% — the substrate's complementary claim holds empirically.
- **Explicit lexical channel.** Identifier and literal content is now first-class in the HDC, not an emergent secondary effect.
- **Cleaner identity model.** No `content_role`, no unbind algebra; one structural axis (bundle+permute) and one lexical channel (seeded leaves).

### What this costs

- **content_hash changed.** Existing v0.4.x hypervectors are not comparable with v0.5.0 hypervectors. Any cached HDC index must be regenerated. This is a Σ substrate identity break; the rootHash for HDC-derived records changes.
- **α is a hyperparameter.** Fusion-sweep results depend on α; deploying any specific α requires a held-out calibration corpus. v0.5.0 ships the sweep characterization, not a fixed-α production rule.
- **Function granularity is the floor.** Prototype-classify doesn't generalize at function granularity. Higher granularity (module, file) may work for HDC-native classification; not tested in v0.5.0.
- **Tests required relative-gate rewrites.** Several v0.4.x absolute-distance assertions had to be re-expressed against the v0.5.0 distance distribution. Future substrate-identity changes will require the same discipline.

## Rejected alternatives

### Keep XOR-bind, fix lexical channel only

Rejected. XOR's similarity-perfect-transmitting property is structurally wrong for tree composition. No amount of lexical-channel work fixes the binary-oracle problem at the structural axis.

### Keep content_role + add bundle composition

Rejected. Once positions are carried by permute and structure by bundle, `content_role` is redundant — and the unbind operations that motivated it no longer have a coherent algebra under bundle composition.

### Keep `sorted_child_kinds` in fp-quantize

Rejected. The bundle composition already encodes child-kind information into the parent hypervector's structure. Carrying it ALSO in the canonical-signature hash makes the canonical-group identity brittle to immaterial AST variation (whitespace nodes, statement reordering) without adding discriminating information.

### Content-free leaves with vec providing lexical signal

Rejected. HDC-alone had no lexical signal under this option, which means fusion cannot recover lexical-similar pairs HDC didn't see. Score-fusion's +7.7% lift depends on HDC carrying its own (partial) lexical fingerprint; without it, fusion degenerates to vec-with-noise.

### Whole-tree XOR-bind with positional roles (the proposal math friend rejected mid-review)

Rejected. Math friend's first encoder rebind proposal silently dropped child positional ordering — XOR is commutative, so content_role-only bind would lose order. The current design (rotation × bundle) preserves order explicitly via permute.

## Open questions

- **Hyperparameter α.** Phase 0B-real produces the sweep curve but not a deployable α. A held-out calibration corpus is the next step for production fusion. Tracked as a future bead; not blocking the substrate ship.
- **Ground-truth coverage.** Function-name prefix grouping favors vec's lexical axis. HDC's complementary contribution against structural ground truth (call-graph component, shared-SQL-table, AST-shape-clustered with human verification) is a likely-larger lift but untested. Specifically: the +7.7% under name-prefix ground truth is a lower bound; the upper bound is unknown.
- **Higher-granularity HDC classify.** Function-granularity prototype-classify failed even with ground-truth groups. Module-granularity or file-granularity classify may work — the substrate-identity rewrite was tested at function granularity only.
- **Cross-language generalization.** v0.5.0 validation was on the LLO daemon corpus (~400 Rust functions, 36 groups, single domain). Generalization to other languages, larger corpora, and multi-domain projects is unproven.
- **HDC at sub-function level.** Statement-level or expression-level HDC retrieval was probed briefly in Phase 1C as a workaround for function-granularity limitations. A focused study at sub-function granularity is a real follow-up — the substrate's value-prop at finer resolutions is structurally different and may be where HDC's complementary contribution is larger.

## Acceptance criteria (met)

- All four pieces shipped in v0.5.0: bundle composition, drop `content_role`, fp-quantize over `canonical_signature_bytes` (sans `sorted_child_kinds`), seeded leaves.
- Phase 0/0B/0C/1/0B-real test suite green, with Phase 0B-real asserting `best_fusion_sweep > vec-alone + 0.02` as a substrate value-prop gate.
- v0.5.0 binary + library published; mache + downstream consumers regenerate HDC indices on upgrade.
- CHANGELOG.md `[0.5.0]` section captures the rewrite + Phase 0B-real numbers.

This ADR records the architectural decisions; per-phase test discipline lives in the Phase 0/0B/0C/1/0B-real test file headers, which document the gate each phase enforces.
