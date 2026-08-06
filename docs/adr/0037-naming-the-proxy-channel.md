# ADR-0037 — Naming the proxy channel: confinement/v1 assumes an egress path it cannot express

**Status:** Accepted (2026-08-05) — cloister reviewed and confirmed the §3 placement; implemented in the same release that lands this ADR.
**Bead:** `ley-line-open-0e73e8`
**Related:**
- `ley-line-open-e41717` (the Seatbelt measurement this rests on)
- ADR-0035 (one manifest, attested enforcement), ADR-0036 O3 (the platform-split digest)
- `cloister/confinement/v1` §3, §4, §5, §6, §9 condition 6

---

## The gap

`confinement/v1` tells you to use a proxy in two of its own refusals:

> §3 — "Route host-scoped egress through the proxy, or omit the dimension."
> §5 — "Vend credentials through the proxy, or omit the dimension."

And it provides no way to name the channel **to** that proxy.

The five dimensions are `fs.allow`, `network.allowHosts`, `port.bind`,
`credentialSource`, `unixSocket.allow`. §4 is the **bind** direction — a
listener the workload owns. §3 is **hostname** egress, refused on this tier
because the mechanism filters by port, not by host. Nothing expresses
"connect to `localhost:8443` and nothing else."

Concretely, an LLO-executed workload on **Linux native** that needs a local
proxy today has: §6 refused (Landlock at this ABI has no AF_UNIX right), §3
refused (hostname), §4 the wrong direction. There is no manifest it can carry.

## Why this is worth deciding now rather than later

**Both mechanisms exist and are measured working.** This is not a proposal to
build enforcement; it is a proposal to name something the kernels already do.

- **Linux:** `AccessNet::ConnectTcp` — `landlock-0.4.5/src/net.rs:49`, present
  for `ABI::V4..V7` (`from_all`). nono targets ABI 5, so it is in range today.
- **macOS:** `(allow network-outbound (remote tcp "localhost:N"))` — measured
  on macOS 26.6 via `sandbox-exec`, four cases with a passing control:

  | case | profile | result |
  |---|---|---|
  | connect to granted port | no `network-bind` | **allowed** |
  | bind | no `network-bind` | **denied** (EPERM) |
  | bind | **with** `network-bind` — control | **allowed** |
  | connect to *ungranted* port | no `network-bind` | **denied** (EPERM) |

  The control is what makes the second row meaningful: bind genuinely works
  when granted, so its denial is the sandbox rather than a broken probe. (The
  first version of this probe used `nc -l`, which blocks; `timeout` killed it
  and a successful bind was indistinguishable from a denied one. The control
  caught that. Probe kept in `ley-line-open-e41717`.)

**It has no platform split between the native tiers.** §4 is Linux-only,
§6 is macOS-only at the current ABI — the asymmetry that makes ADR-0036 O3
(per-platform `confinementDigest`) necessary. A dimension both native tiers
enforce *today* is the first channel clause with no split, which is why it is
worth knowing about **before** O3 is decided rather than after.

**Adding a dimension is cheaper than it sounds.** §6 already ran the
experiment: added in v0.16.0 as a **minor** bump under `### Added`, and the
pinned `d9b5b727` vector never moved, because absent fields are omitted from
canonical bytes. The breakage is confined to exhaustive `Dimensions`
destructures in Rust — which is the forcing function, not the cost: it is what
guarantees both backends handle the new clause instead of one silently
ignoring it. That silent-ignore is exactly what happened to §5, which had no
accessor at all and could not have been refused even if someone had thought to.

## Proposal

A dimension naming outbound TCP connections to loopback, port-scoped.

```json
{
  "version": "cloister/confinement/v1",
  "network": { "connectLocal": [8443] }
}
```

**Compilation.**

| tier | mechanism |
|---|---|
| Linux native | `NetPort::new(port, AccessNet::ConnectTcp)` |
| macOS native | `(allow network-outbound (remote tcp "localhost:N"))`, with `network-bind`/`network-inbound` **absent** |
| microVM | **refused by name**, pointing at §6 — see below |

