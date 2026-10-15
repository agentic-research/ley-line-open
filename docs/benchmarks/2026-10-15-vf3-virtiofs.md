# V-F3: virtio-fs served from VMM memory vs APFS (2026-10-15)

ADR-0044 falsifier V-F3 ("faster than the host mounts"): the same metadata walk,
edit loop and small-file storm through (0) plain APFS directories and (1) a
libkrun guest whose working tree is a virtio-fs share answered by an in-memory
`DynFileSystem` in the VMM process. Harness: `spikes/vf3/` on branch
`spike/vf3-benchmark` (a standalone cargo workspace outside `rs/`, never built
by CI).

## Why there is no host NFS, fuse-t, macFUSE or FSKit leg

V-F3 as written compares against the host mounts. An earlier attempt at this
benchmark ran a loopback userspace NFS server and mounted it; that leg
**kernel-panicked the Mac** in Apple's `com.apple.filesystems.nfs` kext (bead
`ley-line-open-2d49ce`). fuse-t is built on the same NFS client, and macFUSE /
FSKit load or drive host kernel filesystem code. This run therefore measures
only paths that touch no host kernel filesystem driver other than APFS: APFS
itself, and a guest whose `/work` is served in-process over virtqueues (the
guest rootfs is libkrun's passthrough share of an APFS directory; nothing is
mounted on the host). The NFS/FUSE comparison stays open until it can run in a
disposable machine.

## Method

**Host.** Apple M3 Max (14 cores), 36 GiB, macOS 26.6.2 (25G83). Host tools:
`/usr/bin/git` 2.50.1 (Apple Git-155), ripgrep 15.1.0 (Homebrew), BSD `find`,
`wc`, bsdtar.

**VMM.** LLO's linked libkrun (`rs/third_party/libkrun`, upstream v1.19.4 +
LLO's custom-fs patch) at PR #464's head `bf64ca2d`, through
`leyline_runtime::backends::libkrun::api::{prepare_vm, PreparedVm::add_virtiofs_backend}`.
This branch adds READDIRPLUS forwarding to `DynFileSystem` (`dyn_fs.rs`, +29
lines; see "Source check"). Guest kernel: Homebrew libkrunfw 5.5.0 (Linux
6.12.91; dylib sha256 `1bd9cb1f…2f04`). Guest: 4 vCPUs, 2048 MiB, TSI off.
The VMM binary is ad-hoc signed with `com.apple.security.hypervisor`.

**Guest userland.** Alpine 3.24 aarch64 minirootfs plus packages, each
downloaded into its own empty directory, executed only inside the guest. The
Alpine binaries are musl-dynamic (Alpine ships no static git); the minirootfs
supplies musl and busybox (`sh`, `find`, `wc`, `tar`, `stat`).

| artifact | version | sha256 |
|---|---|---|
| alpine-minirootfs-3.24.2-aarch64.tar.gz | 3.24.2 (sha256 file verified) | `9bf70a7f18ea44094cbb5f70c58f9af129c8214745743db0e68e5502cc2ce773` |
| git-2.54.0-r0.apk | git 2.54.0 | `ea41e2f837f1f6a8b66e72a5ece0fc75cca44f76f49cf5500d467fc5627d0ab7` |
| ripgrep-15.1.0-r0.apk | ripgrep 15.1.0 | `1bf67cfa4464bc91e1dd1a4f537c40664c473ce92bf16ebba64bc7968d7545eb` |
| pcre2-10.49-r0.apk | 10.49 | `03efd0b29f7f38deb10a9ccb54f56e19683eb7ac27eb1f347e6dbd2e59ea000d` |
| zlib-1.3.2-r1.apk | 1.3.2 | `1f77a542ac6634761b9fe38d8bdca19b154071dcfa25432e73f2bc8a0740a33f` |
| libexpat-2.8.5-r0.apk | 2.8.5 | `568a12a4889a00711d1c80c197a5540ee1cdec26c1c59da38f8342ad69e7eff9` |
| libgcc-15.2.0-r5.apk | 15.2.0 | `369aaa6e9d099a737bad6dd3e6c2fe7bb1547ca26d22b94ee0411228f709b403` |

The workloads are one Rust program (`spikes/vf3/guest/vf3guest.rs`) built twice:
static aarch64-linux-musl for the guest (it mounts the share itself) and native
macOS for the APFS leg. Timing is taken inside the guest / on the host by the
program around each workload; VM boot and corpus load are outside the timer.
Before timing, the program runs each tool once outside the tree so a cold run
does not time the first read of the binaries.

**Corpus.** 20,857 working-tree files, 431 MB in memory with `.git`: this
repository's `rs/`, `docs/`, `clients/` (tracked files, no `target/`), mache's
tracked source, and real Rust crate sources from `~/.cargo/registry/src`
(alphabetical, until ≥20k files; symlinks, files >4 MiB and `.gitignore` files
removed). One commit, `git gc`. Repository config for both paths:
`core.checkStat=minimal`, `core.trustctime=false`, `core.excludesFile=/dev/null`,
untracked cache and fsmonitor off — so an index written on the host is clean in
the guest (the backend reports host mtimes and sizes; inode numbers differ).

**Backend (path 1).** `spikes/vf3/src/main.rs`, modelled on
`rs/ll-open/storefs` (ADR-0044 V2): Linux errno; OPEN takes a per-handle base
(the bytes and version the file held); WRITE stages on the handle; FLUSH and
FSYNC land the staged bytes only if the base still holds, else `ESTALE`;
RELEASE drops the handle. Beyond storefs, for the workloads: CREATE, MKDIR,
UNLINK, RMDIR, RENAME (incl. `RENAME_NOREPLACE`), SETATTR size (staged with a
handle, its own change without), mode and mtime. WRITE also checks the base
(the D question). A control file `/.vf3ctl` (never listed) lets the guest ask
"another writer" to append to a file, create a file, or refuse UNLINK/RENAME of
a path; it also prints the per-opcode request counts.

**Variants (path 1).**

