# ADR-0045 — The store is a git-shaped tree, parsing is a projection over it, and a moved base merges

**Status:** Accepted (2026-10-15, owner direction); phases 1–5 built (below)
**Bead:** `ley-line-open-bb8aa6`; T8 `ley-line-open-53d701`
**Amends:** ADR-0042 (P1a "what the store can hold", P2 "there is no merge", P4/P5 syntax
refusals, the NFS mount bases of `6116c9`), ADR-0044 (V2 refusal errnos, V3 "Read freshness":
WRITE-time `ESTALE`, V4 the host mount's write rules)
**Related:**

- ADR-0040 D1 (`treeRoot`, `parserId`), D3 (one write path), D6 (`derived_from`, `leyline rebuild`)
- ADR-0043 (one store; R3 checkouts and their index; R4 gix for git interop)
- `ley-line-open-e650d7` / #466 (WRITE-time `ESTALE`, reversed here),
  `ley-line-open-e6780e` (build outputs excluded from the walk)

______________________________________________________________________

## Context

**Requirement (owner, 2026-10-15).** The filesystem serves many agents at once. Their changes
must be aware of each other: a file is reassembled from per-agent diffs without breaking it,
and the store, which is already content-addressed and merkle-shaped like git, must hold what a
git tree holds.

**What the store holds before this ADR** (`source_write.rs`, `cmd_parse.rs`, `checkout.rs`, the
storefs README "Not supported"):

- Only files in a language the parse projects. `README.md`, a notes file or a `Makefile` live
  only in a storefs session and vanish with it; a socket `create` of one is refused
  `no_language`.
- No modes: `chmod +x` through storefs is `EPERM`, and the checkout keeps whatever permissions
  the directory file had. No symlinks. Directories are implied by the files under them, so an
  empty directory exists only in a storefs session, and renaming a non-empty directory is
  `EXDEV`.
- An unparseable save is refused (`Refusal::Syntax`, `EIO` through storefs). An editor's
  half-typed buffer, saved, is lost to the agent that typed it.
- Any change since a writer's base refuses its save (`ESTALE`), even when the two edits touch
  different lines. With many agents in one tree, most refusals are of this kind.

## Decision

### T1 — Every path holds any bytes, with a git mode; directories are entries

The store's manifest is the table `_tree`:

```sql
CREATE TABLE _tree (
    path      TEXT PRIMARY KEY,  -- relative, '/'-separated, plain (no '', '.', '..')
    mode      INTEGER NOT NULL,  -- git mode, below
    blob_hash BLOB,              -- source_blobs key; NULL for a directory
    mtime     INTEGER,           -- stat cache of the walk; not identity
    size      INTEGER            -- stat cache of the walk; not identity
) WITHOUT ROWID;
```

| mode | git name | blob |
|---|---|---|
| `0o100644` | regular file | the file's bytes |
| `0o100755` | executable file | the file's bytes |
| `0o120000` | symbolic link | the link's target, as bytes (git's encoding) |
| `0o040000` | directory | none |

Every directory is an entry, and the set is closed under parents: an entry's parent directories
are entries. A file write that creates a path creates its missing parent directories in the
same proposal. Deleting a file never deletes a directory: a directory leaves only by `rmdir`
(T4), by a directory rename, or by a whole-directory parse that no longer finds it. This is
POSIX's rule, not git's; git cannot represent an empty directory and LLO can.

`_source` stays the projection's file registry: one row per file the parse projected, with
`content_hash` equal to the `_tree` entry's `blob_hash`. Every `_source` row has a `_tree` file
entry; a `_tree` file entry has a `_source` row only when the parse projects it (T3).

The store holds bytes of any size; `MAX_PARSE_FILE_SIZE` limits projection only.

### T2 — `treeRoot` folds the manifest: mode and directories are identity

`treeRoot` (ADR-0040 D1, scheme `leyline/tree-root/v1`) names the manifest's bytes: the set of
`(path, mode, blob)` entries of `_tree`. The fold:

- **Leaves** are every file entry (any mode but `0o040000`) and every **empty** directory (a
  directory entry with no entry beneath it). A directory with entries beneath it is not a leaf:
  it is implied by them, and the parent-closed directory set is recovered from the leaves, so
  the fold is lossless.
