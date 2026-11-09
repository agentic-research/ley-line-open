Section: Security

- **A run never starts without a workspace its grant names, and the refusal
  has its own execution/v1 code, `workspaceUnserved`** (bead
  `ley-line-open-204a94`). `ErrorCode` gains `workspaceUnserved` at ordinal 12
  (append-only). The native tier, the microVM tier without a source tree, and
  the microVM tier given more than one workspace now refuse with it before
  spawning anything; these were `unsupportedBackend`, the same code as an
  egress grant or an unenforceable ceiling, so a caller could not tell a run
  another tier would serve from one no tier would. `ExecutionService` also
  checks each backend's answer: a start whose reported workspace shares are
  not exactly the granted workspaces (one missing, or one the grant never
  named) is cancelled and refused with `workspaceUnserved`, and the run is
  never recorded. A `Backend` implementation outside this repository that
  accepts workspace grants must report a `WorkerChannel::WorkspaceShare` for
  each, or its runs are refused. A client matching `unsupportedBackend` for
  these refusals must match `workspaceUnserved` instead.
