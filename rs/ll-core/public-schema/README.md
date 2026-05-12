# leyline-public-schema

Cap'n Proto schema for the daemon UDS + MCP wire — the typed contract between the ley-line daemon and consumers (mache, cloister, future TS/Swift clients).

## What's here

- **`capnp/daemon.capnp`** — the schema. Source of truth for every base op's request and response shape. Field-level `$Json.name(...)` annotations map camelCase capnp names to snake_case JSON wire names per the capnp-json codec.
- **`capnp/capnp/compat/json.capnp`** — vendored from [capnp-json](https://crates.io/crates/capnp-json) (which vendors from upstream [capnproto/capnproto](https://github.com/capnproto/capnproto)). Provides the `$Json.name` / `$Json.flatten` / `$Json.discriminator` annotation IDs. **Don't edit** — kept byte-identical to upstream so regen is clean.
- **`build.rs`** — invokes capnpc-rust with `crate_provides("capnp_json", ...)` so generated bindings resolve the json annotations to the capnp-json crate's types. Re-runs on any schema change.

## What this crate is NOT

- **Not the substrate contract.** The Σ Merkle-CAS substrate schemas (AstNode, SourceFile, BindingRecord, Head) live in [`leyline-schema-capnp`](../schema-capnp/). Those are the producer↔consumer event-log contract; this crate is the daemon RPC contract.
- **Not the SQLite contract.** That's [`leyline-schema`](../schema/) (the `nodes` table DDL).

## Evolution rules

- **Append-only-additive.** Add fields at the next ordinal; never rename or remove. Per [ADR-0014 §2](../../../docs/adr/0014-capnp-as-protocol.md).
- **Exact-pinned toolchain.** `capnp = "=0.25.0"`, `capnpc = "=0.25.0"`, `capnp-json = "=0.1.0"`. Version drift could change canonical bytes.
- **Cross-runtime gate.** Schema changes regenerate `clients/go/leyline-schema/daemon/daemon.capnp.go`; CI's regen-diff gate fails if the committed bindings don't match.

## Consumers

- **Rust**: `leyline-cli-lib` (handler builders, capnp-json `to_json` on response)
- **Go**: `clients/go/leyline-schema/daemon/` (mache decodes typed responses)
- **TS** (future): cloister via udsForward (per cloister ADR-0005)

## Active threads

- `ley-line-open-40df83` — dual-codec wire (binary capnp + JSON, magic-byte dispatch). The natural step 4 after b0ea2e adopted capnp-json on the JSON carrier.
- `ley-line-open-b07a79` — `op_query` structural skip: schema declares positional `List(QueryRow)`, handler emits column-keyed maps. Design call pending.