| name | entry/attr timeout | READDIRPLUS | file data | DAX |
|---|---|---|---|---|
| ttl=0 | 0 | no | `DIRECT_IO` on every open (storefs README) | no |
| ttl=3600 | 3600 s, negative entries cached | no | `DIRECT_IO` | no |
| +READDIRPLUS | 3600 s | yes | `DIRECT_IO` | no |
| +cached reads | 3600 s | yes | `DIRECT_IO` for write opens; `KEEP_CACHE` for read-only opens; `CACHE_DIR` | no |
| +DAX | as above | yes | as above, plus `FUSE_ATTR_DAX` on corpus files, mount `dax=inode`, 8 GiB window | read-only mappings only (ADR-0044 V3) |

The writeback cache stays off in every variant (libkrun's server does not
offer it, `server.rs:895-905`, and ADR-0044 needs every WRITE at the backend).
DAX ran for workload A only: B and C write.

**Workloads.** N=5 per cell.

- A: `git status --porcelain`; `rg -j4 -c --no-messages ESTALE` over the tree
  (12 matching files); `sh -c 'find . -type f | wc -l'`.
- B: 1,000 × (open `O_RDWR`, read the whole file, flip one byte, `pwrite` at 0,
  `close`) on one 6 KB file; per-operation latency recorded.
- C: create 1,000 files of 128 bytes (`O_CREAT|O_EXCL`, write, close) in a new
  directory; extract an 8.5 MB, 1,000-file ustar tarball (staged in guest
  tmpfs / on APFS) with the platform `tar` (busybox / bsdtar).
- D: correctness probes, below.

**Cold and warm.** virtio-fs cold = a fresh guest boot per run (empty guest
dentry, inode and page caches; the backend is in memory, as the real one's
store is warm). virtio-fs warm = one boot, one unrecorded warm-up, then 5
recorded runs. APFS cold = a fresh `cp -c -R -p` clone of the corpus per run:
new vnodes, so file data misses the unified buffer cache, but the clone has
just written the metadata, so APFS's metadata B-tree nodes are cached. A fully
cold APFS needs `purge`, which needs root; it was not run. APFS warm = one
clone, one warm-up, 5 recorded runs. Percentiles are nearest-rank: over 5 runs
p99 is the slowest run. Per-operation percentiles pool all operations of all
runs.

## Verdict

V-F3 claims the guest path is faster than the host mounts. The host-mount legs
were not run (see above), so V-F3 itself stays open. Against the APFS floor:

- **virtio-fs is slower than APFS in every workload measured.** No variant
  matches APFS on a cold walk (2.7× to 4.7× on A) or on any write workload
  (3.9× to 9.0×).
- **A storefs-style backend with zero timeouts (the README's DIRECT_IO, no
  caching) is unusable for tree walks**: `find` 82×, `rg` 31×, `git status` 29×,
  untar 30× APFS warm. Every `stat` revalidates each path component: `find`
  issues 364k requests for 20,857 files.
- **With long timeouts, READDIRPLUS and page-cached read-only opens, warm
  read-only walks reach APFS**: `find` 1.0×, `git status` 1.5×, `rg` 2.3×.
- **The write path costs one round trip per FUSE request, ~33–39 µs each**, and
  ADR-0044 needs those requests (OPEN for the base, WRITE, FLUSH for the
  verdict). One edit is 7 requests (OPEN, GETATTR, 2× READ, WRITE, FLUSH,
  RELEASE): 232 µs against APFS's 25 µs. One small-file create is 6: 198 µs
  against 47 µs. tar adds 3 SETATTRs per file: untar is 8.3×. No backend
  setting moves these; the per-request latency of libkrun's virtio-fs transport
  on Hypervisor.framework does (`find` at ttl=0: 14.1 s / 364k requests =
  39 µs; edit at ttl=3600: 232 µs / 7 = 33 µs).
- **DAX does not help this tree.** Each mapping is a 2 MiB chunk per file;
  19,904 files need 19,911 SETUPMAPPINGs per `rg` pass (an 8 GiB window holds
  4,096 chunks, so warm passes remap everything), and `rg` warm is 1,261 ms with
  DAX against 524 ms with page-cached reads. `git status` and `find` read no
  file data through the mapping and are unchanged.

## Results

All times in ms unless stated; ratios are p50 against APFS p50 at the same
temperature.

#### A `git status`

| path | cold p50 ms | cold p99 ms | warm p50 ms | warm p99 ms | warm p50 / APFS | cold p50 / APFS |
|---|---|---|---|---|---|---|
| APFS (floor) | 131.0 | 137.9 | 117.3 | 117.7 | 1.0× | 1.0× |
| virtio-fs ttl=0 | 3457.9 | 3476.1 | 3422.0 | 3502.2 | 29.2× | 26.4× |
| virtio-fs ttl=3600 | 613.1 | 617.0 | 475.7 | 487.4 | 4.1× | 4.7× |
| ttl=3600 + READDIRPLUS | 634.8 | 645.7 | 477.9 | 484.7 | 4.1× | 4.8× |
| ttl=3600 + READDIRPLUS + cached reads | 613.5 | 632.0 | 175.4 | 188.0 | 1.5× | 4.7× |
| ttl=3600 + READDIRPLUS + cached reads + DAX | 640.1 | 653.1 | 169.3 | 184.8 | 1.4× | 4.9× |

#### A `rg` whole tree

| path | cold p50 ms | cold p99 ms | warm p50 ms | warm p99 ms | warm p50 / APFS | cold p50 / APFS |
|---|---|---|---|---|---|---|
| APFS (floor) | 831.5 | 853.0 | 224.9 | 244.8 | 1.0× | 1.0× |
| virtio-fs ttl=0 | 6990.3 | 7199.6 | 7028.9 | 7164.0 | 31.3× | 8.4× |
| virtio-fs ttl=3600 | 1754.2 | 1891.5 | 1311.9 | 1315.9 | 5.8× | 2.1× |
| ttl=3600 + READDIRPLUS | 1498.5 | 1545.8 | 1313.0 | 1336.3 | 5.8× | 1.8× |
| ttl=3600 + READDIRPLUS + cached reads | 1110.7 | 1160.4 | 524.3 | 527.4 | 2.3× | 1.3× |
| ttl=3600 + READDIRPLUS + cached reads + DAX | 1404.6 | 1409.2 | 1261.4 | 1274.6 | 5.6× | 1.7× |

