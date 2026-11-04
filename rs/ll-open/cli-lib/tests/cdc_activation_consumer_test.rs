#![cfg(feature = "cdc")]

use leyline_core::{ContentAddressed, Controller};
use rusqlite::Connection;
use tempfile::TempDir;

/// The daemon entry point activates `source_blobs` CDC and then publishes the
/// exact activated database to the arena. The removed `nodes` target leaves
/// no trace: no `content_manifest` table is created.
#[test]
fn the_daemon_entry_point_activates_source_blobs_and_publishes_the_result() {
    let temp = TempDir::new().unwrap();
    let source_dir = temp.path().join("source");
    std::fs::create_dir(&source_dir).unwrap();
    // Comfortably above the 8 KiB chunking floor, so the target has real work
    // and `populated_blobs > 0` distinguishes "activated" from "skipped".
    let payload = "fedcba9876543210".repeat(64 * 1024);
    std::fs::write(
        source_dir.join("blobs.json"),
        format!("{{\"data\":\"{payload}\"}}\n"),
    )
    .unwrap();

    let db_path = temp.path().join("blobs.db");
    let conn = Connection::open(&db_path).unwrap();
    leyline_cli_lib::cmd_parse::parse_into_conn(&conn, &source_dir, Some("json"), None).unwrap();

    let arena_path = temp.path().join("blobs.arena");
    let ctrl_path = temp.path().join("blobs.ctrl");
    leyline_cli_lib::lifecycle::setup_arena(&arena_path, 4 * 1024, Some(&ctrl_path)).unwrap();

    let report = leyline_cli_lib::cmd_daemon::activate_cdc_and_snapshot(&conn, &ctrl_path, true)
        .unwrap()
        .expect("CDC was enabled");
    assert!(
        report.populated_blobs > 0,
        "the fixture blob is above the chunking floor, so activation must populate it"
    );
    assert_eq!(report.populated_blobs, report.eligible_blobs);

    let blob_spans: i64 = conn
        .query_row("SELECT COUNT(*) FROM blob_manifest", [], |row| row.get(0))
        .unwrap();
    assert!(blob_spans > 0, "source_blobs activation must tile the blob");

    let node_manifest_tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
              WHERE type = 'table'
                AND name IN ('content_manifest', 'content_manifest_meta', 'content_generation')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        node_manifest_tables, 0,
        "activation must not create the removed nodes target's tables"
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

/// The disabled path must publish exactly once and activate nothing.
#[test]
fn disabled_cdc_activates_nothing_and_still_publishes() {
    let temp = TempDir::new().unwrap();
    let source_dir = temp.path().join("source");
    std::fs::create_dir(&source_dir).unwrap();
    std::fs::write(source_dir.join("small.json"), "{\"k\":\"v\"}\n").unwrap();

    let db_path = temp.path().join("off.db");
    let conn = Connection::open(&db_path).unwrap();
    leyline_cli_lib::cmd_parse::parse_into_conn(&conn, &source_dir, Some("json"), None).unwrap();

    let arena_path = temp.path().join("off.arena");
    let ctrl_path = temp.path().join("off.ctrl");
    leyline_cli_lib::lifecycle::setup_arena(&arena_path, 4 * 1024, Some(&ctrl_path)).unwrap();

    let report =
        leyline_cli_lib::cmd_daemon::activate_cdc_and_snapshot(&conn, &ctrl_path, false).unwrap();
    assert!(report.is_none(), "disabled CDC must activate nothing");

    let controller = Controller::open_or_create(&ctrl_path).unwrap();
    let serialized = conn.serialize("main").unwrap();
    let serialized_bytes: &[u8] = serialized.as_ref();
    assert_eq!(
        controller.current_root(),
        *serialized_bytes.hash().as_bytes(),
        "the disabled path must still publish the exact database bytes"
    );
}
