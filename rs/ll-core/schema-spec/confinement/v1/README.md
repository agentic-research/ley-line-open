# `cloister/confinement/v1` — vendor-neutral specification

**Status:** Draft (2026-07-13, paired with `ley-line-open-a2f94f`)
**Audience:** anyone building a second implementation of kernel-level
bundle confinement — whether in Rust, Go, or as a different
substrate-side runner. If your enforcement engine consumes a
`ConfinementManifest` conformant to §1's dimensions and passes the
conformance vectors in `test-vectors/`, you're conformant.

**Non-goals:** v1 does NOT cover eBPF-level syscall filtering, seccomp
profile authorship, gVisor / kata runtime selection, cgroup CPU/memory
limits, or per-syscall audit logging. Those are v2+ surfaces.

## What this capability is

A wire-protocol contract for a **kernel-confinement manifest**: the
structured declaration a substrate runner reads at bundle-start time
to decide what filesystem paths, network egress, listener ports, and
credential-vending backends the bundle may reach. The runner enforces
the manifest via kernel primitives (fs sandbox, network namespace,
port filter, credential-source binding). A bundle asking for anything
outside its declared manifest fails closed at the kernel boundary.

Three load-bearing properties this v1 publishes:

1. **Fail-closed by construction.** All five dimensions (fs / network /
   port / unix-socket / credential-source) default to DENY. Anything the manifest
   does not explicitly allow is rejected at the kernel boundary — no
   "implicit inherit-from-parent" fallback.
2. **Declarative, not procedural.** The manifest names desired end
   states (allowed paths, allowed hosts, bound port, credential
   backend). It does NOT ship shell commands, seccomp DSL, or
   iptables rules — enforcement engines translate the manifest into
   their kernel's primitives.
3. **Content-addressed enforcement.** The canonical
   `ConfinementManifest` JSON serialization is BLAKE3-hashed and the
   digest is committed alongside the bundle's identity claim (lane-2,
   per `_capability-mapping.md`). A runner that enforces a different
   manifest than the identity-committed one fails validation at the
   trust boundary — no "the manifest drifted between commit and
   enforce" surface.

## Relationship to other specs

```
             cloister-spec/confinement/v1
                          ▲
                          │ consumes
                          │
          ┌───────────────┴──────────────┐
          │                              │
  interlace-spec/0.1.0            @notme/contract
  (identity bytes)                (scope names, error codes)
```

This v1 **CONSUMES**:

- `interlace-spec/0.1.0/` — the identity claim on which
  `confinementDigest` is committed (lane-2 workload identity, per
  `_capability-mapping.md`).
- `@notme/contract` — for the shared error status vocabulary.

This v1 **DEFINES** (new content not in either upstream spec):

- The `ConfinementManifest` JSON structure (§1-§6).
- The five dimensions and their allow-list semantics
  (`fs.allow` / `network.allowHosts` / `port.bind` / `unixSocket.allow` /
  `credentialSource`).
- The canonical serialization rules (§7) so two independent
  implementations reach the same BLAKE3 digest on the same manifest.

## Document map

- `README.md` (this file) — the spec proper.
- `confinement.schema.json` — the **machine-readable shape** (bead
  `ley-line-open-41297c`). JSON Schema rather than a capnp IDL because
  `confinementDigest` is computed over canonical JSON (§7), and the IDL
  format follows the digest definition — see schema-spec `LAYOUT.md`.
  A capnp source would make the JSON a projection, leaving two
  definitions for one signed surface. Verified by
  `verify_confinement_schema` (schema-spec crate), which checks both
  that the pinned canonical manifest satisfies it and that each
  refusal §2–§6 states in prose is actually refused.
- `test-vectors/manifest-canonical.json` — a canonical example
  manifest.
- `VECTORS.sha256` — **SHA-256 CONTENT-INTEGRITY pins** for the test
  vectors ("the bytes on disk haven't drifted"). Verified by
  `verify_vectors_sha256` (schema-spec crate). This is a
  cross-cutting concern of the whole spec tree, NOT the
  identity digest §8 names.
