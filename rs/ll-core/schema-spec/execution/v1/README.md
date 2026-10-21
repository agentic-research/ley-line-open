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

## Workspace generation

`WorkspaceGrant.expectedGeneration` is ADR-0041 S1's commit `seq` within the
grant's epoch: the number of proposals the workspace had accepted when the
grant was issued. Zero claims no generation; the grant's `graphRoot` alone
pins the run. No LLO store records `(epoch, seq)` yet (ADR-0041 is
Proposed), so a store refuses a grant whose `expectedGeneration` is nonzero
with `identityPolicyMismatch` before the run starts, rather than running it
against a generation nothing checked. When a store implements S1, it compares
the claim with its `seq` instead.

## Issuer signature

`RunGrant.signature` is the only signed surface in this capability.
`wire/run-grant.md` specifies the covered bytes; in short, an issuer signs
`PAE("application/vnd.cloister.execution.run-grant+capnp", canonical(grant with
signature cleared))`. It is what ties `capabilities`, `limits`,
`backendClass`, `confinementDigest` and `expiresAtUnixMs` to an authority —
every other check in this contract reads a field the grant chose for itself.

## Evidence binding

A `RunGrant` carries three `EvidenceRef`s — `issuerEvidence`,
`workloadIdentityEvidence`, `actorProvenanceEvidence`. They are structurally
identical, so evidence that merely resolves and verifies is not authority:
without a binding rule, one trusted envelope in the catalog satisfies all
three fields for every run.

The run identity is the binding. It is derived, not chosen:

```
run_id = "run-" || blake3(
    "cloister/execution/v1/run-id" || 0x00 ||
    u64le(len(canonical_spec_digest)) || canonical_spec_digest ||
    u64le(len(grant_id))              || grant_id              ||
    u64le(len(replay_key))            || replay_key
)
```

where `canonical_spec_digest` is the ASCII `blake3-256:<hex>` digest of the
`RunSpec`'s Cap'n Proto **canonical** form. `test-vectors/run-id.json` pins
the derivation; an implementation can compute a run's name locally without
calling `start`.

For in-toto/DSSE evidence, a statement authorizes field `F` of a run only if
it carries a subject whose `name` is `F`'s field name and whose
`digest["blake3"]` is that run identity, verbatim including the `run-` prefix.
BLAKE3 rather than SHA-256 because a run identity is a content address, not a
key name (signet ADR-012). One envelope may assert several roles by carrying
several subjects; what it cannot do is assert a role it does not name, or
authorize a run it does not name.

Evidence in another format binds under whatever rule its verifier defines. The
substrate derives the run identity before it checks any evidence and hands it
to every verifier, so a verifier that accepts unbound evidence is choosing to;
it has what it needs to refuse.

## Backend evidence

`RunReceipt.backend.evidence` names its contract by media type. LLO's is
`application/vnd.leyline.execution.backend-evidence+json`: the digest
(`blake3-256`) of one JSON document naming the backend that ran the workload
and every channel its worker held beyond the `confinement/v1` manifest. A
confinement manifest describes what the confined process may open; a
descriptor the process inherits already open is outside it and outside any
sandbox profile compiled from it, so the backend declares it here, the
worker refuses to start holding a descriptor it does not declare, and the
receipt commits to the declaration.

```json
{"backendId":"libkrun/1","channels":[{"graphRoot":"blake3-256:<hex>","role":"workspaceShare","workspace":"repo"}],"schemaVersion":"leyline/execution-backend-evidence/v1"}
```

- `backendId` — the receipt's `backend.backendId`.
- `channels` — one entry per channel, empty when the worker holds none. The
  one role v1 defines is `workspaceShare`: the run's `WorkspaceGrant`
  (`workspace` its `name`, `graphRoot` its `graphRoot`) served to the guest
  over a connected UNIX socket whose peer, in the substrate, answers the
  guest's filesystem requests (ADR-0044). A run without a workspace has
  none.
- `schemaVersion` — `leyline/execution-backend-evidence/v1`.

Canonical bytes: UTF-8, compact (no insignificant whitespace), object keys in
ASCII byte order at every level, no trailing newline. Every value is one an
issuer knows from its own grant (no descriptor number or host path), so a
verifier recomputes the document and compares digests.

`capabilities`, `status`, and `inspect` are read-only. In particular, `status`
must not create a volume or initialize a backend. `provision`, `start`,
`cancel`, and `cleanup` are explicit idempotent mutations.

Schema evolution is append-only within v1: add fields at new ordinals and add
enum variants at the end. Removing or reinterpreting a field requires v2.

The canonical vector in `test-vectors/canonical-run.json` pins the JSON carrier
shape used by conformance tests. JSON is a carrier; Cap'n Proto remains the IDL.
