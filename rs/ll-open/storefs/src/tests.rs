//! The filesystem core against a real store: a cold-parsed living database
//! with the daemon's reader pool and single writer.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use leyline_cli_lib::daemon::db_pool::LiveDb;
use leyline_cli_lib::source_write::{self, Base, Grant, Proposed, Refusal, Stale};
use leyline_core::Hash;
use tempfile::TempDir;

use super::open_flags::{O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY};
use super::*;

const A: &str = "package a\n\nfunc A() int { return 1 }\n";
const A_EDIT: &str = "package a\n\nfunc A() int { return 2 }\n";
const A_OTHER: &str = "package a\n\nfunc A() int { return 3 }\n";
const A_SHORT: &str = "package a\n";
const A_TAILED: &str = "package a\n\nfunc A() int { return 1 }\n// tail\n";
const A_BROKEN: &str = "package a\n\nfunc A( {\n";
const B: &str = "package pkg\n\nfunc B() string { return \"b\" }\n";
const C: &str = "package sub\n\nfunc C() {}\n";
const D: &str = "package secret\n\nfunc D() {}\n";

const TTL: Duration = Duration::from_secs(7);

struct Fixture {
    _db_dir: TempDir,
    src: TempDir,
    fs: StoreFs<Arc<LiveDb>>,
}

fn whole() -> Scope {
    Scope {
        see: Grant::Whole,
        propose: Grant::Whole,
    }
}

fn paths(list: &[&str]) -> Grant {
    Grant::Paths(list.iter().map(|p| p.to_string()).collect::<BTreeSet<_>>())
}

