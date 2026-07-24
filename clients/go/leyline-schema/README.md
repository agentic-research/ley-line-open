# leyline-schema (Go)

Go bindings for ley-line-open's public Cap'n Proto schemas. This module
ships the generated `*.capnp.go` files so downstream Go consumers (mache
first, others later) can `import` the bindings instead of forking the
`.capnp` files.

Module path:

```
github.com/agentic-research/ley-line-open/clients/go/leyline-schema
```

## Sub-packages

One per schema. The schema files themselves live in the Rust workspace
under `rs/ll-core/` — that's the single source of truth, regenerated for
both runtimes from the same files.

| Package | Schema source | Notes |
|---------|---------------|-------|
| `common` | `rs/ll-core/schema-capnp/schemas/common.capnp` | `Position`, `Range`, `Hash`, `NodeRef` — shared primitives. |
| `ast` | `rs/ll-core/schema-capnp/schemas/ast.capnp` | `AstNode` projection. Imports `common`. |
| `binding` | `rs/ll-core/schema-capnp/schemas/binding.capnp` | `BindingRecord` (LSP refs). Imports `common`. |
| `head` | `rs/ll-core/schema-capnp/schemas/head.capnp` | `Head` — Σ root pointer for file-backed dbs. Imports `common`. |
| `source` | `rs/ll-core/schema-capnp/schemas/source.capnp` | `SourceFile` projection. Imports `common`. |
| `daemon` | `rs/ll-core/public-schema/capnp/daemon.capnp` | UDS control-socket wire types. |

Import a sub-package directly:

```go
import (
    capnp "capnproto.org/go/capnp/v3"

    "github.com/agentic-research/ley-line-open/clients/go/leyline-schema/binding"
    "github.com/agentic-research/ley-line-open/clients/go/leyline-schema/common"
)

func decodeRef(buf []byte) (binding.BindingRecord, error) {
    msg, err := capnp.Unmarshal(buf)
    if err != nil {
        return binding.BindingRecord{}, err
    }
    return binding.ReadRootBindingRecord(msg)
}
```

## Schema-evolution contract

Every change to a `.capnp` file is governed by ADR-0014 — see
[`docs/adr/0014-capnp-as-protocol.md`](../../../docs/adr/0014-capnp-as-protocol.md).
Highlights:

- Ordinals are stable. Append-only at the next ordinal; never rename or
  remove a field.
- The `capnp` / `capnpc` Rust toolchain pin in
  `rs/ll-core/schema-capnp/Cargo.toml` is exact (`=0.20.0`); the Go
  pin is in this module's `go.mod`. Bumping either requires
  cross-runtime fixture regeneration (see below).
- The `tests/fixtures/binding-record-*.bin` fixtures committed in the
  Rust crate are the cross-runtime byte-equality contract. Both
  `cargo test -p leyline-schema-capnp --test cross_runtime_fixtures`
  and `go test ./binding/...` in this module decode against them; both
  must stay green.

## Daemon protocol gate

The daemon UDS + MCP wire is JSON-encoded but typed against
`daemon.capnp` (`rs/ll-open/cli-lib/src/daemon/wire.rs` is the Rust
serde mirror). Cross-runtime parity for that wire lives in:

- `rs/ll-open/cli-lib/tests/fixtures/daemon-protocol.json` — single
  fixture file pinning each op's request shape, response shape, and
  required-key set.
- `daemon/daemon_protocol_test.go` (this module) — for every entry,
  decodes the fixture response into the matching typed Go struct via
  `json.Decoder` with `DisallowUnknownFields()` plus an explicit EOF
  check, so the test fails on `UnmarshalTypeError`, on unknown fields,
  *and* on trailing content. Runs in CI on every push to LLO.
- `rs/ll-open/cli-lib/tests/integration.rs::daemon_protocol_gate_handlers_emit_required_keys`
  — for every entry, calls the matching Rust handler and asserts the
  output contains every `response_required_keys` entry.

Adding a new op: extend the fixture, add the typed Rust response in
`wire.rs`, run `regen.sh`, add the typed Go mirror struct +
`decoderFor` case in `daemon_protocol_test.go`. wire.rs is hand-written
(not codegen'd from `daemon.capnp`) so schema↔wire parity is enforced
by these tests rather than the Rust compiler — the typed `BaseRequest`
enum + handler signatures catch the common drift class at compile
time, the fixture gates catch the rest at test/CI time.

## Regenerating

Whenever a schema changes:

```sh
clients/go/leyline-schema/regen.sh
```

That script re-runs `capnp compile -ogo` for all six schemas and runs
`go build ./...` to catch cross-package import drift.

CI (`.github/workflows/leyline-schema-go.yml`) gates this:

1. Reruns `regen.sh`.
2. `git diff --exit-code clients/go/leyline-schema/` — fails if the
   committed Go files don't match what regen would produce. So
   *generated files are tracked*; do not gitignore them.
3. `go test ./...`.

## Toolchain

Required for regen:

- `go` ≥ 1.21
- `capnp` ≥ 1.3.0 (`brew install capnp` on macOS, `apt-get install
  capnproto` on Debian/Ubuntu)
- `capnpc-go` from `capnproto.org/go/capnp/v3@v3.1.0-alpha.2`:

  ```sh
  go install capnproto.org/go/capnp/v3/capnpc-go@v3.1.0-alpha.2
  ```

  `capnp compile -ogo` shells out to `capnpc-go`; make sure your
  `$(go env GOPATH)/bin` is on `$PATH`.

End consumers don't need any of this — `go get` the module and import.

## Why a separate module

Multi-module monorepo pattern (kubernetes/api, stripe-go). One
versionable contract, tagged when that public contract changes
(`clients/go/leyline-schema/vX.Y.Z`), with no content-identical tag for
binary-only or private-storage releases and no `replace` directives required
for downstream consumers. The Rust
workspace has no Go dependency; the Go module has no Rust dependency
beyond reading the canonical `.capnp` files at regen time.

## License

**Apache-2.0** — see [LICENSE](LICENSE) in this directory.

Deliberately different from the repository root, which is AGPL-3.0-or-later.
This module is the cross-runtime *contract*, not the engine: it is generated
Cap'n Proto bindings with no dependency on any AGPL crate in the workspace, so
importing it carries no copyleft obligation. Linking ley-line-open's
implementation crates does.
