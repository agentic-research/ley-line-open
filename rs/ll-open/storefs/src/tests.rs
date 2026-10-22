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

pub(crate) const A: &str = "package a\n\nfunc A() int { return 1 }\n";
pub(crate) const A_EDIT: &str = "package a\n\nfunc A() int { return 2 }\n";
pub(crate) const A_OTHER: &str = "package a\n\nfunc A() int { return 3 }\n";
const A_SHORT: &str = "package a\n";
const A_TAILED: &str = "package a\n\nfunc A() int { return 1 }\n// tail\n";
pub(crate) const A_BROKEN: &str = "package a\n\nfunc A( {\n";
pub(crate) const B: &str = "package pkg\n\nfunc B() string { return \"b\" }\n";
const C: &str = "package sub\n\nfunc C() {}\n";
const D: &str = "package secret\n\nfunc D() {}\n";

/// The timeout of a path no change was recorded for within [`WINDOW`].
pub(crate) const TTL: Duration = Duration::from_secs(7);
/// The timeout of a path changed within [`WINDOW`].
pub(crate) const SHORT: Duration = Duration::from_millis(250);
const WINDOW: Duration = Duration::from_secs(30);

pub(crate) fn freshness() -> Freshness {
    Freshness {
        settled: TTL,
        changed: SHORT,
        window: WINDOW,
    }
}

/// A clock the test moves: no test sleeps.
#[derive(Default)]
pub(crate) struct TestClock(parking_lot::Mutex<Duration>);

impl TestClock {
    pub(crate) fn advance(&self, by: Duration) {
        *self.0.lock() += by;
    }
}

/// The wall-clock time of a [`TestClock`]'s origin: a path no change was
/// recorded for reports it as its times.
pub(crate) const ORIGIN: Duration = Duration::new(1_760_000_000, 500);

impl Clock for TestClock {
    fn now(&self) -> Duration {
        *self.0.lock()
    }

    fn origin(&self) -> Duration {
        ORIGIN
    }
}

pub(crate) struct Fixture {
    pub(crate) _db_dir: TempDir,
    pub(crate) src: TempDir,
    pub(crate) fs: StoreFs<Arc<LiveDb>>,
    pub(crate) clock: Arc<TestClock>,
}

pub(crate) fn whole() -> Scope {
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
pub(crate) fn fixture_with(scope: Scope, checkout: bool, a_text: &str) -> Fixture {
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
        freshness: freshness(),
        checkout: checkout.then(|| Checkout {
            dir: src.path().to_path_buf(),
            lang_filter: None,
        }),
    };
    let clock = Arc::new(TestClock::default());
    Fixture {
        _db_dir: db_dir,
        src,
        fs: StoreFs::with_clock(live, scope, options, clock.clone()),
        clock,
    }
}

pub(crate) fn fixture(scope: Scope) -> Fixture {
    fixture_with(scope, false, A)
}

/// What the daemon's `read_source` answers for `path`: the bytes of the
/// blob the store's manifest (`_tree`) names, and that hash.
pub(crate) fn read_source(f: &Fixture, path: &str) -> (Vec<u8>, Hash) {
    f.fs.store()
        .with_read(|conn| {
            let (bytes, hash): (Vec<u8>, Vec<u8>) = conn.query_row(
                "SELECT b.blob_bytes, t.blob_hash FROM _tree t \
                   JOIN source_blobs b ON b.blob_hash = t.blob_hash WHERE t.path = ?1",
                [path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Ok((bytes, Hash::from_bytes(hash.try_into().unwrap())))
        })
        .unwrap()
}

/// Another writer (a socket `propose`, another agent) lands `text` at `path`.
pub(crate) fn other_writer(f: &Fixture, path: &str, text: &str) {
    f.fs.store()
        .with_write(|conn| {
            source_write::propose(
                conn,
                &Grant::Whole,
                vec![Proposed::write(path, Base::Any, text.as_bytes().to_vec())],
            )
        })
        .unwrap();
}

/// Whether the store holds a file at `path`.
fn stored(f: &Fixture, path: &str) -> bool {
    f.fs.store()
        .with_read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT 1 FROM _tree WHERE path = ?1 AND blob_hash IS NOT NULL",
                    [path],
                    |_| Ok(()),
                )
                .is_ok())
        })
        .unwrap()
}

/// Count the projection passes (one per landed proposal) from here on: a
/// temp trigger on the writer's connection counts each `derived_from`
/// stamp, which every pass writes once.
pub(crate) fn count_passes(f: &Fixture) {
    f.fs.store()
        .with_write(|conn| {
            conn.execute_batch(
                "CREATE TEMP TABLE passes (n INTEGER);
                 CREATE TEMP TRIGGER count_pass AFTER INSERT ON main._meta
                   WHEN NEW.key = 'derived_from'
                 BEGIN INSERT INTO passes VALUES (1); END;",
            )?;
            Ok(())
        })
        .unwrap();
}

pub(crate) fn passes(f: &Fixture) -> i64 {
    f.fs.store()
        .with_write(|conn| Ok(conn.query_row("SELECT count(*) FROM passes", [], |r| r.get(0))?))
        .unwrap()
}

pub(crate) fn on_disk(f: &Fixture, path: &str) -> Option<String> {
    std::fs::read_to_string(f.src.path().join(path)).ok()
}

fn names(f: &Fixture, inode: u64) -> Vec<String> {
    f.fs.readdir(inode, 0)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect()
}

/// CREATE `name` in `parent`, write `text`, and flush: what `echo > name`
/// does. Answers the handle and the flush's result.
fn create_file(f: &Fixture, parent: u64, name: &str, text: &str) -> (u64, Result<(), Errno>) {
    let (_, h) = f.fs.create(parent, name.as_bytes(), O_RDWR).unwrap();
    assert_eq!(f.fs.write(h, 0, text.as_bytes()), Ok(text.len()));
    (h, f.fs.flush(h))
}

/// The inode of `path`, found by LOOKUP from the root.
pub(crate) fn ino(f: &Fixture, path: &str) -> u64 {
    path.split('/').fold(ROOT_INODE, |parent, name| {
        f.fs.lookup(parent, name.as_bytes()).unwrap().attr.inode
    })
}

/// Open `path` with `flags`, replace its bytes with `text`, and return the handle.
fn edit(f: &Fixture, path: &str, text: &str) -> u64 {
    let h = f.fs.open(ino(f, path), O_RDWR | O_TRUNC).unwrap().handle;
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
    let h = f.fs.open(entry.attr.inode, O_RDONLY).unwrap().handle;
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
    let h = f.fs.open(ino(&f, "a.go"), O_RDONLY).unwrap().handle;
    other_writer(&f, "a.go", A_OTHER);
    assert_eq!(read_all(&f, h), A.as_bytes());
    let fresh = f.fs.open(ino(&f, "a.go"), O_RDONLY).unwrap().handle;
    assert_eq!(read_all(&f, fresh), A_OTHER.as_bytes());
}

#[test]
fn nodes_carry_linux_modes_sizes_and_the_configured_timeout() {
    let f = fixture(whole());
    let (root, ttl) = f.fs.getattr(ROOT_INODE, None).unwrap();
    assert_eq!(ttl, TTL);
    assert_eq!(root.inode, ROOT_INODE);
    assert_eq!(root.kind, Kind::Directory);
    assert_eq!(root.mode(), 0o040755);
    assert_eq!(root.nlink(), 2);
    assert_eq!(root.size, 0);

    let pkg = f.fs.lookup(ROOT_INODE, b"pkg").unwrap();
    assert_eq!(pkg.ttl, TTL);
    assert_eq!(pkg.attr.kind, Kind::Directory);
    assert_eq!(pkg.attr.mode(), 0o040755);

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

    let reopened = f.fs.open(ino(&f, "a.go"), O_RDONLY).unwrap().handle;
    assert_eq!(read_all(&f, reopened), A_OTHER.as_bytes());
}

/// A store that counts [`Store::request_publish`] calls.
struct Publishing {
    inner: Arc<LiveDb>,
    published: std::sync::atomic::AtomicUsize,
}

impl Store for Publishing {
    fn with_read<T>(&self, f: impl FnOnce(&Connection) -> anyhow::Result<T>) -> anyhow::Result<T> {
        self.inner.with_read(f)
    }

