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
  on macOS.

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

## Beads

Spike: `ley-line-open-bb8cc1`. Filed when this ADR is accepted: building libkrun from the patched
source in LLO's toolchain, the `DynFileSystem` backend over the store, the execution/v1 runtime wiring, measurements V-F2 and V-F3, and the agent-in-VM launch path.
