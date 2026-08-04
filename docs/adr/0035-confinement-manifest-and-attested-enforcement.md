# ADR-0035 — Confinement is one manifest; the enforcement mechanism is attested, not assumed

**Status:** Accepted (2026-08-03)
**Bead:** `ley-line-open-da36d2`
**Related:**
- PR #312 review finding 2 (`confinementDigest` is on the wire and never enforced)
- ADR-0019 (host helper binaries — the precedent for depending on an external binary, and why we are not doing that here)
- `ley-line-open-f8ebcf` (execution/v1 isolation + transport parity — the conformance matrix this feeds)
- `ley-line-open-704853` (porting cloister's kernel-confinement assertions into LLO)
- `ley-line-open-d554a0`, `ley-line-open-60f0d3` (same defect class: hand-mirrored types because a spec ships no machine-readable shape)
- `cloister-043eb8` (multi-root — resolved by §4 below)
- OCI Runtime Specification, `config-linux.md` §`linux.resources`

---

## Context

`RunGrant.confinementDigest @8` and `RunReceipt.confinementDigest @6` are on the wire.
The schema documents the latter as *"Policy digest actually enforced."* Neither claim is
true today: `AuthorizationPolicy.required_confinement_digest` defaults to `None`, the
first-party daemon never sets it, and the policy the backend actually compiles
(`build_process_capabilities`) has no relationship to the digest the receipt attests. The
digest and the enforcement are two unrelated objects that share a name.

Fixing that turned out to require settling three things first.

### 1. nono does not enforce resource limits

nono is a *confinement* primitive: Landlock on Linux, Seatbelt on macOS, applied
irreversibly to the **current process**. It does not spawn, supervise, or reap.

`nono::resource::ResourceLimits` looks like it contradicts that — its doc comment names
`cgroup memory.max`, `memory.swap.max=0`, `memory.oom.group=1`, `pids.max`. It does not.
Nothing in the library's `src/` reads the field: it appears only in the struct, in
`CapabilitySet::with_resource_limits` / its getter, and in `state.rs` serialization
round-trips. `CapabilityManifest::validate()` states the actual contract — `resources`
require `exec_strategy: "supervised"`, i.e. nono's **CLI** supervisor, which is a
separate binary we do not depend on and (per ADR-0019's bar) should not acquire for this.

**So resource enforcement is ours to build.** Calling `with_resource_limits` without
building it would encode a ceiling nothing applies — reproducing finding 2 inside the fix.

### 2. cgroups are declarative, and the declaration is standardized

The kernel interface is `write()` into cgroupfs, but the *policy* has a widely adopted
schema: OCI `linux.resources` — `memory`, `cpu`, `pids`, `devices`, `blockIO`,
`hugepageLimits`, `network`, `rdma`, and `unified`, a raw cgroup-v2 key→value passthrough
("each key references a cgroup v2 hierarchy file"). The same config document carries
`seccomp`, `namespaces`, `maskedPaths`, `readonlyPaths` and `mountLabel`, which is why
Kubernetes → OCI → cgroups/SELinux/seccomp is a mapping chain rather than three unrelated
systems.

`unified` matters specifically: `cgroup.kill` and `memory.oom.group` are not first-class
OCI fields, and `unified` is the sanctioned way to declare them. `cgroup.kill` is atomic
process-tree teardown with no pids involved — it retires the pid-reuse hazard class on
Linux outright, rather than requiring us to hand-roll kill-before-reap correctly.

### 3. The tiers enforce differently, and one cell is a real semantic gap

| dimension | microVM (mac + linux) | native Linux | native macOS |
| --- | --- | --- | --- |
| memory | **hypervisor ceiling, ships today** (`krun_set_vm_config`) | cgroup `memory.max` — not engineered | `RLIMIT_AS` — not engineered; address space ≠ RSS |
| vCPU | **hypervisor, ships today** | cgroup `cpu.max` — not engineered | `RLIMIT_CPU` — not engineered; CPU-seconds ≠ parallelism |
| per-tree pids | guest is Linux → cgroup `pids.max` | cgroup `pids.max` — not engineered | **no mechanism.** `RLIMIT_NPROC` is per-**uid**, and cannot express "this run's tree" |
| tree teardown | kill the VMM; everything dies | `cgroup.kill` — not engineered | process group + kqueue `EVFILT_PROC`/`NOTE_EXIT` (the macOS pidfd analog: fd-bound, immune to pid reuse) |

Almost every cell is *unbuilt*, not *impossible*. Exactly one — per-tree pid capping on
the macOS native host — has no mechanism with the right semantics.

### 4. The tiers nest, and that resolves multi-root

Confinement is not one boundary but three, and they compose:

1. **outer nono** — the VMM process itself, confined on the host before the VM starts.
   Ships today.
2. **microVM** — the hypervisor boundary. One virtiofs mount is the guest's entire host
   surface.
3. **inner nono, per guest process** — the guest runs a Linux kernel, so Landlock is
   available *inside* the VM. **Not built.** Today a guest process is unconfined relative
   to the guest's own filesystem.

This settles `cloister-043eb8`. Cloister assembles N directories; the question was whether
LLO grants N writable host roots. It does not, and does not need to: **N workspaces
materialize into one host tree at known mount points, and layer 3 grants per-workspace
access inside the guest.** The host boundary stays a single content-addressed tree that
can be digested and attested; the "A and B writable, C denied" semantics move inside,
where they are per-process rather than per-run — which is strictly more expressive than N
host roots, since different guest processes can hold different workspace subsets.

`WorkspaceGrant.operations` (`read`/`list`/`query`/`mutate`/`commit`) is already the right
shape to drive layer 3's `AccessMode` per workspace.

### 5. Composition: the scenario the three evidence fields were built for

Cloister instances compose — one Cloister's board API consumed by a UI that is itself
driven by another Cloister. Once that happens, a workspace designated for repo A must
provably not be reachable from a tool designated for repo B, and the guarantee has to
survive *delegation*, not just direct calls.

Some of this already holds. Workspace identity is **content-addressed**:
`WorkspaceIntent { name, graphRoot }` — the `graphRoot` digest is the identity, `name` is
only the local handle the workload uses to address it, and `validate_workspaces` rejects
duplicate names within a run. Two workspaces called `board` from different origins are
different objects. And `CatalogResolver::resolve` requires
`entry.workspace_inputs == authorized.intent.workspace_inputs` exactly, so a caller cannot
attach a workspace to an artifact the catalog did not register it for.

What does not hold is **who may present a grant**. Nothing binds a grant to a subject, and
the grant is not signed at all, so a grant relayed by a second Cloister is
indistinguishable from a first-party one. With a single Cloister the owner-only socket
stood in for identity; under composition it stops meaning anything. Post-start operations
compound it: `cancel`/`collect`/`inspect` take a bare `run_id`, so holding a run id is
holding the run.

The schema anticipated exactly this and is currently inert:

| field | question | today |
| --- | --- | --- |
| `issuerEvidence @1` | who authorized this? | verified as *some* trusted envelope |
| `workloadIdentityEvidence @5` | which workload is this? | same check, byte-identical |
| `actorProvenanceEvidence @6` | **who delegated to whom?** | same check, byte-identical |

Three fields for three distinct questions; `actorProvenanceEvidence` is documented as
*"delegated actor/provenance evidence, such as a Signet bridge certificate"* — it is the
delegation chain. All three are satisfied today by any one trusted APAS envelope, bound to
nothing.

### 6. confinement/v1 ships no machine-readable shape

`schema-spec/confinement/v1/` is `README.md`, `test-vectors/`, `VECTORS.sha256` and
`CONFINEMENT_DIGESTS.blake3`. No IDL. Consumers hand-mirror the manifest shape to compute
a digest — the same defect `ley-line-open-d554a0` reports for execution/v1 and
`ley-line-open-60f0d3` already fixed for MCP protocol facts.

---

## Decision

**1. The confinement manifest is the single declaration.** One document derives all three
of: the `nono::CapabilitySet` that gets applied, the BLAKE3 `confinementDigest`
(canonicalized per confinement/v1 §6), and the digest a backend declares in
`BackendCapabilities`. Because they are projections of one object, they cannot drift —
which is the property finding 2 needs. nono already supports the direction we need:
`TryFrom<&CapabilityManifest> for CapabilitySet`.

**2. The `resources` block conforms to OCI `linux.resources`.** We do not invent a
resource shape. Dimensions OCI does not cover — `fs.allow` as Landlock/Seatbelt path
grants rather than mounts, `network.allowHosts`, `credentialSource` — stay
confinement/v1-native, because inventing there is legitimate and inventing over OCI is
not.

**3. Declare the mechanism and the layer, not just the ceiling.** `memory: 512MiB` means
three different things under a hypervisor, a cgroup, and `RLIMIT_AS`. Since
`confinementDigest` attests the policy, the receipt must record what actually applied.
`RunReceipt.backend` (`BackendEvidence { backendClass, backendId, evidence }`) is the
existing home for it.

**4. A ceiling the selected tier cannot enforce is a rejection, not a no-op.** If a grant
requests per-tree pid capping and the selected backend is native-macOS, authorization
**fails closed**. Silently accepting it is exactly the defect this ADR exists to close,
one level up.

**5. Delegation attenuates only, and the chain is checked.** A delegated grant may narrow
what it conveys and never widen it. That needs three things: the grant unforgeable, the
chain verified through `actorProvenanceEvidence`, and a resolver that refuses widening —
which is already structural via catalog equality.

*Amended 2026-08-03 (PR #312).* The first requirement is met. This decision was drafted
expecting the grant signature to cost an execution/v2, so it deferred to "a subject
binding, and ultimately a signature — see the execution/v2 question". Both landed in v1
instead: evidence now binds to a derived run identity, and `RunGrant.signature @14` covers
`PAE(run-grant payload type, canonical(grant with signature cleared))`. execution/v1 had
never shipped — 0 files on `origin/main`, 0 in the v0.14.0 tag — so the amendment was free
where a released v1 would have forced a new `<v>` directory. There is no execution/v2
question outstanding for delegation; what remains is chain verification through
`actorProvenanceEvidence`.

**6. Multi-tenancy is Cloister's, and this ADR says so out loud.** One LLO daemon is one
trust domain, with one catalog. Cloister runs a daemon per domain. A tenant dimension
inside LLO would duplicate the policy layer the execution goal document explicitly assigns
to Cloister. This is recorded as a *decision* because today it is merely an *absence*, and
from a consumer's side those look identical — a consumer cannot tell "deliberately yours"
from "nobody built it."

*External corroboration (2026-08-03).* Memoria — an agent-memory layer with
snapshot/branch/merge over MatrixOne — shipped the other shape first and migrated out of
it. Their architecture note states the reason directly: Git-for-Data semantics do not hold
in a shared database ("Git-for-Data 语义在共享库里不成立"). Rollback becomes inherently
global, snapshots capture other users' state, and branch/diff filtered in the application
layer is "a patch, not isolation". They moved to one database per user, and describe the
benefit as scoping snapshot/branch/restore/rollback rather than making queries faster. The
rule generalizes past databases: **the version-control boundary and the isolation boundary
must be the same boundary.** Their cost list is the forewarning for us — per-boundary
isolation multiplied object counts and broke naive global aggregation badly enough that
`/metrics` had to become a shared-DB summary with async refresh. If Cloister runs a daemon
per trust domain, that observability bill is ours too. See
`ley-line/docs/prior-art/memoria.md`.

*Scope correction (2026-08-03).* Read plainly, "one daemon per trust domain" bounds the
blast radius of a compromise to one domain — and that is true. It must not be read as
defence in depth, because **the domain boundary is currently the only boundary.** Cloister
confirmed it from their side: LLO appears in `cluster.toml` as `[inputs.llo]` with a
`serviceBinding` and **no `[[bundles]]` entry**, so it has no `executionMode`, no
confinement facet, and their lint Inv 13 cannot see it — it runs as a host process with
zero confinement applied. Their calibration is worth carrying: only one of five declared
bundles is `microvm`, so what cloister has is *declaration discipline* (ADR-0062 makes
`process` an exemption from isolation rather than a peer of it) rather than broad
isolation. LLO's gap is being absent from that ledger rather than permissive within it.
Until §9 lands, "one domain" is the whole of the containment story.

**7. Bind the evidence before building layer 3.** Per-workspace isolation inside the guest
can only enforce a separation that authorization actually decided. Layer 3 over a
forgeable authority would be isolation at the wrong altitude: precise enforcement of an
untrustworthy decision. Order is evidence binding → layer 3.

*Amended 2026-08-03 (PR #312).* This precondition is satisfied. Evidence binding shipped:
a statement authorizes a given `RunGrant` field only if it carries an in-toto subject
named for that field whose `digest["blake3"]` is the derived run identity, so one trusted
envelope can no longer satisfy all three evidence references. Layer 3 is unblocked.

**8. confinement/v1 gains a JSON Schema — JSON-Schema-first, not capnp.** The
`confinementDigest` is defined over canonical JSON (§6: UTF-8, ASCII-sorted keys,
two-space indent, no trailing newline). A capnp IDL would be a *projection* beside a
JSON-defined digest — two sources of truth for one signed surface. That is the same defect
shape as PR #312 finding 10, where `run_id` hashed capnp framing instead of content.

**9. The daemon is in the TCB by declaration, and that declaration is getting more
expensive.** The execution design doc's threat model item 3 states it plainly: neither
`libkrun` nor `nono` protects secrets from host root, a compromised hypervisor, or *a
compromised LLO daemon*. That is a legitimate boundary — nothing defends against its own
TCB — and the code matches: nono is applied in `backends/native.rs` and
`backends/libkrun/confinement.rs` only, never to the daemon process.

What has changed is the cost. The daemon owns the CAS root, is wire-reachable through
`llo_execution_start` over a UDS, is the component that *applies* confinement (so a
compromised one does not escape a sandbox — it decides there is not one), and once
`ley-line-open-410921` lands it holds the trust root for the whole domain. "Out of scope"
cost almost nothing when it held no keys.

So the manifest of §1 applies to the daemon too: one manifest type, two consumers — the
worker's policy and the daemon's own — both digest-pinned from one declaration. Its
capability set is bounded and knowable: CAS root, arena, UDS, spawn. Self-confinement
cannot stop a compromised daemon misusing what it legitimately holds, but it bounds
lateral movement, so a parsing bug on the wire cannot reach `~/.ssh` or open arbitrary
sockets.

**10. The trust root is public key material, and the scheme that loads it is a
confinement decision.** Verification keys need integrity and authenticity, not
confidentiality. A vault buys secrecy we do not need and supplies integrity only because
the vault is trusted anyway. Cloister reached the same conclusion independently:
`INTERLACE_ROOT_PUBKEY` is an env var, deliberately not vaulted, resolved through one
fail-closed resolver (their ADR-0053).

**LLO consumes notme's `/.well-known/jwks.json`, not `/.well-known/ca-bundle.pem`.** notme
publishes both from one Ed25519 authority; they are two projections of the same root.
`jwks.json` is an Ed25519 JWK Set — raw public keys that map directly to
`leyline_envelope::VerifyingKey`, carrying a `kid` in the form `sign/src/kid.rs` already
computes (ADR-012's `SHA-256(SPKI DER)[:16]`). `ca-bundle.pem` is X.509, and consuming it
would require chain validation and the x509-cert/sigstore/aws-lc-rs closure that cloister's
own 2026-05-13 cycle (row 17.1) removed from the signing helper. The correct artifact is
the one whose shape our verifier already speaks.

The scheme then determines the daemon's capability grant, which is why this is a
confinement decision rather than a configuration one:

| Source | Daemon capability required |
| --- | --- |
| `file:///…` | one read-only path |
| notme `jwks.json` over HTTPS | one outbound host, pinned |
| `keychain://` / `secret-tool://` | Security-framework or D-Bus access |

Widening from the first two to the third must appear *in the manifest*, digest-pinned,
rather than as an invisible consequence of a config string.

---

## Identity, authority, interface — and the trust root, which is none of them

Four things are routinely conflated because all four are "security". The first three are
the normative three-lane mapping from the execution design doc:

| Lane | Example | Meaning |
| --- | --- | --- |
| Signet grant | `urn:signet:cap:<action>:<resource>` | what the holder may do |
| Interlace/WIMSE identity | `wimse://…` | which workload is acting |
| Cloister interface | `cloister/<name>/v<n>` | which contract shape is requested |

The fourth is **the trust root**, and it is orthogonal to all three: *which keys do I
trust to sign assertions about any lane.* A `RunGrant` carries all four concerns at once —
`capabilities` is lane 1, `workloadIdentityEvidence` is lane 2, the `schemaVersion` and
capability `interface` are lane 3, and every signature over any of it verifies against the
trust root. Mixing them produces specific, recognisable mistakes: reaching for a *vault*
(a confidentiality tool) to hold a *public* trust root, or pulling an mTLS-shaped artifact
into a DSSE-shaped verifier because both are "the notme cert thing".

Repo ownership does **not** map one-lane-per-repo, which is the second thing routinely got
wrong:

| Repo | Owns |
| --- | --- |
| signet | The capability-grant vocabulary — `urn:signet:cap:<action>:<resource>` (lane 1). |
| cloister | **Interlace** — the workload-identity substrate (`interlace-spec/`, their ADR-0007) — *and* policy resolution, the interface vocabulary (lane 3), and the `EvidenceVerifier` LLO is handed. Interlace is cloister's, not notme's; `INTERLACE_ROOT_PUBKEY` is a cloister deployment variable. |
| notme | The deployed identity **authority** — one Ed25519 root issuing X.509 bridge certs and DPoP tokens, publishing `/.well-known/{signet-authority.json,jwks.json,ca-bundle.pem}`. The thing that *signs*, distinct from the lane whose assertions it signs. |
| LLO | None of them. It verifies what it is given, which is why `EvidenceVerifier` is a trait and not an implementation. |

So cloister owns two of the three lanes and notme owns the trust root, while lane 2's
*spec* and lane 2's *authority* live in different repos. That asymmetry is the reason
"Interlace/notme" reads as one thing and is not.

---

## Consequences

**Closes by construction.** Finding 2 stops being a wiring gap: there is no second object
to drift from. Cloister can generate types instead of hand-mirroring (§5). Conforming to
OCI means a future OCI-runtime backend maps by field copy rather than an adapter. And
`unified` → `cgroup.kill` deletes hand-rolled process-group teardown on Linux instead of
asking us to get it right.

**Costs, stated rather than hidden.** Backends stop being freely substitutable: a grant
naming a dimension only some tiers enforce restricts which backends can serve it, and
`BackendCapabilities` must advertise that so a caller can select before being rejected.
This is a real contract consequence, and it is the honest one — the alternative is
pretending the tiers are equivalent.

**Native-macOS is the weakest tier by construction**, and after §4 it will *say so* rather
than silently accept. Given microVM runs on macOS via Hypervisor.framework, native should
be positioned as the development/fallback tier, not the default for production grants.

**Work implied:** a cgroup v2 writer (Linux), rlimits + kqueue `NOTE_EXIT` (macOS), inner
nono inside the guest (layer 3), manifest plumbing through
`build_process_capabilities`, the JSON Schema, and test vectors pinned in
`CONFINEMENT_DIGESTS.blake3` so cloister can conform without an LLO checkout.

---

## Alternatives considered

**Bridge cgroup ↔ nono ↔ cloister through schema-bridge.** Rejected on two grounds.
schema-bridge is capnp-in (a `capnpc` plugin family reading `CodeGeneratorRequest` from
stdin), so it cannot consume OCI's or nono's JSON Schemas. More fundamentally,
*conformance eliminates the mapping a bridge would institutionalize*: speaking OCI's shape
means there is nothing to translate.

**Fork nono's `capability-manifest.schema.json`.** Rejected. nono owns that schema and
already ships typify-generated Rust. Forking it means owning drift on someone else's
contract — the failure this repo has now diagnosed three times.

**Depend on the `nono` CLI for `exec_strategy: "supervised"`.** Rejected. It would acquire
a host binary dependency of exactly the class the execution goal document forbids, for
functionality (cgroup writes) that is a few hundred lines to own directly.

**Keep hand-rolled process-group supervision.** Rejected. It carries a pid-reuse hazard
that `cgroup.kill` (Linux) and kqueue `NOTE_EXIT` (macOS) remove by construction, rather
than requiring every future call site to re-derive kill-before-reap.

---

## The daemon cannot declare what the worker compiles — the worker must attest it

*Added 2026-08-03, from implementing §1.*

§1 is now real on the compile side: `confinement_manifest()` builds the
`confinement/v1` manifest and `capabilities_from_manifest()` derives the
`nono::CapabilitySet` from it, so the applied policy and the declared digest
are projections of one object and a policy change that skipped the manifest
would not compile.

Closing finding 2 end to end needs one more link — comparing that digest to
the grant's `confinementDigest` — and the obvious placement does not work.
**The policy is compiled in the worker, after fork.** `apply()` runs
worker-side, and the rootfs path comes from `DirectoryRootfsResolver::resolve`,
which canonicalizes a materialized tree. The daemon therefore cannot compute
the digest at `start` time without re-deriving the path itself, which would
mean two implementations of the same derivation — reintroducing precisely the
drift §1 exists to prevent, one layer up.

So the remaining shape is an **attestation, not a lookup**: the worker reports
the digest of the policy it compiled, and the daemon refuses to mark the run
`Running` unless it equals the grant's `confinementDigest`. The readiness
protocol is already that channel — `read_readiness_line` carries a
worker-authored message the supervisor parses, and every failure mode of it
(EOF, malformed, wrong run id) already routes to `abort_failed_start` and a
group kill.

That also matches this ADR's title. A digest the daemon computed about the
worker is an assumption; a digest the worker reports about itself, checked
against what the grant authorized, is attestation. The unenforceable-ceiling
rejection in §4 is the same shape: refuse rather than proceed on an unverified
claim.

Not implemented here. It changes the readiness message, which is a
worker/daemon contract, and belongs with §9's daemon-manifest work rather than
bolted onto the compile-side change.

## Open questions

- **Does libkrunfw's kernel enable Landlock?** Layer 3 requires `CONFIG_SECURITY_LANDLOCK`
  and Landlock present in the guest's LSM list. libkrunfw is a minimal kernel; this must be
  verified before layer 3 is committed to, and if absent, the fallback is seccomp inside
  the guest or a libkrunfw configuration change.

  *Resolved 2026-08-04, and the answer is stronger than the question anticipated:
  there is no LSM framework in the guest at all on this architecture.*

  Measured two ways, agreeing. From `containers/libkrunfw`'s
  `config-libkrunfw_aarch64`:

  ```
  # CONFIG_SECURITY is not set
  # CONFIG_SECURITYFS is not set
  CONFIG_SECCOMP=y
  CONFIG_SECCOMP_FILTER=y
  ```

  `CONFIG_SECURITY_LANDLOCK` and `CONFIG_LSM` do not appear. Against the shipped
  artifact — libkrunfw 5.5.0, guest kernel `Linux version 6.12.91` — the strings
  measurement matches exactly: `securityfs` 0 hits, `security_init` 0,
  `security/security.c` 0, `commoncap.c` 1. `commoncap.o` builds unconditionally;
  `security.c` is `CONFIG_SECURITY`-gated and is absent.

  Three consequences worth keeping:

  1. Guest-side confinement is **seccomp-bpf only** — capabilities, keys, seccomp,
     and nothing else. A future ABI-9 `nono` would not help inside the guest here.
  2. The `/sys/kernel/security/lsm` probe proposed above would have returned ENOENT
     and been misread as "Landlock absent" rather than "no LSM framework". The
     authoritative-looking check was the misleading one.
  3. 6.12 is exactly where Landlock ABI v6 landed, so this is a **config choice,
     not a version limitation**. A custom libkrunfw would be required to change it.

  For the libkrun tier the hypervisor is therefore the only boundary, and there is no
  silent second layer to credit: `Mechanism::Hypervisor` for constructible dimensions,
  `Unenforced` for the rest, with no third state.

- **Does confinement/v1's manifest shape reconcile with nono's `CapabilityManifest`?**
  If they are close, §1's `TryFrom` is direct; if they diverge, LLO owns a mapping and
  should document why rather than silently maintaining two shapes.

  *Resolved 2026-08-04 (bead `ley-line-open-c17486`). They diverge structurally.
  LLO owns the mapping, does not route through nono's manifest, and the reason is
  attestation correctness rather than convenience.*

  An earlier answer here said "close, with one deliberate divergence". That compared
  top-level key names and stopped. Read from
  `nono-0.71.0/schema/capability-manifest.schema.json`:

  | confinement/v1 | nono 0.71 | kind of difference |
  | --- | --- | --- |
  | `fs.allow` | `filesystem.grants` — `FsGrant{path, access, type}` | nesting + per-entry shape |
  | `network.allowHosts` | `network.allow_domains` | rename |
  | `port.bind` (top level) | `network.ports` — `PortConfig{bind, connect, localhost, localhost_range}` | re-parented and widened |
  | `credentialSource` (scalar) | `credentials` — **array** of `Credential{name, upstream, source, …}` | cardinality |
  | — | `dns`, `endpoints`, `mode`, `rollback`, `exec_strategy` | nono-only dimensions |
  | camelCase | snake_case | naming convention |

  The `credentials` row settles it: an array of objects against a scalar means no
  field-for-field rename exists even in principle.

  **Why LLO does not route through nono's manifest.** The alternative — map
  confinement/v1 into `CapabilityManifest`, then reuse nono's `validate()` and its
  `TryFrom<&CapabilityManifest> for CapabilitySet` — looks like free validation. It
  is not:

  1. *It buys no resource enforcement.* Routing through the manifest does not move LLO
     onto the supervised strategy, and off that strategy nono's own `validate()`
     **refuses** a manifest carrying resources at all (`src/manifest.rs:79-87`). LLO
     would have to emit `resources: null` and keep enforcing ceilings itself — which is
     what §3 and §4 already do.
  2. *It requires LLO to invent values it has no basis for.* `dns`, `endpoints`, `mode`,
     `rollback` and `exec_strategy` have no confinement/v1 counterpart. A total mapping
     means LLO making policy decisions in a vocabulary it does not own.
  3. *It breaks the property this ADR rests on.* The design is **one** manifest:
     capabilities derived from it, digest attested over it, so the attestation is true by
     construction rather than by discipline. Inserting nono's manifest between the
     digested object and the applied policy puts a second shape in exactly that gap —
     reintroducing, one layer down, the drift surface the single manifest removes.

  So LLO keeps `confinement_manifest()` → `capabilities_from_manifest()` →
  `CapabilitySet` (`backends/libkrun/confinement.rs`), compiling confinement/v1 directly
  via `allow_path`/`allow_file`. The table above is the mapping this open question asked
  LLO to own.

  **§3's `Supervisor` is not nono's supervisor** — a conflation worth stating outright,
  because the two mechanisms share a word and nothing else:

  | `CeilingMechanism` | what actually applies it | covers |
  | --- | --- | --- |
  | `Unenforced` | nothing | native tier's vcpus + memory |
  | `Supervisor` | **LLO's own backend**, observing a wall-clock deadline | wall time, both tiers |
  | `Hypervisor` | libkrun, at VM configuration time, before the guest runs | vcpus + memory, microVM tier |

  nono's cgroup-v2 enforcement appears nowhere in that table. It is a capability LLO has
  **available and unused**: taking it would mean adopting `exec_strategy: supervised`,
  which the `apply_auto` path does not use. Worth naming as a concrete future option
  rather than a present property — adopting it would move the native tier's `memory`
  ceiling from `Unenforced` to genuinely enforced, with `max_processes` alongside. That
  option is **Linux-only**, and not by omission: nono 0.71 has no macOS resource
  enforcement at all, so a native-tier memory ceiling on macOS stays `Unenforced` and
  only the hypervisor tier can carry one there.

  *Original answer, retained for the resources finding it got right:* nono 0.71's
  `CapabilityManifest` is generated from `schema/capability-manifest.schema.json` via
  typify, and that JSON Schema is stated as the source of truth — the same
  JSON-Schema-first shape §8 chose independently, so §8 is matching its dependency rather
  than inventing. Top-level keys are `credentials, filesystem, network, process,
  resources, rollback, version`, which line up with §2's `fs.allow`,
  `network.allowHosts` and `credentialSource`. `TryFrom<&CapabilityManifest> for
  CapabilitySet` exists at `src/manifest_convert.rs:21`, so §1's conversion is real code,
  not a hope.

  The divergence is `resources`, and it is intentional. nono's covers memory and max
  processes only, and they require the supervised strategy — confirming §1's finding that
  nono does not enforce resource limits on the library path LLO uses.

  *Sourcing corrected 2026-08-04 (`c17486`).* That claim was attributed to the schema's
  property descriptions, which in 0.71 say the opposite-sounding thing — `memory_bytes`
  is "Enforced via cgroup `memory.max`", `max_processes` via `pids.max`. The claim
  survives, from three places that are actually load-bearing:
  `src/manifest.rs:53` ("resources (memory_bytes / max_processes) require
  `exec_strategy: \"supervised\"`"), the validation enforcing it at `:79-87`, a test
  pinning it at `:185` ("memory_bytes without supervised must fail validation"), and
  `src/capability.rs:961` on `CapabilitySet.resource_limits` — "Plumbed through here so
  they ride the serialization layer like other policy; **enforced by the supervisor via
  cgroup v2 on Linux**." Both statements are true and not in tension: the ceilings *are*
  cgroup-enforced, but only on the supervised strategy, and LLO's
  `Sandbox::apply_auto(&CapabilitySet)` carries them while enforcing nothing.

  §2 conforms `resources` to OCI
  `linux.resources`, which is strictly wider. So the mapping is total in one direction
  only: every nono resource has an OCI counterpart, and OCI dimensions nono cannot enforce
  have none. That asymmetry is not a defect to paper over — it is the exact input to §4,
  which requires a ceiling the selected tier cannot enforce to be a **rejection**. The
  empty cells are now enumerable rather than hypothetical.
