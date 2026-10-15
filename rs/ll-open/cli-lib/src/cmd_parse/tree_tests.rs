//! ADR-0045 T1–T3 through the parse pass: the store holds every walked
//! file, symlink and directory with its mode; the projection is the subset
//! the parse knows; a file that does not parse is stored with a diagnostic.

use std::collections::BTreeMap;
use std::path::Path;

use leyline_core::{ContentAddressed, Hash};
use rusqlite::Connection;
use tempfile::TempDir;

use super::*;
use crate::tree::{Entry, Mode};

const GO: &str = "package a\n\nfunc A() int { return 1 }\n";

fn write(root: &Path, rel: &str, text: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

#[cfg(unix)]
fn chmod(root: &Path, rel: &str, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(root.join(rel), std::fs::Permissions::from_mode(mode)).unwrap();
}

fn file(mode: Mode, bytes: &[u8]) -> Entry {
    Entry::File {
        mode,
        blob: bytes.hash(),
    }
}

fn projected(conn: &Connection) -> Vec<String> {
    let mut stmt = conn.prepare("SELECT id FROM _source ORDER BY id").unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn stored_bytes(conn: &Connection, blob: Hash) -> Vec<u8> {
    crate::tree::blob_bytes(conn, blob).unwrap().unwrap()
}

/// M2: a cold parse records every walked file with its git mode, every
/// symlink with its target as its blob, and every directory; only the
/// files the parse knows are projected; `treeRoot` folds the manifest.
#[cfg(unix)]
#[test]
fn a_cold_parse_stores_every_file_with_its_mode() {
    let td = TempDir::new().unwrap();
    let root = td.path();
    write(root, "a.go", GO.as_bytes());
    write(root, "sub/b.go", GO.as_bytes());
    write(root, "README.md", b"# notes\n");
    write(root, "Makefile.local", b"all:\n\ttrue\n");
    write(root, "run.sh", b"#!/bin/sh\necho hi\n");
    chmod(root, "run.sh", 0o755);
    std::os::unix::fs::symlink("README.md", root.join("link")).unwrap();
    std::fs::create_dir_all(root.join("empty/inner")).unwrap();

    let conn = Connection::open_in_memory().unwrap();
    let result = parse_into_conn(&conn, root, None, None).unwrap();

    let expected = BTreeMap::from([
        ("Makefile.local".to_string(), file(Mode::Regular, b"all:\n\ttrue\n")),
        ("README.md".to_string(), file(Mode::Regular, b"# notes\n")),
        ("a.go".to_string(), file(Mode::Regular, GO.as_bytes())),
        ("empty".to_string(), Entry::Dir),
        ("empty/inner".to_string(), Entry::Dir),
        ("link".to_string(), file(Mode::Symlink, b"README.md")),
        ("run.sh".to_string(), file(Mode::Executable, b"#!/bin/sh\necho hi\n")),
        ("sub".to_string(), Entry::Dir),
        ("sub/b.go".to_string(), file(Mode::Regular, GO.as_bytes())),
    ]);
    assert_eq!(crate::tree::entries(&conn).unwrap(), expected);
    // Every file's bytes are in the store, the symlink's target among them.
    assert_eq!(stored_bytes(&conn, b"README.md".hash()), b"README.md");
    assert_eq!(stored_bytes(&conn, b"# notes\n".hash()), b"# notes\n");
    // `README.md` (markdown) and `run.sh` (bash) are projected;
    // `Makefile.local` names no grammar and is stored only, and the symlink
    // is never followed.
    assert_eq!(projected(&conn), ["README.md", "a.go", "run.sh", "sub/b.go"]);
    let parser_id = crate::tree_root::ParserInputs::current(
        &leyline_ts::query_engine::resolve_query_set(&conn, &Default::default())
            .unwrap()
            .query_set,
    )
    .parser_id();
    assert_eq!(
        result.tree_root,
        crate::tree_root::tree_root_of(&crate::tree_root::leaves_of(&expected), parser_id)
    );
}

/// `--language` narrows the projection, never the store.
#[test]
fn a_language_filter_narrows_the_projection_and_not_the_store() {
    let td = TempDir::new().unwrap();
    let root = td.path();
    write(root, "a.go", GO.as_bytes());
    write(root, "lib.py", b"def f():\n    return 1\n");
    let conn = Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, root, Some("go"), None).unwrap();
    assert_eq!(projected(&conn), ["a.go"]);
    let all: Vec<String> = crate::tree::entries(&conn).unwrap().into_keys().collect();
    assert_eq!(all, ["a.go", "lib.py"]);
}

/// M3: a file the parse cannot project (a NUL byte) is stored, has no
/// projection rows, and carries a diagnostic naming why.
#[test]
fn a_binary_file_is_stored_unprojected_with_a_diagnostic() {
    let td = TempDir::new().unwrap();
    let root = td.path();
    write(root, "ok.go", GO.as_bytes());
    let bad = b"package bad\n\x00\n";
    write(root, "bad.go", bad);
    let conn = Connection::open_in_memory().unwrap();
    let result = parse_into_conn(&conn, root, None, None).unwrap();
    assert_eq!(result.errors, 1);
    assert_eq!(projected(&conn), ["ok.go"]);
    assert_eq!(
        crate::tree::entry(&conn, "bad.go").unwrap(),
        Some(file(Mode::Regular, bad))
    );
    let d = crate::tree::diagnostic(&conn, "bad.go").unwrap().unwrap();
    assert!(!d.projected);
    assert_eq!(d.blob, bad.hash());
    assert_eq!(d.span, None);
    assert!(d.message.contains("binary file"), "{}", d.message);
    assert_eq!(crate::tree::diagnostic(&conn, "ok.go").unwrap(), None);
}

/// M3: a file with syntax errors is projected as tree-sitter parsed it and
/// carries a diagnostic at its first error; fixing it clears the row.
#[test]
fn a_syntax_error_is_projected_with_a_diagnostic_that_a_fix_clears() {
    let td = TempDir::new().unwrap();
    let root = td.path();
    let broken = b"package a\n\nfunc A( {\n";
    write(root, "a.go", broken);
    let conn = Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, root, None, None).unwrap();
    assert_eq!(projected(&conn), ["a.go"]);
    let d = crate::tree::diagnostic(&conn, "a.go").unwrap().unwrap();
    assert!(d.projected);
    assert_eq!(d.blob, broken.hash());
    let (start, end, row, _col) = d.span.expect("a projected diagnostic has a span");
    assert!(start <= end && end as usize <= broken.len(), "{start}..{end}");
    assert_eq!(row, 2, "the error is on the third line");
    assert!(d.message.contains("syntax error"), "{}", d.message);

    write(root, "a.go", GO.as_bytes());
    parse_into_conn(&conn, root, None, None).unwrap();
    assert_eq!(crate::tree::diagnostic(&conn, "a.go").unwrap(), None);
}

