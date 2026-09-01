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
use rusqlite::Connection;

/// Verify `all_file_contents` returns exactly the file nodes with non-empty
/// content via the optimized single-query path in SqliteGraphAdapter.
#[test]
fn all_file_contents_bulk_query() -> Result<()> {
    let source = Connection::open_in_memory()?;
    create_schema(&source)?;
    // A node's PATH is not stored under projection-v5; `all_file_contents`
    // renders each one through `v_node_path`, so the fixture has to intern
    // the components those paths are built from.
    for dir in ["a", "a/b"] {
        let dir_id = leyline_schema::intern_dir_chain(&source, dir)?;
        let name = dir.rsplit('/').next().unwrap();
        let name_id = leyline_schema::intern_name(&source, name)?;
        let parent = leyline_schema::intern_dir_chain(
            &source,
            dir.rsplit_once('/').map(|(p, _)| p).unwrap_or(""),
        )?;
        leyline_schema::insert_node(
            &source,
            leyline_schema::dir_nid(dir_id),
            Some(leyline_schema::dir_nid(parent)),
            Some(name_id),
            None,
            1,
            0,
            0,
            1000,
            "",
        )?;
    }
    for (path, mtime, record) in [
        ("a/b/c", 2000, Some("hello")),
        ("a/b/d", 3000, None),
        ("a/b/e", 4000, Some("")),
        ("a/f", 5000, Some("world hello")),
    ] {
        let file_id = leyline_schema::ensure_file_id(&source, path)?;
        let dir_id = leyline_schema::ensure_dir_nodes(&source, path, mtime)?;
        let name_id = leyline_schema::intern_name(&source, path.rsplit('/').next().unwrap())?;
        source.execute(
            "INSERT INTO nodes (nid, parent_nid, name_id, kind, ord, size, mtime, record) \
             VALUES (?1, ?2, ?3, 0, 0, ?4, ?5, ?6)",
            rusqlite::params![
                leyline_schema::file_nid(file_id, 0),
                leyline_schema::dir_nid(dir_id),
                name_id,
                record.map(|r| r.len() as i64).unwrap_or(0),
                mtime,
                record
            ],
        )?;
    }
    let data = source.serialize("main")?;
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
