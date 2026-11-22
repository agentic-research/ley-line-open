# Third-party source built into LLO

## libkrun — the VMM the execution runtime runs

`rs/third_party/libkrun/` is libkrun built by LLO from source instead of loaded
from an installed library (ADR-0044, bead `ley-line-open-f1c46f`). The
unpublished `leyline-krun-worker` crate (`rs/ll-open/krun-worker`) links it
and builds the `leyline-krun-worker` binary; no other crate depends on it
(bead `ley-line-open-395e43`).

| | |
|---|---|
| Upstream | https://github.com/containers/libkrun |
| Tag | `v1.19.6` |
| Commit | `227b2de6ed323fe180e02f871c5f325a90c13cc2` (2026-09-25); vendored at `v1.19.4` (`728df812`) until bead `ley-line-open-397fa6` |
| License | Apache-2.0 (`libkrun/LICENSE`); see "License" below |
| LLO patches | `rs/third_party/libkrun-patches/`, applied in file order |
| Writer and verifier | `tools/vendor_libkrun.sh sync` / `check` (`task vmm:vendor:check`) |

### What the tree holds

The upstream files cargo builds, and nothing LLO wrote by hand:
`AUTHORS`, `Cargo.lock`, `LICENSE`, `README.md`, `hvf-entitlements.plist`,
`include/` and `src/`, with the patches applied. Left out: `.github/`,
`docs/`, `examples/`, `tests/`, `init/` (the AWS Nitro and TEE inits, built
only by upstream's Makefile), `krun-sys/`, the Makefile, upstream's root
`Cargo.toml` (it declares only upstream's own workspace), and
`src/vmm/edk2/KRUN_EFI.silent.fd`, a prebuilt EDK2 firmware read only with
the `efi` feature, which LLO does not enable. `Cargo.lock` records the
dependency versions upstream tested; `rs/Cargo.lock` pins the same versions
where they were new to LLO.

The tree is under `rs/` because cargo-mutants builds a copy of `rs/` alone,
and a path dependency outside it would not resolve there. `rs/Cargo.toml`
excludes `third_party` from the workspace, so these crates are path
dependencies, never members: workspace lints, `cargo fmt`, tests and mutation
testing do not reach them.

`tools/vendor_libkrun.sh check` clones the tag, refuses it unless it resolves
to the pinned commit, copies the same file set, applies the patches, and
`diff -r`s the result against this tree. It needs the network, so `task ci`
does not run it; the build reads only the committed tree. The
`vendor-libkrun` workflow runs it on every pull request that touches this
tree, the patches or the script, and weekly (bead `ley-line-open-396d77`).

### The patches

Prepared as an upstream pull request; not yet submitted. Each is one
`git format-patch` commit. `0001`–`0006` were written on top of `v1.19.4`
and apply unchanged to `v1.19.6`; `0007` to `0012` were written on top of
`v1.19.6`:

1. `0001` — **libkrun/macos: start the mapping worker for virtio-fs DAX
   windows.** On macOS a virtio-fs DAX mapping is serviced by the VMM worker
   thread (`GpuAddMapping` → `hv_vm_map`), which upstream starts only for a virgl
   GPU. Without the fix a guest's first DAX read hangs.
2. `0002` — **devices/fs: `DynFileSystem`.** An object-safe trait for a
   virtio-fs backend supplied by the embedding process, an adapter to the
   server's generic `FileSystem`, and a `Custom` server variant.
3. `0003` — **libkrun: `krun_add_virtiofs_backend`.** The Rust entry point
   that attaches such a backend to a context, and re-exports of the request
   types (`krun::virtio_fs`).
4. `0004` — **devices/fs: forward READDIRPLUS through `DynFileSystem`.**
   `DynFileSystem::readdirplus` (object-safe: entries go to
   `&mut dyn FnMut(DirEntry, Entry)`) and its forward from `DynFs`; without
   it a backend answered `ENOSYS` and the guest sent one LOOKUP per name.
   Measured in `docs/benchmarks/2026-10-15-vf3-virtiofs.md` (bead
   `ley-line-open-789e0f`).
5. `0005` — **init: mount the virtio-fs devices `KRUN_VIRTIOFS_MOUNTS`
   lists.** The guest init mounts each `tag:/directory` pair the VMM lists
   before the workload runs, and refuses to start the workload when one does
   not mount. The libkrun worker lists a run's workspace share (`work:/work`,
   bead `ley-line-open-f210c2`), so an unmodified workload starts in its
   working tree without calling `mount(2)`.
6. `0006` — **devices/fs: forward READLINK and SYMLINK through
   `DynFileSystem`.** `DynFileSystem::readlink` and `DynFileSystem::symlink`
   with `ENOSYS` defaults, forwarded from `DynFs`; without them a guest could
   neither read nor make a symbolic link on the share (ADR-0045 T4, bead
   `ley-line-open-bb8aa6`).
7. `0007` — **init: set the guest clock from the host's, not the RTC's whole
   second.** The VMM passes `KRUN_REALTIME_NS` (the host's wall clock as the
   kernel command line is built) and init sets `CLOCK_REALTIME` to it plus
   `CLOCK_BOOTTIME`. The kernel's own first time, from the PL031 RTC's whole
   seconds set to the middle of the second, started a guest up to half a
   second ahead of or behind the host (+0.09, +0.14, +0.31, −0.24 s over four
   boots); ahead, git's racy-clean check compared a guest-clock index mtime
   with host-clock share mtimes and missed a same-second, same-size edit.
   The guest now trails the host by the VMM's setup time (75–150 ms
   measured) and is never ahead (bead `ley-line-open-f8138d`).
