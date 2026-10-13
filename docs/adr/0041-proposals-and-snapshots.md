# ADR-0041 — Proposals and snapshots: one write boundary for many agents, immutable publication, content-addressed transport

**Status:** Proposed (2026-10-12)
**Bead:** `ley-line-open-d70f99` (ADR-0040 D7)
**Related:**

- ADR-0040 (the write boundary — this ADR is its D7; D1, D3, D4 and D6 are shipped)
- ADR-0021 and `schema-spec/build-cache/v1` (per-file `inputHash → chunkHash`, fetched by digest)
- ADR-0031 (restriction-addressed keys — Proposed; P3 depends on it)
- ADR-0032 (tagged fold; integrity, dedup and logical structures stay separate; no labels beside digests)
- ADR-0034 (`node_hash`, and its amendment on twin constructs)
- `schema-spec/execution/v1` (`WorkspaceGrant { graphRoot, expectedGeneration, operations }`,
  `WorkspaceOperation.commit` — shipped schema this ADR gives a meaning to)
- cloister ADR-0046 (mediated capabilities), ADR-0049 (host runtime and mediator), ADR-0044 and
  ADR-0050 (overlay snapshot boundary) — policy stays in cloister; this ADR is mechanism
- beads `1eefe3`, `5a0f07`, `0a8c14`, `0a9931` (publication and durability defects this ADR answers)

ADR-0040 cites cloister ADR-0013 and ADR-0030 as the model for slice grants. Those ADRs govern
credential slices in the vault (`allowedSubs`) and process isolation; they confer no authority over
code. The grant that scopes a code write is execution/v1's `WorkspaceGrant`, verified in LLO, with
policy authored in cloister under ADR-0046 and ADR-0049.

Reviewed before proposal by the identity-domain and publication-state lenses (2026-10-12); their
blockers are resolved in the text below and recorded on `d70f99`.

______________________________________________________________________

## Thesis

> Many agents write to one codebase. Today each write lands as soon as it is made: the last
> writer wins, a stale overwrite is silent, and a change that spans files lands one file at a time.
> The fix is a step between a change and its application — a **proposal**, checked at the one
> write boundary against the bytes its author saw, accepted or refused whole.
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
| Growing the arena in place while buffer B is active moved B under its readers | `5a0f07` | fixed by renaming a grown arena into place (`write_grown_arena`) |
| A publisher killed mid-publish leaves the seqlock odd; readers spin; a restarted writer inverts parity | `0a8c14` | `control.rs:128-186`, `:278-300`, `:408-419` |
| `live.db` runs `synchronous=NORMAL`: a write acknowledged at fsync or COMMIT survives a process crash, not a power loss | `0a9931` | `cmd_daemon.rs:824-844` |
| Every FUSE fsync publishes a full image: serialise, memcpy, zero-fill, msync, hash | — | `fs/src/fuse.rs:277` → `graph.rs:1405-1409` → `snapshot_to_arena` |
| Two arena writers run outside the `live.db` writer lock | — | `op_load` → `cmd_load.rs:41-75`; `leyline serve` → `HotSwapGraph::flush_to_arena` |
| A write is several SQLite transactions — `COMMIT`, then autocommitted index builds, sweep and `_meta` stamps — so a pool reader (D4) sees partial states, and an error after `COMMIT` leaves new `_source` under an old `derived_from` | — | `cmd_parse.rs:1316`, `:2037`, `:2109`, `:2116`, `:2162`, `~:2190-2200` |
| The warm-start fallback hydrates `live.db` from the arena with no root check | — | `cmd_daemon.rs:1107-1216` |

`VerifiedArena` and verify-on-fault (`fs/src/verified.rs`) have no production caller; T2.3 is the
gate that runs.

### Writes

D3 shipped one write path, `source_write::write` (`cli-lib/src/source_write.rs:67-102`): one path,
stored and re-projected as a cold parse of its bytes. Nothing on any write path compares what is
being replaced with what the writer saw: mache re-checks size and mtime inside its own splice;
LLO's `StagingGraph` copies a node on first write and never revalidates. A refused write is
`EROFS`/`EIO` in LLO with no readable reason, and success at the syscall in mache with the reason in
`_diagnostics/last-write-status`. LLO's `.staging/` control files (`.dirty`, `.commit`, `.discard`,
`vcs/src/lib.rs:719-822`) exist but no mount serves them.