#### A `find . -type f | wc -l`

| path | cold p50 ms | cold p99 ms | warm p50 ms | warm p99 ms | warm p50 / APFS | cold p50 / APFS |
|---|---|---|---|---|---|---|
| APFS (floor) | 179.8 | 196.6 | 173.1 | 174.9 | 1.0× | 1.0× |
| virtio-fs ttl=0 | 14190.1 | 14748.9 | 14138.4 | 14860.3 | 81.7× | 78.9× |
| virtio-fs ttl=3600 | 1453.8 | 1504.9 | 598.2 | 598.5 | 3.5× | 8.1× |
| ttl=3600 + READDIRPLUS | 485.9 | 498.8 | 478.4 | 483.3 | 2.8× | 2.7× |
| ttl=3600 + READDIRPLUS + cached reads | 492.8 | 512.0 | 180.7 | 183.2 | 1.0× | 2.7× |
| ttl=3600 + READDIRPLUS + cached reads + DAX | 486.9 | 500.9 | 184.3 | 191.3 | 1.1× | 2.7× |

#### B read/edit loop (1,000 ops)

| path | cold p50 ms | cold p99 ms | warm p50 ms | warm p99 ms | warm p50 / APFS | cold p50 / APFS | op p50 µs (cold / warm) | op p99 µs (cold / warm) |
|---|---|---|---|---|---|---|---|---|
| APFS (floor) | 26.1 | 26.9 | 25.9 | 26.4 | 1.0× | 1.0× | 24 / 25 | 41 / 30 |
| virtio-fs ttl=0 | 668.1 | 720.8 | 658.4 | 679.9 | 25.4× | 25.6× | 674 / 657 | 771 / 724 |
| virtio-fs ttl=3600 | 246.5 | 257.2 | 233.5 | 235.0 | 9.0× | 9.4× | 245 / 231 | 277 / 259 |
| ttl=3600 + READDIRPLUS | 243.9 | 253.6 | 239.1 | 242.4 | 9.2× | 9.3× | 242 / 237 | 276 / 264 |
| ttl=3600 + READDIRPLUS + cached reads | 241.1 | 251.7 | 234.5 | 234.7 | 9.1× | 9.2× | 239 / 232 | 271 / 261 |

#### C create 1,000 files

| path | cold p50 ms | cold p99 ms | warm p50 ms | warm p99 ms | warm p50 / APFS | cold p50 / APFS | op p50 µs (cold / warm) | op p99 µs (cold / warm) |
|---|---|---|---|---|---|---|---|---|
| APFS (floor) | 52.1 | 77.8 | 50.6 | 51.1 | 1.0× | 1.0× | 50 / 47 | 75 / 68 |
| virtio-fs ttl=0 | 359.9 | 371.3 | 355.0 | 358.5 | 7.0× | 6.9× | 357 / 352 | 399 / 389 |
| virtio-fs ttl=3600 | 201.9 | 207.2 | 199.9 | 205.2 | 3.9× | 3.9× | 199 / 198 | 228 / 223 |
| ttl=3600 + READDIRPLUS | 201.4 | 202.1 | 197.6 | 198.1 | 3.9× | 3.9× | 197 / 195 | 227 / 219 |
| ttl=3600 + READDIRPLUS + cached reads | 203.1 | 210.6 | 201.2 | 205.4 | 4.0× | 3.9× | 204 / 200 | 231 / 225 |

#### C untar 1,000 files

| path | cold p50 ms | cold p99 ms | warm p50 ms | warm p99 ms | warm p50 / APFS | cold p50 / APFS |
|---|---|---|---|---|---|---|
| APFS (floor) | 110.7 | 113.9 | 116.5 | 140.8 | 1.0× | 1.0× |
| virtio-fs ttl=0 | 3504.9 | 3540.5 | 3485.1 | 3507.6 | 29.9× | 31.7× |
| virtio-fs ttl=3600 | 969.2 | 1007.2 | 968.4 | 976.2 | 8.3× | 8.8× |
| ttl=3600 + READDIRPLUS | 964.0 | 988.3 | 959.7 | 960.1 | 8.2× | 8.7× |
| ttl=3600 + READDIRPLUS + cached reads | 959.3 | 977.2 | 964.0 | 975.4 | 8.3× | 8.7× |

#### FUSE requests the backend answered (one recorded run, median over runs)

