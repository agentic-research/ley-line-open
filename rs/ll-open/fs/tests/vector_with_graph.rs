//! Integration test: build a graph + vector index, search for node_ids,
//! resolve them against the graph to get node names.

#![cfg(feature = "vec")]

use anyhow::Result;
use leyline_fs::SqliteGraph;
use leyline_fs::graph::{Graph, SqliteGraphAdapter};
use leyline_fs::vector::{VectorIndex, register_vec};
use leyline_schema::create_schema;
use rusqlite::{Connection, DatabaseName};

fn setup_graph() -> Result<SqliteGraphAdapter> {
    let source = Connection::open_in_memory()?;
    create_schema(&source)?;
    source.execute_batch(
        "INSERT INTO nodes VALUES ('docs', '', 'docs', 1, 0, 1000, NULL);
         INSERT INTO nodes VALUES ('docs/intro', 'docs', 'intro', 0, 14, 2000, 'Introduction');
         INSERT INTO nodes VALUES ('docs/setup', 'docs', 'setup', 0, 11, 3000, 'Setup guide');
         INSERT INTO nodes VALUES ('docs/api', 'docs', 'api', 0, 13, 4000, 'API reference');",
    )?;
    let data = source.serialize(DatabaseName::Main)?;
    let graph = SqliteGraph::from_bytes(data.as_ref())?;
    Ok(SqliteGraphAdapter::new(graph))
}

#[test]
fn vector_index_with_graph() -> Result<()> {
    register_vec();

    let adapter = setup_graph()?;
    let idx = VectorIndex::new(4, None)?;

    // Simulate embeddings for each document node
    idx.insert("docs/intro", &[1.0, 0.0, 0.0, 0.0])?;
    idx.insert("docs/setup", &[0.0, 1.0, 0.0, 0.0])?;
    idx.insert("docs/api", &[0.8, 0.2, 0.0, 0.0])?;

    // Search for nearest to [1, 0, 0, 0] — should find intro first, then api
    let results = idx.search(&[1.0, 0.0, 0.0, 0.0], 3)?;
    assert_eq!(results.len(), 3);

    // Resolve node_ids against the graph
    let mut resolved = Vec::new();
    for (node_id, distance) in &results {
        let node = adapter.get_node(node_id)?.unwrap();
        resolved.push((node.name.clone(), *distance));
    }

    assert_eq!(resolved[0].0, "intro");
    assert!(resolved[0].1 < f64::EPSILON); // exact match
    assert_eq!(resolved[1].0, "api");
    assert_eq!(resolved[2].0, "setup");

    // Verify we can read content through the graph for the top result
    let mut buf = [0u8; 256];
    let n = adapter.read_content(&results[0].0, &mut buf, 0)?;
    assert_eq!(&buf[..n], b"Introduction");

    Ok(())
}

/// Verify `all_file_contents` returns exactly the file nodes with non-empty
/// content via the optimized single-query path in SqliteGraphAdapter.
#[test]
fn all_file_contents_bulk_query() -> Result<()> {
    let source = Connection::open_in_memory()?;
    create_schema(&source)?;
    source.execute_batch(
        "INSERT INTO nodes VALUES ('a', '', 'a', 1, 0, 1000, NULL);
         INSERT INTO nodes VALUES ('a/b', 'a', 'b', 1, 0, 1000, NULL);
         INSERT INTO nodes VALUES ('a/b/c', 'a/b', 'c', 0, 5, 2000, 'hello');
         INSERT INTO nodes VALUES ('a/b/d', 'a/b', 'd', 0, 0, 3000, NULL);
         INSERT INTO nodes VALUES ('a/b/e', 'a/b', 'e', 0, 0, 4000, '');
         INSERT INTO nodes VALUES ('a/f', 'a', 'f', 0, 11, 5000, 'world hello');",
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