execution/v1 names the concept at workspace granularity — `WorkspaceGrant { graphRoot,
expectedGeneration, operations }`, `WorkspaceOperation { read, list, query, mutate, commit }`
(`execution.capnp:40-46`, `:90-95`) — but nothing reads `expectedGeneration` or distinguishes
`mutate` from `commit`, the grant carries no path scope, and whether `graphRoot` names an image or a
tree is undefined. `DigestRef.algorithm` names a hash function, and the shipped verifier accepts
only `blake3-256` (`runtime/src/authorization.rs:787-805`).

### Who coordinates today

rosary dispatches agents with required file or directory scopes, refuses to run two beads whose
scopes overlap (`has_file_overlap`, prefix match), runs one agent per repository, gives each a
worktree, and merges through a pull request: the pessimistic, file-grained form of the same model.

______________________________________________________________________

## Decision

### P1 — A proposal is the unit of change, and one SQLite transaction

```
propose(grant, writes) -> Accepted { applied_to, result, commit } | Refused { reasons }

writes = [ Write::File   { path, base, blob }
         | Write::Node   { path, base, replacement }
         | Write::Remove { path, base } ]
```

The single writer applies a proposal or refuses it whole. Acceptance is **one SQLite transaction**
containing the new `source_blobs` rows, the manifest change, every touched file's re-projection, the
new `treeRoot` (folded inside the transaction), the `_meta` stamps (`derived_from`, `commit`), and
the proposal record. Only `CREATE INDEX IF NOT EXISTS`, which changes no query result, may run
outside it. Neither a pool reader nor a snapshot can observe a subset, and an error anywhere rolls
the whole proposal back. The parse's post-`COMMIT` autocommits are moved inside the transaction as a
precondition (sequencing step 1).

A proposal records `applied_to` (the `treeRoot` it was applied to), `result` (the `treeRoot` after),
and `commit = (epoch, seq)` (P6). D3's `write(grant, path, bytes)` becomes the one-write case.

**Durability.** Acceptance is returned only after the transaction's WAL frames are fsynced
(`synchronous=FULL` on the writer, or an explicit WAL sync before acknowledging). This closes
`0a9931` for every write acknowledged to a client, including mount fsync and NFS COMMIT.

**Grant and scope.** `grant` is execution/v1's `WorkspaceGrant`. The grant gains a path scope LLO can
verify: the sub-manifest it authorises, carried with the grant (not only its digest — the
`confinementManifest` precedent, `execution.capnp:143`). A write outside it is refused with no effect
on ring 0 or ring 1 (ADR-0040 F6). Who may hold which grant is cloister's policy; LLO never decides
it and cloister never verifies it a second time.

### P2 — Bases are compare-and-swap over the bytes the write replaces

Every base commits to every byte its write would replace:

- **File / Remove** — the path's current source blob hash (`_source.content_hash`, BLAKE3 over the
  file's bytes) must equal `base.blob`, or the path must be absent when `base` says so. This is the
  HTTP `If-Match` model, with a content hash as the entity tag.
- **Node** — `base = (file_blob, locator, span_hash)`: the file blob hash the author read, the node's
  locator in that generation, and BLAKE3 of the node's exact byte span there. The writer re-resolves
  the locator from `file_blob`'s generation into the current generation and refuses unless it
  resolves to exactly one node whose current span hashes to `span_hash`. The replacement is spliced
  into the file's current bytes and syntax-checked before it is stored.

`node_hash` is not a base. It folds kind, leaf tokens and child hashes and deliberately excludes
comments, whitespace, sibling attributes and non-UTF-8 leaf bytes (ADR-0034 D4;
`cli-lib/tests/ir_producer.rs:254`), so two different byte spans can share it and a stale edit would
overwrite a concurrent comment or attribute change. It may travel in a refusal as a diagnostic.

