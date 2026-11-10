Section: Added

- **execution/v1 errors carry a structured `executionError` on the daemon
  and MCP wire** (bead `ley-line-open-e347ae`). A failed `llo_execution_*`
  op answered `{"ok": false, "error": "WorkspaceUnserved: …"}`, so a caller
  could tell the error code only by parsing the text, and `retryable` was
  dropped. The error envelope now also carries `executionError: {"code":
  "workspaceUnserved", "retryable": false, "message": "…"}`: execution/v1's
  `ExecutionError` without the optional `detailRef`, the code spelled as
  execution.capnp's `ErrorCode` (camelCase, as in execution-tools.json), on
  the control socket and in MCP's tool result alike (`daemon.capnp`
  `ErrorResponse.executionError @2`, `ExecutionError`, `ExecutionErrorCode`).
  Every failed execution op carries it: the runtime's own errors, a request
  the op cannot decode (`invalidSpec`), and a daemon with no execution/v1
  service (`unsupportedBackend`, whose text now reads `UnsupportedBackend:
  execution/v1 service is not configured`). Go: regenerated bindings and
  `wire.ExecutionErrorResponse`, checked by the daemon protocol fixture
  gate. Additive: the `error` text otherwise keeps its wording, including
  the `<Code>: ` prefix, and other errors carry no `executionError`.
