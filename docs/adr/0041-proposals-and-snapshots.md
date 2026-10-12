# ADR-0041 — Proposals and snapshots: one write boundary for many agents, immutable publication, content-addressed transport

**Status:** Proposed (2026-10-12)
**Bead:** `ley-line-open-d70f99` (ADR-0040 D7)
**Related:**

- ADR-0040 (the write boundary — this ADR is its D7; D1, D3, D4 and D6 are shipped)
- ADR-0021 and `schema-spec/build-cache/v1` (per-file `inputHash → chunkHash`, fetched by digest — the
  receive path below reuses it)
- ADR-0031 (restriction-addressed keys — the closure check below reuses them)
- ADR-0032 (tagged fold; integrity, dedup and logical structures stay separate)
- ADR-0034 (`node_hash` — the base of a node-level proposal)
- `schema-spec/execution/v1` (`WorkspaceGrant.expectedGeneration`, `WorkspaceOperation.commit` — the
  shipped schema this ADR gives a meaning to)
- cloister ADR-0046 (mediated capabilities), ADR-0049 (host runtime and mediator), ADR-0044 and
  ADR-0050 (overlay snapshot boundary) — policy stays in cloister; this ADR is mechanism
- mache `mache-3451a1` (control v3), `mache-0fed77` (writable control mode through LLO mutations)

ADR-0040 cites cloister ADR-0013 and ADR-0030 as the model for slice grants. Those ADRs govern
credential slices in the vault (`allowedSubs`) and process isolation; they confer no authority over
code. The citation is corrected here: the grant that scopes a code write is execution/v1's
`WorkspaceGrant`, verified in LLO, with policy authored in cloister under ADR-0046 and ADR-0049.

______________________________________________________________________

## Thesis

> Many agents write to one codebase. Today each write lands as soon as it is made: the last
> writer wins, a stale overwrite is silent, and a change that spans files lands one file at a time.
> The fix is a step between a change and its application — a **proposal**, checked at the one
> write boundary against the content its author saw, accepted or refused whole.
>
> Publication then only ever shows whole, accepted states, as immutable snapshot files that readers
> map and keep. Transport moves the same content-addressed objects a proposal carries. The arena
> image stops being an identity, a transport unit, or something more than one process writes.

______________________________________________________________________

## Context — shipped state on `integration` 248cbc3

### Publication

`snapshot_to_arena` (`cli-lib/src/cmd_daemon.rs:1320-1394`) serialises `live.db`, writes the image
into the inactive half of a double-buffered arena (`ll-core/core/src/layout.rs:143-183`: memcpy,
zero-fill of the rest of the buffer, header flip, msync of the whole mapping), hashes the image with
BLAKE3 and publishes the hash as `current_root` through the v3 control block (seqlock,
`core/src/control.rs`). Readers verify the image against `current_root` before deserialising it
(T2.3, `fs/src/lib.rs:69-119`).

| Defect | Bead | Evidence |
|---|---|---|
| The header flips before `current_root` publishes; a fresh reader in the window is refused as "substrate corruption". Three LLO sites, plus mache's `ArenaFlusher` and ley-line's net receiver | `1eefe3` | `cmd_daemon.rs:1370` then `:1385`; `fs/src/graph.rs:1718` then `:1738`; `cmd_load.rs:63` then `:71`; mache `graph/arena_writer.go:229-252`; ley-line `crates/net/src/receiver.rs:347-377` |
| Growing the arena while buffer B is active moves B's offset and overwrites the published image before the new one publishes | `5a0f07` | `layout.rs:89-91`, `:143-183`; `cmd_daemon.rs:1345-1370` |
| Every FUSE fsync publishes a full image: serialise, memcpy, zero-fill, msync, hash | — | `fs/src/fuse.rs:277` → `graph.rs:1405-1409` → `snapshot_to_arena` |
| Two arena writers run outside the `live.db` writer lock | — | `op_load` → `cmd_load.rs:41-75`; `leyline serve` → `HotSwapGraph::flush_to_arena` |
| A parse is several SQLite transactions (index builds and `_meta` writes autocommit after COMMIT), so a snapshot taken between them shows a partial write | — | `cmd_parse.rs:1316`, `:2037`, `~:2107-2200` |
| The warm-start fallback hydrates `live.db` from the arena with no root check | — | `cmd_daemon.rs:1107-1216` |

