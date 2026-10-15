# ADR-0044 — Agents see a filesystem served from userspace: an LLO virtio-fs device in a microVM

**Status:** Proposed (2026-10-15)
**Bead:** see "Beads" below
**Related:**

- ADR-0041 (publication), ADR-0042 (proposals), ADR-0043 (one store: refs and checkouts)
- `rs/ll-open/runtime` (execution/v1; the libkrun worker backend, which attaches a root filesystem
  with `krun_add_virtiofs3`)
- `ley-line-open-bc5a6a` (macOS NFS does not report a refused COMMIT), PR #461 (closed, unmerged)
- cloister ADR-0044 (libkrun as the compute-isolation substrate; the guest's only filesystem is
  libkrun's in-process virtio-fs server, `fuse-backend-rs`), ADR-0049 (amended: LLO owns the
  execution runtime and its libkrun backend; cloister keeps policy, authorization and audit), ADR-0050
  (Proposed: content-addressed read-only rootfs plus CoW overlay with validate-on-commit; per-op
  mediation deferred), ADR-0063 (execution/v1 is LLO-owned)

______________________________________________________________________

## Context

**Requirement (2026-10-15).** An agent works through a filesystem: its own file syscalls, with no
LLO-specific tools. An agent on a host and an agent in a container must see the same thing. The
filesystem must carry ADR-0042's guarantees: an edit is checked against what its author read, and
every refusal reaches the agent.

**What the kernel mounts cost.** Agents reach LLO today through NFS (macOS) or FUSE (Linux). Each
operation crosses the host kernel's VFS to LLO's server and back. The end-to-end test of 2026-10-14
showed the cost is correctness as well as latency:

- The macOS NFS client holds writes until `fsync`, so the server cannot tell what the agent read.
  `6116c9` was fixed by taking the base at READ and refusing a stale WRITE.
- The macOS NFS client ignores every error the server sends after a WRITE: a refused COMMIT, a STALE
  or IO answer to the closing GETATTR, and a changed write verifier. `fsync` and `close` succeed, so
  a refusal only the COMMIT can make never reaches the agent (`bc5a6a`, probed on a real mount).
  Carrying the guarantees through this client takes heuristics (#461), and the user rejected them.
- fuse-t implements FUSE over that same NFS client. FSKit and macFUSE keep the VFS round trip.

**What LLO already has.** The execution runtime boots workloads in libkrun microVMs (Hypervisor.framework
on macOS, KVM on Linux) and attaches their root filesystem as virtio-fs. libkrun serves virtio-fs
from a host directory by passthrough, so every guest file operation becomes a host syscall on that
directory.

## Decision

### V1 — The agent runs in a microVM; its working tree is virtio-fs served by LLO

The agent's process runs in a libkrun guest. Its working tree is a virtio-fs share whose device
backend is LLO, not a host directory. A guest file syscall goes to the guest kernel's FUSE client,
then over virtqueues in memory shared with the VMM, then to LLO's backend in the same userspace
process. No host VFS and no NFS client sit on the path.

### V2 — The backend answers from the store and writes through proposals

LLO implements libkrun's virtio-fs `FileSystem` interface (the FUSE request set: LOOKUP, GETATTR,
OPEN, READ, WRITE, FLUSH, FSYNC, RELEASE, CREATE, UNLINK, RENAME, SETATTR, READDIR) over the store:

- **Reads** resolve a path through the checkout's ref (ADR-0043) to a blob in `source_blobs`.
- **OPEN** records the bytes the file held as the open's base (per open, as FUSE handles do since
  #453).
- **FLUSH, FSYNC and RELEASE** propose the file's new bytes against that base (ADR-0042).
- **Refusals** answer the request with an errno: `ESTALE` for a moved base, `EIO` for a syntax
  refusal. The Linux FUSE client returns FLUSH and FSYNC errors to `close` and `fsync`, so every
  refusal reaches the agent.

### V3 — Reads come from immutable blobs; only the write path is served per operation

