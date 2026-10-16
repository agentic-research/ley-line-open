//! ADR-0045 T8 through the host mount and the real writer: a mount edit is
//! a proposal against the base its handle read, decided as storefs's FLUSH
//! decides it. It lands what the same proposal lands (`_tree`, `treeRoot`,
//! diagnostics); a moved base merges at the flush; overlapping edits are
//! refused; an unparseable save lands with a diagnostic. The structural
//! operations the projection presents are proposals; the ones it cannot
//! present are refused.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use leyline_core::{ContentAddressed, Hash};
use leyline_fs::graph::{
    Graph, Handle, MOUNT_HANDLE, RefusalKind, SqliteGraphAdapter, refusal_kind,
};
use rusqlite::Connection;
use tempfile::TempDir;

use super::*;
use crate::tree::Entry;

/// `// one` keeps `func A`'s line apart from a function another writer
/// inserts above it, so the two edits do not touch one base line.
const A: &str = "package a\n\n// one\nfunc A() int { return 1 }\n\nfunc B() int { return 2 }\n";
const B: &str = "package pkg\n\nfunc C() int { return 3 }\n";
/// Another writer's change to `A`: a function above `func A`, which moves
/// every later `function_declaration` of the projection one place on.
const A_SHIFTED: &str = "package a\n\nfunc Z() int { return 0 }\n// one\nfunc A() int { return 1 }\n\nfunc B() int { return 2 }\n";

/// One cold parse of `a.go` and `pkg/b.go`: the image both sides start from.
struct Image {
    _src: TempDir,
    dir: TempDir,
    bytes: Vec<u8>,
}

fn image() -> Image {
    let src = TempDir::new().unwrap();
    std::fs::write(src.path().join("a.go"), A).unwrap();
    std::fs::create_dir_all(src.path().join("pkg")).unwrap();
    std::fs::write(src.path().join("pkg/b.go"), B).unwrap();
    let dir = TempDir::new().unwrap();
    let conn = Connection::open(dir.path().join("cold.db")).unwrap();
    crate::cmd_parse::parse_into_conn(&conn, src.path(), None, None).unwrap();
    let bytes = conn.serialize("main").unwrap().to_vec();
    Image {
        _src: src,
        dir,
        bytes,
    }
}

impl Image {
    /// The host mount over a copy of the store, with the writer the daemon
    /// and `serve` install.
    fn mount(&self) -> SqliteGraphAdapter {
        let mut graph = SqliteGraphAdapter::new_writable(&self.bytes).unwrap();
        graph.set_source_writer(Arc::new(MountWriter {
            grant: Grant::Whole,
            checkout: None,
        }));
        graph
    }

    /// Another copy of the store: the one storefs proposes to.
    fn store(&self) -> Connection {
        self.open_copy("store.db", &self.bytes)
    }

    fn open_copy(&self, name: &str, bytes: &[u8]) -> Connection {
        let path: PathBuf = self.dir.path().join(name);
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, bytes).unwrap();
        Connection::open(path).unwrap()
    }

    /// What the mount's store holds.
    fn mounted(&self, graph: &SqliteGraphAdapter) -> State {
        state(&self.open_copy("mounted.db", &graph.serialize().unwrap()))
    }
}

/// What two writers must agree on: the manifest, its `treeRoot`, and the
/// parse's diagnostics.
#[derive(Debug, PartialEq)]
struct State {
    tree: BTreeMap<String, Entry>,
    tree_root: Hash,
    diagnostics: Vec<(String, Vec<u8>, bool, Option<i64>, String)>,
}

fn state(conn: &Connection) -> State {
    let mut stmt = conn
        .prepare(
            "SELECT path, blob_hash, projected, start_byte, message \
               FROM _parse_diagnostics ORDER BY path",
        )
        .unwrap();
    let diagnostics = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    State {
        tree: crate::tree::entries(conn).unwrap(),
        tree_root: crate::tree_root::DerivedFrom::read(conn)
            .unwrap()
            .unwrap()
            .tree_root,
        diagnostics,
    }
}

/// The text of `path` in the store `state` describes.
fn text(conn: &Connection, path: &str) -> String {
    let blob = crate::tree::entry(conn, path)
        .unwrap()
        .and_then(Entry::blob)
        .unwrap();
    String::from_utf8(crate::tree::blob_bytes(conn, blob).unwrap().unwrap()).unwrap()
}

fn mounted_text(img: &Image, graph: &SqliteGraphAdapter, path: &str) -> String {
    text(
        &img.open_copy("mounted.db", &graph.serialize().unwrap()),
        path,
    )
}

fn read(graph: &dyn Graph, handle: Handle, id: &str) -> String {
    let mut buf = vec![0u8; 4096];
    let n = graph.read_content_as(handle, id, &mut buf, 0).unwrap();
    String::from_utf8(buf[..n].to_vec()).unwrap()
}

