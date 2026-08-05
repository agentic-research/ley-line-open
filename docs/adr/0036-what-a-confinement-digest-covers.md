# ADR-0036 — What a `confinementDigest` covers: the cases the equality contract does not close

**Status:** Proposed (2026-08-04) — narrowed twice; see *Review history*
**Bead:** `ley-line-open-17536d`
**Related:**
- ADR-0035 (confinement is one manifest; the enforcement mechanism is attested)
- PR #329 — the implementation that settled this ADR's original core case
- `cloister/confinement/v1` §1, §4, §6, §9 condition 6
- `docs/superpowers/specs/2026-07-31-execution-v1-design.md` (the daemon is in the TCB)
- `ley-line-open-5de852` (cloister raised the platform-split half)

---

## What is already decided — by implementation, not by this ADR

PR #329 settled the case this ADR was originally written for, and settled it
with **no new wire machinery**. Recording the shape here because this document
previously argued that machinery was necessary, and the argument was wrong:

- A grant-carried confinement document now reaches the worker
  (`ExecutionRequest.confinement_manifest` — carried, never chosen).
- The worker folds §4 into the document it compiles, and enforces an
  **equality contract**: after the fold, the carried document must equal the
  compiled policy or the run is refused at compile time with the differing
  dimensions named by section.
- The digest an issuer must predict is predictable, because every input to the
  compiled document is deployment configuration or the symbolic rootfs:
  `runtime_files` and `devices` come from `LibkrunBackendConfig`, fixed when
  the backend is constructed, and the rootfs is `ATTESTED_RUN_ROOTFS`.

The first draft of this ADR assumed those inputs were unknowable to a pre-host
issuer and designed a symbolic-resource vocabulary, a daemon-side containment
IR, and a profile-keyed candidate set around that assumption. The adversarial
review flagged the assumption (its Serious #6: "the ADR rejects per-host
signing by assuming signing must precede host selection"); the implementation
disproved it for every resource except the rootfs, which already had its
answer. ADR-0035 §1 holds as stated: one object, applied and attested.

## What remains open — the actual subject of this ADR

Three cases the equality contract does not close. Each is real; none blocks a
current consumer except the second, which cloister's macOS shim is waiting on.

### O1. An issuer that genuinely cannot know the deployment

The equality contract requires the issuer to reproduce the compiled document
exactly, which requires knowing the deployment's runtime files and devices.
That is true of every current issuer (cloister signs for deployments it
configures). An issuer signing for an *unknown* deployment — the federation
case — cannot.

If that case becomes real, the first draft's machinery becomes relevant again
(symbolic references for deployment resources, or a host countersignature over
the expansion). It is deliberately NOT built now: it adds a trust decision —
who vouches for the expansion — that nothing currently needs, and the reviewed
draft's mechanisms are preserved in git history at `640a056` for when
something does.

### O2. §6 delivery

A §6 grant is now refused by name at the fold, which is correct and
uninformative: the dimension is enforceable on macOS today. Two pieces, in
increasing order of design weight:

- **Native macOS:** the fold takes `unix_sockets` on the tier whose compiler
  already accepts them (`capabilities_from_manifest` compiles §6 via Seatbelt;
  the per-grant refusals in `unix_socket_mode` keep governing modes). The
  equality contract then admits a §6-carrying document instead of refusing it.
  Small, and unblocks cloister's shim.
- **microVM:** §6 `bind` maps to the vsock `listen=true` mechanism — the
  `add_vsock_port` caller that still does not exist. This needs decisions the
  fold does not: guest port allocation, the path↔port pairing's place in the
  attested document, and receipt evidence for the mapping.

### O3. The platform-split digest

Unchanged from cloister's original statement: a capability needing a local
channel declares §4 where Landlock enforces and §6 where Seatbelt does —
different documents, different digests — so a grant portable across platforms
must carry more than one commitment, and `RunGrant.confinementDigest @8` is
singular. Reinterpreting a shipped v1 field is a v2 break
(`execution/v1/README.md`), so this is an additive-field decision when it is
made. The first draft's selector design (profile-keyed, not platform-keyed —
two Linux ABI generations must be distinguishable) is the starting point, per
the review's finding 4.

Landlock ABI 9 (`RESOLVE_UNIX`) eventually collapses the split for §6, which
argues for deciding this as late as possible.

## Decision

**Nothing further is decided here.** O1 waits for a consumer; O2's macOS half
should be implemented next (it is a fold extension, not new machinery) and its
microVM half needs a design pass against libkrun's muxer semantics; O3 waits
until a cross-platform capability actually needs two commitments, and must land
as a new field.

This ADR stays Proposed as the tracking document for the three remainders. If
all three resolve elsewhere, it should be closed Rejected-as-superseded rather
than Accepted — accepting it would imply it decided something.

## Review history

- **2026-08-04, first draft.** Symbolic vocabulary + daemon-side containment IR
  + profile-keyed candidate set, premised on host resources being unknowable to
  a pre-host issuer.
- **2026-08-04, adversarial review (codex, read-only).** Four fatal findings
  (no sound "unweakened" decision procedure; containment not computable from a
  digest at the worker boundary; the vocabulary bounds names, not authority;
  `(platform, digest)` cannot express two Linux ABI profiles) — all upheld,
  draft rewritten. Also flagged the unknowability premise (Serious #6).
- **2026-08-04, superseded in part by PR #329.** The rewritten draft still
  rested on the unknowability premise; reading `LibkrunBackendConfig` disproved
  it — runtime files and devices are deployment configuration, not per-run
  values. The core case shipped as one optional field plus an equality
  contract. This document was cut down to the three genuinely open remainders
  above. The lesson is recorded on the bead: the premise was checkable in one
  file read, and two hundred lines of design were written before reading it.
