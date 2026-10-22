//! ADR-0045 phase 4 through the filesystem core: every file goes to the
//! store; MKDIR, RMDIR, a directory RENAME, a mode and a symlink are
//! proposals; WRITE stages whatever the store holds and FLUSH decides — a
//! clean merge lands, overlapping edits are `ESTALE`.

use std::sync::Arc;

use leyline_cli_lib::source_write::{self, Base, Grant, Proposed, Refusal, Stale};
use leyline_cli_lib::tree::{self as store_tree, Entry as StoreEntry, Mode};
use leyline_core::{ContentAddressed, Hash};

use super::open_flags::{O_RDONLY, O_RDWR, O_TRUNC};
use super::tests::{A, A_BROKEN, A_OTHER, Fixture, fixture, fixture_with, ino, on_disk, whole};
use super::*;

/// A tail line under `A`'s function: an edit to other lines than `A_OTHER`'s.
const A_TAILED: &str = "package a\n\nfunc A() int { return 1 }\n// tail\n";
/// `A_OTHER`'s change and `A_TAILED`'s, merged.
const A_MERGED: &str = "package a\n\nfunc A() int { return 3 }\n// tail\n";

fn entry(f: &Fixture, path: &str) -> Option<StoreEntry> {
    f.fs.store()
        .with_read(|conn| store_tree::entry(conn, path))
        .unwrap()
}

fn stored_text(f: &Fixture, path: &str) -> Option<String> {
    let Some(StoreEntry::File { blob, .. }) = entry(f, path) else {
        return None;
    };
    let bytes =
        f.fs.store()
            .with_read(|conn| store_tree::blob_bytes(conn, blob))
            .unwrap()
            .unwrap();
    Some(String::from_utf8(bytes).unwrap())
}

