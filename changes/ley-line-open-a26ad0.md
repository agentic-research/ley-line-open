Section: Fixed

- **A microVM run's output and its worker's failure reach the embedder, bounded and typed** (bead `ley-line-open-a26ad0`). `KrunWorkerBackend` used to send the guest's standard output to `/dev/null` and discard the worker's standard error after readiness. A failed guest then left only "libkrun worker exited with exit status: 1", and a serving guest left nothing.
  - **Bounded tails.** The backend now keeps the last 16 KiB of the guest's standard output and the last 8 KiB of the post-readiness standard error, which interleaves the guest's standard error with the worker's diagnostics. Each tail counts the bytes it dropped.
  - **The new `GuestOutput` type.** It is the one shape for guest output: `stdout_tail`, `stdout_dropped_bytes`, `stderr_tail`, `stderr_dropped_bytes`, `untrusted: true` and `reported_failure`, serialized camelCase.
  - **Finished and failed runs.** `KrunWorkerBackend::wait_for_outcome(run_id)` and the new `Backend::poll_outcome(run_id)` return a `RunOutcome { status, output: Option<GuestOutput> }`. The `poll_outcome` default is `poll` with no output.
  - **On failure.** `BackendRunStatus::Failed` also appends both tails to its detail. A reported `failed` event is included only as unverified text, because the guest shares that stream and could forge one. The code and `retryable` stay the supervisor's.
  - **While a run is active.** `Backend::output(run_id)` (default `None`) and `KrunWorkerBackend::wait_for_output` return the tails.
  - **Deny by default.** The bytes reach only the in-process embedder. The service's event log commits to a failure's detail by digest alone, and nothing is written to disk, logged, or sent over the wire. The daemon-socket exposure is `ley-line-open-eea8e1`.
  - **Untrusted bytes.** Invalid UTF-8 and control characters other than newline and tab become U+FFFD.
