Section: Breaking

- **`leyline-krun-worker` now requires `--lifeline-fd`; the worker binary and `leyline-runtime` must be upgraded together** (bead `ley-line-open-f963e6`).
  A libkrun worker and its VM no longer outlive the process supervising
  them. `KrunWorkerBackend` hands each worker the read end of a lifeline pipe
  (`--lifeline-fd`) and keeps its only write end until the worker is reaped.
  When the embedding process ends for any reason, `SIGKILL` included, the
  worker reads end-of-file and exits at once, taking its VM with it; this works
  on macOS, which has no `PR_SET_PDEATHSIG`. The worker refuses, by name, a
  missing or malformed lifeline, a standard stream (0-2), the workspace
  share's descriptor, and a descriptor that is closed, not readable, or not a
  pipe or socket. An older worker given `--lifeline-fd` refuses it as an
  unknown option, and this worker refuses an older backend that does not pass
  it. Anything that spawns `leyline-krun-worker` itself must pass the option
  (`worker::inherit_only_with_lifeline` does so). In the backend API,
  `KrunWorkerBackend::cancel` now reaches a run between spawn and readiness:
  it returns once the worker is reaped and its ephemeral run root removed,
  answers `true` (or the cleanup's error), and the racing `start` ends
  `cancelled`; `Ok(false)` means no such run and no worker.
  `ExecutionService::cancel` reaches a starting run separately
  (`ley-line-open-5308b7`). The lifeline is one module
  (`leyline_runtime::lifeline`), shared with the connection broker.
