//! Integration tests for leyline-cli-lib.

use std::fs;
use std::path::Path;

use leyline_cli_lib::{Commands, EDITION};
use tempfile::TempDir;

/// Create a temporary directory containing two small `.go` files for testing.
fn create_go_fixture() -> TempDir {
    let dir = TempDir::new().expect("create temp dir");

    fs::write(
        dir.path().join("main.go"),
        b"package main\n\nfunc main() {\n\tprintln(\"hello\")\n}\n",
    )
    .expect("write main.go");

    fs::write(
        dir.path().join("util.go"),
        b"package main\n\nfunc add(a, b int) int {\n\treturn a + b\n}\n",
    )
    .expect("write util.go");

    dir
}

#[tokio::test]
async fn test_parse_creates_db() {
    let src = create_go_fixture();
    let out_dir = TempDir::new().expect("create output dir");
    let db_path = out_dir.path().join("test.db");

    let cmd = Commands::Parse {
        source: src.path().to_path_buf(),
        output: db_path.clone(),
        lang: Some("go".to_string()),
    };

    leyline_cli_lib::run(cmd).await.expect("parse should succeed");

    // Verify the database was created.
    assert!(db_path.exists(), "database file should exist");

    // Open and verify tables + row counts.
    let conn = rusqlite::Connection::open(&db_path).expect("open db");

    let nodes_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
        .expect("query nodes");
    // At minimum: root + 2 files + their AST children.
    assert!(nodes_count >= 3, "nodes should have at least 3 rows (root + 2 files), got {nodes_count}");

    let source_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM _source", [], |r| r.get(0))
        .expect("query _source");
    assert_eq!(source_count, 2, "_source should have 2 rows (one per .go file)");

    let ast_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM _ast", [], |r| r.get(0))
        .expect("query _ast");
    assert!(ast_count >= 2, "_ast should have at least 2 rows, got {ast_count}");
}

/// Proves that `Commands` can be embedded in a wrapper enum via `#[command(flatten)]`,
/// which is exactly what ley-line (private) will do to extend the CLI.
#[test]
fn test_commands_flattenable() {
    use clap::{Parser, Subcommand};

    /// A hypothetical wrapper that adds an extended subcommand alongside open's commands.
    #[derive(Subcommand)]
    enum ExtendedCommands {
        #[command(flatten)]
        Open(Commands),
        /// A private-only subcommand.
        Daemon,
    }

    #[derive(Parser)]
    #[command(name = "leyline-extended")]
    struct ExtendedCli {
        #[command(subcommand)]
        command: ExtendedCommands,
    }

    // Parse a `parse` subcommand through the wrapper — proves flatten works.
    let cli = ExtendedCli::try_parse_from(["leyline-extended", "parse", "/tmp/src"])
        .expect("should parse 'parse' subcommand through flattened wrapper");

    match cli.command {
        ExtendedCommands::Open(Commands::Parse { source, .. }) => {
            assert_eq!(source, Path::new("/tmp/src"));
        }
        _ => panic!("expected Open(Parse {{ .. }})"),
    }

    // Parse a `daemon` subcommand through the wrapper — proves extension works.
    let cli2 = ExtendedCli::try_parse_from(["leyline-extended", "daemon"])
        .expect("should parse 'daemon' subcommand");

    assert!(matches!(cli2.command, ExtendedCommands::Daemon));
}

#[test]
fn test_edition_is_open() {
    assert_eq!(EDITION, "open");
}
