Section: Fixed

- **A native worker and its workload no longer outlive the process supervising them, and a starting native run can be cancelled** (bead `ley-line-open-62e3b1`).
  The native tier now keeps the promises the microVM tier makes since
  `ley-line-open-f963e6`. The worker's standard input is its lifeline
  (`--lifeline-fd 0`): `NativeWorkerBackend` keeps its write end until the
  worker is reaped, and when the embedding process ends, `SIGKILL` included,
  the worker kills its workload and process group and exits. Spawning still
  takes the `posix_spawn` path. `cancel` reaches a run between spawn and
  readiness: it returns once the worker is reaped and its run root removed,
  answers `true`, and the start ends `cancelled`; `Ok(false)` means no such run
  and no worker. `leyline-native-worker` requires `--lifeline-fd` and refuses,
  by name, a missing one or a descriptor that is not a readable pipe or socket.
  The workload's standard input is `/dev/null` when the worker's is the
  lifeline.
