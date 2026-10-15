//! The adapter's mapping (README "Adapter"), request by request, against a
//! real store.

use std::sync::Arc;
use std::time::Duration;

use leyline_cli_lib::daemon::db_pool::LiveDb;
use leyline_runtime::backends::libkrun::share::{
    Attr as WireAttr, DT_DIR, DT_REG, DirEntry as WireDirEntry, Entry as WireEntry,
    FOPEN_CACHE_DIR, FOPEN_DIRECT_IO, FOPEN_KEEP_CACHE, FUSE_DO_READDIRPLUS, FsService, Reply,
    Request,
};

use super::StoreShare;
use crate::open_flags::{O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY};
use crate::rename_flags::{RENAME_EXCHANGE, RENAME_NOREPLACE};
use crate::tests::{
    A, A_BROKEN, A_EDIT, A_OTHER, B, Fixture, SHORT, TTL, fixture, fixture_with, freshness,
    on_disk, other_writer, read_source, whole,
};
use crate::{Freshness, Options, ROOT_INODE, Scope, StoreFs};

/// The adapter over a fixture's store, with the fixture's options.
fn share(f: &Fixture) -> StoreShare<Arc<LiveDb>> {
    share_with(
        f,
        whole(),
        Options {
            freshness: freshness(),
            checkout: None,
        },
    )
}

fn share_with(f: &Fixture, scope: Scope, options: Options) -> StoreShare<Arc<LiveDb>> {
    StoreShare::new(StoreFs::with_clock(
        Arc::clone(f.fs.store()),
        scope,
        options,
        f.clock.clone(),
    ))
}

fn lookup(s: &StoreShare<Arc<LiveDb>>, parent: u64, name: &str) -> Reply {
    s.call(Request::Lookup {
        parent,
        name: name.as_bytes().to_vec(),
    })
}

fn entry_of(reply: Reply) -> WireEntry {
    match reply {
        Reply::Entry { entry } => entry,
        other => panic!("expected an entry, got {other:?}"),
    }
}

fn opened(reply: Reply) -> (u64, u32) {
    match reply {
        Reply::Opened { handle, flags } => (handle, flags),
        other => panic!("expected an open, got {other:?}"),
    }
}

fn file_attr(inode: u64, size: u64, perm: u32) -> WireAttr {
    WireAttr {
        inode,
        mode: 0o100000 | perm,
        nlink: 1,
        size,
        blocks: size.div_ceil(512),
        blksize: 4096,
    }
}

const ESTALE: i32 = 116;

#[test]
fn init_wants_readdirplus_and_not_the_writeback_cache() {
    let f = fixture(whole());
    assert_eq!(
        share(&f).call(Request::Init),
        Reply::Init {
            want: FUSE_DO_READDIRPLUS
        }
    );
}

#[test]
fn lookup_answers_linux_attributes_and_the_freshness_timeout_twice() {
    let f = fixture(whole());
    let s = share(&f);
    let entry = entry_of(lookup(&s, ROOT_INODE, "a.go"));
    assert_eq!(
        entry,
        WireEntry {
            inode: entry.attr.inode,
            attr: file_attr(entry.attr.inode, A.len() as u64, 0o644),
            entry_ttl: TTL,
            attr_ttl: TTL,
        }
    );
    assert_ne!(entry.inode, 0);

    let dir = entry_of(lookup(&s, ROOT_INODE, "pkg"));
    assert_eq!(
        dir.attr,
        WireAttr {
            inode: dir.inode,
            mode: 0o040755,
            nlink: 2,
            size: 0,
            blocks: 0,
            blksize: 4096,
        }
    );
}

#[test]
fn a_file_the_scope_may_only_see_is_read_only() {
    let f = fixture(whole());
    let s = share_with(
        &f,
        Scope {
            see: leyline_cli_lib::source_write::Grant::Whole,
            propose: leyline_cli_lib::source_write::Grant::Paths(Default::default()),
        },
        Options::default(),
    );
    let entry = entry_of(lookup(&s, ROOT_INODE, "a.go"));
    assert_eq!(entry.attr.mode, 0o100444);
    assert_eq!(
        s.call(Request::Open {
            inode: entry.inode,
            flags: O_WRONLY
        }),
        Reply::Error { errno: 13 },
        "EACCES: a write open outside the proposal scope"
    );
}

#[test]
fn a_missing_name_is_a_negative_entry_cached_for_its_negative_timeout() {
    let f = fixture(whole());
    let s = share(&f);
    assert_eq!(
        entry_of(lookup(&s, ROOT_INODE, "missing.go")),
        WireEntry {
            inode: 0,
            attr: WireAttr::default(),
            entry_ttl: TTL,
            attr_ttl: Duration::ZERO,
        }
    );
}

