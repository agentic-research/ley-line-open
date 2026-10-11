# ADR-0040 — The write boundary: source and manifest are the data, the arena is a cache

**Status:** Proposed (2026-10-10)
**Bead:** `ley-line-open-e79c0f` (review ledger; the beads under "Next moves" carry the work)
**Related:**

- ADR-0026 (content-addressed pointer store — Phase 1 is retired by D5)
- ADR-0028 (content-addressed source blobs — becomes the content layer's only shipped table)
- ADR-0029 (CAS-backed workspace — its Manifest is the structure layer; its §2.2 write rule becomes D3)
- ADR-0032 (declared decompositions — D2's tagged fold and D3's co-attestation are reused; D1's
  `current_root` row is unchanged)
- ADR-0033 (CDC chunk-backed content — stays rootless; unaffected)
- ADR-0014 (capnp as protocol — the additive-field rule the `Head` change obeys)
- cloister ADR-0013 / ADR-0030 (slice grants and rings — the model this ADR applies to bytes)
- mache `mache-3451a1` (control v3 reader and writer), `mache-3465b9` (build-cache rekey on
  `(sourceHash, parserId)`), `mache-347ed1` (`node_child` joins behind a view before v7) — the
  consumer-side halves; mache's review of this design is reconciled into D5, D6 and D7 below

______________________________________________________________________

## Thesis

> One SQLite file is serving two products. For mache it is a regenerable index. For the
> writable mount, `leyline splice`, and cloister's execution workspace it is the only place an
> edited byte lives. Every root in the repository has been trying to name both, and every fix
> that improved one role broke the other.
>
> The data is the source bytes plus a manifest that maps each path to its source hash under a
> named parser identity. Its root is the one identity that signatures, receipts and transport
> cite. The arena is a cache keyed by that root. **Nothing writes the cache as authority.** A
> write from a mount, a splice or a confined run crosses one boundary: new blob, new manifest
> entry, checked at the crossing; the projection follows.

______________________________________________________________________

## Context — what the 2026-10-09 review established

Three read-only lenses (identity, publication state, frame) reviewed the storage layering at
`main` da774d2. Their findings were re-verified line by line before filing. Shipped behavior:

| Finding | Bead | Evidence |
|---|---|---|
| The daemon's writable mount edits a private deserialised copy and publishes it from there; `live.db` never receives the edit; the next snapshot republishes `live.db` and the mount drops the edit; a restart warm-starts without it | `192018` | `cmd_daemon.rs:660-672`, `fs/src/graph.rs:1455-1502`, `:1540+`, `cmd_daemon.rs:1321` |
| `Head.rootHash` names only the files a run re-parsed: unchanged files are skipped, segment files are truncated per run, the root folds over what remains; a no-op rerun hashes three empty segments | `0c80c7` | `cmd_parse.rs:1156-1211`, `:1421`, `:2604-2612`, `:2218+` |
| `Head.rootHash` commits to each file's absolute `canonicalPath`, `mtime` and `size` | `143002` | `source.capnp` @2/@4/@5, `cmd_parse.rs:2650-2662` |
| splice sets `_source.content_hash` to a hash with no `source_blobs` row; the bytes live only inline; the disk fallback is unverified | `0d3b72` | `ts/src/splice.rs:189-196`, `:96-112` |
| A mount write re-projects the file with `node_hash` NULL, no `node_content`/`node_child` rows, no `_ast_blob` row | `143f17` | `ts/src/splice.rs:170-196`, `ts/src/project.rs:24`, `schema/src/lib.rs:752` |
| `read_head_for_chain` fails open on an unreadable head, even with signatures required | `0c8ee7` | `cmd_parse.rs:2355-2380` |
| The arena header flips before `current_root` publishes; a fresh opener in the window reports corruption | `1eefe3` | `layout.rs:160-182`, `cmd_daemon.rs:1366-1390` |

And the measurements that started the question (bead `17c271` and a parse of this repository's
`rs/` tree with v0.20.0): source bytes are 8 MB of a 361 MB arena; the same syntax tree is stored
three times (`capnp_blobs` 100 MB, the `.ast.capnp` segment log 111 MB, the SQL rows and indexes
240 MB); two of the three copies carry authority (`Head.rootHash`, `current_root`); nothing reads
the first copy; the third is what every consumer uses. A single-file save with a mount attached
performs six image-sized copies and two full hashes (`af6c9d`): two are the cost of hashing an
image, four are the mount adapter deserialising its own copies.

The findings are one defect seen from seven angles: an outer ring is writing an inner ring's
cache as if it were authority, and the identities were defined over the cache.

______________________________________________________________________

## The model — rings, applied to bytes

cloister already uses this shape for credentials. ADR-0013's slice grant is the inner ring: a
bundle holds a capability scoped to one slice and reaches nothing else, enforced at the isolate
boundary. ADR-0030 adds an outer ring per tenant. The rule that makes rings work: authority lives
in the innermost ring, every outer ring holds a narrowed grant, and a write from an outer ring is
a request that crosses the boundary and is checked there.

| Ring | Holds | Writes | Reads |
|---|---|---|---|
| **0 — the store** | `source_blobs` (bytes by BLAKE3) and the manifest (path → source hash, parser identity, projection version), with its signed root | the single manifest writer | anyone holding a blob hash or a manifest |
| **1 — the projection** | the arena: `nodes`, `_ast`, `node_child`, `node_content`, refs/defs, `_lsp*`, indexes; the control block that publishes it | the projection worker, from ring 0 only | every query, mount, LSP, mache |
| **2 — the workloads** | a slice grant: the sub-manifest of paths this mount, agent or confined run may advance | proposals across the boundary: `(path, new blob)` inside the grant | the projection, through ring 1 |

A write acknowledged to a ring-2 client is durable when ring 0 holds the blob and the manifest
entry. That is also exactly the state a restart recovers, which closes `192018` by construction.

______________________________________________________________________

## Decision

### D1 — The tree root is a new identity beside `Head.rootHash`, not a redefinition of it

`Head.rootHash` is a per-run receipt over the segments the run wrote. It is left as that (or
retired later; either is fine) and is never again described as the content identity of a tree.

A new additive `Head` field, `treeRoot`, with its own scheme tag folded into the digest per
ADR-0032 D2:

```
treeRoot = A("leyline/tree-root/v1",
             parserId,
             sorted by relative path: (relativePath, sourceBlobHash)…)
```

Produced by the parse after COMMIT from `_source` and `source_blobs`, so it covers the whole
corpus regardless of how many files the run re-parsed. Carries no absolute path, no mtime, no
size. Verified by a command that recomputes it from the arena and refuses on mismatch; mache and
cloister may run that verifier. `current_root` is unchanged (ADR-0032 D1, integrity row).

`parserId` is itself a tagged fold over grammar versions, `IR_SCHEMA_VERSION`, the extraction,
injection and query-set epochs, and the effective query set including arena-resident overrides
(`_queries`). Two arenas with different overrides must not share a `(sourceHash, parserId)` key.

### D2 — The manifest is ADR-0029's object, under a new name for its root

ADR-0029 §2.1's `Manifest { entries: [(path, blob_hash, mode)] }` is the structure layer. Three
other things are called "manifest" in this repository (ADR-0032's CDC `manifestRoot` over image
chunk regions, `net.capnp`'s signed frame header, ADR-0029's). The root of this one is `treeRoot`
and nothing else; the word "manifest" is not used for a root. A sub-manifest over a subset of
paths is the same object, smaller: it is the slice grant of ring 2 and the transport unit.

### D3 — One write API, called by every writer

`write(grant, path, bytes) -> (sourceBlobHash, treeRoot')`:

1. refuse if `path` is outside `grant`;
2. `INSERT OR IGNORE INTO source_blobs` the bytes under their hash;
3. advance the manifest entry for `path`; recompute `treeRoot`;
4. re-project the one file into ring 1 through the same per-file projection the cold parse uses
   (`node_hash`, `node_content`, `node_child` with field names, `_ast_blob` until D5 retires it);
5. publish.

The FUSE/NFS mount, `leyline splice`, `batch_splice`, and cloister's mediator all call it. No
writer touches `_source.content`, `nodes.record` or the arena directly. ADR-0029 §2.2 said this
and no code did it; this ADR makes it the only write path.

### D4 — The in-process mount reads `live.db` through the WAL reader pool

The daemon's socket readers already read the live WAL database through N pooled connections with
zero copies (`daemon/db_pool.rs`). The mount does the same instead of deserialising a private
copy; its writes go through D3 to the daemon's single writer. This removes four of the six copies
per save and the dual-writer divergence without touching any identity. `snapshot_to_arena` and
`current_root` remain the publication for out-of-process consumers (mache, FFI, warm start).

### D5 — One stored copy of the syntax tree

`capnp_blobs` and `_ast_blob` are retired (ADR-0026 Phase 1 reversed; its §7 kill criteria were
never evaluated and its Phase 2 read measurement was never built). `AstNode.sourceId` leaves the
wire struct so the segment record is content-pure; blob hashes and `rootHash` move once at the
next generation. Whether the segment log itself survives is decided by its one consumer: mache
reaps it as cache today. If a flat stream is wanted for transport it is an export, with no root.
The content-addressed semantic structure the Σ substrate wants already exists and is path-free:
`node_hash` over `node_content` and `node_child`, which mache joins on. Hashing a recomputable
log that no consumer recomputes is a reproducibility check, not authority.

Two constraints from mache's review of this ADR:

- **`bindings.capnp` has a consumer.** mache reads neither `head.capnp` nor `ast.capnp` nor
  `source.capnp` (it only reaps them), but `internal/lsp/binding_log.go` reads `.bindings.capnp`.
  A v7 change to that segment is a cross-runtime contract change. The `op_get_db_path` doc comment
  (`daemon/ops.rs:1405`) claiming mache reads the ast and source segments is stale and is corrected
  under `f30fdf`.
- **If a derivation store ships at all, it is addressed by its own output hash.** A key of
  `(sourceHash, parserId)` names inputs, so a receiver that fetches a derivation cannot verify it
  without re-deriving it. The store holds objects by output hash, plus claims
  `(sourceHash, parserId) → outputHash` signed with the head key. The transport layer wants
  output-hash identities regardless. Given that a fetched AST saves about 4% of a rebuild, the
  default under this ADR is that no derivation store ships; the key discipline is recorded so that
  the first one to ship is built correctly.

A size lever to measure before interning: `fold_children` (`cmd_parse.rs:3229`) folds every
non-`extra` child, anonymous as well as named, into `node_child`, while `_ast` and `nodes` carry
only named children. A named-only `node_child` projection may remove more bytes than interning its
hashes (`9b7d28`); the anonymous share on the kibana slice is unmeasured and is the first number
`f30fdf` records.

### D6 — The arena is declared derived, and says from what

`_meta` records `derived_from = (treeRoot, parserId, projection_schema_version)`. `leyline
rebuild` reproduces the projection from ring 0. The rebuild gate targets the logical row set with
the known non-functions canonicalised (`nodes.mtime`, `_meta.parse_time`, absolute `_source.path`)
and excludes `_lsp*` (external toolchain output) and `current_root` (an image hash). A consumer
that wants to trust a transported arena compares its `derived_from` to the `treeRoot` it holds.

`file_id` is arena-local: `ensure_file_id` (`ll-core/schema/src/lib.rs:347`) is an
`INSERT OR IGNORE` sequence in each database, and `nid = (file_id << 24) | ordinal`. A
sub-manifest arena therefore numbers its files differently from the full arena, so no row image
containing a nid can match across them byte for byte. The per-file derivation stores ordinals
only; `file_id` is applied at projection time; F3 and F5 compare row images modulo `file_id`
(equivalently, with nids rewritten to `(relative path, ordinal)`). `file_id` is not derived from a
path hash: with 39 usable bits, birthday collisions are expected around a million files.

### D7 — Publication stays as it is until the write boundary exists

Replacing image publication with in-place WAL updates and reader reopen changes the authority-
making operation to the WAL commit and turns the control block into a notification. That needs a
declared replacement for `current_root` (an epoch plus commit sequence), a v4 control block, a
replacement for the verify-on-load gate, and answers for inode replacement under cross-process
readers and checkpoint starvation under long readers. It is a separate ADR, after D3 and D4 are
shipped, and it lands in mache in lockstep. Control block v3 ships in v0.20.1 now; the block
changes once more, later.

The candidate that ADR starts from, proposed by mache's review and already shipped on mache's
side for `mache build` (mache PR #727, `fsutil.Publish`): **immutable snapshot files instead of
shared-WAL readers.** The writer keeps `live.db` in WAL. To publish it checkpoints, clones the
file (APFS `clonefile` and Linux `FICLONE` are O(1); ext4 falls back to one copy), renames the
clone into place, then writes path and root to the control block. Readers open the snapshot with
`immutable=1` and `mmap_size` at least the file size: no shared-memory locks, no per-page read,
one image shared in the page cache across processes. Reclamation is the filesystem's: an unlinked
snapshot stays readable until its last reader closes it, which removes the checkpoint-starvation
and inode-replacement questions from the open list. The two remaining image copies per save become
one clone, and zero on filesystems that reflink. mache's own `ArenaFlusher`
(`graph/arena_writer.go`) has the same header-before-root window as `1eefe3`; mache's readers do
not compare payload to root today, so only an LLO reader of a mache-written arena can hit it. Both
writers adopt the v4 protocol together.

______________________________________________________________________

## Falsifiers

Each is a test that fails on `main` today or would fail if the decision were wrong.

| # | Claim | Test |
|---|---|---|
| F1 | `treeRoot` is a tree identity | four parses of the same bytes — a second directory, touched mtimes, shuffled discovery order (f4c), one no-op scoped reparse — yield one `treeRoot`; `rootHash` differs across them today (`0c80c7`, `143002`) |
| F2 | a write is durable at the boundary | daemon with mount: write through the mount, fsync, reparse another file, read back through the mount and the socket, restart, read again; all return the written bytes (`192018`) |
| F3 | the projection is rebuildable after writes | cold parse, N writes through D3, `leyline rebuild` into a fresh arena; the logical row sets agree and so do `treeRoot` and `derived_from` (`143f17`, `0d3b72`) |
| F4 | content keys are location-free | two arenas containing a byte-identical file share that file's `(sourceHash, parserId)` derivation and its `node_hash` set; today `capnp_blobs` holds two blobs (F4 of the identity review) |
| F5 | a sub-manifest transports | a sub-manifest of N paths plus their blobs, parsed on a clean machine, produces the same N file-local row images the full arena holds for those files, compared modulo `file_id` (D6) |
| F6 | the grant is enforced | a write to a path outside the grant is refused with no side effect on ring 0 or ring 1 |
| F7 | the head chain fails closed | with signatures required, a truncated `head.capnp` makes the parse error and write nothing (`0c8ee7`) |

______________________________________________________________________

## Sequencing

1. **v0.20.1** with control block v3, so mache has a tag to pin. No storage change.
2. **D1**: `treeRoot` + `parserId` + verifier; F1, F7. No table change, no mache change.
3. **D4**: mount reads `live.db` via the pool, writes via the daemon writer; F2. Closes `192018`,
   `af6c9d` in part, `af79bb`.
4. **D3**: the one write API; every writer routed; F3, F6. Closes `0d3b72`, `143f17`, `918a75`'s
   silent no-op (the API either works or refuses).
5. **D5 + Phase C** (`6ee4c4`, projection-v7): `sourceId` leaves `AstNode`; `capnp_blobs` and
   `_ast_blob` retired; nid carry across cold parses; `node_child` interning (`9b7d28`) rides the
   same projection break; F4. Lands with mache's migration of its hash joins.
6. **D6**: `derived_from` and `leyline rebuild`; F5.
7. **D7**: the publication ADR and control block v4, with mache.

______________________________________________________________________

## Consequences

- `17c271`'s size clause is reframed: with `capnp_blobs` gone the arena is a cache whose shape
  is free to change without moving an identity; 66× becomes ~43× on the kibana slice before
  interning and ~35× after.
- The memory profile: resident images per mounted daemon go from four to one (D4), and whole-
  image copies per save go from six to two now and to zero under D7.
- execution/v1 receipts gain a root with a defined referent (`treeRoot`, scheme-tagged) to cite;
  their `DigestRef` must carry a domain tag before `current_root` is ever demoted, which D7 is
  the first to need.
- ADR-0026 is amended: Phase 1 retired, Phase 2 and 3 withdrawn. ADR-0029 is unblocked: its
  manifest has a shipped root and a write rule with code behind it. ADR-0032 is partially
  shipped: D2's tagged fold and D3's additive co-attested field, with `treeRoot` where it
  expected `manifestRoot` and `logicalRoot` to follow.
- Documentation drift to fix with D1: `ARCHITECTURE.md` lists ADR-0026 and ADR-0028 as Accepted
  while the files say Proposed.

______________________________________________________________________

## Next moves (beads)

| Bead | Decision | Closes |
|---|---|---|
| `f2df7f` | D1 — `treeRoot`, `parserId`, verifier; F1, F7 | `0c80c7`, `143002` (as tree identity), `0c8ee7` |
| `f2ee9f` | D4 — mount reads `live.db` via the pool, writes via the daemon writer; F2 | `192018`, `af79bb`, four of `af6c9d`'s six copies |
| `f2ffbd` | D3 — the one write API; F3, F6 | `0d3b72`, `143f17`, `918a75`'s silent no-op |
| `f30fdf` | D5 — `sourceId` leaves `AstNode`; `capnp_blobs` and `_ast_blob` retired; projection-v7 with `6ee4c4` and `9b7d28`; F4 | `17c271`'s size clause (reframed) |
| `f31efd` | D6 — `derived_from` and `leyline rebuild`; F5 | — |
| `d70f99` | D7 — the publication ADR, control block v4 with mache | `1eefe3`, the remaining two copies of `af6c9d` |
| `e79c0f` | the review ledger this ADR answers; closes when this ADR is accepted | — |

______________________________________________________________________

## Non-goals

- Cross-head merge. Two grants advancing the same path is a conflict surfaced at the boundary,
  not resolved by it.
- Replacing SQLite as the projection. The defect was hashing and copying the image as identity,
  not the engine.
- A capnp derivation store to replace `capnp_blobs`. The team's own numbers show a fetched AST
  saves about 4% of a rebuild; the unit worth caching is the per-file row image, which file-local
  nids make relocatable.
- Signed-head authority (who may advance a manifest). Upstream of this ADR; cloister's grant
  model is the intended answer.
