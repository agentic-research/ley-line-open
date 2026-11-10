Section: Fixed

- **A microVM run's output and its worker's failure reach the embedder, bounded** (bead `ley-line-open-a26ad0`). `KrunWorkerBackend` used to send the guest's standard output to `/dev/null` and discard the worker's standard error after readiness. A failed guest then left only "libkrun worker exited with exit status: 1", and a serving guest left nothing.
  - **Bounded tails.** The backend now keeps the last 16 KiB of the guest's standard output and the last 8 KiB of the post-readiness standard error, which interleaves the guest's standard error with the worker's diagnostics. Each tail counts the bytes it dropped.
  - **On failure.** `BackendRunStatus::Failed` appends both tails to its detail, plus any reported `failed` event as unverified text. The guest shares that stream and could forge such an event, so the code and `retryable` stay the supervisor's.
  - **While a run is active.** The new `Backend::output(run_id)` (default `None`) and `KrunWorkerBackend::wait_for_output` return the tails as a new `RunOutput`.
  - **Deny by default.** The bytes reach only the in-process embedder. The service's event log commits to a failure's detail by digest alone, and nothing is written to disk, logged, or sent over the wire.
  - **Untrusted bytes.** Invalid UTF-8 and control characters other than newline and tab become U+FFFD.
