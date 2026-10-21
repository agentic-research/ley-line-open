# leyline-storefs

The store-backed filesystem core behind LLO's virtio-fs device (ADR-0044 V2,
bead `ley-line-open-f1ea99`). `StoreFs` answers the FUSE requests a Linux
guest's file syscalls become, from the store, and lands writes, creates,
deletes, renames, directories, modes and symlinks as proposals (ADR-0042,
ADR-0045). The store holds every file the guest makes, in any language, with
its git mode; a write against a file another writer moved since it was read
is merged three ways at FLUSH, and only overlapping edits are refused
(bead `ley-line-open-bb8aa6`). Every answer is data or a **Linux** errno: the
guest is Linux whatever the host is (macOS `ESTALE` is 70; the guest needs
116).

This crate holds no libkrun code. `StoreShare` answers a guest's virtio-fs
requests from `StoreFs`, and `DaemonShares` serves an execution/v1 run's
granted workspace from the daemon's own store (ADR-0044 phase C, bead
`ley-line-open-f210c2`); see "Adapter" below.

## The host mount (`mount` feature)

The same `StoreFs` serves `leyline daemon --mount` (ADR-0045 T8, bead
`ley-line-open-3e73dc`): `host::fuse::HostFuse` behind the host's FUSE client
(Linux; macOS with macFUSE or fuse-t) and `host::nfs::HostNfs`, a loopback
NFSv3 server on `nessie-nfsserve`, behind the host's NFS client (the macOS
default). A host process sees what a guest sees and writes by the same
rules; the `leyline` binary injects the mount into the daemon as
`host::DaemonHostMount`.

- **FUSE** translates the host's `open(2)` and `renamex_np(2)` flags into
  Linux's and the Linux errno values that differ (`ESTALE`, `ENOTEMPTY`,
  `ENOSYS`) into the host's.
- **NFS** has no OPEN or CLOSE: the server keeps one write handle per file
  id. A READ with nothing staged rereads it, so the bytes the client takes
  are its base. A WRITE is checked against the store at once and refused
  `NFS3ERR_STALE` exactly when its COMMIT would be, because the macOS client
  reports a refused WRITE and ignores a refused COMMIT. A SETATTR of the size
  stages unchecked (a save truncates before it writes); a truncate with no
  WRITE after it lands once it has stood idle. CREATE lands the empty file
  at once. A refused WRITE or COMMIT voids the client's save: the handle
  drops what it staged, keeps its base, and refuses every WRITE, SETATTR
  of the size and COMMIT until a READ takes the store's bytes (macOS sets
  the file's length after a refused WRITE; that changes nothing).
- **macOS gate.** `host::mount_nfs` refuses Tahoe before 26.7 and Sequoia
  before 15.8: their NFS client corrupts kernel memory on server replies
  (CVE-2026-43686, CVE-2026-84549; bead `ley-line-open-2d49ce`).