    fn with_write<T>(&self, f: impl FnOnce(&Connection) -> anyhow::Result<T>) -> anyhow::Result<T> {
        self.inner.with_write(f)
    }

    fn request_publish(&self) {
        self.published
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[test]
fn each_landed_proposal_requests_a_publish_and_a_refused_one_does_not() {
    let f = fixture(whole());
    let store = Arc::new(Publishing {
        inner: Arc::clone(f.fs.store()),
        published: Default::default(),
    });
    let fs = StoreFs::new(Arc::clone(&store), whole(), Options::default());
    let a = fs.lookup(ROOT_INODE, b"a.go").unwrap().attr.inode;
    let published = || store.published.load(std::sync::atomic::Ordering::SeqCst);

    // Another writer changed the same line: the flush is refused.
    let stale = fs.open(a, O_RDWR | O_TRUNC).unwrap().handle;
    other_writer(&f, "a.go", A_OTHER);
    fs.write(stale, 0, A_EDIT.as_bytes()).unwrap();
    assert_eq!(fs.flush(stale), Err(Errno::ESTALE));
    assert_eq!(published(), 0, "a refused proposal requests nothing");
    fs.release(stale);

    let h = fs.open(a, O_RDWR | O_TRUNC).unwrap().handle;
    fs.write(h, 0, A_EDIT.as_bytes()).unwrap();
    assert_eq!(fs.flush(h), Ok(()));
    assert_eq!(published(), 1);
    assert_eq!(read_source(&f, "a.go").0, A_EDIT.as_bytes());
    assert_eq!(fs.flush(h), Ok(()));
    assert_eq!(published(), 1, "nothing staged: no proposal, no request");
    fs.unlink(ROOT_INODE, b"a.go").unwrap();
    assert_eq!(published(), 2, "a delete is a landed proposal");
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
    let h = f.fs.open(ino(&f, "a.go"), O_RDWR).unwrap().handle;
    assert_eq!(f.fs.write(h, 0, A_EDIT.as_bytes()), Ok(A_EDIT.len()));
    other_writer(&f, "a.go", A_OTHER);
    assert_eq!(f.fs.flush(h), Err(Errno::ESTALE));
    assert_eq!(read_source(&f, "a.go").0, A_OTHER.as_bytes());
    // The staged bytes stay: the close after a refused fsync answers the same.
    assert_eq!(f.fs.flush(h), Err(Errno::ESTALE));
    assert_eq!(read_all(&f, h), A_EDIT.as_bytes());
}

#[test]
fn an_fsync_against_a_moved_file_is_estale_and_lands_nothing() {
    let f = fixture(whole());
    let h = f.fs.open(ino(&f, "a.go"), O_WRONLY).unwrap().handle;
    assert_eq!(f.fs.write(h, 0, A_EDIT.as_bytes()), Ok(A_EDIT.len()));
    other_writer(&f, "a.go", A_OTHER);
    assert_eq!(f.fs.fsync(h), Err(Errno::ESTALE));
    assert_eq!(read_source(&f, "a.go").0, A_OTHER.as_bytes());
}

/// The spike's D1 under ADR-0045 T6: open, *foreign change*, write. The
/// WRITE stages; the FLUSH decides. A file another writer deleted is
/// stale at the flush (a moved base with nothing to merge into).
#[test]
fn a_write_against_a_moved_file_stages_and_the_flush_decides() {
    let f = fixture(whole());
    let h = f.fs.open(ino(&f, "a.go"), O_RDWR).unwrap().handle;
    other_writer(&f, "a.go", A_OTHER);
    assert_eq!(f.fs.write(h, 0, A_EDIT.as_bytes()), Ok(A_EDIT.len()));
    assert_eq!(read_all(&f, h), A_EDIT.as_bytes(), "staged");
    assert_eq!(f.fs.fsync(h), Err(Errno::ESTALE), "the same line moved");
    assert_eq!(read_source(&f, "a.go").0, A_OTHER.as_bytes());

    let b = f.fs.open(ino(&f, "pkg/b.go"), O_WRONLY).unwrap().handle;
    f.fs.store()
        .with_write(|conn| {
            source_write::propose(
                conn,
                &Grant::Whole,
                vec![Proposed::delete("pkg/b.go", Base::Any)],
            )
        })
        .unwrap();
    assert_eq!(f.fs.write(b, 0, B.as_bytes()), Ok(B.len()));
    assert_eq!(f.fs.flush(b), Err(Errno::ESTALE));

    // A handle whose own flush advanced its base keeps writing.
    let h = edit(&f, "a.go", A_EDIT);
    assert_eq!(f.fs.flush(h), Ok(()));
    assert_eq!(f.fs.write(h, 0, A_TAILED.as_bytes()), Ok(A_TAILED.len()));
    assert_eq!(f.fs.flush(h), Ok(()));
}

#[test]
fn of_two_handles_on_one_file_the_second_to_flush_is_stale() {
    let f = fixture(whole());
    let first = f.fs.open(ino(&f, "a.go"), O_RDWR).unwrap().handle;
    let second = f.fs.open(ino(&f, "a.go"), O_RDWR).unwrap().handle;
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
    let h = f.fs.open(ino(&f, "pkg/b.go"), O_RDONLY).unwrap().handle;
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
    let h = f.fs.open(ino(&f, "a.go"), O_RDWR).unwrap().handle;
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
    let h = f.fs.open(ino(&f, "a.go"), O_RDONLY).unwrap().handle;
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
    let h =
        f.fs.open(ino(&f, "a.go"), O_WRONLY | O_TRUNC)
            .unwrap()
            .handle;
    assert_eq!(read_all(&f, h), b"");
    assert_eq!(f.fs.write(h, 0, A_SHORT.as_bytes()), Ok(A_SHORT.len()));
    assert_eq!(f.fs.flush(h), Ok(()));
    assert_eq!(read_source(&f, "a.go").0, A_SHORT.as_bytes());
}

#[test]
fn o_trunc_on_a_read_only_open_truncates_nothing() {
    let f = fixture(whole());
    let h =
        f.fs.open(ino(&f, "a.go"), O_RDONLY | O_TRUNC)
            .unwrap()
            .handle;
    assert_eq!(read_all(&f, h), A.as_bytes());
}

#[test]
fn an_o_trunc_open_with_no_write_lands_an_empty_file_at_flush() {
    let f = fixture(whole());
    let h =
        f.fs.open(ino(&f, "a.go"), O_WRONLY | O_TRUNC)
            .unwrap()
            .handle;
    assert_eq!(f.fs.flush(h), Ok(()));
    assert_eq!(read_source(&f, "a.go").0, b"");
}

#[test]
fn ftruncate_stages_on_the_handle_and_lands_at_flush() {
    let f = fixture(whole());
    let a = ino(&f, "a.go");
    let h = f.fs.open(a, O_RDWR).unwrap().handle;
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

    // Truncating to bytes that do not parse lands them, with a diagnostic
    // (ADR-0045 T3).
    let (attr, _) =
        f.fs.setattr(
            a,
            None,
            SetAttr {
                size: Some(12),
                ..SetAttr::default()
            },
        )
        .unwrap();
    assert_eq!(attr.size, 12);
    assert_eq!(read_source(&f, "a.go").0, &A.as_bytes()[..12]);
}

#[test]
fn setattr_refuses_what_the_store_does_not_hold() {
    let f = fixture(whole());
    let a = ino(&f, "a.go");
    for set in [
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
    // A directory is writable when the scope may propose a path under it.
    assert_eq!(f.fs.getattr(ROOT_INODE, None).unwrap().0.mode(), 0o040755);
    assert_eq!(
        f.fs.getattr(ino(&f, "pkg"), None).unwrap().0.mode(),
        0o040555
    );
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
    let h = f.fs.open(b, O_RDONLY).unwrap().handle;
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
            expected: Some(h),
            current: Some(h),
        }])),
        Errno::ESTALE
    );
    assert_eq!(Errno::ESTALE, Errno(116));
    // A file deleted since it was read is stale.
    assert_eq!(
        refused(Refusal::Stale(vec![Stale {
            path: "a.go".into(),
            expected: Some(h),
            current: None,
        }])),
        Errno::ESTALE
    );
    // A create whose path now holds a file: the file exists.
    assert_eq!(
        refused(Refusal::Stale(vec![Stale {
            path: "a.go".into(),
            expected: None,
            current: Some(h),
        }])),
        Errno::EEXIST
    );
    assert_eq!(Errno::EEXIST, Errno(17));
    // A rename whose create met a file and whose delete met a moved file
    // is stale: the moved file decides.
    assert_eq!(
        refused(Refusal::Stale(vec![
            Stale {
                path: "a.go".into(),
                expected: Some(h),
                current: Some(Hash::from_bytes([2; 32])),
            },
            Stale {
                path: "b.go".into(),
                expected: None,
                current: Some(h),
            },
        ])),
        Errno::ESTALE
    );
    assert_eq!(
        refused(Refusal::NoSuchFile {
            path: "a.go".into()
        }),
        Errno::ENOENT
    );
    assert_eq!(Errno::ENOENT, Errno(2));
    assert_eq!(
        refused(Refusal::OutsideGrant {
            path: "a.go".into()
        }),
        Errno::EACCES
    );
    assert_eq!(Errno::EACCES, Errno(13));
    assert_eq!(
        refused(Refusal::DuplicatePath {
            path: "a.go".into()
        }),
        Errno::EIO
    );
    assert_eq!(Errno::EIO, Errno(5));
    assert_eq!(errno_for(&anyhow::anyhow!("disk on fire")), Errno::EIO);
}

