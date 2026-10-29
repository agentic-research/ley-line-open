# ADR-0044 — Agents see a filesystem served from userspace: an LLO virtio-fs device in a microVM

**Status:** Accepted (2026-10-15); LLO carries the libkrun patch until upstream takes it
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

LLO implements the FUSE request set (LOOKUP, GETATTR, OPEN, READ, WRITE, FLUSH, FSYNC, RELEASE,
CREATE, UNLINK, RENAME, SETATTR, READDIR) over the store, through `DynFileSystem`: an object-safe
trait that a libkrun patch adds beside libkrun's private, generic `FileSystem` (see "Spike results"):

- **Reads** resolve a path through the checkout's ref (ADR-0043) to a blob in `source_blobs`.
- **OPEN** records the bytes the file held as the open's base (per open, as FUSE handles do since
  #453).
- **FLUSH and FSYNC** propose the file's new bytes against that base (ADR-0042). **RELEASE**
  discards an unproposed buffer: the Linux FUSE client does not return RELEASE errors to `close`.
- **Refusals** answer the request with a Linux errno (the guest is Linux whatever the host is):
  `ESTALE` for a moved base, `EIO` for a syntax refusal. The Linux FUSE client returns FLUSH and
  FSYNC errors to `close` and `fsync`, so every refusal reaches the agent. The spike measured this
  on macOS. *Amended by ADR-0045:* a file that does not parse is stored with a diagnostic, not
  refused, and a moved base is merged three ways; only overlapping edits are `ESTALE`.

### V3 — Reads come from immutable blobs; only the write path is served per operation

Content-addressed blobs never change, so a read needs no round trip to LLO once the blob is in the
guest. Reads are served from immutable blobs mapped into the guest: a virtio-fs DAX window over the
published snapshot's blobs, or a read-only image of them, shared through the page cache. This is
cloister ADR-0050's fast path. Only the operations that carry ADR-0042's guarantees reach LLO per
operation: OPEN for writing (the base), WRITE (staged bytes), and FLUSH, FSYNC and RELEASE (the
proposal and its errno). ADR-0050's "validate-on-commit" becomes "propose on flush": the commit point
is the agent's own `fsync` or `close`, and its outcome is that call's return value.

DAX on macOS works for memory the backend hands to `hv_vm_map`, once libkrun's mapping worker
runs (spike results, Q2). A write through a mapped window reaches the host's memory with no WRITE,
FLUSH or FSYNC carrying it, so the backend refuses writable mappings: DAX serves files the agent
reads, and the writable working tree is served without DAX or with per-inode DAX (`dax=inode`) on
read-only files only.

#### Read freshness

Decided in bead `ley-line-open-e6269a`; built in `rs/ll-open/storefs` (beads `789e0f`, `e650d7`).

**No invalidation channel exists.** The backend cannot tell the guest that a path changed. libkrun's
device offers no `VIRTIO_FS_F_NOTIFICATION`, configures no notification queue, and answers
`notify_reply` with `ENOSYS`; Linux's client has no receiver either: `fs/fuse/virtio_fs.c` at v6.12
and at master defines only `enum { VQ_HIPRIO, VQ_REQUEST }` and an empty `feature_table`
(`docs/benchmarks/2026-10-15-vf3-virtiofs.md`, "Source check"). A guest's cached entry, attribute
or page is therefore as fresh as its timeout or the next OPEN.

**Caching is required for speed.** The V-F3 run (same document) measured a storefs-shaped backend
with zero timeouts and `DIRECT_IO` at 29–82× APFS on tree walks (`find` issues 364k requests for
20,857 files). Long timeouts, READDIRPLUS and page-cached read-only opens bring warm walks to
1.0–2.3× APFS. Those numbers are the in-process prototype's. The shipped path (worker -> share
socket -> daemon storefs, Cap'n Proto frame headers) is measured in
`docs/benchmarks/2026-10-20-share-path.md`: reads cost a socket round trip per request on top,
and every accepted write re-snapshots the whole store (bead `ley-line-open-8c8ded`).

**The contract:**

