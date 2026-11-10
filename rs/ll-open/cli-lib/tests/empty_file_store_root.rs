//! Bead `ley-line-open-bc7df6`, carried to ADR-0047 D-ii: `storeRoot` tells
//! an empty file from an empty directory at the same path. The file's child
//! entry is BLAKE3 of no bytes under its file mode; the directory's is the
//! store-tree fold of no entries under the directory mode. A fold that
//! dropped or flattened the mode, or that gave an empty directory the empty
//! blob as its content, would give the two stores one root. Pinned on the
//! pure fold and on two cold parses.

use std::collections::BTreeMap;

use leyline_cli_lib::store_root::{StoreLeaf, leaves_of, store_root_of};
use leyline_cli_lib::tree::{Entry, Mode};
use leyline_core::{ContentAddressed, Hash};

fn empty() -> Hash {
    b"".as_slice().hash()
}

fn root(leaves: &[StoreLeaf]) -> Hash {
    store_root_of(leaves).expect("the leaves are a tree")
}

#[test]
fn an_empty_file_and_an_empty_directory_at_one_path_fold_differently() {
    let file = BTreeMap::from([(
        "p".to_string(),
        Entry::File {
            mode: Mode::Regular,
            blob: empty(),
        },
    )]);
    let dir = BTreeMap::from([("p".to_string(), Entry::Dir)]);
    let (file_leaves, dir_leaves) = (leaves_of(&file), leaves_of(&dir));
    assert_eq!(file_leaves, vec![StoreLeaf::regular("p", empty())]);
    assert_eq!(dir_leaves, vec![StoreLeaf::EmptyDir { path: "p".into() }]);
    let (file_root, dir_root) = (root(&file_leaves), root(&dir_leaves));
    assert_ne!(file_root, dir_root);

    // An executable empty file is a third store.
    let exec = root(&[StoreLeaf::File {
        path: "p".into(),
        mode: Mode::Executable,
        blob: empty(),
    }]);
    assert_ne!(exec, file_root);
    assert_ne!(exec, dir_root);

    // An empty file is not an absent one, and neither is an empty directory.
    assert_ne!(file_root, root(&[]));
    assert_ne!(dir_root, root(&[]));
}

#[test]
fn cold_parses_of_an_empty_file_and_an_empty_directory_land_different_roots() {
    let root_of = |make: &dyn Fn(&std::path::Path)| {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("a.go"), "package a\n").unwrap();
        make(&src.path().join("p"));
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let result =
            leyline_cli_lib::cmd_parse::parse_into_conn(&conn, src.path(), None, None).unwrap();
        let entries = leyline_cli_lib::tree::entries(&conn).unwrap();
        (result.store_root, entries.get("p").copied())
    };
    let (file_root, file_entry) = root_of(&|p| std::fs::write(p, "").unwrap());
    let (dir_root, dir_entry) = root_of(&|p| std::fs::create_dir(p).unwrap());
    let (none_root, none_entry) = root_of(&|_| {});
    assert_eq!(
        file_entry,
        Some(Entry::File {
            mode: Mode::Regular,
            blob: empty(),
        })
    );
    assert_eq!(dir_entry, Some(Entry::Dir));
    assert_eq!(none_entry, None);
    assert_ne!(file_root, dir_root);
    assert_ne!(file_root, none_root, "the empty file moves the root");
    assert_ne!(dir_root, none_root, "the empty directory moves the root");
}
