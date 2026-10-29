# ADR-0041 — Publication: immutable snapshots of whole proposals, one producer, `current_root` retired

**Status:** Proposed (2026-10-12)
**Bead:** `ley-line-open-d70f99` (ADR-0040 D7)
**Related:**

- ADR-0040 (the write boundary — this ADR is its D7; D1, D3, D4 and D6 are shipped)
- ADR-0042 (proposals — split from this ADR; publication shows only whole proposals)
- ADR-0032 (integrity, dedup and logical structures stay separate; no labels beside digests)
- `schema-spec/execution/v1` (`DigestRef`, `WorkspaceGrant`, `RunReceipt`)
- beads `1eefe3`, `5a0f07`, `0a8c14` (publication defects this ADR answers)
- mache `mache-3451a1` (control reader), `mache-0fed77` (writable control mode through LLO)

Reviewed before proposal by the identity-domain and publication-state lenses (2026-10-12); their
blockers are resolved below and recorded on `d70f99`.

______________________________________________________________________

## Thesis

> The arena image stops being an identity, a transport unit, or something more than one process
> writes. Publication shows only whole, accepted states, as immutable snapshot files that readers map
> and keep; the content-addressed objects a proposal carries are what crosses machines.

______________________________________________________________________

## Context — shipped state

`snapshot_to_arena` (`cli-lib/src/cmd_daemon.rs`) serialises `live.db`, writes the image into the
inactive half of a double-buffered arena (`ll-core/core/src/layout.rs`: memcpy, zero-fill of the
rest of the buffer, header flip, msync of the whole mapping), hashes it with BLAKE3 and publishes the
hash as `current_root` through the v3 control block. Readers verify the image against `current_root`
before deserialising it (T2.3, `fs/src/lib.rs:69-119`).

