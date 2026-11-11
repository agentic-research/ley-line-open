# Roadmap — ley-line-open

This file states where LLO is going and in what order. Beads are the work ledger:
each line here names the bead that owns it, and a bead's acceptance criteria are
the definition of done. When a bead closes or the plan changes, update this file
in the same pull request.

Last updated: 2026-11-11, at the `integration` tree `9d41b225` (latest tag
v0.29.0, plus #566, #562 and #568).

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
| Portable, proven | The in-guest end-to-end test gates every release on Apple silicon (a manual run recorded as a per-tree receipt that tagging requires, `16a994`) and runs on Linux/KVM; the released macOS worker carries the hypervisor entitlement (`984acf`) | `5c2040` (`16a994`, `984acf` done in v0.21.0) |
| Stable contracts | The `propose` wire, the projection ABI (`_tree`, `_source`, `_parse_diagnostics`), execution/v1 and the share header are frozen under semver, with fixtures that fail on a breaking change; the governing ADRs have a decided status | `b70e44`, `10075e` |
| Write path for real workloads | Build output stays out of the store; bursts of authored writes commit as one batch; a write is acknowledged after one durable append; a guest write stays under 50 ms | `39998b`, `18bfdd`, `98d0c3` |
| Fail-closed authority | A run writes only inside its grant's write scope; a backend refuses a workspace it cannot serve | `483134` (`204a94` done in v0.26.0) |
| Installable | The crates publish to crates.io; a release is a tag, not a git revision | `c3e8c6`, `395e43` |
| Git compatible at the boundary | Real `git` (status, diff, commit, log, blame, push, fetch) behaves in an agent's working tree as in a plain clone; git object ids are an export-only identity domain | `cf8fdb` |
| Limits fixed or documented | Two writers on one NFS mount; `.git` in the mount | `5ec040`, `5ee38a` |

Out of v1: the remote or multi-host store (`a7b9d9`), blob-mapped guest reads
(`e0893c`) unless they prove a small change, construct-aware merge (`5ade64`).

## Order of work

**Now**
- The follow-up gap queue (epic `ley-line-open-1262af`): every open design or
  correctness follow-up from the v0.26 to v0.29 releases is worked to zero, in
  groups whose files do not overlap. A pull request files a follow-up only for
  work outside its scope, and schedules it into this queue the same day.
  - **A (tooling, daemon, identity documents and Go): done** in v0.29.0
    (#558, #563, #564; see below).
  - **C (execution service).** `7a3109` is merged (#566, after v0.29.0): a
    worker compares the confinement it will apply with the grant before the
    workload starts. Open: `483134` (write scope), `1022a8` (the service
    materializes workspace shares before spawn), `e32a50` (duplicate
    `workspaceInputs`), `e592ef` (cancel of a finished run), `f7bcb3` (a
    worker-event channel the guest cannot write), `eea8e1` (a run's
    `GuestOutput` on the execution-daemon socket).
  - **D (image import and storage).** Merged: `38ff59` (#562: the per-run
    rootfs holds only the directories the manifest implies, at `0755`) and
    D1 (#568: `b1c18d`, `167b29`, `1665c9`, `168a65`, `2aa5d3`, `cffbb5`).
    Open: `4f08ea` (rootfs manifest v2: directories with modes, and symbolic
    links), `faba8f` (canonical manifest bytes are checked) and `18a7a2`.
  - **B (guest and libkrun).** `d4b07f` with `f7f862` first: argv and the
    environment reach the guest init through a channel nothing parses, so the
    kernel command line carries fixed tokens only. Then the loopback relay
    (`ebc78e`, `bb3395`, `156d83`, `b00ae3`), the evidence gaps (`eba800`,
    `3839c9`, `eb8d81`) and `eb3411`.
  - **E (parse and identity).** `cc56e6`, `0ed720`, `1e64c4`, `1e76ba`,
    `1b5f3c`, `fa9ec8`, `ff385b`, and ADR-0047 M3 (`ab2093`, breaking: the
    cloister issue is filed first).
  - Waiting for a Linux/KVM host: `381b1d` (guest UDP egress under TSI hijack
    on Linux). Until then Linux keeps TSI hijack off, and nothing claims Linux
    microVM evidence.
- For mache: `69305e`, `view(family, target, at)`. The daemon computes a
  restriction-addressed key for a derived view from a dependency predicate the
  consumer sends (names, import sites, a file), so a view is recomputed only
  when a row it read changes. `leyline-cache` (v0.25.0) holds the key
  construction; `1b4dee` reruns its measurement on the production identity
  (`node_hash` containers, the real fact tables) before the daemon operation
  claims a benefit. mache-6f92d0 is blocked on it. mache adopts
  `projection-v13` under mache-1e3d11.

**Next**
- `18bfdd` — group commit: writes that land close together become one proposal,
  one parse pass, one durable flush and one publish, with each file keeping its
  own base, merge and `ESTALE`.
- `483134` (group C above) — `WorkspaceGrant` gains a write scope (exact paths
  and directory prefixes); storefs refuses writes outside it.
- `5c2040` — VM tests on Linux/KVM. Apple silicon is a manual release gate
  instead of CI (hosted macOS runners lack nested Hypervisor.framework):
  `task release:hypervisor:record` writes the receipt `tools/tag_release.sh`
  requires (`16a994`). The release ships `leyline-krun-worker-darwin-arm64`,
  ad-hoc signed with `com.apple.security.hypervisor`, and the microVM backend
  reports itself unavailable for a worker without it (`984acf`, `831bf1`).
  Developer ID signing and notarization follow.

**Measure, then cut operations**
- `be495a` — count operations per workload (FUSE requests, guest exits, socket
  round trips, flushes, bytes copied, parse passes) against the native minimum.
  Counts are deterministic, so `b18099`'s regression gate asserts on them as
  well as on timings, at two store sizes (per-write cost must not grow with the
  store).
- `b6c7ce` — parse after the reply: since ADR-0045 an unparseable file lands
  with a diagnostic, so the parse no longer gates acceptance; the projection
  follows like publication (most of the ~17 ms write that remains after
  `98d0c3`).
- `bf80bd` — collapse the worker-to-daemon hop on the share's hot path.

**Then**
- ADR-0047's remaining identity migrations, before the contract freeze because
  each moves a wire or projection identity: M3 run-id v2 (`ab2093`, scheduled in
  group E above; `derive_key`
  context `cloister/execution/v1/run-id/v2`, rendered `run-v2-…`; cloister
  derives it through the cas-ffi `leyline_derive_key` export that shipped in
  v0.26.0) and M4 rooted blob manifests (`58cf9a`). Each is one versioned
  bump; its mache or cloister issue is filed before its PR merges. M0, M1 and
  M2 shipped in v0.26.0 (see below).
- `b70e44`, `10075e` — the contract freeze, after the ADR status decisions.
- `cf8fdb` — the git conformance suite.
- `c3e8c6` — publish the crates in dependency order (unblocks `395e43`).
- `5ec040`, `5ee38a` — fix or document the host-mount limits.

## Performance

Measured on the shipped path (macOS 27.0.1, Apple M3 Max, APFS and a libkrun
guest only; `docs/benchmarks/2026-10-20-share-path.md`):

| Operation | VM share | APFS | Target |
|---|---|---|---|
| Small-file write in the guest | 17 ms on a quiet machine, 22 ms median of five interleaved runs (47 ms after `8c8ded`, ~15 s before) | 0.07 ms | < 50 ms (v1); `18bfdd` batches bursts |
| Build-output write in the guest (`target/`, derived, `39998b`) | 7.06 ms (loaded machine, before `98d0c3`; an authored write measured 56.6 ms in the same run) | 0.41 ms | near APFS after `2354c3` (scratch inside the guest) |
| open, read, close | 200–280 µs | ~20 µs | within 2× of APFS after `e0893c` (post-v1) |
| `git status`, warm | 163–182 ms | 32–90 ms | |
| `rg`, warm | ~310 ms | ~60 ms | |

Each guest request crosses the worker-to-daemon share socket (120–280 µs); the
header encoding is not the cost. Re-run the benchmark on an idle machine before
quoting a single figure.

## Known limits (documented behavior today)

- Two writers through one host NFS mount are one client to the server. No edit
  is lost: the second save made from an older read is refused `ESTALE` and
  lands after a reload. Their disjoint edits do not merge; per-writer merging
  holds across separate sessions (the VM path) — `5ec040`.
- The store excludes `.git`, so git does not run inside the mount or the share —
  `5ee38a`.
- A load-driven pre-console stall: a macOS guest intermittently stalls before
  its program prints anything, and the stall fails closed as a timeout. The
  quiet-host rule for the release hypervisor gate mitigates it; the root cause
  is open — `c4480a`.
- The host NFS mount is refused on every macOS release below Sequoia 15.8, and
  on Tahoe below 26.7
  (Apple NFS-client CVE-2026-43686 and CVE-2026-84549; `2d49ce`).

## Design direction: one control plane, n data planes

Native filesystems are among the most optimized code there is, and every cost
LLO has paid sits where it deviates from them: no page cache, one flush per
write instead of a group-committed journal, semantic work on the write path,
O(volume) work on a write, build output through the store. The direction
(`d4f424`) is a control plane (the store of record: `storeRoot`, proposals,
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
- **Upstream libkrun** (`10feaa`): submit LLO's twelve patches to
  containers/libkrun, on the owner's go-ahead.

## Merged since v0.29.0 (unreleased)

| PR | Change | Bead |
|---|---|---|
| #566 | A worker compares the confinement it will apply with the grant before the workload starts, on both tiers (`committed_confinement`); the applied policy is parsed from the digested bytes | `7a3109` |
| #568 | Gap queue D1: a registry pull refuses a token realm or redirect to a non-public address and bounds every request's headers; CAS volume `hdiutil` runs are bounded, a crashed provider's partial image is cleaned and only its own image is used, and the real-`hdiutil` tests run in a recorded gate; an image with a regular file at a kernel mount point is refused at import; absolute tar entry names (Nix `dockerTools`) import from the image root; rustix 1.1.6 for RUSTSEC-2026-0486 | `b1c18d`, `167b29`, `1665c9`, `168a65`, `2aa5d3`, `cffbb5`, `d68536` |
| #562 | Gap queue D2: the per-run rootfs copy is built from the manifest, holding exactly the directories the files imply, each at `0755`, so a rootfs digest names one guest tree | `38ff59` |

## Shipped in v0.29.0 (2026-11-11)

Arenas re-parse: `projection-v13`, and a `projection-v12` arena is refused.

| PR | Change | Bead |
|---|---|---|
| #555 | A function used as a value records a `node_refs` row (`EXTRACTION_EPOCH` 6) | `a9ff82` |
| #556 | `leyline rootfs import-oci --from-registry name@sha256:…` pulls a digest-pinned image through the `--layout` verifier | `97b636` |
| #554 | A serve channel holds a connection that arrives before the workload listens, for at most 10 s (libkrun patch 0010) | `a915e3` |
| #560 | A case-sensitive CAS root: `leyline rootfs provision-cas`, `execution-daemon --cas-volume`, `ProvisionOutput.casRoot`; an image with two paths the CAS root would merge is refused by name | `ba86d3` |
| #561 | `projection-v13`: comments are `_ast` occurrences, and `nid` order is no longer document order (`EXTRACTION_EPOCH` 7) | `aa1634` |
| #557 | A microVM run's output reaches the in-process embedder, bounded and typed (`GuestOutput`, `RunOutcome`) | `a26ad0` |
| #559 | Every microVM guest has a bounded tmpfs `/tmp` (libkrun patch 0012) | `db5899` |
| #558 | Gap queue A1: the release bump, verifier and tagger read one schema surface; two cost tests assert counts, not wall time; the Go workflow runs on pull requests into `integration` | `a4946a`, `324f47`, `40300f`, `a4787a`, `b42256` |
| #563 | Gap queue A2: one daemon per control block (`<ctrl>.daemon-lock`); `--reset-arena` removes a crashed reset's staging file; read-only `query` refuses the temp database; a structured `executionError` on the daemon and MCP wire | `1d4003`, `b0800d`, `40594e`, `3befbc`, `765bd5`, `e347ae` |
| #564 | Gap queue A3: Go `sigma.ClaimOf` refuses a head digest v2 `Head`; `head.capnp` describes `unboundFacts` as the producer counts it; a `_tree` directory row with a `blob_hash` is refused; the rootfs digest has a known-answer vector | `a46711`, `cc2130`, `bbd7c8`, `39309e` |

## Shipped in v0.28.0 (2026-11-10)

| PR | Change | Bead |
|---|---|---|
| #546 | The documentation matches the v0.26.0 code | `a678c2` |
| #548 | ADR-0046 Q7: declared connections reach an unmodified microVM workload as TCP on guest loopback, in both directions (confinement/v1 §6 `guestPort`, libkrun patches 0010 and 0011); guest UDP, raw and packet sockets are refused; a declared channel that can't be built refuses the start; `publicEnvironment` keys are `[A-Z_][A-Z0-9_]*` and never `KRUN_…`; every executable is anchored at the guest's `/` | `be3711`, `bb2143`, `eb5e4e`, `fe130c` |
| #549 | `import-oci` drops and reports what a rootfs cannot carry (devices beneath kernel mounts, owner-unreadable files, dangling and kernel-mount links under `--symlinks resolve`) | `fe3d26`, `391965` |
| #550 | Read-only `query` reaches only listed virtual-table modules; sqlite-vec is registered before the reader pool opens | `a9be1f`, `07237c`, `a9d50a`, `3bf5dc` |
| #551 | Only the default instance binds `~/.mache/default.sock`, with no discovery symlinks; every daemon holds the `<ctrl>.lock` lease, and `--reset-arena` recovers a control block of another VERSION when nothing else holds it; signal handlers are installed before `daemon ready` | `e7459a`, `feabf4` |

## Shipped in v0.27.0 (2026-11-10)

| PR | Change | Bead |
|---|---|---|
| #540 | rusqlite 0.41, bundled SQLite 3.54.1 (RUSTSEC-2026-0459); a shim serves witchcraft's rusqlite 0.39 requirement from 0.41 | `6ef210` |
| #541 | `leyline rootfs import-oci`: a digest-pinned OCI image layout becomes a content-addressed execution rootfs | `7736ab` |
| #542 | libkrun patch 0009: with a port map, a TSI datagram socket carries replies only; guest-tools pins are fetched by digest | `0026d2`, `ae6b99` |
| #543 | No RustSec vulnerability in `rs/Cargo.lock`; `task audit` (`cargo deny`, `deny.toml`) runs in `task ci` | `3adcf7`, `643a35` |
| #544 | An OCI-imported rootfs boots in a libkrun guest; a receipt's `inputRoots` names the rootfs first; a replayed start never rewrites a receipt | `68a55d`, `799fbc` |

## Shipped in v0.21.1 to v0.26.0

| Release | Change | Bead |
|---|---|---|
| v0.21.1 | The native tier refuses a run whose grant names a Graph workspace; execution/v1 conformance matrix across native nono and libkrun | `204a94` (part), `f8ebcf` |
| v0.22.0 | `projection-v8` retires `capnp_blobs`/`_ast_blob`; `projection-v9` interns `node_child` into `node_edge` | `f30fdf`, `9b7d28` |
| v0.23.0 | `status` reports `source_root`; Go types for `read_source` and `propose`; `mount` builds NFS without FUSE | `246526`, `247489`, `9d4712` |
| v0.24.0 | `projection-v10` (occurrence tables point at `node_content` by `content_id`); head digest v2 | `738dd9`, `b68fa6` |
| v0.25.0 | Root absence is stated, never zero; arena-local nids leave the wire; `leyline-cache`; ADR-0047 accepted | `80f037`, `754aa1`, `619a99` |
| v0.26.0 | ADR-0047 M0 (axiom suite, `Ref<D>`, locator newtypes, cas-ffi `leyline_derive_key`), M1 (parser-free `storeRoot`, head digest v3, `projection-v11`, wire format major 2) and M2 (keyed `node_hash` merkle-ast-v3, `lang`/`raw_kind` per occurrence, `projection-v12`); verify-on-read for `source_blobs`; MCP serves only advertised, granted tools; read-only `query`; `workspaceUnserved` | `10d71a`, `12e252`, `73c6ee`, `73add7`, `b5d354`, `12d305`, `851a65`, `b7e9a3`, `204a94` |

## Shipped in v0.21.0 (crates not published)

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
| #482 | A proposal's one full flush is its `live.db` commit; the checkout and head writes are ordered | `98d0c3` |
| #483 | Derived paths (build output, tool state, macOS metadata) live in per-run scratch, never in the store | `39998b`, `8d6a8a` |