#[test]
fn a_zero_negative_timeout_answers_enoent_and_other_errors_pass_through() {
    let f = fixture(whole());
    let s = share_with(
        &f,
        whole(),
        Options {
            freshness: Freshness {
                settled: Duration::ZERO,
                changed: Duration::ZERO,
                window: Duration::ZERO,
            },
            checkout: None,
        },
    );
    assert_eq!(
        lookup(&s, ROOT_INODE, "missing.go"),
        Reply::Error { errno: 2 }
    );
    let a = entry_of(lookup(&s, ROOT_INODE, "a.go")).inode;
    assert_eq!(lookup(&s, a, "x"), Reply::Error { errno: 20 }, "ENOTDIR");
    // A name no path can hold answers ENOENT with a zero negative timeout
    // whatever the configuration.
    let s = share(&f);
    assert_eq!(lookup(&s, ROOT_INODE, ""), Reply::Error { errno: 2 });
}

#[test]
fn getattr_and_setattr_answer_attributes_with_their_timeout() {
    let f = fixture(whole());
    let s = share(&f);
    let a = entry_of(lookup(&s, ROOT_INODE, "a.go")).inode;
    assert_eq!(
        s.call(Request::Getattr {
            inode: a,
            handle: None
        }),
        Reply::Attr {
            attr: file_attr(a, A.len() as u64, 0o644),
            ttl: TTL
        }
    );
    let (h, _) = opened(s.call(Request::Open {
        inode: a,
        flags: O_RDWR,
    }));
    // ftruncate: a size with the handle stages on it.
    let reply = s.call(Request::Setattr {
        inode: a,
        handle: Some(h),
        size: Some(3),
        mode: None,
        uid: None,
        gid: None,
    });
    let Reply::Attr { attr, .. } = reply else {
        panic!("expected attributes, got {reply:?}");
    };
    assert_eq!(attr.size, 3);
    assert_eq!(
        s.call(Request::Getattr {
            inode: a,
            handle: Some(h)
        }),
        Reply::Attr {
            attr: file_attr(a, 3, 0o644),
            ttl: TTL
        }
    );
    for (mode, uid, gid) in [
        (Some(0o600), None, None),
        (None, Some(1), None),
        (None, None, Some(1)),
    ] {
        assert_eq!(
            s.call(Request::Setattr {
                inode: a,
                handle: None,
                size: None,
                mode,
                uid,
                gid,
            }),
            Reply::Error { errno: 1 },
            "EPERM: the store holds no mode or owner"
        );
    }
    assert_eq!(
        s.call(Request::Getattr {
            inode: 999_999,
            handle: None
        }),
        Reply::Error { errno: 2 }
    );
}

#[test]
fn a_read_only_open_keeps_the_cache_only_for_the_blob_it_served_before() {
    let f = fixture(whole());
    let s = share(&f);
    let a = entry_of(lookup(&s, ROOT_INODE, "a.go")).inode;
    let first = opened(s.call(Request::Open {
        inode: a,
        flags: O_RDONLY,
    }));
    assert_eq!(
        first.1, 0,
        "the first open drops whatever pages the guest has"
    );
    let second = opened(s.call(Request::Open {
        inode: a,
        flags: O_RDONLY,
    }));
    assert_eq!(second.1, FOPEN_KEEP_CACHE);
    other_writer(&f, "a.go", A_OTHER);
    let third = opened(s.call(Request::Open {
        inode: a,
        flags: O_RDONLY,
    }));
    assert_eq!(third.1, 0, "a new blob: the guest drops its pages");
    assert_eq!(
        s.call(Request::Read {
            handle: third.0,
            offset: 0,
            size: 4096
        }),
        Reply::Data {
            data: A_OTHER.as_bytes().to_vec()
        },
        "fresh at open"
    );
}

#[test]
fn a_write_open_is_direct_io_and_never_keeps_the_cache() {
    let f = fixture(whole());
    let s = share(&f);
    let a = entry_of(lookup(&s, ROOT_INODE, "a.go")).inode;
    opened(s.call(Request::Open {
        inode: a,
        flags: O_RDONLY,
    }));
    let (_, flags) = opened(s.call(Request::Open {
        inode: a,
        flags: O_RDWR,
    }));
    assert_eq!(flags, FOPEN_DIRECT_IO);
    let (h, flags) = opened(s.call(Request::Open {
        inode: a,
        flags: O_WRONLY | O_TRUNC,
    }));
    assert_eq!(flags, FOPEN_DIRECT_IO);
    assert_eq!(
        s.call(Request::Read {
            handle: h,
            offset: 0,
            size: 64
        }),
        Reply::Data { data: Vec::new() },
        "O_TRUNC reaches the store as a flag"
    );
}