`VerifiedArena` and verify-on-fault (`fs/src/verified.rs`) have no production caller; T2.3 is the
gate that runs.

### Writes

D3 shipped one write path, `source_write::write` (`cli-lib/src/source_write.rs:67-102`): one path,
stored and re-projected as a cold parse of its bytes, publication left to the caller. Nothing on
any write path compares what is being replaced with what the writer saw: mache re-checks size and
mtime inside its own splice; LLO's `StagingGraph` copies a node on first write and never
revalidates. A refused write is `EROFS`/`EIO` in LLO with no readable reason, and success at the
syscall in mache with the reason in `_diagnostics/last-write-status`. LLO's `.staging/` control
files (`.dirty`, `.commit`, `.discard`, `vcs/src/lib.rs:719-822`) exist but no mount serves them.

execution/v1 already names the concept this ADR implements, at workspace granularity:
`WorkspaceGrant { graphRoot, expectedGeneration, operations }` and `WorkspaceOperation { read, list,
query, mutate, commit }` (`execution.capnp:40-46`, `:90-95`). The runtime reads `graphRoot`
(`runtime/src/authorization.rs:907-1005`); nothing reads `expectedGeneration` or distinguishes
`mutate` from `commit`. What `graphRoot` names — an image or a tree — is undefined.

### Who coordinates today

rosary dispatches agents with required file or directory scopes, refuses to run two beads whose
scopes overlap (`has_file_overlap`, prefix match), runs one agent per repository, gives each a
worktree, and merges through a pull request. That is the pessimistic, file-grained form of the
same model: conflicts are prevented by locking before work starts, so two agents changing different
functions in one file never run at once.

______________________________________________________________________

## Decision

### P1 — A proposal is the unit of change

```
propose(grant, base, writes) -> Accepted { treeRoot' } | Refused { reasons }

writes = [ Write::File { path, blob } | Write::Node { path, node, replacement } | Write::Remove { path } ]
base   = for each write, the content its author saw:
           File / Remove -> the path's source blob hash (or Absent)
           Node          -> the node's own node_hash
```

The single writer applies a proposal or refuses it whole. On acceptance every blob is in
`source_blobs`, the manifest advances once, `treeRoot` advances once, and every touched file is
re-projected — inside one write-lock hold, so no reader, snapshot or other proposal observes a
subset. D3's `write(grant, path, bytes)` becomes the one-write case of `propose`.

`grant` is execution/v1's `WorkspaceGrant`, verified in LLO. A write outside it is refused with no
effect on ring 0 or ring 1 (ADR-0040 F6). Who may hold which grant is cloister's policy; LLO never
decides it and cloister never verifies it a second time.

### P2 — Bases are compare-and-swap, at the finest grain the write names

The writer refuses a proposal if any base no longer matches:

- **File** — the path's current source blob hash must equal the base.
- **Node** — the node at that address must still have the base `node_hash`. Only the node's own
  hash is compared, never an ancestor's: `node_hash` folds children (ADR-0034), so every enclosing
  node's hash moves whenever anything inside it moves. When the node matches, its replacement is
  spliced into the file's *current* bytes, which may already carry other accepted changes, and the
  result is checked for syntax before it is stored.

Two agents editing different functions of one file both succeed. Two agents editing the same
function: the second is refused, told which node moved and what it now hashes to, and rebases. There
is no merge; ADR-0040's non-goal stands — a conflict is surfaced at the boundary, not resolved by it.

`expectedGeneration` is the coarsest base: it refuses unless nothing in the workspace moved. It is
kept as the base for a proposal that names no finer one, and is defined as the workspace's
publication sequence (P4).

### P3 — The closure check (extension; ships after P1-P2)

A proposal may also carry the restriction-addressed keys (ADR-0031) of the cross-file facts its
author relied on — the definition a new call resolves to, the signature an implementation
satisfies. The writer refuses if any of those keys changed since the base. This catches the
conflict no syntax check can: agent A renames `Bar`, agent B adds a call to `Bar` in another file;
no node overlaps, and B is refused because the definition it depended on moved. Restriction maps are
the part of the sheaf work ADR-0030 found earns its keep; this is their second consumer.

### P4 — Publication is an immutable snapshot of whole proposals

