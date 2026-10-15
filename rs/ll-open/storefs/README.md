# leyline-storefs

The store-backed filesystem core behind LLO's virtio-fs device (ADR-0044 V2,
bead `ley-line-open-f1ea99`). `StoreFs` answers the FUSE requests a Linux
guest's file syscalls become, from the store, and lands writes, creates,
deletes and renames as ADR-0042 proposals (bead `ley-line-open-47fb03`,
ADR-0042 P1a). Every answer is data or a **Linux** errno: the guest is Linux
whatever the host is (macOS `ESTALE` is 70; the guest needs 116).

This crate holds no libkrun code. `StoreShare` answers a guest's virtio-fs
requests from `StoreFs`, and `DaemonShares` serves an execution/v1 run's
granted workspace from the daemon's own store (ADR-0044 phase C, bead
`ley-line-open-f210c2`); see "Adapter" below.

## Contract

| Request | Answer |
|---|---|
| LOOKUP `(parent, name)` | the entry for `parent/name`, with its timeout (see "Read freshness") as both entry and attribute timeout; `ENOENT` for a name the scope does not see, an empty name, a name containing `/`, or non-UTF-8, and `negative_ttl(parent, name)` is the negative-entry timeout; `ENOTDIR` when `parent` is a file |
| GETATTR `(inode, handle?)` | the node's attributes and attribute timeout; with a live handle, the size of the bytes that handle serves |
| READDIR `(inode, offset)` | the visible children after `offset`, in name order, each with the offset that resumes after it; `.` and `..` are not listed; `ENOTDIR` on a file |
| READDIRPLUS `(inode, offset)` | READDIR's entries, each with the entry a LOOKUP of it answers (attributes and timeout) |
| OPENDIR `(inode)` | `keep_cache` when the directory lists the names and kinds the previous OPENDIR of it served; `ENOTDIR` on a file |
| OPEN `(inode, flags)` | a handle that serves the blob the file holds at open, read from the store at every OPEN, with `keep_cache` and `direct_io` (see "Read freshness"). Opened for writing (`O_WRONLY`/`O_RDWR`), that blob is the handle's **base**; `EACCES` when the scope may not propose the path; `O_TRUNC` stages an empty file; `EISDIR` on a directory |
| READ `(handle, offset, size)` | bytes the handle serves: its base, or what it staged |
| WRITE `(handle, offset, data)` | stages `data` on the handle (a gap reads as zeros); `ESTALE`, staging nothing, when the path's `_source.content_hash` is no longer the handle's blob base (another writer changed or deleted it; compared without reading the bytes); `EBADF` on a handle not opened for writing |
| FLUSH / FSYNC `(handle)` | proposes the staged bytes against the base through `source_write::propose`, on the store's one writer. Accepted: the base advances to the blob written, and an attached checkout is brought up to it. Nothing staged: no proposal, `Ok` |
| RELEASE `(handle)` | drops the handle and any bytes no FLUSH landed |
| SETATTR `(inode, handle?, {size, mode, uid, gid})` | a size with a handle stages on it (`ftruncate`); a size without one is a proposal of its own against the file's current blob (`truncate`), `EACCES` outside the proposal scope; mode/uid/gid are `EPERM`; times are not carried; `EISDIR` for a directory's size |
| CREATE `(parent, name, flags)` | the new file's entry and a handle whose base is "no file"; the file exists for the guest at once, and its first FLUSH proposes a **create** (bead `ley-line-open-47fb03`, ADR-0042 P1a). `EEXIST` when the name exists, `EACCES` outside the proposal scope |
| UNLINK `(parent, name)` | a **delete** proposal against the blob the file holds; the directory it was in stays for the session, as on a disk. `EISDIR` on a directory, `ENOENT` when the name is not visible, `EACCES` outside the proposal scope |
| RENAME `(olddir, oldname, newdir, newname, flags)` | a file moves by **one** proposal: delete the old path against its blob, create the new path (or replace the file there against its blob) with the same bytes; refused whole, so a refused rename leaves the old path. Handles open on the old path follow the file, and its inode moves with it. `EXDEV` for a stored file renamed to a name the store cannot hold, and for a directory that holds anything (the caller copies); `EISDIR`, `ENOTDIR`, `ENOTEMPTY` as `rename(2)`; `EEXIST` with `RENAME_NOREPLACE` over an existing name; `EINVAL` for `RENAME_EXCHANGE` and unknown flags; `EACCES` outside the proposal scope at either end |
| MKDIR `(parent, name)` | a directory for the session, visible until RMDIR; nothing is proposed. `EEXIST` when the name exists, `EACCES` when the scope may propose no path under it |
| RMDIR `(parent, name)` | removes a directory that holds nothing; `ENOTEMPTY` when a file or directory is under it, `ENOTDIR` on a file |

