# ADR-0036 — What a `confinementDigest` covers: host resources the signer cannot know, and channels the platforms do not share

**Status:** Proposed (2026-08-04) — revised after adversarial review, see *Review history*
**Bead:** `ley-line-open-17536d`
**Related:**
- ADR-0035 (confinement is one manifest; the enforcement mechanism is attested) — this ADR
  resolves the tension its §1 creates once a grant actually carries a document
- `cloister/confinement/v1` §1, §4, §6, §9 condition 6 (refuse what you cannot express)
- PR #319 (the listener dimension), PR #322 (§6 `unixSocket.allow`), PR #324 (§9 condition 6)
- `docs/superpowers/specs/2026-07-31-execution-v1-design.md` — the threat model that places
  the LLO daemon inside the TCB, which bounds what this ADR can honestly claim
- `ley-line-open-5de852` (release train — cloister raised the platform-split half)

---

## Context

ADR-0035 §1 states the invariant this ADR exists to keep honest:

> the applied `CapabilitySet` and the declared `confinementDigest` must be projections
> of one object.

Today that holds only because a grant-carried confinement document cannot reach
successful enforcement unless it digests **identically** to the document LLO builds
internally. Two independent facts break the invariant the moment a grant carries
anything else, and they are the same problem from two sides.

### 1. The last hop is missing, and it cannot be closed by plumbing

The ingest route exists and is enforced end to end:

- `ConfinementManifest::parse` ingests a confinement/v1 document (`runtime/src/confinement.rs:539`).
- `authorization.rs:536` calls it on `RunGrant.confinementManifest` and refuses a grant
  whose carried document does not digest to the `confinementDigest` it names (`:543`).
- `backend.rs:314` compares the digest the worker attests against the one the grant
  authorized, and stops the run on drift.

What is missing is one hop: `AuthorizedRun.confinement_manifest` (`authorization.rs:379`,
set at `:615`) is never read again — only the digest reaches `ExecutionRequest`
(`catalog.rs:165`). `worker.rs:174` and `native.rs:125` both call
`confinement_manifest(runtime_files, devices)` and compile LLO's own document.

The consequence is fail-closed and non-functional: a grant declaring §4 or §6 is parsed,
digest-verified, and then not the policy applied — and the drift check refuses the run.
Nothing is attested that did not take effect. It simply cannot succeed.

Reading the hop as plumbing is the trap. The applied policy must ALSO carry host
resources that a **pre-host** issuer cannot name:

| Resource | Why a pre-host signer cannot name it |
| --- | --- |
| run rootfs | a per-run temporary directory; the path does not exist when the grant is signed |
| libkrun runtime files | host- and tier-specific — a native worker needs a different set |
| device nodes | `/dev/kvm`, `/dev/net/tun`, platform-dependent |

**This is a workflow constraint, not a law of nature, and the ADR states it as a
requirement rather than smuggling it in as an assumption.** A host-aware issuer that
signs *after* scheduling and materialization knows all three. LLO requires
pre-host issuance because a grant is authorized before a host is selected, and the
alternative is evaluated explicitly under *Alternatives considered*.

Merge those resources into the authorized document and it digests differently from the
one signed, so the equality check rejects every run. Leave them out and the worker loses
the grants it needs to function.

`ATTESTED_RUN_ROOTFS` (`backends/libkrun/confinement.rs:66`) already solves exactly one
cell, and its reasoning is the seed of this ADR: a symbolic name in the attested bytes,
substituted for the real path at compile time, because "a digest nobody can predict is a
digest no issuer can commit to."

### 2. The channel dimensions do not agree across platforms

