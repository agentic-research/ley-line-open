# ADR-0042 — Proposals: a write is checked against what its author read, and applied whole or refused

**Status:** Proposed (2026-10-12); P1, P2 (file bases), the daemon ops and the mount's file bases
are implemented on the PRs listed under "Shipped so far"; amended 2026-10-14 with P1a (create,
delete, rename)
**Bead:** `ley-line-open-d70f99` (design); implementation beads below
**Related:**

- ADR-0040 (the write boundary; D3's one write path is the one-file case of a proposal)
- ADR-0041 (publication — snapshots show whole proposals; split from this ADR)
- ADR-0034 (`node_hash`, and its amendment on twin constructs)
- ADR-0031 (restriction-addressed keys — Proposed; P3 depends on it)
- `schema-spec/execution/v1` (`WorkspaceGrant { graphRoot, expectedGeneration, operations }`)
- cloister ADR-0046 (mediated capabilities), ADR-0049 (host runtime and mediator) — policy stays in
  cloister; this ADR is mechanism

ADR-0040 cites cloister ADR-0013 and ADR-0030 as the model for slice grants. Those govern credential
slices in the vault and process isolation; they confer no authority over code. The grant that
scopes a code write is execution/v1's `WorkspaceGrant`, verified in LLO, with policy authored in
cloister under ADR-0046 and ADR-0049.

______________________________________________________________________

## Thesis

> Many agents write to one codebase. Without a check, each write lands as soon as it is made: the
> last writer wins, a stale overwrite is silent, and a change that spans files lands one file at a
> time. A **proposal** is the step between a change and its application: it names the bytes its
> author saw, and the one write boundary applies it whole or refuses it whole.

rosary does the pessimistic, file-grained form today: required scopes, a pre-dispatch overlap check,
one agent per repository, a worktree each, a pull request to merge. Proposals are the optimistic,
fine-grained form — two agents may share a file; a real conflict is refused when it happens.

______________________________________________________________________

## Decision

### P1 — A proposal is the unit of change, and one SQLite transaction

```
propose(grant, writes) -> Accepted { blobs, tree_root, post_commit_error } | Refused(Refusal)

writes = [ Proposed { path, base, bytes } ]
base   = Any | Blob(hash of the bytes the author read)
```

The single writer refuses a proposal before touching anything when it writes nothing, a path is
outside the grant, names no file or repeats, a base no longer matches or names a blob the arena
does not store, the files are under different roots, or a file does not parse. A refusal is typed
(`Refusal`, one reason per case) and a stale one names every file that moved. Otherwise it applies
every file in one parse pass and one SQLite transaction, which includes the sweep and the `_meta`
stamps (`derived_from`). The new `ast.capnp` / `source.capnp` sidecars are written beside the old
ones and renamed into place after `COMMIT`, so a proposal that rolls back leaves them unchanged.
Only `CREATE INDEX IF NOT EXISTS`, which changes no query result, the sidecar renames and the head
write run after `COMMIT`; the head is written to a temp file, synced and renamed. A step after
`COMMIT` that fails does not make the proposal a failure: it is applied, and the failure is
reported beside it (`post_commit_error`). A proposal is acknowledged only after its WAL frames are
synced (`synchronous=FULL`). D3's `write(grant, path, bytes)` is the one-file `Base::Any` case.

### P2 — Bases are compare-and-swap over the bytes the write replaces

- **File** (implemented) — the file's `_source.content_hash` must equal the base. This is HTTP
  `If-Match` with a content hash as the entity tag. A refusal is typed (`Stale`, naming every moved
  file's `{ path, expected, current }`). A file with no stored blob (a NULL `content_hash`)
  matches no blob base.
- **Node** (not implemented) — `base = (file_blob, locator, span_hash)`: the file blob the author
  read, the node's locator there, and BLAKE3 of the node's exact byte span. The writer re-resolves
  the locator into the current file and refuses unless it resolves to exactly one node whose span
  hashes to `span_hash`; the replacement is spliced into the current bytes and syntax-checked.
  `node_hash` is not a base: it excludes comments, whitespace, sibling attributes and non-UTF-8 leaf
  bytes (ADR-0034 D4; `cli-lib/tests/ir_producer.rs:254`), so a stale edit would overwrite a
  concurrent comment or attribute change. Twin constructs (same kind, name and `node_hash`,
  distinguished only by an attribute such as `#[cfg]`) have no stable locator (ADR-0034 amendment):
  ambiguity is refused. The locator rule needs its own specification and tests before node bases
  ship.
- **Coarse** (not implemented) — a proposal naming no finer base is refused unless the current
  `treeRoot` equals the grant's `graphRoot`. `expectedGeneration` is defined by ADR-0041's per-
  proposal `(epoch, seq)`.

There is no merge: a conflict is refused and named; resolving it is the proposer's job.

### P1a — Amendment (2026-10-14): a proposal creates, deletes and renames files

Bead `ley-line-open-47fb03` (ADR-0044 phase B2). Agents work through an ordinary filesystem: they
create files, delete them, and editors and agent harnesses save by writing a temporary file and
renaming it over the original. A proposal that can only replace an existing file's bytes cannot
carry any of these, so the write path grows three kinds of change at a path:

```
writes = [ Proposed { path, base, change } ]
base   = Any | Blob(hash) | Absent
change = Write(bytes) | Delete

replace = Write  against Any | Blob   (the only kind before this amendment)
create  = Write  against Absent       refused stale when the path holds a file
delete  = Delete against Any | Blob   refused stale when the file moved or is gone
```

- **Absent is a base.** A create's author read no file at the path; the compare-and-swap of P2 is
  extended with "no file" on either side. A stale refusal's `{path, expected, current}` names
  `absent` where a side holds no file: a create over a file is `{expected: absent, current:
  blob}`, a replace or delete of a file deleted since it was read is `{expected: blob, current:
  absent}`. `Delete` against `Absent` is refused `malformed_base`.
- **A rename is one proposal**: the delete of the old path against the blob it held, and the
  create of the new path (or the replace of the file already there, against that file's blob)
  with the same bytes. It is applied whole or refused whole (P1), so a rename whose create is
  refused leaves the old path in place.
- **One transaction.** Deleted files' rows leave in the projection's transaction, through the
  sweep the incremental parse already runs for a file gone from disk (`delete_file_rows`); a
  deleted file is out of the pass's tree whether or not the directory still holds it. A created
  file is parsed as a cold parse parses it and gets a `file_id` of its own. `treeRoot` is folded
  from the committed `_source` set, so it covers creates and deletes, and equals the cold parse's
  root of the resulting tree.
- **What the store can hold.** The store holds files in languages the parse projects. A create
  is refused before anything changes when its path is not a plain relative path
  (`malformed_path`), names no projected language (`no_language`), or is under a file or is the
  directory of another file once the proposal's deletes are applied (`path_conflict`). A daemon
  started with `--language` refuses a create in another language (`no_language`): its checkout
  would never write the file out, and its next whole-directory parse would drop it. A proposal
  that names no stored file takes its tree root from the root the store's last parse recorded
  (`_meta.source_root`).
- **The checkout (ADR-0043 R3) carries "no file" as a value on every side.** A path the store
  deleted whose directory file still equals the index is removed from the directory (unlink, then
  `fsync` of its directory; emptied directories go with it), and its index entry dropped; a
  directory edit to it is a conflict that keeps both sides, and a whole-directory parse does not
  ingest it back. Removals run before materializations, so a file that becomes a directory of
  files lands. A crash between the commit and the unlink is carried out by the next reconcile.
- **The daemon wire** (`daemon.capnp`): `ProposedWrite` gains `kind :WriteKind` (`replace`,
  `create`, `delete`; absent reads as `replace`, so existing clients behave as before), `base`
  accepts `absent` for a create, and `content` is optional for a delete. `RefusalReason` gains
  `no_language`, `malformed_path` and `path_conflict`. A replace or create without `content` is a
  malformed request, not an empty file.

Directories are not stored: a directory is implied by the files under it. The virtio-fs core
(`leyline-storefs`, ADR-0044 V2) keeps the directories a guest makes, and the created files the
store cannot hold (an editor's temp file), for its session only; their place in this ADR is the
write-temp-then-rename case above, which reaches the store as one proposal.

### P3 — The closure check (extension; depends on ADR-0031)

A proposal may carry restriction-addressed keys (ADR-0031) for cross-file facts its author relied
on; the writer refuses if any changed. The only key family today is call-target resolution
(`sheaf/src/restriction_cache.rs:197-250`, test-only, not persisted, no scheme tag), which catches a
rename of a called function and not a signature or body change. P3 ships only after ADR-0031 is
accepted with persisted, scheme-tagged keys, and claims per key family only what it covers.

### P4 — How a proposal begins and ends

- **Explicit, over the daemon** (implemented) — `read_source {path}` returns content, `blob_hash`
  (the file's `_source.content_hash`, the value `propose` compares) and `tree_root`;
  `propose {writes: [{path, base, content, kind}]}` requires `base`: a `blob_hash`, `any` for no
  check, or `absent` for a create (`kind` is `replace` by default, `create` or `delete`; P1a). A refusal answers `refusal {reason, path, moved}`; on moved bases `reason` is `stale` and
  `moved` lists every `{path, expected, current}`. An applied proposal answers `ok` and, when a
  step after its commit failed, `post_commit_error`. Both are MCP tools; `propose` is in its own
  cloister group, `write`. This is the only form that checks against what an agent actually
  read.
- **Implicit, through the mount** (implemented for files) — an edit's base is the hash of the
  file's stored bytes. FUSE takes it per open handle, when the file is opened for writing. A stale
  flush is refused and its edit discarded: FUSE `ESTALE` at `fsync`/`close`, NFS `NFS3ERR_STALE`
  (below). POSIX writes do not say what the client read, so a FUSE client that reads, closes and
  reopens to write takes its base at the reopen.
- **Mount bases over NFS** (bead `6116c9`) — NFSv3 has no open and names no client, so every edit
  is the mount handle's, with one base per file. A client holds UNSTABLE writes until `fsync` or
  close, so the server can first see a WRITE after another writer moved the file; a base taken at
  that WRITE would be the moved file and the edit would land over a change the client never read.
  The base is therefore taken no later than the server's READ:
  - every NFS READ of a node of a parsed file, while the mount handle has no edit to that file in
    progress, re-takes the file's base from its stored bytes before the bytes are served (a change
    landing in between leaves the base older than what the client got, so the edit is refused);
  - a READ while an edit to the file is in progress leaves the base the edit was made against;
  - a landed COMMIT sets the base to the bytes it wrote (the client's cache holds them); a refused
    edit keeps the base, so a retry from the client's cache with no READ in between is refused
    again;
  - a WRITE or truncate with no base yet takes one from the stored bytes, as before.

  A WRITE or truncate (SETATTR size) to a file whose stored bytes no longer hash to the base is
  refused `NFS3ERR_STALE` before it is staged; a COMMIT whose file moved after its WRITEs is refused
  `NFS3ERR_STALE` and its edit discarded. The WRITE refusal is the one the client sees: measured end
  to end against a macOS NFS mount (bead `6116c9`), a failed WRITE surfaces as `ESTALE` at `fsync`, while a failed
  COMMIT — `NFS3ERR_STALE` or `NFS3ERR_IO` — is ignored and `fsync` and `close` return success.

  What is checked: an edit is staged, and lands, only if the file holds the bytes it held at the
  client's last server READ of any node of that file with no edit in progress, or at the mount's
  last landed edit to it, whichever came later. The mount holds one base per file it has served,
  replaced in place; no timer drops it. Residual cases: (1) a client that sent no READ for the file
  to this server and made no landed edit to it — a page cached from a previous mount or a previous
  server process — gets its base at the WRITE, and a change landed between the client's read and
  that WRITE is overwritten, not refused; (2) the base is per file, not per node, so a client that
  edits node A from a cache older than its latest server READ of node B of the same file is
  checked against B's read and not refused; (3) a refusal only the COMMIT can make — the file moved
  between the WRITE and the COMMIT (another node of the same file committed first), or the edit
  does not parse — discards the edit, but a macOS client does not report it.

  **Amended by ADR-0045 T8** (bead `53d701`): a moved base merges, so the WRITE no longer refuses
  every moved base. It runs the COMMIT's own decision on the edit it stages and refuses exactly what
  that decision refuses; an unparseable edit lands with a diagnostic. Residual (3) narrows to a
  conflicting change landed between the client's last WRITE and its COMMIT. The bases above, taken
  at READ, still name what the client read, and each edit is now applied to those bytes.
- **Explicit, through the mount** (not implemented) — `.staging/` (in `leyline-vcs`, unmounted
  today) served by the mount; writes under it record bases at first read; `.commit` proposes them.
- **Lifecycle points** (not implemented) — a jj or git commit, the end of a confined run (cloister's
  overlay upper layer is the proposal, its lower layer the snapshot it read).

### P5 — A refusal is visible and discarded

A refused proposal is never reported as success. Through the mount, the refused edit is discarded
(owner decision, 2026-10-12): the stored file is re-projected so the node reads as it, the file's
pending edits are dropped, and the reason is recorded in `_errors` for the node. FUSE `fsync`
returns the refusal instead of reporting success. mache's draft-on-invalid behaviour remains mache's
choice as a client.

______________________________________________________________________

## Shipped so far

| Slice | Bead | PR |
|---|---|---|
| a write is one transaction, stamps included | `1360fd` | #437 |
| acknowledged writes are synced | `0a9931` | #438 |
| `propose` with file bases, typed `Stale` | `d25ec4` | #439 |
| daemon `read_source` / `propose`, MCP, schema 0.20.3 | `609525` | #440 |
| mount bases, `ESTALE`, refused edits discarded, fsync reports refusals | `843e03` | — |
| typed refusals, required `base`, applied-after-commit, atomic head and sidecars, schema 0.20.5 | `5db28e`, `5dc3a5` | — |
| NFS mount bases taken at READ, a stale WRITE refused, bases kept after a COMMIT | `6116c9` | — |
| create, delete and rename proposals; the checkout removes deleted files; `WriteKind` on the wire (P1a) | `47fb03` | — |

## Falsifiers

| # | Claim | Test |
|---|---|---|
| G1 | a proposal is atomic | three files, one invalid: nothing lands (`a_proposal_with_one_invalid_file_changes_nothing`); a failure at the `derived_from` stamp leaves the image byte-identical (`a_write_that_fails_after_its_rows_leaves_the_arena_unchanged`) |
| G2 | a stale file base is refused | `a_stale_base_is_refused_and_names_what_moved`, `a_stale_refusal_names_every_moved_file`; over the daemon, `propose_applies_against_what_was_read_and_refuses_a_stale_base` |
| G2b | every refusal is typed and changes nothing | `every_refusal_is_typed_and_changes_nothing`, `propose_refusals_carry_their_reason_and_path`, `daemon_protocol_gate_proposals_produce_the_fixture_shapes` |
| G2c | a committed proposal is reported applied | `a_proposal_whose_head_write_fails_after_commit_is_applied`, `propose_reports_an_applied_proposal_as_applied_when_its_head_write_fails`, `the_mount_writer_answers_ok_when_the_edit_landed` |
| G2d | a rolled-back proposal leaves the head and sidecars readable | `a_rolled_back_proposal_leaves_the_sidecars_byte_identical`, `a_head_write_replaces_the_file_and_never_rewrites_it_in_place`, `a_failed_head_write_leaves_the_previous_head_readable` |
| G3 | the mount refuses a moved file | `a_refused_edit_restores_the_node_and_reports_stale`, `the_mount_writer_refuses_a_moved_file_as_stale`, `a_stale_refusal_answers_estale_and_others_eio` |
| G3a | creates and deletes land as a cold parse of the resulting tree | `a_create_lands_the_new_file_as_a_cold_parse_of_the_tree_holding_it`, `a_delete_removes_the_files_rows_as_a_cold_parse_of_the_rest`, `a_rename_is_one_proposal_that_lands_as_a_cold_parse_of_the_renamed_tree` |
| G3b | a structural proposal is atomic and its bases hold | `a_rename_whose_create_is_refused_leaves_the_source_in_place`, `a_create_over_a_file_and_a_change_to_a_deleted_file_are_stale`, `a_create_the_store_cannot_hold_is_refused`, `structural_proposals_are_refused_typed` |
| G3c | the checkout carries creates and deletes, across a restart | `a_proposed_create_is_materialized_and_a_proposed_delete_removes_the_file`, `a_create_and_delete_committed_before_a_crash_are_carried_out_at_restart`, `a_store_delete_against_a_directory_edit_is_a_conflict_that_keeps_both` |
| G4 | node bases allow disjoint edits | (open) two node-based proposals on different functions of one file both land |
| G5 | node bases cover trivia | (open) a comment edit inside F refuses a stale node base on F |
| G6 | twins refuse rather than misdirect | (open) `#[cfg]` twins after an ordinal shift |
| G7 | closure check | (after ADR-0031) a rename refuses a call added against the old name |

## Prior art

Compare-and-swap on an entity tag is HTTP `If-Match`. Per-definition versioning goes back to
ENVY/Developer and Monticello; Darcs and Pijul detect conflicts when a patch is applied; Unison
content-addresses definitions in its own language; structural merge tools (mergiraf,
SemanticMerge, IntelliMerge, Spork) merge after the fact. What this ADR adds is conflict decided at
a live filesystem and socket boundary between concurrent agents, over existing many-language code
projected by tree-sitter. A prior-art search is owed before the claim is made outside this
repository.

## Non-goals

- Merging.
- Authorisation policy: grants are issued by cloister and verified by LLO.