| path | workload | temp | lookup | getattr | readdir | readdirplus | opendir | open | read | write | flush | release | create | setattr | rename | unlink | mkdir | dax_map |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| virtio-fs ttl=0 | status | cold | 155481 | 183474 | 7053 | 0 | 3505 | 14 | 45 | 1 | 15 | 15 | 1 | 0 | 0 | 1 | 0 | 0 |
| virtio-fs ttl=0 | status | warm | 155481 | 183474 | 7053 | 0 | 3505 | 14 | 45 | 1 | 15 | 15 | 1 | 0 | 0 | 1 | 0 | 0 |
| virtio-fs ttl=3600 | status | cold | 25699 | 9 | 7053 | 0 | 3505 | 14 | 45 | 1 | 15 | 15 | 1 | 0 | 0 | 1 | 0 | 0 |
| virtio-fs ttl=3600 | status | warm | 2 | 3527 | 7053 | 0 | 3505 | 14 | 45 | 1 | 15 | 15 | 1 | 0 | 0 | 1 | 0 | 0 |
| ttl=3600 + READDIRPLUS | status | cold | 25697 | 9 | 0 | 7265 | 3505 | 14 | 45 | 1 | 15 | 15 | 1 | 0 | 0 | 1 | 0 | 0 |
| ttl=3600 + READDIRPLUS | status | warm | 2 | 3526 | 0 | 7265 | 3505 | 14 | 45 | 1 | 15 | 15 | 1 | 0 | 0 | 1 | 0 | 0 |
| ttl=3600 + READDIRPLUS + cached reads | status | cold | 25697 | 5 | 0 | 7265 | 3505 | 14 | 31 | 1 | 15 | 15 | 1 | 0 | 0 | 1 | 0 | 0 |
| ttl=3600 + READDIRPLUS + cached reads | status | warm | 2 | 3 | 0 | 0 | 3505 | 14 | 0 | 1 | 15 | 15 | 1 | 0 | 0 | 1 | 0 | 0 |
| ttl=3600 + READDIRPLUS + cached reads + DAX | status | cold | 25697 | 2 | 0 | 7265 | 3505 | 14 | 0 | 1 | 15 | 15 | 1 | 0 | 0 | 1 | 0 | 9 |
| ttl=3600 + READDIRPLUS + cached reads + DAX | status | warm | 2 | 3 | 0 | 0 | 3505 | 14 | 0 | 1 | 15 | 15 | 1 | 0 | 0 | 1 | 0 | 0 |
| virtio-fs ttl=0 | rg | cold | 237891 | 284056 | 6607 | 0 | 3282 | 19904 | 61723 | 1 | 19904 | 19904 | 0 | 0 | 0 | 0 | 0 | 0 |
| virtio-fs ttl=0 | rg | warm | 237891 | 284056 | 6607 | 0 | 3282 | 19904 | 61770 | 1 | 19904 | 19904 | 0 | 0 | 0 | 0 | 0 | 0 |
| virtio-fs ttl=3600 | rg | cold | 39596 | 1 | 6607 | 0 | 3282 | 19904 | 61712 | 1 | 19904 | 19904 | 0 | 0 | 0 | 0 | 0 | 0 |
| virtio-fs ttl=3600 | rg | warm | 1 | 2 | 6607 | 0 | 3282 | 19904 | 61914 | 1 | 19904 | 19904 | 0 | 0 | 0 | 0 | 0 | 0 |
| ttl=3600 + READDIRPLUS | rg | cold | 16412 | 1 | 0 | 6819 | 3282 | 19904 | 61985 | 1 | 19904 | 19904 | 0 | 0 | 0 | 0 | 0 | 0 |
| ttl=3600 + READDIRPLUS | rg | warm | 1 | 2 | 0 | 6819 | 3282 | 19904 | 61909 | 1 | 19904 | 19904 | 0 | 0 | 0 | 0 | 0 | 0 |
| ttl=3600 + READDIRPLUS + cached reads | rg | cold | 16412 | 1 | 0 | 6819 | 3282 | 19904 | 26640 | 1 | 19904 | 19904 | 0 | 0 | 0 | 0 | 0 | 0 |
| ttl=3600 + READDIRPLUS + cached reads | rg | warm | 1 | 1 | 0 | 0 | 3282 | 19904 | 0 | 1 | 19904 | 19904 | 0 | 0 | 0 | 0 | 0 | 0 |
| ttl=3600 + READDIRPLUS + cached reads + DAX | rg | cold | 16412 | 1 | 0 | 6819 | 3282 | 19904 | 0 | 1 | 19904 | 19904 | 0 | 0 | 0 | 0 | 0 | 19911 |
| ttl=3600 + READDIRPLUS + cached reads + DAX | rg | warm | 1 | 1 | 0 | 0 | 3282 | 19904 | 0 | 1 | 19904 | 19904 | 0 | 0 | 0 | 0 | 0 | 19911 |
| virtio-fs ttl=0 | find | cold | 148979 | 204781 | 7070 | 0 | 3515 | 1 | 0 | 1 | 1 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| virtio-fs ttl=0 | find | warm | 148979 | 204781 | 7070 | 0 | 3515 | 1 | 0 | 1 | 1 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| virtio-fs ttl=3600 | find | cold | 24385 | 1 | 7070 | 0 | 3515 | 1 | 0 | 1 | 1 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| virtio-fs ttl=3600 | find | warm | 1 | 3516 | 7070 | 0 | 3515 | 1 | 0 | 1 | 1 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| ttl=3600 + READDIRPLUS | find | cold | 1 | 1 | 0 | 7282 | 3515 | 1 | 0 | 1 | 1 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| ttl=3600 + READDIRPLUS | find | warm | 1 | 2 | 0 | 7282 | 3515 | 1 | 0 | 1 | 1 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| ttl=3600 + READDIRPLUS + cached reads | find | cold | 1 | 1 | 0 | 7282 | 3515 | 1 | 0 | 1 | 1 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| ttl=3600 + READDIRPLUS + cached reads | find | warm | 1 | 1 | 0 | 0 | 3515 | 1 | 0 | 1 | 1 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| ttl=3600 + READDIRPLUS + cached reads + DAX | find | cold | 1 | 1 | 0 | 7282 | 3515 | 1 | 0 | 1 | 1 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| ttl=3600 + READDIRPLUS + cached reads + DAX | find | warm | 1 | 1 | 0 | 0 | 3515 | 1 | 0 | 1 | 1 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| virtio-fs ttl=0 | edit | cold | 5001 | 7002 | 0 | 0 | 0 | 1001 | 2000 | 1001 | 1001 | 1001 | 0 | 0 | 0 | 0 | 0 | 0 |
| virtio-fs ttl=0 | edit | warm | 5001 | 7002 | 0 | 0 | 0 | 1001 | 2000 | 1001 | 1001 | 1001 | 0 | 0 | 0 | 0 | 0 | 0 |
| virtio-fs ttl=3600 | edit | cold | 6 | 1000 | 0 | 0 | 0 | 1001 | 2000 | 1001 | 1001 | 1001 | 0 | 0 | 0 | 0 | 0 | 0 |
| virtio-fs ttl=3600 | edit | warm | 1 | 1001 | 0 | 0 | 0 | 1001 | 2000 | 1001 | 1001 | 1001 | 0 | 0 | 0 | 0 | 0 | 0 |
| ttl=3600 + READDIRPLUS | edit | cold | 6 | 1000 | 0 | 0 | 0 | 1001 | 2000 | 1001 | 1001 | 1001 | 0 | 0 | 0 | 0 | 0 | 0 |
| ttl=3600 + READDIRPLUS | edit | warm | 1 | 1001 | 0 | 0 | 0 | 1001 | 2000 | 1001 | 1001 | 1001 | 0 | 0 | 0 | 0 | 0 | 0 |
| ttl=3600 + READDIRPLUS + cached reads | edit | cold | 6 | 1000 | 0 | 0 | 0 | 1001 | 2000 | 1001 | 1001 | 1001 | 0 | 0 | 0 | 0 | 0 | 0 |
| ttl=3600 + READDIRPLUS + cached reads | edit | warm | 1 | 1001 | 0 | 0 | 0 | 1001 | 2000 | 1001 | 1001 | 1001 | 0 | 0 | 0 | 0 | 0 | 0 |
| virtio-fs ttl=0 | create | cold | 2002 | 4004 | 0 | 0 | 0 | 1 | 0 | 1001 | 1001 | 1001 | 1000 | 0 | 0 | 0 | 1 | 0 |
| virtio-fs ttl=0 | create | warm | 2002 | 4004 | 0 | 0 | 0 | 1 | 0 | 1001 | 1001 | 1001 | 1000 | 0 | 0 | 0 | 1 | 0 |
| virtio-fs ttl=3600 | create | cold | 1002 | 1001 | 0 | 0 | 0 | 1 | 0 | 1001 | 1001 | 1001 | 1000 | 0 | 0 | 0 | 1 | 0 |
| virtio-fs ttl=3600 | create | warm | 1002 | 1001 | 0 | 0 | 0 | 1 | 0 | 1001 | 1001 | 1001 | 1000 | 0 | 0 | 0 | 1 | 0 |
| ttl=3600 + READDIRPLUS | create | cold | 1002 | 1001 | 0 | 0 | 0 | 1 | 0 | 1001 | 1001 | 1001 | 1000 | 0 | 0 | 0 | 1 | 0 |
| ttl=3600 + READDIRPLUS | create | warm | 1002 | 1001 | 0 | 0 | 0 | 1 | 0 | 1001 | 1001 | 1001 | 1000 | 0 | 0 | 0 | 1 | 0 |
| ttl=3600 + READDIRPLUS + cached reads | create | cold | 1002 | 1001 | 0 | 0 | 0 | 1 | 0 | 1001 | 1001 | 1001 | 1000 | 0 | 0 | 0 | 1 | 0 |
| ttl=3600 + READDIRPLUS + cached reads | create | warm | 1002 | 1001 | 0 | 0 | 0 | 1 | 0 | 1001 | 1001 | 1001 | 1000 | 0 | 0 | 0 | 1 | 0 |
| virtio-fs ttl=0 | untar | cold | 33050 | 38120 | 0 | 0 | 0 | 1 | 0 | 1015 | 1001 | 1001 | 1000 | 3000 | 0 | 0 | 215 | 0 |
| virtio-fs ttl=0 | untar | warm | 33050 | 38120 | 0 | 0 | 0 | 1 | 0 | 1015 | 1001 | 1001 | 1000 | 3000 | 0 | 0 | 215 | 0 |
| virtio-fs ttl=3600 | untar | cold | 5068 | 1216 | 0 | 0 | 0 | 1 | 0 | 1015 | 1001 | 1001 | 1000 | 3000 | 0 | 0 | 215 | 0 |
| virtio-fs ttl=3600 | untar | warm | 5068 | 1216 | 0 | 0 | 0 | 1 | 0 | 1015 | 1001 | 1001 | 1000 | 3000 | 0 | 0 | 215 | 0 |
| ttl=3600 + READDIRPLUS | untar | cold | 5068 | 1216 | 0 | 0 | 0 | 1 | 0 | 1015 | 1001 | 1001 | 1000 | 3000 | 0 | 0 | 215 | 0 |
| ttl=3600 + READDIRPLUS | untar | warm | 5068 | 1216 | 0 | 0 | 0 | 1 | 0 | 1015 | 1001 | 1001 | 1000 | 3000 | 0 | 0 | 215 | 0 |
| ttl=3600 + READDIRPLUS + cached reads | untar | cold | 5068 | 1216 | 0 | 0 | 0 | 1 | 0 | 1015 | 1001 | 1001 | 1000 | 3000 | 0 | 0 | 215 | 0 |
| ttl=3600 + READDIRPLUS + cached reads | untar | warm | 5068 | 1216 | 0 | 0 | 0 | 1 | 0 | 1015 | 1001 | 1001 | 1000 | 3000 | 0 | 0 | 215 | 0 |