- **OPEN always reaches the backend** and reads the path's current blob. A read-only open returns
  `KEEP_CACHE` only when that blob is the one the previous open of the inode served (that open
  read-only); otherwise it omits `KEEP_CACHE` and the guest drops the file's pages at the open.
  An OPEN reply carries no attributes, and the guest bounds a cached read by the size its cached
  attributes hold, so a read-only open serving a blob of another size than the last reply carrying
  the inode's attributes told the guest is `DIRECT_IO`: its reads reach the whole blob (bead
  `f210c2`; the in-guest test read a grown file cut to its old size before this rule). File
  contents are thereby current at every open (open-to-close consistency from content addressing,
  without notifications). OPENDIR applies the same rule to a directory's listing and sets
  `CACHE_DIR`.
- **Write opens stay `DIRECT_IO`**, the writeback cache stays off, and WRITE checks the handle's base
  against the path's current `content_hash`: a stale base is `ESTALE` from `write(2)` itself, and
  FLUSH and FSYNC keep their checks. No write guarantee depends on a timeout. *Reversed by
  ADR-0045 T6:* WRITE stages whatever the store holds; FLUSH and FSYNC decide, merging a moved
  base and answering `ESTALE` only for overlapping edits.
- **Timeouts adapt to change.** Entry, attribute and negative-entry timeouts are long (default 60 s)
  for a path with no recent change and short (default 1 s) for a path, and its parent directory,
  changed within a window (default 60 s): changed by a proposal the backend landed, or observed
  changed when the backend next served it in a different state than it last served. All three
  values are configuration.
- **What stays stale:** another writer's change to a path the guest holds a long timeout for is
  invisible to `stat` and LOOKUP (including a cached negative entry) until that timeout expires.
  A notification path in libkrun and the Linux client would remove this bound; neither exists.

READDIRPLUS reaches the backend through the vendored libkrun's `DynFileSystem` (patch
`0004`, `rs/third_party/libkrun-patches/`).

### V4 — The host mount is compatibility

The NFS and FUSE host mounts remain for people and for tools that need a directory on the host.
They keep the guarantees they can carry (per-open bases on FUSE; READ-time bases and WRITE-time
stale refusal on NFS) and document what they cannot. NFS-only heuristics are not added: #461 is
closed unmerged, and its probe results are recorded above. ADR-0045 T8 makes the host mount
storefs itself, behind FUSE and NFS adapters: a host process sees the tree a guest sees and its
writes are the proposals a guest's are, with the NFS WRITE-time refusal the decision the COMMIT
would make. The merge and `ESTALE` hold between separate writers (a guest's session, a FUSE
handle, a socket proposal); two processes through one NFS mount share one NFS client, and the
last save wins (`ley-line-open-5ec040`). On macOS the NFS mount is refused before Tahoe 26.7 /
Sequoia 15.8, whose NFS client corrupts kernel memory on server replies
(`ley-line-open-2d49ce`). The agent path stays the virtio-fs share: the host mounts pay a kernel
round trip per request.

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
- libkrun 1.19.4 does not accept a custom virtio-fs backend through any API. LLO links libkrun as
  a Rust library (its `libkrun` crate builds as `cdylib` and `lib`) from a patched source tree
  instead of dlopening the Homebrew dylib. That is a supply-chain change: LLO builds and ships the
  VMM it runs. The patch is offered upstream; until it lands LLO carries it (spike results).

## Spike results (bead `ley-line-open-bb8cc1`, 2026-10-14)

Source read: libkrun tag `v1.19.4` (Apache-2.0, `src/devices/Cargo.toml`). Run on macOS 26.6.2
arm64 with Homebrew libkrunfw 5.5.0. Prototype: `spikes/virtiofs-backend/` on branch
`spike/bb8cc1-virtiofs-backend` (a standalone cargo workspace outside `rs/`, never built by CI or
release); `run.sh` clones the tag, applies `libkrun-v1.19.4-custom-fs.patch`, builds, signs and boots the guest.

**Q1 — how the backend plugs in.**

