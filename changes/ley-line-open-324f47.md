Section: Fixed

- **Two cost tests assert deterministic counts, not wall time** (beads
  `ley-line-open-324f47`, `ley-line-open-40300f`). `leyline-cache`'s
  `restriction_is_cheaper_than_review` compared millisecond timings at its
  largest corpus, and `leyline-sheaf`'s `real_repo_sheaf_bench` compared
  µs of simulated parsing; both failed under `task ci` load while their row
  and eviction counts held. The restriction test now asserts that the
  restriction touches the same substrate rows at every corpus size while the
  review's rows grow (a restriction made O(n) fails it), and the sheaf bench
  asserts the δ⁰ invalidation set is a strict subset of the heuristic's and
  reparses strictly fewer source bytes.
