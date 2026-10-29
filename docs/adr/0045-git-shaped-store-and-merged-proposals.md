# ADR-0045 — The store is a git-shaped tree, parsing is a projection over it, and a moved base merges

**Status:** Accepted (2026-10-15, owner direction); phases 1–6 built (below)
**Bead:** `ley-line-open-bb8aa6`; T8 `ley-line-open-53d701`, `ley-line-open-3e73dc`
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
transaction.

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
| — overlapping hunks | refused `stale`; the path's `moved` entry carries `conflicts`: the base line ranges where the two sides overlap, 0-based and half-open (`[start, end)`; two insertions at one line are the empty range `[line, line)`), as `daemon.capnp` `LineRange` and `merge::LineRange` define them |
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

### T8 — The host mount is storefs

Beads `ley-line-open-53d701`, `ley-line-open-3e73dc`. The host's FUSE and NFS mounts serve the
same filesystem core a guest's virtio-fs share serves: `leyline_storefs::StoreFs`, behind
`storefs::host::fuse::HostFuse` and `storefs::host::nfs::HostNfs`. A host process and a guest
see one tree (`_tree`: every file of any kind, mode, symlink and directory) and write it by one
set of rules (T4–T6): each save is a proposal on the daemon's one writer, a moved base merges, an
overlap is `ESTALE`, an unparseable save lands with its diagnostic, and every landed proposal is
written into the daemon's checkout and published to its arena. CREATE, MKDIR, RMDIR, RENAME,
SYMLINK, READLINK and a SETATTR of the mode are the proposals T4 names.

Before this the host mount was `leyline-fs`'s projection view: a parsed file presented as a
directory of its syntax nodes, a file of no language not presented at all, and creates, empty
directories and modes refused `EPERM` because that view had nothing to present them as. The
syntax view of a tree is mache's to serve; the store's mount is a filesystem. `leyline-fs`
keeps the nodes-table graph, its arena hot-swap and CDC reads, and refuses every write to a
store's nodes: only a proposal changes a store. `leyline serve`, which mounted that view over a
private arena with no daemon, is removed; `leyline daemon --mount` is the host mount.

The `leyline` binary injects the mount (`cmd_daemon::HostMount`, `DaemonOptions::host_mount`):
storefs depends on cli-lib, so cli-lib cannot construct it. A daemon asked to mount with no
provider is refused before it starts.

**Errno translation.** storefs answers Linux errno values (its guest is Linux). The FUSE adapter
translates the three whose host values differ (`ESTALE`, `ENOTEMPTY`, `ENOSYS`), the host's
`open(2)` and `renamex_np(2)` flags into Linux's, and the NFS adapter maps each errno to its
`nfsstat3`.

**The macOS COMMIT tension.** NFSv3 has no OPEN or CLOSE, so `HostNfs` keeps one storefs write
handle per file id; a READ with nothing staged rereads it (the bytes the client takes become its
base). The base it replaces stays among the handle's earlier reads (bead `4a8953`): the macOS
client READs a page again before a partial WRITE once its attribute cache saw the file change, so
a READ can hand another writer's bytes to the kernel while the editor still holds the bytes it
read first. A landed save stays among them too (bead `5ec040`): one NFS client serves every
process on the host, so a process that read before another process's save may save next. The
server cannot tell which read an edit was made from, so a save lands only when it keeps every
change the store took since each remembered read (`StoreFs::agree`): for each read that differs
from what the store holds now, the lines changed since that read (read → current) must be
untouched by the save's own edit (current → save), compared as line ranges of the current file;
a save that touches them may have been made from that read and would undo them. Decided at the
WRITE and again at the COMMIT under the store's writer; otherwise it is `NFS3ERR_STALE`. A
handle remembers at most `MAX_REMEMBERED_READS` (32) reads; past that bound every save is
refused until a READ settles the handle, rather than checked against an incomplete history. One
writer's successive saves land; a save that reverts a line the store took since an earlier read
is refused once, and the READ after the refusal settles the handle so the retry lands. Guest and
FUSE handles are never reread and keep no such history. The macOS NFS client reports a refused WRITE (at `fsync` or `close`) and ignores a refused
COMMIT, so each WRITE runs the COMMIT's own decision (`source_write::check`: grant, bases, merge,
structure) on the bytes it stages and refuses `NFS3ERR_STALE` exactly when that decision does. A
SETATTR of the size stages unchecked: a save truncates before it writes, and the empty file
between the two is no edit the client means; a truncate with no WRITE after it (`: > f`) lands
once it has stood idle. A refused WRITE or COMMIT voids the client's save: the handle drops what
it staged, keeps its base, and refuses every WRITE, SETATTR of the size and COMMIT until a READ
takes the store's bytes. (macOS goes on with a refused save by setting the file's length; staged
on the stale base, that length once landed the client's old bytes with a NUL appended, merged
over the other writer's change.) What a WRITE cannot decide is a conflicting change landed after the
client's last WRITE and before its COMMIT: that COMMIT is refused, macOS does not report it, and
the edit is discarded. A FUSE handle, like a guest's, is decided at its FLUSH, which the kernel
reports.