// ── structural requests: CREATE, UNLINK, RENAME, MKDIR, RMDIR (47fb03) ───

const N: &str = "package a\n\nfunc N() int { return 7 }\n";

/// `echo > new.go`: CREATE, WRITE, FLUSH. The file exists for the guest
/// from CREATE on; the flush lands it in the store and the checkout.
#[test]
fn create_write_and_flush_lands_a_new_file_in_the_store_and_the_checkout() {
    let f = fixture_with(whole(), true, A);
    let (entry, h) = f.fs.create(ROOT_INODE, b"new.go", O_RDWR).unwrap();
    assert_eq!(entry.attr.kind, Kind::File);
    assert_eq!(entry.attr.mode(), 0o100644);
    assert_eq!(entry.attr.size, 0);
    assert_eq!(entry.ttl, TTL);
    assert_eq!(ino(&f, "new.go"), entry.attr.inode);
    assert!(names(&f, ROOT_INODE).contains(&"new.go".to_string()));
    assert!(!stored(&f, "new.go"), "nothing lands before the flush");

    assert_eq!(f.fs.write(h, 0, N.as_bytes()), Ok(N.len()));
    assert_eq!(f.fs.flush(h), Ok(()));
    assert_eq!(read_source(&f, "new.go").0, N.as_bytes());
    assert_eq!(on_disk(&f, "new.go").as_deref(), Some(N));
    assert_eq!(
        f.fs.getattr(entry.attr.inode, None).unwrap().0.size,
        N.len() as u64
    );

    // The handle's base is now the blob it created: a second edit on it is
    // a replace, checked against that blob.
    assert_eq!(f.fs.write(h, 0, A_EDIT.as_bytes()), Ok(A_EDIT.len()));
    assert_eq!(f.fs.fsync(h), Ok(()));
    assert_eq!(read_source(&f, "new.go").0, A_EDIT.as_bytes());
    f.fs.release(h);
}

/// `touch new.go`: a created file with nothing written lands empty.
#[test]
fn a_create_with_nothing_written_lands_an_empty_file() {
    let f = fixture(whole());
    let (_, h) = f.fs.create(ROOT_INODE, b"empty.go", O_WRONLY).unwrap();
    assert_eq!(f.fs.flush(h), Ok(()));
    assert_eq!(read_source(&f, "empty.go").0, b"");
}

/// CREATE of a name that exists is `EEXIST`; a create another writer beat
/// to the path is `EEXIST` at the flush and lands nothing.
#[test]
fn a_create_over_an_existing_name_is_eexist() {
    let f = fixture(whole());
    assert_eq!(
        f.fs.create(ROOT_INODE, b"a.go", O_RDWR).map(|_| ()),
        Err(Errno::EEXIST)
    );
    assert_eq!(
        f.fs.create(ROOT_INODE, b"pkg", O_RDWR).map(|_| ()),
        Err(Errno::EEXIST)
    );
    let (_, h) = f.fs.create(ROOT_INODE, b"race.go", O_RDWR).unwrap();
    f.fs.store()
        .with_write(|conn| {
            source_write::propose(
                conn,
                &Grant::Whole,
                vec![Proposed::create("race.go", A_OTHER.as_bytes().to_vec())],
            )
        })
        .unwrap();
    assert_eq!(f.fs.write(h, 0, N.as_bytes()), Ok(N.len()));
    assert_eq!(f.fs.flush(h), Err(Errno::EEXIST));
    assert_eq!(read_source(&f, "race.go").0, A_OTHER.as_bytes());
}

/// A created file whose bytes do not parse lands at its flush, with a
/// diagnostic (ADR-0045 T3); the guest's fix lands over it.
#[test]
fn a_created_file_that_does_not_parse_lands_and_its_fix_lands_over_it() {
    let f = fixture(whole());
    let (h, flushed) = create_file(&f, ROOT_INODE, "broken.go", A_BROKEN);
    assert_eq!(flushed, Ok(()));
    assert_eq!(read_source(&f, "broken.go").0, A_BROKEN.as_bytes());
    assert_eq!(f.fs.write(h, 0, N.as_bytes()), Ok(N.len()));
    assert_eq!(
        f.fs.setattr(
            ino(&f, "broken.go"),
            Some(h),
            SetAttr {
                size: Some(N.len() as u64),
                ..SetAttr::default()
            }
        )
        .map(|_| ()),
        Ok(())
    );
    assert_eq!(f.fs.flush(h), Ok(()));
    assert_eq!(read_source(&f, "broken.go").0, N.as_bytes());
}

/// A file in no projected language (an editor's temp file, a note) is a
/// file like any other (ADR-0045 T1): proposed at its flush, stored,
/// readable, listable, and unlinked by a proposal.
#[test]
fn a_file_in_no_projected_language_lands_in_the_store() {
    let f = fixture(whole());
    count_passes(&f);
    let (h, flushed) = create_file(&f, ROOT_INODE, "notes.txt", "hello\n");
    assert_eq!(flushed, Ok(()));
    f.fs.release(h);
    assert_eq!(passes(&f), 1, "the flush was one proposal");
    assert!(stored(&f, "notes.txt"));
    let notes = ino(&f, "notes.txt");
    assert_eq!(f.fs.getattr(notes, None).unwrap().0.size, 6);
    let r = f.fs.open(notes, O_RDONLY).unwrap().handle;
    assert_eq!(read_all(&f, r), b"hello\n");
    assert!(names(&f, ROOT_INODE).contains(&"notes.txt".to_string()));
    assert_eq!(f.fs.unlink(ROOT_INODE, b"notes.txt"), Ok(()));
    assert_eq!(f.fs.lookup(ROOT_INODE, b"notes.txt"), Err(Errno::ENOENT));
}

/// UNLINK is a delete proposal: the file leaves the store and the
/// checkout. A directory it empties stays until RMDIR, as on a disk.
#[test]
fn unlink_deletes_the_file_from_the_store_and_the_checkout() {
    let f = fixture_with(whole(), true, A);
    assert_eq!(f.fs.unlink(ROOT_INODE, b"a.go"), Ok(()));
    assert_eq!(f.fs.lookup(ROOT_INODE, b"a.go"), Err(Errno::ENOENT));
    assert!(!stored(&f, "a.go"));
    assert_eq!(on_disk(&f, "a.go"), None);

    let sub = ino(&f, "pkg/sub");
    assert_eq!(f.fs.unlink(sub, b"c.go"), Ok(()));
    assert!(!stored(&f, "pkg/sub/c.go"));
    assert_eq!(names(&f, ino(&f, "pkg")), ["b.go", "sub"]);
    assert_eq!(names(&f, sub), Vec::<String>::new());
    assert_eq!(f.fs.rmdir(ino(&f, "pkg"), b"sub"), Ok(()));
    assert_eq!(names(&f, ino(&f, "pkg")), ["b.go"]);
}

