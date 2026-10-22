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
`git format-patch` commit, written on top of `v1.19.4`; all six apply
unchanged to `v1.19.6`:

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

The spike that motivated them is on branch `spike/bb8cc1-virtiofs-backend`
(bead `ley-line-open-bb8cc1`).

### The guest init

libkrun embeds `/init.krun`, the first process of every guest, at build time
(`src/init_blob/build.rs`). It is compiled from the vendored
`src/init_blob/init/init.c` by `$CC_LINUX`, which `.cargo/config.toml` points
at `tools/krun_init_cc.sh`: the host `cc` (static glibc) on a Linux host
building for its own architecture, otherwise `zig cc` against musl for the
target's architecture. Nothing is taken from an installed libkrun. With zig
0.15.2 the aarch64 init is 81,672 bytes and byte-identical across builds.

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
