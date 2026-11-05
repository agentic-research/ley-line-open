//! Bead `ley-line-open-bc7df6`: an empty file is a real file. A zero-byte
//! file is created, replaced to and from, deleted and recreated, parsed and
//! merged like any other: it lands as 0 bytes whose blob hash is BLAKE3 of
//! no bytes, and every accepted proposal lands the `treeRoot` a cold parse
//! of the resulting tree lands. None of these paths skips it, drops it or
//! reports success with nothing landed (the bug class of bead `8a26d8`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use leyline_core::{ContentAddressed, Hash};
use rusqlite::Connection;
use tempfile::TempDir;

use super::*;
use crate::merge::{LineRange, Merged};
use crate::tree::{Entry, Mode};

const A: &str = "package a\n\nfunc A() int { return 1 }\n";
const RS: &str = "pub fn a() -> u32 {\n    1\n}\n";

/// BLAKE3 of no bytes: the blob every empty file names.
fn empty() -> Hash {
    b"".as_slice().hash()
}

fn h(text: &str) -> Hash {
    text.as_bytes().hash()
}

fn lay_out(root: &Path, files: &[(&str, &str)]) {
    for (rel, text) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
}

struct Store {
    src: TempDir,
    _dir: TempDir,
    conn: Connection,
}

/// A cold-parsed, file-backed store over `files`.
fn store(files: &[(&str, &str)]) -> Store {
    let src = TempDir::new().unwrap();
    lay_out(src.path(), files);
    let dir = TempDir::new().unwrap();
    let conn = Connection::open(dir.path().join("live.db")).unwrap();
    crate::cmd_parse::parse_into_conn(&conn, src.path(), None, None).unwrap();
    Store {
        src,
        _dir: dir,
        conn,
    }
}

fn propose_whole(s: &Store, writes: Vec<Proposed>) -> Result<Accepted> {
    propose(&s.conn, &Grant::Whole, writes)
}

fn refused(err: anyhow::Error) -> Refusal {
    err.downcast::<Refusal>().expect("a typed refusal")
}

/// The bytes the store holds at `path`, read through its manifest entry.
fn stored(s: &Store, path: &str) -> Option<Vec<u8>> {
    let Some(Entry::File { blob, .. }) = crate::tree::entry(&s.conn, path).unwrap() else {
        return None;
    };
    Some(
        crate::tree::blob_bytes(&s.conn, blob)
            .unwrap()
            .expect("the entry's blob is stored"),
    )
}

/// The `source_blobs` row's recorded length for `blob`.
fn blob_len(conn: &Connection, blob: Hash) -> Option<i64> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT byte_len FROM source_blobs WHERE blob_hash = ?1",
        [blob.as_bytes().as_slice()],
        |r| r.get(0),
    )
    .optional()
    .unwrap()
}

/// `_source.content_hash` for `path`, when the file is projected.
fn source_hash(conn: &Connection, path: &str) -> Option<Hash> {
    use rusqlite::OptionalExtension;
    let bytes: Option<Vec<u8>> = conn
        .query_row(
            "SELECT content_hash FROM _source WHERE id = ?1",
            [path],
            |r| r.get(0),
        )
        .optional()
        .unwrap();
    bytes.map(|b| Hash::from_bytes(b.try_into().expect("a 32-byte content hash")))
}

fn file_id(conn: &Connection, path: &str) -> leyline_schema::FileId {
    conn.query_row("SELECT file_id FROM _source WHERE id = ?1", [path], |r| {
        r.get(0)
    })
    .unwrap()
}

/// One `_ast` row: masked nid, grammar kind, byte span and `node_hash`.
type AstRow = (i64, String, i64, i64, Option<Vec<u8>>);

