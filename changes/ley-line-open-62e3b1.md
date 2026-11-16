Section: Breaking

- **`leyline-native-worker` now requires `--lifeline-fd`; the worker binary and `leyline-runtime` must be upgraded together** (bead `ley-line-open-62e3b1`).
  A native worker and its workload no longer outlive the process supervising
  them, as the microVM tier's since `ley-line-open-f963e6`. The worker's
  standard input is its lifeline (`--lifeline-fd 0`): `NativeWorkerBackend`
  keeps its write end until the worker is reaped, and when the embedding
  process ends, `SIGKILL` included, the worker kills its workload and its
  process group and exits. Spawning still takes the `posix_spawn` path. The
  worker refuses, by name, a missing or malformed lifeline, standard output
  or error as one, and a descriptor that is not a readable pipe or socket.
  The workload's standard input is `/dev/null` when the worker's is the
  lifeline. In the backend API, `NativeWorkerBackend::cancel` now reaches a
  run from before its spawn until readiness: it returns once the worker is
  reaped and its run root removed, answers `true` (or the cleanup's error),
  and the racing `start` ends `cancelled`; `Ok(false)` means no such run and
  no worker. `ExecutionService::cancel` reaches a starting run separately
  (`ley-line-open-5308b7`).
