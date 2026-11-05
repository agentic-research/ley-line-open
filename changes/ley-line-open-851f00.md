Section: Fixed

- **Every op that changes state emits exactly one `daemon.<op>` event, over
  the socket and over MCP, and an op that changes nothing emits none** (bead
  `ley-line-open-851f00`). Only the control socket emitted, and only for a
  hand-kept list of op names, so a `propose`, `reparse` or `enrich` made over
  MCP reached no subscriber; `hdc_calibrate`, a writing `query` and the lazy
  schema installs of `agreement` and the HDC ops wrote without an event; and
  `snapshot` emitted `daemon.snapshot` whether or not anything changed. The
  live db's write boundary (`DaemonContext::with_write`) now compares the
  writer's row-change count and schema version before and after each write,
  and the publish boundary (`with_publish`, used by `snapshot`, `reparse`,
  `enrich` and `load`) compares the published `current_root`;
  `DaemonContext::observe_op`, around the dispatch both transports share,
  emits once per op that crossed a changing boundary. `STATE_CHANGING_OPS`
  is gone. Lazy LSP enrichment, which a `lsp_*` read queues onto the blocking
  pool, stays where it is and runs as its own `enrich` op, so the `_lsp` rows
  it lands emit `daemon.enrich`: moving it off the read path would make the
  first `lsp_*` call on an unenriched file answer nothing instead of
  answering `enriched: true` and filling on the retry. Subscribers see new
  topics (`daemon.query`, `daemon.hdc_calibrate`, `daemon.agreement`, ...)
  with the existing `{"op": <op>}` payload, and no longer see an event for
  a refused proposal or a snapshot of unchanged bytes.