- `CONFINEMENT_DIGESTS.blake3` — **BLAKE3-256 IDENTITY pins** for the
  test vectors — the `confinementDigest` per §8. Verified by
  `verify_confinement_digest` (schema-spec crate). Bead
  `ley-line-open-193170`: distinct from the SHA-256 integrity pin
  above because §8's semantics require the substrate's Σ hash
  (BLAKE3-256), and pinning it separately lets us prove cross-impl
  conformance on every workspace test run.

## §1 Five dimensions

A `ConfinementManifest` describes five orthogonal capability
boundaries. Every dimension defaults to **DENY**; the manifest names
only what is allowed.

| Dimension | Field | What it constrains | Kernel primitive (Linux) | Kernel primitive (macOS) |
|-----------|-------|--------------------|--------------------------|--------------------------|
| **fs** | `fs.allow` | Path prefixes readable/writable by the bundle | `landlock_ruleset_add_rule` (LANDLOCK) | `sandbox_init` with path allow-list |
| **network** | `network.allowHosts` | Host allow-list for egress | Network namespace + userspace SOCKS filter | `pf` (packet filter) allow-list |
| **port** | `port.bind` | Listener ports the bundle may bind | Landlock `BindTcp`, per port | **Not expressible** — see below |
| **unixSocket** | `unixSocket.allow` | UNIX socket paths the bundle may connect to, and may bind | **Not available** at the targeted ABI — see below | Seatbelt `(allow network-outbound (literal …))`, per path |
| **credentialSource** | `credentialSource` | Vault backend for credential vending | URL/scheme validation before `nono::keystore::load_secret_by_ref` | Same |

**§4 and §6 are not interchangeable, and the asymmetry is a property of the
kernels rather than of any implementation.** Landlock filters TCP `bind(2)` by
port, so §4 means on Linux what it says. Seatbelt cannot: it scopes only the
*outbound* direction per port, and the bind/inbound direction is all-or-nothing
— a conformant macOS runner asked for one listener would have to grant every
port on every address. A runner that cannot express a §4 declaration MUST refuse
it (§9 condition 6) rather than grant the wider rule.

**§6 has the mirror-image gap, and the two do not cancel.** Seatbelt filters a
UNIX socket by path — it classifies the connect as `network-outbound` and emits
a literal rule. Landlock, at the ABI this implementation targets, cannot: its
network access set is `BindTcp | ConnectTcp` and nothing else, and no *network*
right covers AF_UNIX at any ABI. A §6 grant is therefore enforceable on macOS
and unavailable on Linux here, exactly inverting §4.

**"Unavailable here", not "impossible".** The distinction is load-bearing, and
the sloppier claim expires. Landlock ABI 9 (kernel 7.1) adds
`LANDLOCK_ACCESS_FS_RESOLVE_UNIX` — a *filesystem* right, not a network one —
which mediates `connect(2)` and `sendmsg(2)`-with-explicit-recipient on pathname
UNIX sockets, at exactly the per-path granularity §6 declares. So the dimension
is expressible on Linux upstream. What blocks it is this stack: the `landlock`
crate stops at ABI 7 and has no `RESOLVE_UNIX` constant to emit, and the sandbox
layer targets ABI 5 and consults its UNIX-socket capabilities only in the macOS
backend. When those advance, §6 becomes enforceable on Linux without a spec
change, and the refusal in condition 6 stops firing on its own.

Note also that `RESOLVE_UNIX` is scoped by domain: it governs connections to
server sockets created *outside* the new Landlock domain, while sockets created
within it stay reachable. A §6 grant naming a proxy socket that the sandboxed
process itself creates is therefore unaffected by it either way.

An earlier draft of this section claimed a socket, being a filesystem object, is
filtered by path on both kernels, and that §6 was therefore the one channel
dimension enforceable everywhere. That is false, and it is recorded here rather
than quietly deleted because the mistake is instructive: routing a channel
through a socket instead of a port does not close the platform gap, it swaps
which platform has one. There is currently **no** channel dimension enforceable
at declared granularity on both.

The practical rule that follows: a capability needing a local channel declares
§4 where Landlock enforces and §6 where Seatbelt does, and a conformant runner
REFUSES the dimension its kernel cannot express (§9 condition 6) rather than
compiling it to nothing. A silently-dropped grant is worse than a refused one,
because §8 commits the manifest's digest either way — the identity claim then
attests a clause that had no effect.

