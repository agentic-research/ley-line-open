#![cfg(feature = "cdc")]

use leyline_core::{ContentAddressed, Controller};
use leyline_fs::SqliteGraph;
use leyline_fs::chunked::{ContentSource, chunks_touched, read_content_at_traced};
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
