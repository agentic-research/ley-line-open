@0xa6cfea4a67062fc7;

# The workspace share's frame header (ADR-0044 phase C, bead
# ley-line-open-39d3a4): the FUSE request a libkrun guest sent, as the
# `leyline-krun-worker` forwards it to the daemon that holds the store, and
# the daemon's reply. Rust: leyline_runtime::backends::libkrun::share.
#
# INTERNAL, not a consumer contract. The worker and the daemon are the same
# LLO release (the daemon spawns the worker it ships with), so this schema has
# no Go or TypeScript bindings, is not in compatibility.json, and is not
# compiled by clients/go/leyline-schema/regen.sh. It still follows the
# append-only rule (ADR-0014 §2): add at the next ordinal, never renumber.
#
# A frame is two little-endian u32 lengths (header, payload), this header as
# one unpacked Cap'n Proto message (`capnp::serialize`), then the payload:
# the bytes a WRITE carries or a READ/READLINK answers. The payload never
# travels in the header.
#
# The vocabulary is FUSE's own: inode numbers, Linux `st_mode` bits and
# errno values, `FOPEN_*` and `FUSE_*` flag bits, `DT_*` entry types. A
# union member a reader does not know (a newer peer) is refused: the reader
# fails the stream, and the guest's request answers EIO.

# A duration or a time since the Unix epoch.
struct Time {
  secs  @0 :UInt64;
  nanos @1 :UInt32;
}

struct OptionalU64 {
  set   @0 :Bool;
  value @1 :UInt64;
}

struct OptionalU32 {
  set   @0 :Bool;
  value @1 :UInt32;
}

# A node's attributes, as `stat` reports them to the guest.
struct Attr {
  inode   @0 :UInt64;
  mode    @1 :UInt32;
  nlink   @2 :UInt32;
  size    @3 :UInt64;
  blocks  @4 :UInt64;   # 512-byte blocks
  blksize @5 :UInt32;
  atime   @6 :Time;
  mtime   @7 :Time;
  ctime   @8 :Time;
}

# A LOOKUP answer; inode 0 is a negative entry.
struct Entry {
  inode    @0 :UInt64;
  attr     @1 :Attr;
  entryTtl @2 :Time;
  attrTtl  @3 :Time;
}

struct DirEntry {
  inode  @0 :UInt64;
  offset @1 :UInt64;
  kind   @2 :UInt32;   # DT_REG, DT_DIR or DT_LNK
  name   @3 :Data;
}

struct DirPlusEntry {
  dir   @0 :DirEntry;
  entry @1 :Entry;
}

struct NodeRef {
  inode @0 :UInt64;
}

struct HandleRef {
  handle @0 :UInt64;
}

struct Name {
  parent @0 :UInt64;
  name   @1 :Data;
}

struct Getattr {
  inode  @0 :UInt64;
  handle @1 :OptionalU64;
}

# The attributes SETATTR names as valid; the rest are unset.
struct Setattr {
  inode  @0 :UInt64;
  handle @1 :OptionalU64;
  size   @2 :OptionalU64;
  mode   @3 :OptionalU32;
  uid    @4 :OptionalU32;
  gid    @5 :OptionalU32;
}

# `flags` are renameat2(2)'s; 0 for RENAME.
struct Rename {
  olddir  @0 :UInt64;
  oldname @1 :Data;
  newdir  @2 :UInt64;
  newname @3 :Data;
  flags   @4 :UInt32;
}

# `flags` are the guest's open(2) flags.
struct Open {
  inode @0 :UInt64;
  flags @1 :UInt32;
}

struct Create {
  parent @0 :UInt64;
  name   @1 :Data;
  flags  @2 :UInt32;
}

struct Read {
  handle @0 :UInt64;
  offset @1 :UInt64;
  size   @2 :UInt32;
}

# The bytes are the frame's payload.
struct Write {
  handle @0 :UInt64;
  offset @1 :UInt64;
}

struct Listing {
  inode  @0 :UInt64;
  offset @1 :UInt64;
}

struct Symlink {
  parent @0 :UInt64;
  name   @1 :Data;
  target @2 :Data;
}

struct Request {
  union {
    init        @0  :Void;
    lookup      @1  :Name;
    getattr     @2  :Getattr;
    setattr     @3  :Setattr;
    mkdir       @4  :Name;
    unlink      @5  :Name;
    rmdir       @6  :Name;
    rename      @7  :Rename;
    open        @8  :Open;
    create      @9  :Create;
    read        @10 :Read;
    write       @11 :Write;
    flush       @12 :HandleRef;
    fsync       @13 :HandleRef;
    release     @14 :HandleRef;
    opendir     @15 :NodeRef;
    readdir     @16 :Listing;
    readdirplus @17 :Listing;
    symlink     @18 :Symlink;
    readlink    @19 :NodeRef;
  }
}

struct AttrReply {
  attr @0 :Attr;
  ttl  @1 :Time;
}

# OPEN and OPENDIR: the handle and its FOPEN_* bits.
struct Opened {
  handle @0 :UInt64;
  flags  @1 :UInt32;
}

struct Created {
  entry  @0 :Entry;
  handle @1 :UInt64;
  flags  @2 :UInt32;
}

struct Reply {
  union {
    error   @0  :Int32;          # a Linux errno value
    init    @1  :UInt64;         # the FUSE_* capability bits the service wants
    entry   @2  :Entry;
    attr    @3  :AttrReply;
    opened  @4  :Opened;
    created @5  :Created;
    data    @6  :Void;           # the bytes are the frame's payload
    written @7  :UInt32;
    done    @8  :Void;
    dir     @9  :List(DirEntry);
    dirPlus @10 :List(DirPlusEntry);
  }
}
