# sample-repo

Tiny fixture repo for `task cluster:smoke` — exercises the full LLO + mache integration loop end-to-end: tree-sitter parses these Go files into LLO's `nodes` / `_ast` / `node_refs` / `node_defs` tables, then mache queries them via the daemon's UDS protocol.

Contents:

| File | Purpose |
|---|---|
| `main.go` | Entry point, calls `Greet()` from `greet.go`. Validates cross-file `node_refs` population. |
| `greet.go` | Defines `Greet(name string) string`. Validates `node_defs` and `read_content`. |
| `util.go` | `formatGreeting(template, name string) string` helper used by `Greet`. Validates `find_callers` and `find_callees`. |

Deliberately:
- All three files are pure Go (most-tested tree-sitter grammar in the workspace).
- No external imports (parse-stable across go versions).
- Multi-file with cross-file references (exercises `node_refs`, `find_callers`, `find_callees`).
- Tiny (sub-50-line total), so the parse is fast even on cold cache.

If you want to test a different language, add files here; the LLO parser auto-detects extensions.
