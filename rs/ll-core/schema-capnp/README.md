# leyline-schema-capnp

Cap'n Proto schemas for the Σ event log. T8 (decade
`ley-line-open-9d30ac`, thread `T8/capnp-as-protocol`).

## What this crate is

The **typed cross-process / cross-runtime contract** for Σ events.
SQLite tables (`_ast`, `_lsp_refs`, etc.) are local projections of
these messages — not the contract.

This crate holds the schemas; producers (LLO) emit messages; consumers
(mache, control-room, future workerd cache) read them. Every runtime
generates its own bindings from the same `.capnp` files.

## Schemas

| File | Content | Status |
|------|---------|--------|
| `schemas/common.capnp` | `Position`, `Range`, `Hash` (BLAKE3-32), `NodeRef` | T8.1 ✅ |
| `schemas/binding.capnp` | `BindingRecord` — LSP refs with both `constructNodeId` and `refSiteNodeId` | T8.2 ✅ |
| `schemas/ast.capnp` | `AstNode` — tree-sitter projection | T8.3 (open) |
| `schemas/source.capnp` | `SourceFile` — canonicalized path, content hash, mtime | T8.3 (open) |

Schema-evolution rules: append fields at next `@N` ordinal with default;
never rename, never repurpose, never re-use ordinals; remove only by
leaving a hole. ADR-0014 (T8.6) will formalize.

## Event-log file conventions

Producer emits records as **plain back-to-back capnp segment messages**
written via `capnp::serialize::write_message` (Rust) /
`capnp.NewEncoder` (Go) / equivalents.

| Producer entry point | Output path |
|---|---|
| `leyline lsp <input.db> -o <output.db>` | `<output>.bindings.capnp` |
| daemon `enrich` pass on file-backed db | `<live.db>.bindings.capnp` (skip on `:memory:`) |

Readers iterate via `read_message` until EOF. Each message's root is
the schema's top-level struct (`BindingRecord` for `*.bindings.capnp`).

## Consumer pattern (Rust)

```rust
use leyline_schema_capnp::binding_capnp::binding_record;
use capnp::serialize;

let mut bytes: &[u8] = &std::fs::read(path)?;
while !bytes.is_empty() {
    let msg = serialize::read_message(&mut bytes, Default::default())?;
    let rec: binding_record::Reader = msg.get_root()?;
    // rec.get_target_node_id(), rec.get_construct_node_id(), ...
}
```

## Consumer pattern (Go, for mache)

```go
import (
    capnp "capnproto.org/go/capnp/v3"
    binding "your/generated/binding"
)

f, _ := os.Open(path)
defer f.Close()
dec := capnp.NewDecoder(f)
for {
    msg, err := dec.Decode()
    if err == io.EOF { break }
    rec, _ := binding.ReadRootBindingRecord(msg)
    // rec.TargetNodeId(), rec.ConstructNodeId(), ...
}
```

## Generating bindings outside Rust

The `.capnp` files in `schemas/` are the canonical artifacts. Other
runtimes generate from them:

- **Go** (mache): `capnp compile -ogo --src-prefix=schemas schemas/*.capnp`
- **TypeScript** (cloister/workerd): via `capnpc-ts` or `@capnp-ts/cli`
- **Swift** (control-room): via `capnpc-swift`

Vendor or submodule the schemas/ dir; do not duplicate the `.capnp`
files in consumer repos.

## Build prereq

`capnp` binary on PATH:
- macOS: `brew install capnp`
- Debian/Ubuntu: `apt-get install capnproto`

Verified against Cap'n Proto 1.3.0+.