/// The leaf under `under` whose bytes are `token`, found as a client walks
/// the mount.
fn leaf(graph: &dyn Graph, under: &str, token: &str) -> String {
    let mut stack = vec![under.to_string()];
    while let Some(id) = stack.pop() {
        for child in graph.list_children(&id).unwrap() {
            if child.is_dir {
                stack.push(child.id);
            } else if read(graph, MOUNT_HANDLE, &child.id) == token {
                return child.id;
            }
        }
    }
    panic!("no leaf under {under} holds {token:?}")
}

/// What an editor's save sends: truncate, then write.
fn save(graph: &dyn Graph, handle: Handle, id: &str, text: &str) -> anyhow::Result<()> {
    graph.truncate_as(handle, id)?;
    graph.write_content_as(handle, id, text.as_bytes(), 0)?;
    Ok(())
}

fn blob(text: &str) -> Base {
    Base::Blob(text.as_bytes().hash())
}

fn storefs(conn: &Connection, writes: Vec<Proposed>) {
    propose(conn, &Grant::Whole, writes).unwrap();
}

fn assert_refused(err: &anyhow::Error, kind: RefusalKind) {
    assert_eq!(refusal_kind(err), Some(kind), "{err:#}");
}

/// The bytes of `A` once its `return 1` reads `to`.
fn a_returning(to: &str) -> String {
    A.replacen("return 1", &format!("return {to}"), 1)
}

/// T8: through a write handle, a clean edit, an unparseable edit, and an
/// edit whose base another writer moved (shifting the projection's paths
/// under it) each land in the mount's store exactly what the same
/// proposal lands through storefs: one `_tree`, one `treeRoot`, one set of
/// diagnostics.
#[test]
fn a_mount_edit_lands_what_the_same_storefs_edit_lands() {
    let cases: [(&str, Option<&str>, &str); 3] = [
        ("a clean edit", None, "7"),
        ("an unparseable edit", None, "1 +"),
        ("an edit whose base moved", Some(A_SHIFTED), "7"),
    ];
    for (case, moved, token) in cases {
        let img = image();
        let graph = img.mount();
        let store = img.store();
        let one = leaf(&graph, "a.go", "1");
        let h: Handle = 7;
        graph.note_base_as(h, &one).unwrap();
        if let Some(theirs) = moved {
            storefs(
                &store,
                vec![Proposed::write(
                    "a.go",
                    Base::Any,
                    theirs.as_bytes().to_vec(),
                )],
            );
            land(&graph, "a.go", theirs);
        }
        save(&graph, h, &one, token).unwrap_or_else(|e| panic!("{case}: {e:#}"));
        graph
            .flush_node_as(h, &one)
            .unwrap_or_else(|e| panic!("{case}: {e:#}"));
        storefs(
            &store,
            vec![Proposed::write(
                "a.go",
                blob(A),
                a_returning(token).into_bytes(),
            )],
        );
        assert_eq!(img.mounted(&graph), state(&store), "{case}");
        let landed = text(&store, "a.go");
        assert!(
            landed.contains(&format!("return {token}")),
            "{case}: {landed}"
        );
        if moved.is_some() {
            assert!(
                landed.contains("func Z() int { return 0 }"),
                "{case}: {landed}"
            );
            // The mount serves the merged file's projection: the edit is
            // the node it was made to, under that node's path now.
            leaf(&graph, "a.go", token);
            assert_eq!(read(&graph, h, &one), "0", "{case}: the path names Z's now");
        }
    }
}

/// Land `bytes` as the whole of `path` in the mount's store as another
/// writer does: a splice of the file's root node, which reaches the write
/// path without passing through a handle.
fn land(graph: &SqliteGraphAdapter, path: &str, bytes: &str) {
    graph
        .batch_splice(&[(path.to_string(), Some(bytes.to_string()))])
        .unwrap();
}

/// T8, T6: the NFS client's edit (the mount handle) to a file another
/// writer moved on other lines since the client READ it is staged at its
/// WRITE and merged at its COMMIT; it is not refused.
#[test]
fn a_disjoint_edit_through_the_mount_handle_merges_at_its_commit() {
    let img = image();
    let graph = img.mount();
    let one = leaf(&graph, "a.go", "1");
    graph.note_read(&one).unwrap();
    let moved = A.replacen("return 2", "return 5", 1);
    land(&graph, "a.go", &moved);
    graph.write_content(&one, b"7", 0).unwrap();
    graph.flush_node(&one).unwrap();
    assert_eq!(
        mounted_text(&img, &graph, "a.go"),
        moved.replacen("return 1", "return 7", 1)
    );
}