/// A created file unlinked before its flush is never proposed: the flush
/// of its open handle lands nothing.
#[test]
fn a_created_file_unlinked_before_its_flush_lands_nothing() {
    let f = fixture(whole());
    let (_, h) = f.fs.create(ROOT_INODE, b"gone.go", O_RDWR).unwrap();
    assert_eq!(f.fs.write(h, 0, N.as_bytes()), Ok(N.len()));
    assert_eq!(f.fs.unlink(ROOT_INODE, b"gone.go"), Ok(()));
    assert_eq!(f.fs.flush(h), Ok(()));
    assert!(!stored(&f, "gone.go"));
    assert_eq!(f.fs.lookup(ROOT_INODE, b"gone.go"), Err(Errno::ENOENT));
}

#[test]
fn unlink_answers_its_errnos() {
    let f = fixture(Scope {
        see: paths(&["a.go", "pkg/b.go"]),
        propose: paths(&["a.go"]),
    });
    assert_eq!(f.fs.unlink(ROOT_INODE, b"pkg"), Err(Errno::EISDIR));
    assert_eq!(f.fs.unlink(ROOT_INODE, b"nope.go"), Err(Errno::ENOENT));
    assert_eq!(f.fs.unlink(ino(&f, "pkg"), b"b.go"), Err(Errno::EACCES));
    assert!(stored(&f, "pkg/b.go"));
    // Deleted by another writer since the guest looked it up: gone.
    let a = ino(&f, "a.go");
    f.fs.store()
        .with_write(|conn| {
            source_write::propose(
                conn,
                &Grant::Whole,
                vec![Proposed::delete("a.go", Base::Any)],
            )
        })
        .unwrap();
    assert_eq!(f.fs.getattr(a, None), Err(Errno::ENOENT));
    assert_eq!(f.fs.unlink(ROOT_INODE, b"a.go"), Err(Errno::ENOENT));
}

/// The save an editor or agent harness makes: write a temp file, then
/// rename it over the original. The temp file lands in the store at its
/// flush (ADR-0045 T1), and the rename is one proposal: the temp file
/// deleted and the original's new bytes, checked against the blob it held.
#[test]
fn write_temp_then_rename_over_the_original_is_one_proposal() {
    let f = fixture_with(whole(), true, A);
    let a = ino(&f, "a.go");
    count_passes(&f);
    let (h, flushed) = create_file(&f, ROOT_INODE, "a.go.tmp.4242.1700000000", A_EDIT);
    assert_eq!(flushed, Ok(()));
    f.fs.release(h);
    assert_eq!(passes(&f), 1, "the temp file's flush");
    assert_eq!(
        f.fs.rename(
            ROOT_INODE,
            b"a.go.tmp.4242.1700000000",
            ROOT_INODE,
            b"a.go",
            0
        ),
        Ok(())
    );
    assert_eq!(passes(&f), 2, "the rename is one proposal");
    assert_eq!(read_source(&f, "a.go").0, A_EDIT.as_bytes());
    assert_eq!(on_disk(&f, "a.go").as_deref(), Some(A_EDIT));
    assert_eq!(
        f.fs.lookup(ROOT_INODE, b"a.go.tmp.4242.1700000000"),
        Err(Errno::ENOENT)
    );
    assert!(!f.src.path().join("a.go.tmp.4242.1700000000").exists());
    assert_ne!(ino(&f, "a.go"), a, "the name now names the renamed inode");
}

/// A rename of a stored file is one proposal: the source deleted against
/// the blob it held and the target created with its bytes. A handle open
/// on the source follows it.
#[test]
fn rename_moves_a_stored_file_in_one_proposal_and_its_handle_follows() {
    let f = fixture_with(whole(), true, A);
    let a = ino(&f, "a.go");
    let h = f.fs.open(a, O_RDWR).unwrap().handle;
    count_passes(&f);
    let pkg = ino(&f, "pkg");
    assert_eq!(
        f.fs.rename(ROOT_INODE, b"a.go", pkg, b"moved.go", 0),
        Ok(())
    );
    assert_eq!(passes(&f), 1);
    assert!(!stored(&f, "a.go"));
    assert_eq!(read_source(&f, "pkg/moved.go").0, A.as_bytes());
    assert_eq!(on_disk(&f, "a.go"), None);
    assert_eq!(on_disk(&f, "pkg/moved.go").as_deref(), Some(A));
    assert_eq!(ino(&f, "pkg/moved.go"), a, "the inode moved with the name");
    assert_eq!(f.fs.lookup(ROOT_INODE, b"a.go"), Err(Errno::ENOENT));

    assert_eq!(f.fs.write(h, 0, A_EDIT.as_bytes()), Ok(A_EDIT.len()));
    assert_eq!(f.fs.flush(h), Ok(()));
    assert_eq!(read_source(&f, "pkg/moved.go").0, A_EDIT.as_bytes());
    assert!(!stored(&f, "a.go"));
}

/// A rename over an existing file replaces it in the same proposal that
/// removes the source; a stored temp file (a name the store can hold)
/// lands at its flush and leaves at the rename.
#[test]
fn rename_over_an_existing_file_replaces_it() {
    let f = fixture_with(whole(), true, A);
    let (h, flushed) = create_file(&f, ROOT_INODE, "b-new.go", B);
    assert_eq!(flushed, Ok(()));
    f.fs.release(h);
    assert!(stored(&f, "b-new.go"));
    let pkg = ino(&f, "pkg");
    count_passes(&f);
    let replaced = format!("{B}// replaced\n");
    let h = f.fs.open(ino(&f, "b-new.go"), O_RDWR).unwrap().handle;
    assert_eq!(f.fs.write(h, 0, replaced.as_bytes()), Ok(replaced.len()));
    assert_eq!(f.fs.flush(h), Ok(()));
    assert_eq!(
        f.fs.rename(ROOT_INODE, b"b-new.go", pkg, b"b.go", 0),
        Ok(())
    );
    assert_eq!(passes(&f), 2, "the flush and the rename");
    assert!(!stored(&f, "b-new.go"));
    assert_eq!(read_source(&f, "pkg/b.go").0, replaced.as_bytes());
    assert_eq!(on_disk(&f, "pkg/b.go").as_deref(), Some(replaced.as_str()));
    assert_eq!(on_disk(&f, "b-new.go"), None);
}

#[test]
fn rename_answers_its_errnos_and_changes_nothing() {
    let f = fixture(Scope {
        see: Grant::Whole,
        propose: paths(&["a.go", "pkg/b.go", "pkg/x.go"]),
    });
    use rename_flags::{RENAME_EXCHANGE, RENAME_NOREPLACE};
    let pkg = ino(&f, "pkg");
    // A directory whose new place the scope may propose nothing under.
    assert_eq!(
        f.fs.rename(ROOT_INODE, b"pkg", ROOT_INODE, b"pkg2", 0),
        Err(Errno::EACCES)
    );
    // A directory into itself.
    assert_eq!(
        f.fs.rename(ROOT_INODE, b"pkg", pkg, b"inner", 0),
        Err(Errno::EINVAL)
    );
    assert_eq!(
        f.fs.rename(ROOT_INODE, b"a.go", ROOT_INODE, b"pkg", 0),
        Err(Errno::EISDIR)
    );
    assert_eq!(
        f.fs.rename(ROOT_INODE, b"nope.go", ROOT_INODE, b"x.go", 0),
        Err(Errno::ENOENT)
    );
    assert_eq!(
        f.fs.rename(ROOT_INODE, b"a.go", pkg, b"b.go", RENAME_NOREPLACE),
        Err(Errno::EEXIST)
    );
    assert_eq!(
        f.fs.rename(ROOT_INODE, b"a.go", pkg, b"b.go", RENAME_EXCHANGE),
        Err(Errno::EINVAL)
    );
    // Outside the proposal scope, at either end.
    assert_eq!(
        f.fs.rename(ROOT_INODE, b"a.go", ROOT_INODE, b"z.go", 0),
        Err(Errno::EACCES)
    );
    assert_eq!(
        f.fs.rename(ino(&f, "pkg/sub"), b"c.go", pkg, b"x.go", 0),
        Err(Errno::EACCES)
    );
    // The same name: nothing to do.
    assert_eq!(
        f.fs.rename(ROOT_INODE, b"a.go", ROOT_INODE, b"a.go", 0),
        Ok(())
    );
    assert_eq!(read_source(&f, "a.go").0, A.as_bytes());
    assert_eq!(read_source(&f, "pkg/b.go").0, B.as_bytes());
    assert!(stored(&f, "pkg/sub/c.go"));
}