| Route | Finding |
|---|---|
| C API | None. `krun_add_virtiofs`..`4` take a host directory (`include/libkrun.h:316-389`). `krun_fs_add_overlay_file` serves a `&'static [u8]` read-only (`virtual_entry.rs:22`); a write to it is `EPERM` and its FLUSH/FSYNC return success unconditionally (`augment_fs.rs:381-410`), so it cannot carry a refusal. |
| Link the `devices` crate | Not possible unpatched: `mod filesystem` is private (`fs/mod.rs:4`), the trait is generic (`read<W>`, associated types; not object-safe), and the worker's server is a closed enum of passthrough, read-only passthrough and null (`fs/worker.rs:28-32`). |
| vhost-user-fs | Absent: the only `vhost` use in the tree is the sound device (`virtio/snd/stream.rs:348`). |
| Patch, link as a Rust lib | **Chosen and proven.** The patch (7 files, +574/-2) adds `dyn_fs.rs` (`DynFileSystem`, an adapter to `FileSystem`), a `Custom` server variant, `custom_backend` on `FsDeviceConfig`, and `krun_add_virtiofs_backend(ctx, tag, shm_size, Arc<dyn DynFileSystem>)`. The non-DAX path is platform-independent; the patched tree type-checks for `aarch64-unknown-linux-musl`. |

The guest (libkrunfw kernel, libkrun's `init.krun`, a static musl program) mounted the backend's
tag and printed:

```text
PASS read-from-host-memory: guest read "from-llo-memory pid=21150 t=1792033654879836000"
PASS accepted edit: fsync=0 close=0 reread="key=edited\n"
PASS stale edit -> ESTALE: fsync errno=116 close errno=116 (ESTALE=116); store holds "key=base\n"
PASS syntax edit -> EIO on close: close errno=5 (EIO=5); store holds "key=base\n"
PASS dax llodax: STATX_ATTR_DAX=true read="from-llo-memory ..." mmap="from-llo-memory ..."
guest done: 0 failure(s)
```

The bytes were generated in the VMM process at start-up; no file holds them. The backend logged
`FSYNC ... base v1 but store at v2 -> ESTALE` and `FLUSH ... does not parse -> EIO` for the two
refusals. This closes V-F1's mechanism on macOS; V-F2 to V-F5 remain open.

**Q2 — DAX on macOS.** libkrun implements it (`macos/passthrough.rs:2687-2765`: mmap the file,
then `GpuAddMapping` to `hv_vm_map`), and the libkrunfw kernel mounts `dax=always` and sets
`STATX_ATTR_DAX`. Two defects make it fail in 1.19.4:

1. The worker thread that services `GpuAddMapping` starts only when a virgl GPU is configured
   (`src/libkrun/src/lib.rs:3022-3025`). With virtio-fs DAX and no GPU, the fs worker blocks
   forever on the reply and the guest's first DAX read hangs (observed; log shows `setupmapping`
   and no `add_mapping`). The patch starts the worker when any fs device has a window.
2. Passthrough maps a read-only open as `MAP_SHARED` over an `O_RDONLY` fd, and `hv_vm_map`
   rejects that mapping (`0xfae94001`, also with `HV_MEMORY_READ` alone). A standalone probe
   accepted anonymous private/shared, file `MAP_SHARED` over a writable fd, and file `MAP_PRIVATE`.
   The guest reading a passthrough DAX file is killed with `SIGBUS`. LLO's backend is unaffected:
   it hands over anonymous memory, so the store's blobs must live in (or be copied once into)
   memory of an accepted kind.

On Linux, libkrun's passthrough DAX `mmap`s an fd `MAP_FIXED` into the window
(`linux/passthrough.rs:2057-2100`); an in-memory backend there hands over a memfd. Not built.

**libkrun does not use `fuse-backend-rs`.** Cloister's ADR-0044 and ADR-0049 describe libkrun's
virtio-fs server as built on `fuse-backend-rs`. In v1.19.4 no crate depends on it (absent from
`Cargo.lock`); the server and `FileSystem` trait are an in-tree copy of crosvm's
(`fs/server.rs:1`, `fs/filesystem.rs:1`, "The Chromium OS Authors"). A "fuse-backend-rs adapter
over the store" is therefore a `DynFileSystem` implementation against the patch above.

