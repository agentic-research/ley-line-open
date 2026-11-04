# Restriction-Addressed Review Caching over Real Facts

**Status:** measured, verdict GO (for this review family)
**Artifact:** `rs/ll-open/sheaf/tests/restriction_review_real.rs`
**Predecessors:** the toy harness (`rs/ll-open/sheaf/src/restriction_review.rs`,
`docs/superpowers/specs/2026-07-17-restriction-ast-review-toy-design.md`) and
the ADR-0030 rung-2 experiment (`docs/research/sheaf-ablation-study.md`).

## Claim under test

A cached EXPENSIVE review result can be safely reused when its CHEAP
fact-specific restriction hash is unchanged, even when the whole-object
content hash changed.

The toy could not test this: its "restriction" hashed the same observables
its oracle compared, so soundness was true by construction, and its facts
came from a line parser, not the substrate. This experiment re-runs the
question over LLO's real fact columns — `node_refs` / `node_defs` /
`_imports` with `qualifier` and `container_node_id`, emitted by
`leyline_ts::refs::extract_rust` (the same extraction the daemon persists)
— and keeps three artifacts structurally separate:

1. **Restriction** (cheap): a hash over a sound superset of the review's
   *input rows* — F's container identity, the sorted `(token, qualifier)`
   pairs of `node_refs` rows contained in F, the `(alias, path)` import
   rows of F's file that any target token/qualifier names, and the
   `node_defs` rows F's target tokens index to (token-indexed point
   lookup, cross-file). No resolution logic runs.
2. **Review result** (expensive): the resolved call graph of F — for each
   call-target row, an unindexed cross-corpus JOIN over all `node_defs`
   rows plus the import surface, producing resolved edges. This is the
   cached artifact a skip avoids recomputing.
3. **Oracle**: the review result computed independently on before and
   after, compared for equality. Never consults the restriction.

Review family: the call-target review of one function F (`fn score`).

## Fixtures

Two-file corpus (`main.rs` with `score` + `audit`; `math.rs` with the
callee defs plus 200 padding defs). All fixtures are real Rust parsed by
tree-sitter; facts are the live extractor's emission.

| fixture | edit | review changed? |
|---|---|---|
| whitespace | blank lines inside `score` | no |
| **local-rename** | local `adjusted` → `shifted` (never a call target) | no |
| **body-arith** | `value + 1` → `value * 3` (no call touched) | no |
| elsewhere-edit | `audit`'s arithmetic changes; `score` byte-identical | no |
| unrelated-def-add | new def in dep file, token unrelated to F | no |
| callee-swap | `compute_weight(value)` → `compute_penalty(value)` | yes |
| import-path | `use crate::math::compute_weight` → `crate::math_v2::…`, alias unchanged | yes |
| qualifier-swap | `mathq::qhelper(..)` → `mathr::qhelper(..)` | yes |
| corpus-def-rename | callee def renamed in the DEP file; F's file untouched | yes |

## Verdict table (measured, debug and release identical)

```
fixture              review_changed  WholeObject   AstShape   Restriction
whitespace                    false    recompute       SKIP          SKIP
local-rename                  false    recompute       SKIP          SKIP
body-arith                    false    recompute       SKIP          SKIP
elsewhere-edit                false    recompute       SKIP          SKIP
unrelated-def-add             false    recompute  recompute          SKIP
callee-swap                    true    recompute       SKIP     recompute
import-path                    true    recompute       SKIP     recompute
qualifier-swap                 true    recompute       SKIP     recompute
corpus-def-rename              true    recompute       SKIP     recompute

policy          false_skip_rate  true_skip_rate  recompute_saved
WholeObject                0/4             0/5                 0
AstShape                   4/4             4/5                 4
Restriction                0/4             5/5                 5
```

- **Restriction: zero false skips, 5/5 sound skips.** The two
  load-bearing fixtures — local-rename and body-arith, the semantic
  edits the toy was missing — are skipped soundly while whole-object
  CAS recomputes. The projection is real: edits elsewhere in F's file
  and in the dep file also skip.
