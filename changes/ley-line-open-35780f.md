Section: Changed

- **Mutation testing runs one unmutated baseline per scope, not one per
  shard** (bead `ley-line-open-35780f`, GitHub #607). `mutants.yml` gains a
  baseline job per scope: it builds and tests the unmutated tree once
  (`MUTANTS_BASELINE=record` in `tools/mutants_diff.sh`) and the shards run
  `--baseline=skip` with an explicit `--timeout`, cargo-mutants' own rule
  applied to the recorded test time. A failed or missing baseline fails every
  shard of its scope with exit 4 and runs no mutant. The scope's cache has one
  key, saved by the baseline job alone and restored by the shards, in place of
  one cache per shard. A local `task rs:mutants:pr` is unchanged: each slice
  there already ran its baseline once.