/// MKDIR makes a directory in the store: visible, empty, and gone at
/// RMDIR. A directory holding files is `ENOTEMPTY`.
#[test]
fn mkdir_and_rmdir_manage_stored_directories() {
    let f = fixture_with(whole(), true, A);
    let d = f.fs.mkdir(ROOT_INODE, b"newdir").unwrap();
    assert_eq!(d.attr.kind, Kind::Directory);
    assert_eq!(d.attr.mode(), 0o040755);
    assert_eq!(ino(&f, "newdir"), d.attr.inode);
    assert!(names(&f, ROOT_INODE).contains(&"newdir".to_string()));
    assert_eq!(names(&f, d.attr.inode), Vec::<String>::new());
    assert_eq!(
        f.fs.mkdir(ROOT_INODE, b"newdir").map(|_| ()),
        Err(Errno::EEXIST)
    );
    assert_eq!(
        f.fs.mkdir(ROOT_INODE, b"a.go").map(|_| ()),
        Err(Errno::EEXIST)
    );
    let e = f.fs.mkdir(d.attr.inode, b"e").unwrap();
    assert_eq!(f.fs.rmdir(ROOT_INODE, b"newdir"), Err(Errno::ENOTEMPTY));
    assert_eq!(f.fs.rmdir(d.attr.inode, b"e"), Ok(()));
    assert_eq!(f.fs.lookup(d.attr.inode, b"e"), Err(Errno::ENOENT));
    assert_eq!(
        f.fs.getattr(e.attr.inode, None).map(|_| ()),
        Err(Errno::ENOENT)
    );

    // A file created in it lands under it, in the store and the checkout.
    let (_, flushed) = create_file(&f, d.attr.inode, "x.go", N);
    assert_eq!(flushed, Ok(()));
    assert_eq!(read_source(&f, "newdir/x.go").0, N.as_bytes());
    assert_eq!(on_disk(&f, "newdir/x.go").as_deref(), Some(N));
    assert_eq!(f.fs.rmdir(ROOT_INODE, b"newdir"), Err(Errno::ENOTEMPTY));

    assert_eq!(f.fs.rmdir(ROOT_INODE, b"pkg"), Err(Errno::ENOTEMPTY));
    assert_eq!(f.fs.rmdir(ROOT_INODE, b"a.go"), Err(Errno::ENOTDIR));
    assert_eq!(f.fs.rmdir(ROOT_INODE, b"nope"), Err(Errno::ENOENT));

    // Directories are stored (ADR-0045 T1): a new filesystem over the
    // same store sees the empty one too.
    let empty = f.fs.mkdir(ROOT_INODE, b"empty").unwrap();
    assert_eq!(empty.attr.kind, Kind::Directory);
    let again = StoreFs::new(f.fs.store().clone(), whole(), Options::default());
    assert!(again.lookup(ROOT_INODE, b"empty").is_ok());
    assert!(again.lookup(ROOT_INODE, b"newdir").is_ok());
}

/// A checkout's `--language` filter narrows its projection, not what it
/// holds (ADR-0045 T3): a file in another language lands in the store.
#[test]
fn a_create_outside_the_checkouts_language_lands_in_the_store() {
    let f = fixture_with(whole(), true, A);
    let filtered = StoreFs::new(
        f.fs.store().clone(),
        whole(),
        Options {
            freshness: freshness(),
            checkout: Some(Checkout {
                dir: f.src.path().to_path_buf(),
                lang_filter: Some("go".to_string()),
            }),
        },
    );
    let (_, h) = filtered.create(ROOT_INODE, b"tool.py", O_RDWR).unwrap();
    assert_eq!(filtered.write(h, 0, b"x = 1\n"), Ok(6));
    assert_eq!(filtered.flush(h), Ok(()));
    assert!(stored(&f, "tool.py"));
    assert_eq!(on_disk(&f, "tool.py").as_deref(), Some("x = 1\n"));
    assert!(f.fs.lookup(ROOT_INODE, b"tool.py").is_ok());
}

/// A file created read-only (`open(O_CREAT|O_RDONLY)`) exists and lands
/// empty at its flush; its handle takes no writes.
#[test]
fn a_read_only_create_lands_an_empty_file_and_refuses_writes() {
    let f = fixture(whole());
    let (_, h) = f.fs.create(ROOT_INODE, b"ro.go", O_RDONLY).unwrap();
    assert_eq!(f.fs.write(h, 0, N.as_bytes()), Err(Errno::EBADF));
    assert_eq!(f.fs.flush(h), Ok(()));
    assert_eq!(read_source(&f, "ro.go").0, b"");
}

/// A session directory renames within the session: onto a free name or an
/// empty directory; onto a file `ENOTDIR`, onto a directory holding
/// anything `ENOTEMPTY`, into itself `EINVAL`, outside the proposal scope
/// `EACCES`.
#[test]
fn an_empty_session_directory_renames_as_rename_2_does() {
    let f = fixture(Scope {
        see: Grant::Whole,
        propose: paths(&["a.go", "d/x.go", "e/x.go", "pkg/x.go"]),
    });
    let d = f.fs.mkdir(ROOT_INODE, b"d").unwrap().attr.inode;
    f.fs.mkdir(ROOT_INODE, b"e").unwrap();
    assert_eq!(
        f.fs.rename(ROOT_INODE, b"d", ROOT_INODE, b"a.go", 0),
        Err(Errno::ENOTDIR)
    );
    assert_eq!(
        f.fs.rename(ROOT_INODE, b"d", ROOT_INODE, b"pkg", 0),
        Err(Errno::ENOTEMPTY)
    );
    assert_eq!(
        f.fs.rename(ROOT_INODE, b"d", d, b"inner", 0),
        Err(Errno::EINVAL)
    );
    assert_eq!(
        f.fs.rename(ROOT_INODE, b"d", ROOT_INODE, b"elsewhere", 0),
        Err(Errno::EACCES)
    );
    assert_eq!(f.fs.rename(ROOT_INODE, b"d", ROOT_INODE, b"e", 0), Ok(()));
    assert_eq!(f.fs.lookup(ROOT_INODE, b"d"), Err(Errno::ENOENT));
    assert_eq!(ino(&f, "e"), d, "the inode moved with the name");
}

/// A file the store cannot hold moves within the session, and only inside
/// the proposal scope at both ends.
#[test]
fn a_session_file_renames_within_the_scope() {
    let f = fixture(Scope {
        see: Grant::Whole,
        propose: paths(&["a.go", "notes.txt", "kept.txt"]),
    });
    let (h, flushed) = create_file(&f, ROOT_INODE, "notes.txt", "hello\n");
    assert_eq!(flushed, Ok(()));
    f.fs.release(h);
    assert_eq!(
        f.fs.rename(ROOT_INODE, b"notes.txt", ROOT_INODE, b"other.txt", 0),
        Err(Errno::EACCES)
    );
    assert_eq!(
        f.fs.rename(ROOT_INODE, b"notes.txt", ROOT_INODE, b"kept.txt", 0),
        Ok(())
    );
    let r = f.fs.open(ino(&f, "kept.txt"), O_RDONLY).unwrap().handle;
    assert_eq!(read_all(&f, r), b"hello\n");
    assert_eq!(f.fs.lookup(ROOT_INODE, b"notes.txt"), Err(Errno::ENOENT));
}

#[test]
fn mkdir_outside_the_proposal_scope_is_eacces() {
    let f = fixture(Scope {
        see: Grant::Whole,
        propose: paths(&["a.go", "tools/x.go"]),
    });
    assert_eq!(
        f.fs.mkdir(ROOT_INODE, b"other").map(|_| ()),
        Err(Errno::EACCES)
    );
    assert!(f.fs.mkdir(ROOT_INODE, b"tools").is_ok());
    assert_eq!(
        f.fs.create(ROOT_INODE, b"other.go", O_RDWR).map(|_| ()),
        Err(Errno::EACCES)
    );
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
        f.fs.rename(ROOT_INODE, b"pkg", ROOT_INODE, b"x", 0),
        Err(Errno::ENOENT)
    );
    assert_eq!(
        f.fs.rename(ROOT_INODE, b"a.go", 999, b"x", 0),
        Err(Errno::ENOENT)
    );
}

