# sample-repo

Tiny Go fixture used as parse-input for LLO integration tests and (eventually) cross-repo smoke tests in mache. Three files with cross-file references — exercises tree-sitter parse + `node_refs` / `find_callers` / `find_callees` / `read_content`.

| File | Role |
|---|---|
| `main.go` | Entry point; calls `Greet()` from `greet.go`. Cross-file caller side. |
| `greet.go` | Defines `Greet(name string) string`; calls `formatGreeting` from `util.go`. Both caller and callee. |
| `util.go` | `formatGreeting(template, name string) string` — leaf of the call graph. Callee target. |

Deliberately:
- Pure Go (most-stable tree-sitter grammar in the workspace).
- No external imports beyond `fmt` (parse-stable across Go versions).
- Multi-file with cross-file references (so `node_refs` / `find_callers` / `find_callees` have something to find).
- Sub-50-line total — cold-parse is fast on every run.

The intended cross-repo consumer is mache's `task cluster:smoke` target (see bead `mache-cafce9`), which copies this directory into a running LLO daemon container, sends a `reparse` op, then exercises mache's MCP surface against the parsed nodes.

Adding another language? Drop files here; LLO's parser auto-detects extensions.