/// An incremental pass follows the directory: a changed unprojected file,
/// a mode change with no content change, a deletion and a new directory.
#[cfg(unix)]
#[test]
fn an_incremental_pass_follows_bytes_modes_deletions_and_directories() {
    let td = TempDir::new().unwrap();
    let root = td.path();
    write(root, "a.go", GO.as_bytes());
    write(root, "notes.txt", b"one\n");
    write(root, "tool", b"#!/bin/sh\n");
    let conn = Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, root, None, None).unwrap();

    write(root, "notes.txt", b"two\n");
    chmod(root, "tool", 0o755);
    std::fs::remove_file(root.join("a.go")).unwrap();
    std::fs::create_dir_all(root.join("new")).unwrap();
    parse_into_conn(&conn, root, None, None).unwrap();

    assert_eq!(
        crate::tree::entries(&conn).unwrap(),
        BTreeMap::from([
            ("new".to_string(), Entry::Dir),
            ("notes.txt".to_string(), file(Mode::Regular, b"two\n")),
            ("tool".to_string(), file(Mode::Executable, b"#!/bin/sh\n")),
        ])
    );
    assert!(projected(&conn).is_empty());
}

/// A scoped pass touches only its paths, unprojected files included.
#[test]
fn a_scoped_pass_stores_an_unprojected_file_in_its_scope() {
    let td = TempDir::new().unwrap();
    let root = td.path();
    write(root, "a.go", GO.as_bytes());
    let conn = Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, root, None, None).unwrap();
    write(root, "deep/notes.txt", b"n\n");
    write(root, "other.txt", b"o\n");
    parse_into_conn(&conn, root, None, Some(&["deep/notes.txt".to_string()])).unwrap();
    let all: Vec<String> = crate::tree::entries(&conn).unwrap().into_keys().collect();
    assert_eq!(all, ["a.go", "deep", "deep/notes.txt"]);
}