8. `0008` — **devices/vsock: a configured TSI port map lists every
   published listener.** Upstream's TSI stream proxy used the port map only
   to rename ports: a guest listen on an unmapped port was bound on the host
   anyway, at the address the guest asked for. With a map configured
   (`krun_set_port_map` called, as the worker always does), an unmapped
   INET/INET6 listen is now refused with `EACCES`, and a mapped one is bound
   on the host's loopback interface instead of the guest's `0.0.0.0`. Found
   by the live-guest §4 test, which on macOS reached an undeclared guest
   listener and reached the declared ones on the host's LAN address: the
   VMM's Seatbelt profile grants `network-bind` for every port once a §4
   grant opens one (bead `ley-line-open-7d4d45`).
9. `0009` — **devices/vsock: with a port map, a TSI datagram socket
   carries replies only.** The datagram proxy never read the map. A guest
   `sendto` made it bind a host UDP socket at `0.0.0.0` and an ephemeral
   port, and every datagram that arrived there was forwarded to the guest,
   from any source on any interface, even when the VMM's sandbox had refused
   to transmit the guest's own datagram. With a map configured, an INET
   datagram proxy binds nothing for the guest (the host's own `sendto`
   binds when a datagram leaves), admits a peer only once the host has
   reached it (a delivered `sendto`, a successful `connect`; the 64 most
   recent), forwards only datagrams from admitted peers, and drops at most
   64 others per wakeup. A destination the guest merely named admits
   nothing, so a hostile guest cannot choose who reaches it. Under LLO's
   VMM profile no guest datagram leaves, so the VMM holds no UDP port at
   all. A guest UDP bind to a fixed port was already unpublished (the
   proxy's `listen` is a no-op). Found by the live-guest datagram test,
   which reached a guest UDP socket from loopback and from the host's LAN
   address under a §4 grant (bead `ley-line-open-0026d2`).
10. `0010` — **init: present declared connections as guest-loopback TCP,
    deny UDP.** The embedder lists its declared connections in
    `KRUN_LOOPBACK_CHANNELS` (`dial:<tcp port>:<vsock port>`,
    `serve:<tcp port>:<vsock port>`); init binds every listener before the
    workload starts, and a relay process joins each connection to the
    host's vsock port (dial) or to the workload's loopback listener
    (serve), one process per direction, copying bytes only. A malformed list or a listener that cannot
    be bound stops the guest before the workload runs. Init also installs a
    seccomp filter every guest process inherits: `AF_INET`/`AF_INET6`
    sockets may only be TCP streams, `AF_PACKET` and `io_uring_setup` are
    refused with `EACCES`, another architecture's system call (the i386
    ABI on x86_64) ends the process, and an x32 call is refused with
    `EACCES`; a kernel that
    refuses the filter stops the guest. The worker computes the list from
    the attested confinement document (§6 `guestPort`; ADR-0046 Q7, bead
    `ley-line-open-be3711`). A host peer that connects to a serve channel
    before the workload listens is held, and the workload's port retried
    (backoff 5 ms to 200 ms, `SERVE_HOLD_MS` = 10 s in all, ending early
    if the peer hangs up), so its first connection works once the workload
    listens; at the bound the connection is closed, which a macOS UNIX
    socket peer sees as end-of-file (bead `ley-line-open-a915e3`).
