# leyline-storefs

The store-backed filesystem core behind LLO's virtio-fs device (ADR-0044 V2,
bead `ley-line-open-f1ea99`). `StoreFs` answers the FUSE requests a Linux
guest's file syscalls become, from the store, and lands writes as ADR-0042
proposals. Every answer is data or a **Linux** errno: the guest is Linux
whatever the host is (macOS `ESTALE` is 70; the guest needs 116).

This crate holds no libkrun code. Phase C adapts `StoreFs` onto the patched
libkrun's `DynFileSystem` trait; see "Adapter" below.

## Contract

| Request | Answer |
|---|---|
| LOOKUP `(parent, name)` | the entry for `parent/name`, with the configured TTL as both entry and attribute timeout; `ENOENT` for a name the scope does not see, an empty name, a name containing `/`, or non-UTF-8; `ENOTDIR` when `parent` is a file |
| GETATTR `(inode, handle?)` | the node's attributes; with a live handle, the size of the bytes that handle serves |
| READDIR `(inode, offset)` | the visible children after `offset`, in name order, each with the offset that resumes after it; `.` and `..` are not listed; `ENOTDIR` on a file |
| OPEN `(inode, flags)` | a handle that serves the blob the file held at open. Opened for writing (`O_WRONLY`/`O_RDWR`), that blob is the handle's **base**; `EACCES` when the scope may not propose the path; `O_TRUNC` stages an empty file; `EISDIR` on a directory |
| READ `(handle, offset, size)` | bytes the handle serves: its base, or what it staged |
| WRITE `(handle, offset, data)` | stages `data` on the handle (a gap reads as zeros); `EBADF` on a handle not opened for writing |
| FLUSH / FSYNC `(handle)` | proposes the staged bytes against the base through `source_write::propose`, on the store's one writer. Accepted: the base advances to the blob written, and an attached checkout is brought up to it. Nothing staged: no proposal, `Ok` |
| RELEASE `(handle)` | drops the handle and any bytes no FLUSH landed |
| SETATTR `(inode, handle?, {size, mode, uid, gid})` | a size with a handle stages on it (`ftruncate`); a size without one is a proposal of its own against the file's current blob (`truncate`), `EACCES` outside the proposal scope; mode/uid/gid are `EPERM`; times are not carried; `EISDIR` for a directory's size |
| CREATE, MKDIR | `EPERM` (`ENOENT`/`ENOTDIR` when the parent is not a visible directory) |
| UNLINK, RMDIR, RENAME | `EPERM` (`ENOENT` when the named entry or target directory is not visible) |

Unknown inodes are `ENOENT`; unknown handles are `EBADF`.

### Proposal errno map

`errno_for` maps the error a proposal returns:

| `source_write::Refusal` | errno |
|---|---|
| `Stale` (the base moved) | `ESTALE` (116) |
| `OutsideGrant` | `EACCES` (13) |
| `Syntax`, every other refusal, every internal failure | `EIO` (5) |

A refused FLUSH keeps the staged bytes and the base, so the `close` after a
refused `fsync` answers the same errno. The Linux FUSE client returns FLUSH
and FSYNC errors to `close` and `fsync` and drops RELEASE errors, so FLUSH is
the commit point (spike `ley-line-open-bb8cc1`).

### Scope

`Scope { see, propose }` is a run's `WorkspaceGrant` (`execution/v1`)
resolved to paths: `read`/`list` operations become `see`, `mutate`/`commit`
become `propose`. A path outside `see` does not exist for the guest. A
directory exists when some visible file is under it; the root always exists.
A file the scope may propose has mode `0644`, one it may only see `0444`;
directories are `0555`.

### Store and concurrency

`Store` is the daemon's connection discipline: `with_read` on a reader pool,
`with_write` on its one writer. It is implemented for `LiveDb` (the daemon's
living database) and `Arc<S>`. Any number of handles stage concurrently; each
proposal holds the writer for the propose and the checkout reconcile, as the
daemon's `propose` op does. Of two handles that read the same blob, the first
to flush lands and the second is `ESTALE`.

Reads resolve `_source.content_hash` to `source_blobs`: the one checkout
ADR-0043 names today. Inode numbers are stable per path for the life of a
`StoreFs`. The crate adds no persisted state.

## Not supported

- Creating, deleting, renaming files or directories: the store has no
  create, delete or move proposal. Editors that save by writing a temporary
  file and renaming it over the original fail at CREATE; editors that write
  in place work.
- Mode, owner, and times; extended attributes; links; locks.
- Files the store holds no blob for (`_source.content_hash` NULL) do not
  exist for the guest.
- DAX (ADR-0044 V3): the core serves reads per request.
- Publication: an accepted proposal is committed to the living database and
  visible to its readers at once; snapshotting it to the arena stays with
  the caller, as for `source_write`.
- A directory's LOOKUP or GETATTR scans `_source` for a visible file under
  it.

## Adapter (phase C)

The adapter implements libkrun's `DynFileSystem` over `StoreFs`:

- `Errno(n)` → `io::Error::from_raw_os_error(n)`, untranslated (the values
  are already Linux).
- `Attr` → `stat64`: `st_ino`, `st_mode = attr.mode()` (Linux `S_IFREG` /
  `S_IFDIR` bits), `st_nlink = attr.nlink()`, `st_size`, `st_blocks`,
  `st_blksize`. `Entry` → `Entry { inode, generation: 0, attr, attr_timeout:
  ttl, entry_timeout: ttl }`.
- OPEN returns the handle with `OpenOptions::DIRECT_IO`, as the spike did;
  the FUSE writeback cache stays off so every WRITE reaches the handle before
  FLUSH.
- READ writes the returned bytes to the `io::Write`; WRITE reads `size`
  bytes from the `io::Read` and passes them on.
- SETATTR: `SetattrValid::SIZE` → `size`, `MODE`/`UID`/`GID` → `mode`/`uid`/
  `gid`, the handle when the request carries one.
- READDIR: `DirEntry { ino, offset, type_: DT_REG (8) / DT_DIR (4), name }`
  into `add_entry` until it returns 0.
- INIT, FORGET, OPENDIR, RELEASEDIR, ACCESS, STATFS stay adapter-local.
  `dax_host_addr` stays `ENOSYS`.

Dependency direction: this crate depends on `leyline-cli-lib`, which
depends on `leyline-runtime`. The runtime therefore cannot depend on this
crate; its libkrun backend takes an `Arc<dyn DynFileSystem>` from its caller,
and the adapter lives here (behind a feature) or in a crate above both. The
daemon wiring implements `Store` for `DaemonContext` (delegating to
`with_read`/`with_write`) and builds `Scope` from the run's `WorkspaceGrant`.