#### Raw wall times (ms, run order)

```text
status  apfs      cold  137.9 130.5 132.6 131.0 129.5
status  apfs      warm  117.7 116.6 117.5 117.3 115.2
status  vf-t0     cold  3476.1 3457.9 3468.0 3365.4 3418.6
status  vf-t0     warm  3502.2 3418.3 3391.0 3422.0 3434.2
status  vf-t3600  cold  613.1 617.0 616.1 574.5 595.4
status  vf-t3600  warm  487.4 480.3 475.7 472.3 474.8
status  vf-plus   cold  605.6 634.8 645.7 582.0 637.3
status  vf-plus   warm  477.9 481.3 476.1 484.7 475.0
status  vf-cache  cold  613.5 607.7 632.0 604.8 628.5
status  vf-cache  warm  188.0 177.4 169.2 150.1 175.4
status  vf-dax    cold  640.1 647.9 618.0 617.1 653.1
status  vf-dax    warm  184.8 142.5 169.3 168.3 171.1
rg      apfs      cold  847.5 853.0 826.8 831.5 831.1
rg      apfs      warm  244.8 216.2 218.2 224.9 243.1
rg      vf-t0     cold  6878.9 6832.9 7199.6 7007.9 6990.3
rg      vf-t0     warm  7144.3 7164.0 7028.9 6977.6 6981.5
rg      vf-t3600  cold  1763.8 1740.7 1891.5 1754.2 1737.6
rg      vf-t3600  warm  1311.9 1303.9 1312.7 1315.9 1306.6
rg      vf-plus   cold  1498.5 1496.8 1545.8 1528.6 1471.5
rg      vf-plus   warm  1336.3 1309.3 1315.8 1313.0 1309.2
rg      vf-cache  cold  1092.4 1106.7 1160.4 1110.7 1126.8
rg      vf-cache  warm  522.7 524.3 520.8 527.4 526.1
rg      vf-dax    cold  1398.3 1404.6 1408.4 1398.2 1409.2
rg      vf-dax    warm  1194.1 1253.0 1261.4 1262.3 1274.6
find    apfs      cold  181.8 179.8 196.6 178.8 177.7
find    apfs      warm  173.1 171.9 173.5 174.9 172.9
find    vf-t0     cold  14190.1 14229.1 14748.9 14128.3 14128.2
find    vf-t0     warm  14860.3 14816.1 14132.8 14107.2 14138.4
find    vf-t3600  cold  1448.0 1453.8 1504.9 1461.6 1431.4
find    vf-t3600  warm  598.5 597.0 598.2 596.0 598.2
find    vf-plus   cold  485.9 480.3 498.8 485.5 490.4
find    vf-plus   warm  483.3 478.4 479.9 477.2 477.0
find    vf-cache  cold  492.8 486.8 512.0 493.3 490.3
find    vf-cache  warm  183.2 181.4 149.9 180.6 180.7
find    vf-dax    cold  494.3 500.9 484.0 485.4 486.9
find    vf-dax    warm  191.3 171.3 184.3 186.2 184.1
edit    apfs      cold  26.0 26.9 25.7 26.7 26.1
edit    apfs      warm  25.5 25.2 25.9 26.4 25.9
edit    vf-t0     cold  665.1 697.7 720.8 668.1 665.3
edit    vf-t0     warm  658.4 655.1 657.4 659.4 679.9
edit    vf-t3600  cold  239.8 252.3 257.2 246.5 240.4
edit    vf-t3600  warm  232.9 235.0 233.7 232.9 233.5
edit    vf-plus   cold  253.6 249.4 242.7 243.9 238.4
edit    vf-plus   warm  236.6 240.9 239.1 238.9 242.4
edit    vf-cache  cold  229.6 241.1 251.7 242.2 239.6
edit    vf-cache  warm  234.5 234.7 234.4 234.2 234.7
create  apfs      cold  52.1 77.8 52.1 50.5 51.5
create  apfs      warm  51.1 50.6 51.1 50.1 50.1
create  vf-t0     cold  365.5 371.3 356.2 357.3 359.9
create  vf-t0     warm  358.2 358.5 355.0 352.9 354.0
create  vf-t3600  cold  207.2 202.8 201.9 201.8 198.3
create  vf-t3600  warm  194.1 193.9 199.9 205.2 203.2
create  vf-plus   cold  134.3 201.6 202.1 199.0 201.4
create  vf-plus   warm  197.8 197.1 197.2 198.1 197.6
create  vf-cache  cold  210.5 210.6 200.9 199.7 203.1
create  vf-cache  warm  201.2 201.9 201.1 200.5 205.4
untar   apfs      cold  112.4 113.9 110.7 110.0 109.9
untar   apfs      warm  140.8 111.3 114.8 116.9 116.5
untar   vf-t0     cold  3489.8 3540.5 3412.4 3524.6 3504.9
untar   vf-t0     warm  3477.3 3507.6 3502.5 3485.1 3474.5
untar   vf-t3600  cold  969.2 1007.2 987.8 955.8 958.0
untar   vf-t3600  warm  976.2 974.3 968.4 965.1 965.2
untar   vf-plus   cold  969.1 964.0 962.0 948.8 988.3
untar   vf-plus   warm  956.8 959.7 959.8 960.1 957.5
untar   vf-cache  cold  958.8 975.5 956.9 959.3 977.2
untar   vf-cache  warm  975.4 964.0 939.5 964.1 962.6
```

