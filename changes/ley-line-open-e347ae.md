Section: Added

- **execution/v1 errors carry a structured classification on the daemon
  and MCP wire** (bead `ley-line-open-e347ae`). A failed `llo_execution_*`
  op answered `{"ok": false, "error": "WorkspaceUnserved: …"}`, so a caller
  could tell the error code only by parsing the text, and `retryable` was
  dropped. The error envelope now also carries `executionError: {"code":
  "workspaceUnserved", "retryable": false}`, the code spelled as
  execution.capnp's `ErrorCode` (camelCase, as in execution-tools.json), on
  the control socket and in MCP's tool result alike (`daemon.capnp`
  `ErrorResponse.executionError @2`, `ExecutionErrorClass`,
  `ExecutionErrorCode`; Go bindings regenerated). Additive: the `error`
  text is unchanged, including the `<Code>: ` prefix Cloister matches
  today, and other errors carry no `executionError`.
