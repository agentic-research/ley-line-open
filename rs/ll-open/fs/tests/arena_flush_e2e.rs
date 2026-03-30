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
    ctrl.set_arena(arena_path.to_str().unwrap(), arena_size, 1)?;
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

    // 5. Verify: control block generation was bumped
    let ctrl = Controller::open_or_create(&ctrl_path)?;
    assert_eq!(ctrl.generation(), 2, "generation should bump from 1 to 2");
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
fn double_flush_increments_generation() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let ctrl_path = dir.path().join("test.ctrl");
    let arena_path = dir.path().join("test.arena");

    let arena_size: u64 = 4096 + 65536 * 2;

    let mut mmap = layout::create_arena(&arena_path, arena_size)?;
    let db_bytes = seed_db();
    layout::write_to_arena(&mut mmap, &db_bytes)?;
    drop(mmap);

    let mut ctrl = Controller::open_or_create(&ctrl_path)?;
    ctrl.set_arena(arena_path.to_str().unwrap(), arena_size, 1)?;
    drop(ctrl);

    let graph = HotSwapGraph::new(ctrl_path.clone())?.with_writable();

    // First write + flush
    graph.truncate("docs/readme")?;
    graph.write_content("docs/readme", b"v2", 0)?;
    graph.flush_to_arena()?;

    let ctrl = Controller::open_or_create(&ctrl_path)?;
    assert_eq!(ctrl.generation(), 2);
    drop(ctrl);

    // Second write + flush
    graph.truncate("docs/readme")?;
    graph.write_content("docs/readme", b"v3", 0)?;
    graph.flush_to_arena()?;

    let ctrl = Controller::open_or_create(&ctrl_path)?;
    assert_eq!(ctrl.generation(), 3);
    drop(ctrl);

    // Fresh reader sees latest
    let fresh = SqliteGraphAdapter::from_arena(&ctrl_path)?;
    let mut buf = [0u8; 64];
    let n = fresh.read_content("docs/readme", &mut buf, 0)?;
    assert_eq!(&buf[..n], b"v3");

    Ok(())
}
