# The shipped workspace-share path vs APFS (2026-10-20)

Bead `ley-line-open-39d3a4`. The V-F3 run
([2026-10-15](2026-10-15-vf3-virtiofs.md)) measured an in-memory
`DynFileSystem` inside the VMM process: a prototype. This run measures what
ships: a libkrun guest whose `/work` is the daemon's store, every FUSE request
going worker -> share socket -> daemon storefs, against the same workloads in
a plain APFS directory. It also compares the share's frame header encoded as
JSON (before) and as Cap'n Proto (after, `capnp/share.capnp`).

## Why there is no host NFS, fuse-t, macFUSE or FSKit leg

Those legs drive host kernel filesystem code, and a loopback NFS load
kernel-panicked a development Mac on 2026-10-15 (bead `ley-line-open-2d49ce`;
the panic matches Apple NFS-client CVEs fixed in macOS 26.7). This run
measures APFS and the libkrun guest only.

## Method

**Host.** Apple M3 Max (14 cores), 36 GiB, macOS 27.0.1 (26A434). Host tools:
`/usr/bin/git` 2.50.1 (Apple Git-155), ripgrep 15.1.0 (Homebrew). Other build
work ran on the machine at the same time (load average 5-12); see "Noise".

**Guest.** The daemon's libkrun backend and the real `leyline-krun-worker`,
started by `leyline execution-daemon --backend micro-vm --source <checkout>`
through an execution/v1 run, exactly as a caller starts one. Guest kernel:
Homebrew libkrunfw 5.5.0. 4 vCPUs, 2048 MiB. Share timeouts: settled 3 s,
changed 500 ms. Guest tools: Alpine aarch64 git and ripgrep from
`tools/guest_tools.sh` (pinned by SHA-256).

**Harness.** `rs/ll-open/cli/tests/share_bench.rs` (ignored by default):

```text
cargo build -p leyline-krun-worker   # leyline-runtime --bin leyline-krun-worker before bead 395e43
LEYLINE_REQUIRE_HYPERVISOR=1 LEYLINE_LIBKRUNFW_PATH=/opt/homebrew/lib/libkrunfw.5.dylib \
LEYLINE_GUEST_TOOLS=<dir>/rootfs LEYLINE_BENCH_OUT=<table> \
  cargo test -p leyline-cli --test share_bench -- --ignored --nocapture
```

One program (`share_bench/bench.rs`) runs the workloads, compiled once as a
static Linux guest and once natively for the APFS directory; it times each
workload itself, outside boot, corpus load and tool warm-up.

| measurement | what |
|---|---|
| `open_read_close` | 1,000 × open, read 4 KiB, close of one file (every OPEN reaches the store) |
| `lookup_miss` | 1,000 × `stat` of a name never seen before (every LOOKUP reaches the store) |
| `git_status_cold` | the first `git status --porcelain` (the index was written in another directory, so git rehashes) |
| `git_status_warm_min` | the fastest of the next three |
| `rg_first`, `rg_warm_min` | `rg -c fn .` over the tree: the first run, then the fastest of three |
| `storm_total`, `storm_per_file` | create, write 14 bytes, close: 20 files |

**Corpus.** 2,034 files: this repository's tracked files plus crate sources
from `~/.cargo/registry/src` until 2,000; symlinks, `.gitignore` files and
files over 4 MiB left out. The daemon's living database for it is ~0.75 GB
(the projection: bead `ley-line-open-b4509b`).

**A/B.** Both header encodings were built from one branch (the JSON build
reverts only `runtime/src/backends/libkrun/share/{mod,tests}.rs` and drops
`codec.rs`) and run alternately, JSON then Cap'n Proto, twice, so both saw the
same machine load.

## Results

Guest through the share; APFS in parentheses from the same run.

| measurement | JSON, round 1 | Cap'n Proto, round 1 | JSON, round 2 | Cap'n Proto, round 2 |
|---|---|---|---|---|
| `open_read_close` (µs/op) | 281 (29) | 237 (22) | 203 (22) | 219 (18) |
| `lookup_miss` (µs/op) | 187 (4.0) | 127 (5.5) | 142 (7.6) | 126 (3.8) |
| `git_status_cold` (ms) | 1,979 (946) | 1,113 (447) | 1,165 (511) | 1,062 (440) |
| `git_status_warm_min` (ms) | 182 (90) | 175 (34) | 163 (32) | 166 (35) |
| `rg_first` (ms) | 643 (149) | 434 (67) | 463 (71) | 419 (99) |
| `rg_warm_min` (ms) | 531 (126) | 308 (57) | 309 (62) | 307 (59) |
| `storm_per_file` (ms) | 17,182 (15.6) | 16,475 (0.094) | 14,654 (0.81) | 14,617 (0.089) |

## Findings

1. **The header encoding is not what the share costs.** JSON and Cap'n Proto
   overlap within run-to-run noise on every read workload; Cap'n Proto's
   `lookup_miss` was lower in both rounds (126-127 µs vs 142-187 µs), by about
   the size of the spread. A request costs ~120-280 µs end to end: a guest
   FUSE request, a virtqueue, the worker, a socket round trip to the daemon,
   and storefs' store lookup. The prototype's in-process backend paid none of
   the socket or daemon cost.
