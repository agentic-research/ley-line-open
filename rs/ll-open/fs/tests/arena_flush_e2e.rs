//! E2E test: write through Graph → flush_to_arena → fresh reader sees writes.
//!
//! Proves the serialize→arena→external-read path that mache depends on,
//! without requiring an NFS mount (no root needed in CI).

use anyhow::Result;
use rusqlite::{Connection, DatabaseName};

use leyline_core::{Controller, layout};
use leyline_fs::graph::{Graph, HotSwapGraph, SqliteGraphAdapter};
use leyline_schema::create_schema;

/// Create a minimal nodes-table SQLite DB and return its serialized bytes.
fn seed_db() -> Vec<u8> {
    let conn = Connection::open_in_memory().unwrap();
    create_schema(&conn).unwrap();
    conn.execute_batch(
        "INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('docs', '', 'docs', 1, 0, 1000, NULL);
        INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('docs/readme', 'docs', 'readme', 0, 5, 2000, 'hello');",
    )
    .unwrap();
    conn.serialize(DatabaseName::Main).unwrap().to_vec()
}

#[test]
fn flush_round_trip() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let ctrl_path = dir.path().join("test.ctrl");
    let arena_path = dir.path().join("test.arena");

    // Each buffer must hold a serialized SQLite DB (~16KB+)
    let arena_size: u64 = 4096 + 65536 * 2;

    // 1. Create arena + control block
    let mut mmap = layout::create_arena(&arena_path, arena_size)?;
    let db_bytes = seed_db();
    layout::write_to_arena(&mut mmap, &db_bytes)?;
    drop(mmap);

    let mut ctrl = Controller::open_or_create(&ctrl_path)?;
    // T2.4: publish initial root so HotSwapGraph::new reads from arena
    // (zero-root sentinel would serve an empty MemoryGraph instead).
    let initial_root: [u8; 32] = blake3::hash(&db_bytes).into();
    ctrl.set_arena_with_root(arena_path.to_str().unwrap(), arena_size, initial_root)?;
    drop(ctrl);

    // 2. Open HotSwapGraph in writable mode — deserializes from arena
    let graph = HotSwapGraph::new(ctrl_path.clone())?.with_writable();

    // Verify initial data
    let mut buf = [0u8; 64];
    let n = graph.read_content("docs/readme", &mut buf, 0)?;
    assert_eq!(&buf[..n], b"hello");

    // 3. Write new content through Graph trait
    graph.write_content("docs/readme", b"modified", 0)?;
    graph.create_node("docs", "new.txt", false)?;
    graph.write_content("docs/new.txt", b"brand new file", 0)?;

    // 4. Flush to arena
    graph.flush_to_arena()?;

    // 5. T2.4: verify current_root advanced — content changed → root changed.
    let ctrl = Controller::open_or_create(&ctrl_path)?;
    let post_flush_root = ctrl.current_root();
    assert_ne!(
        post_flush_root, initial_root,
        "T2.4: flush must advance current_root (content changed)"
    );
    assert_ne!(
        post_flush_root, [0u8; 32],
        "T2.4: post-flush root must not be the zero sentinel"
    );
    drop(ctrl);

    // 6. Verify: fresh SqliteGraphAdapter from arena sees the writes
    let fresh = SqliteGraphAdapter::from_arena(&ctrl_path)?;

    let mut buf = [0u8; 64];
    let n = fresh.read_content("docs/readme", &mut buf, 0)?;
    assert_eq!(
        &buf[..n],
        b"modified",
        "fresh reader should see modified content"
    );

    let node = fresh
        .get_node("docs/new.txt")?
        .expect("new.txt should exist in fresh reader");
    assert_eq!(node.name, "new.txt");
    assert!(!node.is_dir);

    let n = fresh.read_content("docs/new.txt", &mut buf, 0)?;
    assert_eq!(&buf[..n], b"brand new file");

    // 7. Verify: HotSwapGraph still works after flush (no spurious re-open)
    let n = graph.read_content("docs/readme", &mut buf, 0)?;
    assert_eq!(
        &buf[..n],
        b"modified",
        "HotSwapGraph should still work after flush"
    );

    let children = graph.list_children("docs")?;
    assert_eq!(children.len(), 2, "should have readme + new.txt");

    Ok(())
}

#[test]
fn double_flush_advances_root() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let ctrl_path = dir.path().join("test.ctrl");
    let arena_path = dir.path().join("test.arena");

    let arena_size: u64 = 4096 + 65536 * 2;

    let mut mmap = layout::create_arena(&arena_path, arena_size)?;
    let db_bytes = seed_db();
    layout::write_to_arena(&mut mmap, &db_bytes)?;
    drop(mmap);

    let mut ctrl = Controller::open_or_create(&ctrl_path)?;
    let initial_root: [u8; 32] = blake3::hash(&db_bytes).into();
    ctrl.set_arena_with_root(arena_path.to_str().unwrap(), arena_size, initial_root)?;
    drop(ctrl);

    let graph = HotSwapGraph::new(ctrl_path.clone())?.with_writable();

    // First write + flush — root advances away from initial.
    graph.truncate("docs/readme")?;
    graph.write_content("docs/readme", b"v2", 0)?;
    graph.flush_to_arena()?;

    let root_v2 = Controller::open_or_create(&ctrl_path)?.current_root();
    assert_ne!(root_v2, initial_root, "v2 root differs from initial");

    // Second write + flush — root advances again.
    graph.truncate("docs/readme")?;
    graph.write_content("docs/readme", b"v3", 0)?;
    graph.flush_to_arena()?;

    let root_v3 = Controller::open_or_create(&ctrl_path)?.current_root();
    assert_ne!(root_v3, root_v2, "v3 root differs from v2");
    assert_ne!(root_v3, initial_root, "v3 root differs from initial");

    // Fresh reader sees latest
    let fresh = SqliteGraphAdapter::from_arena(&ctrl_path)?;
    let mut buf = [0u8; 64];
    let n = fresh.read_content("docs/readme", &mut buf, 0)?;
    assert_eq!(&buf[..n], b"v3");

    Ok(())
}