// ── read freshness (ADR-0044 V3 "Read freshness") ─────────────────────────

const C_EDIT: &str = "package sub\n\nfunc C() {}\n\nfunc C2() {}\n";

/// The entry timeout LOOKUP answers for `path`.
fn ttl_of(f: &Fixture, path: &str) -> Duration {
    let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
    let parent = if parent.is_empty() {
        ROOT_INODE
    } else {
        ino(f, parent)
    };
    f.fs.lookup(parent, name.as_bytes()).unwrap().ttl
}

/// Another writer deletes `path`.
fn other_deleter(f: &Fixture, path: &str) {
    f.fs.store()
        .with_write(|conn| {
            source_write::propose(conn, &Grant::Whole, vec![Proposed::delete(path, Base::Any)])
        })
        .unwrap();
}

/// Another writer creates `path`.
fn other_creator(f: &Fixture, path: &str, text: &str) {
    f.fs.store()
        .with_write(|conn| {
            source_write::propose(
                conn,
                &Grant::Whole,
                vec![Proposed::create(path, text.as_bytes().to_vec())],
            )
        })
        .unwrap();
}

/// A read-only open and its release: what the guest's page cache is told.
fn read_open(f: &Fixture, inode: u64) -> Opened {
    let opened = f.fs.open(inode, O_RDONLY).unwrap();
    f.fs.release(opened.handle);
    opened
}

#[test]
fn readdirplus_answers_each_entry_with_its_attributes_and_timeout() {
    let f = fixture(whole());
    let pkg = ino(&f, "pkg");
    let plus = f.fs.readdirplus(pkg, 0).unwrap();
    let listed: Vec<DirEntry> = plus.iter().map(|(d, _)| d.clone()).collect();
    assert_eq!(listed, f.fs.readdir(pkg, 0).unwrap());
    assert_eq!(
        plus.iter().map(|(_, e)| e.clone()).collect::<Vec<_>>(),
        vec![
            f.fs.lookup(pkg, b"b.go").unwrap(),
            f.fs.lookup(pkg, b"sub").unwrap()
        ]
    );
    let (b, b_entry) = &plus[0];
    assert_eq!(b_entry.attr.inode, b.inode);
    assert_eq!(
        (b_entry.attr.kind, b_entry.attr.size, b_entry.attr.mode()),
        (Kind::File, B.len() as u64, 0o100644)
    );
    assert_eq!(b_entry.ttl, TTL);
    assert_eq!(plus[1].1.attr.mode(), 0o040755);
    assert_eq!(f.fs.readdirplus(pkg, 1).unwrap(), plus[1..].to_vec());
    assert_eq!(f.fs.readdirplus(pkg, 2).unwrap(), vec![]);
    assert_eq!(f.fs.readdirplus(ino(&f, "a.go"), 0), Err(Errno::ENOTDIR));
    assert_eq!(f.fs.readdirplus(999, 0), Err(Errno::ENOENT));

    // A file only the session holds carries its bytes' size.
    let (_, flushed) = create_file(&f, ROOT_INODE, "notes.txt", "hi\n");
    assert_eq!(flushed, Ok(()));
    let root = f.fs.readdirplus(ROOT_INODE, 0).unwrap();
    let notes = root.iter().find(|(d, _)| d.name == "notes.txt").unwrap();
    assert_eq!((notes.1.attr.kind, notes.1.attr.size), (Kind::File, 3));
}

#[test]
fn readdirplus_answers_the_modes_the_scope_allows() {
    let f = fixture(Scope {
        see: Grant::Whole,
        propose: paths(&["a.go"]),
    });
    let modes: Vec<(String, u32)> =
        f.fs.readdirplus(ROOT_INODE, 0)
            .unwrap()
            .into_iter()
            .map(|(d, e)| (d.name, e.attr.mode()))
            .collect();
    assert_eq!(
        modes,
        [
            ("a.go".to_string(), 0o100644),
            ("pkg".to_string(), 0o040555),
            ("secret".to_string(), 0o040555),
        ]
    );
    let b = f.fs.readdirplus(ino(&f, "pkg"), 0).unwrap();
    assert_eq!(b[0].1.attr.mode(), 0o100444);
}

/// The guest keeps a file's pages across opens only while the file holds
/// the blob the last open served: content addressing gives open-to-close
/// consistency with no invalidation channel.
#[test]
fn a_read_only_open_keeps_the_guests_pages_only_while_the_blob_is_unchanged() {
    let f = fixture(whole());
    let a = ino(&f, "a.go");
    let first = read_open(&f, a);
    assert_eq!((first.keep_cache, first.direct_io), (false, false));
    assert!(read_open(&f, a).keep_cache);
    assert!(read_open(&f, a).keep_cache);
    assert!(!read_open(&f, ino(&f, "pkg/b.go")).keep_cache, "per file");

    // Same size: the guest's cached size still describes the new blob.
    other_writer(&f, "a.go", A_OTHER);
    let after = read_open(&f, a);
    assert_eq!((after.keep_cache, after.direct_io), (false, false));
    assert!(read_open(&f, a).keep_cache);

    // A write open bypasses the page cache, and the guest drops the
    // file's pages at it.
    let w = f.fs.open(a, O_RDWR).unwrap();
    assert_eq!((w.keep_cache, w.direct_io), (false, true));
    assert!(!read_open(&f, a).keep_cache);
    assert!(read_open(&f, a).keep_cache);

    // The session's own accepted flush moves the blob as well.
    assert_eq!(f.fs.write(w.handle, 0, A_EDIT.as_bytes()), Ok(A_EDIT.len()));
    assert_eq!(f.fs.flush(w.handle), Ok(()));
    assert!(!read_open(&f, a).keep_cache);
    assert!(read_open(&f, a).keep_cache);

    // A file of any name is stored (ADR-0045 T1): the same rule holds.
    let (h, flushed) = create_file(&f, ROOT_INODE, "notes.txt", "hi\n");
    assert_eq!(flushed, Ok(()));
    f.fs.release(h);
    let notes = ino(&f, "notes.txt");
    assert!(!read_open(&f, notes).keep_cache);
    assert!(read_open(&f, notes).keep_cache);
}

/// The guest bounds a cached read by the size its cached attributes hold;
/// an OPEN reply carries no attributes. A read-only open serving a blob of
/// another size than the last attributes the guest was told is therefore
/// direct I/O, so its reads reach the whole current blob. Found by the
/// in-guest end-to-end test (bead `ley-line-open-f210c2`): after another
/// writer grew a file, the guest's next open read back the new blob cut to
/// the old size.
#[test]
fn an_open_serving_another_size_than_the_guest_was_told_is_direct_io() {
    let f = fixture(whole());
    let a = ino(&f, "a.go"); // LOOKUP: the guest is told A's size
    other_writer(&f, "a.go", A_TAILED);
    let grown = f.fs.open(a, O_RDONLY).unwrap();
    assert_eq!((grown.keep_cache, grown.direct_io), (false, true));
    assert_eq!(read_all(&f, grown.handle), A_TAILED.as_bytes());
    f.fs.release(grown.handle);
    // The open told the guest no size: the next one is direct I/O too.
    assert!(read_open(&f, a).direct_io);

    // A GETATTR tells it the new size; opens are cached again.
    let (attr, _) = f.fs.getattr(a, None).unwrap();
    assert_eq!(attr.size, A_TAILED.len() as u64);
    assert!(!read_open(&f, a).direct_io);

    // A LOOKUP, READDIRPLUS, or a SETATTR's reply tells it too.
    other_writer(&f, "a.go", A_SHORT);
    assert!(read_open(&f, a).direct_io);
    f.fs.lookup(ROOT_INODE, b"a.go").unwrap();
    assert!(!read_open(&f, a).direct_io);
    other_writer(&f, "a.go", A);
    assert!(read_open(&f, a).direct_io);
    f.fs.readdirplus(ROOT_INODE, 0).unwrap();
    assert!(!read_open(&f, a).direct_io);
    other_writer(&f, "a.go", A_TAILED);
    let w = f.fs.open(a, O_RDWR).unwrap();
    f.fs.setattr(
        a,
        Some(w.handle),
        SetAttr {
            size: Some(A_SHORT.len() as u64),
            ..SetAttr::default()
        },
    )
    .unwrap();
    f.fs.release(w.handle);
    assert!(
        read_open(&f, a).direct_io,
        "the SETATTR reply told the staged size, not the stored one"
    );
    f.fs.getattr(a, None).unwrap();
    assert!(!read_open(&f, a).direct_io);

    // A created file's entry tells its (empty) size.
    let (entry, h) = f.fs.create(ROOT_INODE, b"n.go", O_RDWR).unwrap();
    f.fs.release(h);
    assert!(!read_open(&f, entry.attr.inode).direct_io);
}

