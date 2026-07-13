# `cloister/confinement/v1` — vendor-neutral specification

**Status:** Draft (2026-07-13, paired with `ley-line-open-a2f94f`)
**Audience:** anyone building a second implementation of kernel-level
bundle confinement — whether in Rust, Go, or as a different
substrate-side runner. If your enforcement engine consumes a
`ConfinementManifest` conformant to §5's shape and passes the
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

1. **Fail-closed by construction.** All four dimensions (fs / network /
   port / credential-source) default to DENY. Anything the manifest
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

- The `ConfinementManifest` JSON structure (§5).
- The four dimensions and their allow-list semantics
  (`fs.allow` / `network.allowHosts` / `port.bind` / `credentialSource`).
- The canonical serialization rules (§6) so two independent
  implementations reach the same BLAKE3 digest on the same manifest.

## Document map

- `README.md` (this file) — the spec proper.
- `test-vectors/manifest-canonical.json` — a canonical example
  manifest with the BLAKE3 digest pinned in `VECTORS.sha256`.
- `VECTORS.sha256` — SHA-256 pins for the test vectors (verified by
  the `verify_vectors_sha256` cargo test in the schema-spec crate).

## §1 Four dimensions

A `ConfinementManifest` describes four orthogonal capability
boundaries. Every dimension defaults to **DENY**; the manifest names
only what is allowed.

| Dimension | Field | What it constrains | Kernel primitive (Linux) | Kernel primitive (macOS) |
|-----------|-------|--------------------|--------------------------|--------------------------|
| **fs** | `fs.allow` | Path prefixes readable/writable by the bundle | `landlock_ruleset_add_rule` (LANDLOCK) | `sandbox_init` with path allow-list |
| **network** | `network.allowHosts` | Host allow-list for egress | Network namespace + userspace SOCKS filter | `pf` (packet filter) allow-list |
| **port** | `port.bind` | Listener ports the bundle may bind | `SO_REUSEPORT` + per-port capability | Same |
| **credentialSource** | `credentialSource` | Vault backend for credential vending | URL/scheme validation before `nono::keystore::load_secret_by_ref` | Same |

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

## §6 Canonical serialization

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
same BLAKE3 digest on is `test-vectors/manifest-canonical.json`, and
its BLAKE3-256 digest is pinned in `VECTORS.sha256`.

## §7 Committing the manifest to identity

At bundle-start time, the substrate runner:

1. Reads the `ConfinementManifest` JSON.
2. Canonicalizes per §6.
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

## §8 Conformance

A second implementation is conformant when:

1. It parses `test-vectors/manifest-canonical.json` without error.
2. Its canonical serialization of the parsed manifest reaches the
   BLAKE3-256 digest pinned in `VECTORS.sha256` for that vector.
3. Its enforcement engine implements the four dimensions with
   fail-closed defaults matching §1's DENY-by-default rule.
4. Its identity-commit check (§7) refuses to start a bundle whose
   identity claim commits to a `confinementDigest` different from
   the runner's computed one.
