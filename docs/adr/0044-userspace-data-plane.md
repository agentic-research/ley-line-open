# ADR-0044 — Agents use a userspace data plane; the mount is compatibility

**Status:** Proposed (2026-10-15)
**Bead:** see "Beads" below
**Related:**

- ADR-0041 (publication: the snapshot an mmap reader maps)
- ADR-0042 (proposals: what a write is checked against)
- ADR-0043 (one store: refs and checkouts)
- `ley-line-open-bc5a6a` (macOS NFS does not report a refused COMMIT), PR #461 (closed, unmerged)

______________________________________________________________________

## Context

Agents reach LLO today through a mount (NFS on macOS, FUSE on Linux) and through the daemon's
socket and MCP ops. Every mount operation crosses the kernel twice: the agent's syscall enters the
kernel's VFS, the kernel forwards it to LLO's server, and the answer travels back the same way.

The end-to-end test of 2026-10-14 measured what that costs beyond latency:

- The macOS NFS client holds writes until `fsync`, so the server cannot know what the agent read
  (`6116c9`, fixed by taking the base at READ and refusing a stale WRITE).
- The macOS NFS client ignores every error the server sends after a WRITE. A refused COMMIT, a
  STALE or IO answer to the closing GETATTR, and a changed write verifier all leave `fsync` and
  `close` returning success (`bc5a6a`, probed on a real mount). Refusals the server can only make
  at COMMIT never reach the agent.
- fuse-t implements FUSE over that same NFS client. FSKit removes the kernel extension but keeps
  the VFS round trip.

Making the mount carry ADR-0042's guarantees on macOS means heuristics in the NFS server (#461:
one node per file at a time, syntax checks guessed from write offsets). The direction set on
2026-10-15 is to stop routing agents through the kernel at all.

## Decision

### U1 — Reads are in-process over the published snapshot

An agent's client maps the published snapshot (ADR-0041) and queries it directly: no syscall per
read once mapped. The seqlock control block already gives a reader a consistent `(path, size,
root)`; a reader that meets an abandoned publish keeps its last snapshot (#448). mache reads the
arena this way today.

### U2 — Writes are proposals through a shared-memory ring

A client writes proposals (ADR-0042, with the bases it read) into a ring in a mapping it shares with
the daemon, and the daemon's single writer drains it. Each reply (accepted with blob hashes and the
new `treeRoot`, or a typed refusal as #455 defines it) comes back on a reply ring. The kernel is
involved only to wake a sleeper: a futex-style wait when a ring is empty or full, never per byte or
per proposal. Every refusal reaches the proposer, because the proposer reads the reply itself.

### U3 — The agent interface is a library and tools

A Rust library with a C ABI (extending `leyline_fs`) and a Go binding for mache carries U1 and U2.
Model agents use it through MCP tools (`read_source`, `propose`, queries), not through file
syscalls. A tool built into an agent harness that reads the operating system's files is outside
this path.

### U4 — The mount is compatibility

The NFS and FUSE mounts remain for people and for tools that need a directory. They keep the
guarantees they can carry (per-open bases on FUSE; READ-time bases and WRITE-time stale refusal on
NFS) and document what they cannot. NFS-only heuristics are not added. #461 is closed unmerged; its
probe results are recorded here.

### U5 — The socket stays as the slow path

The UDS and MCP ops remain for remote callers and clients that do not map the store. They carry the
same proposal and refusal types as the ring.

## Consequences

- `bc5a6a` closes as superseded: agents do not depend on the macOS NFS client.
- The ring is a new cross-process contract: framing, versioning and the reply correlation id need
  the same discipline as Cap'n Proto messages (schema first, generated bindings, fixtures).
- ADR-0043's refs (R2) travel on the ring as on the socket.

## Open questions

1. **Ring layout.** One ring per client (no cross-client contention; the daemon polls N rings) or
   one shared multi-producer ring (one wait point; producers contend).
2. **Wake primitive on macOS.** `os_sync_wait_on_address` (macOS 14.4+) or a kqueue user event
   on a shared descriptor; Linux uses a futex.
3. **Large proposals.** Inline in the ring up to a size, else bytes in a shared blob area
   referenced by hash.

## Falsifiers

| # | Claim | Test |
|---|---|---|
| X1 | a read takes no syscall once mapped | count syscalls (dtrace on macOS, strace on Linux) over N reads: zero file syscalls |
| X2 | every refusal reaches the proposer | a stale, a syntax and a grant refusal each return their typed reason on the reply ring |
| X3 | a proposal round trip is faster than the mount and the socket | the same edit through ring, socket, NFS and FUSE: p50 and p99 recorded |
| X4 | a crashed client leaves the daemon consistent | kill a client mid-write to the ring: the daemon discards the partial entry and keeps serving |
| X5 | the ring is versioned | a client built against another ring version is refused at attach |

## Beads

Filed when this ADR is accepted: ring contract (schema, bindings, fixtures), daemon drain, client
library (Rust, C, Go), MCP tools over the library, measurements X1 and X3.
