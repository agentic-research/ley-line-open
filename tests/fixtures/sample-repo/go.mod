// go.mod present so go tooling treats the fixture as a coherent
// package. LLO's tree-sitter parser doesn't actually require it
// (parses files individually), but having the module declaration
// makes gopls + IDE integration cleaner and lets `go vet ./...`
// validate the fixture is well-formed.
module github.com/agentic-research/ley-line-open/tests/fixtures/sample-repo

go 1.21