**Hybrid on macOS (cloister ADR-0050's shape).** Feasible with the pieces the spike ran: immutable
blobs served through a DAX window cost one SETUPMAPPING per 2 MiB chunk and no request per read
(`PASS dax llodax`); the write path (OPEN for write, WRITE, FLUSH, FSYNC, RELEASE) stays per-op in
LLO and its refusals reach `fsync` and `close` (`PASS stale edit`, `PASS syntax edit`). The read
side can also be a read-only disk image (`krun_add_disk`; the installed library reports
`KRUN_FEATURE_BLK`). Metadata (LOOKUP, GETATTR, OPEN) of immutable files still crosses to LLO
unless the backend returns long entry and attribute timeouts, which immutable content allows. Not
run: combining the two in one tree (a guest overlayfs with the read-only share as lower and LLO's
writable share as upper, or `dax=inode` on one share), so copy-up and overlayfs's demands on the
upper filesystem (xattrs, whiteouts, rename) are unmeasured.

## Open questions

1. **How the backend plugs in.** Answered by the spike: a libkrun patch and LLO linking libkrun
   from source. Open: whether upstream takes the patch (a C API shape, a callback vtable, would
   serve non-Rust embedders; LLO needs only the Rust one).
2. **DAX on macOS.** Answered by the spike: works for backend memory with the worker fix; refused
   for writable mappings by design (V3).
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

## Phases

| Phase | Scope | Bead | Status |
|---|---|---|---|
| A | LLO builds its own VMM from vendored libkrun v1.19.4 plus the custom-fs patch; the runtime links it instead of loading the Homebrew dylib; re-pinned to v1.19.6 with the same patches (`397fa6`), and CI checks the vendored tree (`396d77`); the worker is its own unpublished crate, `leyline-krun-worker`, so `leyline-runtime` has no libkrun dependency and no `vmm` feature (`395e43`) | `f1c46f` | Done (#464) |
| B | the store-backed `DynFileSystem`: reads from the checkout's ref, writes as proposals, refusals as Linux errno | `f1ea99` | Done (#463) |
| C | agents run in a guest whose working tree is that share; the end-to-end scenarios run inside the guest; V-F2 and V-F3 measured | `f210c2` | Done on macOS/HVF: the in-guest scenarios pass (below). V-F3 was measured by the spike harness (`docs/benchmarks/2026-10-15-vf3-virtiofs.md`); the daemon's share is measured by `share_bench` (`docs/benchmarks/2026-10-20-share-path.md`, bead `39d3a4`); V-F2 (dtrace) is not measured; Linux/KVM not run |

## Phase C results (bead `ley-line-open-f210c2`, 2026-10-15)

**How it is wired.** The VMM runs in the `leyline-krun-worker` process and the store lives in the
daemon, so the share crosses a process boundary on one connected UNIX socket pair: the daemon's
libkrun backend (`KrunWorkerBackend::with_workspace_shares`) opens the run's share before it
spawns the worker and passes one end as an inherited descriptor (`--workspace-share-fd`); the
worker's virtio-fs device (`ShareFs`, libkrun's `DynFileSystem`) forwards each FUSE request down
it (`backends::libkrun::share`: JSON header, READ and WRITE bytes as raw payload), and the daemon
answers from `StoreShare` (the storefs README "Adapter" mapping) over a `StoreFs` on its own
`DaemonContext`: the share and the socket propose on one writer, and a landed share proposal is
written into the checkout and published to the arena as a socket proposal is. The grant scopes
the share (`read`/`list` see, `mutate`/`commit` propose), and the share opens only when the
store's `treeRoot` (ADR-0040 D1, the tagged fold over `_source`) is the grant's `graphRoot` (not
`Controller.current_root`, not `Head.rootHash`). The guest init mounts the share at `/work`
(libkrun patch `0005`, `KRUN_VIRTIOFS_MOUNTS`) and starts the workload there.

**The share in the confinement evidence** (bead `ley-line-open-b83347`). The guest sees only
virtio-fs, but the worker process holds the share socket, and that descriptor is outside its
`confinement/v1` manifest: nono confines what the worker may open, not what it inherits open. The
worker and the backend derive one declaration from the grant (`worker::declared_descriptors`: the
share's descriptor, workspace and `graphRoot` for a run granted a workspace, nothing otherwise).
The backend spawns the worker with its standard streams and that declaration only
(`worker::inherit_only` marks every other descriptor it holds close-on-exec in the child, so one
the daemon inherited open from its own parent does not pass on). Before it reads the rootfs, the worker lists its open descriptors and refuses to start holding any
beyond its standard streams and that declaration; its readiness event carries the declaration,
and the backend refuses one that differs from what it handed the worker. The receipt's
`backend.evidence` is the digest of the backend evidence document (execution/v1 README "Backend
evidence"), which names each `workspaceShare` channel; a run without a workspace names none. The
in-guest scenarios below run through this check: the daemon's worker holds exactly its streams and
the share.

**`expectedGeneration`.** ADR-0041 S1 defines the grant's `WorkspaceGrant.expectedGeneration` as
the commit `seq` within the grant's epoch, and no store implements `(epoch, seq)` yet (ADR-0041 is
Proposed), so there is no value to compare a claim with. The daemon's share refuses a grant whose
`expectedGeneration` is nonzero (`identityPolicyMismatch`, before the worker starts; bead
`ley-line-open-39abfa`): a claim nothing checks is not run. Zero claims no generation, and the
share is pinned by the `graphRoot` check: it opens only when the store's `treeRoot` is the grant's
`graphRoot`. Comparing a nonzero claim with the store's `seq` lands with ADR-0041 S1.

**The test.** `rs/ll-open/cli/tests/agent_in_guest.rs` runs the real `leyline execution-daemon
--backend micro-vm --source <checkout>` and starts an execution/v1 run over the socket whose
RunGrant carries a WorkspaceGrant. The workload is a static guest program using file syscalls,
`git` and `rg` (Alpine aarch64, `tools/guest_tools.sh`); it and the test coordinate through files
in the store. macOS 26.6.2 arm64, Hypervisor.framework, libkrunfw 5.5.0, settled timeout 3 s:

| # | Scenario | Guest syscall result | Host check (socket and checkout) |
|---|---|---|---|
| a | read a file | `read(2)` a.go: 37 bytes, the stored blob | — |
| b | open/write/close edit | `close(2)` = 0 | `read_source` and the checkout hold the edit |
| c | socket proposal to a file the guest holds open | `write(2)` = -1, `errno=116` (`ESTALE`); `close(2)` = 0 | b.go holds the socket proposal, not the guest's bytes |
| d | unparseable edit | `fsync(2)` = -1 `errno=5` (`EIO`); `close(2)` = -1 `errno=5` | c.go unchanged in the store and the checkout |
| e | create, unlink, write-temp-then-rename | `close(2)` = 0; `unlink(2)` = 0; `rename(2)` = 0, a.go reads back the save | n.go created, d.go deleted, a.go holds the save; the temp file reached neither |
| f | `git status`, `rg` over `/work` | `git status --porcelain` lists ` M a.go`, ` M b.go`, ` D d.go`, `?? n.go`; `rg -n` finds `n.go:3`; `rg --files` lists the tree | — |
| g | host proposal, then the guest's next open; TTL bound | `open`/`read` g.go returns the new 88 bytes; `stat(2)` of t.go (never opened) shows the old size right after the proposal and the new size after 2.81 s (settled 3 s) | — |
| h | `kill -9` the daemon and restart | — | every accepted guest edit, the deletion, and both host proposals are in the store and the checkout |

The first run found a defect in (g): the guest read the new blob cut to the 37 bytes its cached
attributes held (V3 "Read freshness", now fixed by the `DIRECT_IO` rule above).

## Beads

Spike: `ley-line-open-bb8cc1`. Filed when this ADR is accepted: building libkrun from the patched
source in LLO's toolchain, the `DynFileSystem` backend over the store, the execution/v1 runtime wiring, measurements V-F2 and V-F3, and the agent-in-VM launch path.