**Who is a writer.** The merge and the `ESTALE` refusal decide between writers that hold their
own handles, each with its own base: a guest's virtio-fs session, a FUSE `open(2)` (each open is
its own handle, its base taken at OPEN), the socket's `propose` op. Two processes writing through
**one NFS mount** are not two writers. NFSv3 carries no open state, and both processes share the
host's one NFS client and its page cache, so the server receives one client's READs and WRITEs
against one handle per file and cannot know that one process's bytes rest on an older read than
the other's. No edit is lost silently: the handle keeps every read and landed save since it last
settled, so a save made from an older read than the last landed one touches the lines that save
changed and is refused `NFS3ERR_STALE` at `fsync` or `close`, whether the two edits overlap or
not; the writer reloads and saves again. What one mount cannot do is merge the two processes'
disjoint edits the way separate writers' edits merge: that needs a session per writer
(`ley-line-open-5ec040`). macOS FUSE through fuse-t is itself an NFS server to the
kernel's NFS client; whether it separates writers is not measured.

**The macOS NFS client gate.** Before Tahoe 26.7 and Sequoia 15.8, Apple's NFS client corrupts
kernel memory on server replies it mishandles (CVE-2026-43686, a use-after-free; CVE-2026-84549,
an out-of-bounds read); a loopback NFS workload panicked a development Mac
(`ley-line-open-2d49ce`). `host::mount_nfs` refuses those releases, naming the CVEs and the fixed
releases. Loopback NFSv3 on a patched kernel is the arrangement Hugging Face's `hf-mount` ships on
xetdata's `nfsserve`, which this mount's `nessie-nfsserve` forks. The client's attribute cache is
held to one second (`actimeo=1`, storefs's changed-path timeout); close-to-open consistency
revalidates every open.

### T9 — Derived paths are never proposals

Bead `ley-line-open-39998b`. A path is **authored** or **derived**. Authored paths are what T1–T8
describe: every change is a proposal, merged, durable, in `_tree`, `treeRoot` and the checkout.
Derived paths are build output and tool state: `cargo build`'s `target/`, `npm install`'s
`node_modules/`, a virtualenv, `.git/`. They are regenerated, never merged, and written in volume
(a build writes thousands of files; at T8's ~47 ms a proposal, ten thousand take eight minutes),
so storefs never proposes them. There is no mode to enable: the class is decided per path, the
same way for the guest share and the host mount.

**Which paths are derived.** Decided in order, for each request:

1. A path the store holds (or this session created and holds pending) is authored: tracked wins,
   as in git. A tracked file under an ignored directory stays a proposal.
2. A path this session's scratch holds is derived.
3. A new path (CREATE, MKDIR, SYMLINK, the target of a RENAME) is derived when its parent is
   derived, or when the rules match it:
   - a component named `target`, `node_modules`, `__pycache__`, `__pypackages__`, `venv`,
     `.venv` or `.git` (the directory names of the parse walk's `is_bloat_dir` that are always
     generated; `vendor` and dot-names in general are not, since vendored and dot-prefixed files
     are often authored);
   - a file named `._*` or `.DS_Store` (macOS metadata; bead `8d6a8a`);
   - a `.gitignore` rule: the `.gitignore` of each directory from the path's parent up to the root,
     read from the store or, absent there, from the checkout, nearest first; an ignore match makes
     it derived, a `!` whitelist match authored. A `.gitignore` this filesystem lands is reread at
     its next match. `.git/info/exclude` and global excludes are not read.

   Otherwise it is authored.

A rule change does not move what exists: a scratch entry stays derived until it is removed, and a
stored entry stays authored.

**Where derived paths live.** In a scratch directory this `StoreFs` creates on its first derived
write: one per guest run (each run's share is its own `StoreFs`) and one per host-mount session.
It is a host directory served through the same share, so a derived request costs a share round
trip and a host file operation, and no parse, merge, durable flush or publish. A guest-local overlay
(virtio-blk or tmpfs inside the VM) would remove the round trip as well; it needs a guest-init
change and is not part of this decision (bead `ley-line-open-2354c3`). Measured: 7.06 ms per
derived file against 56.6 ms per authored one in the same guest run, 0.41 ms on APFS
(`docs/benchmarks/2026-10-20-share-path.md`). The scratch directory is removed when the `StoreFs` is
dropped, at the end of the run or unmount.

- **Confinement.** Every scratch access resolves the path component by component with `openat`
  and `O_NOFOLLOW` beneath the scratch root, so a symlink the guest made there is served to the
  guest as a symlink and is never followed on the host. Derived writes are inside the run's scope
  as authored ones are: `EACCES` where the scope may not propose.
- **Size.** Scratch is limited (16 GiB by default). A write past the limit is `ENOSPC`, and so is
  a full host disk. A derived write never falls back to the store.
- **Durability.** None. FSYNC of a derived file does not sync, and a crash or restart loses the
  scratch.

**Crossing classes.** A RENAME of a file from a derived path to an authored one lands its bytes as
a proposal create at the new path, then removes the scratch file; from authored to derived, it
writes the bytes into scratch, then proposes the delete of the old path. A directory RENAME
across classes is `EXDEV`, which `mv` answers by copying. A directory listing is the store's
entries and the scratch entries under the same directory; a name both hold lists the stored one.

## Consequences

- An agent's `README.md`, `notes.txt`, `Makefile`, `chmod +x`, symlink and empty directory
  persist in the store, in the checkout and across a daemon restart.
- Two agents editing different lines of one file both land; the second's flush lands the merge.
  Two agents editing the same lines: the second's `close` or `fsync` is `ESTALE`, and the
  refusal names the conflicting lines. This holds for agents that are separate writers (their
  own virtio-fs sessions, FUSE handles or socket proposals); two processes through one host NFS
  mount are one client: the second save made from an older read is refused, never lands over
  the first, and their disjoint edits do not merge (T8, `ley-line-open-5ec040`).
- A `cargo build`, `npm install` or `git` command in the guest or on the host mount writes its
  output to scratch: nothing it writes under a derived path reaches the store, the tree root or
  the checkout, and none of it outlives the run or the mount session (T9).
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
| 5 | T8: the host mount's writes as proposals; the NFS WRITE check; the mount's syntax validation, `serve --language` and `StaleWrite` removed (bead `53d701`) | Done |
| 6 | T8: the host mount is storefs behind FUSE and NFS adapters; the macOS NFS-client gate; `leyline serve`, `leyline-fs`'s mount servers and its `fuse`, `nfs` and `splice` features removed (bead `3e73dc`) | Done |
| 7 | T9: derived paths in a per-run (per-mount-session) scratch directory; built-in, macOS-metadata and `.gitignore` rules; tracked wins; class-crossing renames (bead `39998b`) | Done |

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
| M9 | the host mount is storefs: it lands new files, directories, modes and symlinks, merges, refuses overlap (and a retry made without a read), stores an unparseable save, and refuses an unpatched macOS NFS client | storefs `host::tests` (`a_new_file_lands_at_create_and_its_bytes_at_commit`, `mkdir_and_rmdir_are_directory_entries`, `a_mode_change_lands_the_executable_mode`, `a_symlink_lands_with_its_target`, `a_save_over_a_disjoint_landed_change_merges_at_commit`, `a_save_over_an_overlapping_landed_change_is_stale_at_the_write`, `a_page_fill_read_does_not_hide_another_writers_change`, `a_page_fill_read_does_not_hide_an_overlapping_change`, `a_commit_refuses_a_save_whose_result_turned_ambiguous`, `a_read_after_a_refusal_settles_the_handle`, `a_refused_commit_drops_the_edit_and_the_next_read_serves_the_store`, `an_unparseable_save_lands_with_its_diagnostic`, `an_unpatched_macos_nfs_client_is_refused_and_a_patched_one_mounts`); through the kernel, storefs `tests/host_mount.rs` and `tools/cli_smoke.sh --mount`; fs `a_store_refuses_every_write` |
| M10 | derived paths are never proposals, tracked wins, scratch is confined, limited and gone with its filesystem | storefs `derived_tests` (`build_output_under_target_and_node_modules_is_never_a_proposal`, `a_tracked_file_under_an_ignored_directory_stays_a_proposal`, `a_gitignore_rule_makes_a_new_path_derived_and_a_whitelist_keeps_one_authored`, `a_gitignore_in_the_checkout_only_is_read`, `macos_metadata_files_are_derived`, `a_rename_from_a_derived_path_to_an_authored_one_lands_a_create`, `a_rename_from_an_authored_path_to_a_derived_one_proposes_the_delete`, `a_directory_rename_across_classes_is_exdev`, `a_symlink_in_scratch_is_served_and_never_followed_on_the_host`, `a_write_past_the_scratch_limit_is_enospc_and_nothing_reaches_the_store`, `a_derived_write_outside_the_proposal_scope_is_eacces`, `the_scratch_is_gone_when_the_filesystem_drops`); in the guest, `agent_in_guest.rs` scenario (o); `share_bench` `derived_storm_per_file` |
