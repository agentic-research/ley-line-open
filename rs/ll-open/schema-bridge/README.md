# leyline-schema-bridge

Capnp + JSON-extension schemas → zod TS / Go / JSON Schema.
Single source of truth, fail-fast codegen. Build-time only: no daemon,
no port, no SQLite sidecar — same posture as `leyline-cas-ffi`.

Born as `cloister/tools/schema-bridge`, generalized to multi-output
there (cloister ADR-0036 Phase 1), lifted here as ADR-0036 Phase 2 —
the same move as the 2026-05-09 `leyline-sign` lift. Cloister consumes
this crate upstream; other ART substrate repos (signet, notme, mache,
rosary) adopt as they grow schemas to consume.

## Why

cloister had two parallel schema pipelines: capnp→TS for the manifest,
zod→JSON Schema for tool I/O. Adding a third source (the `.cloister.json`
CLI config) would have meant hand-mirroring a capnp struct against a
zod schema, with ADR-0004's append-only / monotonic-ordinal guarantees
dropped on the floor. Bad shape, deferred forever.

schema-bridge is the missing piece: read a capnp schema, lower into a
small intermediate representation (IR), emit every downstream target
from that IR. capnp's own ordinal rules carry through; new fields land
in one place; nothing drifts.

## Self-maintenance invariant

The point of this tool is that it stays correct without anyone
remembering to update it. The mechanism: **any capnp construct without
a complete IR-and-emit mapping is a hard error.**

```text
unmapped capnp construct `list` at node id=aaaa (Foo.items):
  add a mapping for `list` in schema-bridge, or open an issue
```

This means the codegen is *intentionally incomplete*, but every gap is
loud. notme's older `capnp-to-ts.ts` (which this tool replaces in
spirit) silently emitted `z.unknown()` for unrecognised constructs;
that's the precise failure mode schema-bridge exists to prevent.

**Today the codegen is opt-in** — cloister's Taskfile carries entries
per (schema, format) that regenerate + drift-check, but none are wired
into its `task lint` or `task verify` yet:

| schema | task | drift gate | verify gate |
|---|---|---|---|
| cluster.capnp | `cluster:zod` / `cluster:go` | `cluster:zod:check-drift` / `cluster:go:check-drift` | `cluster:go:verify` (round-trip; ADR-0036 D) |
| identity.capnp | `identity:zod` / `identity:go` | `identity:zod:check-drift` / `identity:go:check-drift` | none yet (no canonical const) |

An unmapped capnp construct won't break CI automatically; it WILL
break the moment a developer runs any of the regen or drift-check
tasks. The plan is to wire the drift gates into `task verify` once
mapping coverage stabilises — at that point unmapped constructs
become a hard CI failure. No silent fallbacks regardless.

## What's mapped today

| capnp construct                        | IR                          | zod emit                                        |
|----------------------------------------|-----------------------------|-------------------------------------------------|
| `struct`                               | `Struct { fields, union }`  | `z.lazy(() => z.object({…}))`                   |
| scalar fields                          | `Scalar(_)`                 | `z.string()` / `z.number()` / etc.              |
| struct refs                            | `StructRef(name)`           | `{Name}Schema`                                  |
| enum refs                              | `EnumRef(name)`             | `{Name}Schema` (where `{Name}Schema = z.enum`) |
| `List(T)`                              | `List(Box<FieldType>)`      | `z.array(T)` (recurses)                         |
| top-level `enum`                       | `Enum { name, variants }`   | `z.enum([…])` + `type X = "a" \| "b"`           |
| `name :union { … }` (group form)       | `Union { discriminant_name: Some(_) }` | nested: `z.object({ disc: z.union([{<variant>: <T>}, …]) })` |
| `struct Foo { union { … } }` (anonymous inline) | `Union { discriminant_name: None }` | flat: `z.union([z.object({…base, <variant>: <T>}).strict(), …])` |
| Void union variants                    | `UnionVariant.ty = Void`    | `{<variant>: z.null()}` inside the union (both shapes) |
| union-only structs (no base fields)    | empty `fields`, `Some(union)` | same union shape, no base-field props |

Continuously emitted + drift-gated in cloister (via `task
{cluster,identity}:{zod,go}` + `:check-drift`):

- `manifest/cluster.capnp` → 440 lines zod TS (`src/generated/cluster.zod.ts`)
  + 360 lines Go (`pkg/cluster/cluster.go`); includes the all-Void
  `Wire.transport` union (drives the C void-marshaler emit) and the
  6-variant `Bundle.kind` union
- `manifest/identity.capnp` → 359 lines zod TS (`src/generated/identity.zod.ts`)
  + 186 lines Go (`pkg/identity/identity.go`); vendored from notme,
  covers `Proof`'s anonymous-inline union — the second-schema proof
  per ADR-0036 Phase 1 piece E / cloister-77172d