- The NFS client's attribute cache is one second (`actimeo=1`); every open
  revalidates (close-to-open).

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
| WRITE `(handle, offset, data)` | stages `data` on the handle (a gap reads as zeros), whatever the store holds now: another writer's change is decided at FLUSH (ADR-0045 T6, reversing the WRITE-time `ESTALE` of bead `e650d7`); `EBADF` on a handle not opened for writing |
| FLUSH / FSYNC `(handle)` | proposes the staged bytes (and a staged mode) against the base through `source_write::propose`, on the store's one writer. When the file moved since the base, the store merges the two three ways: a clean merge lands, the handle serves the merged bytes from then on, and the next read-only open drops the guest's pages; overlapping edits are `ESTALE`. A file that does not parse lands with a diagnostic. Accepted: the base advances to the blob written, and an attached checkout is brought up to it. Nothing staged: no proposal, `Ok` |
| RELEASE `(handle)` | drops the handle and any bytes no FLUSH landed |
| SETATTR `(inode, handle?, {size, mode, uid, gid})` | a size with a handle stages on it (`ftruncate`); a size without one is a proposal of its own against the file's current blob (`truncate`), `EACCES` outside the proposal scope. A mode with a write handle stages on it (`fchmod`); without one it is a proposal of the file's git mode against its current blob (`chmod`: executable when any execute bit is set, regular otherwise); a directory's or symlink's mode changes nothing. uid/gid are `EPERM`; times are not carried; `EISDIR` for a directory's size |
| CREATE `(parent, name, flags)` | the new file's entry and a handle whose base is "no file"; the file exists for the guest at once, and its first FLUSH proposes a **create** (bead `ley-line-open-47fb03`, ADR-0042 P1a). `EEXIST` when the name exists, `EACCES` outside the proposal scope |
| SYMLINK `(parent, name, target)` | a **create** proposal of `target`'s bytes with git mode `120000` (ADR-0045 T4); the link's entry. `EEXIST` when the name exists, `EACCES` outside the proposal scope |
| READLINK `(inode)` | a symlink's stored target; `EINVAL` for anything else |
| UNLINK `(parent, name)` | a **delete** proposal against the blob the file or symlink holds; the directory it was in stays, in the store too (ADR-0045 T1). `EISDIR` on a directory, `ENOENT` when the name is not visible, `EACCES` outside the proposal scope |
| RENAME `(olddir, oldname, newdir, newname, flags)` | a file moves by **one** proposal: delete the old path against its blob, create the new path (or replace the file there against its blob) with the same bytes and mode; a directory moves by **one** proposal over every path under it (`source_write::rename_dir`), replacing an empty directory at the target. Refused whole, so a refused rename leaves the old paths. Handles open on the old paths follow the files, and inodes move with them. `EISDIR`, `ENOTDIR`, `ENOTEMPTY` as `rename(2)`; `EEXIST` with `RENAME_NOREPLACE` over an existing name; `EINVAL` for `RENAME_EXCHANGE`, unknown flags and a directory moved under itself; `EACCES` outside the proposal scope at either end |
| MKDIR `(parent, name)` | an **mkdir** proposal. `EEXIST` when the name exists, `EACCES` when the scope may propose no path under it |
| RMDIR `(parent, name)` | an **rmdir** proposal for a directory that holds nothing; `ENOTEMPTY` when a file or directory is under it, `ENOTDIR` on a file |

Unknown inodes are `ENOENT`; unknown handles are `EBADF`. Structural
requests whose parent is not a visible directory are `ENOENT` or `ENOTDIR`.

### What the store holds, and what the session holds

The store holds every file the guest makes (ADR-0045 T1): any name, any
bytes, with its git mode (`100644`, `100755`, `120000` for a symlink whose
blob is its target), and every directory, empty ones included. Parsing is a
projection over the files the parse knows; a file that does not parse lands
with a `_parse_diagnostics` row. A checkout's `lang_filter` narrows the
projection, not what is stored.

The session holds only a **created file not landed yet**: from CREATE until
its first FLUSH lands it. A FLUSH the store refuses keeps the written bytes
in the session, so a later open or rename carries them; an UNLINK before any
FLUSH lands nothing; a RENAME of it lands it under the new name. A `chmod`
of such a file lands with it.

### Proposal errno map

`errno_for` maps the error a proposal returns:

| `source_write::Refusal` | errno |
|---|---|
| `Stale` naming only creates whose path holds other bytes | `EEXIST` (17) |
| any other `Stale` (a base moved and could not merge, or its file was deleted), and `Conflict` (overlapping edits) | `ESTALE` (116) |
| `OutsideGrant` | `EACCES` (13) |
| `NoSuchFile` | `ENOENT` (2) |
| `Exists` | `EEXIST` (17) |
| `NotEmpty` | `ENOTEMPTY` (39) |
| `NotADirectory`, `PathConflict` (a path under a file) | `ENOTDIR` (20) |
| `IsADirectory` | `EISDIR` (21) |
| every other refusal, every internal failure | `EIO` (5) |

A refused FLUSH keeps the staged bytes and the base, so the `close` after a
refused `fsync` answers the same errno. The Linux FUSE client returns FLUSH
and FSYNC errors to `close` and `fsync` and drops RELEASE errors, so FLUSH is
the commit point (spike `ley-line-open-bb8cc1`).

### Scope

`Scope { see, propose }` is a run's `WorkspaceGrant` (`execution/v1`)
resolved to paths: `read`/`list` operations become `see`, `mutate`/`commit`
become `propose`. A path outside `see` does not exist for the guest. A
stored directory exists for the guest when `see` is the whole tree, or when
some visible file is under it; the root always exists. A file the scope may
propose has mode `0644`, one it may only see `0444`, each with the execute
bits where the read bits are for an executable file (`0755`, `0555`); a
directory the scope may propose a path under is `0755`, any other `0555`; a
symlink is `0777`.

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
flush lands and the second merges with it: it lands when the two changed
different lines, and is `ESTALE` when they overlap.