- Leaves sort by path bytes. Leaf `i` contributes two `Entry`s, as before: `{addr:
  BLAKE3(path), a: 2i, b: len(path)}` and `{addr: content, a: 2i+1, b: mode_tag}`.
- `content` is the blob hash for a file and BLAKE3 of the empty byte string for an empty
  directory (git's empty tree, in LLO's hash).
- `mode_tag` is `0` for `0o100644` and the git mode as an integer otherwise (`0o100755` =
  33261, `0o120000` = 40960, `0o040000` = 16384).
- Params stay `parserId`; the domain and scheme tag do not change.

**How `treeRoot` changes.** A tree whose every file is `0o100644`, which holds no empty
directory, and whose every file the parse projects, folds to the same root as before this ADR:
`mode_tag` is `0`, exactly the `b` every v1 content entry carried. The root moves for a tree
that holds an executable, a symlink, an empty directory, or a file the parse does not project
(it is in the manifest now; it was not before). The scheme tag stays `v1` because every root a
pre-ADR producer computed is still the root of the tree it computed it over. A pre-ADR verifier
over a post-ADR arena recomputes from `_source` alone and reports `TreeRootMismatch` for such a
tree: it fails closed.

`Head.treeRoot` therefore names the manifest's bytes, not `_source`'s. It is not
`Controller.current_root` (the serialized SQLite image), not `Head.rootHash` (one parse run's
segments), and not a blob hash (AGENTS.md "Identity and Authority Vocabulary").

### T3 — Parsing is a projection; an unparseable file is stored with a diagnostic

The walk ingests every file it does not exclude: `.gitignore` (and `.ignore`, git excludes) and
the build-output and bloat directories `is_bloat_dir` names (`node_modules`, `target`,
`vendor`, `__pycache__`, `__pypackages__`, `venv`, hidden directories). Bead `e6780e` (a
configurable exclude list for build outputs) refines that list; this ADR does not change it.
Symlinks are entries (mode `0o120000`, never followed); an executable bit on disk is mode
`0o100755`; a directory with no walked entry beneath it is an empty directory entry.

A file is **projected** when its extension or name maps to a grammar and a daemon's
`--language` filter, if any, allows it. `--language` no longer decides what the store holds: a
daemon started with `--language go` stores the whole tree and projects its Go files.

A file the parse projects but that does not parse cleanly is stored and diagnosed, never
refused:

- **Syntax errors** (tree-sitter `ERROR` or `MISSING` nodes): projected as tree-sitter parsed
  it, error nodes included, and one `_parse_diagnostics` row names the first error.
- **Not projectable** (a NUL byte in the first 8 KiB, or larger than `MAX_PARSE_FILE_SIZE`):
  stored, no projection rows, one `_parse_diagnostics` row with `projected = 0`.

```sql
CREATE TABLE _parse_diagnostics (
    path       TEXT PRIMARY KEY,  -- the _tree path
    blob_hash  BLOB NOT NULL,     -- the bytes the diagnostic describes
    projected  INTEGER NOT NULL,  -- 1: projected with error nodes; 0: no projection rows
    start_byte INTEGER,           -- first ERROR/MISSING node; NULL when not projected
    end_byte   INTEGER,
    start_row  INTEGER,           -- 0-based
    start_col  INTEGER,
    message    TEXT NOT NULL
) WITHOUT ROWID;
```

The row is a function of the bytes and the grammar, so a proposal and a cold parse of the same
tree write the same rows. A file whose next version parses cleanly loses its row in the same
transaction. `_errors` (the leyline-fs mount's validation table, keyed by display path, with a
timestamp) is not reused: its rows are not a function of the tree.

`projection-v7` adds `_tree` and `_parse_diagnostics`. A v6 arena is refused at parse open and
rebuilt cold, as every earlier shape change was.

### T4 — Proposals change entries: bytes, modes, symlinks, directories

```
writes = [ Proposed { path, base, change } ]
change = Write { bytes, mode: Option<Mode> } | Delete | Mkdir | Rmdir
mode   = Regular | Executable | Symlink
```