2. **Reads are 5-40× APFS per request** (`lookup_miss` ~30×, `open_read_close`
   ~10×), and **2-6× on whole workloads** (`git status` warm ~5×, `rg` warm
   ~5×, `git status` cold ~2.4×): the guest's attribute and page caches
   absorb most requests of a walk.
3. **Writes cost O(store): 14.6-17.2 s per small file.** Every accepted
   proposal re-publishes the whole store: it serializes the full living
   database, hashes all of it with BLAKE3, and writes the arena (a `sample`
   of the daemon during a storm is dominated by BLAKE3 over the serialized
   image). One write costs as much as the store is large. A guest `cargo
   build`, `npm install` or `git checkout` in `/work` is unusable until this
   is fixed: bead `ley-line-open-8c8ded` (P0). An earlier attempt with a
   200-file storm on a 10,000-file corpus did not finish in the run's 900 s.

## Noise

The APFS column moves 2-10× between runs (`git_status_cold` 440-946 ms,
`storm_per_file` 0.089-15.6 ms) because other builds ran on the machine. The
alternation keeps the JSON/Cap'n Proto comparison fair; absolute numbers are
an upper bound on an idle machine's. Re-run on an idle machine before quoting
a single figure.

## After bead `ley-line-open-8c8ded` (2026-10-23)

The write cost is fixed by four changes (ADR-0041 amendment): a proposal
requests a publish and the daemon's single publisher coalesces every proposal
landed since its last publish, off the writer; the arena keeps its size while
the image fits a buffer and grows with a quarter of headroom (the earlier
sizing rewrote the whole arena on a few KiB of growth); `current_root` is
hashed across cores (the same BLAKE3 bytes); and a parse pass that changes no
reference or definition reuses the unbound-reference count recorded with the
rows instead of scanning `node_refs`.

Release build, `perf/8c8ded-proposal-cost` @ e55a79b6, macOS 27.0.1, Apple M3
Max, one run, 2,044 files, a 200-file storm, APFS and the libkrun guest only:

| measurement | guest (share) | APFS |
|---|---:|---:|
| `open_read_close` | 180.2 µs/op | 12.0 µs/op |
| `lookup_miss` | 113.2 µs/op | 2.1 µs/op |
| `git_status_cold` | 996.3 ms | 328.0 ms |
| `git_status_warm_min` | 136.8 ms | 23.3 ms |
| `rg_first` | 297.7 ms | 61.4 ms |
| `rg_warm_min` | 242.7 ms | 60.8 ms |
| `storm_total` | 9,417.2 ms | 27.6 ms |
| `storm_per_file` | **47.1 ms** | 0.14 ms |

A small-file create went from ~15 s to 47.1 ms (320×). An earlier tree of the
same branch measured 59.0 ms in release, so the bead's 50 ms bound holds with
little margin and run-to-run noise of that size. About a third of the
remainder is four `F_FULLFSYNC` calls per accepted proposal (4.65 ms each on
this disk against 0.03 ms for a plain `fsync`); they carry the proposal's
power-loss durability, and replacing them with barriers and one flush is bead
`ley-line-open-98d0c3`.

## Derived paths (bead `ley-line-open-39998b`, 2026-10-23)

ADR-0045 T9 keeps build output out of the store: a path under `target/`,
`node_modules/` and the like is a scratch file, not a proposal. The benchmark
gained `derived_storm`: 2,000 create, write, close under `target/bench-storm/`
(on APFS, the same files in a plain directory). Release build, macOS 27.0.1,
Apple M3 Max (Mac15,11), 2,001 files, one run, with other builds on the
machine (load average ~33): `storm_per_file` measured 56.6 ms here against
47.1 ms above, so every guest figure in this run is inflated by the load.

| measurement | guest (share) | APFS |
|---|---:|---:|
| `open_read_close` | 246.9 µs/op | 47.4 µs/op |
| `lookup_miss` | 130.4 µs/op | 12.0 µs/op |
| `git_status_cold` | 2,216.8 ms | 654.3 ms |
| `git_status_warm_min` | 195.2 ms | 75.4 ms |
| `rg_first` | 394.6 ms | 115.2 ms |
| `rg_warm_min` | 266.5 ms | 108.8 ms |
| `storm_total` | 11,319.5 ms | 62.4 ms |
| `storm_per_file` (authored: one proposal per file) | 56.6 ms | 0.31 ms |
| `derived_storm_total` | 14,117.0 ms | 818.1 ms |
| `derived_storm_per_file` (scratch, no proposal) | **7.06 ms** | 0.41 ms |

A derived file costs an eighth of an authored one in the same run. In-process,
the same create, write, flush and release on a `StoreFs` with derived paths
(release build, 2,000 files, a lookup and a getattr per file) takes 0.9 ms, so
most of the 7.06 ms is the share's round trips: each FUSE request crosses
worker -> socket -> daemon (120-280 µs each). The agent-in-guest scenario (o)
wrote 300 build files at 3.9 ms each. Reaching APFS speed takes scratch inside
the guest (a guest-local overlay), which ADR-0045 T9 names and leaves out.
