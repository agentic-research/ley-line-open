#![cfg(feature = "cdc")]

use clap::Parser;
use leyline_core::ContentAddressed;
use leyline_fs::activation::{ActivationOptions, BlobActivationProgress, BlobActivationReport};
use leyline_fs::gc::GcOptions;
use rusqlite::{Connection, params};
use tempfile::TempDir;

#[derive(Parser)]
#[command(name = "leyline")]
struct TestCli {
    #[command(subcommand)]
    command: leyline_cli_lib::Commands,
}

/// A projection file carrying two `source_blobs` rows above the 8 KiB
/// chunking floor — the rows CDC activation consumes.
fn seed_projection_file() -> (TempDir, std::path::PathBuf) {
    let temp = TempDir::new().unwrap();
    let db = temp.path().join("graph.db");
    let conn = Connection::open(&db).unwrap();
    leyline_ts::schema::create_source_blobs_table(&conn).unwrap();
    for fill in [b'a', b'b'] {
        let bytes = vec![fill; 9_000];
        conn.execute(
            "INSERT INTO source_blobs (blob_hash, blob_bytes) VALUES (?1, ?2)",
            params![bytes.as_slice().hash().as_bytes().as_slice(), bytes],
        )
        .unwrap();
    }
    drop(conn);
    (temp, db)
}

#[test]
fn cdc_enable_mutates_a_real_db_and_returns_stable_json() {
    let (_temp, db) = seed_projection_file();
    let report =
        leyline_cli_lib::cmd_cdc::enable_database(&db, ActivationOptions { batch_size: 1 })
            .unwrap();
    let value = serde_json::to_value(report).unwrap();
    assert_eq!(value["eligible_blobs"], 2);
    assert_eq!(value["populated_blobs"], 2);
    assert_eq!(value["already_fresh_blobs"], 0);
}