#### D (correctness) lines as the guest printed them

```text
D vf-cache-correct D1 open,read,FOREIGN,write,fsync,close: write=ESTALE(116) fsync=ok close=ok store=Ok("line one|changed by another writer|")
D vf-cache-correct D2 open,write,FOREIGN,fsync,close: write=ok fsync=ESTALE(116) close=ESTALE(116) store=Ok("line one|changed by another writer|")
D vf-cache-correct D3 open,write,FOREIGN,close: write=ok close=ESTALE(116) store=Ok("line one|changed by another writer|")
D vf-cache-correct D4 refused namespace ops: rename=ESTALE(116) (d4 exists=true, d4.renamed exists=false) unlink=ESTALE(116) (d4 exists=true)
D vf-cache-correct D5 foreign append to llo/docs/adr/0014-capnp-as-protocol.md: before stat=18156 statx=Ok(18156) read=Ok((18156, "alization-not-canonical/>|")) | after stat=18156 statx=Ok(18156) read=Ok((18156, "alization-not-canonical/>|"))
D vf-cache-correct D7 foreign create after a negative lookup: stat=ENOENT(2) listed_by_readdir=true
D vf-cache-correct D6 FOREIGN,open,write,fsync,close: write=ok fsync=ok close=ok store=Ok("guest edit|anged by another writer|")
D vf-plus-correct D1 open,read,FOREIGN,write,fsync,close: write=ESTALE(116) fsync=ok close=ok store=Ok("line one|changed by another writer|")
D vf-plus-correct D2 open,write,FOREIGN,fsync,close: write=ok fsync=ESTALE(116) close=ESTALE(116) store=Ok("line one|changed by another writer|")
D vf-plus-correct D3 open,write,FOREIGN,close: write=ok close=ESTALE(116) store=Ok("line one|changed by another writer|")
D vf-plus-correct D4 refused namespace ops: rename=ESTALE(116) (d4 exists=true, d4.renamed exists=false) unlink=ESTALE(116) (d4 exists=true)
D vf-plus-correct D5 foreign append to llo/docs/adr/0014-capnp-as-protocol.md: before stat=18156 statx=Ok(18156) read=Ok((18156, "alization-not-canonical/>|")) | after stat=18156 statx=Ok(18156) read=Ok((18182, "changed by another writer|"))
D vf-plus-correct D7 foreign create after a negative lookup: stat=ENOENT(2) listed_by_readdir=true
D vf-plus-correct D6 FOREIGN,open,write,fsync,close: write=ok fsync=ok close=ok store=Ok("guest edit|anged by another writer|")
D vf-t0-correct D1 open,read,FOREIGN,write,fsync,close: write=ESTALE(116) fsync=ok close=ok store=Ok("line one|changed by another writer|")
D vf-t0-correct D2 open,write,FOREIGN,fsync,close: write=ok fsync=ESTALE(116) close=ESTALE(116) store=Ok("line one|changed by another writer|")
D vf-t0-correct D3 open,write,FOREIGN,close: write=ok close=ESTALE(116) store=Ok("line one|changed by another writer|")
D vf-t0-correct D4 refused namespace ops: rename=ESTALE(116) (d4 exists=true, d4.renamed exists=false) unlink=ESTALE(116) (d4 exists=true)
D vf-t0-correct D5 foreign append to llo/docs/adr/0014-capnp-as-protocol.md: before stat=18156 statx=Ok(18156) read=Ok((18156, "alization-not-canonical/>|")) | after stat=18182 statx=Ok(18182) read=Ok((18182, "changed by another writer|"))
D vf-t0-correct D7 foreign create after a negative lookup: stat=ok(26 bytes) listed_by_readdir=true
D vf-t0-correct D6 FOREIGN,open,write,fsync,close: write=ok fsync=ok close=ok store=Ok("guest edit|anged by another writer|")
D vf-t3600-correct D1 open,read,FOREIGN,write,fsync,close: write=ESTALE(116) fsync=ok close=ok store=Ok("line one|changed by another writer|")
D vf-t3600-correct D2 open,write,FOREIGN,fsync,close: write=ok fsync=ESTALE(116) close=ESTALE(116) store=Ok("line one|changed by another writer|")
D vf-t3600-correct D3 open,write,FOREIGN,close: write=ok close=ESTALE(116) store=Ok("line one|changed by another writer|")
D vf-t3600-correct D4 refused namespace ops: rename=ESTALE(116) (d4 exists=true, d4.renamed exists=false) unlink=ESTALE(116) (d4 exists=true)
D vf-t3600-correct D5 foreign append to llo/docs/adr/0014-capnp-as-protocol.md: before stat=18156 statx=Ok(18156) read=Ok((18156, "alization-not-canonical/>|")) | after stat=18156 statx=Ok(18156) read=Ok((18182, "changed by another writer|"))
D vf-t3600-correct D7 foreign create after a negative lookup: stat=ENOENT(2) listed_by_readdir=true
D vf-t3600-correct D6 FOREIGN,open,write,fsync,close: write=ok fsync=ok close=ok store=Ok("guest edit|anged by another writer|")
```

