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

#[tokio::test]
async fn test_splice_modifies_node() {
    let src = create_go_fixture();
    let out_dir = TempDir::new().expect("create output dir");
    let db_path = out_dir.path().join("splice_test.db");

    // Step 1: Parse the Go fixture into a .db.
    let cmd = Commands::Parse {
        source: src.path().to_path_buf(),
        output: db_path.clone(),
        lang: Some("go".to_string()),
    };
    leyline_cli_lib::run(cmd).await.expect("parse should succeed");

    // Step 2: Find a node_id from the _ast table that we can splice.
    let conn = rusqlite::Connection::open(&db_path).expect("open db");
    let node_id: String = conn
        .query_row(
            "SELECT node_id FROM _ast WHERE source_id = 'util.go' AND node_kind = 'function_declaration' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .expect("find function_declaration node in _ast");
    drop(conn);

    // Step 3: Splice new text for that node.
    let new_func = "func add(a, b int) int {\n\treturn a * b\n}";
    let cmd = Commands::Splice {
        db: db_path.clone(),
        node: node_id.clone(),
        text: new_func.to_string(),
    };
    leyline_cli_lib::run(cmd).await.expect("splice should succeed");

    // Step 4: Verify the source changed in the database.
    let conn = rusqlite::Connection::open(&db_path).expect("open db after splice");
    let source: Vec<u8> = conn
        .query_row(
            "SELECT content FROM _source WHERE id = 'util.go'",
            [],
            |r| r.get(0),
        )
        .expect("read _source after splice");
    let source_str = String::from_utf8(source).expect("source should be valid UTF-8");
    assert!(
        source_str.contains("return a * b"),
        "spliced source should contain 'return a * b', got: {source_str}"
    );
    assert!(
        !source_str.contains("return a + b"),
        "spliced source should NOT contain original 'return a + b', got: {source_str}"
    );
}

#[tokio::test]
async fn test_load_into_arena() {
    use leyline_core::{ArenaHeader, Controller, create_arena};

    let src = create_go_fixture();
    let out_dir = TempDir::new().expect("create output dir");
    let db_path = out_dir.path().join("load_test.db");

    // Step 1: Parse Go fixture into a .db.
    let cmd = Commands::Parse {
        source: src.path().to_path_buf(),
        output: db_path.clone(),
        lang: Some("go".to_string()),
    };
    leyline_cli_lib::run(cmd).await.expect("parse should succeed");
    assert!(db_path.exists(), "db should exist after parse");

    let db_bytes = fs::read(&db_path).expect("read db bytes");

    // Step 2: Create a temp arena file large enough for the .db.
    //   Arena = 4096 header + 2 * buffer_size.
    //   Each buffer must fit the full .db.
    let buffer_size = db_bytes.len().max(4096) as u64;
    let arena_size = ArenaHeader::HEADER_SIZE + buffer_size * 2;
    let arena_path = out_dir.path().join("test.arena");
    let mmap = create_arena(&arena_path, arena_size).expect("create arena");
    drop(mmap); // Release the mmap so cmd_load can open it.

    // Step 3: Create a Controller and point it at the arena.
    let ctrl_path = out_dir.path().join("test.ctrl");
    {
        let mut ctrl = Controller::open_or_create(&ctrl_path).expect("create controller");
        ctrl.set_arena(
            arena_path.to_str().expect("arena path is utf-8"),
            arena_size,
            0,
        )
        .expect("set arena in controller");
    }

    // Step 4: Run load.
    let cmd = Commands::Load {
        db: db_path.clone(),
        control: ctrl_path.clone(),
    };
    leyline_cli_lib::run(cmd).await.expect("load should succeed");

    // Step 5: Verify generation bumped to 1.
    let ctrl = Controller::open_or_create(&ctrl_path).expect("reopen controller");
    assert_eq!(
        ctrl.generation(),
        1,
        "generation should be 1 after first load"
    );

    // Step 6: Verify the arena contains the db bytes in the active buffer.
    let arena_mmap = create_arena(&arena_path, arena_size).expect("reopen arena");
    let header: ArenaHeader =
        *bytemuck::from_bytes(&arena_mmap[..std::mem::size_of::<ArenaHeader>()]);
    assert_eq!(header.sequence, 1, "arena sequence should be 1");
    let offset = header.active_buffer_offset(arena_size).unwrap() as usize;
    assert_eq!(
        &arena_mmap[offset..offset + db_bytes.len()],
        &db_bytes[..],
        "active buffer should contain the .db bytes"
    );
}

/// Inspect the arena — look up the root node (id="") after building a db in-memory,
/// loading it into the arena, and querying it back.
#[test]
fn test_inspect_node_lookup() {
    use leyline_core::{ArenaHeader, create_arena, write_to_arena};
    use leyline_schema::create_schema;
    use rusqlite::DatabaseName;

    let out_dir = TempDir::new().expect("create output dir");

    // Step 1: Create an in-memory database with the nodes schema and a root node.
    let source_conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
    create_schema(&source_conn).expect("create schema");
    source_conn
        .execute(
            "INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params!["", "", "root", 1, 0, 1000, ""],
        )
        .expect("insert root node");
    source_conn
        .execute(
            "INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params!["main.go", "", "main.go", 0, 42, 2000, ""],
        )
        .expect("insert file node");

    let serialized = source_conn
        .serialize(DatabaseName::Main)
        .expect("serialize db");
    let db_bytes = serialized.to_vec();
    drop(source_conn);

    // Step 2: Create an arena and write the serialized db into it.
    let buffer_size = db_bytes.len().max(4096) as u64;
    let arena_size = ArenaHeader::HEADER_SIZE + buffer_size * 2;
    let arena_path = out_dir.path().join("inspect.arena");
    let mut mmap = create_arena(&arena_path, arena_size).expect("create arena");

    write_to_arena(&mut mmap, &db_bytes).expect("write db into arena");
    drop(mmap);

    // Step 3: Inspect the root node (id="").
    let result = leyline_cli_lib::cmd_inspect::cmd_inspect(
        "",              // root node id
        &arena_path,
        None,            // no controller
        None,            // node-lookup mode
    );
    assert!(result.is_ok(), "inspect root node should succeed: {:?}", result.err());

    // Step 4: Test SQL mode.
    let result = leyline_cli_lib::cmd_inspect::cmd_inspect(
        "",
        &arena_path,
        None,
        Some("SELECT COUNT(*) FROM nodes"),
    );
    assert!(result.is_ok(), "inspect SQL mode should succeed: {:?}", result.err());

    // Step 5: Verify a missing node returns an error.
    let result = leyline_cli_lib::cmd_inspect::cmd_inspect(
        "nonexistent",
        &arena_path,
        None,
        None,
    );
    assert!(result.is_err(), "inspect missing node should fail");
}

#[test]
fn test_edition_is_open() {
    assert_eq!(EDITION, "open");
}