The writer keeps `live.db` in WAL. To publish, holding the write lock between proposals:

1. clone `live.db` and `live.db-wal` into a work directory beside the snapshot directory
   (`clonefile` on APFS, `FICLONE` on Linux filesystems that reflink, a copy otherwise);
2. release the write lock;
3. open the clone alone, `PRAGMA wal_checkpoint(TRUNCATE)`, `journal_mode=DELETE`, close; fsync the
   file;
4. rename it to `<arena>.snapshots/<seq>-<treeRoot16>.db`; fsync the directory;
5. publish `(seq, path, treeRoot, parserId, projection_schema_version)` in control block v4.

Only `live.db`'s own writer touches `live.db`; the checkpoint runs in a private clone, so no reader
of `live.db` blocks it and no long reader of a snapshot starves it. A proposal is applied entirely
before step 1 or entirely after it, because both hold the write lock — including the parse's
post-COMMIT index and `_meta` writes. Publication is coalesced: at most one snapshot per interval
(500 ms, as today), each ending on a proposal boundary. An fsync on the mount no longer publishes.

Readers open `file:<path>?immutable=1` with `mmap_size` at least the file size: no locks, no
shared-memory file, pages served from the page cache. A snapshot is never written after its rename,
so `immutable=1` is true. The writer unlinks snapshots older than the newest few after a grace
period; a reader that still has one open keeps reading it, because an unlinked file lives until its
last descriptor closes. There is one image per snapshot and no double buffer, so `1eefe3` and
`5a0f07` have nothing to act on.

Copies per published state: zero image copies where the filesystem reflinks, one elsewhere; no
image hash.

### P5 — `current_root` is retired; the verify gate checks the tree