## D: which syscall carries a refusal

Identical in all four variants (ttl=0, ttl=3600, +READDIRPLUS, +cached reads);
writes are `DIRECT_IO` in every variant and the writeback cache is off.

| sequence | backend refuses at | syscall → errno | store afterwards |
|---|---|---|---|
| D1 open, read, *foreign change*, write, fsync, close | WRITE (base check) | `write()` → `ESTALE` (116); `fsync()` → 0, `close()` → 0 (nothing staged) | foreign bytes only |
| D2 open, write, *foreign change*, fsync, close | FSYNC, then FLUSH | `write()` → 0; `fsync()` → `ESTALE`; `close()` → `ESTALE` | foreign bytes only |
| D3 open, write, *foreign change*, close | FLUSH | `close()` → `ESTALE` | foreign bytes only |
| D4 rename / unlink of a refused path | RENAME / UNLINK | `rename()` → `ESTALE`, `unlink()` → `ESTALE`, on the syscall itself; the name stays, the target is not created | unchanged |
| D6 *foreign change*, open, write, fsync, close | — | all 0: OPEN takes the new base | guest bytes over the foreign version |

So: a refusal at WRITE returns from `write()`; a refusal the backend can only
make at commit returns from `fsync()` and again from `close()`; namespace
refusals return from `rename()`/`unlink()` directly. The guest kernel sends a
refused RENAME and UNLINK **twice** (the backend logged each refusal twice):
Linux's VFS retries a path walk once on `ESTALE` with revalidation. A backend
that refuses with `ESTALE` must give the same answer to the retry.

**Visibility without invalidation (D5, D7).** Another writer appends 26 bytes
to a corpus file whose attributes the guest holds, then creates a file whose
name the guest looked up and missed:

| variant | `stat` size after the append | fresh open+read after the append | `stat` of the new name |
|---|---|---|---|
| ttl=0 | new (18182) | new bytes | found |
| ttl=3600 | **old (18156)** | new bytes (DIRECT_IO: OPEN and READ reach the backend) | **ENOENT** (cached negative entry); `readdir` lists it |
| +READDIRPLUS | **old** | new bytes | **ENOENT**; `readdir` lists it |
| +cached reads | **old** | **old bytes** (page cache, `KEEP_CACHE`) | **ENOENT**; `readdir` lists it |

Long timeouts never weaken the write guarantee: OPEN always reaches the
backend, so the base is current, and WRITE/FLUSH/FSYNC check it. They weaken
freshness: the guest sees another writer's change only when the timeout
expires, because nothing can push an invalidation (next section). A read
invalidates the cached atime, so the first `stat` after a read refetches
attributes; the probe re-stats before the foreign change to measure the
cached state.