Reads resolve the manifest `_tree` to `source_blobs`: the one checkout
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
  `direct_io`, never `keep_cache`, so every WRITE reaches the handle. A
  file not landed yet never sets `keep_cache`, and a FLUSH that landed a
  merge clears the inode's `keep_cache` record. OPENDIR applies
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
  timeout: OPEN, WRITE, FLUSH and FSYNC always reach the store, and FLUSH
  merges against, or refuses, a moved base.

### File times

Bead `ley-line-open-b8100d`. git decides whether to rehash a file from its
stat (size, mtime, ctime, inode), so a same-size edit must move the times
the guest is told, and an unchanged file must keep them. Every `Attr`
carries `mtime`, since the Unix epoch; the adapter reports it as `st_atime`,
`st_mtime` and `st_ctime`.

- **Source.** The store records no time per file (`_tree` holds modes and
  blob hashes, and its `mtime` is the host walk's stat cache, not a time a
  guest change made; `_checkout_index` holds entries), and the crate adds no
  persisted state, so times come from the change tracking `StoreFs`
  already does for "Read freshness". A path's time is the `Clock`'s origin
  (`Clock::origin`: the wall-clock time `MonotonicClock` read at its
  creation) plus the `Clock::now` reading of the latest change recorded to
  it, or the origin alone when none was recorded.
- **What moves it.** A change to a path moves its time and its parent
  directory's: a proposal `StoreFs` lands (FLUSH, FSYNC, SETATTR of a size,
  UNLINK, RENAME), another writer's proposal observed when `StoreFs` next
  serves the path in another state (the rule "Read freshness" records
  changes by), and a change only the session holds (CREATE, and UNLINK of a
  file not landed yet). An OPENDIR that lists other names or kinds than the
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

- `RENAME_EXCHANGE`.
- A handle open on a file another writer deleted, or that an UNLINK or a
  RENAME over it removed, answers `ESTALE` at its next FLUSH of staged
  bytes, where a disk would write to the orphaned inode.
- Merging binary files (bytes that are not UTF-8) and symlinks: a moved
  base refuses them `ESTALE`.
- Invalidation: the guest learns of another writer's change at OPEN or when
  a timeout expires, never by notification (see "Read freshness").
- Owner, modes other than the execute bit, and setting times (SETATTR
  carries none; see "File times" for the times reported); extended
  attributes; hard links; locks.
- DAX (ADR-0044 V3): the core serves reads per request.
- Publication beyond `Store::publish`: an accepted proposal is committed to
  the living database and visible to its readers at once, and the store's
  `publish` runs after it (a `DaemonContext` snapshots to its arena; a bare
  `LiveDb` publishes nothing).
- Under a path grant, a directory's LOOKUP or GETATTR scans `_tree` for a
  visible file under it.

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
`expectedGeneration` is carried and not consulted: it names ADR-0041's S1
commit sequence `(epoch, seq)`, which is not implemented; the `graphRoot`
check pins the share's tree.

The mapping:

- `Errno(n)` → `io::Error::from_raw_os_error(n)`, untranslated (the values
  are already Linux).
- `Attr` → `stat64`: `st_ino`, `st_mode = attr.mode()` (Linux `S_IFREG` /
  `S_IFDIR` / `S_IFLNK` bits), `st_nlink = attr.nlink()`, `st_size`, `st_blocks`,
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
  carried. UNLINK and RMDIR pass `(parent, name)`. SYMLINK: `(linkname,
  parent, name)` → `symlink(parent, name, target)`, reply with the entry;
  READLINK: `inode` → `readlink`, the target as `Reply::Data` (libkrun
  patch `0006` forwards both through `DynFileSystem`).
- RENAME and RENAME2: `(olddir, oldname, newdir, newname, flags)` → `rename`
  (`flags` = 0 for RENAME; RENAME2's `RENAME_NOREPLACE` = 1,
  `RENAME_EXCHANGE` = 2 and `RENAME_WHITEOUT` = 4 pass through, and every
  flag but `RENAME_NOREPLACE` is `EINVAL`).
- READDIR: `DirEntry { ino, offset, type_: DT_REG (8) / DT_DIR (4) / DT_LNK
  (10), name }`
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
