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

## Workspace graph root

A `WorkspaceGrant.graphRoot` served by an LLO store is
`blake3-256:<storeRoot>`: the store's parser-free directory Merkle root
(`leyline/store-tree/v1`, ADR-0047 D-ii; construction in
`rs/ll-core/schema-capnp/schemas/head.capnp` `storeRoot`, known-answer
vectors in `../../sigma/v1/test-vectors/constructions.json` `storeRoot`).
The `blake3-256:` tag names the hash algorithm only; this paragraph names
the construction. A grant is never compared with a blob hash, an arena
`current_root` or a run's `rootHash`. Before ADR-0047 M1 (bead
`ley-line-open-73c6ee`) the store compared `graphRoot` with the retired
`treeRoot`, which folded `parserId`; a grant minted against a `treeRoot`
does not match any `storeRoot` and is refused with
`identityPolicyMismatch` before the run starts.

## Workspace generation

`WorkspaceGrant.expectedGeneration` is ADR-0041 S1's commit `seq` within the
grant's epoch: the number of proposals the workspace had accepted when the
grant was issued. Zero claims no generation; the grant's `graphRoot` alone
pins the run. No LLO store records `(epoch, seq)` yet (ADR-0041 is
Proposed), so a store refuses a grant whose `expectedGeneration` is nonzero
with `identityPolicyMismatch` before the run starts, rather than running it
against a generation nothing checked. When a store implements S1, it compares
the claim with its `seq` instead.

## Workspace write scope

`WorkspaceGrant.writeScope` (field 4, bead `ley-line-open-483134`) names
where the run may write, create, rename or delete. Each entry is relative to
the workspace root:

| entry | grants |
| --- | --- |
| `src/a.rs` | that one file |
| `docs/` | everything beneath that directory, at any depth, existing or not |
| `/` (alone) | the whole workspace |

Rules an authorizer holds, each refused as `invalidSpec` naming the
workspace:

- A grant whose `operations` include `mutate` or `commit` names a non-empty
  `writeScope`. Write authority is never implied, so a grant issued before
  this field existed (`mutate` with no scope) is refused; reissue it with
  `["/"]` to keep its old meaning.
- A grant with neither operation leaves `writeScope` unset.
- **"No scope" has one encoding: the field left unset.** An explicit empty
  list (`"writeScope": []`) is refused. Cap'n Proto canonicalization keeps a
  zero-length list as a non-null pointer, so the empty list and the unset
  field are different signed bytes (`test-vectors/run-grant-signing.json`);
  accepting both would give one grant two digests.
- An entry is `/` alone, or a path with no leading `/`, no empty, `.` or `..`
  component and no NUL. No entry appears twice.

The scope bounds authored paths, the ones a write turns into a proposal. What
the run may read is the `graphRoot`, and `read` or `list` shows the whole
tree. A change outside the scope is refused by the share with the typed
`outside_grant` refusal, which the guest sees as `EACCES`; nothing lands.

- A rename needs both its names inside the scope.
- Creating a directory on the way to a granted path is inside the scope.
  Removing or moving a directory is inside it only when the directory is a
  granted tree or lies beneath one: a directory above the scope is never the
  run's to take away.
- Derived paths are outside the scope's subject. A path the tree's ignore
  rules match (build output such as `target/`, and the editor and OS temp
  names the share treats as derived) is never a proposal: it lives in the
  run's scratch and never reaches the store, so a run with any write scope
  may write it, whatever the scope names. A temp name the ignore rules do
  not match is an authored path and is held to the scope; an editor that
  saves by writing such a name beside a granted file needs the directory in
  scope.

### A daemon says whether it enforces the scope

The field is additive on the wire, and that is a hazard in one direction. A
daemon that predates it decodes a scoped grant, ignores the scope, and lets
`mutate` write anywhere in the workspace; nothing in its replies says so. So
support is advertised and required:

- A daemon that enforces `writeScope` lists
  `{name: "cloister/execution/v1/workspace-write-scope", version: "1"}` in
  `llo_execution_capabilities`.
- An issuer MUST find that entry before it issues a grant whose `writeScope`
  is narrower than `/`, and MUST refuse to issue one to a daemon that does
  not list it.

Inside one deployment the daemon and its worker binary may be different
builds. Either skew is safe: the scope is enforced in the daemon, by the
share it serves, so an older worker has nothing to ignore; and a newer worker
refuses an older daemon's request for a run with a workspace, which lacks the
field.

## Workspace delivery

A run never starts without a workspace its grant names. The execution
service opens each granted workspace itself, before it calls the backend,
and hands the backend the open shares (bead `ley-line-open-1022a8`). A start
that cannot be served is refused before anything is spawned, with
`workspaceUnserved` (not retryable under the same grant):

| tier | serves |
| --- | --- |
| microVM, daemon started with a source tree | one workspace, as the guest's `/work` |
| microVM, no source tree | none |
| native | none |

