Section: Changed

- **`SheafCache` is documented and tested as advisory: a region it keeps is
  a candidate for reuse, not a valid value** (bead `ley-line-open-f04610`).
  On the 60-commit `task cache:ablation` replay (10,751 lookups, 617 changed
  values) the heuristic mode kept 154 stale values at a 67.9% hit rate, the
  δ⁰ mode kept 201 at 79.3%, and `leyline-cache`'s `RestrictionKey` kept 0
  at 94.0%. Every stale value belongs to a function whose bytes did not
  change and whose value moved through a fact no region owns: a def of a
  called token elsewhere in the corpus, or an import edit. No shipped
  consumer serves a value on the cache's say-so: the daemon's cache holds
  `()`, `sheaf_invalidate` returns regions to recompute, and mache only
  widens its own evictions with the list. The `sheaf_restriction_replay`
  bench's δ⁰ arm now builds stalks with an interface digest in the
  agreement coordinates and a body digest in the projected-away ones, the
  layout mache pushes; its first version derived every coordinate from a
  byte hash and could not separate the two modes. A regression test shrunk
  from the replay pins one stale keep and the restriction key's recompute of
  it, and asserts the advisory statement in the `cache` module doc and the
  sheaf README.
