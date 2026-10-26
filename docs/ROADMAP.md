# Roadmap — ley-line-open

This file states where LLO is going and in what order. Beads are the work ledger:
each line here names the bead that owns it, and a bead's acceptance criteria are
the definition of done. When a bead closes or the plan changes, update this file
in the same pull request.

Last updated: 2026-10-26, at integration `ac742873` (latest tag v0.20.5).

## v1.0: single host, portable

**v1 is one daemon owning one store on one machine, the same on macOS and
Linux.** Agents work through an ordinary filesystem: a libkrun microVM over the
store-backed share (ADR-0044), or the host mount, which serves the same storefs
core (ADR-0045 T8). Multi-host stores come after v1.

Epic: `ley-line-open-b2e687`. It closes when every blocker below is closed, a
v1.0.0 tag is published with its crates on crates.io, and
`docs/COMPATIBILITY.md` states the semver promise for each frozen contract.

| v1 criterion | What it means | Beads |
|---|---|---|
| Portable, proven | The in-guest end-to-end test runs in CI on Apple silicon and on Linux/KVM, not only on a developer machine | `16a994`, `5c2040` |
| Stable contracts | The `propose` wire, the projection ABI (`_tree`, `_source`, `_parse_diagnostics`), execution/v1 and the share header are frozen under semver, with fixtures that fail on a breaking change; the governing ADRs have a decided status | `b70e44`, `10075e` |
| Write path for real workloads | Build output stays out of the store; bursts of authored writes commit as one batch; a write is acknowledged after one durable append; a guest write stays under 50 ms | `39998b`, `18bfdd`, `98d0c3` |
| Fail-closed authority | A run writes only inside its grant's write scope; a backend refuses a workspace it cannot serve | `483134`, `204a94` |
| Installable | The crates publish to crates.io; a release is a tag, not a git revision | `c3e8c6`, `395e43` |
| Git compatible at the boundary | Real `git` (status, diff, commit, log, blame, push, fetch) behaves in an agent's working tree as in a plain clone; git object ids are an export-only identity domain | `cf8fdb` |
| Limits fixed or documented | Two writers on one NFS mount; `.git` in the mount | `5ec040`, `5ee38a` |

Out of v1: the remote or multi-host store (`a7b9d9`), blob-mapped guest reads
(`e0893c`) unless they come cheaply, construct-aware merge (`5ade64`).

## Order of work

**Now**
- `98d0c3` — acknowledge a proposal after one durable append (the `live.db` WAL
  commit); the checkout write, head write and publish follow the reply.

**Next**
- `0fadab` — cut v0.21.0 from integration and hand mache the upgrade note
  (release train `5de852`).
- `18bfdd` — group commit: writes that land close together become one proposal,
  one parse pass, one durable flush and one publish, with each file keeping its
  own base, merge and `ESTALE`.
- `483134` — `WorkspaceGrant` gains a write scope (exact paths and directory
  prefixes); storefs refuses writes outside it.
- `204a94` — the native backend refuses a workspace-bearing run instead of
  starting without the workspace.
- `16a994`, `5c2040` — VM tests in CI (a self-hosted Apple-silicon runner; Linux/KVM).

**Measure, then cut operations**
- `be495a` — count operations per workload (FUSE requests, guest exits, socket
  round trips, flushes, bytes copied, parse passes) against the native minimum.
  Counts are deterministic, so `b18099`'s regression gate asserts on them as
  well as on timings, at two store sizes (per-write cost must not grow with the
  store).
- `b6c7ce` — parse after the reply: since ADR-0045 an unparseable file lands
  with a diagnostic, so the parse no longer gates acceptance; the projection
  follows like publication (~21 ms of the 47 ms write).
- `bf80bd` — collapse the worker-to-daemon hop on the share's hot path.

**Then**
- `b70e44`, `10075e` — the contract freeze, after the ADR status decisions.
- `cf8fdb` — the git conformance suite.
- `c3e8c6` — publish the crates in dependency order (unblocks `395e43`).
- `5ec040`, `5ee38a` — fix or document the host-mount limits.

## Performance

Measured on the shipped path (macOS 27.0.1, Apple M3 Max, APFS and a libkrun
guest only; `docs/benchmarks/2026-10-20-share-path.md`):

