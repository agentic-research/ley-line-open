# ADR-0035 — Confinement is one manifest; the enforcement mechanism is attested, not assumed

**Status:** Proposed (2026-08-03)
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
what it conveys and never widen it. That needs three things: the grant unforgeable (a
subject binding, and ultimately a signature — see the execution/v2 question), the chain
verified through `actorProvenanceEvidence`, and a resolver that refuses widening — which
is already structural via catalog equality. Until the first two land, composition is
unsafe regardless of what the enforcement layers do.

**6. Multi-tenancy is Cloister's, and this ADR says so out loud.** One LLO daemon is one
trust domain, with one catalog. Cloister runs a daemon per domain. A tenant dimension
inside LLO would duplicate the policy layer the execution goal document explicitly assigns
to Cloister. This is recorded as a *decision* because today it is merely an *absence*, and
from a consumer's side those look identical — a consumer cannot tell "deliberately yours"
from "nobody built it."

**7. Bind the evidence before building layer 3.** Per-workspace isolation inside the guest
can only enforce a separation that authorization actually decided. Layer 3 over a
forgeable authority would be isolation at the wrong altitude: precise enforcement of an
untrustworthy decision. Order is evidence binding → layer 3.

**8. confinement/v1 gains a JSON Schema — JSON-Schema-first, not capnp.** The
`confinementDigest` is defined over canonical JSON (§6: UTF-8, ASCII-sorted keys,
two-space indent, no trailing newline). A capnp IDL would be a *projection* beside a
JSON-defined digest — two sources of truth for one signed surface. That is the same defect
shape as PR #312 finding 10, where `run_id` hashed capnp framing instead of content.

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

## Open questions

- **Does libkrunfw's kernel enable Landlock?** Layer 3 requires `CONFIG_SECURITY_LANDLOCK`
  and Landlock present in the guest's LSM list. libkrunfw is a minimal kernel; this must be
  verified before layer 3 is committed to, and if absent, the fallback is seccomp inside
  the guest or a libkrunfw configuration change.
- **Does confinement/v1's manifest shape reconcile with nono's `CapabilityManifest`?**
  If they are close, §1's `TryFrom` is direct; if they diverge, LLO owns a mapping and
  should document why rather than silently maintaining two shapes.