Re-resolution refuses on ambiguity. Twin constructs — same kind, name and `node_hash`, distinguished
only by a sibling attribute such as `#[cfg]` — have no locator that is unique and stable across
generations (ADR-0034 amendment); when the author's node and the current candidate cannot be paired
one-to-one, the proposal is refused `ESTALE` naming the cohort, and the author retries with a file
base.

Two agents editing different functions of one file both succeed. Two agents editing the same
function, or one editing a comment inside it, refuse the later. There is no merge; ADR-0040's
non-goal stands.

**The coarse base** is the grant's `graphRoot`: a proposal that names no finer base is refused unless
the current `treeRoot` equals `graphRoot`. `expectedGeneration` is defined as the workspace's
`commit.seq` within the grant's epoch (P6) and, when present, must also match. Every proposal's
`applied_to` is checked against nothing coarser than these.

### P3 — The closure check (extension; depends on ADR-0031)

A proposal may also carry restriction-addressed keys (ADR-0031, Proposed) for cross-file facts its
author relied on; the writer refuses if any changed since the author's generation. Today the only
key family is call-target resolution (`sheaf/src/restriction_cache.rs:197-250`, test-only, not
persisted, no scheme tag), whose definition contribution is `(token, source_id, kind)`: it catches a
rename of a called function and does not catch a signature or body change. P3 ships only after
ADR-0031 is accepted with persisted, scheme-tagged keys, and states per key family what it covers;
until a family covers signatures, P3 makes no claim about them.

### P4 — Publication is an immutable snapshot of whole proposals

The writer keeps `live.db` in WAL. To publish, between proposals:

1. make a **fresh** work directory per attempt (`mkdtemp`) on the same volume as `live.db`; at
   daemon start, delete every leftover work directory and every snapshot newer than the control
   block's published one;
2. under the write lock: record `(treeRoot, commit)` from `live.db`; clone `live.db`, and
   `live.db-wal` if it exists, into the work directory (`clonefile` on APFS, `FICLONE` where the
   filesystem reflinks); never clone `-shm`; if `live.db-wal` is absent, assert no `-wal` is in the
   work directory; release the lock. Without reflink, instead begin a read transaction on a pool
   connection under the lock, release, and `sqlite3_backup` from it, so the writer is not stalled for
   an O(size) copy;
3. open the clone alone; `PRAGMA wal_checkpoint(TRUNCATE)` must report `busy = 0`;
   `journal_mode=DELETE`; stamp `_meta.publication = (epoch, seq)`; recompute the `_source` fold and
   refuse to publish unless it equals the clone's `derived_from` and the `treeRoot` recorded in
   step 2; close; fsync the file;
4. rename it to `<ctrl dir>/snapshots/<epoch>-<seq>.db` (same volume; `EXDEV` is a configuration
   error, not a fallback); fsync the directory;
5. publish in control block v4 (P6).

The procedure was checked against a WAL database with a pool reader holding a read transaction
(macOS, APFS `cp -c`): the clone checkpointed to the exact row set, `integrity_check` ok, no `-wal`
or `-shm` left. The same check found the defect step 1 prevents: a clone paired with a leftover
`-wal` from an earlier attempt silently replayed it (5,000 rows where 2,500 were committed) and
passed `integrity_check`.

Publication is coalesced: at most one snapshot per interval (500 ms, as today), each at a proposal
boundary because step 2 holds the write lock. An fsync on the mount no longer publishes.

Readers open `file:<path>?immutable=1&mode=ro`, with `mmap_size` up to SQLite's compile-time cap
(`SQLITE_MAX_MMAP_SIZE`, 2 GiB in the bundled build; above that, pages beyond the cap are read, not
mapped). A snapshot is never written after its rename, so `immutable=1` is true.

**Retention.** The writer keeps the newest K snapshots (default 3) and unlinks older ones after a
grace period; a reader that has a snapshot open keeps reading it, since an unlinked file lives until
its last descriptor closes. A reader that reads the control block and then gets `ENOENT` on open
re-reads the control block; it never falls back to another file. Unlinked-but-open bytes are
reported by the daemon's status op so retention is accountable.

