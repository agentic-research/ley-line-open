//! Integration test: bulk-content lookup via SqliteGraphAdapter.
//!
//! Originally this file also covered the on-disk `VectorIndex` sidecar, but
//! that has moved to `leyline-cli-lib::daemon::vec_index` (closer to the
//! enrichment pipeline). The remaining test still exercises the FS adapter's
//! optimized `all_file_contents` query.

use anyhow::Result;
use leyline_fs::SqliteGraph;
use leyline_fs::graph::{Graph, SqliteGraphAdapter};
use leyline_schema::create_schema;
use rusqlite::{Connection, DatabaseName};

/// Verify `all_file_contents` returns exactly the file nodes with non-empty
/// content via the optimized single-query path in SqliteGraphAdapter.
#[test]
fn all_file_contents_bulk_query() -> Result<()> {
    let source = Connection::open_in_memory()?;
    create_schema(&source)?;
    source.execute_batch(
        "INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('a', '', 'a', 1, 0, 1000, NULL);
         INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('a/b', 'a', 'b', 1, 0, 1000, NULL);
         INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('a/b/c', 'a/b', 'c', 0, 5, 2000, 'hello');
         INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('a/b/d', 'a/b', 'd', 0, 0, 3000, NULL);
         INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('a/b/e', 'a/b', 'e', 0, 0, 4000, '');
         INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('a/f', 'a', 'f', 0, 11, 5000, 'world hello');",
    )?;
    let data = source.serialize(DatabaseName::Main)?;
    let graph = SqliteGraph::from_bytes(data.as_ref())?;
    let adapter = SqliteGraphAdapter::new(graph);

    let contents = adapter.all_file_contents()?;

    // Should return only nodes with kind=0 AND non-empty record:
    // a/b/c ("hello"), a/f ("world hello")
    // NOT: a (dir), a/b (dir), a/b/d (NULL record), a/b/e (empty string)
    assert_eq!(contents.len(), 2);
    let ids: Vec<&str> = contents.iter().map(|(id, _)| id.as_str()).collect();
    assert!(ids.contains(&"a/b/c"));
    assert!(ids.contains(&"a/f"));

    // Verify content is correct
    for (id, content) in &contents {
        match id.as_str() {
            "a/b/c" => assert_eq!(content, "hello"),
            "a/f" => assert_eq!(content, "world hello"),
            _ => panic!("unexpected node: {id}"),
        }
    }

    Ok(())
}
