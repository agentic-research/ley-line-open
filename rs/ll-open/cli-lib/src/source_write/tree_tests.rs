//! ADR-0045 T4, T5 through `propose_with(.., Rules::Tree)`: any bytes at
//! any plain path, modes, symlinks and directories are proposals; a moved
//! base merges three ways and overlapping hunks refuse with their ranges.
//! Every accepted proposal lands the `treeRoot` a cold parse of the
//! resulting tree lands.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use leyline_core::{ContentAddressed, Hash};
use rusqlite::Connection;
use tempfile::TempDir;

use super::*;
use crate::merge::LineRange;
use crate::tree::{Entry, Mode};

const A: &str = "package a\n\nfunc A() int { return 1 }\n\nfunc B() int { return 2 }\n\nfunc C() int { return 3 }\n";

/// A tree on disk: files (with a mode), symlinks and empty directories.
#[derive(Clone)]
enum Node {
    File(&'static str, Mode),
    Link(&'static str),
    Dir,
}

fn lay_out(root: &Path, tree: &[(&str, Node)]) {
    for (rel, node) in tree {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        match node {
            Node::File(text, mode) => {
                std::fs::write(&p, text).unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let bits = if *mode == Mode::Executable { 0o755 } else { 0o644 };
                    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(bits)).unwrap();
                }
            }
            Node::Link(target) => {
                #[cfg(unix)]
                std::os::unix::fs::symlink(target, &p).unwrap();
            }
            Node::Dir => std::fs::create_dir_all(&p).unwrap(),
        }
    }
}

struct Store {
    src: TempDir,
    _dir: TempDir,
    conn: Connection,
}

fn store(tree: &[(&str, Node)]) -> Store {
    let src = TempDir::new().unwrap();
    lay_out(src.path(), tree);
    let dir = TempDir::new().unwrap();
    let conn = Connection::open(dir.path().join("live.db")).unwrap();
    crate::cmd_parse::parse_into_conn(&conn, src.path(), None, None).unwrap();
    Store {
        src,
        _dir: dir,
        conn,
    }
}

/// The `treeRoot` a cold parse of `tree` lands.
fn cold_root(tree: &[(&str, Node)]) -> Hash {
    let src = TempDir::new().unwrap();
    lay_out(src.path(), tree);
    let conn = Connection::open_in_memory().unwrap();
    crate::cmd_parse::parse_into_conn(&conn, src.path(), None, None)
        .unwrap()
        .tree_root
}

fn tree_propose(s: &Store, writes: Vec<Proposed>) -> Result<Accepted> {
    propose_with(&s.conn, &Grant::Whole, writes, Rules::Tree)
}

fn refused(err: anyhow::Error) -> Refusal {
    err.downcast::<Refusal>().expect("a typed refusal")
}

fn h(text: &str) -> Hash {
    text.as_bytes().hash()
}

fn stored_text(s: &Store, path: &str) -> Option<String> {
    let Some(Entry::File { blob, .. }) = crate::tree::entry(&s.conn, path).unwrap() else {
        return None;
    };
    Some(String::from_utf8(crate::tree::blob_bytes(&s.conn, blob).unwrap().unwrap()).unwrap())
}

fn projected(s: &Store, path: &str) -> bool {
    s.conn
        .query_row("SELECT EXISTS(SELECT 1 FROM _source WHERE id = ?1)", [path], |r| r.get(0))
        .unwrap()
}

fn image(s: &Store) -> Vec<u8> {
    s.conn.serialize("main").unwrap().to_vec()
}

/// T4: a file in no projected language is a create like any other: it is
/// stored, not projected, and the tree root is the cold parse's.
#[test]
fn a_create_of_any_bytes_lands_in_the_store() {
    let s = store(&[("a.go", Node::File(A, Mode::Regular))]);
    let notes = "remember the milk\n";
    let accepted = tree_propose(&s, vec![Proposed::create("docs/notes.txt", notes.into())]).unwrap();
    assert_eq!(accepted.blobs["docs/notes.txt"], h(notes));
    assert_eq!(stored_text(&s, "docs/notes.txt").as_deref(), Some(notes));
    assert_eq!(crate::tree::entry(&s.conn, "docs").unwrap(), Some(Entry::Dir));
    assert!(!projected(&s, "docs/notes.txt"));
    assert_eq!(
        accepted.tree_root,
        cold_root(&[
            ("a.go", Node::File(A, Mode::Regular)),
            ("docs/notes.txt", Node::File(notes, Mode::Regular)),
        ])
    );
}

