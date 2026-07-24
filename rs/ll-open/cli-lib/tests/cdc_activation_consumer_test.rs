#![cfg(feature = "cdc")]

use leyline_core::{ContentAddressed, Controller};
use leyline_fs::SqliteGraph;
use leyline_fs::chunked::{ContentSource, chunks_touched, read_content_at_traced};
use leyline_fs::graph::{Graph, SqliteGraphAdapter};
use rusqlite::Connection;
use tempfile::TempDir;

#[test]
fn activated_projection_publishes_chunked_4k_reads_to_a_real_arena() {
    let temp = TempDir::new().unwrap();
    let source_dir = temp.path().join("source");
    std::fs::create_dir(&source_dir).unwrap();
    let payload = "abcdef0123456789".repeat(64 * 1024);
    let source = format!("{{\"data\":\"{payload}\"}}\n");
    std::fs::write(source_dir.join("big.json"), &source).unwrap();

    let db_path = temp.path().join("graph.db");
    let conn = Connection::open(&db_path).unwrap();
    let parsed =
        leyline_cli_lib::cmd_parse::parse_into_conn(&conn, &source_dir, Some("json"), None)
            .unwrap();
    assert_eq!(parsed.parsed, 1);
    let (node_id, authoritative): (String, Vec<u8>) = conn
        .query_row(
            "SELECT id, CAST(record AS BLOB)
               FROM nodes
              WHERE kind = 0 AND record IS NOT NULL
              ORDER BY length(record) DESC
              LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(
        authoritative.len() >= 1024 * 1024,
        "fixture must produce a large readable structural leaf"
    );

    let arena_path = temp.path().join("graph.arena");
    let ctrl_path = temp.path().join("graph.ctrl");
    leyline_cli_lib::cmd_serve::setup_arena(&arena_path, 4 * 1024, Some(&ctrl_path)).unwrap();

    let report = leyline_cli_lib::cmd_daemon::activate_cdc_and_snapshot(&conn, &ctrl_path, true)
        .unwrap()
        .expect("CDC was enabled");
    assert!(report.populated_nodes > 0);
    assert_eq!(report.populated_nodes, report.eligible_nodes);

    let graph = SqliteGraph::from_arena(&ctrl_path).unwrap();
    let offset = authoritative.len() / 2;
    let mut actual = vec![0_u8; 4096];
    let (read, content_source) =
        read_content_at_traced(graph.conn(), &node_id, &mut actual, offset as u64).unwrap();
    assert_eq!(read, 4096);
    assert_eq!(actual, authoritative[offset..offset + 4096]);
    assert_eq!(content_source, ContentSource::Chunked);

    let touched = chunks_touched(graph.conn(), &node_id, offset as u64, 4096).unwrap();
    assert!(
        touched <= 2,
        "4 KiB range must touch at most two chunks, touched {touched}"
    );

    let controller = Controller::open_or_create(&ctrl_path).unwrap();
    assert!(
        controller.arena_size() > 4 * 1024,
        "snapshot must grow the deliberately undersized arena"
    );
    let serialized = conn.serialize("main").unwrap();
    let serialized_bytes: &[u8] = serialized.as_ref();
    assert_eq!(
        controller.current_root(),
        *serialized_bytes.hash().as_bytes(),
        "published root must pin the exact activated database bytes"
    );
}

#[test]
fn downstream_nodes_only_projection_survives_the_complete_cdc_lifecycle() {
    let temp = TempDir::new().unwrap();
    let source_dir = temp.path().join("source");
    std::fs::create_dir(&source_dir).unwrap();
    let payload = "0123456789abcdef".repeat(64 * 1024);
    std::fs::write(
        source_dir.join("consumer.json"),
        format!("{{\"data\":\"{payload}\"}}\n"),
    )
    .unwrap();

    let db_path = temp.path().join("consumer.db");
    let conn = Connection::open(&db_path).unwrap();
    leyline_cli_lib::cmd_parse::parse_into_conn(&conn, &source_dir, Some("json"), None).unwrap();
    let private_tables_before: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
              WHERE type = 'table' AND name LIKE 'content_%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        private_tables_before, 0,
        "the downstream producer starts without LLO-private content tables"
    );
    let (node_id, mut model): (String, Vec<u8>) = conn
        .query_row(
            "SELECT id, CAST(record AS BLOB)
               FROM nodes
              WHERE kind = 0 AND record IS NOT NULL
              ORDER BY length(record) DESC
              LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    drop(conn);

    leyline_cli_lib::cmd_cdc::enable_database(
        &db_path,
        leyline_fs::activation::ActivationOptions::default(),
    )
    .unwrap();
    let activated_bytes = std::fs::read(&db_path).unwrap();
    let adapter = SqliteGraphAdapter::new_writable(&activated_bytes).unwrap();

    let edit_offset = model.len() / 2;
    let edit = b"CDC!";
    adapter
        .write_content(&node_id, edit, edit_offset as u64)
        .unwrap();
    model[edit_offset..edit_offset + edit.len()].copy_from_slice(edit);

    let reopened_bytes = adapter.serialize().unwrap();
    let reopened_adapter = SqliteGraphAdapter::new_writable(&reopened_bytes).unwrap();
    let mut actual = vec![0_u8; 4096];
    let range_offset = edit_offset - 2048;
    let read = reopened_adapter
        .read_content(&node_id, &mut actual, range_offset as u64)
        .unwrap();
    assert_eq!(read, actual.len());
    assert_eq!(
        actual,
        model[range_offset..range_offset + actual.len()],
        "serialized/reopened consumer bytes must match the authoritative edit"
    );

    reopened_adapter.remove_node(&node_id).unwrap();
    std::fs::write(&db_path, reopened_adapter.serialize().unwrap()).unwrap();

    let collected =
        leyline_cli_lib::cmd_cdc::gc_database(&db_path, leyline_fs::gc::GcOptions::default())
            .unwrap();
    assert!(
        collected.deleted_chunk_rows > 0,
        "removing through the public graph surface must leave collectible history"
    );
    let second =
        leyline_cli_lib::cmd_cdc::gc_database(&db_path, leyline_fs::gc::GcOptions::default())
            .unwrap();
    assert_eq!(second.deleted_chunk_rows, 0, "consumer GC is idempotent");

    let final_conn = Connection::open(&db_path).unwrap();
    let removed: i64 = final_conn
        .query_row(
            "SELECT COUNT(*) FROM nodes WHERE id = ?1",
            [&node_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(removed, 0);
}

#[test]
fn release_docs_pin_the_private_derived_ownership_contract() {
    let readme = include_str!("../../../../README.md");
    let changelog = include_str!("../../../../CHANGELOG.md");
    let normalized_readme = readme.split_whitespace().collect::<Vec<_>>().join(" ");

    assert!(readme.contains(
        "`content_manifest`, and `content_manifest_meta` tables are private derived\nindexes"
    ));
    assert!(
        normalized_readme.contains("Changes to those private indexes do not bump `leyline-schema`")
    );
    assert!(
        !changelog.contains("At v0.10.2 release time this was not yet wired"),
        "the v0.10.2 changelog must not contradict the now-wired incremental write path"
    );
}
