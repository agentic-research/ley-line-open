//! Bead `ley-line-open-bc7df6`: `treeRoot` tells an empty file from an
//! empty directory at the same path. Both fold BLAKE3 of no bytes as their
//! content (ADR-0045 T2); only the leaf's mode tag separates them, so a
//! fold that dropped or flattened the mode would give the two trees one
//! root. Pinned on the pure fold and on two cold parses.

use std::collections::BTreeMap;

use leyline_cli_lib::tree::{DIRECTORY_MODE, Entry, Mode};
use leyline_cli_lib::tree_root::{TreeLeaf, leaves_of, tree_root_of};
use leyline_core::{ContentAddressed, Hash};

fn empty() -> Hash {
    b"".as_slice().hash()
}

#[test]
fn an_empty_file_and_an_empty_directory_at_one_path_fold_differently() {
    let parser = Hash::from_bytes([7; 32]);
    let file = BTreeMap::from([(
        "p".to_string(),
        Entry::File {
            mode: Mode::Regular,
            blob: empty(),
        },
    )]);
    let dir = BTreeMap::from([("p".to_string(), Entry::Dir)]);
    let (file_leaves, dir_leaves) = (leaves_of(&file), leaves_of(&dir));
    assert_eq!(file_leaves, vec![TreeLeaf::regular("p", empty())]);
    assert_eq!(
        dir_leaves,
        vec![TreeLeaf {
            path: "p".into(),
            mode: DIRECTORY_MODE,
            content: empty(),
        }]
    );
    assert_eq!(
        (&file_leaves[0].path, file_leaves[0].content),
        (&dir_leaves[0].path, dir_leaves[0].content),
        "the two leaves share path and content; only the mode differs"
    );
    let (file_root, dir_root) = (
        tree_root_of(&file_leaves, parser),
        tree_root_of(&dir_leaves, parser),
    );
    assert_ne!(file_root, dir_root);

    // An executable empty file is a third tree.
    let exec = tree_root_of(
        &[TreeLeaf {
            path: "p".into(),
            mode: 0o100755,
            content: empty(),
        }],
        parser,
    );
    assert_ne!(exec, file_root);
    assert_ne!(exec, dir_root);

    // An empty file is not an absent one.
    assert_ne!(file_root, tree_root_of(&[], parser));
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
        (result.tree_root, entries.get("p").copied())
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