/// T3: a write that does not parse lands, projected with its error nodes,
/// with a diagnostic; the strict rules still refuse it.
#[test]
fn an_unparseable_write_lands_with_a_diagnostic() {
    let s = store(&[("a.go", Node::File(A, Mode::Regular))]);
    let half = "package a\n\nfunc A() int { return\n";
    let strict = propose(&s.conn, &Grant::Whole, vec![Proposed::write("a.go", Base::Blob(h(A)), half.into())]);
    assert!(matches!(refused(strict.unwrap_err()), Refusal::Syntax { .. }));

    let accepted = tree_propose(&s, vec![Proposed::write("a.go", Base::Blob(h(A)), half.into())]).unwrap();
    assert_eq!(stored_text(&s, "a.go").as_deref(), Some(half));
    assert!(projected(&s, "a.go"));
    let d = crate::tree::diagnostic(&s.conn, "a.go").unwrap().unwrap();
    assert!(d.projected && d.blob == h(half), "{d:?}");
    assert_eq!(accepted.tree_root, cold_root(&[("a.go", Node::File(half, Mode::Regular))]));
}

/// T5: another writer moved the file; this writer's edit touches other
/// lines, so the merge lands both, and the proposal says it merged.
#[test]
fn a_moved_base_with_disjoint_edits_lands_the_merge() {
    let s = store(&[("a.go", Node::File(A, Mode::Regular))]);
    let theirs = A.replace("return 3", "return 30");
    tree_propose(&s, vec![Proposed::write("a.go", Base::Blob(h(A)), theirs.clone().into())]).unwrap();
    let ours = A.replace("return 1", "return 10");
    let accepted = tree_propose(&s, vec![Proposed::write("a.go", Base::Blob(h(A)), ours.into())]).unwrap();
    let both = A.replace("return 1", "return 10").replace("return 3", "return 30");
    assert_eq!(accepted.merged, BTreeSet::from(["a.go".to_string()]));
    assert_eq!(accepted.blobs["a.go"], h(&both));
    assert_eq!(stored_text(&s, "a.go").as_deref(), Some(both.as_str()));
    assert_eq!(accepted.tree_root, cold_root(&[("a.go", Node::File(&*both.leak(), Mode::Regular))]));
}

/// T5: overlapping hunks refuse; the refusal names the moved path and the
/// base lines both sides changed, and nothing lands.
#[test]
fn overlapping_edits_refuse_stale_with_conflict_ranges() {
    let s = store(&[("a.go", Node::File(A, Mode::Regular)), ("b.txt", Node::File("b\n", Mode::Regular))]);
    let theirs = A.replace("return 2", "return 20");
    tree_propose(&s, vec![Proposed::write("a.go", Base::Blob(h(A)), theirs.clone().into())]).unwrap();
    let before = image(&s);
    let ours = A.replace("return 2", "return 200");
    let err = tree_propose(
        &s,
        vec![
            Proposed::write("a.go", Base::Blob(h(A)), ours.into()),
            Proposed::write("b.txt", Base::Blob(h("b\n")), "bee\n".into()),
        ],
    )
    .unwrap_err();
    let Refusal::Conflict { moved, conflicts } = refused(err) else {
        panic!("a conflicting merge is refused with its ranges");
    };
    assert_eq!(
        moved,
        vec![Stale {
            path: "a.go".into(),
            expected: Some(h(A)),
            current: Some(h(&theirs)),
        }]
    );
    assert_eq!(
        conflicts,
        BTreeMap::from([("a.go".to_string(), vec![LineRange { start: 4, end: 5 }])])
    );
    assert_eq!(image(&s), before, "a refused proposal changes nothing");
}

/// T5: bytes that are not UTF-8 never merge; a moved base refuses.
#[test]
fn binary_files_never_merge() {
    let s = store(&[("a.go", Node::File(A, Mode::Regular))]);
    let bin = |tail: u8| {
        let mut b = b"\xff\xfe\x00 blob ".to_vec();
        b.push(tail);
        b
    };
    let base = tree_propose(&s, vec![Proposed::create("img.bin", bin(1))]).unwrap().blobs["img.bin"];
    tree_propose(&s, vec![Proposed::write("img.bin", Base::Blob(base), bin(2))]).unwrap();
    let err = tree_propose(&s, vec![Proposed::write("img.bin", Base::Blob(base), bin(3))]).unwrap_err();
    assert!(matches!(refused(err), Refusal::Stale(m) if m.len() == 1 && m[0].path == "img.bin"));
}

