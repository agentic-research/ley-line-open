//! Reader pool concurrency tests for SqliteGraphAdapter.
//!
//! TDD: these tests define the behavior of the reader pool pattern.
//! Reads use a lock-free pool of SqliteGraph instances; writes go
//! through a single writer Mutex. After each write, cached bytes
//! are refreshed so subsequent readers see mutations.

use std::sync::{Arc, Barrier};
use std::thread;

use anyhow::Result;
use rusqlite::{Connection, DatabaseName};

use leyline_fs::SqliteGraph;
use leyline_fs::graph::{Graph, SqliteGraphAdapter};
use leyline_schema::create_schema;

/// Seed SQL for the common 3-node test dataset.
const SEED_DATA: &str = "\
    INSERT INTO nodes VALUES ('docs', '', 'docs', 1, 0, 1000, NULL);
    INSERT INTO nodes VALUES ('docs/readme', 'docs', 'readme', 0, 5, 2000, 'hello');
    INSERT INTO nodes VALUES ('docs/notes', 'docs', 'notes', 0, 5, 3000, 'world');";

/// Create a writable adapter seeded with test data.
fn test_adapter() -> SqliteGraphAdapter {
    let source = Connection::open_in_memory().unwrap();
    create_schema(&source).unwrap();
    source.execute_batch(SEED_DATA).unwrap();
    let data = source.serialize(DatabaseName::Main).unwrap();
    SqliteGraphAdapter::new_writable(data.as_ref()).unwrap()
}

/// Create a read-only adapter with a specific pool size for exhaustion tests.
fn test_adapter_with_pool(pool_size: usize) -> SqliteGraphAdapter {
    let source = Connection::open_in_memory().unwrap();
    create_schema(&source).unwrap();
    source.execute_batch(SEED_DATA).unwrap();
    let data = source.serialize(DatabaseName::Main).unwrap();
    let graph = SqliteGraph::from_bytes(data.as_ref()).unwrap();
    SqliteGraphAdapter::new_with_pool_size(graph, pool_size)
}

/// Spawn 8 threads all doing reads simultaneously. All must return correct results.
#[test]
fn concurrent_reads_dont_block() {
    let adapter = Arc::new(test_adapter());
    let barrier = Arc::new(Barrier::new(8));
    let mut handles = vec![];

    for _ in 0..8 {
        let adapter = Arc::clone(&adapter);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait(); // all threads start at once

            let node = adapter.get_node("docs/readme").unwrap().unwrap();
            assert_eq!(node.name, "readme");
            assert!(!node.is_dir);

            let mut buf = [0u8; 64];
            let n = adapter.read_content("docs/readme", &mut buf, 0).unwrap();
            assert_eq!(&buf[..n], b"hello");

            let children = adapter.list_children("docs").unwrap();
            assert_eq!(children.len(), 2);

            let child = adapter.lookup_child("docs", "notes").unwrap().unwrap();
            assert_eq!(child.id, "docs/notes");
        }));
    }

    for h in handles {
        h.join().unwrap();
    }
}

/// Start a write on one thread and a read on another simultaneously.
/// The read (on a different node) must succeed without waiting for the write.
#[test]
fn read_during_write_succeeds() {
    let adapter = Arc::new(test_adapter());
    let barrier = Arc::new(Barrier::new(2));

    let adapter_w = Arc::clone(&adapter);
    let barrier_w = Arc::clone(&barrier);
    let writer = thread::spawn(move || {
        barrier_w.wait();
        let data = vec![b'X'; 10_000];
        adapter_w.write_content("docs/readme", &data, 0).unwrap();
    });

    let adapter_r = Arc::clone(&adapter);
    let barrier_r = Arc::clone(&barrier);
    let reader = thread::spawn(move || {
        barrier_r.wait();
        // Read a different node — should not block on the write
        let node = adapter_r.get_node("docs/notes").unwrap().unwrap();
        assert_eq!(node.name, "notes");

        let mut buf = [0u8; 64];
        let n = adapter_r.read_content("docs/notes", &mut buf, 0).unwrap();
        assert_eq!(&buf[..n], b"world");
    });

    writer.join().unwrap();
    reader.join().unwrap();
}

