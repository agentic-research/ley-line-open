#![cfg(feature = "cdc")]

use clap::Parser;
use leyline_fs::activation::{ActivationOptions, ActivationProgress, ActivationReport};
use rusqlite::{Connection, params};
use tempfile::TempDir;

#[derive(Parser)]
#[command(name = "leyline")]
struct TestCli {
    #[command(subcommand)]
    command: leyline_cli_lib::Commands,
}

fn seed_projection_file() -> (TempDir, std::path::PathBuf) {
    let temp = TempDir::new().unwrap();
    let db = temp.path().join("graph.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE nodes (
            id TEXT PRIMARY KEY,
            parent_id TEXT,
            name TEXT NOT NULL,
            kind INTEGER NOT NULL,
            size INTEGER DEFAULT 0,
            mtime INTEGER NOT NULL,
            record TEXT
        );",
    )
    .unwrap();
    for (id, record) in [("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")] {
        conn.execute(
            "INSERT INTO nodes
             (id,parent_id,name,kind,size,mtime,record)
             VALUES (?1,'',?1,0,?2,7,?3)",
            params![id, record.len() as i64, record],
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
    assert_eq!(value["eligible_nodes"], 2);
    assert_eq!(value["populated_nodes"], 2);
    assert_eq!(value["already_fresh_nodes"], 0);
}

#[test]
fn cdc_enable_rejects_a_non_projection_database() {
    let temp = TempDir::new().unwrap();
    let db = temp.path().join("empty.db");
    Connection::open(&db).unwrap();
    let error =
        leyline_cli_lib::cmd_cdc::enable_database(&db, ActivationOptions::default()).unwrap_err();
    assert!(
        format!("{error:#}").contains("missing required nodes table"),
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
}

#[test]
fn cdc_report_formats_as_stable_human_and_json_output() {
    let report = ActivationReport {
        eligible_nodes: 3,
        populated_nodes: 2,
        already_fresh_nodes: 1,
        processed_source_bytes: 99,
        manifest_rows: 7,
        unique_chunk_rows: 5,
        unique_chunk_bytes: 88,
    };
    let human = leyline_cli_lib::cmd_cdc::format_report(report, false).unwrap();
    assert_eq!(
        human,
        "CDC enabled: eligible=3 populated=2 already_fresh=1 source_bytes=99 \
         manifest_rows=7 unique_chunks=5 unique_chunk_bytes=88"
    );

    let json = leyline_cli_lib::cmd_cdc::format_report(report, true).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["eligible_nodes"], 3);
    assert_eq!(value["unique_chunk_bytes"], 88);
}

#[test]
fn cdc_progress_formats_as_one_stable_stderr_line() {
    let line = leyline_cli_lib::cmd_cdc::format_progress(ActivationProgress {
        visited_nodes: 8,
        eligible_nodes: 21,
        populated_nodes: 5,
        already_fresh_nodes: 3,
        processed_source_bytes: 4096,
    });
    assert_eq!(
        line,
        "CDC activation: visited=8/21 populated=5 already_fresh=3 source_bytes=4096"
    );
}