11. `0011` — **devices/vsock: bind a listening mapping's host socket before
    the guest starts.** The muxer thread bound it when the device
    activated, inside a running guest, and only logged a socket it could
    not bind, so a VM ran without a declared channel (bead
    `ley-line-open-bb2143`). The sockets are now bound when the device is
    created, owner-only (a 077 umask around the bind), and a failure
    returns `-EADDRNOTAVAIL` from `krun_start_enter`.
12. `0012` — **init: mount a bounded tmpfs on /tmp.** Upstream's init
    touched `/tmp` only in TDX builds, so a guest whose rootfs carries no
    `tmp/` had none. A rootfs imported from an OCI image never carries
    one, because its manifest lists files and an empty `tmp/` does not
    survive. Dolt's `sql-server` then failed with "open /tmp: no such file
    or directory" (bead `ley-line-open-db5899`). On every non-TDX boot,
    init now mounts a tmpfs on `/tmp` before the workload starts. It has
    mode 1777, `nosuid` and `nodev`, and is sized at 25 % of the guest's
    RAM (`TMP_SIZE_PERCENT`), so a write past that fails `ENOSPC`. Init
    creates `/tmp` when the rootfs has none and mounts only over a real
    directory: `mount(2)` follows a link in its target, so a `/tmp` that is
    a link or a file stops the guest, as does a mount that fails.
13. `0013` — **init: exec the workload's argv and environment from the
    config file, decoded exactly.** libkrun hands a workload's arguments
    (after ` -- `) and environment (`KEY="value"`) to the guest on the
    kernel command line, which the kernel and init re-parse. An argument
    `--` cut the argv there, so `tini -- docker-entrypoint.sh` ran with no
    program; a double quote was consumed or ended its value; a word inside
    a value could be read as an init token (beads `ley-line-open-7410ec`,
    `d4b07f`, `f7f862`). With `KRUN_CONFIG_ARGV=1`, init now takes the
    workload's argv and environment from the file `KRUN_CONFIG` names,
    through a strict reader of its own (`config_parse_exec`): one JSON
    object whose only keys are `Cmd` and `Env`, matched exactly, each at
    most once, each an array of strings. The general config reader is not
    used for it, because it matches its key names, without regard to case,
    against every token of the file, so an argument `uid`, `gid` or
    `entrypoint` would set init's uid or prepend to the argv. Anything
    else, or a missing or empty `Cmd`, stops the guest. Init execs `Cmd`
    (ignoring `KRUN_INIT`), removes the file, and unsets `KRUN_CONFIG` and
    `KRUN_CONFIG_ARGV` before the workload runs. The worker writes that
    file (`/.leyline-exec.json`) into the run's own root and keeps the
    workload's argv and environment off the command line. The reader reads
    the file until all of it is in, since one `read(2)` on a virtio-fs root
    may return less. The patch also replaces the `\uXXXX` case of
    `unescape_string`: jsmn has already decoded the escape in place (it
    admits only `\u0000`-`\u007F` and stores the byte where the fourth hex
    digit was), so the case now takes that byte; upstream re-read the hex
    and came out right only because the main loop then copied that byte
    over its output. `hexToDigit` and `Utf32toUtf8` are removed.