/// The file's `_ast` rows, with the arena-local `file_id` masked out of
/// every nid (ADR-0040 F5): what a cold parse and a write must agree on.
fn ast_rows(conn: &Connection, path: &str) -> Vec<AstRow> {
    let (lo, hi) = leyline_schema::file_nid_range(file_id(conn, path));
    let mask = leyline_schema::NID_ORDINAL_MASK;
    let mut stmt = conn
        .prepare(
            "SELECT a.nid, k.raw_kind, a.start_byte, a.end_byte, a.node_hash \
               FROM v_ast_hash a JOIN kinds k ON k.kind_id = a.kind_id \
              WHERE a.nid BETWEEN ?1 AND ?2 ORDER BY a.nid",
        )
        .unwrap();
    stmt.query_map([lo, hi], |r| {
        Ok((
            r.get::<_, i64>(0)? & mask,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
        ))
    })
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

/// The `treeRoot` a cold parse of `files` lands.
fn cold_root(files: &[(&str, &str)]) -> Hash {
    let src = TempDir::new().unwrap();
    lay_out(src.path(), files);
    let conn = Connection::open_in_memory().unwrap();
    crate::cmd_parse::parse_into_conn(&conn, src.path(), None, None)
        .unwrap()
        .tree_root
}

/// Every claim an empty file at `path` must satisfy in `s`: a regular
/// manifest entry naming BLAKE3(""), a `source_blobs` row of 0 bytes under
/// that hash, and 0 bytes read back.
fn assert_empty_file(s: &Store, path: &str) {
    assert_eq!(
        crate::tree::entry(&s.conn, path).unwrap(),
        Some(Entry::File {
            mode: Mode::Regular,
            blob: empty(),
        }),
        "{path} is a regular file naming the empty blob"
    );
    assert_eq!(
        blob_len(&s.conn, empty()),
        Some(0),
        "the empty blob is stored"
    );
    assert_eq!(
        stored(s, path),
        Some(Vec::new()),
        "{path} reads back 0 bytes"
    );
}

/// Create an empty file in a projected language and in none: each lands as
/// a 0-byte file under BLAKE3(""), the projected one with a `_source` row
/// of its own, and the tree root is the cold parse's of the tree holding
/// them.
#[test]
fn an_empty_create_lands_a_zero_byte_file() {
    let s = store(&[("a.go", A)]);
    let accepted = propose_whole(
        &s,
        vec![
            Proposed::create("e.go", Vec::new()),
            Proposed::create("docs/e.txt", Vec::new()),
        ],
    )
    .unwrap();
    assert_eq!(accepted.blobs["e.go"], empty());
    assert_eq!(accepted.blobs["docs/e.txt"], empty());
    assert!(accepted.merged.is_empty());
    assert_empty_file(&s, "e.go");
    assert_empty_file(&s, "docs/e.txt");
    assert_eq!(source_hash(&s.conn, "e.go"), Some(empty()));
    assert_eq!(
        leyline_ts::splice::source_bytes(&s.conn, file_id(&s.conn, "e.go")).unwrap(),
        Vec::<u8>::new()
    );
    let cold = [("a.go", A), ("e.go", ""), ("docs/e.txt", "")];
    assert_eq!(accepted.tree_root, cold_root(&cold));
    let cold_store = store(&cold);
    assert_eq!(
        ast_rows(&s.conn, "e.go"),
        ast_rows(&cold_store.conn, "e.go")
    );
    assert!(
        !s.src.path().join("e.go").exists(),
        "a proposal writes no file"
    );
}

/// Replace a non-empty file with no bytes, against its blob and with no
/// check: the file stays, holding 0 bytes, projected as a cold parse of an
/// empty file; then back to bytes against the empty blob.
#[test]
fn a_replace_to_empty_and_back_lands_each_side() {
    for base in [Base::Blob(h(A)), Base::Any] {
        let s = store(&[("a.go", A), ("b.txt", "b\n")]);
        let accepted = propose_whole(
            &s,
            vec![
                Proposed::write("a.go", base.clone(), Vec::new()),
                Proposed::write("b.txt", Base::Blob(h("b\n")), Vec::new()),
            ],
        )
        .unwrap();
        assert_eq!(accepted.blobs["a.go"], empty(), "{base:?}");
        assert!(
            accepted.removed.is_empty(),
            "an empty replace is not a delete"
        );
        assert_empty_file(&s, "a.go");
        assert_empty_file(&s, "b.txt");
        assert_eq!(source_hash(&s.conn, "a.go"), Some(empty()));
        let cold = [("a.go", ""), ("b.txt", "")];
        assert_eq!(accepted.tree_root, cold_root(&cold));
        assert_eq!(
            ast_rows(&s.conn, "a.go"),
            ast_rows(&store(&cold).conn, "a.go")
        );

        let back = propose_whole(
            &s,
            vec![Proposed::write("a.go", Base::Blob(empty()), A.into())],
        )
        .unwrap();
        assert_eq!(back.blobs["a.go"], h(A));
        assert_eq!(stored(&s, "a.go").as_deref(), Some(A.as_bytes()));
        assert_eq!(back.tree_root, cold_root(&[("a.go", A), ("b.txt", "")]));
    }
}

/// Delete a file, then create it again empty; and an empty file deletes
/// against the empty blob like any other.
#[test]
fn delete_then_recreate_empty_lands_and_the_empty_file_deletes() {
    let s = store(&[("a.go", A), ("b.go", "package a\n")]);
    propose_whole(&s, vec![Proposed::delete("a.go", Base::Blob(h(A)))]).unwrap();
    assert_eq!(crate::tree::entry(&s.conn, "a.go").unwrap(), None);
    let recreated = propose_whole(&s, vec![Proposed::create("a.go", Vec::new())]).unwrap();
    assert_empty_file(&s, "a.go");
    assert_eq!(source_hash(&s.conn, "a.go"), Some(empty()));
    assert_eq!(
        recreated.tree_root,
        cold_root(&[("a.go", ""), ("b.go", "package a\n")])
    );

    // A create of exactly what the path holds (no bytes over no bytes) is
    // accepted and changes nothing; a create of bytes over the empty file
    // is stale against the empty blob.
    let same = propose_whole(&s, vec![Proposed::create("a.go", Vec::new())]).unwrap();
    assert_eq!(same.tree_root, recreated.tree_root);
    let Refusal::Stale(moved) =
        refused(propose_whole(&s, vec![Proposed::create("a.go", A.into())]).unwrap_err())
    else {
        panic!("a create over the empty file is stale");
    };
    assert_eq!(
        moved,
        vec![Stale {
            path: "a.go".into(),
            expected: None,
            current: Some(empty()),
        }]
    );

    let deleted = propose_whole(&s, vec![Proposed::delete("a.go", Base::Blob(empty()))]).unwrap();
    assert_eq!(deleted.removed, BTreeSet::from(["a.go".to_string()]));
    assert_eq!(crate::tree::entry(&s.conn, "a.go").unwrap(), None);
    assert_eq!(source_hash(&s.conn, "a.go"), None);
    assert_eq!(deleted.tree_root, cold_root(&[("b.go", "package a\n")]));
}

/// An empty file in each projected language parses: its `_source` row
/// names BLAKE3(""), its source reads back as 0 bytes, its `_ast` holds no
/// node past byte 0, and a reparse changes nothing.
#[test]
fn an_empty_file_in_a_projected_language_parses_and_reparses_idempotently() {
    let files = [
        ("e.go", ""),
        ("e.rs", ""),
        ("e.py", ""),
        ("e.ts", ""),
        ("a.go", A),
        ("a.rs", RS),
    ];
    let s = store(&files);
    for (path, _) in files.iter().filter(|(_, text)| text.is_empty()) {
        assert_empty_file(&s, path);
        assert_eq!(
            source_hash(&s.conn, path),
            Some(empty()),
            "{path} is projected"
        );
        assert_eq!(
            leyline_ts::splice::source_bytes(&s.conn, file_id(&s.conn, path)).unwrap(),
            Vec::<u8>::new(),
            "{path}"
        );
        // One node: the grammar's root, spanning no bytes.
        let rows = ast_rows(&s.conn, path);
        let root = match *path {
            "e.py" => "module",
            "e.ts" => "program",
            _ => "source_file",
        };
        assert_eq!(rows.len(), 1, "{path}: {rows:?}");
        let (nid, kind, start, end, hash) = &rows[0];
        assert_eq!(
            (*nid, kind.as_str(), *start, *end),
            (0, root, 0, 0),
            "{path}"
        );
        assert!(hash.is_some(), "{path}: the root has a node_hash");
    }
    let first = crate::tree_root::DerivedFrom::read(&s.conn)
        .unwrap()
        .expect("a parse stamps derived_from")
        .tree_root;
    assert_eq!(first, cold_root(&files));
    let images: Vec<_> = files.iter().map(|(p, _)| ast_rows(&s.conn, p)).collect();

    let again = crate::cmd_parse::parse_into_conn(&s.conn, s.src.path(), None, None).unwrap();
    assert_eq!(again.tree_root, first, "a reparse lands the same tree root");
    assert_eq!(again.errors, 0);
    let after: Vec<_> = files.iter().map(|(p, _)| ast_rows(&s.conn, p)).collect();
    assert_eq!(after, images, "a reparse keeps every file's rows");

    // A file emptied on disk reparses to the empty file's projection.
    std::fs::write(s.src.path().join("a.rs"), "").unwrap();
    let emptied = crate::cmd_parse::parse_into_conn(&s.conn, s.src.path(), None, None).unwrap();
    assert_empty_file(&s, "a.rs");
    assert_eq!(ast_rows(&s.conn, "a.rs"), ast_rows(&s.conn, "e.rs"));
    let mut now = files.to_vec();
    now.retain(|(p, _)| *p != "a.rs");
    now.push(("a.rs", ""));
    assert_eq!(emptied.tree_root, cold_root(&now));
}

/// The three-way merge over an empty side, directly. Every change from an
/// empty base is an insertion at line 0, so two different ones always
/// overlap there; an empty side that removes every line conflicts with
/// any edit of those lines; both sides emptying the file merge to empty.
#[test]
fn merges_over_an_empty_side() {
    let merge = |base: &str, ours: &str, theirs: &str| {
        crate::merge::merge(base.as_bytes(), ours.as_bytes(), theirs.as_bytes())
    };
    let at = |start, end| LineRange { start, end };
    // Base empty.
    assert_eq!(merge("", "x\n", "y\n"), Merged::Conflict(vec![at(0, 0)]));
    assert_eq!(merge("", "x\n", "x\n"), Merged::Clean(b"x\n".to_vec()));
    assert_eq!(merge("", "x\n", ""), Merged::Clean(b"x\n".to_vec()));
    assert_eq!(merge("", "", "y\n"), Merged::Clean(b"y\n".to_vec()));
    assert_eq!(merge("", "", ""), Merged::Clean(Vec::new()));
    // A side empty: it deleted every base line.
    assert_eq!(
        merge("a\nb\n", "", "a\nB\n"),
        Merged::Conflict(vec![at(0, 2)])
    );
    assert_eq!(
        merge("a\nb\n", "a\nB\n", ""),
        Merged::Conflict(vec![at(0, 2)])
    );
    assert_eq!(merge("a\nb\n", "", ""), Merged::Clean(Vec::new()));
    assert_eq!(merge("a\nb\n", "", "a\nb\n"), Merged::Clean(Vec::new()));
    assert_eq!(merge("a\nb\n", "a\nb\n", ""), Merged::Clean(Vec::new()));
}

/// The merge through `propose`: an empty base under two different writes
/// is a conflict at line 0 and lands nothing; the same write over it is
/// accepted; a write that empties a file another writer edited conflicts
/// over the lines it removed; and two writers emptying one file land the
/// empty file.
#[test]
fn a_moved_base_with_an_empty_side_merges_or_refuses_typed() {
    // Base empty, theirs and ours both non-empty and different.
    let s = store(&[("e.txt", ""), ("a.go", A)]);
    propose_whole(
        &s,
        vec![Proposed::write(
            "e.txt",
            Base::Blob(empty()),
            "theirs\n".into(),
        )],
    )
    .unwrap();
    let before = s.conn.serialize("main").unwrap().to_vec();
    let Refusal::Conflict { moved, conflicts } = refused(
        propose_whole(
            &s,
            vec![Proposed::write(
                "e.txt",
                Base::Blob(empty()),
                "ours\n".into(),
            )],
        )
        .unwrap_err(),
    ) else {
        panic!("two insertions into an empty base conflict");
    };
    assert_eq!(
        moved,
        vec![Stale {
            path: "e.txt".into(),
            expected: Some(empty()),
            current: Some(h("theirs\n")),
        }]
    );
    assert_eq!(
        conflicts,
        BTreeMap::from([("e.txt".to_string(), vec![LineRange { start: 0, end: 0 }])])
    );
    assert_eq!(
        s.conn.serialize("main").unwrap().to_vec(),
        before,
        "a refused proposal changes nothing"
    );
    // The same bytes over the moved empty base merge to them.
    let same = propose_whole(
        &s,
        vec![Proposed::write(
            "e.txt",
            Base::Blob(empty()),
            "theirs\n".into(),
        )],
    )
    .unwrap();
    assert!(same.merged.is_empty());
    assert_eq!(stored(&s, "e.txt").as_deref(), Some(b"theirs\n".as_slice()));

    // Ours empty, theirs an edit of a line ours removed.
    let s = store(&[("f.txt", "a\nb\n")]);
    propose_whole(
        &s,
        vec![Proposed::write(
            "f.txt",
            Base::Blob(h("a\nb\n")),
            "a\nB\n".into(),
        )],
    )
    .unwrap();
    let Refusal::Conflict { conflicts, .. } = refused(
        propose_whole(
            &s,
            vec![Proposed::write(
                "f.txt",
                Base::Blob(h("a\nb\n")),
                Vec::new(),
            )],
        )
        .unwrap_err(),
    ) else {
        panic!("emptying a file another writer edited conflicts");
    };
    assert_eq!(
        conflicts,
        BTreeMap::from([("f.txt".to_string(), vec![LineRange { start: 0, end: 2 }])])
    );
    assert_eq!(stored(&s, "f.txt").as_deref(), Some(b"a\nB\n".as_slice()));

    // Theirs empty, ours an edit of a line theirs removed.
    let s = store(&[("f.txt", "a\nb\n")]);
    propose_whole(
        &s,
        vec![Proposed::write(
            "f.txt",
            Base::Blob(h("a\nb\n")),
            Vec::new(),
        )],
    )
    .unwrap();
    let Refusal::Conflict { .. } = refused(
        propose_whole(
            &s,
            vec![Proposed::write(
                "f.txt",
                Base::Blob(h("a\nb\n")),
                "a\nB\n".into(),
            )],
        )
        .unwrap_err(),
    ) else {
        panic!("editing a file another writer emptied conflicts");
    };
    assert_eq!(stored(&s, "f.txt"), Some(Vec::new()));

    // Both writers emptied it: the empty file lands, as a write.
    let accepted = propose_whole(
        &s,
        vec![Proposed::write(
            "f.txt",
            Base::Blob(h("a\nb\n")),
            Vec::new(),
        )],
    )
    .unwrap();
    assert_eq!(accepted.blobs["f.txt"], empty());
    assert!(accepted.merged.is_empty(), "what landed is what was sent");
    assert_empty_file(&s, "f.txt");
    assert_eq!(accepted.tree_root, cold_root(&[("f.txt", "")]));
}