#[test]
fn an_edit_lands_through_write_and_flush_and_a_stale_or_broken_one_does_not() {
    let f = fixture_with(whole(), true, A);
    let s = share_with(
        &f,
        whole(),
        Options {
            freshness: freshness(),
            checkout: Some(crate::Checkout {
                dir: f.src.path().to_path_buf(),
                lang_filter: None,
            }),
        },
    );
    let a = entry_of(lookup(&s, ROOT_INODE, "a.go")).inode;
    let (h, _) = opened(s.call(Request::Open {
        inode: a,
        flags: O_RDWR | O_TRUNC,
    }));
    assert_eq!(
        s.call(Request::Write {
            handle: h,
            offset: 0,
            data: A_EDIT.as_bytes().to_vec()
        }),
        Reply::Written {
            size: A_EDIT.len() as u32
        }
    );
    assert_eq!(s.call(Request::Flush { handle: h }), Reply::Done);
    assert_eq!(read_source(&f, "a.go").0, A_EDIT.as_bytes());
    assert_eq!(on_disk(&f, "a.go").as_deref(), Some(A_EDIT));
    assert_eq!(s.call(Request::Fsync { handle: h }), Reply::Done);
    assert_eq!(s.call(Request::Release { handle: h }), Reply::Done);

    // Another writer moves the file under an open handle: WRITE is ESTALE,
    // the Linux value (macOS's is 70).
    let (h, _) = opened(s.call(Request::Open {
        inode: a,
        flags: O_RDWR,
    }));
    other_writer(&f, "a.go", A_OTHER);
    assert_eq!(
        s.call(Request::Write {
            handle: h,
            offset: 0,
            data: b"x".to_vec()
        }),
        Reply::Error { errno: ESTALE }
    );

    // An unparseable edit is EIO at FLUSH and FSYNC; nothing lands.
    let (h, _) = opened(s.call(Request::Open {
        inode: a,
        flags: O_RDWR | O_TRUNC,
    }));
    s.call(Request::Write {
        handle: h,
        offset: 0,
        data: A_BROKEN.as_bytes().to_vec(),
    });
    assert_eq!(
        s.call(Request::Flush { handle: h }),
        Reply::Error { errno: 5 }
    );
    assert_eq!(
        s.call(Request::Fsync { handle: h }),
        Reply::Error { errno: 5 }
    );
    assert_eq!(read_source(&f, "a.go").0, A_OTHER.as_bytes());
    assert_eq!(
        s.call(Request::Read {
            handle: 999,
            offset: 0,
            size: 1
        }),
        Reply::Error { errno: 9 },
        "EBADF"
    );
}

#[test]
fn create_answers_its_entry_and_a_direct_io_handle() {
    let f = fixture(whole());
    let s = share(&f);
    let reply = s.call(Request::Create {
        parent: ROOT_INODE,
        name: b"n.go".to_vec(),
        flags: O_RDWR,
    });
    let Reply::Created {
        entry,
        handle,
        flags,
    } = reply
    else {
        panic!("expected a create, got {reply:?}");
    };
    assert_eq!(flags, FOPEN_DIRECT_IO);
    assert_eq!(entry.attr, file_attr(entry.inode, 0, 0o644));
    assert_eq!(entry.entry_ttl, entry.attr_ttl);
    assert_eq!(
        entry.entry_ttl, TTL,
        "nothing landed yet: no change recorded"
    );
    let text = b"package a\n\nfunc N() int { return 7 }\n";
    s.call(Request::Write {
        handle,
        offset: 0,
        data: text.to_vec(),
    });
    assert_eq!(s.call(Request::Flush { handle }), Reply::Done);
    assert_eq!(read_source(&f, "n.go").0, text);
    assert_eq!(
        s.call(Request::Create {
            parent: ROOT_INODE,
            name: b"a.go".to_vec(),
            flags: O_RDWR,
        }),
        Reply::Error { errno: 17 },
        "EEXIST"
    );
}

#[test]
fn opendir_always_caches_the_directory_and_keeps_an_unchanged_listing() {
    let f = fixture(whole());
    let s = share(&f);
    assert_eq!(
        opened(s.call(Request::Opendir { inode: ROOT_INODE })).1,
        FOPEN_CACHE_DIR
    );
    assert_eq!(
        opened(s.call(Request::Opendir { inode: ROOT_INODE })).1,
        FOPEN_CACHE_DIR | FOPEN_KEEP_CACHE
    );
    let a = entry_of(lookup(&s, ROOT_INODE, "a.go")).inode;
    assert_eq!(
        s.call(Request::Opendir { inode: a }),
        Reply::Error { errno: 20 }
    );
}