#[test]
fn cdc_enable_rejects_a_database_without_the_source_blobs_table() {
    let temp = TempDir::new().unwrap();
    let db = temp.path().join("empty.db");
    Connection::open(&db).unwrap();
    let error =
        leyline_cli_lib::cmd_cdc::enable_database(&db, ActivationOptions::default()).unwrap_err();
    assert!(
        format!("{error:#}").contains("missing required source_blobs table"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn cdc_enable_does_not_create_a_misspelled_database_path() {
    let temp = TempDir::new().unwrap();
    let missing = temp.path().join("misspelled.db");
    let error = leyline_cli_lib::cmd_cdc::enable_database(&missing, ActivationOptions::default())
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("open CDC database"),
        "unexpected error: {error:#}"
    );
    assert!(
        !missing.exists(),
        "activating an existing projection must not create a typo path"
    );
}

#[test]
fn cdc_enable_cli_parses_nested_command_and_options() {
    let cli = TestCli::try_parse_from([
        "leyline",
        "cdc",
        "enable",
        "--db",
        "graph.db",
        "--batch-size",
        "8",
        "--json",
    ])
    .unwrap();
    match cli.command {
        leyline_cli_lib::Commands::Cdc {
            command:
                leyline_cli_lib::CdcCommands::Enable {
                    db,
                    batch_size,
                    json,
                },
        } => {
            assert_eq!(db, std::path::PathBuf::from("graph.db"));
            assert_eq!(batch_size, 8);
            assert!(json);
        }
        _ => panic!("expected cdc enable command"),
    }

    // The removed `--target` selector is a parse error, not a silent no-op.
    assert!(
        TestCli::try_parse_from([
            "leyline", "cdc", "enable", "--db", "graph.db", "--target", "nodes",
        ])
        .is_err()
    );
}

#[test]
fn cdc_report_formats_as_stable_human_and_json_output() {
    let report = BlobActivationReport {
        eligible_blobs: 3,
        populated_blobs: 2,
        already_fresh_blobs: 1,
        skipped_sub_floor_blobs: 9,
        processed_source_bytes: 99,
        manifest_rows: 7,
        unique_chunk_rows: 5,
        unique_chunk_bytes: 88,
    };
    let human = leyline_cli_lib::cmd_cdc::format_report(report, false).unwrap();
    assert_eq!(
        human,
        "CDC source_blobs enabled: eligible=3 populated=2 already_fresh=1 \
         skipped_sub_floor=9 source_bytes=99 manifest_rows=7 unique_chunks=5 \
         unique_chunk_bytes=88"
    );

    let json = leyline_cli_lib::cmd_cdc::format_report(report, true).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["eligible_blobs"], 3);
    // The counted policy field must survive serialization — it is how an
    // operator sees the floor doing its job.
    assert_eq!(value["skipped_sub_floor_blobs"], 9);
    assert_eq!(value["unique_chunk_bytes"], 88);
}

#[test]
fn cdc_progress_formats_as_one_stable_stderr_line() {
    let line = leyline_cli_lib::cmd_cdc::format_progress(BlobActivationProgress {
        visited_blobs: 8,
        eligible_blobs: 21,
        populated_blobs: 5,
        already_fresh_blobs: 3,
        processed_source_bytes: 4096,
    });
    assert_eq!(
        line,
        "CDC source_blobs activation: visited=8/21 populated=5 already_fresh=3 source_bytes=4096"
    );
}

#[test]
fn cdc_gc_dry_run_then_delete_mutates_a_real_projection() {
    let (_temp, db) = seed_projection_file();
    leyline_cli_lib::cmd_cdc::enable_database(&db, ActivationOptions::default()).unwrap();
    let conn = Connection::open(&db).unwrap();
    conn.execute("DELETE FROM source_blobs", []).unwrap();
    let before: i64 = conn
        .query_row("SELECT COUNT(*) FROM content_chunks", [], |row| row.get(0))
        .unwrap();
    assert!(before > 0);
    drop(conn);

    let dry_run = leyline_cli_lib::cmd_cdc::gc_database(&db, GcOptions { dry_run: true }).unwrap();
    assert_eq!(dry_run.unreachable_chunk_rows, before as u64);
    assert_eq!(dry_run.deleted_chunk_rows, 0);

    let deleted = leyline_cli_lib::cmd_cdc::gc_database(&db, GcOptions { dry_run: false }).unwrap();
    assert_eq!(deleted.deleted_chunk_rows, before as u64);
    assert_eq!(deleted.remaining_chunk_rows, 0);
}

#[test]
fn cdc_gc_does_not_create_a_misspelled_database_path() {
    let temp = TempDir::new().unwrap();
    let missing = temp.path().join("misspelled.db");

    let error = leyline_cli_lib::cmd_cdc::gc_database(&missing, GcOptions::default()).unwrap_err();

    assert!(
        format!("{error:#}").contains("open CDC database"),
        "unexpected error: {error:#}"
    );
    assert!(
        !missing.exists(),
        "collecting an existing projection must not create a typo path"
    );
}

#[tokio::test]
async fn cdc_gc_dispatches_through_the_public_command_runner() {
    let (_temp, db) = seed_projection_file();
    leyline_cli_lib::cmd_cdc::enable_database(&db, ActivationOptions::default()).unwrap();
    let conn = Connection::open(&db).unwrap();
    conn.execute("DELETE FROM source_blobs", []).unwrap();
    drop(conn);

    leyline_cli_lib::run(leyline_cli_lib::Commands::Cdc {
        command: leyline_cli_lib::CdcCommands::Gc {
            db: db.clone(),
            dry_run: false,
            json: true,
        },
    })
    .await
    .unwrap();

    let conn = Connection::open(&db).unwrap();
    let remaining: i64 = conn
        .query_row("SELECT COUNT(*) FROM content_chunks", [], |row| row.get(0))
        .unwrap();
    assert_eq!(remaining, 0);
}

#[test]
fn cdc_gc_cli_parses_options_and_formats_stable_json() {
    let cli = TestCli::try_parse_from([
        "leyline",
        "cdc",
        "gc",
        "--db",
        "graph.db",
        "--dry-run",
        "--json",
    ])
    .unwrap();
    match cli.command {
        leyline_cli_lib::Commands::Cdc {
            command: leyline_cli_lib::CdcCommands::Gc { db, dry_run, json },
        } => {
            assert_eq!(db, std::path::PathBuf::from("graph.db"));
            assert!(dry_run);
            assert!(json);
        }
        _ => panic!("expected cdc gc command"),
    }

    let report = leyline_fs::gc::GcReport {
        before_chunk_rows: 4,
        before_chunk_bytes: 400,
        unreachable_chunk_rows: 2,
        unreachable_chunk_bytes: 120,
        deleted_chunk_rows: 0,
        deleted_chunk_bytes: 0,
        remaining_chunk_rows: 4,
        remaining_chunk_bytes: 400,
        reaped_blob_manifest_rows: 4,
        reaped_blob_manifest_blobs: 2,
        dry_run: true,
    };
    let json = leyline_cli_lib::cmd_cdc::format_gc_report(report, true).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["unreachable_chunk_rows"], 2);
    assert_eq!(value["dry_run"], true);
    // Dead-manifest reaping is operator-visible, or a silent reclaim looks
    // identical to nothing having happened (bead ley-line-open-b5e56f).
    assert_eq!(value["reaped_blob_manifest_rows"], 4);
    assert_eq!(value["reaped_blob_manifest_blobs"], 2);
}