fn other_writer(f: &Fixture, path: &str, text: &str) {
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

/// A second filesystem over the same store: what the guest sees after the
/// session that made a change is gone.
fn next_session(f: &Fixture) -> StoreFs<Arc<leyline_cli_lib::daemon::db_pool::LiveDb>> {
    StoreFs::new(f.fs.store().clone(), whole(), Options::default())
}

fn lookup_in(fs: &StoreFs<impl Store>, path: &str) -> Result<Entry, Errno> {
    let mut parent = ROOT_INODE;
    let mut found = Err(Errno::ENOENT);
    for name in path.split('/') {
        let e = fs.lookup(parent, name.as_bytes())?;
        parent = e.attr.inode;
        found = Ok(e);
    }
    found
}

/// T6: a WRITE against a moved file stages; the FLUSH merges three ways and
/// lands the merge; the handle and the next open serve the merged bytes,
/// and the next open does not keep the guest's pages.
#[test]
fn a_write_against_a_moved_file_stages_and_the_flush_lands_the_merge() {
    let f = fixture_with(whole(), true, A);
    let a = ino(&f, "a.go");
    let h = f.fs.open(a, O_RDWR | O_TRUNC).unwrap().handle;
    other_writer(&f, "a.go", A_OTHER);
    assert_eq!(f.fs.write(h, 0, A_TAILED.as_bytes()), Ok(A_TAILED.len()));
    assert_eq!(f.fs.flush(h), Ok(()));
    assert_eq!(stored_text(&f, "a.go").as_deref(), Some(A_MERGED));
    assert_eq!(on_disk(&f, "a.go").as_deref(), Some(A_MERGED));
    assert_eq!(f.fs.read(h, 0, u32::MAX).unwrap(), A_MERGED.as_bytes());
    // The handle's base is the merge: its next edit lands without a merge.
    assert_eq!(f.fs.write(h, 0, A_MERGED.as_bytes()), Ok(A_MERGED.len()));
    assert_eq!(f.fs.flush(h), Ok(()));
    let opened = f.fs.open(a, O_RDONLY).unwrap();
    assert!(!opened.keep_cache, "the guest's pages predate the merge");
    assert_eq!(
        f.fs.read(opened.handle, 0, u32::MAX).unwrap(),
        A_MERGED.as_bytes()
    );
}

/// T5 through the mount: overlapping edits refuse at FLUSH and FSYNC with
/// `ESTALE`; WRITE itself never does.
#[test]
fn overlapping_edits_refuse_at_flush_not_at_write() {
    let f = fixture(whole());
    let h = f.fs.open(ino(&f, "a.go"), O_RDWR).unwrap().handle;
    other_writer(&f, "a.go", A_OTHER);
    let edit = A.replace("return 1", "return 2");
    assert_eq!(f.fs.write(h, 0, edit.as_bytes()), Ok(edit.len()));
    assert_eq!(f.fs.fsync(h), Err(Errno::ESTALE));
    assert_eq!(f.fs.flush(h), Err(Errno::ESTALE));
    assert_eq!(stored_text(&f, "a.go").as_deref(), Some(A_OTHER));
}

/// T3: a half-written Go file lands, with a diagnostic.
#[test]
fn a_half_written_file_lands_with_a_diagnostic() {
    let f = fixture(whole());
    let h = f.fs.open(ino(&f, "a.go"), O_RDWR | O_TRUNC).unwrap().handle;
    assert_eq!(f.fs.write(h, 0, A_BROKEN.as_bytes()), Ok(A_BROKEN.len()));
    assert_eq!(f.fs.flush(h), Ok(()));
    assert_eq!(stored_text(&f, "a.go").as_deref(), Some(A_BROKEN));
    let d =
        f.fs.store()
            .with_read(|conn| store_tree::diagnostic(conn, "a.go"))
            .unwrap()
            .unwrap();
    assert!(d.projected, "{d:?}");
}

/// T1: a file in no projected language is stored, listed, and outlives the
/// session that made it.
#[test]
fn any_file_lands_in_the_store_and_outlives_the_session() {
    let f = fixture_with(whole(), true, A);
    for (name, text) in [("README.md", "# readme\n"), ("notes", "remember\n")] {
        let (_, h) = f.fs.create(ROOT_INODE, name.as_bytes(), O_RDWR).unwrap();
        assert_eq!(f.fs.write(h, 0, text.as_bytes()), Ok(text.len()));
        assert_eq!(f.fs.flush(h), Ok(()));
        assert_eq!(stored_text(&f, name).as_deref(), Some(text));
        assert_eq!(on_disk(&f, name).as_deref(), Some(text));
    }
    let next = next_session(&f);
    let names: Vec<String> = next
        .readdir(ROOT_INODE, 0)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert!(names.contains(&"README.md".to_string()) && names.contains(&"notes".to_string()));
}

/// T4: `chmod` is a proposal of the mode; the attributes show it.
#[test]
fn a_mode_change_is_a_proposal_of_the_mode() {
    let f = fixture_with(whole(), true, A);
    let a = ino(&f, "a.go");
    let set = |mode| SetAttr {
        mode: Some(mode),
        ..SetAttr::default()
    };
    let (attr, _) = f.fs.setattr(a, None, set(0o100755)).unwrap();
    assert_eq!(attr.perm, 0o755);
    assert_eq!(
        entry(&f, "a.go"),
        Some(StoreEntry::File {
            mode: Mode::Executable,
            blob: A.as_bytes().hash()
        })
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let bits = std::fs::metadata(f.src.path().join("a.go"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(bits & 0o111, 0o111);
    }
    let (attr, _) = f.fs.setattr(a, None, set(0o644)).unwrap();
    assert_eq!(attr.perm, 0o644);
    assert_eq!(
        entry(&f, "a.go"),
        Some(StoreEntry::File {
            mode: Mode::Regular,
            blob: A.as_bytes().hash()
        })
    );
    // A directory has no git mode: its mode change is accepted, unchanged.
    assert!(f.fs.setattr(ino(&f, "pkg"), None, set(0o700)).is_ok());
    // Owners are not carried.
    let owner = SetAttr {
        uid: Some(0),
        ..SetAttr::default()
    };
    assert_eq!(f.fs.setattr(a, None, owner), Err(Errno::EPERM));
}

/// T4: a symlink is a proposal whose blob is its target; READLINK answers it.
#[test]
fn a_symlink_is_a_proposal_and_readlink_answers_its_target() {
    let f = fixture_with(whole(), true, A);
    let e = f.fs.symlink(ROOT_INODE, b"link", b"a.go").unwrap();
    assert_eq!(e.attr.kind, Kind::Symlink);
    assert_eq!(e.attr.mode() & 0o170000, S_IFLNK);
    assert_eq!(e.attr.size, 4);
    assert_eq!(f.fs.readlink(e.attr.inode).unwrap(), b"a.go");
    assert_eq!(
        entry(&f, "link"),
        Some(StoreEntry::File {
            mode: Mode::Symlink,
            blob: b"a.go".hash()
        })
    );
    #[cfg(unix)]
    assert_eq!(
        std::fs::read_link(f.src.path().join("link")).unwrap(),
        std::path::Path::new("a.go")
    );
    assert_eq!(f.fs.symlink(ROOT_INODE, b"link", b"x"), Err(Errno::EEXIST));
    assert_eq!(f.fs.readlink(ino(&f, "a.go")), Err(Errno::EINVAL));
    let listed = f.fs.readdir(ROOT_INODE, 0).unwrap();
    let link = listed.iter().find(|d| d.name == "link").unwrap();
    assert_eq!(link.kind, Kind::Symlink);
}

/// T4: MKDIR and RMDIR are proposals; an empty directory outlives the
/// session.
#[test]
fn mkdir_and_rmdir_are_proposals() {
    let f = fixture_with(whole(), true, A);
    let made = f.fs.mkdir(ROOT_INODE, b"empty").unwrap();
    assert_eq!(made.attr.kind, Kind::Directory);
    let listed = f.fs.readdir(ROOT_INODE, 0).unwrap();
    assert_eq!(
        listed.iter().find(|d| d.name == "empty").map(|d| d.kind),
        Some(Kind::Directory),
        "an empty directory lists as a directory"
    );
    assert_eq!(entry(&f, "empty"), Some(StoreEntry::Dir));
    assert!(f.src.path().join("empty").is_dir());
    assert_eq!(
        lookup_in(&next_session(&f), "empty").unwrap().attr.kind,
        Kind::Directory
    );
    assert_eq!(f.fs.mkdir(ROOT_INODE, b"empty"), Err(Errno::EEXIST));
    assert_eq!(f.fs.rmdir(ROOT_INODE, b"pkg"), Err(Errno::ENOTEMPTY));
    assert_eq!(f.fs.rmdir(ROOT_INODE, b"a.go"), Err(Errno::ENOTDIR));
    assert_eq!(f.fs.rmdir(ROOT_INODE, b"empty"), Ok(()));
    assert_eq!(entry(&f, "empty"), None);
    assert!(!f.src.path().join("empty").exists());
    // The last file leaves; its directory stays, in the store too.
    let sub = ino(&f, "pkg/sub");
    assert_eq!(f.fs.unlink(sub, b"c.go"), Ok(()));
    assert_eq!(entry(&f, "pkg/sub"), Some(StoreEntry::Dir));
    assert_eq!(
        lookup_in(&next_session(&f), "pkg/sub").unwrap().attr.kind,
        Kind::Directory
    );
}

/// T4: a directory RENAME is one proposal over every path under it.
#[test]
fn a_directory_rename_is_one_proposal() {
    let f = fixture_with(whole(), true, A);
    let pkg = ino(&f, "pkg");
    let b = ino(&f, "pkg/b.go");
    assert_eq!(
        f.fs.rename(ROOT_INODE, b"pkg", ROOT_INODE, b"lib", 0),
        Ok(())
    );
    let paths: Vec<String> =
        f.fs.store()
            .with_read(|conn| Ok(store_tree::entries(conn)?.into_keys().collect()))
            .unwrap();
    assert_eq!(
        paths,
        [
            "a.go",
            "lib",
            "lib/b.go",
            "lib/sub",
            "lib/sub/c.go",
            "secret",
            "secret/d.go"
        ]
    );
    assert_eq!(ino(&f, "lib"), pkg, "the directory's inode follows it");
    assert_eq!(ino(&f, "lib/b.go"), b, "and its files' inodes");
    assert!(f.src.path().join("lib/sub/c.go").is_file());
    assert!(!f.src.path().join("pkg").exists());
    // Onto a non-empty directory: ENOTEMPTY, nothing moves.
    assert_eq!(
        f.fs.rename(ROOT_INODE, b"lib", ROOT_INODE, b"secret", 0),
        Err(Errno::ENOTEMPTY)
    );
}

// ── the host NFS mount's handle (ADR-0045 T8) ───────────────────────────

/// A checked WRITE is decided as its FLUSH would be, against what the store
/// holds now: an edit that overlaps a change landed since the handle's
/// base is `ESTALE` at the WRITE and leaves nothing staged.
#[test]
fn a_checked_write_that_overlaps_a_landed_change_is_refused_and_not_staged() {
    let f = fixture(whole());
    let h = f.fs.open(ino(&f, "a.go"), O_RDWR).unwrap().handle;
    other_writer(&f, "a.go", A_OTHER);
    let edit = A.replace("return 1", "return 2");
    assert_eq!(
        f.fs.write_checked(h, 0, edit.as_bytes()),
        Err(Errno::ESTALE)
    );
    assert_eq!(f.fs.read(h, 0, u32::MAX).unwrap(), A.as_bytes());
    assert_eq!(f.fs.flush(h), Ok(()), "nothing is staged to propose");
    assert_eq!(stored_text(&f, "a.go").as_deref(), Some(A_OTHER));
}

/// A checked WRITE that merges with the landed change is staged; its FLUSH
/// lands the merge.
#[test]
fn a_checked_write_that_merges_is_staged_and_its_flush_lands_the_merge() {
    let f = fixture(whole());
    let h = f.fs.open(ino(&f, "a.go"), O_RDWR | O_TRUNC).unwrap().handle;
    other_writer(&f, "a.go", A_OTHER);
    assert_eq!(
        f.fs.write_checked(h, 0, A_TAILED.as_bytes()),
        Ok(A_TAILED.len())
    );
    assert_eq!(f.fs.read(h, 0, u32::MAX).unwrap(), A_TAILED.as_bytes());
    assert_eq!(f.fs.flush(h), Ok(()));
    assert_eq!(stored_text(&f, "a.go").as_deref(), Some(A_MERGED));
}

/// A checked WRITE that changes nothing the handle holds stages nothing, so
/// its FLUSH proposes nothing.
#[test]
fn a_checked_write_of_the_bytes_held_stages_nothing() {
    let f = fixture(whole());
    super::tests::count_passes(&f);
    let h = f.fs.open(ino(&f, "a.go"), O_RDWR).unwrap().handle;
    assert_eq!(f.fs.write_checked(h, 0, &A.as_bytes()[..4]), Ok(4));
    assert_eq!(f.fs.flush(h), Ok(()));
    assert_eq!(
        super::tests::passes(&f),
        0,
        "an unchanged file lands nothing"
    );
}

/// A checked WRITE on a read-only handle is `EBADF`.
#[test]
fn a_checked_write_needs_a_write_handle() {
    let f = fixture(whole());
    let h = f.fs.open(ino(&f, "a.go"), O_RDONLY).unwrap().handle;
    assert_eq!(f.fs.write_checked(h, 0, b"x"), Err(Errno::EBADF));
}

/// REREAD brings a handle with nothing staged to the blob the store holds
/// now, its base with it; a handle with a staged edit keeps the bytes and
/// the base the edit was made against.
#[test]
fn reread_brings_a_clean_handle_to_the_store_and_leaves_a_dirty_one() {
    let f = fixture(whole());
    let h = f.fs.open(ino(&f, "a.go"), O_RDWR).unwrap().handle;
    other_writer(&f, "a.go", A_OTHER);
    assert_eq!(f.fs.reread(h), Ok(()));
    assert_eq!(f.fs.read(h, 0, u32::MAX).unwrap(), A_OTHER.as_bytes());
    // The base moved with the bytes: an edit of them lands unmerged.
    let edit = A_OTHER.replace("return 3", "return 4");
    assert_eq!(f.fs.write_checked(h, 0, edit.as_bytes()), Ok(edit.len()));
    other_writer(&f, "a.go", A);
    assert_eq!(f.fs.reread(h), Ok(()));
    assert_eq!(f.fs.read(h, 0, u32::MAX).unwrap(), edit.as_bytes());
    assert_eq!(f.fs.flush(h), Err(Errno::ESTALE), "against A_OTHER, not A");
}

/// REREAD of a file the store no longer holds is `ENOENT`.
#[test]
fn reread_of_a_deleted_file_is_enoent() {
    let f = fixture(whole());
    let h = f.fs.open(ino(&f, "a.go"), O_RDWR).unwrap().handle;
    f.fs.store()
        .with_write(|conn| {
            source_write::propose(
                conn,
                &Grant::Whole,
                vec![Proposed::delete("a.go", Base::Any)],
            )
        })
        .unwrap();
    assert_eq!(f.fs.reread(h), Err(Errno::ENOENT));
}

/// PARENT names the directory an inode is in; the root is its own parent.
#[test]
fn parent_names_the_directory_above_an_inode() {
    let f = fixture(whole());
    assert_eq!(f.fs.parent(ino(&f, "pkg/sub/c.go")), Ok(ino(&f, "pkg/sub")));
    assert_eq!(f.fs.parent(ino(&f, "pkg")), Ok(ROOT_INODE));
    assert_eq!(f.fs.parent(ROOT_INODE), Ok(ROOT_INODE));
    assert_eq!(f.fs.parent(9_999), Err(Errno::ENOENT));
}

#[test]
fn each_new_refusal_answers_its_linux_errno() {
    let refused = |r: Refusal| errno_for(&anyhow::Error::from(r));
    let h = Hash::from_bytes([1; 32]);
    let path = || "p".to_string();
    assert_eq!(refused(Refusal::Exists { path: path() }), Errno::EEXIST);
    assert_eq!(
        refused(Refusal::NotEmpty { path: path() }),
        Errno::ENOTEMPTY
    );
    assert_eq!(
        refused(Refusal::NotADirectory { path: path() }),
        Errno::ENOTDIR
    );
    assert_eq!(
        refused(Refusal::IsADirectory { path: path() }),
        Errno::EISDIR
    );
    assert_eq!(
        refused(Refusal::PathConflict {
            path: path(),
            other: path()
        }),
        Errno::ENOTDIR
    );
    assert_eq!(
        refused(Refusal::Conflict {
            moved: vec![Stale {
                path: path(),
                expected: Some(h),
                current: Some(h),
            }],
            conflicts: Default::default(),
        }),
        Errno::ESTALE
    );
}