Two properties of §6 that no kernel enforces, and which a runner therefore
cannot make true on its own:

- **A grant inherits its peer's authority.** Both kernels evaluate at
  `connect(2)`, never at use. A file descriptor received over `SCM_RIGHTS`
  carries no residual policy, so a peer may delegate any capability it holds —
  a socket, a directory outside `fs.allow`, its own control channel. A §6 grant
  means "may talk to X, and holds whatever X hands over." Endpoints reachable
  from a confined workload SHOULD refuse ancillary data.
- **The abstract namespace is out of scope.** Sockets whose name begins with a
  NUL byte have no filesystem path, so a path-based grant cannot name them and
  §1's DENY-by-default does not reach them. `socketpair(2)` is likewise
  ungrantable and undeniable. Confining that address space requires a
  whole-namespace control, not a per-path one.

Any dimension the manifest omits defaults to DENY. There is no
"unrestricted" mode; a runner given a manifest with `fs.allow: []`
MUST refuse every filesystem operation.

## §2 fs.allow

A list of path prefixes the bundle may traverse. Prefixes are
canonicalized (symlinks resolved, `.` and `..` collapsed) at manifest
compile time; the runner rejects any manifest containing
non-canonical prefixes.

- **Read-only vs read-write.** Each entry is either a plain string
  (read-only) or an object `{"path": "...", "mode": "rw"}`. Any other
  `mode` is rejected. Read-write requires the prefix be under a
  writable filesystem partition; runners MAY refuse read-write on
  `/nix/store`-style content-addressed stores.
- **No file-level entries.** Prefixes MUST end at directory
  boundaries. This keeps the enforcement engine's decision O(depth)
  not O(n_files).

  > **Erratum (2026-08-03, bead `ley-line-open-41297c`).** This rule
  > and the load-bearing example directly below it contradict each
  > other: `/etc/hosts` is a file, and it is also in
  > `test-vectors/manifest-canonical.json`, whose BLAKE3 digest both
  > LLO and cloister have independently reproduced. Writing
  > `confinement.schema.json` forced the contradiction into the open —
  > encoding "must end at `/`" would have made this spec's own pinned
  > vector invalid.
  >
  > The schema therefore does **not** encode the directory-boundary
  > rule, and this is recorded rather than resolved: choosing a side
  > changes either the normative text or a digest two implementations
  > already agree on, and that is a v1-vs-v2 decision, not an errata
  > edit. The O(depth) rationale is real, so the likely resolution is
  > that file-level entries are permitted and the rule should read as
  > a recommendation — but the vector is the load-bearing artifact and
  > it says files are allowed today.
- **Load-bearing example.** A bundle that reads `/etc/hosts` and
  writes to `/var/lib/bundle-X/` declares:
  ```json
  "fs": {
    "allow": [
      "/etc/hosts",
      {"path": "/var/lib/bundle-X/", "mode": "rw"}
    ]
  }
  ```

## §3 network.allowHosts

A list of hostnames the bundle may reach for egress. Wildcards with a
leading `*.` are supported; wildcards anywhere else in the pattern
are rejected. Ports are OUT of this dimension — port control belongs
to §4.

- **DNS resolution boundary.** The runner MAY resolve hostnames at
  manifest-compile time and cache the resolved IPs, OR it MAY defer
  resolution to bundle runtime. Both are conformant; the runner
  publishes its choice in its own capabilities doc.
- **Fail-closed default.** `network.allowHosts: []` (or the field
  omitted) means "no egress at all." A bundle that needs no network
  at all should omit the whole `network` block.
- **Example.**
  ```json
  "network": {
    "allowHosts": ["api.example.com", "*.telemetry.example.com"]
  }
  ```

## §4 port.bind

Zero or one listener port the bundle may bind. v1 is deliberately
single-port; multi-port bundles publish v2. If the manifest omits
`port`, the bundle MUST NOT bind any listener.

- **Port number.** Integer 1024–65535 (privileged ports out of scope
  in v1). Runners MAY reject 8080 or other well-known
  reverse-proxied-elsewhere ports if their policy documents that.
