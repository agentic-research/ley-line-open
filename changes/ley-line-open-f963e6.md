Section: Fixed

- **A libkrun worker and its VM no longer outlive the process supervising them** (bead `ley-line-open-f963e6`).
  `KrunWorkerBackend` hands each worker the read end of a lifeline pipe
  (`--lifeline-fd`) and keeps its only write end until the worker is reaped.
  When the embedding process ends for any reason, `SIGKILL` included, the
  worker reads end-of-file and exits at once, taking its VM with it; this works
  on macOS, which has no `PR_SET_PDEATHSIG`. The worker requires the option and
  refuses, by name, a missing, malformed or closed lifeline descriptor. A run
  can now be cancelled between spawn and readiness: `cancel` stops the worker,
  returns once it is reaped and its ephemeral run root removed, and answers
  `true`; the racing `start` ends `cancelled`. `Ok(false)` now means no such
  run and no worker. The lifeline is one module (`leyline_runtime::lifeline`),
  shared with the connection broker. Anything that spawns `leyline-krun-worker`
  itself must now pass `--lifeline-fd`
  (`worker::inherit_only_with_lifeline` does so).