| Defect | Bead | Evidence |
|---|---|---|
| The header flips before `current_root` publishes; a fresh reader in the window is refused. Three LLO sites, plus mache's `ArenaFlusher` and ley-line's net receiver | `1eefe3` | `cmd_daemon.rs` snapshot; `fs/src/graph.rs` `flush_to_arena`; `cmd_load.rs`; mache `graph/arena_writer.go:229-252`; ley-line `crates/net/src/receiver.rs:347-377` |
| Growing the arena in place while buffer B was active moved B under its readers | `5a0f07` | fixed by `write_grown_arena` (#436); the double buffer stays a hazard |
| A publisher killed mid-publish leaves the seqlock odd; readers spin | `0a8c14` | `core/src/control.rs:128-186`, `:278-300`, `:408-419` |
| Every host-mount save publishes a full image | — | storefs `StoreFs::propose` → `Store::publish` → `snapshot_to_arena` (since ADR-0045 T8; before, `fs/src/fuse.rs` → `graph.rs`) |
| An arena writer runs outside the `live.db` writer lock | — | `op_load` (`cmd_load.rs`); `leyline serve`'s `HotSwapGraph::flush_to_arena` was the second, removed with `serve` (ADR-0045 T8) |
| The warm-start fallback hydrates `live.db` from the arena with no root check | — | `cmd_daemon.rs` `try_warm_start_from_arena` |

`VerifiedArena` and verify-on-fault (`fs/src/verified.rs`) have no production caller.

### Amendment (2026-10-22) — shipped interim: one publisher, off the writer

Bead `ley-line-open-8c8ded`. The share benchmark
([2026-10-20](../benchmarks/2026-10-20-share-path.md)) measured ~15 s to create one small file in
the guest on a 2,034-file store: every landed proposal published inline, on the `live.db` writer
lock. A profile of `op_propose` on a 3,080-file store (1.3 GB `live.db`) put the time in that
publish: 53% in `write_grown_arena` (a new 2.6 GB arena file, although the image had grown by a few
KiB: the resize sized the buffer to the image, so any growth after a resize rewrote the arena) and
36% in a single-threaded BLAKE3 of the image. This ships ahead of S1–S6; nothing below changes what
`current_root` names (BLAKE3 over one exact serialized image), and the decisions below stay Proposed.

- **A proposal requests a publish; it does not publish.** `propose` and storefs's FLUSH commit to
  `live.db` and call `LiveDb::request_publish`. A proposal is durable once committed (WAL,
  `synchronous=FULL`, and `fullfsync` since the 2026-10-23 amendment; warm start opens `live.db`
  first), so the reply waits for the commit only.
  Out-of-process readers see it after the publisher's next publish; `snapshot` publishes on demand.
- **One publisher, off the writer** (S5's daemon half; H5). Every daemon publish goes through
  `publish_living_db`: it holds `LiveDb::publish_lock` from the serialize to the root advance and
  reads the image through a pool connection, one read transaction, so the image is one committed
  state and proposals commit while it is written. The daemon's publisher task wakes on a request
  or the 500 ms tick and publishes once for every commit since its last publish
  (`publish_if_dirty`). `op_load` writes the arena under the same lock. The ops whose reply
  promises a published image (`reparse`, `enrich`, `snapshot`) and the watcher's reparse publish
  before replying or emitting, as before.
- **The arena grows with headroom.** `arena_size_for` keeps the arena while the image fits a buffer
  and otherwise allocates buffers a quarter larger than the image, so a growing database resizes
  O(log n) times.
- **The image root hashes across cores.** `image_root` is `blake3::Hasher::update_rayon`, which
  yields the bytes `ContentAddressed::hash` yields (tested across chunk sizes).

A publish still costs one serialize, write and hash of the whole database; it no longer sits on any
proposal's path. S2's immutable snapshot files remove that cost.

### Amendment (2026-10-23) — a proposal's one full flush is its commit

Bead `ley-line-open-98d0c3`. After the amendment above, an accepted proposal on macOS issued four
`F_FULLFSYNC` (4.65 ms each on the benchmark machine's disk, against 0.03 ms for `fsync(2)`): the
checkout file and its directory, and the Σ head file and its directory. Its acknowledgement, the
`live.db` commit, used plain `fsync(2)`, which on macOS stops at the drive's cache: the copies were
flushed harder than the rows they copy.

**Durability contract.** A proposal is *accepted* when its rows are durable in `live.db`: the WAL
commit at `synchronous=FULL` with `fullfsync` and `checkpoint_fullfsync`, one `F_FULLFSYNC` on
macOS. The reply promises that and nothing else. Everything a proposal writes after it is a copy
of those rows that a later pass rebuilds, so it is *ordered*, not flushed
(`leyline_cli_lib::durable::order`: `F_BARRIERFSYNC` on macOS, `fsync(2)` elsewhere):

| write | ordering | after a power loss |
|---|---|---|
| checkout file | its bytes before its rename | the old file or the whole new one, never a rename landed ahead of its data |
| checkout rename or removal | before the `_checkout_index` row that records it | the index never names a file the directory lost; reconcile converges or materializes |
| `_checkout_index`, `_checkout_conflict` | one transaction after every rename, committed without a sync of its own | lost rows leave the index behind, which the next reconcile converges |
| Σ head | its bytes before its rename, the rename ordered after | the previous head or the new one, never a torn one |

Ordering the index row after the rename is load-bearing: an index row durable ahead of its rename
names the store's bytes over a directory that lost them, which reconcile reads as a person's edit
and ingests, undoing the proposal (`an_index_row_landed_ahead_of_its_rename_reads_as_an_edit`).

The copies become durable at the next `F_FULLFSYNC`: the next proposal's commit or a checkpoint.
Until then a power loss can roll the checkout or the head back to its predecessor; the checkout is
brought forward by the next reconcile, and the head by the next parse over the durable rows.
Sidecar segment files are renamed without a sync, as before; that is unchanged here.

Falsifiers: `a_materialized_file_is_ordered_before_its_rename_and_its_index_row`,
`a_removal_is_ordered_before_its_index_row`, `a_crash_before_the_write_materializes_the_accepted_bytes_at_restart`,
`configure_wal_activates_wal_on_file_backed_connection` (`fullfsync`, `checkpoint_fullfsync`),
and `durable`'s unit tests.

______________________________________________________________________

## Decision

### S1 — Commit sequence and epoch

`commit = (epoch, seq)`. `seq` advances by one per accepted proposal (ADR-0042), inside the
proposal's transaction, and is stored in `live.db`. `epoch` is 16 random bytes minted when `live.db`
is created or reset and stored with it. A grant, a base or a cached key from another epoch never
matches. On start, the daemon refuses to publish if `live.db`'s `commit` is behind the control
block's published `commit` in the same epoch, and reports it. `WorkspaceGrant.expectedGeneration`
is `seq` within the grant's epoch.

### S2 — A snapshot is an immutable file of whole proposals

The writer keeps `live.db` in WAL. To publish, between proposals:

1. make a **fresh** work directory per attempt (`mkdtemp`) on the same volume as `live.db`; at
   daemon start, delete leftover work directories and every snapshot newer than the control block's
   published one;
2. under the write lock: record `(treeRoot, commit)` from `live.db`; clone `live.db`, and
   `live.db-wal` if it exists, into the work directory (`clonefile` on APFS, `FICLONE` where the
   filesystem reflinks); never clone `-shm`; if `live.db-wal` is absent, assert no `-wal` is in the
   work directory; release the lock. Without reflink, begin a read transaction on a pool connection
   under the lock, release, and `sqlite3_backup` from it, so the writer is not stalled for an
   O(size) copy;
3. open the clone alone; `PRAGMA wal_checkpoint(TRUNCATE)` must report `busy = 0`;
   `journal_mode=DELETE`; stamp `_meta.publication = (epoch, seq)`; recompute the `_source` fold and
   refuse to publish unless it equals the clone's `derived_from` and the `treeRoot` recorded in step
   2; close; fsync;
4. rename to `<ctrl dir>/snapshots/<epoch>-<seq>.db` (same volume; `EXDEV` is a configuration
   error); fsync the directory;
5. publish in control block v4 (S4).

The procedure was checked against a WAL database with a pool reader holding a read transaction
(macOS, APFS `cp -c`): the clone checkpointed to the exact row set, `integrity_check` ok, no `-wal`
or `-shm` left. The same check found what step 1 prevents: a clone paired with a leftover `-wal`
from an earlier attempt silently replayed it (5,000 rows where 2,500 were committed) and passed
`integrity_check`.

Publication is coalesced: at most one snapshot per interval (500 ms), each at a proposal boundary
because step 2 holds the write lock. An fsync on the mount does not publish.

Readers open `file:<path>?immutable=1&mode=ro`, with `mmap_size` up to SQLite's compile-time cap
(`SQLITE_MAX_MMAP_SIZE`, 2 GiB in the bundled build). A snapshot is never written after its rename.

**Retention.** The writer keeps the newest K snapshots (default 3) and unlinks older ones after a
grace period; a reader with one open keeps reading it. A reader that gets `ENOENT` after reading the
control block re-reads it; it never falls back to another file. Unlinked-but-open bytes are reported
by the status op.

There is one image per snapshot and no double buffer, so `1eefe3` and `5a0f07` have nothing to act
on. Copies per published state: zero image copies where the filesystem reflinks, one elsewhere; no
image hash.

### S3 — `current_root` is retired; the reader check selects, `--verify` checks

The reader **selection check**, replacing T2.3: read v4 under the seqlock; open the snapshot it
names; refuse unless `_meta.publication` equals the published `(epoch, seq)` and
`_meta.derived_from` equals the published `(treeRoot, parserId, projection_schema_version)`.

This proves the reader opened the snapshot the control block names. It does not prove the bytes are
intact or that the rows derive from the tree: the publisher writes both stamps into the file the
check reads. Byte integrity, which T2.3 gave per open, is retired as a per-open check — snapshots are
same-user, same-host files written once and fsynced — and `--verify` restores it on demand by
hashing the snapshot against a digest the publisher records off the write lock after S2 step 4.
Derivation was never checked by T2.3; `leyline rebuild` (ADR-0040 D6) remains the derivation check.

Removed with `current_root`, in the release that ships v4:

| Consumer | Replacement |
|---|---|
| daemon wire `current_root` (`StatusResponse`, `ReparseResponse`, `SnapshotResponse`, `FlushResponse`, `EnrichResponse`, `LoadResponse`) | `treeRoot` and `(epoch, seq)` |
| sheaf invalidation payloads | `(epoch, seq)` |
| vcs root watcher | `(epoch, seq)` (moves on enrichment, as `current_root` did) |
| outboard tree and `VerifiedArena` | removed with the arena; `--verify` |
| text-search "never advances `current_root`" | "never advances `seq`" |
| `ARCHITECTURE.md`, `TABLE_CONTRACT.md` | rewritten with the cutover |

**Receipts.** A `DigestRef` keeps naming a hash function (`blake3-256`, as `read_digest` requires);
the domain of a root travels in a separate `rootDomain` field, whose value for a tree root is
`leyline/tree-root/v1`, the scheme tag already folded into the digest. `treeRoot` is parameterised by
`parserId`, so a receipt records both. `RunReceipt` gains, per proposal a run made, `appliedTo`,
`resultRoot` and `proposalDigest`, so a verifier can tell the run's change from proposals that
interleaved.

### S4 — Control block v4

| Offset | Field | |
|---|---|---|
| 0 | magic `LEY4` | new magic: a v3 reader mapped before the cutover rejects the block |
| 4 | version = 4 | |
| 8 | sequence | seqlock, odd while a publish is in progress |
| 16 | `snapshot_path` [512] | |
| 528 | `epoch` [16] | |
| 544 | `seq` u64 | 0 = nothing published |
| 552 | `tree_root` [32] | |
| 584 | `parser_id` [32] | |
| 616 | `projection_schema_version` [32] | NUL-padded text |
| 1024-1064 | interrupt fields | moved, unchanged |

Every publisher holds an exclusive lock on the control file for as long as the sequence is odd, and
a publish that finds the sequence already odd keeps it odd and ends it even over a whole payload.
A reader facing an odd sequence probes the lock shared: held means a live publish, so it waits; free,
with the sequence unmoved under the shared lock, means an abandoned publish, so it answers the last
consistent snapshot its handle read, or nothing published if it has read none, and never the torn
payload. The lock decides, not a clock. Readers re-read the version inside the seqlock. This shipped
on control block v3 in v0.20.5 (`0a8c14`, mache's reader mirrors it); v4 keeps it.

### S5 — One producer

Only the daemon's projection writer publishes (`leyline serve`, which ran the same writer, is
removed: ADR-0045 T8).
`op_load` and `cmd_load.rs` are removed — loading a corpus is a proposal of its files. mache's
writable control mode stops writing arena images and proposes through the daemon (`mache-0fed77`).
ley-line's net receiver stops writing an arena and submits what it received as a proposal. The
warm-start fallback is removed with the arena.

### S6 — Transport carries what proposals carry

ADR-0040 D2 makes the sub-manifest plus its source blobs the transport unit. `treeRoot` folds in
`parserId`, so the unit is named by the pair `(treeRoot, parserId)` and the pair travels with it; a
receiver verifies the received blobs against the sub-manifest's per-path blob hashes, which do not
depend on the parser, and recomputes `treeRoot` only under the sender's `parserId`. Receiving one is
a proposal whose bases are the receiver's current state of those paths; the
receiver re-parses (ADR-0040 F5). Fetching derived rows instead is deferred: today's rows carry the
source path and arena-local interned ids, so `(sourceHash, parserId) → outputHash` is not a function;
a later ADR may define a path-free encoding and its signer. ley-line's net layer keeps its
signed-manifest and Merkle page verification and changes the payload it carries. The have/want
exchange is not decided here.

______________________________________________________________________

## Falsifiers

| # | Claim | Test |
|---|---|---|
| H1 | snapshots are immutable and selected exactly | an open reader of an unlinked snapshot reads identical bytes; a polling reader only opens snapshots whose stamps match the control block; two snapshots at one `treeRoot` are told apart; a reader racing an unlink re-reads |
| H2 | a work directory cannot pair stale WAL | crash after cloning `live.db` and before its WAL; the next snapshot equals `VACUUM INTO` of `live.db` row for row |
| H3 | no header-versus-root window | concurrent fresh readers during 1,000 publishes never refuse (`1eefe3`) |
| H4 | growth is not an event | a snapshot ten times larger publishes with no reader refusal |
| H5 | publication cost | after 1,000 publishes on APFS: no image-sized memcpy or hash on the write lock |
| H6 | one producer | only the publisher writes the control block |
| H7 | a mid-publish death does not wedge readers | kill between seqlock begin and end; readers do not hang and keep their last snapshot; the next publish repairs parity (`0a8c14`) |
| H8 | epochs separate lifecycles | a grant from before `--reset-arena` is refused after it |
| H9 | receipts attribute | interleaved proposals: `appliedTo`, `resultRoot`, `proposalDigest` separate the run's writes |

______________________________________________________________________

## What lands where

| Repository | Change |
|---|---|
| LLO | epoch and sequence, snapshot publisher, v4, selection check and `--verify`, `current_root` and its consumers removed, `op_load` and the warm-start fallback removed; execution/v1 `rootDomain`, `appliedTo`, `resultRoot`, `proposalDigest` |
| mache | v4 control reader; `ArenaFlusher` removed in favour of daemon proposals |
| ley-line | net receiver proposes; payload becomes sub-manifest and blobs; reads `(epoch, seq)` |
| cloister | consumes the regenerated execution/v1 schema |

v4, the wire break and the execution/v1 additions ship in one LLO release; mache and ley-line move
to it together and pin the last v3 release until then.

## Open questions

- Whether a reflinked snapshot shares page-cache pages with its source (APFS, Linux), and whether
  `clonefile` captures pages written but not yet flushed by the writer.
- Have/want for transport.
- Windows: unlink-while-open does not hold; out of scope until a Windows consumer exists.
