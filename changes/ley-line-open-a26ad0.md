Section: Fixed

- **A microVM run's output and its worker's failure reach the embedder, bounded** (bead `ley-line-open-a26ad0`). `KrunWorkerBackend` used to send the guest console to `/dev/null` and discard the worker's standard error after readiness. A failed guest then left only "libkrun worker exited with exit status: 1", and a serving guest left nothing.
  - **Bounded tails.** The backend now keeps the console's last 16 KiB and the worker's post-readiness standard error's last 8 KiB, each with a count of the bytes it dropped.
  - **On failure.** `BackendRunStatus::Failed` carries the worker's terminal `failed` event, meaning its code, `retryable` and detail, followed by both tails.
  - **While a run is active.** The new `Backend::output(run_id)` (default `None`) and `KrunWorkerBackend::wait_for_output` return them. The new `RunOutput` type holds them.
  - **Deny by default.** The bytes go only to the in-process embedder. The service's event log commits to a failure's detail by digest alone, and nothing is written to disk, logged, or sent over the wire.
  - **Untrusted bytes.** Guest bytes are sanitized: invalid UTF-8 and control characters other than newline and tab become U+FFFD.
