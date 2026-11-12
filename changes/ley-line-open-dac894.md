Section: Fixed

- **The execution daemon removes the run directories a crashed daemon left, and refuses a run when the run root is nearly full** (bead `ley-line-open-dac894`).
  - **Directory names.** Each run's directory is now named for the daemon that made it: `leyline-run-<pid>-…` or `leyline-native-run-<pid>-…`.
  - **Sweep at start.** `execution-daemon` removes, at start, each such directory whose daemon is gone, restoring owner permissions first. It names each one. It leaves a live daemon's directories and any other name alone, including earlier releases' pid-less names. `backends::run_root::sweep_stale_runs` does this.
  - **Free-space floor.** A run whose run root has less than `RUN_ROOT_FREE_FLOOR_BYTES` (1 GiB) free is refused as retryable `resourceExhausted`, before anything is created or spawned. `with_run_root_free_floor` on either backend changes the floor. On a CAS volume, runs and the CAS share one size cap.
  - **Not yet bounded.** What a running guest writes is still unbounded; that is `ley-line-open-b1418f`.
  - **Refused run roots.** A run root refused as case-insensitive is no longer left behind. The refusal now reads "check the run root", not "provision the CAS root".
  - **Hard links into the CAS.** The native confinement suite now pins that a guest's hard link to a CAS file on the run root's own filesystem is refused by the sandbox (`ley-line-open-dab663`).
