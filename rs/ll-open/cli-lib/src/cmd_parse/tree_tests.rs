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
        (
            "Makefile.local".to_string(),
            file(Mode::Regular, b"all:\n\ttrue\n"),
        ),
        ("README.md".to_string(), file(Mode::Regular, b"# notes\n")),
        ("a.go".to_string(), file(Mode::Regular, GO.as_bytes())),
        ("empty".to_string(), Entry::Dir),
        ("empty/inner".to_string(), Entry::Dir),
        ("link".to_string(), file(Mode::Symlink, b"README.md")),
        (
            "run.sh".to_string(),
            file(Mode::Executable, b"#!/bin/sh\necho hi\n"),
        ),
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
    assert_eq!(
        projected(&conn),
        ["README.md", "a.go", "run.sh", "sub/b.go"]
    );
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
    assert!(
        start <= end && end as usize <= broken.len(),
        "{start}..{end}"
    );
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

fn scoped(conn: &Connection, root: &Path, paths: &[&str]) {
    let scope: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
    parse_into_conn(conn, root, None, Some(&scope)).unwrap();
}

/// A scoped pass (the watcher's) reads each path's kind from disk: a
/// symlink is stored as one, a path gone from disk or turned into a socket
/// leaves, a directory gone from disk takes its entries, and a path it
/// cannot stat is left as the store holds it.
#[cfg(unix)]
#[test]
fn a_scoped_pass_follows_each_paths_kind_on_disk() {
    let td = TempDir::new().unwrap();
    let root = td.path();
    write(root, "a.go", GO.as_bytes());
    write(root, "gone.txt", b"g\n");
    write(root, "sock.txt", b"s\n");
    write(root, "d/x.txt", b"x\n");
    write(root, "locked/y.txt", b"y\n");
    let conn = Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, root, None, None).unwrap();

    std::os::unix::fs::symlink("a.go", root.join("link")).unwrap();
    std::fs::remove_file(root.join("gone.txt")).unwrap();
    std::fs::remove_file(root.join("sock.txt")).unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(root.join("sock.txt")).unwrap();
    std::fs::remove_dir_all(root.join("d")).unwrap();
    chmod(root, "locked", 0o000);
    scoped(
        &conn,
        root,
        &["link", "gone.txt", "sock.txt", "d", "locked/y.txt"],
    );
    chmod(root, "locked", 0o755);

    let all = crate::tree::entries(&conn).unwrap();
    assert_eq!(all.get("link"), Some(&file(Mode::Symlink, b"a.go")));
    for gone in ["gone.txt", "sock.txt", "d", "d/x.txt"] {
        assert!(!all.contains_key(gone), "{gone} left the store: {all:?}");
    }
    assert_eq!(
        all.get("locked/y.txt"),
        Some(&file(Mode::Regular, b"y\n")),
        "a path the pass cannot stat keeps its entry"
    );
}

/// The walk's stat cache is trusted as git's index is: a file whose mode
/// and stat match what the walk stored is not read again.
#[test]
fn a_file_whose_stat_matches_the_store_is_not_read_again() {
    let td = TempDir::new().unwrap();
    let root = td.path();
    write(root, "notes.txt", b"one\n");
    let conn = Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, root, None, None).unwrap();
    let mtime = std::fs::metadata(root.join("notes.txt"))
        .unwrap()
        .modified()
        .unwrap();
    write(root, "notes.txt", b"two\n");
    std::fs::File::options()
        .write(true)
        .open(root.join("notes.txt"))
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    parse_into_conn(&conn, root, None, None).unwrap();
    assert_eq!(
        crate::tree::entry(&conn, "notes.txt").unwrap(),
        Some(file(Mode::Regular, b"one\n"))
    );
}

/// A diagnostic belongs to the bytes it describes: a stat-only change
/// keeps it; new bytes the parse does not take up drop it.
#[test]
fn a_diagnostic_survives_a_touch_and_leaves_with_its_bytes() {
    let td = TempDir::new().unwrap();
    let root = td.path();
    write(root, "a.go", b"package a\n\nfunc A( {\n");
    let conn = Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, root, None, None).unwrap();
    assert!(crate::tree::diagnostic(&conn, "a.go").unwrap().is_some());

    // The same bytes, a new mtime: the parse skips it by its hash.
    write(root, "a.go", b"package a\n\nfunc A( {\n");
    parse_into_conn(&conn, root, None, None).unwrap();
    assert!(
        crate::tree::diagnostic(&conn, "a.go").unwrap().is_some(),
        "the bytes did not change"
    );

    // New bytes under a filter that does not project the file.
    write(root, "a.go", b"package a\n\nfunc B( {\n");
    parse_into_conn(&conn, root, Some("python"), None).unwrap();
    assert_eq!(crate::tree::diagnostic(&conn, "a.go").unwrap(), None);
}

/// A scoped pass naming no filter keeps the one a whole pass recorded, so
/// a later proposal projects under it.
#[test]
fn a_scoped_pass_keeps_the_recorded_language_filter() {
    let td = TempDir::new().unwrap();
    let root = td.path();
    write(root, "a.go", GO.as_bytes());
    let conn = Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, root, Some("go"), None).unwrap();
    write(root, "b.go", GO.as_bytes());
    scoped(&conn, root, &["b.go"]);
    assert_eq!(
        get_meta(&conn, PROJECTION_LANGUAGE_KEY).unwrap().as_deref(),
        Some("go")
    );
    parse_into_conn(&conn, root, None, None).unwrap();
    assert_eq!(get_meta(&conn, PROJECTION_LANGUAGE_KEY).unwrap(), None);
}

/// A file that leaves the projection without leaving the store (it became
/// a symlink) takes the presentation rows of the directory it emptied.
#[test]
fn a_file_leaving_the_projection_sweeps_its_emptied_directory() {
    let td = TempDir::new().unwrap();
    let root = td.path();
    write(root, "a.go", GO.as_bytes());
    write(root, "sub/b.go", GO.as_bytes());
    let conn = Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, root, None, None).unwrap();
    let dirs = |conn: &Connection| -> i64 {
        conn.query_row("SELECT count(*) FROM nodes WHERE nid < -1", [], |r| {
            r.get(0)
        })
        .unwrap()
    };
    assert_eq!(dirs(&conn), 1, "sub's presentation row");
    parse_written_files(
        &conn,
        root,
        &BTreeMap::from([("sub/b.go".to_string(), b"a.go".to_vec())]),
        &std::collections::BTreeSet::new(),
        &TreeEdits {
            modes: BTreeMap::from([("sub/b.go".to_string(), Mode::Symlink)]),
            ..TreeEdits::default()
        },
    )
    .unwrap();
    assert_eq!(dirs(&conn), 0, "sub holds no projected file");
    assert_eq!(
        crate::tree::entry(&conn, "sub/b.go").unwrap(),
        Some(file(Mode::Symlink, b"a.go"))
    );
}
