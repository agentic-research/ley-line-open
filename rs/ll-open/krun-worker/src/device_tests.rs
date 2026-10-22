//! `ShareFs`: each FUSE request transcribed onto the wire, each reply into
//! libkrun's types.

use std::collections::VecDeque;
use std::ffi::CString;
use std::io;
use std::time::Duration;

use leyline_runtime::backends::libkrun::share::{
    Attr, DT_DIR, DT_REG, DirEntry, Entry, FOPEN_CACHE_DIR, FOPEN_DIRECT_IO, FOPEN_KEEP_CACHE,
    FUSE_DO_READDIRPLUS, FUSE_WRITEBACK_CACHE, FsService, Reply, Request,
};
use parking_lot::Mutex;

use crate::api::DynFileSystem;
use crate::api::virtio_fs::filesystem::{
    Context, DirEntry as KrunDirEntry, Entry as KrunEntry, Extensions, FsOptions, OpenOptions,
    SetattrValid,
};
use crate::device::{ShareFs, Stat};

/// Answers with scripted replies, in order, and records each request.
#[derive(Default)]
struct Script {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<Request>>,
}

impl Script {
    fn answering(replies: impl IntoIterator<Item = Reply>) -> ShareFs<Script> {
        ShareFs::new(Script {
            replies: Mutex::new(replies.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        })
    }
}

impl FsService for Script {
    fn call(&self, request: Request) -> Reply {
        self.requests.lock().push(request);
        self.replies
            .lock()
            .pop_front()
            .expect("a scripted reply for every request")
    }
}

fn requests(fs: &ShareFs<Script>) -> Vec<Request> {
    fs.service().requests.lock().clone()
}

fn ctx() -> Context {
    Context {
        uid: 0,
        gid: 0,
        pid: 1,
    }
}

fn name(value: &str) -> CString {
    CString::new(value).unwrap()
}

/// The error of a request that must fail (libkrun's reply types are not
/// `Debug`, so `unwrap_err` is unavailable).
fn failed<T>(result: io::Result<T>) -> io::Error {
    match result {
        Ok(_) => panic!("the request must fail"),
        Err(error) => error,
    }
}

fn errno(error: io::Error) -> Option<i32> {
    error.raw_os_error()
}

const FILE_ATTR: Attr = Attr {
    inode: 7,
    mode: 0o100644,
    nlink: 1,
    size: 1234,
    blocks: 3,
    blksize: 4096,
    atime: Duration::new(1_760_000_001, 11),
    mtime: Duration::new(1_760_000_002, 22),
    ctime: Duration::new(1_760_000_003, 33),
};

const DIR_ATTR: Attr = Attr {
    inode: 8,
    mode: 0o040755,
    nlink: 2,
    size: 0,
    blocks: 0,
    blksize: 4096,
    atime: Duration::new(1_760_000_004, 44),
    mtime: Duration::new(1_760_000_005, 55),
    ctime: Duration::new(1_760_000_006, 66),
};

fn file_entry() -> Entry {
    Entry {
        inode: 7,
        attr: FILE_ATTR,
        entry_ttl: Duration::from_secs(60),
        attr_ttl: Duration::from_millis(1500),
    }
}

// `stat` field widths differ by platform (macOS `st_mode` is u16 and
// `st_nlink` u16; Linux x86_64 has u32 and u64), so each cast is needed on
// one platform and redundant on another.
#[allow(clippy::unnecessary_cast)]
fn assert_stat(st: &Stat, attr: &Attr) {
    assert_eq!(st.st_ino as u64, attr.inode);
    assert_eq!(st.st_mode as u32, attr.mode);
    assert_eq!(st.st_nlink as u32, attr.nlink);
    assert_eq!(st.st_size as u64, attr.size);
    assert_eq!(st.st_blocks as u64, attr.blocks);
    assert_eq!(st.st_blksize as u32, attr.blksize);
    let time = |secs: i64, nanos: i64| Duration::new(secs as u64, nanos as u32);
    assert_eq!(
        time(st.st_atime as i64, st.st_atime_nsec as i64),
        attr.atime
    );
    assert_eq!(
        time(st.st_mtime as i64, st.st_mtime_nsec as i64),
        attr.mtime
    );
    assert_eq!(
        time(st.st_ctime as i64, st.st_ctime_nsec as i64),
        attr.ctime
    );
}

fn assert_entry(entry: &KrunEntry, expected: &Entry) {
    assert_eq!(entry.inode, expected.inode);
    assert_eq!(entry.generation, 0);
    assert_eq!(entry.attr_flags, 0);
    assert_eq!(entry.entry_timeout, expected.entry_ttl);
    assert_eq!(entry.attr_timeout, expected.attr_ttl);
    assert_stat(&entry.attr, &expected.attr);
}

#[test]
fn init_asks_for_exactly_the_capabilities_the_service_wants() {
    let fs = Script::answering([Reply::Init {
        want: FUSE_DO_READDIRPLUS,
    }]);
    let want = fs.init(FsOptions::all()).unwrap();
    assert_eq!(want, FsOptions::DO_READDIRPLUS);
    assert!(!want.contains(FsOptions::WRITEBACK_CACHE));
    assert_eq!(requests(&fs), vec![Request::Init]);
}

#[test]
fn lookup_transcribes_the_entry_and_its_two_timeouts() {
    let fs = Script::answering([Reply::Entry {
        entry: file_entry(),
    }]);
    let entry = fs.lookup(ctx(), 1, &name("a.go")).unwrap();
    assert_entry(&entry, &file_entry());
    assert_eq!(
        requests(&fs),
        vec![Request::Lookup {
            parent: 1,
            name: b"a.go".to_vec()
        }]
    );
}

#[test]
fn a_negative_entry_is_inode_zero_cached_for_its_entry_timeout() {
    let negative = Entry {
        inode: 0,
        attr: Attr::default(),
        entry_ttl: Duration::from_secs(60),
        attr_ttl: Duration::ZERO,
    };
    let fs = Script::answering([Reply::Entry { entry: negative }]);
    let entry = fs.lookup(ctx(), 1, &name("gone.go")).unwrap();
    assert_entry(&entry, &negative);
}

#[test]
fn an_error_reply_is_the_linux_errno_untranslated() {
    // 116 is Linux ESTALE; macOS's ESTALE is 70. The device must not map it.
    let fs = Script::answering([Reply::Error { errno: 116 }]);
    let error = failed(fs.lookup(ctx(), 1, &name("a.go")));
    assert_eq!(errno(error), Some(116));
}

#[test]
fn a_reply_of_the_wrong_kind_is_eio() {
    let fs = Script::answering([Reply::Done]);
    assert_eq!(errno(failed(fs.lookup(ctx(), 1, &name("a.go")))), Some(5));
}

#[test]
fn getattr_and_setattr_answer_the_attributes_and_their_timeout() {
    let ttl = Duration::from_millis(250);
    let fs = Script::answering([
        Reply::Attr {
            attr: FILE_ATTR,
            ttl,
        },
        Reply::Attr {
            attr: DIR_ATTR,
            ttl,
        },
    ]);
    let (st, got) = fs.getattr(ctx(), 7, Some(3)).unwrap();
    assert_stat(&st, &FILE_ATTR);
    assert_eq!(got, ttl);

    // SAFETY: stat is plain old data; all-zero is a valid value.
    let mut set: Stat = unsafe { std::mem::zeroed() };
    set.st_size = 42;
    set.st_mode = 0o600 as _;
    set.st_uid = 11;
    set.st_gid = 12;
    let (st, got) = fs
        .setattr(ctx(), 8, set, None, SetattrValid::SIZE | SetattrValid::GID)
        .unwrap();
    assert_stat(&st, &DIR_ATTR);
    assert_eq!(got, ttl);
    assert_eq!(
        requests(&fs),
        vec![
            Request::Getattr {
                inode: 7,
                handle: Some(3)
            },
            Request::Setattr {
                inode: 8,
                handle: None,
                size: Some(42),
                mode: None,
                uid: None,
                gid: Some(12),
            },
        ]
    );
}

#[test]
fn setattr_carries_each_valid_field_and_only_those() {
    let reply = || Reply::Attr {
        attr: FILE_ATTR,
        ttl: Duration::ZERO,
    };
    let fs = Script::answering([reply(), reply(), reply()]);
    // SAFETY: as above.
    let mut set: Stat = unsafe { std::mem::zeroed() };
    set.st_size = 9;
    set.st_mode = 0o100600 as _;
    set.st_uid = 21;
    set.st_gid = 22;
    fs.setattr(ctx(), 7, set, Some(4), SetattrValid::MODE)
        .unwrap();
    fs.setattr(ctx(), 7, set, Some(4), SetattrValid::UID)
        .unwrap();
    fs.setattr(ctx(), 7, set, Some(4), SetattrValid::ATIME)
        .unwrap();
    let carried = |mode, uid| Request::Setattr {
        inode: 7,
        handle: Some(4),
        size: None,
        mode,
        uid,
        gid: None,
    };
    assert_eq!(
        requests(&fs),
        vec![
            carried(Some(0o100600), None),
            carried(None, Some(21)),
            carried(None, None),
        ]
    );
}

#[test]
fn structural_requests_carry_their_names_and_flags() {
    let fs = Script::answering([
        Reply::Entry {
            entry: file_entry(),
        },
        Reply::Done,
        Reply::Done,
        Reply::Done,
        Reply::Done,
    ]);
    let entry = fs
        .mkdir(ctx(), 1, &name("d"), 0o777, 0o022, Extensions::default())
        .unwrap();
    assert_entry(&entry, &file_entry());
    fs.unlink(ctx(), 1, &name("x.go")).unwrap();
    fs.rmdir(ctx(), 1, &name("d")).unwrap();
    fs.rename(ctx(), 1, &name("a.tmp"), 8, &name("a.go"), 0)
        .unwrap();
    fs.rename(ctx(), 8, &name("b"), 1, &name("c"), 1).unwrap();
    assert_eq!(
        requests(&fs),
        vec![
            Request::Mkdir {
                parent: 1,
                name: b"d".to_vec()
            },
            Request::Unlink {
                parent: 1,
                name: b"x.go".to_vec()
            },
            Request::Rmdir {
                parent: 1,
                name: b"d".to_vec()
            },
            Request::Rename {
                olddir: 1,
                oldname: b"a.tmp".to_vec(),
                newdir: 8,
                newname: b"a.go".to_vec(),
                flags: 0
            },
            Request::Rename {
                olddir: 8,
                oldname: b"b".to_vec(),
                newdir: 1,
                newname: b"c".to_vec(),
                flags: 1
            },
        ]
    );
}

#[test]
fn open_and_create_answer_the_handle_and_its_cache_flags() {
    let fs = Script::answering([
        Reply::Opened {
            handle: 3,
            flags: FOPEN_KEEP_CACHE,
        },
        Reply::Opened {
            handle: 4,
            flags: FOPEN_DIRECT_IO,
        },
        Reply::Created {
            entry: file_entry(),
            handle: 5,
            flags: FOPEN_DIRECT_IO,
        },
        Reply::Opened {
            handle: 0,
            flags: FOPEN_CACHE_DIR | FOPEN_KEEP_CACHE,
        },
    ]);
    assert_eq!(
        fs.open(ctx(), 7, false, 0).unwrap(),
        (Some(3), OpenOptions::KEEP_CACHE)
    );
    assert_eq!(
        fs.open(ctx(), 7, true, 0o2).unwrap(),
        (Some(4), OpenOptions::DIRECT_IO)
    );
    let (entry, handle, options) = fs
        .create(
            ctx(),
            1,
            &name("n.go"),
            0o644,
            false,
            0o100102,
            0o022,
            Extensions::default(),
        )
        .unwrap();
    assert_entry(&entry, &file_entry());
    assert_eq!((handle, options), (Some(5), OpenOptions::DIRECT_IO));
    assert_eq!(
        fs.opendir(ctx(), 1, 0).unwrap(),
        (Some(0), OpenOptions::CACHE_DIR | OpenOptions::KEEP_CACHE)
    );
    assert_eq!(
        requests(&fs),
        vec![
            Request::Open { inode: 7, flags: 0 },
            Request::Open {
                inode: 7,
                flags: 0o2
            },
            Request::Create {
                parent: 1,
                name: b"n.go".to_vec(),
                flags: 0o100102
            },
            Request::Opendir { inode: 1 },
        ]
    );
}

#[test]
fn read_writes_the_served_bytes_and_write_passes_exactly_size_bytes() {
    let fs = Script::answering([
        Reply::Data {
            data: b"package a\n".to_vec(),
        },
        Reply::Written { size: 4 },
    ]);
    let mut out = Vec::new();
    assert_eq!(
        fs.read(ctx(), 7, 3, &mut out, 4096, 10, None, 0).unwrap(),
        10
    );
    assert_eq!(out, b"package a\n");

    // The reader holds more than the request's size; only `size` bytes are
    // the WRITE's data.
    let mut input: &[u8] = b"editMORE";
    assert_eq!(
        fs.write(ctx(), 7, 3, &mut input, 4, 2, None, false, false, 0)
            .unwrap(),
        4
    );
    assert_eq!(input, b"MORE");
    assert_eq!(
        requests(&fs),
        vec![
            Request::Read {
                handle: 3,
                offset: 10,
                size: 4096
            },
            Request::Write {
                handle: 3,
                offset: 2,
                data: b"edit".to_vec()
            },
        ]
    );
}

#[test]
fn a_write_whose_reader_runs_short_reaches_no_service() {
    let fs = Script::answering([]);
    let mut input: &[u8] = b"ab";
    let error = fs
        .write(ctx(), 7, 3, &mut input, 4, 0, None, false, false, 0)
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    assert!(requests(&fs).is_empty());
}

#[test]
fn flush_fsync_and_release_reach_the_service_and_return_its_errno() {
    let fs = Script::answering([
        Reply::Error { errno: 116 },
        Reply::Error { errno: 5 },
        Reply::Done,
        Reply::Done,
    ]);
    assert_eq!(errno(fs.flush(ctx(), 7, 3, 0).unwrap_err()), Some(116));
    assert_eq!(errno(fs.fsync(ctx(), 7, false, 3).unwrap_err()), Some(5));
    fs.fsync(ctx(), 7, true, 3).unwrap();
    fs.release(ctx(), 7, 0, 3, false, false, None).unwrap();
    assert_eq!(
        requests(&fs),
        vec![
            Request::Flush { handle: 3 },
            Request::Fsync { handle: 3 },
            Request::Fsync { handle: 3 },
            Request::Release { handle: 3 },
        ]
    );
}

fn dirent(inode: u64, offset: u64, kind: u32, name: &str) -> DirEntry {
    DirEntry {
        inode,
        offset,
        kind,
        name: name.as_bytes().to_vec(),
    }
}

#[test]
fn readdir_adds_entries_until_the_buffer_is_full() {
    let fs = Script::answering([Reply::Dir {
        entries: vec![
            dirent(2, 1, DT_REG, "a.go"),
            dirent(3, 2, DT_DIR, "pkg"),
            dirent(4, 3, DT_REG, "z.go"),
        ],
    }]);
    let mut added = Vec::new();
    fs.readdir(ctx(), 1, 0, 4096, 5, &mut |d: KrunDirEntry| {
        added.push((d.ino, d.offset, d.type_, d.name.to_vec()));
        Ok(if added.len() == 2 { 0 } else { 32 })
    })
    .unwrap();
    assert_eq!(
        added,
        vec![
            (2, 1, DT_REG, b"a.go".to_vec()),
            (3, 2, DT_DIR, b"pkg".to_vec()),
        ],
        "the entry that did not fit stops the listing"
    );
    assert_eq!(
        requests(&fs),
        vec![Request::Readdir {
            inode: 1,
            offset: 5
        }]
    );
}

#[test]
fn readdirplus_adds_each_entry_with_its_lookup_entry() {
    let fs = Script::answering([Reply::DirPlus {
        entries: vec![
            (dirent(7, 1, DT_REG, "a.go"), file_entry()),
            (dirent(9, 2, DT_REG, "b.go"), file_entry()),
        ],
    }]);
    let mut added = Vec::new();
    fs.readdirplus(
        ctx(),
        1,
        0,
        4096,
        0,
        &mut |d: KrunDirEntry, e: KrunEntry| {
            assert_entry(&e, &file_entry());
            added.push((d.ino, d.name.to_vec()));
            Ok(1)
        },
    )
    .unwrap();
    assert_eq!(added, vec![(7, b"a.go".to_vec()), (9, b"b.go".to_vec())]);
    assert_eq!(
        requests(&fs),
        vec![Request::Readdirplus {
            inode: 1,
            offset: 0
        }]
    );
}

#[test]
fn a_listing_error_reaches_the_guest_and_adds_nothing() {
    let fs = Script::answering([Reply::Error { errno: 20 }, Reply::Error { errno: 20 }]);
    let error = fs
        .readdir(ctx(), 7, 0, 4096, 0, &mut |_| panic!("no entry"))
        .unwrap_err();
    assert_eq!(errno(error), Some(20));
    let error = fs
        .readdirplus(ctx(), 7, 0, 4096, 0, &mut |_, _| panic!("no entry"))
        .unwrap_err();
    assert_eq!(errno(error), Some(20));
}

/// ADR-0045 T4: SYMLINK carries its name and target and answers the entry;
/// READLINK answers the target the service sends as data.
#[test]
fn symlink_and_readlink_reach_the_service() {
    let link = Entry {
        attr: Attr {
            mode: 0o120777,
            size: 4,
            ..FILE_ATTR
        },
        ..file_entry()
    };
    let fs = Script::answering([
        Reply::Entry { entry: link },
        Reply::Data {
            data: b"a.go".to_vec(),
        },
        Reply::Error { errno: 22 },
    ]);
    let made = fs.symlink(ctx(), &name("a.go"), 1, &name("link")).unwrap();
    assert_entry(&made, &link);
    assert_eq!(fs.readlink(ctx(), 7).unwrap(), b"a.go");
    assert_eq!(errno(failed(fs.readlink(ctx(), 8))), Some(22));
    assert_eq!(
        requests(&fs),
        [
            Request::Symlink {
                parent: 1,
                name: b"link".to_vec(),
                target: b"a.go".to_vec(),
            },
            Request::Readlink { inode: 7 },
            Request::Readlink { inode: 8 },
        ]
    );
}

#[test]
fn local_requests_answer_without_the_service() {
    let fs = Script::answering([]);
    fs.forget(ctx(), 7, 1);
    fs.access(ctx(), 7, 0o4).unwrap();
    fs.releasedir(ctx(), 1, 0, 0).unwrap();
    fs.destroy();
    assert!(requests(&fs).is_empty());
}

#[test]
fn the_wire_bits_are_libkruns_fuse_bits() {
    assert_eq!(
        FsOptions::from_bits_truncate(FUSE_DO_READDIRPLUS),
        FsOptions::DO_READDIRPLUS
    );
    assert_eq!(FsOptions::WRITEBACK_CACHE.bits(), FUSE_WRITEBACK_CACHE);
    assert_eq!(OpenOptions::DIRECT_IO.bits(), FOPEN_DIRECT_IO);
    assert_eq!(OpenOptions::KEEP_CACHE.bits(), FOPEN_KEEP_CACHE);
    assert_eq!(OpenOptions::CACHE_DIR.bits(), FOPEN_CACHE_DIR);
}