/// T5: a delete against a moved base refuses; a create over identical
/// bytes changes nothing and is accepted; over other bytes it is stale.
#[test]
fn deletes_and_creates_against_a_moved_path() {
    let s = store(&[("a.go", Node::File(A, Mode::Regular))]);
    let moved = A.replace("return 1", "return 9");
    tree_propose(&s, vec![Proposed::write("a.go", Base::Blob(h(A)), moved.clone().into())]).unwrap();
    let err = tree_propose(&s, vec![Proposed::delete("a.go", Base::Blob(h(A)))]).unwrap_err();
    assert!(matches!(refused(err), Refusal::Stale(_)));

    let root = crate::tree_root::DerivedFrom::read(&s.conn).unwrap().unwrap().tree_root;
    let same = tree_propose(&s, vec![Proposed::create("a.go", moved.clone().into())]).unwrap();
    assert_eq!(same.tree_root, root, "an identical create changes nothing");
    let err = tree_propose(&s, vec![Proposed::create("a.go", A.into())]).unwrap_err();
    assert_eq!(
        refused(err),
        Refusal::Stale(vec![Stale {
            path: "a.go".into(),
            expected: None,
            current: Some(h(&moved)),
        }])
    );
}

/// T4: a mode change is a write of the same bytes with the new mode; a
/// symlink is a write whose bytes are its target, and is never projected.
#[test]
fn modes_and_symlinks_are_proposals() {
    let s = store(&[("a.go", Node::File(A, Mode::Regular)), ("run.sh", Node::File("echo\n", Mode::Regular))]);
    let accepted = tree_propose(
        &s,
        vec![
            Proposed::write_mode("run.sh", Base::Blob(h("echo\n")), "echo\n".into(), Mode::Executable),
            Proposed::write_mode("link.go", Base::Absent, "a.go".into(), Mode::Symlink),
        ],
    )
    .unwrap();
    assert_eq!(
        crate::tree::entry(&s.conn, "run.sh").unwrap(),
        Some(Entry::File { mode: Mode::Executable, blob: h("echo\n") })
    );
    assert_eq!(
        crate::tree::entry(&s.conn, "link.go").unwrap(),
        Some(Entry::File { mode: Mode::Symlink, blob: h("a.go") })
    );
    assert!(!projected(&s, "link.go"), "a symlink is never projected");
    assert_eq!(
        accepted.tree_root,
        cold_root(&[
            ("a.go", Node::File(A, Mode::Regular)),
            ("link.go", Node::Link("a.go")),
            ("run.sh", Node::File("echo\n", Mode::Executable)),
        ])
    );
    // A write naming no mode keeps the entry's mode.
    tree_propose(&s, vec![Proposed::write("run.sh", Base::Blob(h("echo\n")), "echo 2\n".into())]).unwrap();
    assert_eq!(
        crate::tree::entry(&s.conn, "run.sh").unwrap(),
        Some(Entry::File { mode: Mode::Executable, blob: h("echo 2\n") })
    );
    // A symlink does not merge: a moved symlink refuses.
    let err = tree_propose(
        &s,
        vec![Proposed::write_mode("link.go", Base::Blob(h("b.go")), "c.go".into(), Mode::Symlink)],
    )
    .unwrap_err();
    assert!(matches!(refused(err), Refusal::Stale(_)));
}