- **Bind address.** Optional, defaults to `127.0.0.1`. A bundle
  wanting to bind `0.0.0.0` must declare it explicitly:
  ```json
  "port": {"bind": 8443, "address": "0.0.0.0"}
  ```

## §5 credentialSource

The URL of the vault backend the bundle authenticates against for
credential vending, matching the schemes `nono::keystore` validates:

- `keychain://<label>` — macOS Keychain
- `secret-tool://<lookup>` — GNOME libsecret
- `keyring://<lookup>` — cross-platform `keyring` crate default
- `file://<path>` — file-backed secret (test/dev)
- `op://<vault>/<item>` — 1Password CLI (requires `host-extras` feature)
- `apple-password://<lookup>` — macOS `security` CLI

Only ONE `credentialSource` per manifest; multi-vault fan-out is v2+.

A bundle needing no credentials omits the field. `nono::keystore`'s
URI validator is the reference implementation; conforming runners
call it before storing the manifest.

## §6 unixSocket.allow

A list of UNIX socket paths the bundle may reach. Same spelling
convention as §2, for the same reason — the shape of the entry says
what kind of grant it is, so a reader of the JSON can tell:

```json
"unixSocket": {
  "allow": [
    "/run/llo/vault-proxy.sock",
    {"path": "/run/llo/shims/", "mode": "connect-bind"}
  ]
}
```

- **Bare string ⇒ `connect` only.** As in §2, the cheaper spelling is
  the safer one: a plain path grants `connect(2)` and nothing else.
- **`mode` is closed: `"bind"` | `"connect-bind"`.** It appears only on
  the object form, and is **required** there. Connect-only has no
  spelling as a `mode` because it *is* the bare-string form — one
  grant, one spelling, so two documents cannot differ only in how they
  write the same grant and digest differently. An explicit
  `"mode": "connect"` is rejected, exactly as §2 rejects `"mode": "ro"`.
- **Trailing slash ⇒ directory.** `/run/llo/shims/` grants the sockets
  directly inside that directory; a path with no trailing slash names
  one socket. Same distinction §2 draws, encoded the same way.

**The three modes are not a convenience set.** Each names a distinct
authority over the path:

| mode | `connect(2)` | `bind(2)` | who wants it |
|---|---|---|---|
| `connect` (bare string) | ✅ | ❌ | a capability dialling a shim |
| `bind` | ❌ | ✅ | the shim itself — serve, never dial |
| `connect-bind` | ✅ | ✅ | a process that owns an endpoint *and* dials through it |

`connect` requires the socket to already exist — the grant is "you may
talk to whatever is listening there". `bind` *creates* the socket file
and so lets the bundle decide what the path means, while withholding
the dial. `connect-bind` is both. Granting a wider mode by default
would repeat §4's failure at a different granularity: a declaration
that names one thing and permits another.

**`bind` exists because a mechanism enforces exactly it.** It was not
added for vocabulary symmetry. A hypervisor tier that pairs a host
socket to a guest port with a listen side enforces serve-without-dial
directly — libkrun's vsock muxer answers a guest-originated connection
request against a `listen=true` mapping with a reset rather than a
connection. That is a boundary rather than a filter: it enforces by not
constructing the dial path at all, which is why it can hold a clause a
path filter cannot express.

That asymmetry is the mode's cost. `connect` and `connect-bind` are
purely positive grants, but `bind` carries a **negative** clause —
MUST NOT dial — and a runner whose mechanism cannot withhold
`connect(2)` MUST refuse `bind` (§9 condition 6) rather than widen it
to `connect-bind`. A mode that some tier refuses is recoverable; a mode
missing from v1 is a v2 break, which is why the vocabulary is fixed
here even though not every tier can serve all of it.

**A `connect` grant carries an ordering requirement.** The endpoint it
names MUST be bound before confinement is applied. This is not an
implementation quirk to be worked around — the path is *resolved* when
the grant is compiled, not merely recorded, and that resolution is what
stops a symlink planted at the path from redirecting the grant to a
different endpoint. A grant that cannot be resolved cannot be shown to
mean what it says.

A runner MUST therefore refuse a `connect` grant whose endpoint is not
yet bound, and MUST distinguish that refusal from a malformed manifest:
the same document compiles unchanged once the peer is up. `bind` and
`connect-bind` carry no such requirement — they create the socket — but
their *parent directory* must exist, for the same resolution reason.