`manifest/cloister.capnp` is NOT in this list: it goes through the
separate capnp-eval pipeline (`task manifest` → `src/generated/manifest.ts`)
because it's evaluated as a value, not codegen'd to a type-only module.
Schema-bridge has never processed `cloister.capnp`; if it ever did, this
README would gain a third bullet.

| Deliberately unmapped (errors today)| reason                                       |
|-------------------------------------|----------------------------------------------|
| `interface`                         | RPC types — out of scope for now             |
| `anyPointer`                        | typed-erasure escape hatch; unmapped         |
| generics (`$Foo(T)`)                | needs IR generics representation             |
| non-union group (field namespacing) | unused in cloister                           |
| group variant inside a union        | legal capnp, unused in cloister              |
| annotation USES on a node/field     | including `$Json.flatten`, `$Json.discriminator`, `$Json.name`, `$Json.base64`, `$Json.hex`, `$Json.notification` (ids from `capnp/compat/json.capnp`) — affect JSON encoding and so MUST be handled or fail loudly. File-level annotation uses (e.g. `$Go.package` on the file node) are tolerated; node/field-level uses still fail-fast. |

Top-level annotation DECLARATIONS (e.g. an imported `go.capnp` defining
`annotation package(file) :Text;`) are skipped — they're metadata
describing what annotations EXIST, not data to render. USES of those
annotations on individual nodes/fields still fail-fast per the table
above.

Adding any of these is a focused change: extend the IR variant, add
the emit in `outputs/zod.rs`, add one golden test + leave one
fail-case test for the still-unmapped neighbour. The fail-case tests
stay forever as regression guards — they catch a future construct
that silently slips through because it looks "close enough" to
something that IS supported.

## Visibility of known gaps

Every unmapped construct above is paired with two tests:

1. **A regression-guard fail-fast test** — must throw
   `UnmappedConstruct`. Stays active forever; catches a future
   construct that silently slips through.
2. **An `#[ignore]`'d aspirational stub** (where the emit shape is
   already clear) — documents what success will look like. `cargo
   test` prints `<name> ... ignored, schema-bridge does not yet …`
   on every run, so the gap is visible in CI output without breaking
   the build. Activation gesture: remove `#[ignore]`, implement, fill
   in the assertions. The paired regression-guard stays.

Today's `#[ignore]`'d stubs (search for them in
`tests/integration.rs`):

- `flat_union_emit_under_json_flatten` — emit when `$Json.flatten`
  is on a union field
- `non_union_group_emits_nested_object` — emit for
  `field :group { x; y; }` (field namespacing without discriminator)