/// T4: directories are entries a proposal makes and removes.
#[test]
fn mkdir_and_rmdir_are_proposals() {
    let s = store(&[("a.go", Node::File(A, Mode::Regular)), ("sub/b.txt", Node::File("b\n", Mode::Regular))]);
    let accepted = tree_propose(&s, vec![Proposed::mkdir("empty/inner")]).unwrap();
    assert_eq!(accepted.mkdirs, BTreeSet::from(["empty/inner".to_string()]));
    assert_eq!(crate::tree::entry(&s.conn, "empty").unwrap(), Some(Entry::Dir));
    assert_eq!(crate::tree::entry(&s.conn, "empty/inner").unwrap(), Some(Entry::Dir));
    assert_eq!(
        accepted.tree_root,
        cold_root(&[
            ("a.go", Node::File(A, Mode::Regular)),
            ("empty/inner", Node::Dir),
            ("sub/b.txt", Node::File("b\n", Mode::Regular)),
        ])
    );

    let before = image(&s);
    let refusal = |w: Proposed| refused(tree_propose(&s, vec![w]).unwrap_err());
    assert_eq!(refusal(Proposed::mkdir("sub")), Refusal::Exists { path: "sub".into() });
    assert_eq!(refusal(Proposed::mkdir("a.go")), Refusal::Exists { path: "a.go".into() });
    assert_eq!(refusal(Proposed::rmdir("sub")), Refusal::NotEmpty { path: "sub".into() });
    assert_eq!(refusal(Proposed::rmdir("a.go")), Refusal::NotADirectory { path: "a.go".into() });
    assert_eq!(refusal(Proposed::rmdir("nope")), Refusal::NoSuchFile { path: "nope".into() });
    assert_eq!(
        refusal(Proposed::write("sub", Base::Any, "x".into())),
        Refusal::IsADirectory { path: "sub".into() }
    );
    assert_eq!(
        refusal(Proposed::delete("sub", Base::Any)),
        Refusal::IsADirectory { path: "sub".into() }
    );
    assert_eq!(refusal(Proposed::create("sub", "x".into())), Refusal::Exists { path: "sub".into() });
    assert_eq!(
        refusal(Proposed::create("a.go/x", "x".into())),
        Refusal::PathConflict { path: "a.go/x".into(), other: "a.go".into() }
    );
    assert_eq!(
        refusal(Proposed { path: "d".into(), base: Base::Any, change: Change::Mkdir }),
        Refusal::MalformedBase { path: "d".into() }
    );
    assert_eq!(
        refusal(Proposed { path: "sub".into(), base: Base::Absent, change: Change::Rmdir }),
        Refusal::MalformedBase { path: "sub".into() }
    );
    assert_eq!(refusal(Proposed::mkdir("../up")), Refusal::MalformedPath { path: "../up".into() });
    assert_eq!(image(&s), before, "every refusal changes nothing");

    // The last file leaves; its directory stays, empty, until an rmdir in
    // the same proposal or a later one removes it.
    tree_propose(&s, vec![Proposed::delete("sub/b.txt", Base::Blob(h("b\n")))]).unwrap();
    assert_eq!(crate::tree::entry(&s.conn, "sub").unwrap(), Some(Entry::Dir));
    let accepted = tree_propose(&s, vec![Proposed::rmdir("sub"), Proposed::rmdir("empty/inner")]).unwrap();
    assert_eq!(crate::tree::entry(&s.conn, "sub").unwrap(), None);
    assert_eq!(
        accepted.tree_root,
        cold_root(&[("a.go", Node::File(A, Mode::Regular)), ("empty", Node::Dir)])
    );
}

/// T4: a directory rename is one proposal over every path under it; each
/// file keeps its bytes and mode, each directory moves, and nothing is
/// left at the old path.
#[test]
fn a_directory_rename_is_one_proposal() {
    let s = store(&[
        ("a.go", Node::File(A, Mode::Regular)),
        ("pkg/b.go", Node::File(A, Mode::Regular)),
        ("pkg/tool", Node::File("#!/bin/sh\n", Mode::Executable)),
        ("pkg/empty", Node::Dir),
        ("pkg/deep/c.txt", Node::File("c\n", Mode::Regular)),
    ]);
    let writes = rename_dir(&s.conn, "pkg", "lib").unwrap();
    let accepted = tree_propose(&s, writes).unwrap();
    let paths: Vec<String> = crate::tree::entries(&s.conn).unwrap().into_keys().collect();
    assert_eq!(
        paths,
        ["a.go", "lib", "lib/b.go", "lib/deep", "lib/deep/c.txt", "lib/empty", "lib/tool"]
    );
    assert!(projected(&s, "lib/b.go") && !projected(&s, "pkg/b.go"));
    assert_eq!(
        accepted.tree_root,
        cold_root(&[
            ("a.go", Node::File(A, Mode::Regular)),
            ("lib/b.go", Node::File(A, Mode::Regular)),
            ("lib/tool", Node::File("#!/bin/sh\n", Mode::Executable)),
            ("lib/empty", Node::Dir),
            ("lib/deep/c.txt", Node::File("c\n", Mode::Regular)),
        ])
    );
    // A rename whose source moved since it was planned refuses whole.
    let writes = rename_dir(&s.conn, "lib", "src").unwrap();
    tree_propose(&s, vec![Proposed::write("lib/deep/c.txt", Base::Any, "moved\n".into())]).unwrap();
    let before = image(&s);
    assert!(matches!(refused(tree_propose(&s, writes).unwrap_err()), Refusal::Stale(_)));
    assert_eq!(image(&s), before);
    // Onto an existing directory with entries: refused, nothing moves.
    let err = rename_dir(&s.conn, "lib/deep", "lib").unwrap_err();
    assert!(format!("{err:#}").contains("lib"), "{err:#}");
    let _ = &s.src;
}