Content-addressed blobs never change, so a read needs no round trip to LLO once the blob is in the
guest. Reads are served from immutable blobs mapped into the guest: a virtio-fs DAX window over the
published snapshot's blobs, or a read-only image of them, shared through the page cache. This is
cloister ADR-0050's fast path. Only the operations that carry ADR-0042's guarantees reach LLO per
operation: OPEN for writing (the base), WRITE (staged bytes), and FLUSH, FSYNC and RELEASE (the
proposal and its errno). ADR-0050's "validate-on-commit" becomes "propose on flush": the commit point
is the agent's own `fsync` or `close`, and its outcome is that call's return value. Whether libkrun
maps DAX windows on macOS is measured, not assumed.

### V4 — The host mount is compatibility

The NFS and FUSE host mounts remain for people and for tools that need a directory on the host.
They keep the guarantees they can carry (per-open bases on FUSE; READ-time bases and WRITE-time
stale refusal on NFS) and document what they cannot. NFS-only heuristics are not added: #461 is
closed unmerged, and its probe results are recorded above.

### V5 — LLO owns the filesystem; cloister owns policy; they meet at execution/v1

Everything on disk and in the guest's view of files is LLO's: the content-addressed base, the
overlay and write path, the virtio-fs backend, how blobs are mapped into the guest, and the libkrun
runtime that attaches it (cloister ADR-0049 amendment, ADR-0063). cloister does not decide the
filesystem substrate; cloister ADR-0050's substrate decision moves here.

cloister owns policy: which refs and paths a run sees, whether it may propose, credentials, audit.
The two meet at contracts LLO already owns:

- **cloister → LLO:** the RunGrant's `WorkspaceGrant {graphRoot, expectedGeneration, operations}`
  (`execution/v1`) scopes the guest's filesystem. The backend serves only what it grants and refuses
  proposals it does not allow, with an errno the agent sees.
- **LLO → cloister:** the run receipt carries, per proposal, `appliedTo`, `resultRoot` and
  `proposalDigest` (ADR-0041 S3), which cloister links into its receipt chain.

### V6 — The socket stays

The UDS and MCP ops remain for remote callers and for mache. They carry the same proposal and refusal
types the virtio-fs backend uses.

## Consequences

- `bc5a6a` closes as superseded: agents no longer depend on the macOS NFS client.
- The guest sees ordinary files, so an agent harness, a shell and an editor inside the VM need no
  change.
- cloister ADR-0050 defers per-op mediation as slow and unnecessary for isolation. It predates
  proposals: its validate-on-commit has no base an edit was made against and no refusal that reaches
  the agent. V3 keeps its read path and serves only the write path per operation. ADR-0050 needs an
  amendment on the cloister side deferring the filesystem substrate to this ADR (V5).
- LLO depends on libkrun accepting a custom virtio-fs backend. If its C API does not, LLO links the
  devices crate directly or carries a fork, which is a supply-chain decision (open question 1).

## Open questions

1. **How the backend plugs in.** libkrun's C API attaches a host directory (`krun_add_virtiofs3`).
   A custom backend needs a new API in libkrun, linking libkrun's `devices` crate from Rust with a
   custom `FileSystem`, or a vhost-user-fs device (virtiofsd's protocol) if libkrun supports one on
   macOS. Spike first: serve one file from memory through each viable route.
2. **DAX on macOS.** Whether libkrun maps DAX windows under Hypervisor.framework.
3. **Agents on the host.** A host agent outside any VM gets no virtio-fs. Either host agents run in
   the VM too (the requirement treats host and container alike), or they use the compatibility
   mount with its documented limits.

## Falsifiers

| # | Claim | Test |
|---|---|---|
| V-F1 | every refusal reaches the agent | in the guest: a stale edit makes `fsync` or `close` fail with `ESTALE`; an unparseable edit fails with `EIO`; nothing lands |
| V-F2 | no host VFS on the path | count host file syscalls (dtrace on macOS) while the guest reads and writes the share: none on the store's behalf |
| V-F3 | faster than the host mounts | the same edit and read loop through virtio-fs, host NFS and host FUSE: p50 and p99 recorded |
| V-F4 | per-open bases | two guest processes open one file; the second's stale flush is refused |
| V-F5 | a guest crash leaves the store consistent | kill the VM mid-write: no partial proposal lands |

## Beads

Filed when this ADR is accepted: the spike (open question 1), the `FileSystem` backend over the
store, the execution/v1 runtime wiring, measurements V-F2 and V-F3, and the agent-in-VM launch path.