There is one image per snapshot and no double buffer, so `1eefe3` and `5a0f07` have nothing to act
on. Copies per published state: zero image copies where the filesystem reflinks, one elsewhere; no
image hash.

### P5 — `current_root` is retired; the reader check selects, the opt-in verify checks

The control block stops carrying an image hash. The reader **selection check**, replacing T2.3:

1. read v4 under the seqlock; open the snapshot it names;
2. refuse unless `_meta.publication` equals the published `(epoch, seq)` and `_meta.derived_from`
   equals the published `(treeRoot, parserId, projection_schema_version)`.

This proves the reader opened the snapshot the control block names. It does not prove the bytes are
intact or that the rows are the derivation of the tree: both stamps are written by the publisher
into the file the check reads. What T2.3 provided and the selection check does not:

- **byte integrity** (bit rot, a torn or substituted file at the path). Retired as a per-open check:
  snapshots are same-user, same-host files written once and fsynced. `--verify` restores it on
  demand by hashing the snapshot file with BLAKE3 against a digest the publisher records in the
  control block's history op, computed off the write lock after step 4.
- **derivation.** Never provided by T2.3 either (it hashed whatever image was published). `leyline
  rebuild` (ADR-0040 D6) remains the only derivation check, and `_lsp*` and embedding rows are
  outside it by definition.

`current_root` and its consumers are removed in the release that ships v4, per this repository's
practice (removed, not deprecated; recorded in the CHANGELOG):

| Consumer | Replacement |
|---|---|
| daemon wire `current_root` in `StatusResponse`, `ReparseResponse`, `SnapshotResponse`, `FlushResponse`, `EnrichResponse`, `LoadResponse` (`daemon.capnp`) | `treeRoot` and `(epoch, seq)` |
| sheaf invalidation payloads (`daemon/sheaf_ops.rs:298-310`, `:597-606`) | `(epoch, seq)` |
| vcs root watcher (`vcs/src/lib.rs:1399-1450`) | `(epoch, seq)` — moves on enrichment too, as `current_root` did; `treeRoot` would not |
| outboard tree and `VerifiedArena` (`core/src/outboard.rs`, `fs/src/verified.rs`) | removed with the arena; `--verify` above |
| text-search invariant "never advances `current_root`" (`text-search/src/lib.rs:16`) | "never advances `seq`" |
| `ARCHITECTURE.md` (identity table, publication section), `TABLE_CONTRACT.md:335` | rewritten with the cutover |