| Operation | VM share | APFS | Target |
|---|---|---|---|
| Small-file write in the guest | 47 ms (was ~15 s before `8c8ded`) | 0.14 ms | < 50 ms (v1); `98d0c3` and `18bfdd` widen the margin |
| Build-output write in the guest (`target/`, derived) | 7.06 ms (loaded machine; authored 56.6 ms in the same run) | 0.41 ms | near APFS after `2354c3` (scratch inside the guest) |
| open, read, close | 200–280 µs | ~20 µs | within 2× of APFS after `e0893c` (post-v1) |
| `git status`, warm | 163–182 ms | 32–90 ms | |
| `rg`, warm | ~310 ms | ~60 ms | |

Each guest request crosses the worker-to-daemon share socket (120–280 µs); the
header encoding is not the cost. Re-run the benchmark on an idle machine before
quoting a single figure.

## Known limits (documented behavior today)

- Two writers through one host NFS mount are one client to the server; the last
  close wins. Per-writer merging holds across separate sessions (the VM path) —
  `5ec040`.
- The store excludes `.git`, so git does not run inside the mount or the share —
  `5ee38a`.
- A guest in TSI-hijack mode intermittently stalls before its program runs —
  `c4480a`.
- The host NFS mount is refused on macOS Tahoe below 26.7 and Sequoia below 15.8
  (Apple NFS-client CVE-2026-43686 and CVE-2026-84549; `2d49ce`).

## Design direction: one control plane, n data planes

Native filesystems are among the most optimized code there is, and every cost
LLO has paid sits where it deviates from them: no page cache, one flush per
write instead of a group-committed journal, semantic work on the write path,
O(volume) work on a write, build output through the store. The direction
(`d4f424`) is a control plane (the store of record: `treeRoot`, proposals,
merge, grants, projection) over several data planes that hold or serve the same
bytes, each with its own identity domain and an adapter that turns its writes
into proposals:

| Data plane | Addresses bytes by | Bead |
|---|---|---|
| Store-backed share | blob hash (BLAKE3) | shipped |
| Native copy-on-write checkout (APFS `clonefile`, Linux reflink or overlayfs), passthrough virtio-fs | path | `c739e3` |
| Scratch for derived paths | path, never stored | shipped (`39998b`); guest-local `2354c3` |
| `.git` object database served from the store (gix ODB backend) | git object id | `cf8fdb` |
| Object store (R2/S3), ordered by compare-and-swap | key | `a7b9d9` |

Identities never cross planes by inference: a mapping such as "blob X is git
object Y" is a row the control plane records and attests (ADR-0032). Git
compatibility is a boundary contract: internals are free, and the end state is
an ordinary repository to git, CI, review, editors and build tools.

## After v1

- **Remote and multi-host store** (`a7b9d9`): blobs in object storage, an ordered
  log advanced by compare-and-swap, every host a revalidating cache (prior art:
  Cursor's Continuity). The identity question comes first: which root is
  authoritative across hosts.
- **Blob-mapped guest reads** (`e0893c`): map immutable blobs, not paths, from
  shared memory on unified-memory hosts; revalidate by root or generation at open.
- **Merge gate for the shared tree** (`39c0fc`, `6bbeea`): a clean line merge can
  still break the build; the gate is the owner's design decision.
- **Upstream libkrun** (`10feaa`): submit LLO's seven patches to
  containers/libkrun, on the owner's go-ahead.

## Shipped in the 0.21.0 cycle (integration)

| PR | Change | Bead |
|---|---|---|
| #470 | Git-shaped store and three-way merge (ADR-0045) | `bb8aa6` |
| #471 | Vendored libkrun v1.19.6; CI checks the vendored tree | `397fa6`, `396d77` |
| #472 | A workspace grant claiming a generation is refused | `39abfa` |
| #473 | The host FUSE/NFS mount serves storefs; macOS NFS version gate | `3e73dc`, `53d701` |
| #474 | `leyline-sheaf` is an optional, default-on feature | `39e239` |
| #475 | The share header is Cap'n Proto; the shipped share path is benchmarked | `39d3a4` |
| #476 | The microVM worker is its own unpublished crate | `395e43` (open on `c3e8c6`) |
| #477 | The guest clock comes from the host (libkrun patch 0007) | `f8138d` |
| #478 | A proposal costs one coalesced publish, not O(store); arena growth fails closed | `8c8ded` |
| #479 | A host NFS save agrees with every read it may have been made from | `4a8953` |
| #480 | The guest-listener probe is event-driven | `bbfa52` |
| this PR | Derived paths (build output, tool state, macOS metadata) live in per-run scratch, never in the store | `39998b`, `8d6a8a` |
