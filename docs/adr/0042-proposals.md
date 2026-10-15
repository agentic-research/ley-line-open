# ADR-0042 — Proposals: a write is checked against what its author read, and applied whole or refused

**Status:** Proposed (2026-10-12); P1, P2 (file bases), the daemon ops and the mount's file bases
are implemented on the PRs listed under "Shipped so far"
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

### P3 — The closure check (extension; depends on ADR-0031)

A proposal may carry restriction-addressed keys (ADR-0031) for cross-file facts its author relied
on; the writer refuses if any changed. The only key family today is call-target resolution
(`sheaf/src/restriction_cache.rs:197-250`, test-only, not persisted, no scheme tag), which catches a
rename of a called function and not a signature or body change. P3 ships only after ADR-0031 is
accepted with persisted, scheme-tagged keys, and claims per key family only what it covers.

### P4 — How a proposal begins and ends

- **Explicit, over the daemon** (implemented) — `read_source {path}` returns content, `blob_hash`
  (the file's `_source.content_hash`, the value `propose` compares) and `tree_root`;
  `propose {writes: [{path, base, content}]}` requires `base`: a `blob_hash`, or `any` for no
  check. A refusal answers `refusal {reason, path, moved}`; on moved bases `reason` is `stale` and
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
  checked against B's read and not refused; (3) see the refusals a COMMIT makes, below.
- **Refusals a macOS client reports** (bead `bc5a6a`) — measured end to end with the server's
  request log on a macOS NFS mount: `write`+`fsync`+`close` of a node sends WRITE (UNSTABLE), COMMIT
  at `fsync`, and one GETATTR at `close`. A refused COMMIT (`NFS3ERR_IO` or `NFS3ERR_STALE`) is
  ignored and `fsync` returns success; an error on the GETATTR at `close` (`NFS3ERR_STALE`,
  `NFS3ERR_IO`, `NFS3ERR_NOSPC` tried) is ignored too, `close` returns success, and after
  `NFS3ERR_STALE` the client re-LOOKUPs the node; a COMMIT answered `NFS3_OK` with a new write
  verifier does not make the client resend its WRITEs. Only a refused WRITE reaches the agent: it
  surfaces as `EIO`/`ESTALE` at `fsync`. The mount therefore refuses at the WRITE every edit whose
  refusal it can decide there, and makes a refusal only the COMMIT can decide visible as a stale
  handle:
  - a WRITE or truncate to a node of a file while the mount has an edit to another node of that
    file staged is refused `NFS3ERR_IO` (typed `EditInProgress`, recorded in `_errors`): the staged
    edit's COMMIT moves the file under the second edit's base, so its COMMIT would be refused. The
    second node's edit lands once the first's COMMIT has;
  - an UNSTABLE WRITE that is a whole dirty range of its client — it starts at 0 or off a page
    boundary and ends off one — and leaves the node's file unparseable is refused `NFS3ERR_IO`, its
    edit (with any truncate before it) discarded and recorded in `_errors`. A client sends a range
    as one WRITE per buffer, at buffer boundaries, in any order (measured on macOS: 131072-byte
    chunks at multiples of 131072, the tail sent before chunks below it), so a WRITE that ends on a
    page boundary, or starts on one past 0, may be one chunk of several and is left to the COMMIT;
  - a refused COMMIT, a refused idle commit of a bare truncate, a refused commit before a
    structural operation, and the WRITE refusal above retire the node's file handle: the next
    operation on it answers `NFS3ERR_STALE`, the client re-LOOKUPs, and the new handle reads the
    stored node, so the client stops serving the refused bytes from its cache. The refusal is in
    `_errors` (recorded by the discard).

  Residual, not reported to a macOS client though nothing of the edit lands: a COMMIT refused
  because the file moved between the WRITE and the COMMIT; a syntax refusal of an edit whose WRITEs
  all end on page boundaries or that spans more than one chunk; a bare truncate (`: > f`, no
  WRITE) that does not parse; and any other write-path refusal at COMMIT. An edit that writes two
  disjoint ranges of one node past a buffer boundary is checked at the first range's WRITE, before
  the second arrives.
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
| NFS mount bases taken at READ, a stale WRITE refused, bases kept after a COMMIT | `6116c9` | #458 |
| NFS refusals at the WRITE: a second staged node, an unparseable whole range; a refused COMMIT retires the handle | `bc5a6a` | — |

## Falsifiers

| # | Claim | Test |
|---|---|---|
| G1 | a proposal is atomic | three files, one invalid: nothing lands (`a_proposal_with_one_invalid_file_changes_nothing`); a failure at the `derived_from` stamp leaves the image byte-identical (`a_write_that_fails_after_its_rows_leaves_the_arena_unchanged`) |
| G2 | a stale file base is refused | `a_stale_base_is_refused_and_names_what_moved`, `a_stale_refusal_names_every_moved_file`; over the daemon, `propose_applies_against_what_was_read_and_refuses_a_stale_base` |
| G2b | every refusal is typed and changes nothing | `every_refusal_is_typed_and_changes_nothing`, `propose_refusals_carry_their_reason_and_path`, `daemon_protocol_gate_proposals_produce_the_fixture_shapes` |
| G2c | a committed proposal is reported applied | `a_proposal_whose_head_write_fails_after_commit_is_applied`, `propose_reports_an_applied_proposal_as_applied_when_its_head_write_fails`, `the_mount_writer_answers_ok_when_the_edit_landed` |
| G2d | a rolled-back proposal leaves the head and sidecars readable | `a_rolled_back_proposal_leaves_the_sidecars_byte_identical`, `a_head_write_replaces_the_file_and_never_rewrites_it_in_place`, `a_failed_head_write_leaves_the_previous_head_readable` |
| G3 | the mount refuses a moved file | `a_refused_edit_restores_the_node_and_reports_stale`, `the_mount_writer_refuses_a_moved_file_as_stale`, `a_stale_refusal_answers_estale_and_others_eio` |
| G3b | an NFS refusal reaches the client | `an_unparseable_whole_write_is_refused_at_the_write`, `a_write_to_a_second_node_of_a_file_with_an_edit_staged_is_refused`, `a_refused_commit_retires_the_nodes_handle`; end to end on macOS, `fsync` raises `EIO` for an unparseable node edit |
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
