# LLO execution/v1 boundary

**Status:** accepted direction; implementation tracked by `ley-line-open-d7abd6`

## Decision

LLO will expose one schema-first execution substrate with native `nono` and
`libkrun + nono` backends. Cloister resolves its policy into an authenticated
grant and calls that API. It does not shell out to `krunvm`, Taskfile, or an LLO
CLI. The Rust library, daemon/UDS, first-party CLI, and MCP tools are projections
of the same versioned contract.

`workerd` remains a useful Cloudflare compatibility runtime, but it is guest
workload software rather than LLO's isolation boundary. LLO does not need to
embed V8 to obtain isolation: `nono` constrains a native process and `libkrun`
provides the VM boundary. A workload may run workerd inside either backend when
that compatibility is required.

## Ownership

| Component | Owns | Does not own |
| --- | --- | --- |
| LLO | content-addressed storage, CDC, Graph access, execution lifecycle, backend enforcement, events and receipts | product policy, Claude authentication, agent scheduling |
| Cloister | policy compilation, user-facing runtime CLI, Claude Code audit/auth behavior, translation to LLO requests | creating VMs or opening LLO arenas directly |
| Signet | principals, delegated authority, bridge certificates and signing formats | execution lifecycle or filesystem implementation |
| Interlace | lease, workload-identity, peer-attestation and discovery protocol | execution lifecycle or product policy |
| Mache | structural projection and code-intelligence behavior | live SQLite arena ownership or raw arena mutation |
| Rosary | orchestration and dispatch as a downstream client | gating the LLO execution contract |
| workerd | Cloudflare-compatible guest runtime | host or VM isolation boundary |

Taskfile may invoke first-party commands for repository automation. It must not
contain product behavior that installed clients need.

## Security claim and threat model

The substrate distinguishes three adversaries:

1. An untrusted agent or tool is denied ambient filesystem, network, process,
   and credential access by an enforced confinement policy.
2. An ordinary host process is denied direct access to state placed exclusively
   inside a VM. The VM receives only explicit capabilities and logical content
   references.
3. A privileged host administrator remains trusted. Neither `libkrun` nor
   `nono` protects secrets from host root, a compromised hypervisor, or a
   compromised LLO daemon.

The native backend is suitable when kernel confinement is enough. The VM
backend is required when the threat model includes other unprivileged host
processes reading raw workspace or database state. Backend choice is a resolved
policy decision recorded in the grant and receipt, not a caller-controlled hint.

## Identity, authority, and interface are separate

The existing three-lane mapping remains normative:

| Lane | Example | Meaning |
| --- | --- | --- |
| Signet grant | `urn:signet:cap:<action>:<resource>` | what the holder may do |
| Interlace/WIMSE identity | `wimse://...` | which workload is acting |
| Cloister interface | `cloister/<name>/v<n>` | which contract shape is requested |

Signet additionally distinguishes owner, machine, actor, and the identity that
binds owner to machine. Provenance, environment evidence, and boundary evidence
remain distinct. `execution/v1` therefore does not introduce a generic signer
string or copy Signet certificate fields into its schema. It carries typed,
content-addressed references to verified identity and provenance evidence.

The verifier resolves those references and confirms that:

- the lane-1 grants authorize every requested lane-3 interface and resource;
- the lane-2 workload identity is the intended subject;
- delegation and actor provenance are valid for this run; and
- the enforced confinement digest equals the digest committed into the
  workload identity certificate.

The last comparison closes the gap between "a policy was requested" and "the
identified workload actually ran under that policy."

### Protocol composition, not protocol amalgamation

There are three related contracts, with different attesters and truth claims:

| Contract | Truth claim | How execution/v1 relates |
| --- | --- | --- |
| Interlace | this workload/caller holds a verified lease and identity within a peer boundary | verifier input and identity/confinement evidence |
| `execution/v1` receipt | this substrate enforced this grant and produced these content roots | primary output of LLO execution |
| APAS | this dispatch participated in this agent decision/provenance chain | an orchestrator may include the LLO receipt as execution evidence |

An LLO receipt is not automatically an APAS attestation, and an Interlace lease
is not an execution grant. APAS deliberately requires separation between the
dispatch and attestation authority at L3; execution receipts give that authority
substrate evidence, but Rosary/Signet still build and sign the APAS statement.
Likewise, Interlace can authenticate a channel or identity without deciding
which filesystem or network capabilities a run receives.