#[test]
fn opendir_keeps_the_guests_listing_only_while_it_is_unchanged() {
    let f = fixture(whole());
    let pkg = ino(&f, "pkg");
    assert_eq!(f.fs.opendir(pkg), Ok(OpenedDir { keep_cache: false }));
    assert_eq!(f.fs.opendir(pkg), Ok(OpenedDir { keep_cache: true }));
    other_writer(&f, "pkg/b.go", "package pkg\n");
    assert!(
        f.fs.opendir(pkg).unwrap().keep_cache,
        "same names, same kinds"
    );
    other_creator(&f, "pkg/new.go", "package pkg\n");
    assert!(!f.fs.opendir(pkg).unwrap().keep_cache);
    assert!(f.fs.opendir(pkg).unwrap().keep_cache);
    f.fs.mkdir(pkg, b"d").unwrap();
    assert!(!f.fs.opendir(pkg).unwrap().keep_cache);
    assert!(!f.fs.opendir(ROOT_INODE).unwrap().keep_cache);
    assert_eq!(f.fs.opendir(ino(&f, "a.go")), Err(Errno::ENOTDIR));
    assert_eq!(f.fs.opendir(999), Err(Errno::ENOENT));
}

/// "Recent": a change recorded at `t0` keeps the path's (and its parent
/// directory's) timeouts short while `now - t0 < window`.
#[test]
fn a_proposal_shortens_the_timeouts_of_its_path_and_parent_for_the_window() {
    let f = fixture(whole());
    let c = ino(&f, "pkg/sub/c.go");
    let sub = ino(&f, "pkg/sub");
    let timeouts = |f: &Fixture| {
        [
            ttl_of(f, "pkg/sub/c.go"),
            f.fs.getattr(c, None).unwrap().1,
            ttl_of(f, "pkg/sub"),
            f.fs.getattr(sub, None).unwrap().1,
            ttl_of(f, "pkg"),
            ttl_of(f, "a.go"),
        ]
    };
    assert_eq!(timeouts(&f), [TTL; 6]);

    f.clock.advance(Duration::from_secs(100));
    let h = f.fs.open(c, O_RDWR).unwrap().handle;
    assert_eq!(f.fs.write(h, 0, C_EDIT.as_bytes()), Ok(C_EDIT.len()));
    assert_eq!(timeouts(&f), [TTL; 6], "nothing changes until it lands");
    assert_eq!(f.fs.flush(h), Ok(()));
    assert_eq!(timeouts(&f), [SHORT, SHORT, SHORT, SHORT, TTL, TTL]);

    f.clock.advance(WINDOW - Duration::from_nanos(1));
    assert_eq!(timeouts(&f), [SHORT, SHORT, SHORT, SHORT, TTL, TTL]);
    f.clock.advance(Duration::from_nanos(1));
    assert_eq!(timeouts(&f), [TTL; 6]);

    // A delete shortens the negative entry of the name it removed.
    f.fs.unlink(sub, b"c.go").unwrap();
    assert_eq!(f.fs.negative_ttl(sub, b"c.go"), SHORT);
    assert_eq!(ttl_of(&f, "pkg/sub"), SHORT);
}

/// No proposal notifies storefs of another writer's change: it records the
/// change when it next serves the path in a state other than the one it
/// last served.
#[test]
fn a_foreign_change_shortens_the_timeouts_from_when_storefs_next_serves_the_path() {
    let f = fixture(whole());
    assert_eq!(ttl_of(&f, "a.go"), TTL);
    f.clock.advance(Duration::from_secs(100));
    other_writer(&f, "a.go", A_OTHER);
    assert_eq!(ttl_of(&f, "a.go"), SHORT);
    assert_eq!(f.fs.getattr(ROOT_INODE, None).unwrap().1, SHORT);
    assert_eq!(ttl_of(&f, "pkg"), TTL);
    f.clock.advance(WINDOW);
    assert_eq!(ttl_of(&f, "a.go"), TTL);
    assert_eq!(f.fs.getattr(ROOT_INODE, None).unwrap().1, TTL);

    // A foreign delete: the name's negative entry is short; a name never
    // served is not.
    let pkg = ino(&f, "pkg");
    assert_eq!(ttl_of(&f, "pkg/b.go"), TTL);
    other_deleter(&f, "pkg/b.go");
    assert_eq!(f.fs.lookup(pkg, b"b.go"), Err(Errno::ENOENT));
    assert_eq!(f.fs.negative_ttl(pkg, b"b.go"), SHORT);
    assert_eq!(f.fs.negative_ttl(pkg, b"never.go"), TTL);
    assert_eq!(f.fs.negative_ttl(999, b"x.go"), Duration::ZERO);
    assert_eq!(f.fs.negative_ttl(ROOT_INODE, b""), Duration::ZERO);

    // A foreign create after a negative lookup.
    assert_eq!(f.fs.lookup(ROOT_INODE, b"x.go"), Err(Errno::ENOENT));
    other_creator(&f, "x.go", "package a\n");
    assert_eq!(ttl_of(&f, "x.go"), SHORT);

    // GETATTR, OPEN and READDIRPLUS observe as LOOKUP does.
    f.clock.advance(WINDOW);
    let c = ino(&f, "pkg/sub/c.go");
    assert_eq!(f.fs.getattr(c, None).unwrap().1, TTL);
    other_writer(&f, "pkg/sub/c.go", C_EDIT);
    assert_eq!(f.fs.getattr(c, None).unwrap().1, SHORT);
    f.clock.advance(WINDOW);
    let plus = f.fs.readdirplus(ino(&f, "pkg/sub"), 0).unwrap();
    assert_eq!(plus[0].1.ttl, TTL);
    other_writer(&f, "pkg/sub/c.go", C);
    let plus = f.fs.readdirplus(ino(&f, "pkg/sub"), 0).unwrap();
    assert_eq!(plus[0].1.ttl, SHORT);
    f.clock.advance(WINDOW);
    assert_eq!(ttl_of(&f, "a.go"), TTL);
    other_writer(&f, "a.go", A);
    f.fs.release(f.fs.open(ino(&f, "a.go"), O_RDONLY).unwrap().handle);
    assert_eq!(ttl_of(&f, "a.go"), SHORT);
}

#[test]
fn the_default_freshness_is_long_for_settled_paths_and_one_second_for_changed_ones() {
    assert_eq!(
        Freshness::default(),
        Freshness {
            settled: Duration::from_secs(60),
            changed: Duration::from_secs(1),
            window: Duration::from_secs(60),
        }
    );
}

#[test]
fn the_monotonic_clock_counts_from_its_origin() {
    let clock = MonotonicClock {
        origin: std::time::Instant::now() - Duration::from_secs(5),
        wall: ORIGIN,
    };
    let first = clock.now();
    assert!(first >= Duration::from_secs(5));
    assert!(clock.now() >= first);
    assert_eq!(clock.origin(), ORIGIN);
    assert!(MonotonicClock::default().now() < Duration::from_secs(5));
}

#[test]
fn the_monotonic_clocks_origin_is_the_wall_clock_at_its_creation() {
    let wall = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
    };
    let before = wall();
    let origin = MonotonicClock::default().origin();
    let after = wall();
    assert!(
        before <= origin && origin <= after,
        "{before:?} {origin:?} {after:?}"
    );
}