14. `0014` — **init: refuse a mount target that is not a real directory.**
    mount(2) and mkdir(2) follow a link anywhere in their path, so a rootfs
    holding `work -> usr/bin` with a workspace attached at /work had the
    workspace mounted over usr/bin, covering the verified executable (bead
    `ley-line-open-10b516`). Init now walks /dev, /proc, /sys and every
    `KRUN_VIRTIOFS_MOUNTS` target with lstat, component by component (the
    parent before creating a share target, the whole target after), and
    mounts only over a real directory. Anything else, or a path with an
    empty component, stops the guest with "`<target>` is not a directory,
    refusing to mount over it" (bead `ley-line-open-2594af`). The import
    and the manifest reader refuse such a link first; this is the in-guest
    backstop. A `.` or `..` component is refused too.
15. `0015` — **init: a refusal before the workload stops the guest as a
    failure the host sees.** Upstream init bailed out with `exit()`: PID 1
    exiting panics the kernel and libkrun reports exit status 0, so every
    refusal before the workload (a mount, a 0014 mount target, a 0013
    exec config, a 0010 channel list, the datagram filter) read on the
    host as a success with an empty console (bead `ley-line-open-7164ec`).
    Init now calls `stop_guest()`: it points stdout and stderr at the krun
    console ports once /sys and /dev are mounted, prints the reason
    (recorded by `record_stop_reason()` where the refusal is made), sets
    exit status 125 through the root filesystem as it does for a workload,
    and restarts. The host's supervisor then records the run as
    `Failed`, with the reason in its `GuestOutput`. The console ports are
    used only once the kernel's sysfs and devtmpfs are mounted (checked by
    filesystem magic); a refusal before that (a link at /dev) reaches the
    host as exit status 125 alone.
16. `0016` — **vmm: a guest that records no exit status does not exit 0.**
    When the VM stops and the guest recorded no status through the root
    virtio-fs device, libkrun fell back to the vcpu's code, "OK" whenever
    the guest merely restarted: after a kernel panic, after init died, or
    after a workload run as PID 1 exited. The VMM now exits 125
    (`NO_GUEST_EXIT_STATUS`) in that case and says so on stderr; a status
    the guest recorded, 0 included, still wins (bead
    `ley-line-open-7164ec`).

The spike that motivated them is on branch `spike/bb8cc1-virtiofs-backend`
(bead `ley-line-open-bb8cc1`).

### The guest init

libkrun embeds `/init.krun`, the first process of every guest, at build time
(`src/init_blob/build.rs`). It is compiled from the vendored
`src/init_blob/init/init.c` by `$CC_LINUX`, which `.cargo/config.toml` points
at `tools/krun_init_cc.sh`: the host `cc` (static glibc) on a Linux host
building for its own architecture, otherwise `zig cc` against musl for the
target's architecture. Nothing is taken from an installed libkrun. With zig
0.15.2 the aarch64 init is 89,160 bytes (with patches `0001` to `0012`)
and byte-identical across builds.

### What stays installed

libkrunfw — the guest kernel, distributed as a shared library — is not
vendored. libkrun dlopens it (`libkrunfw.5.dylib`, `libkrunfw.so.5`) at
`krun_create_ctx`; the worker is granted it with `--runtime-file`.

### License

libkrun is Apache-2.0. Parts derive from Firecracker (Apache-2.0, "Amazon.com,
Inc.") and crosvm (BSD-3-Clause, "The Chromium OS Authors";
`libkrun/src/rutabaga_gfx/LICENSE`); those files keep their headers. Both
licenses permit combination with LLO's AGPL-3.0-or-later code: a binary that
links libkrun is distributed under AGPL-3.0-or-later, with libkrun's notices
retained as Apache-2.0 §4 and BSD-3-Clause require. Upstream ships no NOTICE
file. The LLO patches are offered upstream under Apache-2.0.

### Updating

Change `TAG` and `SHA` in `tools/vendor_libkrun.sh`, rebase the patches onto
the new tag (dropping any upstream merged), run `tools/vendor_libkrun.sh sync`,
then `task runtime:test` with `LEYLINE_LIBKRUNFW_PATH` set, and record the move
in `CHANGELOG.md` and this file.