- **Write** sets a file entry's bytes and mode. `mode: None` keeps the entry's current mode (a
  create is `Regular`); a mode-only change (`chmod`) is a `Write` of the current bytes with the
  new mode against the current blob. A symlink is a `Write` with mode `Symlink` whose bytes are
  the target.
- **Delete** removes a file entry. **Mkdir** (base `Absent`) adds a directory entry, with its
  missing parents. **Rmdir** (base `Any`) removes a directory entry that no entry remains under
  once the proposal's other changes apply.
- **A directory rename is one proposal over every path under it**: a `Delete` of each file and
  a `Write` (base `Absent`) of the same bytes and mode at the new path, an `Rmdir` of each
  directory and a `Mkdir` of its new path. `source_write::rename_dir` builds it from the
  store. It lands whole or not at all (ADR-0042 P1).
- Paths stay plain and relative (`malformed_path`); a path under a file is `path_conflict`.

New typed refusals: `exists` (a `Mkdir` over any entry, or a create over a directory),
`not_empty` (an `Rmdir` with entries left under it), `not_a_directory` (an `Rmdir` of a file),
`is_a_directory` (a `Write` or `Delete` of a directory). `syntax` and `no_language` are no
longer refusals of a proposal: their reasons become diagnostics (T3) and the projection's
`--language` filter.

### T5 — A moved base merges three ways; overlapping hunks refuse

The single writer checks each base before the projection's transaction opens (ADR-0042 P1).
When a `Write`'s base `Blob(b)` no longer matches the path's current blob `c`, the proposal is
no longer refused outright:

| case | outcome |
|---|---|
| `Write` against a moved base, all three of base (`b`), ours (the proposal's bytes) and theirs (`c`) UTF-8, both entries non-symlink | three-way merge with `gix-merge`'s builtin text driver (`blob::builtin_driver::text`, Myers, line tokens), in process |
| — clean merge | the merged bytes land as the proposal's write for that path; `Accepted::merged` names it |
| — overlapping hunks | refused `stale`; the path's `moved` entry carries `conflicts`: the base line ranges (1-based, inclusive) where the two sides overlap |
| any of the three not UTF-8 ("binary"), or either side a symlink, or the base blob no longer stored | refused `stale` (no merge) |
| `Delete` against a moved base | refused `stale` |
| create (`Absent`) where the path holds a file | refused `stale` (`expected: absent`), unless the file holds the same bytes and mode: then it is accepted and changes nothing |
| `Mkdir` where the path holds an entry | refused `exists` |

The merge's three inputs are blobs the store holds, so the result is a function of `(b, ours,
c)`, and the merged write is checked like any other: it is projected (T3), the proposal's other
paths land with it in one transaction, and a conflict on any path refuses the whole proposal and
names every path that moved. A mode change merges trivially: the proposal's mode, when it names
one, wins; otherwise the current entry's mode is kept.

`gix-merge` is pinned `=0.17.0`, the release that shares its `gix-*` dependency set with the
`gix 0.84` already in the build (MIT OR Apache-2.0). `gix-imara-diff` (`=0.2.3`, already in the
build) computes the conflict ranges with `gix-merge`'s own overlap rule: a hunk of one side
whose base range contains the start of the other side's hunk, or two insertions at one base
line.

### T6 — The write-time `ESTALE` of `e650d7` is reversed: writes stage, flush decides

ADR-0044 V3 made a storefs WRITE whose handle's base no longer matched the path answer `ESTALE`
from `write(2)` itself (bead `e650d7`, #466). Under T5 a moved base is not a refusal: the
staged bytes may still merge. A WRITE stages on its handle whatever the store holds; FLUSH and
FSYNC propose against the handle's base, and their answer is the decision: a clean merge lands
(and the next open of the file must read the merged bytes, so that FLUSH withholds
`KEEP_CACHE`), a conflict is `ESTALE` from `close(2)` or `fsync(2)`. The host mount decides its
writes by the same rule (T8); its NFS server also checks each WRITE, because the macOS NFS client
never reports a refused COMMIT.

### T7 — The checkout carries modes, symlinks and directories

ADR-0043 R3's three-way table compares entries, not blob hashes: an entry is `(mode, blob)` for a
file and `(dir)` for a directory, and "no entry" is a value on every side. `_checkout_index`
records each entry's mode. Materialization writes a file's bytes and sets its permissions
(`0o755` for executable, `0o644` for regular, before the rename), creates a symlink (staged,
then renamed over the target), creates an empty directory, and removes a directory the store
removed (when it is empty on disk; otherwise the path is reported, not forced). A file the
store deleted is removed and its directory is left in place (T1).

### T8 — The host mount proposes what storefs proposes, decided the same way

Bead `ley-line-open-53d701`. The FUSE and NFS host mounts (`leyline-fs`) present the projection,
not the manifest: a projected file is a directory of its syntax nodes, a leaf's bytes are its
token, and a directory appears while a projected file is under it. Before T8 the mount wrote by
rules of its own over that view: it refused an unparseable record, refused any NFS WRITE whose
base had moved, spliced a flushed edit into whatever the file held at the flush (so another
writer's change could move a node's path onto another node), and made, removed and renamed
`nodes` rows without touching `_tree`. Every mount operation on a store (a database with
`_source`) is now a proposal through `source_write::propose`, or refused:

| mount operation | proposal |
|---|---|
| WRITE / truncate of a leaf, then FLUSH, FSYNC or COMMIT | `Write` of the file: the bytes of the handle's base with the node's range in that base replaced by the staged token, against that base. The node's range is taken with the base (OPEN, an NFS READ, or the first write), so the edit is the one made to what the handle read. A moved base merges (T5); a merge that lands is what the node and file read from then on; overlap is `ESTALE`; a file left unparseable lands with its diagnostic (T3) |
| UNLINK of a leaf, RMDIR of an interior syntax node | `Write` of the file without the node's bytes, against the blob it holds |
| RMDIR of a source file's entry | `Delete` of the file against its blob |
| RMDIR of a directory | `Rmdir`; `ENOTEMPTY` while anything is under it in the store |
| RENAME of a source file's entry or a directory into a directory | the file's `Delete` and `Write` (`source_write::rename`), or `rename_dir` |
| CREATE, MKDIR; RENAME of a syntax node or into one; a SETATTR mode other than the one presented | refused `EPERM` (NFS `NFS3ERR_PERM`): the projection has nothing to present a new regular file, an empty directory, a syntax node's chosen name or a mode as. SYMLINK stays unimplemented (`ENOSYS`, `NFS3ERR_NOTSUPP`) |

A refusal reaches the client in storefs's `errno_for` classes: stale `ESTALE`, exists `EEXIST`,
not empty `ENOTEMPTY`, outside the grant `EACCES`, and so on. A write to a store node that is no
syntax node of a parsed file is refused `EPERM`; a database that is no store keeps its record
writes. New files, directories, modes and symlinks reach the store through storefs, the daemon's
`propose` op, or the checkout.

**The macOS COMMIT tension.** The macOS NFS client reports a refused WRITE (at `fsync` or
`close`) and ignores a refused COMMIT. Under T5 a moved base is no refusal, so the mount handle
cannot refuse every WRITE whose base moved, as `6116c9` did. Each WRITE and truncate of the
mount handle instead runs the COMMIT's own decision (`source_write::check`: grant, bases, merge,
structure) on the edit it stages, against what the store holds at that WRITE, and refuses
`NFS3ERR_STALE` exactly when that decision refuses. What a WRITE cannot decide is a conflicting
change landed after the client's last WRITE and before its COMMIT. That COMMIT is refused and
macOS does not report it; the edit is discarded, `_errors` records the refusal for the node, and
the client's next READ serves the store's bytes. The window is the one `6116c9` left, and it is
the only refusal an NFS client is not told of at `close`. A FUSE handle, like storefs's, is
decided at its FLUSH, which the kernel reports.

## Consequences

- An agent's `README.md`, `notes.txt`, `Makefile`, `chmod +x`, symlink and empty directory
  persist in the store, in the checkout and across a daemon restart.
- Two agents editing different lines of one file both land; the second's flush lands the merge.
  Two agents editing the same lines: the second's `close` or `fsync` is `ESTALE`, and the
  refusal names the conflicting lines.
- A half-written Go file lands. Its projection carries error nodes and a diagnostic row; queries
  over it see what tree-sitter recovered.
- `treeRoot` moves for trees holding non-projected files, executables, symlinks or empty
  directories. A cloister grant whose `graphRoot` was computed before this ADR over such a tree
  no longer matches; grants are re-derived from the daemon's `read_source` `tree_root`.
- mache reads `projection-v7`: two new tables; `_source` is unchanged in shape. A consumer that
  listed "every file" from `_source` now lists the projected files; `_tree` lists every entry.
- The daemon wire (`daemon.capnp`) changes; see the CHANGELOG entry for bead `bb8aa6`.

## Phases

| Phase | Scope | Status |
|---|---|---|
| 1 | this ADR | Done |
| 2 | `_tree`, `_parse_diagnostics`; the walk ingests every file; proposals with modes, symlinks, directories and merges; the checkout's modes, symlinks and directories | Done |
| 3 | the daemon wire: `mode`, `mkdir`/`rmdir`, conflict ranges, retired `syntax`/`no_language` | Done |
| 4 | storefs: every file to the store, MKDIR/RMDIR/directory RENAME/SETATTR mode/symlink as proposals, WRITE stages, FLUSH merges; libkrun patch `0006` (READLINK, SYMLINK through `DynFileSystem`); the in-guest end-to-end scenarios | Done |
| 5 | T8: the host mount's writes, removals and renames as proposals; edits against the handle's base; the NFS WRITE check; the mount's syntax validation, `serve --language` and `StaleWrite` removed (bead `53d701`) | Done |

During phases 2 and 3 `source_write::propose` kept ADR-0042's rules for the virtio-fs core alone
(a `Rules::ParsedFilesOnly` mode); phase 4 moved storefs and the compatibility mounts to these
rules and removed that mode, `Refusal::Syntax`, `Refusal::NoLanguage` and `can_hold`.

## Falsifiers

| # | Claim | Test |
|---|---|---|
| M1 | the fold is the declared fold, and default trees keep their root | `tree_root_is_the_declared_fold`, `a_default_tree_folds_as_before_modes_and_dirs`, `modes_and_empty_dirs_move_the_tree_root` |
| M2 | the store holds every walked file, mode, symlink and empty directory | `a_cold_parse_stores_every_file_with_its_mode` (cli-lib `cmd_parse`) |
| M3 | an unparseable file is stored with a diagnostic | `an_unparseable_write_lands_with_a_diagnostic`, `a_binary_file_is_stored_unprojected_with_a_diagnostic` |
| M4 | disjoint edits merge; overlapping edits refuse with ranges | `a_moved_base_with_disjoint_edits_lands_the_merge`, `overlapping_edits_refuse_stale_with_conflict_ranges`, `binary_files_never_merge` |
| M5 | directories are entries | `mkdir_and_rmdir_are_proposals`, `a_directory_rename_is_one_proposal` |
| M6 | the checkout carries modes, symlinks and directories | `checkout_materializes_modes_symlinks_and_empty_dirs` |
| M7 | the wire carries it | `daemon_protocol_gate_proposals_produce_the_fixture_shapes` (fixtures for `mode`, `mkdir`, `conflicts`) |
| M8 | in the guest | `agent_in_guest.rs` scenarios (c), (C), (d), (p), (h): two writers on different lines both land; same lines `ESTALE`; README, `chmod +x`, symlink, empty directory, directory rename and a half-written Go file persist across a daemon restart; storefs `tree_tests` |
| M9 | the host mount lands what storefs lands, merges, refuses overlap, stores an unparseable save, and maps structure to proposals | cli-lib `source_write::mount_tests` (`a_mount_edit_lands_what_the_same_storefs_edit_lands`, `a_disjoint_edit_through_the_mount_handle_merges_at_its_commit`, `an_overlapping_edit_through_the_mount_is_refused_stale`, `an_unparseable_mount_save_lands_with_a_diagnostic`, `removals_through_the_mount_are_proposals`, `renames_through_the_mount_are_proposals`, `the_mount_refuses_what_its_projection_cannot_present`); fs `nfs::a_write_that_merges_is_staged_and_an_overlapping_one_refused` |
