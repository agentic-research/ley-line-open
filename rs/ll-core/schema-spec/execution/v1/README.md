# execution/v1

`cloister/execution/v1` is LLO's vendor-neutral execution capability.
`execution.capnp` is the data-contract source of truth. The Rust service,
daemon transport, first-party CLI, and MCP adapter must all implement the same
operations and error vocabulary derived from it.

The central split is intentional:

- `RunSpec` is content-addressed intent and is safe to accept from an
  untrusted caller.
- `RunGrant` is authenticated, resolved authority bound to one `RunSpec`
  digest. It is accepted only after Signet/Interlace verification or over an
  authenticated policy-resolver channel.
- `RunReceipt` is LLO's evidence of enforcement and artifact lineage. A
  separate orchestrator may include its digest in APAS; the receipt is not
  itself an APAS statement.

No live arena path, SQLite path, raw credential, product authentication rule,
or backend implementation name appears in the portable contract. Workspaces
are logical Graph roots with explicit operations. A `.db` can be an immutable
artifact, but never live storage authority.

`capabilities`, `status`, and `inspect` are read-only. In particular, `status`
must not create a volume or initialize a backend. `provision`, `start`,
`cancel`, and `cleanup` are explicit idempotent mutations.

Schema evolution is append-only within v1: add fields at new ordinals and add
enum variants at the end. Removing or reinterpreting a field requires v2.

The canonical vector in `test-vectors/canonical-run.json` pins the JSON carrier
shape used by conformance tests. JSON is a carrier; Cap'n Proto remains the IDL.