Unknown inodes are `ENOENT`; unknown handles are `EBADF`. Structural
requests whose parent is not a visible directory are `ENOENT` or `ENOTDIR`.

### Files and directories the store does not hold

The store holds files in languages the parse projects; a directory is the
prefix of the files under it. The session holds the rest, in memory, for
the life of the `StoreFs`:

- A **created file the store cannot hold** (its name names no projected
  language: an editor's or harness's temp file such as
  `a.go.tmp.4242.1700000000`, a note; or a language the attached
  checkout's `lang_filter` excludes) keeps its bytes in the session at
  each FLUSH and is never proposed. Renamed onto a name the store can
  hold, it is proposed there as a create or replace: a write-temp-then-
  rename save is one proposal. A stored file renamed to such a name is
  `EXDEV`, so `mv` copies and unlinks instead.
- A **created file the store can hold** is in the session from CREATE
  until a FLUSH lands it. A FLUSH the store refuses (a syntax error) keeps
  the written bytes in the session, so a later open or rename carries them;
  an UNLINK before any FLUSH lands nothing.
- A **directory** made by MKDIR, or emptied by UNLINK, RMDIR or RENAME,
  stays until RMDIR. Directories are not persisted: a new `StoreFs` over
  the same store sees the directories its files imply.

### Proposal errno map

`errno_for` maps the error a proposal returns:

| `source_write::Refusal` | errno |
|---|---|
| `Stale` naming only creates whose path holds a file | `EEXIST` (17) |
| any other `Stale` (a base moved, or its file was deleted) | `ESTALE` (116) |
| `OutsideGrant` | `EACCES` (13) |
| `NoSuchFile` | `ENOENT` (2) |
| `Syntax`, every other refusal, every internal failure | `EIO` (5) |

A refused FLUSH keeps the staged bytes and the base, so the `close` after a
refused `fsync` answers the same errno. The Linux FUSE client returns FLUSH
and FSYNC errors to `close` and `fsync` and drops RELEASE errors, so FLUSH is
the commit point (spike `ley-line-open-bb8cc1`).

### Scope

`Scope { see, propose }` is a run's `WorkspaceGrant` (`execution/v1`)
resolved to paths: `read`/`list` operations become `see`, `mutate`/`commit`
become `propose`. A path outside `see` does not exist for the guest. A
directory exists when some visible file is under it or the session made it;
the root always exists. A file the scope may propose has mode `0644`, one it
may only see `0444`; a directory the scope may propose a path under is
`0755`, any other `0555`.

### Store and concurrency

`Store` is the daemon's connection discipline: `with_read` on a reader pool,
`with_write` on its one writer, and `publish` after a proposal landed. It is
implemented for `LiveDb` (the daemon's living database; `publish` does
nothing), `DaemonContext` (a running daemon; `publish` snapshots the living
database into its arena, as the `propose` op does after its commit) and
`Arc<S>`. Any number of handles stage concurrently; each proposal holds the
writer for the propose and the checkout reconcile, as the daemon's `propose`
op does, then publishes; a failed publish is logged and does not refuse the
committed proposal. Of two handles that read the same blob, the first to
flush lands and the second is `ESTALE`.

Reads resolve `_source.content_hash` to `source_blobs`: the one checkout
ADR-0043 names today. Inode numbers are stable per path for the life of a
`StoreFs`; a RENAME carries the inode to the new path. The crate adds no
persisted state.

### Read freshness

ADR-0044 V3, "Read freshness" (beads `ley-line-open-e6269a`, `789e0f`,
`e650d7`). The guest caches entries, attributes and file pages, and nothing
can tell it that another writer changed a path: libkrun's virtio-fs device
has no notification queue and Linux's virtio-fs client has none either
(`docs/benchmarks/2026-10-15-vf3-virtiofs.md`, "Source check"). Long
timeouts and the page cache are what make a guest tree walk fast (same
document: `find` 82× APFS with no caching, 1.0× with it), so `StoreFs`
caches what it can prove unchanged and keeps the rest short:

- **Fresh at open.** Every OPEN reads the path's current blob from the
  store. `Opened::keep_cache` is set on a read-only open of a stored file
  only when that blob is the one the previous open of the inode served and
  that open was read-only; otherwise the guest drops the file's pages at
  this open and reads the current blob. A write open (and CREATE) is
  `direct_io`, never `keep_cache`, so every WRITE reaches the base check.
  A file only the session holds never sets `keep_cache`. OPENDIR applies
  the same rule to a directory's names and kinds. An OPEN reply carries no
  attributes, and the guest bounds a cached read by the size its cached
  attributes hold: a read-only open serving a blob of another size than
  the last LOOKUP, GETATTR, SETATTR, READDIRPLUS or CREATE reply told the
  guest for the inode is `direct_io`, so its reads reach the whole blob
  (bead `ley-line-open-f210c2`: without it the guest read a grown file cut
  to its old size).
- **Adaptive timeouts.** `Options::freshness` is `Freshness { settled,
  changed, window }`, default 60 s, 1 s, 60 s. A path is *recently changed*
  at time `now` when `StoreFs` recorded a change to it at `t0` with
  `now - t0 < window`. It records a change to a path, and to the path's
  parent directory, when a proposal it lands writes, creates or deletes the
  path, or when it serves the path (LOOKUP, GETATTR, SETATTR, OPEN,
  READDIRPLUS, a parent's directory check) in a state — absent, a
  directory, or a file and its blob — other than the one it last served
  (another writer's proposal, observed). The entry, attribute and
  negative-entry timeouts of a recently changed path are `changed`; every
  other path's are `settled`. Time comes from a `Clock`
  (`MonotonicClock` by default; `StoreFs::with_clock` injects one).
- **What stays stale.** A name the guest cached is not asked about until
  its timeout expires, so another writer's change to a settled path is
  visible after at most `settled` (a `stat` or a cached negative entry);
  file contents are current at each open. Writes never depend on a
  timeout: OPEN, WRITE, FLUSH and FSYNC always reach the store, and WRITE
  refuses a stale base with `ESTALE`.

### File times

Bead `ley-line-open-b8100d`. git decides whether to rehash a file from its
stat (size, mtime, ctime, inode), so a same-size edit must move the times
the guest is told, and an unchanged file must keep them. Every `Attr`
carries `mtime`, since the Unix epoch; the adapter reports it as `st_atime`,
`st_mtime` and `st_ctime`.

- **Source.** The store records no time per file (`_source` holds content
  and its hash; `_checkout_index` holds blobs), and the crate adds no
  persisted state, so times come from the change tracking `StoreFs`
  already does for "Read freshness". A path's time is the `Clock`'s origin
  (`Clock::origin`: the wall-clock time `MonotonicClock` read at its
  creation) plus the `Clock::now` reading of the latest change recorded to
  it, or the origin alone when none was recorded.
- **What moves it.** A change to a path moves its time and its parent
  directory's: a proposal `StoreFs` lands (FLUSH, FSYNC, SETATTR of a size,
  UNLINK, RENAME), another writer's proposal observed when `StoreFs` next
  serves the path in another state (the rule "Read freshness" records
  changes by), and a change only the session holds (CREATE, MKDIR, RMDIR,
  UNLINK or RENAME of a session file or directory, a FLUSH of a file the
  store cannot hold). An OPENDIR that lists other names or kinds than the
  previous OPENDIR of the directory moves the directory's time: another
  writer created or removed an entry in it. Session changes leave the
  timeouts alone; the guest made them.
- **Monotonic.** Each change takes `now`, or the nanosecond after the
  path's previous time when that is later, so two changes in one clock
  reading report two times, and no change reports the origin.
- **Through the guest's cache.** A reply's times are cached with its
  attributes; a changed path's attribute timeout is `Freshness::changed`,
  so the next GETATTR or LOOKUP after it expires carries the new time. A
  WRITE invalidates the guest's cached attributes for the file, so the
  guest's own edit shows its new time at the `stat` after its `close`.
- **Per session.** A new `StoreFs` has a new origin: every path reports a
  new time once, and git rehashes each file once, then trusts the stat.

## Not supported

- Renaming a directory that holds anything (`EXDEV`: `mv` copies), and
  `RENAME_EXCHANGE`.
- Files the store cannot hold persist only for the session (see above):
  a note or temp file never renamed onto a projected name is gone when the
  `StoreFs` is.
- A handle open on a file another writer deleted, or that an UNLINK or a
  RENAME over it removed, answers `ESTALE` at its next WRITE, or at its
  next FLUSH of bytes already staged, where a disk would write to the
  orphaned inode.
- Invalidation: the guest learns of another writer's change at OPEN or when
  a timeout expires, never by notification (see "Read freshness").
- Mode and owner, and setting times (SETATTR carries none; see "File
  times" for the times reported); extended attributes; links; locks.
- Files the store holds no blob for (`_source.content_hash` NULL) do not
  exist for the guest.
- DAX (ADR-0044 V3): the core serves reads per request.
- Publication beyond `Store::publish`: an accepted proposal is committed to
  the living database and visible to its readers at once, and the store's
  `publish` runs after it (a `DaemonContext` snapshots to its arena; a bare
  `LiveDb` publishes nothing).
- A directory's LOOKUP or GETATTR scans `_source` for a visible file under
  it.

## Adapter (phase C)

The VMM runs in the `leyline-krun-worker` process; the store lives in the
daemon. The adapter therefore has two halves joined by the share wire
(`leyline_runtime::backends::libkrun::share`: one FUSE request and reply per
frame, JSON header, READ and WRITE bytes as raw payload, on a socket pair the
daemon's libkrun backend hands the worker):

- `StoreShare` (this crate) answers each `Request` from `StoreFs` and makes
  every FUSE-level decision below. Its replies are in FUSE's vocabulary:
  inode numbers, `st_mode` bits, Linux errno values, `FOPEN_*` and
  `FUSE_*` bits, `DT_*` types, timeouts.
- `ShareFs` (`leyline-runtime`, `vmm` feature) is libkrun's `DynFileSystem`
  over any `FsService`: it transcribes each reply into libkrun's types and
  decides nothing. `ShareFs<StoreShare<S>>` is `DynFileSystem` over
  `StoreFs` in one process; the worker uses `ShareFs<RemoteFs>`.

`DaemonShares` is the daemon's `WorkspaceShares`: for a run granted a
workspace it opens a `StoreShare` over a `StoreFs` on the daemon's own
`DaemonContext` (so the share and the socket propose on one writer, and a
landed share proposal reaches the checkout and the arena as a socket
proposal does), when the store's `treeRoot` (ADR-0040 D1) is the grant's
`graphRoot`, scoped by `scope_for(operations)`: `read`/`list` see the whole
tree, `mutate`/`commit` propose anywhere in it, `query` neither.
`expectedGeneration` is carried and not consulted: the `graphRoot` check
pins the tree.

The mapping:

- `Errno(n)` → `io::Error::from_raw_os_error(n)`, untranslated (the values
  are already Linux).
- `Attr` → `stat64`: `st_ino`, `st_mode = attr.mode()` (Linux `S_IFREG` /
  `S_IFDIR` bits), `st_nlink = attr.nlink()`, `st_size`, `st_blocks`,
  `st_blksize`. `Entry` → `Entry { inode, generation: 0, attr, attr_timeout:
  ttl, entry_timeout: ttl }`; GETATTR and SETATTR reply with their returned
  duration as the attribute timeout.
- LOOKUP's `ENOENT` → `Ok(Entry { inode: 0, entry_timeout:
  negative_ttl(parent, name), .. })`, a negative entry the guest caches for
  that long; a zero `negative_ttl` → `ENOENT` as is.
- INIT returns `FsOptions::DO_READDIRPLUS`. READDIRPLUS: each `(DirEntry,
  Entry)` → `add_entry(DirEntry, Entry)` as READDIR and LOOKUP map them,
  until it returns 0.
- OPEN: `OpenOptions::KEEP_CACHE` when `Opened::keep_cache`,
  `OpenOptions::DIRECT_IO` when `Opened::direct_io`. OPENDIR:
  `OpenOptions::CACHE_DIR` always, plus `KEEP_CACHE` when
  `OpenedDir::keep_cache`. The FUSE writeback cache stays off so every
  WRITE reaches the handle before FLUSH.
- READ writes the returned bytes to the `io::Write`; WRITE reads `size`
  bytes from the `io::Read` and passes them on.
- SETATTR: `SetattrValid::SIZE` → `size`, `MODE`/`UID`/`GID` → `mode`/`uid`/
  `gid`, the handle when the request carries one.
- CREATE: `(parent, name, args.flags)` → `create`; reply with the entry and
  the handle, `OpenOptions::DIRECT_IO` as for a write open, never
  `KEEP_CACHE`. The request's mode and
  umask are not carried. MKDIR: `(parent, name)` → `mkdir`, mode not
  carried. UNLINK and RMDIR pass `(parent, name)`.
- RENAME and RENAME2: `(olddir, oldname, newdir, newname, flags)` → `rename`
  (`flags` = 0 for RENAME; RENAME2's `RENAME_NOREPLACE` = 1,
  `RENAME_EXCHANGE` = 2 and `RENAME_WHITEOUT` = 4 pass through, and every
  flag but `RENAME_NOREPLACE` is `EINVAL`).
- READDIR: `DirEntry { ino, offset, type_: DT_REG (8) / DT_DIR (4), name }`
  into `add_entry` until it returns 0.
- FORGET, RELEASEDIR, ACCESS, STATFS stay local to `ShareFs`.
  `dax_host_addr` stays `ENOSYS`.
- A reply of the wrong kind for its request, and an unreachable daemon, are
  `EIO`; a broken share socket stays broken (every later request is `EIO`).

Dependency direction: this crate depends on `leyline-cli-lib`, which
depends on `leyline-runtime` without `vmm`. The wire and `ShareFs` live in
the runtime; this crate implements the wire's `FsService` and
`WorkspaceShares`; `leyline-cli` wires `DaemonShares` into the libkrun
backend (`leyline execution-daemon --backend micro-vm --source <dir>`).