#[test]
fn readdir_answers_dt_kinds_and_resumable_offsets() {
    let f = fixture(whole());
    let s = share(&f);
    let pkg = entry_of(lookup(&s, ROOT_INODE, "pkg")).inode;
    let reply = s.call(Request::Readdir {
        inode: pkg,
        offset: 0,
    });
    let Reply::Dir { entries } = reply else {
        panic!("expected a listing, got {reply:?}");
    };
    let b = entry_of(lookup(&s, pkg, "b.go")).inode;
    let sub = entry_of(lookup(&s, pkg, "sub")).inode;
    assert_eq!(
        entries,
        vec![
            WireDirEntry {
                inode: b,
                offset: 1,
                kind: DT_REG,
                name: b"b.go".to_vec()
            },
            WireDirEntry {
                inode: sub,
                offset: 2,
                kind: DT_DIR,
                name: b"sub".to_vec()
            },
        ]
    );
    assert_eq!(
        s.call(Request::Readdir {
            inode: pkg,
            offset: 1
        }),
        Reply::Dir {
            entries: entries[1..].to_vec()
        }
    );
}

#[test]
fn readdirplus_answers_each_entry_with_what_its_lookup_answers() {
    let f = fixture(whole());
    let s = share(&f);
    let pkg = entry_of(lookup(&s, ROOT_INODE, "pkg")).inode;
    let reply = s.call(Request::Readdirplus {
        inode: pkg,
        offset: 0,
    });
    let Reply::DirPlus { entries } = reply else {
        panic!("expected a listing, got {reply:?}");
    };
    assert_eq!(entries.len(), 2);
    for (dirent, entry) in entries {
        let name = String::from_utf8(dirent.name.clone()).unwrap();
        assert_eq!(entry, entry_of(lookup(&s, pkg, &name)));
        assert_eq!(dirent.inode, entry.inode);
    }
    assert_eq!(entry_of(lookup(&s, pkg, "b.go")).attr.size, B.len() as u64);
    let a = entry_of(lookup(&s, ROOT_INODE, "a.go")).inode;
    assert_eq!(
        s.call(Request::Readdirplus {
            inode: a,
            offset: 0
        }),
        Reply::Error { errno: 20 }
    );
}

#[test]
fn structural_requests_land_as_proposals_and_answer_done() {
    let f = fixture(whole());
    let s = share(&f);
    assert!(matches!(
        s.call(Request::Mkdir {
            parent: ROOT_INODE,
            name: b"made".to_vec()
        }),
        Reply::Entry { .. }
    ));
    assert_eq!(
        s.call(Request::Rmdir {
            parent: ROOT_INODE,
            name: b"made".to_vec()
        }),
        Reply::Done
    );
    let pkg = entry_of(lookup(&s, ROOT_INODE, "pkg")).inode;
    assert_eq!(
        s.call(Request::Rename {
            olddir: pkg,
            oldname: b"b.go".to_vec(),
            newdir: ROOT_INODE,
            newname: b"b.go".to_vec(),
            flags: RENAME_NOREPLACE,
        }),
        Reply::Done
    );
    assert_eq!(read_source(&f, "b.go").0, B.as_bytes());
    assert_eq!(
        s.call(Request::Rename {
            olddir: ROOT_INODE,
            oldname: b"b.go".to_vec(),
            newdir: ROOT_INODE,
            newname: b"a.go".to_vec(),
            flags: RENAME_EXCHANGE,
        }),
        Reply::Error { errno: 22 },
        "EINVAL: RENAME_EXCHANGE passes through and is refused"
    );
    assert_eq!(
        s.call(Request::Unlink {
            parent: ROOT_INODE,
            name: b"b.go".to_vec()
        }),
        Reply::Done
    );
    assert_eq!(
        lookup(&s, ROOT_INODE, "b.go"),
        Reply::Entry {
            entry: WireEntry {
                inode: 0,
                attr: WireAttr::default(),
                entry_ttl: SHORT,
                attr_ttl: Duration::ZERO,
            }
        },
        "the deleted name is a negative entry with the short timeout"
    );
    assert_eq!(
        s.call(Request::Unlink {
            parent: ROOT_INODE,
            name: b"b.go".to_vec()
        }),
        Reply::Error { errno: 2 }
    );
}

#[test]
fn open_flags_are_the_bits_whose_condition_holds() {
    use crate::share::fopen_bits;
    for (keep, direct, expected) in [
        (false, false, 0),
        (true, false, FOPEN_KEEP_CACHE),
        (false, true, FOPEN_DIRECT_IO),
        (true, true, FOPEN_KEEP_CACHE | FOPEN_DIRECT_IO),
    ] {
        assert_eq!(
            fopen_bits(&[(keep, FOPEN_KEEP_CACHE), (direct, FOPEN_DIRECT_IO)]),
            expected
        );
    }
}

#[test]
fn the_wrapped_filesystem_is_reachable() {
    let f = fixture(whole());
    let s = share(&f);
    assert_eq!(s.fs().lookup(ROOT_INODE, b"a.go").unwrap().ttl, TTL);
}