The consequence for §8 is worth stating plainly, because it qualifies a
claim made there: compiling a manifest is **not** a pure function of the
manifest. Two runs of the same bytes can differ in outcome as the peer
comes up. The digest still commits to the declaration, and the applied
policy is still exactly what the declaration says — but "compiles" is a
property of the declaration *and* the moment, and a verifier must not
read a compile failure as evidence the manifest was wrong.

Per §1, this dimension is enforceable at declared granularity on macOS
and unavailable on Linux at the ABI this implementation targets — the
mirror image of §4, not a dimension that escapes the platform gap.

A bundle needing no local channel omits the block entirely, which —
per §1 — denies all of them.

## §7 Canonical serialization

Two implementations reach the same BLAKE3 digest on the same manifest
by following these rules:

1. **UTF-8, no BOM.** The manifest is emitted as UTF-8-encoded JSON
   with no byte-order mark.
2. **Sorted object keys.** All object keys — at every nesting level
   — are sorted in ASCII byte order (`sort_keys=True` in Python;
   `serde_json::to_value` + `BTreeMap` reordering in Rust).
3. **No trailing whitespace, no trailing newline.** The last byte of
   the serialization is the closing `}` of the outermost object.
4. **Two-space indentation.** Human-readable but deterministic. A
   `null`-valued field is omitted, not emitted as `"field": null`.

The reference example that conforming implementations MUST reach the
same BLAKE3-256 digest on is `test-vectors/manifest-canonical.json`.
Its BLAKE3-256 `confinementDigest` is pinned in
`CONFINEMENT_DIGESTS.blake3` (not `VECTORS.sha256`, which is a
SHA-256 content-integrity pin — a distinct concern; see the
Document map for the split).

## §8 Committing the manifest to identity

At bundle-start time, the substrate runner:

1. Reads the `ConfinementManifest` JSON.
2. Canonicalizes per §7 (Canonical serialization).
3. Computes BLAKE3-256 of the canonical bytes. Call this
   `confinementDigest`.
4. Verifies that the bundle's identity claim (lane-2 workload
   identity, per `_capability-mapping.md`) commits to
   `confinementDigest` — the identity's cert extension
   `confinementDigest` field MUST byte-match. Otherwise the runner
   fails closed and the bundle does not start.

This makes the confinement manifest **part of the workload
identity** — a runner enforcing a different manifest than the one
committed at identity issuance surfaces as a cryptographic
mismatch, not a runtime drift.

## §9 Conformance

A second implementation is conformant when:

1. It parses `test-vectors/manifest-canonical.json` without error.
2. Its canonical serialization of the parsed manifest reaches the
   BLAKE3-256 `confinementDigest` pinned in
   `CONFINEMENT_DIGESTS.blake3` for that vector.
3. Independently, its stored bytes match the SHA-256 content-integrity
   pin in `VECTORS.sha256` (a cross-cutting spec-tree convention;
   distinct concern from §8's identity digest).
4. Its enforcement engine implements the five dimensions with
   fail-closed defaults matching §1's DENY-by-default rule.
5. Its identity-commit check (§8) refuses to start a bundle whose
   identity claim commits to a `confinementDigest` different from
   the runner's computed one.
6. It REFUSES any declaration its mechanism cannot express at the
   granularity the dimension states, rather than widening it to
   something broader or dropping it silently. A grant that compiles
   to nothing, or to more than was asked for, leaves the digest
   committing to a policy that never took effect — which conditions
   2 and 5 cannot detect, because the bytes still hash correctly.
   This is the condition §1, §3, §4, §5 and §6 each invoke when they
   say a runner that cannot express a declaration MUST refuse it.

Cross-impl conformance already proven: cloister computed
`d9b5b7270bb6e5ec068aec92798dd76b0f71d1fe2640b3a09833b7742d51c617`
for `manifest-canonical.json` via `leyline-cas-ffi` (Σ substrate
hash). LLO's `verify_confinement_digest` test computes the same
value via the `blake3` crate directly. Byte-identical results prove
substrate-Σ = direct-blake3 for canonical manifest bytes.