`workspaceUnserved` names only this refusal, so a caller that chose the
backend class can tell "this tier serves no workspace" from the
`unsupportedBackend` refusals (egress, an unenforceable ceiling, another
tier's grant) by code. The Rust `ExecutionError` carries it as `code`; the
daemon's error envelope, on the control socket and over MCP alike, carries
it as `executionError: {"code": "workspaceUnserved", "retryable": false,
"message": "…"}` (`daemon.capnp` `ExecutionError`, bead
`ley-line-open-e347ae`), beside
the unchanged error text, whose `WorkspaceUnserved: …` prefix earlier
consumers match. Every failed `llo_execution_*` op carries
`executionError` with its own code, including `invalidSpec` for a request
the op cannot decode and `unsupportedBackend` from a daemon with no
execution/v1 service; `detailRef` is not carried. Choosing another tier is the caller's policy; the runtime
does not reroute a run.

Workspace authority is handed over, not reported back. In LLO's Rust
embedding a backend receives `ServedWorkspaces`, which only the service can
construct and only for a verified grant, and it names a share in its evidence
document (the `workspaceShare` channels, below) only by taking one of those
shares. So a backend cannot claim a share the grant never named. A request
that names workspaces without a verified grant behind it (a direct
`ExecutionService::start`, or a backend called with no shares) is refused
with `workspaceUnserved`.

The service asks the backend how many workspaces it serves a run before it
opens anything, so a tier that serves none is refused with no share opened.

One assertion remains when a backend answers a start. Each share it took
must have been opened for this run under this run's grant, whole (name,
`graphRoot`, operations and `writeScope`), each granted workspace once; and
each must still be held by whatever the backend gave it to. That proves the
backend took the run's own shares and something is serving them. It does not
prove the worker's descriptor is wired to the share, which is each backend's
contract with its worker (the libkrun tier holds its worker's readiness
declaration to the descriptor it handed over). A backend that answers a
start having dropped, let go of, or swapped a share has its run cancelled
and the start refused with
`workspaceUnserved`; the run is never recorded, collected or receipted. If
that cancel fails, the workload may still be running, so the run is recorded
`failed` instead of forgotten: it has no receipt, its `runId` refuses
another start with `resourceConflict`, and `cleanup` retries the cancel. No
first-party tier reaches it: each attaches what it is handed or refuses
before it spawns.

`workspaceUnserved` was appended to `ErrorCode` at ordinal 12 (bead
`ley-line-open-204a94`). Before it, the same refusals were
`unsupportedBackend`.

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

A run's rootfs is named by the rootfs digest, `RunReceipt.inputRoots[0]`:
BLAKE3-256 over the exact bytes of its `rootfs.manifest.json`.
`test-vectors/rootfs-manifest.json` states the canonical form those bytes
take and pins one case, the checked-in OCI fixture's linux/arm64 manifest
and its digest, checked against what `leyline rootfs import-oci` writes
(ADR-0047 §3, bead `ley-line-open-39309e`).

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

## Verifier selection

The verifier is local configuration, not wire content; nothing in a request
selects it. An embedding authority passes its own. The first-party
`leyline execution-daemon` installs one of three:

- `--evidence-store DIR --trust-key FILE...` — LLO's `CasDsseEvidenceVerifier`
  over a CAS blob store, trusting exactly the operator's Ed25519 keys. It
  checks the grant signature above and resolves each `EvidenceRef.digest`
  (`blake3-256:<hex>` over the exact DSSE envelope bytes) in the store.
- No verifier flags — every start is refused `unauthenticated`.
- `--allow-unverified-evidence` — metadata only, for local fixtures.

Any partial or invalid configuration refuses startup; the downgrade never
coexists with a real verifier. A signature is checked under every trusted key,
and `RunGrant.signature.keyId` and the DSSE `keyid` select nothing. After every
key has refused a signature, its hint is compared with the trusted keys'
ADR-012 canonical kids only to name the refusal: `signature-invalid` when it
names a trusted key, `untrusted-signer` otherwise. The wire `ExecutionError`
is unchanged; LLO's verifier leads `message` with a stable rejection token
(`grant-unsigned`, `evidence-missing`, …).

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

`ProvisionOutput.casRoot` (appended 2026-11-10, bead `ley-line-open-ba86d3`)
is the absolute host path of the content-addressed rootfs store the provider
serves, made to exist by that provision: with `execution-daemon --cas-volume`,
the CAS root of the case-sensitive volume it provisioned (on macOS, a
sparsebundle it attached); with `--cas-root`, that directory. It is the
directory `leyline rootfs import-oci --cas-root` writes to. Empty from a
provider that reports none.

Schema evolution is append-only within v1: add fields at new ordinals and add
enum variants at the end. Removing or reinterpreting a field requires v2.

*One recorded exception (2026-10-27, bead `ley-line-open-da36d2`).*
`ResourceLimits` and `EnforcedCeilings` were replaced in place, pre-1.0, by
the user's decision; see "Resource limits" below and ADR-0035 "OCI resources
on execution/v1". The rule above holds for every other change.

## Resource limits