The control block stops carrying an image hash. The authority a reader needs is "this snapshot is
the projection of that tree", and the snapshot records it: `_meta.derived_from = (treeRoot,
parserId, projection_schema_version)` (ADR-0040 D6). The v4 gate, replacing T2.3:

1. read v4 under the seqlock; open the snapshot it names;
2. refuse unless `_meta.derived_from` equals the published `(treeRoot, parserId, version)`;
3. `--verify` (opt-in, the cost T2.3 paid on every open): recompute `treeRoot` from the snapshot's
   `_source` set and refuse on mismatch.

Step 2 is a row read. It catches every failure T2.3 caught in practice — a reader looking at the
wrong state — and the image hash it replaces named bytes no consumer compared to anything else.

`current_root` leaves the daemon wire (`StatusResponse`, `ReparseResponse`, `SnapshotResponse`,
`FlushResponse`, `EnrichResponse`, `LoadResponse` in `daemon.capnp`) in favour of `treeRoot` and the
publication `seq`; sheaf invalidation keys on `seq`. This is a wire break, made once, in the release
that ships control block v4. Per this repository's practice it is removed, not deprecated, with the
break recorded in the CHANGELOG.

`graphRoot`, `inputRoots` and every other execution/v1 `DigestRef` that names a code tree carry
`treeRoot` under the algorithm tag `leyline-tree-root/v1`, so a receipt's root is unambiguous.
`RunReceipt` gains `resultRoot`: the `treeRoot` after the run's proposal was accepted, absent if it
was refused. A receipt then shows the tree a run started from and the tree it produced.

### P6 — Control block v4

| Offset | Field | |
|---|---|---|
| 0 | magic `LEYC` | unchanged |
| 4 | version = 4 | |
| 8 | sequence | seqlock, odd while a publish is in progress (v3 protocol) |
| 16 | `snapshot_path` [256] | |
| 272 | `publication_seq` u64 | monotonic; 0 = nothing published |
| 280-320 | interrupt fields | unchanged |
| 320 | `tree_root` [32] | |
| 352 | `parser_id` [32] | |
| 384 | `projection_schema_version` [32] | NUL-padded text |

`arena_size` and `current_root` are gone; the snapshot's size is its file's. A v3 reader refuses a
v4 block by its version check, which is the cutover signal. Readers loop on an odd or changed
sequence (the v3 seqlock); mache's Go reader adopts the same protocol (`mache-3451a1` widens to v4).

### P7 — One producer

Only the projection writer in the daemon (or `leyline serve`, which runs the same writer in
process) publishes. Every other producer becomes a proposer:

- `op_load` and `cmd_load.rs`, which write an image into the arena outside the writer lock, are
  removed; loading a corpus is a proposal of its files.
- mache's writable control mode stops writing arena images (`ArenaFlusher`) and proposes through the
  daemon (`mache-0fed77`).
- ley-line's net receiver stops writing an arena and submits what it received as a proposal.

### P8 — Transport carries what proposals carry

ADR-0040 D2 already makes the sub-manifest plus its source blobs the transport unit, named by
`treeRoot`. Receiving one is a proposal whose base is the receiver's current state of those paths.
A receiver that already holds the derived rows for a file need not re-parse it: per-file row images
are fetched by output hash through the ADR-0021 / build-cache/v1 lockfile (`inputHash → chunkHash`,
digest-verified, insert-idempotent), carried under signed `(sourceHash, parserId) → outputHash`
claims (ADR-0040 D5), and assembled with `file_id` applied at projection (ADR-0040 D6). Cross-file
facts are recomputed only where their ADR-0031 input closure changed. The have/want exchange that
decides which blobs and row images cross the wire is not decided here.

The arena image is no longer a transport unit. ley-line's net layer keeps its signed-manifest and
Merkle page verification and changes the payload it carries.

### P9 — Refusal is legible at the syscall and in the tree

| Refusal | errno | Reason |
|---|---|---|
| a base moved | `ESTALE` | the paths or nodes that moved, with their current hashes |
| outside the grant | `EACCES` | the grant and the path |
| invalid syntax after splice | `EIO` | `file:line:col: message`, every error, not the first |
| closure moved (P3) | `ESTALE` | the restriction keys that moved |

The reason is written to `_diagnostics/last-proposal` in the directory of every node or file the
proposal touched, and returned whole from the daemon op. mache's draft-on-invalid behaviour remains
mache's choice as a client; the boundary itself never reports success for a refused proposal.

### P10 — How a proposal begins and ends

- **Implicit.** A close, fsync or NFS COMMIT of one file is a one-file proposal whose base is the
  file's content at open — NFS close-to-open semantics, made a compare-and-swap. Editors, `cp` and
  `git checkout` keep working; a stale overwrite is refused instead of winning.
- **Explicit.** `.staging/` (already in `leyline-vcs`) is served by the mount. Writes under it are
  private to the session. `echo 1 > .staging/.commit` proposes them together with the bases recorded
  at first read; `.discard` drops them; `.dirty` lists them.
- **From existing lifecycle points.** A jj or git commit (the `vcs` crate already watches commits),
  the end of a confined run (cloister's overlay upper layer is the proposal, its lower layer the
  snapshot it read), and a daemon `propose` op all call P1.

LLO emits `proposed`, `accepted`, `refused` and `published` events on the daemon socket. Policy
plugs in by issuing and revoking grants, not by running inside the writer.

______________________________________________________________________

## Falsifiers

| # | Claim | Test |
|---|---|---|
| G1 | a proposal is atomic | a three-file proposal with one invalid file leaves no blob, manifest entry, row or snapshot change; with all valid, a reader polling snapshots never observes one or two of the three |
| G2 | node bases allow disjoint edits | two proposals with node bases on different functions of one file, applied in either order, both accept, and the file contains both edits |
| G3 | node bases refuse overlapping edits | two proposals with the same node base: the second is refused `ESTALE`, names the node and its current hash, and changes nothing |
| G4 | a stale whole-file write is refused | open a file through the mount, change it through a second path, write and close the first: `ESTALE`, and the second change survives |
| G5 | the closure check refuses semantic conflicts | proposal A renames a function; proposal B, based before A, adds a call to it in another file and carries that definition's restriction key: B is refused |
| G6 | snapshots are immutable and whole | publish, reopen the snapshot by path after a newer one is published and the old one unlinked: an open reader reads identical bytes; a reader polling `publication_seq` only ever opens snapshots whose `derived_from` matches the control block |
| G7 | no header-versus-root window | concurrent fresh readers during 1,000 publishes never refuse (closes `1eefe3`) |
| G8 | growth is not an event | a snapshot ten times larger than the last publishes with no reader refusal (closes `5a0f07`) |
| G9 | publication cost | per published state on APFS: no image-sized memcpy, no image hash; the clone is O(1) in file size |
| G10 | one producer | `op_load` and every arena-writing path are gone; a test that searches the workspace for writes to the control block finds only the publisher |
| G11 | receive without re-parse | a sub-manifest whose row images are all present by output hash is projected with zero parser invocations, and its row images match a cold parse modulo `file_id` |
| G12 | receipts name trees | a confined run's receipt carries `inputRoots` and `resultRoot` as `leyline-tree-root/v1` digests, and `resultRoot` equals the receiver's `treeRoot` after acceptance |

______________________________________________________________________

## What lands where

| Repository | Change |
|---|---|
| LLO | `propose` (P1-P2), refusal surface (P9), `.staging/` served (P10), snapshot publisher and v4 control block (P4, P6), v4 reader gate (P5), `current_root` removed from the daemon wire, `op_load` removed, execution/v1 `resultRoot` and `leyline-tree-root/v1` (P5) at a tagged release |
| mache | v4 control reader with the seqlock; `ArenaFlusher` removed in favour of daemon proposals; `_diagnostics/` reads LLO's reasons; prompt template points at the construct directory |
| ley-line | net receiver submits proposals; payload becomes sub-manifest, blobs and row images; reads `treeRoot` and `publication_seq` instead of `current_root` |
| cloister | consumes the regenerated execution/v1 schema; maps its overlay to proposals; issues grants. No verifier of its own |
| rosary | may replace pre-dispatch overlap locking with proposal refusal; no change required |

Control block v4, the wire break and execution/v1 `resultRoot` ship in one LLO release. mache and
ley-line move to it together; until they do, they pin the last v3 release.

______________________________________________________________________

## Sequencing

1. **P1, P2, P9, P10 implicit** — `propose` with file and node bases behind the existing write path;
   the mount's close and COMMIT become one-file proposals. No publication or wire change. G1-G4.
2. **P4-P7** — snapshots, v4, the reader gate, `current_root` removed, one producer. G6-G10. The
   release that ships this is the cutover for mache and ley-line.
3. **P10 explicit** — `.staging/` served; commit hooks; cloister overlay mapping. G1 through the
   mount.
4. **P8** — receive as proposal; row images by output hash. G11.
5. **P3** — the closure check. G5.
6. **P5 receipts** — `resultRoot` in execution/v1. G12.

______________________________________________________________________

## Consequences

- The defects in the context table close by construction: there is no shared mutable image to tear,
  grow or write from two places.
- `current_root` and T2.3's per-open image hash are removed. Anything that compared image hashes
  across processes compares `(treeRoot, publication_seq)` instead.
- A refused write becomes visible. Tools that today "succeed" against mache's mount and leave a
  draft will see `ESTALE` or `EIO` where the boundary refuses; mache decides whether its clients keep
  draft semantics on top.
- rosary's scopes can become grants and its worktrees snapshots; agents sharing a file no longer
  serialise.
- The novelty this rests on is that existing, many-language code becomes content-addressed structure
  (`node_hash` over `node_content` and `node_child`) that is queryable, mountable and transportable
  by hash, so that a conflict can be decided per construct at write time. Prior art does parts of
  this — git content-addresses files, Unison content-addresses definitions of its own language, and
  structural merge tools (mergiraf, SemanticMerge, IntelliMerge, Spork) merge after the fact. A
  prior-art search is owed before the claim is made outside this repository.

## Open questions

- **Page cache across clones.** A reflinked snapshot shares disk blocks with its source; whether it
  shares page-cache pages on APFS and on Linux is unmeasured. If it does not, two readers of
  successive snapshots hold two copies of unchanged pages. Measure before claiming memory parity
  with the double buffer.
- **Node addressing.** A node base names a node by its address in the mount (`file/functions/Bar`);
  how an address survives a sibling insertion that renumbers ordinals needs a rule (by kind and name
  where the projection has a name, by ordinal otherwise).
- **Have/want for transport (P8).** Which side states what it holds, and in what structure.
- **Snapshot retention.** How many, and for how long, under many readers.
- **Windows.** Unlink-while-open does not hold; out of scope until a Windows consumer exists.

## Non-goals

- Merging. A conflict is refused and named; resolving it is the proposer's job.
- Authorisation policy. Grants are issued by cloister and verified by LLO.
- Replacing SQLite, or the WAL inside the writer.