// ── file times (bead `ley-line-open-b8100d`) ──────────────────────────────
//
// git decides whether to rehash a file from its stat (size, mtime, ctime,
// inode). A path's time is the clock's origin until this filesystem records
// a change to it, then the time of the latest change, strictly increasing
// per path.

/// The modification time GETATTR answers for `path` ("" is the root).
fn mtime(f: &Fixture, path: &str) -> Duration {
    let inode = if path.is_empty() {
        ROOT_INODE
    } else {
        ino(f, path)
    };
    f.fs.getattr(inode, None).unwrap().0.mtime
}

/// Advance the clock one second; answers the time a change recorded now
/// reports.
fn tick(f: &Fixture) -> Duration {
    f.clock.advance(Duration::from_secs(1));
    ORIGIN + f.clock.now()
}

#[test]
fn a_path_no_change_was_recorded_for_reports_the_clocks_origin() {
    let f = fixture(whole());
    f.clock.advance(Duration::from_secs(40));
    for path in ["", "a.go", "pkg", "pkg/b.go", "pkg/sub", "pkg/sub/c.go"] {
        assert_eq!(mtime(&f, path), ORIGIN, "{path:?}");
    }
    assert_eq!(f.fs.lookup(ROOT_INODE, b"a.go").unwrap().attr.mtime, ORIGIN);
    let plus = f.fs.readdirplus(ino(&f, "pkg"), 0).unwrap();
    assert!(plus.iter().all(|(_, entry)| entry.attr.mtime == ORIGIN));
}

#[test]
fn a_same_size_edit_moves_the_files_time_and_an_unchanged_file_keeps_its_own() {
    let f = fixture(whole());
    assert_eq!(A.len(), A_EDIT.len());
    assert_eq!(A.len(), A_OTHER.len());
    let a = ino(&f, "a.go");
    assert_eq!(mtime(&f, "a.go"), ORIGIN);

    let at = tick(&f);
    let h = edit(&f, "a.go", A_EDIT);
    assert_eq!(mtime(&f, "a.go"), ORIGIN, "nothing changes until it lands");
    assert_eq!(f.fs.flush(h), Ok(()));
    assert_eq!(mtime(&f, "a.go"), at);
    assert_eq!(f.fs.getattr(a, Some(h)).unwrap().0.mtime, at);
    assert_eq!(f.fs.lookup(ROOT_INODE, b"a.go").unwrap().attr.mtime, at);
    f.fs.release(h);

    // A second same-size edit at the same clock reading still moves it.
    let h = edit(&f, "a.go", A_OTHER);
    assert_eq!(f.fs.flush(h), Ok(()));
    let second = mtime(&f, "a.go");
    assert_eq!(second, at + Duration::from_nanos(1));
    f.fs.release(h);

    // Unchanged since: stable, however long the clock runs.
    f.clock.advance(Duration::from_secs(600));
    assert_eq!(mtime(&f, "a.go"), second);
    for path in ["pkg", "pkg/b.go", "pkg/sub/c.go"] {
        assert_eq!(mtime(&f, path), ORIGIN, "{path:?} did not change");
    }
}

#[test]
fn a_foreign_proposal_moves_the_time_from_when_storefs_next_serves_the_path() {
    let f = fixture(whole());
    assert_eq!(mtime(&f, "a.go"), ORIGIN);
    tick(&f);
    other_writer(&f, "a.go", A_OTHER);
    let served = tick(&f);
    assert_eq!(f.fs.lookup(ROOT_INODE, b"a.go").unwrap().attr.mtime, served);
    tick(&f);
    assert_eq!(mtime(&f, "a.go"), served);
    assert_eq!(mtime(&f, ""), served, "the directory it is in");

    // The same bytes again: the same blob, no change.
    other_writer(&f, "a.go", A_OTHER);
    tick(&f);
    assert_eq!(mtime(&f, "a.go"), served);

    // READDIRPLUS observes as LOOKUP does. (A path never served has told
    // the guest no time to compare against.)
    assert_eq!(mtime(&f, "pkg/b.go"), ORIGIN);
    other_writer(&f, "pkg/b.go", "package pkg\n");
    let listed = tick(&f);
    let plus = f.fs.readdirplus(ino(&f, "pkg"), 0).unwrap();
    assert_eq!(plus[0].0.name, "b.go");
    assert_eq!(plus[0].1.attr.mtime, listed);
}

#[test]
fn a_directorys_time_moves_when_an_entry_is_created_removed_or_renamed_in_it() {
    let f = fixture(whole());
    let pkg = ino(&f, "pkg");

    let at = tick(&f);
    let (_, h) = f.fs.create(pkg, b"n.go", O_RDWR).unwrap();
    f.fs.release(h);
    assert_eq!(mtime(&f, "pkg"), at, "CREATE");
    assert_eq!(mtime(&f, "pkg/n.go"), at);
    assert_eq!(mtime(&f, ""), ORIGIN, "only the directory the entry is in");

    let at = tick(&f);
    f.fs.unlink(pkg, b"b.go").unwrap();
    assert_eq!(mtime(&f, "pkg"), at, "UNLINK of a stored file");

    let at = tick(&f);
    f.fs.mkdir(pkg, b"d").unwrap();
    assert_eq!(mtime(&f, "pkg"), at, "MKDIR");
    assert_eq!(mtime(&f, "pkg/d"), at);

    let at = tick(&f);
    f.fs.rmdir(pkg, b"d").unwrap();
    assert_eq!(mtime(&f, "pkg"), at, "RMDIR");

    let at = tick(&f);
    f.fs.rename(pkg, b"n.go", ROOT_INODE, b"n.txt", 0).unwrap();
    assert_eq!((mtime(&f, "pkg"), mtime(&f, "")), (at, at), "RENAME");
    assert_eq!(mtime(&f, "n.txt"), at);

    let at = tick(&f);
    f.fs.unlink(ROOT_INODE, b"n.txt").unwrap();
    assert_eq!(mtime(&f, ""), at, "UNLINK of a file only the session holds");

    // A file only the session holds, saved onto a name the store holds in
    // another directory: the directory it left changes too.
    let (h, flushed) = create_file(&f, pkg, "a.go.tmp", A_EDIT);
    assert_eq!(flushed, Ok(()));
    f.fs.release(h);
    let at = tick(&f);
    f.fs.rename(pkg, b"a.go.tmp", ROOT_INODE, b"a.go", 0)
        .unwrap();
    assert_eq!((mtime(&f, "pkg"), mtime(&f, "a.go")), (at, at));

    // A session directory renamed: both directories' entries change.
    f.fs.mkdir(pkg, b"e").unwrap();
    let at = tick(&f);
    f.fs.rename(pkg, b"e", ROOT_INODE, b"e", 0).unwrap();
    assert_eq!((mtime(&f, "pkg"), mtime(&f, "")), (at, at));
}

#[test]
fn a_foreign_create_moves_its_directorys_time_when_storefs_next_lists_it() {
    let f = fixture(whole());
    let sub = ino(&f, "pkg/sub");
    f.fs.opendir(sub).unwrap();
    tick(&f);
    f.fs.opendir(sub).unwrap();
    assert_eq!(mtime(&f, "pkg/sub"), ORIGIN, "the same listing");
    other_creator(&f, "pkg/sub/x.go", "package sub\n");
    let listed = tick(&f);
    f.fs.opendir(sub).unwrap();
    assert_eq!(mtime(&f, "pkg/sub"), listed);
    tick(&f);
    f.fs.opendir(sub).unwrap();
    assert_eq!(mtime(&f, "pkg/sub"), listed);
}

#[test]
fn a_file_only_the_session_holds_moves_its_time_at_each_flush() {
    let f = fixture(whole());
    let (h, flushed) = create_file(&f, ROOT_INODE, "notes.txt", "hi\n");
    assert_eq!(flushed, Ok(()));
    f.fs.release(h);
    let at = tick(&f);
    let h = edit(&f, "notes.txt", "ho\n");
    assert_eq!(f.fs.flush(h), Ok(()));
    f.fs.release(h);
    assert_eq!(mtime(&f, "notes.txt"), at);
    tick(&f);
    assert_eq!(mtime(&f, "notes.txt"), at);
}