- §4 `port.bind` is enforceable on Linux (Landlock filters TCP `bind(2)` per port) and
  **not** on macOS (Seatbelt's bind/inbound direction is all-or-nothing).
- §6 `unixSocket.allow` is enforceable on macOS (Seatbelt filters a UNIX socket per path)
  and **not** on Linux at the ABI this stack targets.

§9 condition 6 requires a runner that cannot express a declaration to refuse it, so a
capability needing a local channel must declare §4 where Landlock enforces and §6 where
Seatbelt does — different bytes, different digests.

Critically, **enforceability is not a property of the OS.** It depends on the kernel's
Landlock ABI, the `landlock` crate version, and the ABI `nono` targets
(`confinement/v1/README.md:128`). During an ABI-5 → ABI-9 transition two *Linux* profiles
must coexist and differ. Any selector keyed on "platform" cannot distinguish them.

---

## Decision

**A `confinementDigest` covers the authorized policy exactly. The applied policy is a
distinct object, computed and digested by the trusted parent, and the relationship
between them is verified where both objects are in hand — never across a digest.**

### D1. The signed document is the workload policy, with tagged symbolic resources

A grant's confinement document names only what a pre-host issuer can know: the workload's
own dimensions, with host resources referred to by **tagged symbolic references** drawn
from a vocabulary the spec closes.

Tagged, not path-shaped. `ATTESTED_RUN_ROOTFS` is a magic string (`/run/rootfs/`) that
occupies the same namespace as a literal filesystem grant
(`backends/libkrun/confinement.rs:66`, substituted by equality at `:137`). Promoting it
to a vocabulary member requires a distinct schema variant — e.g.
`{"symbolic": "runRootfs"}` — so that no literal path can ever collide with a symbol and
no symbol can be mistaken for a path. A symbolic name the spec does not define MUST be
refused (§9 condition 6 applied to itself).

### D2. The trusted parent computes the applied document; the worker boundary keeps equality

The daemon — already inside the TCB per the execution/v1 threat model — performs:

```text
applied  = expand(signed, host_profile)
expected = digest(canonical(applied))
```

It then passes **the applied document itself** to the worker, and keeps today's equality
check: the worker compiles exactly that document and attests its digest, which must equal
`expected`.

This is the correction that the first draft of this ADR got wrong. It proposed that the
drift check verify a bounded extension — but `WorkerEvent::Ready` carries only `run_id`
and a digest string (`worker.rs:34`), and set containment is not computable from two
hashes. Containment must be checked in the daemon, where `signed` and `applied` are both
live objects; the worker boundary continues to do the one thing a digest comparison can
soundly do, which is detect drift.

The containment check itself is defined over a **normalized authority IR**, not over the
JSON model. `ConfinementManifest` is syntactic: paths are raw strings checked only for a
leading `/` and `..` (`confinement.rs:257`), a trailing slash silently switches
`allow_file` to `allow_path` (`backends/libkrun/confinement.rs:98`), and an absent §4
address means loopback rather than "unspecified" (`confinement.rs:195`). A set difference
over grants is therefore unsound. Each manifest lowers to atomic effects
`(dimension, canonical selector, operation)` with:

- `None` address normalized to `127.0.0.1` — never treated as a wildcard
- leaf vs subtree distinguished **structurally**, not by string suffix
- modes split into operation bits (`read`/`write`; `connect`/`bind`)
- duplicate and overlapping selectors resolved before comparison

and the requirement is:

```text
Rights(signed) ⊆ Rights(applied)  ⊆  Rights(signed) ∪ Rights(expansion)
```

Worked negatives the implementation MUST reject, each of which a naive comparison accepts:

| signed | applied | why it is a widening |
| --- | --- | --- |
| `/srv/data/allowed.json` (ro) | `/srv/data/` (ro) | subtree covers every sibling |
| `/run/llo/shims` | `/run/llo/shims/` | leaf becomes directory tree |
| `/data/` (ro) | `/data/` (rw) | write added on the same path |
| `/run/proxy.sock` (`connect`) | same path, `connect-bind` | `bind(2)` added |
| `{"bind":8443}` | `{"bind":8443,"address":"0.0.0.0"}` | loopback becomes all-address |

### D3. What the vocabulary bounds — stated honestly

The closed vocabulary bounds the **shape** of what a correct daemon may add. It does not
bound authority against a compromised one: a runner that resolves `runtimeFiles` to
`/etc/shadow` still reports "this came from a known symbol."

That is not a regression this ADR introduces. The execution/v1 threat model already places
the LLO daemon in the TCB, and today the same worker both applies the policy and authors
the digest it reports — a malicious worker could already apply one thing and attest
another. What D2 costs relative to strict equality is narrower and must be named: **weaker
drift detection, and expansion that the daemon approves for itself.** The vocabulary and
the IR check are audit and correctness properties, not containment of a hostile TCB.

If that trade is ever unacceptable, the escape hatch is a host countersignature — an
independent host authority signs the exact expansion, binding it to the issuer digest and
run ID — not a larger vocabulary.

### D4. Profile-scoped candidates, on a new field

A grant carries a set of candidates, each binding a **selector** to a manifest and digest:

```text
selector = { hostOs, backendClass, enforcementProfileId, minimumKernelAbi, compilerConfig }
```

Selection is deterministic against independently attested runtime evidence; a runner with
no matching candidate MUST refuse the run rather than pick the nearest. Because §4 and §6
policies are incomparable, there is no "strongest" to fall back to and no total order to
select on — matching is exact or it is a refusal.

`enforcementProfileId` and `minimumKernelAbi` are what make the ABI-9 transition
expressible: two Linux candidates that differ only by ABI are distinguishable, which a
`platform` key could never be. The first draft claimed a `(platform, digest)` set absorbed
that transition; it does not.

This is a **new additive field**. `RunGrant.confinementDigest @8` is singular
(`execution/v1/execution.capnp:127`) and reinterpreting a shipped v1 field is a v2 break
(`execution/v1/README.md:74`). The selected profile and the applied document reference are
recorded in the terminal receipt, not only in `Ready`.

### D5. The applied object is a frozen resolution record

§6's ordering contract makes `capabilities_from_manifest` read filesystem state
(`Path::exists`, `backends/libkrun/confinement.rs:374`), and the spec says compilation
depends on "the declaration and the moment" (`confinement/v1/README.md:336`). Under D2 the
applied document is digested, so that state becomes load-bearing: the same signed document
could otherwise produce different applied digests as sockets appear or symlinks change.
The first draft deferred this; D2 makes it undeferrable.

The applied object therefore freezes resolution: declared path, canonical target, file
identity and type, resolver generation, enforcement profile, and expansion provenance.
That record is what gets digested and retained, so an offline auditor can answer *which*
endpoint existed and *why* the expansion was legal. An `exists()` check preceding apply is
not a race-free proof and must not be treated as one.

---

## Consequences

- §4 and §6 become deliverable. §4 lowers to the guest `port_map` (`worker.rs:224`); §6 at
  the microVM tier is what would give `add_vsock_port` its first caller. These are
  **different mechanisms** and the first draft conflated them — the vsock mapping (path,
  port, direction, mode, receipt evidence) is its own decision, not a consequence of this one.
- ADR-0035 §1 survives, sharpened: the applied `CapabilitySet` and the *applied* digest are
  projections of one object; the authorized digest projects a different object, and the two
  are related by a checked containment computed where both are live.
- The receipt grows: applied digest, selected profile, expansion evidence. Consumers reading
  `confinementDigest` as "what was enforced" become wrong — a breaking read for cloister,
  and the reason this is an ADR.
- §9 needs a further condition: a runner refuses symbolic names outside the vocabulary.

## Alternatives considered

**Per-host signing of the fully expanded document.** The honest stronger design, and it is
rejected only by the pre-host issuance requirement stated in Context §1 — not because
expansion is unknowable. If that requirement is ever relaxed, this becomes the preferred
answer and D1–D3 can be retired. A short-lived host countersignature is the middle option.

**Keep digest equality; grant host resources outside the manifest.** Rejected: the applied
policy is then strictly wider than any attested document and nothing describes the gap.

**Merge host resources into the signed document and re-digest.** Rejected: unpredictable at
signing time, so no issuer can commit — `ATTESTED_RUN_ROOTFS`'s argument, generalized.

**One platform-invariant digest with the channel dimension omitted.** Rejected: makes every
capability needing a local channel undeclarable.

## Open questions

- **Per-tier scoping of the vocabulary.** A native and a microVM worker need different
  runtime files for the same symbol. Scoping in the spec multiplies documents; expanding at
  the runner widens what the IR check must verify. D4's `enforcementProfileId` is the
  candidate mechanism.
- **Path identity under symlinks.** The IR requires canonical selectors; deciding identity
  before apply, race-free, is not solved by `canonicalize()` alone.

## Review history

- **2026-08-04, first draft.** Proposed a bounded-extension check at the worker drift
  boundary, a `(platform, digest)` candidate set, and deferred §6 purity.
- **2026-08-04, adversarial review (codex, read-only).** Four fatal findings, all upheld:
  "appears unweakened" had no sound decision procedure over the syntactic model; the
  containment check was not computable from the digest the boundary carries; the closed
  vocabulary bounded names rather than authority; and the `platform` selector could not
  express two Linux ABI profiles. Also corrected two overstated claims — that no grant can
  carry a document (one digesting identically to LLO's own is accepted,
  `authorization.rs:527`), and that this work gives `add_vsock_port` its first caller (§4
  lowers to `port_map`). Revised above; D2, D3, D4 and D5 are rewrites, not amendments.