## Source check (`rs/third_party/libkrun`, v1.19.4 + LLO patch)

**Backend → guest invalidation (FUSE_NOTIFY_INVAL_INODE / INVAL_ENTRY): not
supported.**

- `src/devices/src/virtio/fs/fuse.rs:715-724` defines `NotifyOpcode`
  (`InvalInode = 2`, `InvalEntry = 3`) and `fuse.rs:1179-1193` the
  `NotifyInvalInodeOut` / `NotifyInvalEntryOut` structs. Nothing else in the
  tree references them.
- `src/devices/src/virtio/fs/device.rs:74`: the device offers only
  `VIRTIO_F_VERSION_1 | VIRTIO_RING_F_EVENT_IDX`, not
  `VIRTIO_FS_F_NOTIFICATION`; `src/devices/src/virtio/fs/mod.rs:40-42`
  configures two queues (hiprio + one request queue) and no notification
  queue.
- `src/devices/src/virtio/fs/filesystem.rs:1202-1205`: `notify_reply` is
  `ENOSYS` ("TODO: support this"); `server.rs:140` dispatches it there.
- The guest side has no receiver either: Linux 6.12's `fs/fuse/virtio_fs.c`
  sets up the hiprio and request queues only (the notification-queue series
  was not merged upstream). This is from the kernel source, not verified
  against a checkout in this run.

**READDIRPLUS: the server supports it; the `DynFileSystem` patch did not
expose it.**

- `server.rs:143` dispatches `Opcode::Readdirplus`; `server.rs:1038` →
  `do_readdir(.., plus = true)` (`server.rs:967`), which calls
  `FileSystem::readdirplus` at `server.rs:999`.
- `filesystem.rs:1032-1043`: the trait default is `ENOSYS`; the kernel sends it
  only when `init` returns `FsOptions::DO_READDIRPLUS` (`server.rs:895-905`
  does not include it in the server's own `supported` set).
- At PR #464's head, `dyn_fs.rs` has `readdir` (`:183`, forwarded at `:481`)
  and no `readdirplus`, so `DynFs` fell back to the `ENOSYS` default. This
  branch adds `DynFileSystem::readdirplus` (`dyn_fs.rs:197`) and the forward
  (`dyn_fs.rs:510`), 29 lines.
- The writeback cache is not offered: `WRITEBACK_CACHE` (`fuse.rs:124`) is
  absent from `server.rs:895-905` and the spike backend does not request it.

## What LLO's backend must do to be competitive

1. **Return long entry and attribute timeouts, with negative entries.** ttl=0
   costs 26–82× APFS on walks; ttl=3600 removes 67–99% of LOOKUP+GETATTR
   traffic. The write guarantee does not depend on the timeout (OPEN, WRITE,
   FLUSH, FSYNC always reach the backend). Freshness does: with no
   invalidation channel (above), another writer's change is invisible to
   `stat`/LOOKUP until the timeout expires. Choose the timeout per what may
   change underneath the guest: immutable snapshot content can take hours; a
   tree other writers edit needs a short timeout (not measured here) or a
   notification path added to libkrun and the guest kernel.
2. **Implement READDIRPLUS** (this branch's `dyn_fs.rs` change, or an
   equivalent upstream): cold `find` 1,454 → 486 ms, cold `rg` 1,754 → 1,499 ms.
3. **Serve read-only opens through the page cache** (`KEEP_CACHE` without
   `DIRECT_IO`, `CACHE_DIR` on directories), keeping `DIRECT_IO` on write
   opens. Largest warm win: `git status` 478 → 175 ms, `rg` 1,313 → 524 ms,
   `find` 478 → 181 ms. Its cost is D5's stale bytes. Content addressing
   offers a fix this run did not measure: return `KEEP_CACHE` only when the
   file's blob is the one the previous open served, and omit it otherwise so
   the kernel drops the cached pages at open (open-to-close consistency
   without notifications).
4. **Leave DAX off for source trees.** 2 MiB per-file mappings cost one
   SETUPMAPPING per file and thrash the window; page-cached reads are 2.4×
   faster on `rg`. DAX remains a candidate for large immutable blobs only.
5. **Keep the writeback cache off and DIRECT_IO on write opens**: D depends on
   every WRITE reaching the backend. The remaining 4–9× on B and C is the
   per-request transport cost (~33–39 µs), owned by libkrun's virtio-fs
   transport (one request queue, `mod.rs:40`), not by the backend.

## Caveats

- No host NFS / FUSE leg (kernel panic, above). V-F3's comparison against the
  host mounts is not answered.
- APFS cold is a fresh clone, not a purged cache (no root). Its data reads are
  cold; its metadata is warm. The guest's cold runs are colder than APFS's.
- The tools differ by platform: git 2.54.0 (guest) vs Apple git 2.50.1 (host);
  busybox `find`/`tar` (guest; busybox `find` lstat()s every entry) vs BSD
  `find`/bsdtar (host); ripgrep 15.1.0 on both, `-j4` on both. The guest has
  4 vCPUs; git on the host uses the host's 14 cores for index preload.
- The backend is in memory and single-locked; the store-backed backend adds
  SQLite reads and proposal cost on top of the transport cost measured here.
- The control file's own requests (one OPEN, WRITE, FLUSH, RELEASE per
  `reset`/`stats`) are included in the request counts.
- N=5: p99 is the slowest run. Per-operation p50/p99 (B, C create) pool 5,000
  operations.

## Reproduce

```sh
spikes/vf3/prepare.sh <scratch>          # downloads, corpus, rootfs, builds, signs
spikes/vf3/run.sh <scratch> <scratch>/results 5
python3 -I spikes/vf3/summarize.py <scratch>/results
```

Raw logs of this run: `spikes/vf3/results/` (`*.out`: RESULT / OPS / D lines;
`*.err`: backend request counts `VF3OPS`). The D probes in `results/d/` come
from a second pass after D5 gained the re-stat and D7 was added.