Interlace's normative spec currently lives in Cloister even though LLO already
hosts the shared schema/IDL bridge, capability-spec tree, and native certificate
verification. Whether the protocol spec and vectors should publish from LLO is
a real ownership question, but it is not a prerequisite for execution/v1. It is
tracked separately as `ley-line-open-151b29` under the
`interlace-substrate/protocol-publication` thread. Whichever repository becomes
the publisher, there must be one source of truth and generated consumers rather
than copied definitions.

## Contract model

### RunSpec is intent

`RunSpec` is safe to accept from an untrusted client. It is content-addressed
and contains requested behavior, not authority:

- immutable command/artifact reference and arguments;
- named workspace inputs expressed as logical Graph or artifact references;
- public environment values and opaque secret-handle references;
- requested capability interfaces;
- resource limits, cancellation policy, and result declarations; and
- an optional compatibility runtime such as workerd.

Host paths, raw arena paths, SQLite filenames, certificate private keys, and
secret values are not portable `RunSpec` fields.

### RunGrant is resolved authority

`RunGrant` binds a `RunSpec` digest to authority resolved by a trusted policy
compiler. It contains:

- grant ID, issuer, expiry, replay/idempotency key, and `RunSpec` digest;
- verified workload-identity and actor/provenance evidence references;
- the authorized lane-1 grants and lane-3 interface versions;
- exact logical resources and allowed operations;
- a `confinement/v1` digest;
- required backend (`native-nono` or `libkrun-nono`);
- resource ceilings and permitted egress/credential brokers; and
- the expected input roots/generations used for optimistic concurrency.

An arbitrary JSON object is never authority. A grant must arrive in a verified
Signet envelope or across an authenticated local channel whose peer is allowed
to resolve policy. Verification happens before provisioning or materialization.

`execution/v1` references the existing `confinement/v1` contract rather than
duplicating its filesystem, network, port, and credential policy.

### Workspace and Graph capabilities

A workspace is identified by a logical Graph reference: graph/store identity,
root or head digest, generation, mount name, and allowed operations. The live
arena file and its SQLite projection are implementation details, never granted
resources.

The initial operation vocabulary is `read`, `list`, `query`, `mutate`, and
`commit`. A mutable grant includes the expected generation and produces a new
root plus a mutation receipt. LLO owns serialization, CDC, page/ring I/O, root
publication, and conflict detection.

A prebuilt `.db` may still be supplied as an immutable, read-only artifact for
compatibility. It is not treated as the live source of truth and cannot be used
to recover raw arena authority.

This matches Mache's existing read path: its `udsGraph` asks the LLO daemon for
nodes, children, content, callers, and callees without opening SQLite. Mache's
writable control path must migrate from `ExtractActiveDB`/`ArenaFlusher` to the
Graph mutation capability. That migration is tracked by `mache-0fed77` and
depends on the LLO ring-I/O and Graph exposure beads.

## Lifecycle API

The stable operation set is:

| Operation | Effect |
| --- | --- |
| `capabilities` | read-only backend/interface discovery |
| `status` / `inspect` | read-only substrate or run observation; never creates storage |
| `provision` | explicit, idempotent backend/storage preparation |
| `start` | verifies a grant, materializes capabilities, and starts a run |
| `cancel` | requests cancellation using the run capability |
| `collect` | obtains declared outputs and the terminal receipt |
| `cleanup` | explicitly releases ephemeral resources; idempotent |

Run states are `accepted`, `provisioning`, `ready`, `running`, `succeeded`,
`failed`, `cancelled`, `cleaning`, and `cleaned`. State changes append events;
they do not rewrite history. Retried mutating calls use an idempotency key and
return the same run or result.

Errors are typed and stable across transports. The first vocabulary includes
`invalid-spec`, `invalid-grant`, `unauthenticated`, `unauthorized`,
`identity-policy-mismatch`, `unsupported-backend`, `not-provisioned`,
`resource-conflict`, `resource-exhausted`, `backend-failed`, `cancelled`, and
`internal`. Errors carry retryability and structured details but never secrets.

In particular, `status` on an uninitialized or unavailable VM store reports a
typed state/error. It does not attempt to create `/Volumes/krunvm`, and it does
not tell an installed user to run an internal Taskfile target.

## Receipts

The terminal receipt binds:

- run ID and ordered event-log root;
- `RunSpec`, `RunGrant`, and confinement digests;
- workload-identity and delegated actor/provenance evidence references;
- backend identity/version and relevant VM or kernel enforcement evidence;
- input artifact/Graph roots and output artifact/Graph roots;
- timestamps, terminal state, exit classification, and resource accounting; and
- receipt schema/interface versions.

