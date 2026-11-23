Section: Changed

- **Mutation testing runs one unmutated baseline per scope, not one per
  shard** (bead `ley-line-open-35780f`, GitHub #607). `mutants.yml` gains a
  baseline job per scope (`MUTANTS_BASELINE=record` in
  `tools/mutants_diff.sh`); the shards run `--baseline=skip` with an explicit
  `--timeout`, 1.5 times the value cargo-mutants would set, because the
  baseline ran on another runner. A failed or missing baseline fails every
  shard of its scope with exit 4 and runs no mutant. The generic slice's
  multi-package remainder keeps its own baseline in each shard. The scope's
  cache has one key, saved by the baseline job alone, in place of one per
  shard. The gain is modest: on a comparable runner one 4-shard run went from
  3861 s to 3807 s of runner time. What is dependable is that 3 of 4 baseline
  test runs are gone, shards no longer build cargo-mutants from source, and
  there is one cache per scope. The repeated baseline build moved to each
  shard's first mutants; carrying a built target to the shards was tried and
  does not work. Wall clock grows, because every shard waits for the baseline
  job (172 to 353 s measured). A local `task rs:mutants:pr` is unchanged.