Closed gaps (no longer #[ignore]'d):

- `anonymous_inline_union_emits_flat` — `struct Foo { union { … } }`
  emits flat per cloister-77172d (the second-schema generalization,
  needed for notme's `Proof` struct in `identity.capnp`)

## Go output — canonical Marshal/Unmarshal for Void union variants

The Go emitter (v2, cloister-765d83) emits custom `MarshalJSON` +
`UnmarshalJSON` methods on union helper types that contain at least
one Void variant. Without these, `*struct{}{}` round-trips through
Go's default JSON encoder as `{}`, but capnp's canonical JSON
convention uses `null` for the void payload — and unmarshaling
`{"variant":null}` with default decoding clears the pointer
silently, losing the variant selection.

The generated marshalers fix both directions:

- **Marshal**: each variant with a non-nil pointer emits its key
  with a `null` value for Void variants or the payload's `json.Marshal`
  output for struct/scalar variants.
- **Unmarshal**: KEY PRESENCE (not value) selects the Void variant.
  Payload variants unmarshal normally.

Payload-only unions (no Void variants) skip the custom marshalers —
Go's default encoder handles them correctly, and adding noise to
generated code without value is the wrong tradeoff.

**Not yet emitted**: canonical CBOR. Cross-language canonical CBOR
in the ART substrate today (signet's `BundleCanonical`) uses
integer-keyed maps per the capnp ordinals, but the field-inclusion
rules are protocol-specific (signet excludes the signature field
from its signing input; other consumers would have different
exclusions). Schema-bridge stays out of that decision; consumers
that need canonical CBOR hand-roll the integer-key map from the
generated types, using fxamacker/cbor's CanonicalEncOptions for
deterministic byte output.

Constructs without aspirational stubs (`interface`, generics,
`anyPointer`) are deferred indefinitely — they're non-goals for the
zod-validation surface today, not just "not yet."

## JSON Schema output — draft 2020-12 (ley-line-open-6585aa)

Third emitter. First consumer: rosary's MCP tool registry
(rosary-08a278) — MCP `inputSchema` is a draft 2020-12 object schema,
so each capnp struct lands as a `$defs` entry
(`type`/`properties`/`required`) a consumer plucks verbatim into a
`tools/list` response. One document per schema file:
`$schema` + `$comment` + `$id` (`<basename>.schema.json`) + `$defs`.

Mapping semantics mirror the zod emitter:

- **structs** → object schemas with ALL fields `required` (capnp has
  no optional fields) and `additionalProperties: false` (the
  `.strict()` typo-rejection invariant, cloister-cf2e6a).
- **named-group unions** → the discriminant property is a `oneOf` of
  single-key branch objects (`"kind": {"durableObject": {…}}`), Void
  payloads as `{"type":"null"}` — capnp's nested JSON convention.
- **anonymous-inline unions** → the whole struct def is a `oneOf`
  whose branches inline base fields + exactly one variant key (flat
  encoding, cloister-77172d).
- **Data** → `{"type":"string","contentEncoding":"base64"}` — the
  wire-JSON view (Go's `encoding/json` base64s `[]byte`); zod's
  `z.instanceof(Uint8Array)` validates the decoded in-memory value
  instead.
- **consts** → `{"const": <value>}` `$defs` entries. Non-finite float
  consts are a hard error — JSON has no Inf/NaN literal and no
  sentinel that stays valid JSON.
- **no `description` fields**: capnp's CodeGeneratorRequest carries no
  doc comments (capnp discards comments at parse). Descriptions arrive
  via an annotation vocabulary when a consumer needs them, not
  invented here.

Determinism: direct string emission in IR declaration order (enums,
structs, consts — the zod/go order); no map-keyed serialization, so
key order is stable by construction. A cross-emitter consistency gate
in `tests/integration.rs` (`assert_cross_emitter_agreement`) holds all
three emitters to the same field names, optionality, union variants,
and enum values for the shared fixtures.

## How it runs

```sh
# As a capnp plugin (the supported invocation):
capnp compile \
  -o./target/release/capnpc-schema-bridge-zod:./gen \
  manifest/cluster.capnp
# → ./gen/cluster.zod.ts

# Same shape for Go output:
capnp compile \
  -o./target/release/capnpc-schema-bridge-go:./gen \
  manifest/cluster.capnp
# → ./gen/cluster.go

# And JSON Schema:
capnp compile \
  -o./target/release/capnpc-schema-bridge-jsonschema:./gen \
  manifest/cluster.capnp
# → ./gen/cluster.schema.json
```

One binary per output format, dispatched by argv[0] basename — same
shape as `capnpc-rust` / `capnpc-go` / `capnpc-c++`. Today
`capnpc-schema-bridge-zod` (cloister-7585bc),
`capnpc-schema-bridge-go` (cloister-75f6d5), and
`capnpc-schema-bridge-jsonschema` (ley-line-open-6585aa); Cargo
declares all three `[[bin]]` entries and they compile from the same
`src/main.rs`.

`capnp compile` invokes the binary with the parsed `CodeGeneratorRequest`
on stdin. The binary writes `<output-dir>/<schema-basename>.<format-suffix>`
(e.g. `cluster.zod.ts` from `manifest/cluster.capnp`) — zod schemas
plus TS interface declarations in one file. One emit per invocation
today; per-file splitting is on the follow-on list.

For development the library is also drivable directly — see
`tests/integration.rs` for examples of building a `CodeGeneratorRequest`
by hand. That's how the test suite stays hermetic (no capnp CLI
needed in CI).

## Layout

```
rs/ll-open/schema-bridge/
├── Cargo.toml          workspace member; depends only on capnp + thiserror
├── README.md           this file
├── src/
│   ├── lib.rs          public API for tests
│   ├── main.rs         capnp plugin entry — stdin → emit → file
│   ├── error.rs        SchemaBridgeError + UnmappedConstruct
│   ├── ir/             the intermediate representation
│   ├── inputs/         capnp → IR (future: json-extension/ for aggregation)
│   └── outputs/        IR → zod / go / json_schema (future: ts.rs)
└── tests/
    └── integration.rs  golden + fail-case suite
```

## Follow-on work

Tracked separately from this initial drop. In rough priority order:

1. Wire into `task manifest` + `task verify` — codegen step alongside
   the existing capnp→TS pipeline. Decide whether the output replaces
   `src/generated/cluster.ts` or sits beside it as
   `src/generated/cluster.zod.ts`.
2. JSON-extension input adapter for the aggregation pattern (capnp
   defines the structural backbone, JSON files supply per-variant
   field extensions). Where the polymorphism for skill / mcp / agent
   actually lands.
3. TS-types-only output adapter, separated from the zod emit, so
   consumers can pick one or both.
4. End-to-end fixture tests against cloister's `manifest/*.capnp` —
   currently verified manually (see README "What's mapped today");
   locking that in as a golden-output test in CI prevents silent
   regressions.

## Non-goals (the helm comparison)

The aggregation pattern this tool serves looks superficially like
helm — multiple inputs composing into one output — but the design
explicitly avoids helm's failure modes:

- ❌ No string templating (no `{{ … }}` substitution anywhere)
- ❌ No runtime value substitution
- ❌ No values.yaml-style override layers chained 4-deep
- ✅ All aggregation is at the IR level, statically resolved
- ✅ Output is plain emitted source code, reviewable and diffable

If a feature looks like it might pull this toward helm-shaped
templating, reject the feature.