`ResourceLimits` carries OCI runtime-spec `linux.resources` `memory.limit`
(bytes), `cpu.quota`/`cpu.period` (microseconds) and `pids.limit`, plus the
LLO-only `wallTimeMs` and `outputBytes`. Zero means no ceiling for every
field; a zero `cpu.period` means 100000, and a `cpu.period` without a
`cpu.quota` is refused. A run is held to `RunGrant.limits`, which may only
narrow `RunSpec.requestedLimits` (CPU compares bandwidth, `quota / period`).

| field | microVM | native, Linux, `--cgroup-root` | native, macOS or no root |
| --- | --- | --- | --- |
| `wallTimeMs` | `supervisor` | `supervisor` | `supervisor` |
| `outputBytes` | refused | refused | refused |
| `memory.limit` | `hypervisor`, whole MiB | `cgroup` (`memory.max`) | refused |
| `cpu.quota`/`period` | `hypervisor`, whole vCPUs 1..=255 | `cgroup` (`cpu.max`) | refused |
| `pids.limit` | refused | `cgroup` (`pids.max`) | refused |

A refused ceiling fails `start` with `unsupportedBackend` and names the field
(`grant limit pids.limit cannot be enforced by the selected backend`). The
microVM tier also requires `memory.limit` and `cpu.quota`, since it sizes the
guest from them. `RunReceipt.backend.enforced` records one `CeilingMechanism`
per field. `test-vectors/oci-resources.json` is the conformance case: one
spec and grant, three tiers, the outcome each must produce.

The replaced shape was `{wallTimeMs @0, memoryBytes @1, cpuMillis @2,
outputBytes @3}`: four data words and no pointers. What an old producer gets
from a current daemon:

- **Binary.** Cap'n Proto would let a current reader see only words 0 and 1
  (as `wallTimeMs` and `outputBytes`) and null `memory`, `cpu` and `pids`,
  dropping `cpuMillis` and the old `outputBytes` without a trace. The daemon
  does not let it: a `ResourceLimits` with no pointer section and more than
  two data words is the old layout, and any nonzero `memoryBytes`, `cpuMillis`
  or `outputBytes` in it is refused with `invalidSpec`, naming the old field.
  An old struct that sets only `wallTimeMs` means the same in both layouts and
  is accepted. More generally, any nonzero data word past the ones this
  reader knows, in `ResourceLimits` or in `memory`, `cpu` or `pids`, is
  refused rather than ignored: an unread ceiling is an unenforced one.
- **JSON.** The carrier refuses the keys `memoryBytes`, `cpuMillis`, and any
  other key `ResourceLimits` does not define, by name.

What a current client gets from an OLD daemon is not something the old daemon
can refuse: it reads the OCI layout as its four words, and `memory`, `cpu` and
`pids` vanish. So the layout has a revision of its own, which
`llo_execution_capabilities` advertises as the entry
`{name: "cloister/execution/v1/resource-limits", version: "2"}`. A client MUST
find that entry before it sends `start`, and treat its absence as a daemon
that would drop every OCI ceiling. Revision 1, the pre-OCI layout, was never
advertised.

The canonical vector in `test-vectors/canonical-run.json` pins the JSON carrier
shape used by conformance tests. JSON is a carrier; Cap'n Proto remains the IDL.

## Release assets

Every LLO release publishes this contract in two forms, each listed with its
SHA-256 in the release's `SHA256SUMS`:

| Asset | Contents |
| --- | --- |
| `execution.capnp` | This directory's `execution.capnp`, byte for byte. |
| `traits.capnp` | `rs/ll-core/schema-spec/_traits.capnp`, byte for byte: the one file `execution.capnp` imports. |
| `execution.tools.json` | The MCP tool definitions `capnpc-schema-bridge-tooldefs` emits from `execution.capnp`. |

`execution.tools.json` is a projection: JSON Schema for the eight
`llo_execution_*` payloads. A consumer that wants its own types for `RunSpec`,
`RunGrant` and `RunReceipt` compiles the schema assets instead. The traits file
ships as `traits.capnp`, without its leading underscore, and
`execution.capnp` imports it as `../../_traits.capnp`, so lay the two out
before compiling:

```sh
mkdir -p schema/execution/v1
cp traits.capnp    schema/_traits.capnp
cp execution.capnp schema/execution/v1/execution.capnp
cd schema && capnp compile --no-standard-import \
  -o"$PLUGIN:$OUT" execution/v1/execution.capnp
```

With `PLUGIN` set to the same release's `capnpc-schema-bridge-zod-<os>-<arch>`,
run under its published name, this writes `execution.zod.ts` with `RunSpecSchema`, `RunGrantSchema` and
`RunReceiptSchema`. Verify every asset against `SHA256SUMS` before use, and
take the schema, plugin and tooldefs from one release. No other file is
needed: staging refuses to publish an `execution.capnp` that imports anything
but the traits file, and `task schema:execution-asset-consumer-test` (in
`task ci`) and the release dry-run compile the staged assets exactly this way.