Receipts contain neither secret values nor the unrestricted environment. They
use the existing LLO/Signet envelope and Interlace certificate mechanisms; the
execution API does not create a second signing or identity system.

## Backends and isolation composition

`native-nono` launches through the Rust `nono` integration and exposes only the
resolved handles. `libkrun-nono` embeds libkrun rather than invoking the
`krunvm` CLI. LLO constructs the guest, exposes the minimum virtiofs/UDS or
ring-I/O capabilities, and applies `nono` to the host-side runner and brokers.
Using both is intentional defense in depth: the VM boundary and host broker
confinement cover different processes.

The content-addressed filesystem is a capability transport, not by itself an
isolation boundary. Isolation comes from making it the only reachable data path
and denying raw host paths. Content addressing then supplies immutability,
deduplication, reproducibility, and receipt-verifiable inputs and outputs.

## One schema, several projections

`execution/v1` is defined as Cap'n Proto data schemas and annotations consumed
by the existing schema/IDL bridge. Generation emits:

- Rust wire types through the Cap'n Proto compiler and hand-written service
  traits over those generated data types;
- Cap'n Proto/UDS request and event surfaces for local processes and VM brokers;
- JSON Schema and MCP tool definitions;
- CLI argument/result bindings; and
- conformance fixtures consumed without importing Cloister, Rosary, or Mache.

The daemon and library implement one core service. CLI and MCP adapters call it;
they do not reimplement lifecycle behavior. Schema evolution is additive within
v1. Removed fields, changed semantics, or newly required behavior require v2.
Unknown fields are preserved or rejected according to the generated transport
rules, never silently reinterpreted.

The bridge is build-time infrastructure owned by LLO; Cloister consumes pinned
generator artifacts and generated outputs. This does not move Cloister's policy
schema or product CLI into LLO. The bridge currently rejects Cap'n Proto
`interface` declarations, so v1 models operations as annotated request/response
data and keeps the Rust service trait explicit. Native interface lowering is a
separate bridge enhancement only if a second concrete consumer justifies it.

## Adoption sequence and level of effort

| Slice | Outcome | Bead | Estimated effort |
| --- | --- | --- | --- |
| Boundary | this design and dependency cleanup | `ley-line-open-d7abd6` | 1 day |
| Schema | `RunSpec`, `RunGrant`, events, errors, receipts and fixtures | `ley-line-open-f7d6cd` | 3–5 days |
| Native backend | core lifecycle plus `nono` enforcement | `ley-line-open-f81567` | 1–2 weeks |
| Async I/O | io_uring/ring abstraction with portable fallback | `ley-line-open-cb3453` | 1–2 weeks |
| VM backend | embedded libkrun, no `krunvm` subprocess | `ley-line-open-f839c1` | 2–4 weeks |
| Graph transport | capability-scoped UDS/virtiofs/ring access | `ley-line-open-f861c7` | 1–2 weeks |
| Product surfaces | Rust, daemon, CLI, and MCP projections | `ley-line-open-f8a079` | 1–2 weeks |
| Cloister adoption | policy translation and first-party CLI | `cloister-f94486`, `cloister-f980b1` | 1–2 weeks |
| Auth/audit validation | Claude Code Max/paid flows and receipts | `cloister-f9ceb3` | 3–5 days |
| Hardening | adversarial conformance and release matrix | `ley-line-open-f8ebcf` | 1–2 weeks |

These slices overlap. A useful native vertical slice is roughly 3–5 weeks; a
hardened VM path is roughly 6–10 weeks for one experienced engineer, excluding
upstream platform defects and CI queue time.

## Acceptance and falsifiability

The boundary is only complete when fixtures demonstrate that:

1. the same request and receipt round-trip through Rust, UDS, CLI, and MCP;
2. `status` is side-effect free on an unprovisioned host;
3. an unsigned, expired, replayed, or over-broad grant fails before materializing
   a workspace;
4. mismatched identity and confinement digests fail closed;
5. a guest cannot address raw arena, SQLite, host credential, or undeclared host
   paths;
6. Graph writes publish a new root through LLO and conflict on a stale expected
   generation;
7. workerd behaves the same as another declared guest workload and receives no
   ambient host privilege; and
8. receipts contain enough evidence to reproduce authorization and artifact
   lineage without containing secrets.

## Non-goals

- Replacing Signet or Interlace identity formats.
- Moving Cloister product policy or Claude authentication into LLO.
- Making Rosary a prerequisite for execution.
- Claiming protection from a malicious host root or hypervisor.
- Making V8/workerd the universal runtime.
- Treating a content-addressed filesystem as confinement without an enforcing
  process or VM boundary.