**Receipts.** A `DigestRef` keeps naming a hash function: `algorithm = "blake3-256"`, as
`read_digest` requires. The domain of a root travels in a separate field, following the
`EvidenceRef`/`ArtifactRef` pattern: `WorkspaceGrant` and `RunReceipt` gain `rootDomain`, and
`leyline/tree-root/v1` — the scheme tag already folded into the digest (`tree_root.rs:36`) — is its
value for a tree root. `treeRoot` is parameterised by `parserId` (`tree_root.rs:151-156`), so a
grant's `graphRoot` is meaningful only under the parser identity the receipt also records.
`RunReceipt` gains, per proposal the run made: `appliedTo` (the `treeRoot` the proposal was applied
to), `resultRoot`, and `proposalDigest` (BLAKE3 over the proposal's canonical writes and bases), so
a verifier can tell the run's change from proposals that interleaved between `inputRoots` and
`resultRoot`.

### P6 — Commit sequence, epoch, and control block v4

`commit = (epoch, seq)`. `seq` advances by one per accepted proposal, inside the proposal's
transaction, and is stored in `live.db`. `epoch` is 16 random bytes minted when `live.db` is created
or reset (`--reset-arena`, cold start) and stored with it. A grant, a base or a cached key from
another epoch never matches. On start, the daemon refuses to publish if `live.db`'s `commit` is
behind the control block's published `commit` in the same epoch (a lost WAL tail), and reports it.

| Offset | Field | |
|---|---|---|
| 0 | magic `LEY4` | new magic: a v3 process mapped before the cutover rejects the block instead of reading v4 fields as v3 |
| 4 | version = 4 | |
| 8 | sequence | seqlock, odd while a publish is in progress |
| 16 | `snapshot_path` [512] | |
| 528 | `epoch` [16] | |
| 544 | `seq` u64 | 0 = nothing published |
| 552 | `tree_root` [32] | |
| 584 | `parser_id` [32] | |
| 616 | `projection_schema_version` [32] | NUL-padded text |
| 1024-1064 | interrupt fields | moved, unchanged |

The sole publisher (P7) holds an exclusive lock on the control file. On open it repairs an odd
sequence to even before publishing (closing `0a8c14`); readers bound their spin and re-read the
version inside the seqlock. mache's Go reader adopts the same protocol.

### P7 — One producer

Only the projection writer in the daemon (or `leyline serve`, which runs the same writer in
process) publishes. Every other producer becomes a proposer:

- `op_load` and `cmd_load.rs`, which write an image into the arena outside the writer lock, are
  removed; loading a corpus is a proposal of its files.
- mache's writable control mode stops writing arena images (`ArenaFlusher`) and proposes through the
  daemon (`mache-0fed77`).
- ley-line's net receiver stops writing an arena and submits what it received as a proposal.
- The warm-start fallback that hydrates `live.db` from the arena is removed with the arena; a daemon
  whose `live.db` is missing cold-starts from source.

### P8 — Transport carries what proposals carry

ADR-0040 D2 makes the sub-manifest plus its source blobs the transport unit, named by `treeRoot`.
Receiving one is a proposal whose bases are the receiver's current state of those paths; a receiver
re-parses what it receives (ADR-0040 F5, shipped).

Fetching derived rows instead of re-parsing is deferred, because no canonical row image exists yet:
today's rows carry the source path in node ids (`cmd_parse.rs:3137`) and arena-local interned ids
(`name_id`, `kind_id`, `dir_id`), so the same bytes at two paths yield two images and
`(sourceHash, parserId) → outputHash` is not a function. The only canonicaliser is test code
(`cmd_rebuild.rs:186`, `:224-376`). A follow-up ADR may define a path-free canonical encoding, name
its signer, and reverse ADR-0040 D5's default that no derivation store ships; until then a
receiver's `derived_from` is always a derivation it performed.

The arena image is no longer a transport unit. ley-line's net layer keeps its signed-manifest and
Merkle page verification and changes the payload it carries. The have/want exchange that decides
which blobs cross the wire is not decided here.

### P9 — Refusal is legible at the syscall and in the tree

| Refusal | errno | Reason |
|---|---|---|
| a base moved | `ESTALE` | the paths or nodes that moved, with their current blob or span hashes |
| ambiguous node (twins) | `ESTALE` | the cohort that could not be paired |
| outside the grant | `EACCES` | the grant and the path |
| invalid syntax after splice | `EIO` | `file:line:col: message`, every error, not the first |
| closure moved (P3) | `ESTALE` | the restriction keys that moved |

The reason is written to `_diagnostics/last-proposal` in the directory of every node or file the
proposal touched, and returned whole from the daemon op. A refused proposal is never reported as
success; mache's draft-on-invalid behaviour remains mache's choice as a client.

### P10 — How a proposal begins and ends

- **Implicit, FUSE.** Each open handle records the file's blob hash at open; a close or fsync on it
  is a one-file proposal with that base, and acceptance advances the handle's base, so repeated
  fsyncs on one handle do not refuse each other.
- **Implicit, NFS.** NFSv3 has no OPEN. A file's base is taken at the first WRITE after the client's
  last accepted COMMIT for that file (its pre-operation state), and advances on each acceptance.
- **Rename and unlink.** Replacing a file by rename (an editor's save-to-temp-then-rename) and
  unlinking one are proposals whose base is the target's blob hash when the client last read or
  opened it; with no recorded read the target's current hash is taken at the rename, and the
  proposal is refused only if the target changed between that read and the rename. `git checkout`'s
  unlink-then-create is two proposals.
- **Explicit.** `.staging/` (already in `leyline-vcs`) is served by the mount. Writes under it are
  private to the session and record their bases at first read. `echo 1 > .staging/.commit` proposes
  them together; `.discard` drops them; `.dirty` lists them.
- **From existing lifecycle points.** A jj or git commit (the `vcs` crate already watches commits),
  the end of a confined run (cloister's overlay upper layer is the proposal, its lower layer the
  snapshot it read), and a daemon `propose` op all call P1.

LLO emits `proposed`, `accepted`, `refused` and `published` events on the daemon socket. Policy
plugs in by issuing and revoking grants, not by running inside the writer.

______________________________________________________________________

## Falsifiers

| # | Claim | Test |
|---|---|---|
| G1 | a proposal is atomic | a three-file proposal with one invalid file leaves no blob, manifest entry, row or snapshot change; with all valid, a snapshot poller **and a pool-reader poller** never observe one or two of the three, nor `_source` under a stale `derived_from` |
| G1b | an error after the rows rolls back | inject a failure in the index build; nothing of the proposal is visible and no snapshot publishes |
| G2 | node bases allow disjoint edits | two proposals with node bases on different functions of one file, applied in either order, both accept, and the file contains both edits |
| G3 | node bases refuse overlapping edits | two proposals with the same node base: the second is refused `ESTALE`, names the node and its current span hash, and changes nothing |
| G3b | node bases cover trivia | proposal A edits only a comment inside function F; proposal B with a node base on F from before A is refused |
| G3c | twins refuse instead of misdirecting | `#[cfg(unix)] fn open` and `#[cfg(windows)] fn open` with equal bodies; insert a function above them; a proposal based on the windows twin is refused or lands on it, never on the unix twin |
| G4 | stale whole-file writes are refused | via FUSE open/close, via NFS WRITE/COMMIT, via save-to-temp-then-rename, and via `git checkout`: a concurrent change made through another path survives and the stale write is refused `ESTALE` |
| G4b | the coarse base advances per proposal | two proposals in one publish interval, the second carrying the `graphRoot`/`expectedGeneration` from before the first: refused |
| G4c | epochs separate lifecycles | a grant issued before `--reset-arena` or a cold start is refused after it |
| G5 | the closure check refuses what it covers | (after ADR-0031) A renames a called function; B, based before A, adds a call to it in another file and carries that key: refused. A signature change is not claimed until a key family covers it |
| G6 | snapshots are immutable and selected exactly | publish, keep a reader open on it, publish K more and unlink it: the open reader reads identical bytes; a reader polling the control block only ever opens a snapshot whose `publication` and `derived_from` match; two snapshots at one `treeRoot` (an enrichment between them) are told apart; a reader racing an unlink re-reads the control block |
| G6b | a work directory cannot pair stale WAL | crash after cloning `live.db` and before cloning its WAL, and after step 2; the next snapshot equals a fresh `VACUUM INTO` of `live.db`, row for row |
| G7 | no header-versus-root window | concurrent fresh readers during 1,000 publishes never refuse (closes `1eefe3`) |
| G8 | growth is not an event | a snapshot ten times larger than the last publishes with no reader refusal |
| G9 | publication cost | per published state on APFS after 1,000 publishes: no image-sized memcpy, no image hash on the write lock; clone time measured against extent count |
| G10 | one producer | `op_load` and every arena-writing path are gone; only the publisher writes the control block |
| G11 | a mid-publish death does not wedge readers | kill the publisher between seqlock begin and end; readers do not hang; the restarted publisher repairs parity and publishes (closes `0a8c14`) |
| G12 | acceptance is durable | an acknowledged proposal survives dropping un-fsynced WAL frames; the restart rule refuses to publish a `commit` behind the control block's (closes `0a9931`) |
| G13 | receipts attribute | a confined run's proposal interleaved with another agent's: the receipt's `appliedTo`, `resultRoot` and `proposalDigest` let a verifier separate the run's writes; `DigestRef.algorithm` stays `blake3-256` and `rootDomain` is `leyline/tree-root/v1` |

______________________________________________________________________

## What lands where

| Repository | Change |
|---|---|
| LLO | proposal transaction and durability (P1), bases (P2), refusal surface (P9), handle and NFS bases, `.staging/` served (P10), snapshot publisher (P4), v4 control block, epoch and sequence (P6), selection check and `--verify` (P5), `current_root` and its consumers removed, `op_load` and the warm-start fallback removed (P7); execution/v1 `rootDomain`, grant scope, `appliedTo`, `resultRoot`, `proposalDigest` at a tagged release |
| mache | v4 control reader with the seqlock; `ArenaFlusher` removed in favour of daemon proposals; `_diagnostics/` reads LLO's reasons |
| ley-line | net receiver submits proposals; payload becomes sub-manifest and blobs; reads `(epoch, seq)` and `treeRoot` instead of `current_root` |
| cloister | consumes the regenerated execution/v1 schema; carries the granted sub-manifest; maps its overlay to proposals; issues grants. No verifier of its own |
| rosary | may replace pre-dispatch overlap locking with proposal refusal; no change required |

Control block v4, the wire break and the execution/v1 additions ship in one LLO release. mache and
ley-line move to it together; until they do, they pin the last v3 release.

______________________________________________________________________

## Sequencing

1. **One transaction per write; durable acceptance** — move the parse's post-`COMMIT` work inside the
   transaction; fsync before acknowledging. G1 (pool reader), G1b, G12. No interface change; closes
   `0a9931`.
2. **P1, P2, P9, P10 implicit** — `propose` with file and node bases behind the existing write path;
   mount and NFS writes become proposals. G2-G4c.
3. **P4-P7** — snapshots, epoch and sequence, v4, the selection check, `current_root` removed, one
   producer. G6-G11. The release that ships this is the cutover for mache and ley-line.
4. **P10 explicit** — `.staging/` served; commit hooks; cloister overlay mapping.
5. **P5 receipts and grant scope** — execution/v1 additions. G13.
6. **P8** — receive as proposal (re-parse).
7. **P3** — after ADR-0031 is accepted. G5.

______________________________________________________________________

## Consequences

- The defects in the context table close by construction or by a named step: there is no shared
  mutable image to tear, grow or write from two places; acceptance is durable; the seqlock repairs.
- `current_root` and T2.3's per-open image hash are removed. Byte integrity becomes an opt-in check;
  this is a deliberate reduction, justified by write-once same-user snapshot files, and recorded
  here rather than implied.
- A refused write becomes visible. Tools that today "succeed" against mache's mount and leave a
  draft will see `ESTALE` or `EIO` where the boundary refuses.
- rosary's scopes can become grants and its worktrees snapshots; agents sharing a file no longer
  serialise.
- **Prior art.** Compare-and-swap on an entity tag is HTTP `If-Match`; P2's file base is that model
  with a content hash. Per-definition versioning and conflict detection go back to ENVY/Developer
  (VisualAge) and Monticello; Darcs and Pijul detect conflicts when a patch is applied; Unison
  content-addresses definitions in its own language; structural merge tools (mergiraf,
  SemanticMerge, IntelliMerge, Spork) merge after the fact; git content-addresses files. What this
  ADR adds is the combination for existing, many-language code: tree-sitter projection into a
  queryable, mountable structure, with conflicts between concurrent agents decided per construct at
  a live filesystem boundary. A prior-art search is owed before the claim is made outside this
  repository.

## Open questions

- **Page cache across clones.** A reflinked snapshot shares disk blocks with its source; whether it
  shares page-cache pages on APFS and on Linux is unmeasured, and whether `clonefile` captures pages
  written but not yet flushed under the writer is unverified (test: write without fsync, clone, diff).
- **Locator re-resolution.** The rule that pairs a node across generations (by kind and name, then
  structural position) and declares a cohort ambiguous needs its own specification and tests.
- **Have/want for transport (P8).** Which side states what it holds, and in what structure.
- **Windows.** Unlink-while-open does not hold; out of scope until a Windows consumer exists.

## Non-goals

- Merging. A conflict is refused and named; resolving it is the proposer's job.
- Authorisation policy. Grants are issued by cloister and verified by LLO.
- Replacing SQLite, or the WAL inside the writer.
- A derivation store (P8); a later ADR.