/// T5 through the mount: an edit that overlaps another writer's change is
/// refused `Stale`. A write handle's refusal comes at its flush; the mount
/// handle's at its WRITE, the request a macOS NFS client reports. The
/// store keeps the other writer's bytes.
#[test]
fn an_overlapping_edit_through_the_mount_is_refused_stale() {
    let theirs = a_returning("9");
    for handle in [7, MOUNT_HANDLE] {
        let img = image();
        let graph = img.mount();
        let one = leaf(&graph, "a.go", "1");
        if handle == MOUNT_HANDLE {
            graph.note_read(&one).unwrap();
        } else {
            graph.note_base_as(handle, &one).unwrap();
        }
        land(&graph, "a.go", &theirs);
        let err = match save(&graph, handle, &one, "7") {
            Err(refused) => {
                assert_eq!(handle, MOUNT_HANDLE, "a write handle stages: {refused:#}");
                refused
            }
            Ok(()) => {
                assert_ne!(
                    handle, MOUNT_HANDLE,
                    "the mount handle refuses at its WRITE"
                );
                graph.flush_node_as(handle, &one).unwrap_err()
            }
        };
        assert_refused(&err, RefusalKind::Stale);
        assert_eq!(mounted_text(&img, &graph, "a.go"), theirs);
    }
}

/// T3 through the mount: a save that leaves the file unparseable lands,
/// and the store records the file's diagnostic.
#[test]
fn an_unparseable_mount_save_lands_with_a_diagnostic() {
    let img = image();
    let graph = img.mount();
    let one = leaf(&graph, "a.go", "1");
    graph.note_base_as(7, &one).unwrap();
    save(&graph, 7, &one, "1 +").unwrap();
    graph.flush_node_as(7, &one).unwrap();
    let broken = a_returning("1 +");
    let conn = img.open_copy("mounted.db", &graph.serialize().unwrap());
    assert_eq!(text(&conn, "a.go"), broken);
    let d = crate::tree::diagnostic(&conn, "a.go").unwrap().unwrap();
    assert!(d.projected, "{d:?}");
    assert_eq!(d.blob, broken.as_bytes().hash());
}

/// T8: the projection presents a file as a directory of its syntax nodes,
/// so a new regular file, an empty directory and a new name for a syntax
/// node are nothing it can show: each is refused and the store is
/// unchanged.
#[test]
fn the_mount_refuses_what_its_projection_cannot_present() {
    let img = image();
    let graph = img.mount();
    let before = img.mounted(&graph);
    let one = leaf(&graph, "a.go", "1");
    let parent = one.rsplit_once('/').unwrap().0.to_string();
    for (what, result) in [
        ("a file", graph.create_node("", "new.go", false).map(drop)),
        (
            "a directory",
            graph.create_node("pkg", "empty", true).map(drop),
        ),
        (
            "a syntax node's name",
            graph.rename_node(&one, &parent, "x"),
        ),
        (
            "a file under a syntax node",
            graph.create_node(&parent, "x", false).map(drop),
        ),
    ] {
        let err = result.expect_err(what);
        assert_refused(&err, RefusalKind::NotInProjection);
    }
    assert_eq!(img.mounted(&graph), before);
}

/// T8: removing a source file's entry deletes the file; removing a syntax
/// node writes its file without the node's bytes; a directory with files
/// under it is not removed. Each lands what the same storefs proposal
/// lands.
#[test]
fn removals_through_the_mount_are_proposals() {
    let img = image();
    let graph = img.mount();
    let store = img.store();
    graph.remove_node("pkg/b.go").unwrap();
    storefs(&store, vec![Proposed::delete("pkg/b.go", blob(B))]);
    assert_eq!(img.mounted(&graph), state(&store));

    let decl_b = leaf(&graph, "a.go", "2")
        .split('/')
        .take(2)
        .collect::<Vec<_>>()
        .join("/");
    graph.remove_node(&decl_b).unwrap();
    let without_b = A.replacen("func B() int { return 2 }", "", 1);
    storefs(
        &store,
        vec![Proposed::write("a.go", blob(A), without_b.into_bytes())],
    );
    assert_eq!(img.mounted(&graph), state(&store));

    let img = image();
    let graph = img.mount();
    let err = graph.remove_node("pkg").unwrap_err();
    assert_refused(&err, RefusalKind::NotEmpty);
}

/// T8: renaming a source file's entry moves the file, and renaming a
/// directory moves everything under it, as storefs's RENAME does.
#[test]
fn renames_through_the_mount_are_proposals() {
    let img = image();
    let graph = img.mount();
    let store = img.store();
    graph.rename_node("a.go", "pkg", "c.go").unwrap();
    storefs(
        &store,
        vec![
            Proposed::delete("a.go", blob(A)),
            Proposed::write("pkg/c.go", Base::Absent, A.as_bytes().to_vec()),
        ],
    );
    assert_eq!(img.mounted(&graph), state(&store));

    graph.rename_node("pkg", "", "lib").unwrap();
    let writes = rename_dir(&store, "pkg", "lib").unwrap();
    storefs(&store, writes);
    assert_eq!(img.mounted(&graph), state(&store));
    assert_eq!(
        read(&graph, MOUNT_HANDLE, &leaf(&graph, "lib/c.go", "1")),
        "1"
    );
}
