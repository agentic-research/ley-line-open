Section: Breaking

- **The execution service opens a run's workspace shares and hands them to the backend** (bead `ley-line-open-1022a8`). The libkrun backend used to open the share itself, and the service then compared the shares the backend reported with the grant, after the worker had started.
  - **`Backend::start(request, workspaces: ServedWorkspaces)`.** The service opens each granted workspace before it calls the backend. Only the service can construct a non-empty `ServedWorkspaces`, and only for a run started through `start_authorized`. `ServedWorkspaces::none()` is public: it is what a direct `Backend::start` embedder passes.
  - **`BackendRun.attached` replaces `BackendRun.channels`.** A backend names a share for the receipt only by taking one (`ServedWorkspace::attach` returns the `AttachedWorkspace` proof), so it cannot claim a share the grant never named.
  - **`ExecutionService::with_workspace_shares` replaces `KrunWorkerBackend::with_workspace_shares`.**
  - **Refused earlier.** A service with no share provider, or a store that does not hold the granted graph root, refuses the start before the backend is called. A request that names workspaces without a verified grant (`ExecutionService::start`, or `Backend::start` with no shares) is refused `workspaceUnserved`.
  - **One assertion remains.** A backend that answers a start having dropped a share it was handed is cancelled and refused, as before. No first-party tier can reach it.