/// A store cold-parsed from a source tree holding `a.go` (`a_text`),
/// `pkg/b.go`, `pkg/sub/c.go` and `secret/d.go`; with `checkout`, the source
/// tree is attached as the checkout accepted proposals are written into.
fn fixture_with(scope: Scope, checkout: bool, a_text: &str) -> Fixture {
    let src = tempfile::tempdir().unwrap();
    for (rel, text) in [
        ("a.go", a_text),
        ("pkg/b.go", B),
        ("pkg/sub/c.go", C),
        ("secret/d.go", D),
    ] {
        let p = src.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    let db_dir = tempfile::tempdir().unwrap();
    let live = Arc::new(LiveDb::open_fresh_for_test(&db_dir.path().join("live.db")));
    live.with_write(|conn| {
        leyline_cli_lib::cmd_parse::parse_into_conn(conn, src.path(), None, None)?;
        leyline_cli_lib::checkout::ensure_tables(conn)
    })
    .unwrap();
    let options = Options {
        ttl: TTL,
        checkout: checkout.then(|| Checkout {
            dir: src.path().to_path_buf(),
            lang_filter: None,
        }),
    };
    Fixture {
        _db_dir: db_dir,
        src,
        fs: StoreFs::new(live, scope, options),
    }
}

fn fixture(scope: Scope) -> Fixture {
    fixture_with(scope, false, A)
}

/// What the daemon's `read_source` answers for `path`: the bytes of the
/// blob `_source.content_hash` names, and that hash.
fn read_source(f: &Fixture, path: &str) -> (Vec<u8>, Hash) {
    f.fs.store()
        .with_read(|conn| {
            let (bytes, hash): (Vec<u8>, Vec<u8>) = conn.query_row(
                "SELECT b.blob_bytes, s.content_hash FROM _source s \
                   JOIN source_blobs b ON b.blob_hash = s.content_hash WHERE s.id = ?1",
                [path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Ok((bytes, Hash::from_bytes(hash.try_into().unwrap())))
        })
        .unwrap()
}

/// Another writer (a socket `propose`, another agent) lands `text` at `path`.
fn other_writer(f: &Fixture, path: &str, text: &str) {
    f.fs.store()
        .with_write(|conn| {
            source_write::propose(
                conn,
                &Grant::Whole,
                vec![Proposed {
                    path: path.to_string(),
                    base: Base::Any,
                    bytes: text.as_bytes().to_vec(),
                }],
            )
        })
        .unwrap();
}

/// The inode of `path`, found by LOOKUP from the root.
fn ino(f: &Fixture, path: &str) -> u64 {
    path.split('/').fold(ROOT_INODE, |parent, name| {
        f.fs.lookup(parent, name.as_bytes()).unwrap().attr.inode
    })
}

/// Open `path` with `flags`, replace its bytes with `text`, and return the handle.
fn edit(f: &Fixture, path: &str, text: &str) -> u64 {
    let h = f.fs.open(ino(f, path), O_RDWR | O_TRUNC).unwrap();
    assert_eq!(f.fs.write(h, 0, text.as_bytes()), Ok(text.len()));
    h
}

fn read_all(f: &Fixture, h: u64) -> Vec<u8> {
    f.fs.read(h, 0, u32::MAX).unwrap()
}

// ── reads ────────────────────────────────────────────────────────────────

#[test]
fn a_read_serves_the_blob_the_store_holds_for_the_path() {
    let f = fixture(whole());
    let (stored, _) = read_source(&f, "a.go");
    assert_eq!(stored, A.as_bytes());

    let entry = f.fs.lookup(ROOT_INODE, b"a.go").unwrap();
    assert_eq!(entry.attr.size, A.len() as u64);
    let h = f.fs.open(entry.attr.inode, O_RDONLY).unwrap();
    assert_eq!(read_all(&f, h), stored);
    assert_eq!(f.fs.read(h, 2, 3).unwrap(), &A.as_bytes()[2..5]);
    assert_eq!(
        f.fs.read(h, A.len() as u64 - 2, 10).unwrap(),
        &A.as_bytes()[A.len() - 2..]
    );
    assert_eq!(f.fs.read(h, A.len() as u64 + 10, 5).unwrap(), b"");
}

#[test]
fn an_open_reads_its_blob_once_and_keeps_serving_it() {
    let f = fixture(whole());
    let h = f.fs.open(ino(&f, "a.go"), O_RDONLY).unwrap();
    other_writer(&f, "a.go", A_OTHER);
    assert_eq!(read_all(&f, h), A.as_bytes());
    let fresh = f.fs.open(ino(&f, "a.go"), O_RDONLY).unwrap();
    assert_eq!(read_all(&f, fresh), A_OTHER.as_bytes());
}

#[test]
fn nodes_carry_linux_modes_sizes_and_the_configured_timeout() {
    let f = fixture(whole());
    let (root, ttl) = f.fs.getattr(ROOT_INODE, None).unwrap();
    assert_eq!(ttl, TTL);
    assert_eq!(root.inode, ROOT_INODE);
    assert_eq!(root.kind, Kind::Directory);
    assert_eq!(root.mode(), 0o040555);
    assert_eq!(root.nlink(), 2);
    assert_eq!(root.size, 0);

    let pkg = f.fs.lookup(ROOT_INODE, b"pkg").unwrap();
    assert_eq!(pkg.ttl, TTL);
    assert_eq!(pkg.attr.kind, Kind::Directory);
    assert_eq!(pkg.attr.mode(), 0o040555);

    let b = f.fs.lookup(pkg.attr.inode, b"b.go").unwrap();
    assert_eq!(b.attr.kind, Kind::File);
    assert_eq!(b.attr.mode(), 0o100644);
    assert_eq!(b.attr.nlink(), 1);
    assert_eq!(b.attr.size, B.len() as u64);
    assert_eq!(f.fs.getattr(b.attr.inode, None).unwrap(), (b.attr, TTL));
}

#[test]
fn inode_numbers_are_distinct_per_path_and_stable_within_a_session() {
    let f = fixture(whole());
    let all = [
        ROOT_INODE,
        ino(&f, "a.go"),
        ino(&f, "pkg"),
        ino(&f, "pkg/b.go"),
        ino(&f, "pkg/sub"),
        ino(&f, "pkg/sub/c.go"),
    ];
    assert_eq!(all.iter().collect::<BTreeSet<_>>().len(), all.len());
    assert_eq!(ino(&f, "pkg/b.go"), all[3]);
    other_writer(&f, "a.go", A_OTHER);
    assert_eq!(ino(&f, "a.go"), all[1]);
}

#[test]
fn readdir_lists_each_child_once_with_its_kind_and_resumes_at_an_offset() {
    let f = fixture(whole());
    let pkg = ino(&f, "pkg");
    let listed = f.fs.readdir(pkg, 0).unwrap();
    assert_eq!(
        listed,
        vec![
            DirEntry {
                inode: ino(&f, "pkg/b.go"),
                offset: 1,
                kind: Kind::File,
                name: "b.go".into(),
            },
            DirEntry {
                inode: ino(&f, "pkg/sub"),
                offset: 2,
                kind: Kind::Directory,
                name: "sub".into(),
            },
        ]
    );
    assert_eq!(f.fs.readdir(pkg, 1).unwrap(), listed[1..].to_vec());
    assert_eq!(f.fs.readdir(pkg, 2).unwrap(), vec![]);
    let names: Vec<String> =
        f.fs.readdir(ROOT_INODE, 0)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
    assert_eq!(names, ["a.go", "pkg", "secret"]);
}

#[test]
fn a_name_the_tree_does_not_hold_is_enoent_and_a_file_is_not_a_directory() {
    let f = fixture(whole());
    let a = ino(&f, "a.go");
    assert_eq!(f.fs.lookup(ROOT_INODE, b"nope.go"), Err(Errno::ENOENT));
    assert_eq!(f.fs.lookup(ROOT_INODE, b"pk"), Err(Errno::ENOENT));
    assert_eq!(f.fs.lookup(ROOT_INODE, b""), Err(Errno::ENOENT));
    assert_eq!(f.fs.lookup(ROOT_INODE, b"pkg/b.go"), Err(Errno::ENOENT));
    assert_eq!(f.fs.lookup(ROOT_INODE, b"\xff.go"), Err(Errno::ENOENT));
    assert_eq!(f.fs.lookup(999, b"a.go"), Err(Errno::ENOENT));
    assert_eq!(f.fs.lookup(a, b"x"), Err(Errno::ENOTDIR));
    assert_eq!(f.fs.getattr(999, None), Err(Errno::ENOENT));
    assert_eq!(f.fs.readdir(a, 0), Err(Errno::ENOTDIR));
    assert_eq!(f.fs.readdir(999, 0), Err(Errno::ENOENT));
    assert_eq!(f.fs.open(ino(&f, "pkg"), O_RDONLY), Err(Errno::EISDIR));
    assert_eq!(f.fs.open(999, O_RDONLY), Err(Errno::ENOENT));
}

// ── the write path ───────────────────────────────────────────────────────

#[test]
fn an_accepted_flush_lands_in_the_store_and_advances_the_handles_base() {
    let f = fixture(whole());
    let h = edit(&f, "a.go", A_EDIT);
    assert_eq!(f.fs.flush(h), Ok(()));
    let (stored, hash) = read_source(&f, "a.go");
    assert_eq!(stored, A_EDIT.as_bytes());

    // The base moved to what the flush wrote: a second edit on the same
    // handle is checked against it and lands.
    assert_eq!(f.fs.write(h, 0, A_OTHER.as_bytes()), Ok(A_OTHER.len()));
    assert_eq!(f.fs.fsync(h), Ok(()));
    let (stored, after) = read_source(&f, "a.go");
    assert_eq!(stored, A_OTHER.as_bytes());
    assert_ne!(after, hash);
    f.fs.release(h);

    let reopened = f.fs.open(ino(&f, "a.go"), O_RDONLY).unwrap();
    assert_eq!(read_all(&f, reopened), A_OTHER.as_bytes());
}

#[test]
fn an_accepted_flush_is_written_into_an_attached_checkout_and_its_index_advances() {
    let f = fixture_with(whole(), true, A);
    let h = edit(&f, "a.go", A_EDIT);
    assert_eq!(f.fs.flush(h), Ok(()));
    let (_, hash) = read_source(&f, "a.go");
    assert_eq!(
        std::fs::read_to_string(f.src.path().join("a.go")).unwrap(),
        A_EDIT
    );
    let indexed: Vec<u8> =
        f.fs.store()
            .with_read(|conn| {
                Ok(conn.query_row(
                    "SELECT blob_hash FROM _checkout_index WHERE path = 'a.go'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
    assert_eq!(indexed, hash.as_bytes());
}

#[test]
fn without_a_checkout_an_accepted_flush_writes_no_file() {
    let f = fixture(whole());
    let h = edit(&f, "a.go", A_EDIT);
    assert_eq!(f.fs.flush(h), Ok(()));
    assert_eq!(
        std::fs::read_to_string(f.src.path().join("a.go")).unwrap(),
        A
    );
}

#[test]
fn a_flush_against_a_moved_file_is_estale_and_lands_nothing() {
    let f = fixture(whole());
    let h = f.fs.open(ino(&f, "a.go"), O_RDWR).unwrap();
    other_writer(&f, "a.go", A_OTHER);
    assert_eq!(f.fs.write(h, 0, A_EDIT.as_bytes()), Ok(A_EDIT.len()));
    assert_eq!(f.fs.flush(h), Err(Errno::ESTALE));
    assert_eq!(read_source(&f, "a.go").0, A_OTHER.as_bytes());
    // The staged bytes stay: the close after a refused fsync answers the same.
    assert_eq!(f.fs.flush(h), Err(Errno::ESTALE));
    assert_eq!(read_all(&f, h), A_EDIT.as_bytes());
}

#[test]
fn an_fsync_against_a_moved_file_is_estale_and_lands_nothing() {
    let f = fixture(whole());
    let h = f.fs.open(ino(&f, "a.go"), O_WRONLY).unwrap();
    other_writer(&f, "a.go", A_OTHER);
    assert_eq!(f.fs.write(h, 0, A_EDIT.as_bytes()), Ok(A_EDIT.len()));
    assert_eq!(f.fs.fsync(h), Err(Errno::ESTALE));
    assert_eq!(read_source(&f, "a.go").0, A_OTHER.as_bytes());
}

#[test]
fn a_flush_of_bytes_that_do_not_parse_is_eio_and_lands_nothing() {
    let f = fixture(whole());
    let h = edit(&f, "a.go", A_BROKEN);
    assert_eq!(f.fs.flush(h), Err(Errno::EIO));
    assert_eq!(f.fs.fsync(h), Err(Errno::EIO));
    assert_eq!(read_source(&f, "a.go").0, A.as_bytes());
}

#[test]
fn of_two_handles_on_one_file_the_second_to_flush_is_stale() {
    let f = fixture(whole());
    let first = f.fs.open(ino(&f, "a.go"), O_RDWR).unwrap();
    let second = f.fs.open(ino(&f, "a.go"), O_RDWR).unwrap();
    assert_eq!(f.fs.write(first, 0, A_EDIT.as_bytes()), Ok(A_EDIT.len()));
    assert_eq!(f.fs.write(second, 0, A_OTHER.as_bytes()), Ok(A_OTHER.len()));
    assert_eq!(f.fs.flush(first), Ok(()));
    assert_eq!(f.fs.flush(second), Err(Errno::ESTALE));
    assert_eq!(read_source(&f, "a.go").0, A_EDIT.as_bytes());
}

#[test]
fn concurrent_flushes_serialize_on_the_one_writer() {
    let f = fixture(whole());
    // Two handles on one file, flushed from two threads: exactly one lands.
    let x = edit(&f, "a.go", A_EDIT);
    let y = edit(&f, "a.go", A_OTHER);
    // Two handles on different files: both land.
    let b = edit(&f, "pkg/b.go", "package pkg\n");
    let c = edit(&f, "pkg/sub/c.go", "package sub\n");
    let fs = &f.fs;
    let results = std::thread::scope(|s| {
        let spawned: Vec<_> = [x, y, b, c]
            .into_iter()
            .map(|h| s.spawn(move || fs.flush(h)))
            .collect();
        spawned
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect::<Vec<_>>()
    });
    let same_file = [results[0], results[1]];
    assert!(
        same_file == [Ok(()), Err(Errno::ESTALE)] || same_file == [Err(Errno::ESTALE), Ok(())],
        "{same_file:?}"
    );
    let landed = if results[0].is_ok() { A_EDIT } else { A_OTHER };
    assert_eq!(read_source(&f, "a.go").0, landed.as_bytes());
    assert_eq!(&results[2..], [Ok(()), Ok(())]);
    assert_eq!(read_source(&f, "pkg/b.go").0, b"package pkg\n");
    assert_eq!(read_source(&f, "pkg/sub/c.go").0, b"package sub\n");
}

#[test]
fn a_flush_with_nothing_staged_proposes_nothing() {
    // A read-only open of a file the scope may not propose: a proposal would
    // be refused, so `Ok` means none was made.
    let f = fixture(Scope {
        see: Grant::Whole,
        propose: paths(&["a.go"]),
    });
    let h = f.fs.open(ino(&f, "pkg/b.go"), O_RDONLY).unwrap();
    assert_eq!(f.fs.flush(h), Ok(()));
    assert_eq!(f.fs.fsync(h), Ok(()));

    // After an accepted flush the handle holds nothing new: another writer
    // moving the file does not make the next flush stale.
    let a = edit(&f, "a.go", A_EDIT);
    assert_eq!(f.fs.flush(a), Ok(()));
    other_writer(&f, "a.go", A_OTHER);
    assert_eq!(f.fs.flush(a), Ok(()));
    assert_eq!(read_source(&f, "a.go").0, A_OTHER.as_bytes());
}

#[test]
fn writes_stage_on_the_handle_at_their_offsets() {
    let f = fixture(whole());
    let h = f.fs.open(ino(&f, "a.go"), O_RDWR).unwrap();
    assert_eq!(f.fs.write(h, 0, b"PACK"), Ok(4));
    let mut expect = A.as_bytes().to_vec();
    expect[..4].copy_from_slice(b"PACK");
    assert_eq!(read_all(&f, h), expect);
    let end = A.len() as u64 + 2;
    assert_eq!(f.fs.write(h, end, b"zz"), Ok(2));
    expect.extend_from_slice(b"\0\0zz");
    assert_eq!(read_all(&f, h), expect);
    assert_eq!(
        f.fs.getattr(ino(&f, "a.go"), Some(h)).unwrap().0.size,
        expect.len() as u64
    );
    // The store and a GETATTR without the handle still answer the base.
    assert_eq!(
        f.fs.getattr(ino(&f, "a.go"), None).unwrap().0.size,
        A.len() as u64
    );
    assert_eq!(read_source(&f, "a.go").0, A.as_bytes());
}

#[test]
fn a_handle_that_was_not_opened_for_writing_refuses_writes_and_released_handles_are_gone() {
    let f = fixture(whole());
    let h = f.fs.open(ino(&f, "a.go"), O_RDONLY).unwrap();
    assert_eq!(f.fs.write(h, 0, b"x"), Err(Errno::EBADF));
    assert_eq!(
        f.fs.setattr(
            ino(&f, "a.go"),
            Some(h),
            SetAttr {
                size: Some(0),
                ..SetAttr::default()
            }
        ),
        Err(Errno::EBADF)
    );
    f.fs.release(h);
    assert_eq!(f.fs.read(h, 0, 1), Err(Errno::EBADF));
    assert_eq!(f.fs.flush(h), Err(Errno::EBADF));
    assert_eq!(f.fs.fsync(h), Err(Errno::EBADF));
    assert_eq!(f.fs.write(h, 0, b"x"), Err(Errno::EBADF));
}

#[test]
fn an_unflushed_edit_is_discarded_at_release() {
    let f = fixture(whole());
    let h = edit(&f, "a.go", A_EDIT);
    f.fs.release(h);
    assert_eq!(read_source(&f, "a.go").0, A.as_bytes());
}

// ── truncation ───────────────────────────────────────────────────────────

#[test]
fn open_with_o_trunc_then_a_shorter_write_lands_exactly_those_bytes() {
    let f = fixture(whole());
    let h = f.fs.open(ino(&f, "a.go"), O_WRONLY | O_TRUNC).unwrap();
    assert_eq!(read_all(&f, h), b"");
    assert_eq!(f.fs.write(h, 0, A_SHORT.as_bytes()), Ok(A_SHORT.len()));
    assert_eq!(f.fs.flush(h), Ok(()));
    assert_eq!(read_source(&f, "a.go").0, A_SHORT.as_bytes());
}

#[test]
fn o_trunc_on_a_read_only_open_truncates_nothing() {
    let f = fixture(whole());
    let h = f.fs.open(ino(&f, "a.go"), O_RDONLY | O_TRUNC).unwrap();
    assert_eq!(read_all(&f, h), A.as_bytes());
}

#[test]
fn an_o_trunc_open_with_no_write_lands_an_empty_file_at_flush() {
    let f = fixture(whole());
    let h = f.fs.open(ino(&f, "a.go"), O_WRONLY | O_TRUNC).unwrap();
    assert_eq!(f.fs.flush(h), Ok(()));
    assert_eq!(read_source(&f, "a.go").0, b"");
}

#[test]
fn ftruncate_stages_on_the_handle_and_lands_at_flush() {
    let f = fixture(whole());
    let a = ino(&f, "a.go");
    let h = f.fs.open(a, O_RDWR).unwrap();
    let (attr, ttl) =
        f.fs.setattr(
            a,
            Some(h),
            SetAttr {
                size: Some(A_SHORT.len() as u64),
                ..SetAttr::default()
            },
        )
        .unwrap();
    assert_eq!((attr.size, ttl), (A_SHORT.len() as u64, TTL));
    assert_eq!(read_all(&f, h), A_SHORT.as_bytes());
    assert_eq!(read_source(&f, "a.go").0, A.as_bytes());
    assert_eq!(f.fs.flush(h), Ok(()));
    assert_eq!(read_source(&f, "a.go").0, A_SHORT.as_bytes());

    // Truncate to zero, then write: exactly the written bytes.
    f.fs.setattr(
        a,
        Some(h),
        SetAttr {
            size: Some(0),
            ..SetAttr::default()
        },
    )
    .unwrap();
    assert_eq!(f.fs.write(h, 0, A_EDIT.as_bytes()), Ok(A_EDIT.len()));
    assert_eq!(f.fs.flush(h), Ok(()));
    assert_eq!(read_source(&f, "a.go").0, A_EDIT.as_bytes());
}

#[test]
fn truncate_by_path_is_a_proposal_of_its_own() {
    let f = fixture_with(whole(), false, A_TAILED);
    let a = ino(&f, "a.go");
    let (attr, _) =
        f.fs.setattr(
            a,
            None,
            SetAttr {
                size: Some(A.len() as u64),
                ..SetAttr::default()
            },
        )
        .unwrap();
    assert_eq!(attr.size, A.len() as u64);
    assert_eq!(read_source(&f, "a.go").0, A.as_bytes());

    // Truncating to bytes that do not parse is refused, and lands nothing.
    assert_eq!(
        f.fs.setattr(
            a,
            None,
            SetAttr {
                size: Some(12),
                ..SetAttr::default()
            }
        ),
        Err(Errno::EIO)
    );
    assert_eq!(read_source(&f, "a.go").0, A.as_bytes());
}

#[test]
fn setattr_refuses_what_the_store_does_not_hold() {
    let f = fixture(whole());
    let a = ino(&f, "a.go");
    for set in [
        SetAttr {
            mode: Some(0o600),
            ..SetAttr::default()
        },
        SetAttr {
            uid: Some(0),
            ..SetAttr::default()
        },
        SetAttr {
            gid: Some(0),
            ..SetAttr::default()
        },
    ] {
        assert_eq!(f.fs.setattr(a, None, set), Err(Errno::EPERM));
    }
    // Times are not carried: a SETATTR of nothing else answers the attributes.
    assert_eq!(
        f.fs.setattr(a, None, SetAttr::default()),
        f.fs.getattr(a, None)
    );
    assert_eq!(
        f.fs.setattr(
            ino(&f, "pkg"),
            None,
            SetAttr {
                size: Some(0),
                ..SetAttr::default()
            }
        ),
        Err(Errno::EISDIR)
    );
    assert_eq!(
        f.fs.setattr(999, None, SetAttr::default()),
        Err(Errno::ENOENT)
    );
    assert_eq!(read_source(&f, "a.go").0, A.as_bytes());
}

// ── the scope ────────────────────────────────────────────────────────────

#[test]
fn a_path_outside_the_scope_does_not_exist_for_the_guest() {
    let f = fixture(Scope {
        see: paths(&["a.go", "pkg/b.go"]),
        propose: paths(&["a.go"]),
    });
    assert_eq!(f.fs.lookup(ROOT_INODE, b"secret"), Err(Errno::ENOENT));
    let pkg = ino(&f, "pkg");
    assert_eq!(f.fs.lookup(pkg, b"sub"), Err(Errno::ENOENT));
    let names = |inode| -> Vec<String> {
        f.fs.readdir(inode, 0)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect()
    };
    assert_eq!(names(ROOT_INODE), ["a.go", "pkg"]);
    assert_eq!(names(pkg), ["b.go"]);
}

#[test]
fn the_root_is_a_directory_when_the_scope_sees_nothing() {
    let f = fixture(Scope {
        see: paths(&[]),
        propose: paths(&[]),
    });
    assert_eq!(
        f.fs.getattr(ROOT_INODE, None).unwrap().0.kind,
        Kind::Directory
    );
    assert_eq!(f.fs.readdir(ROOT_INODE, 0), Ok(vec![]));
    assert_eq!(f.fs.lookup(ROOT_INODE, b"a.go"), Err(Errno::ENOENT));
}

#[test]
fn a_write_outside_the_proposal_scope_is_eacces() {
    let f = fixture(Scope {
        see: paths(&["a.go", "pkg/b.go"]),
        propose: paths(&["a.go"]),
    });
    let b = ino(&f, "pkg/b.go");
    assert_eq!(f.fs.getattr(b, None).unwrap().0.mode(), 0o100444);
    assert_eq!(f.fs.open(b, O_RDWR), Err(Errno::EACCES));
    assert_eq!(f.fs.open(b, O_WRONLY), Err(Errno::EACCES));
    assert_eq!(
        f.fs.setattr(
            b,
            None,
            SetAttr {
                size: Some(0),
                ..SetAttr::default()
            }
        ),
        Err(Errno::EACCES)
    );
    let h = f.fs.open(b, O_RDONLY).unwrap();
    assert_eq!(read_all(&f, h), B.as_bytes());
    assert_eq!(read_source(&f, "pkg/b.go").0, B.as_bytes());
    // The file the scope may propose is writable.
    let a = edit(&f, "a.go", A_EDIT);
    assert_eq!(f.fs.flush(a), Ok(()));
}

#[test]
fn each_refusal_answers_its_linux_errno() {
    let refused = |r: Refusal| errno_for(&anyhow::Error::from(r));
    let h = Hash::from_bytes([1; 32]);
    assert_eq!(
        refused(Refusal::Stale(vec![Stale {
            path: "a.go".into(),
            expected: h,
            current: h,
        }])),
        Errno::ESTALE
    );
    assert_eq!(Errno::ESTALE, Errno(116));
    assert_eq!(
        refused(Refusal::OutsideGrant {
            path: "a.go".into()
        }),
        Errno::EACCES
    );
    assert_eq!(Errno::EACCES, Errno(13));
    assert_eq!(
        refused(Refusal::Syntax {
            path: "a.go".into(),
            detail: "x".into()
        }),
        Errno::EIO
    );
    assert_eq!(Errno::EIO, Errno(5));
    assert_eq!(
        refused(Refusal::NoSuchFile {
            path: "a.go".into()
        }),
        Errno::EIO
    );
    assert_eq!(errno_for(&anyhow::anyhow!("disk on fire")), Errno::EIO);
}

// ── what the store has no proposal for ───────────────────────────────────

#[test]
fn create_mkdir_unlink_rmdir_and_rename_are_eperm_and_change_nothing() {
    let f = fixture(whole());
    let pkg = ino(&f, "pkg");
    assert_eq!(
        f.fs.create(ROOT_INODE, b"new.go", O_RDWR).map(|_| ()),
        Err(Errno::EPERM)
    );
    assert_eq!(
        f.fs.mkdir(ROOT_INODE, b"newdir").map(|_| ()),
        Err(Errno::EPERM)
    );
    assert_eq!(f.fs.unlink(ROOT_INODE, b"a.go"), Err(Errno::EPERM));
    assert_eq!(f.fs.rmdir(ROOT_INODE, b"pkg"), Err(Errno::EPERM));
    assert_eq!(
        f.fs.rename(ROOT_INODE, b"a.go", pkg, b"a.go"),
        Err(Errno::EPERM)
    );
    assert_eq!(read_source(&f, "a.go").0, A.as_bytes());
    assert_eq!(f.fs.lookup(ROOT_INODE, b"new.go"), Err(Errno::ENOENT));
}

#[test]
fn structural_requests_naming_what_the_guest_cannot_see_are_enoent() {
    let f = fixture(Scope {
        see: paths(&["a.go"]),
        propose: paths(&["a.go"]),
    });
    let a = ino(&f, "a.go");
    assert_eq!(
        f.fs.create(999, b"n.go", O_RDWR).map(|_| ()),
        Err(Errno::ENOENT)
    );
    assert_eq!(
        f.fs.create(a, b"n.go", O_RDWR).map(|_| ()),
        Err(Errno::ENOTDIR)
    );
    assert_eq!(f.fs.mkdir(999, b"d").map(|_| ()), Err(Errno::ENOENT));
    assert_eq!(f.fs.unlink(ROOT_INODE, b"secret"), Err(Errno::ENOENT));
    assert_eq!(f.fs.rmdir(ROOT_INODE, b"secret"), Err(Errno::ENOENT));
    assert_eq!(
        f.fs.rename(ROOT_INODE, b"pkg", ROOT_INODE, b"x"),
        Err(Errno::ENOENT)
    );
    assert_eq!(
        f.fs.rename(ROOT_INODE, b"a.go", 999, b"x"),
        Err(Errno::ENOENT)
    );
}