**Why the microVM tier refuses it.** There the workload runs in the guest and
reaches the host only over vsock. `krun_add_vsock_port2` pairs a guest vsock
port with a host **UNIX socket** — there is no TCP analogue — so delivering
guest→host TCP would require `KRUN_TSI_HIJACK_INET`, which converts the
boundary from "channels the guest was given" into "the guest's sockets are
carried out", and whose outbound destinations we have not established are
scopeable at all. The honest answer on that tier is a named refusal pointing
at §6, which it *does* deliver: the guest dials a host UNIX socket, and a
host-side proxy bridges to TCP if the upstream needs it. §6 stays the
recommended form wherever the transport allows it — a filesystem path is a
real identity, and a loopback port is whoever bound it first.

**Relationship to §6 — complementary, not a replacement.**

| | §6 `unixSocket.allow` | this |
|---|---|---|
| names | a filesystem path | a loopback port |
| transport | workload must dial AF_UNIX | any unmodified TCP client |
| peer identity | strong — named, permissioned | weak — first binder wins |
| Linux native | unavailable ≤ABI 7 (ABI 9: `RESOLVE_UNIX`) | available now |
| macOS native | available now | available now |
| microVM | delivered (vsock mappings) | refused → use §6 |

## The open question this proposal cannot answer alone

**Who is the consumer?**

This is the risk that must be named, because we just paid it. §6 was designed
for cloister's macOS shim, shipped, and cloister cannot use it — their API
transport is TCP-only, so a `unix://` proxy URL is discarded and HTTPS rides
`net.connect({host, port})`. A dimension built for the proxy pattern that no
LLO workload has yet exercised has the same shape.

What differs: this one is trivial semantically (one direction, one port, no
modes, no platform inversion), and both mechanisms are measured rather than
argued. But "measured mechanism" is not "waiting consumer", and the §6 lesson
is that those are different things.

**Note that the nono connect-only fix (`e41717`) does NOT need this
dimension.** cloister's harness configures nono directly through its own
`CapabilityManifest`, not through confinement/v1. That fix stands alone and
should not wait on this. This dimension matters only for workloads whose
confinement is expressed as an *attested confinement/v1 document* — i.e. LLO's
execution path.

## Questions for cloister

1. **Do you have, or foresee, a workload whose confinement/v1 document needs
   to name a local proxy channel?** A "not yet" is a perfectly good answer and
   argues for recording this and stopping.
2. **`network.connectLocal` as its own dimension, or `port.connect` extending
   the existing block?** The latter avoids the `Dimensions` break entirely and
   keeps v1's single-listener constraint, at the cost of `port` no longer
   meaning "the listener". We lean separate — one dimension, one idea, and the
   destructure break is a feature.
3. **Does a split-free channel dimension change your read of ADR-0036 O3?**
   O3 exists because §4 and §6 disagree across kernels; a clause both native
   tiers enforce may narrow what O3 has to cover.
4. **Is the microVM refusal-pointing-at-§6 the right shape**, or do you want
   guest→host TCP delivered (which means establishing whether TSI's outbound
   destinations can be scoped at all)?
5. **Any objection to it landing as a minor `Added`**, precedent being §6 in
   v0.16.0 with the canonical pin unmoved?

## Not proposed here

- Anything about §3 `allowHosts`. Host-scoped egress still needs the proxy;
  this names the channel to it, not a replacement for it.
- Any change to §4. `port.bind` remains the listener, and remains refused on
  Seatbelt for the reason it always was: bind cannot be port-scoped there.
- Any change to the `confinementDigest` semantics, the equality contract, or
  the pinned vector.

## Decision

**None yet.** This is a proposal for the other implementer of
`cloister/confinement/v1` to review. If question 1 answers "not yet", the
correct outcome is to leave this recorded on `ley-line-open-0e73e8` and build
nothing — the gap is real either way, and knowing it exists is most of the
value.