- **AstShape: 4/4 false skips.** The identifier-blind kind-sequence
  hash false-skips *every* review-changing fixture here, because
  call-target changes are identifier-text changes — precisely the
  rung-2 blind spot (callee swap parses to the same
  `call_expression > identifier` shape). This is the strongest
  reproduction of ADR-0030's finding yet: for THIS review family the
  structural embedding is not merely lossy, it is blind.
- **WholeObject: sound but useless.** Defined over the corpus (the only
  per-object hash that is sound for a cross-item review), it never
  skips. The per-file variant would false-skip `corpus-def-rename` —
  the dep-side def edit that leaves F's file byte-identical — which is
  the concrete way "CAS of the reviewed object" under-covers a
  cross-item review.

## restriction_cost vs review_cost

Rows touched is deterministic; wall time swept over corpus scale
(release build; debug in parentheses at the smallest scale):

```
 def_rows  restr_ops   rev_ops   restr_time     rev_time  op_ratio time_ratio
      205          6       416      386.0ns      597.0ns     69.3x       1.5x
     2005          6      4016      365.0ns        2.2µs    669.3x       6.0x
    10005          6     20016      337.0ns       10.9µs   3336.0x      32.3x
```

The restriction touches 6 rows regardless of corpus size (F's 2 ref
rows + the file's 2 import rows + 2 indexed def rows); the review's
join grows linearly with `node_defs`. In an unoptimized debug build the
smallest scale inverts (6.8µs vs 3.3µs): SHA-256 plus buffer constants
exceed a 416-row in-memory compare. That inversion is part of the
finding, not noise — restriction gating pays off only when the gated
computation costs more than the hash's constant overhead (~1µs here).
Any real review (deeper analysis, an LLM pass, anything that leaves L1)
clears that bar by orders of magnitude; the join measured here is a
deliberate lower bound stand-in.

## Preconditions and caveats (what the experiment surfaced)

1. **Stable container identity is a precondition.** The experiment keys
   containers by name (`fn:score`), not the daemon's positional
   `node_id` paths. Hashing positional ids would invalidate the
   restriction on any line shift above F, degenerating it into
   whole-file sensitivity. The substrate's `(token, qualifier,
   container_node_id)` columns carry everything needed, but a deployed
   restriction cache needs a position-independent container key
   (name-path or content-derived), which `node_id` today is not.
2. **The restriction must span objects.** Clause (c) — the def rows the
   target tokens index to — is what catches `corpus-def-rename`.
   A restriction computed only from F's own file is NOT a sound
   superset for a cross-item review. Restriction-addressing for this
   family is inherently a multi-object projection (sheaf-shaped: F's
   cell plus the def cells its tokens touch), and its cheapness depends
   on the `node_defs` token index existing.
3. **Set semantics.** Fact rows are compared as sets (like rung 2's
   oracle): duplicating an existing call site `compute_weight(x)` at a
   second location inside F changes neither the restriction nor the
   set-valued review result. A review family whose result is
   multiplicity- or position-sensitive needs those dimensions added to
   both the restriction and the result.
4. **Tightness.** For this family the restriction is a near-minimal
   superset — nearly every input row it hashes can individually change
   the review output, so "restriction changed but review unchanged"
   recomputes (the wasteful-but-sound quadrant) are rare by
   construction. Families with looser restrictions will show more
   wasteful recomputes; that costs efficiency, never soundness.

## Go / no-go

**GO** — for the call-target review family over the real fact
substrate, the claim held without engineering:

- restriction false_skip_rate = 0 across every review-changing fixture,
  including the dep-side edit F's file never sees;
- restriction true_skip_rate (5/5) strictly dominates WholeObject
  (0/5), and the wins include the two load-bearing semantic edits
  (local-rename, body-arith), not just whitespace;
- restriction_cost < review_cost by 69×–3336× in rows touched, and in
  wall time everywhere in release (1.5×–32×), with an honest constant-
  overhead crossover documented for sub-µs "reviews";
- the restriction degenerated into neither the whole object (it skips
  five whole-object-changing edits) nor the review result (it is a hash
  of input rows computed by a separate, resolution-free path).

The result is conditional on the preconditions above: stable container
identity, a token-indexed `node_defs`, and per-family restriction
design. AstShape's 4/4 false-skip rate is the sharpest statement yet of
why the approximate-structural shortcut cannot substitute: exact
restriction hashes over substrate facts are both cheaper and sound.