/// Write content through the adapter, then immediately read it back.
/// Proves that refresh_readers makes writes visible to subsequent reads.
#[test]
fn write_visible_to_subsequent_reads() -> Result<()> {
    let adapter = test_adapter();

    // Write new content
    adapter.write_content("docs/readme", b"updated", 0)?;

    // Read it back — must see the new content
    let mut buf = [0u8; 64];
    let n = adapter.read_content("docs/readme", &mut buf, 0)?;
    assert_eq!(&buf[..n], b"updated");

    // Create a node and verify it's visible
    let id = adapter.create_node("docs", "new.txt", false)?;
    assert_eq!(id, "docs/new.txt");
    adapter.write_content("docs/new.txt", b"fresh", 0)?;

    let node = adapter.get_node("docs/new.txt")?.unwrap();
    assert_eq!(node.name, "new.txt");

    let n = adapter.read_content("docs/new.txt", &mut buf, 0)?;
    assert_eq!(&buf[..n], b"fresh");

    // Remove a node and verify it's gone
    adapter.remove_node("docs/notes")?;
    assert!(adapter.get_node("docs/notes")?.is_none());

    // Truncate and verify
    adapter.truncate("docs/readme")?;
    let n = adapter.read_content("docs/readme", &mut buf, 0)?;
    assert_eq!(n, 0);

    Ok(())
}

/// With pool_size=2, spawn 16 threads doing concurrent reads.
/// Reads beyond pool capacity create on-demand readers from cached bytes.
/// All must succeed and return correct results.
#[test]
fn pool_recovers_from_exhaustion() {
    let adapter = Arc::new(test_adapter_with_pool(2));
    let thread_count = 16;
    let barrier = Arc::new(Barrier::new(thread_count));
    let mut handles = vec![];

    for _ in 0..thread_count {
        let adapter = Arc::clone(&adapter);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();

            let node = adapter.get_node("docs/readme").unwrap().unwrap();
            assert_eq!(node.name, "readme");

            let mut buf = [0u8; 64];
            let n = adapter.read_content("docs/readme", &mut buf, 0).unwrap();
            assert_eq!(&buf[..n], b"hello");
        }));
    }

    for h in handles {
        h.join().unwrap();
    }
}

/// Verify that a reader checked out before a write does NOT pollute the
/// pool with stale data after the write completes. The generation counter
/// must cause the stale reader to be discarded, not reused.
#[test]
fn stale_reader_discarded_after_write() -> Result<()> {
    let adapter = Arc::new(test_adapter());

    // Phase 1: read on a background thread, hold it across a write via barrier
    let phase1 = Arc::new(Barrier::new(2));
    let phase2 = Arc::new(Barrier::new(2));

    let a = Arc::clone(&adapter);
    let p1 = Arc::clone(&phase1);
    let p2 = Arc::clone(&phase2);
    let reader_thread = thread::spawn(move || {
        // Pop a reader from the pool (pre-write generation)
        let mut buf = [0u8; 64];
        let n = a.read_content("docs/readme", &mut buf, 0).unwrap();
        assert_eq!(&buf[..n], b"hello");

        // Signal main thread: "I have a reader checked out (now returned to pool)"
        p1.wait();
        // Wait for main thread to complete the write
        p2.wait();

        // The reader we used was returned to pool with gen 0.
        // After the write, generation bumped to 1.
        // This subsequent read must NOT see stale "hello" — it must see "changed".
        let n = a.read_content("docs/readme", &mut buf, 0).unwrap();
        assert_eq!(
            &buf[..n],
            b"changed",
            "read after write must see new data, not stale pool reader"
        );
    });

    // Wait for reader thread to complete its first read (stale reader returned to pool)
    phase1.wait();

    // Write while the stale reader is sitting in the pool
    adapter.write_content("docs/readme", b"changed", 0)?;

    // Release reader thread to do its second read
    phase2.wait();

    reader_thread.join().unwrap();
    Ok(())
}

/// Write → serialize → deserialize → verify. Regression test for the
/// serialize path that arena flush depends on.
#[test]
fn serialize_reflects_writes() -> Result<()> {
    let adapter = test_adapter();

    adapter.write_content("docs/readme", b"modified", 0)?;
    adapter.create_node("docs", "new.txt", false)?;
    adapter.write_content("docs/new.txt", b"brand new", 0)?;

    // Serialize and re-open
    let bytes = adapter.serialize()?;
    let adapter2 = SqliteGraphAdapter::new_writable(&bytes)?;

    // Verify writes survived
    let mut buf = [0u8; 64];
    let n = adapter2.read_content("docs/readme", &mut buf, 0)?;
    assert_eq!(&buf[..n], b"modified");

    let node = adapter2.get_node("docs/new.txt")?.unwrap();
    assert_eq!(node.name, "new.txt");

    let n = adapter2.read_content("docs/new.txt", &mut buf, 0)?;
    assert_eq!(&buf[..n], b"brand new");

    Ok(())
}
