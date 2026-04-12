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
        /// A private-only subcommand (named differently to avoid clash with open's daemon).
        Embed,
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

    // Parse an `embed` subcommand through the wrapper — proves extension works.
    let cli2 = ExtendedCli::try_parse_from(["leyline-extended", "embed"])
        .expect("should parse 'embed' subcommand");

    assert!(matches!(cli2.command, ExtendedCommands::Embed));
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

/// Verify that serve's setup phase creates the arena file and controller with
/// correct state, without actually mounting a filesystem (which needs privileges).
#[test]
fn test_serve_creates_arena() {
    use leyline_core::Controller;

    let dir = TempDir::new().expect("create temp dir");
    let arena_path = dir.path().join("test.arena");
    let ctrl_path = dir.path().join("test.ctrl");

    let arena_size_mib: u64 = 2;
    let arena_bytes = arena_size_mib * 1024 * 1024;

    // Run setup_arena — the reusable core of cmd_serve.
    let returned_ctrl = leyline_cli_lib::cmd_serve::setup_arena(
        &arena_path,
        arena_bytes,
        Some(&ctrl_path),
    )
    .expect("setup_arena should succeed");

    // Verify the arena file was created with the expected size.
    assert!(arena_path.exists(), "arena file should exist");
    let arena_meta = fs::metadata(&arena_path).expect("stat arena");
    assert_eq!(
        arena_meta.len(),
        arena_bytes,
        "arena file should be {arena_bytes} bytes"
    );

    // Verify the controller file was created.
    assert_eq!(returned_ctrl, ctrl_path, "returned ctrl path should match");
    assert!(ctrl_path.exists(), "controller file should exist");

    // Verify controller state: generation 0 and arena path is set.
    let ctrl = Controller::open_or_create(&ctrl_path).expect("open controller");
    assert_eq!(ctrl.generation(), 0, "fresh controller should be gen 0");
    assert_eq!(
        ctrl.arena_size(),
        arena_bytes,
        "controller should record arena size"
    );
    // The arena path in the controller is canonicalized, so just check it ends with our filename.
    let stored_path = ctrl.arena_path();
    assert!(
        stored_path.ends_with("test.arena"),
        "controller arena path should reference the arena file, got: {stored_path}"
    );
}

/// Verify that setup_arena derives the control path from the arena path when
/// no explicit control path is given (arena.ctrl next to arena file).
#[test]
fn test_serve_derives_control_path() {
    let dir = TempDir::new().expect("create temp dir");
    let arena_path = dir.path().join("my.arena");

    let ctrl_path = leyline_cli_lib::cmd_serve::setup_arena(
        &arena_path,
        2 * 1024 * 1024,
        None,
    )
    .expect("setup_arena should succeed");

    // Should derive .ctrl extension from .arena
    let expected = dir.path().join("my.ctrl");
    assert_eq!(ctrl_path, expected, "derived ctrl path should use .ctrl extension");
    assert!(expected.exists(), "derived controller file should exist");
}

#[test]
fn test_edition_is_open() {
    assert_eq!(EDITION, "open");
}

// ---------------------------------------------------------------------------
// Daemon socket tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_daemon_socket_status_op() {
    use leyline_cli_lib::daemon::{DaemonContext, EventRouter, NoExt};
    use leyline_core::{Controller, create_arena};
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let dir = TempDir::new().unwrap();

    // Set up arena + controller.
    let arena_path = dir.path().join("test.arena");
    let ctrl_path = dir.path().join("test.ctrl");
    let _mmap = create_arena(&arena_path, 2 * 1024 * 1024).unwrap();
    let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
    ctrl.set_arena(&arena_path.to_string_lossy(), 2 * 1024 * 1024, 0)
        .unwrap();
    drop(ctrl);

    let ctx = Arc::new(DaemonContext {
        ctrl_path,
        ext: Arc::new(NoExt),
        router: EventRouter::new(16),
    });

    let sock_path = dir.path().join("test.sock");
    leyline_cli_lib::daemon::socket::spawn(ctx, sock_path.clone());

    // Give the listener a moment to bind.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let stream = UnixStream::connect(&sock_path).await.expect("connect to socket");
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    writer
        .write_all(b"{\"op\":\"status\"}\n")
        .await
        .expect("write status op");

    let response = lines.next_line().await.unwrap().expect("read response");
    let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["generation"], 0);
}

#[tokio::test]
async fn test_daemon_ext_dispatches_to_extension() {
    use leyline_cli_lib::daemon::ext::DaemonExt;
    use leyline_cli_lib::daemon::{DaemonContext, EventRouter};
    use leyline_core::{Controller, create_arena};
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    /// Custom extension that handles "custom_op".
    struct TestExt;
    impl DaemonExt for TestExt {
        fn handle_op(&self, op: &str, _req: &serde_json::Value) -> Option<String> {
            if op == "custom_op" {
                Some(r#"{"ok":true,"custom":"hello from extension"}"#.to_string())
            } else {
                None
            }
        }
    }

    let dir = TempDir::new().unwrap();

    // Set up arena + controller.
    let arena_path = dir.path().join("test.arena");
    let ctrl_path = dir.path().join("test.ctrl");
    let _mmap = create_arena(&arena_path, 2 * 1024 * 1024).unwrap();
    let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
    ctrl.set_arena(&arena_path.to_string_lossy(), 2 * 1024 * 1024, 0)
        .unwrap();
    drop(ctrl);

    let ctx = Arc::new(DaemonContext {
        ctrl_path,
        ext: Arc::new(TestExt),
        router: EventRouter::new(16),
    });

    let sock_path = dir.path().join("ext_test.sock");
    leyline_cli_lib::daemon::socket::spawn(ctx, sock_path.clone());

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let stream = UnixStream::connect(&sock_path).await.expect("connect to socket");
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    // Test custom extension op.
    writer
        .write_all(b"{\"op\":\"custom_op\"}\n")
        .await
        .expect("write custom_op");
    let response = lines.next_line().await.unwrap().expect("read custom response");
    let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["custom"], "hello from extension");

    // Test unknown op returns error.
    writer
        .write_all(b"{\"op\":\"nonexistent\"}\n")
        .await
        .expect("write unknown op");
    let response = lines.next_line().await.unwrap().expect("read unknown response");
    let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert!(parsed.get("error").is_some());
    let err_str = parsed["error"].as_str().unwrap();
    assert!(err_str.contains("unknown op"), "error should mention 'unknown op', got: {err_str}");
}

/// Compile-time proof that the Lsp variant exists when the `lsp` feature is enabled.
/// We can't spawn a real LSP server in CI, so we just prove the variant parses and matches.
#[cfg(feature = "lsp")]
#[test]
fn test_lsp_variant_exists() {
    use std::path::PathBuf;

    let cmd = Commands::Lsp {
        server: "gopls".to_string(),
        server_args: vec!["-remote=auto".to_string()],
        input: PathBuf::from("/tmp/main.go"),
        output: PathBuf::from("/tmp/out.db"),
        merge_db: None,
        language_id: Some("go".to_string()),
    };

    match cmd {
        Commands::Lsp {
            server,
            server_args,
            input,
            output,
            merge_db,
            language_id,
        } => {
            assert_eq!(server, "gopls");
            assert_eq!(server_args, vec!["-remote=auto"]);
            assert_eq!(input, PathBuf::from("/tmp/main.go"));
            assert_eq!(output, PathBuf::from("/tmp/out.db"));
            assert!(merge_db.is_none());
            assert_eq!(language_id.as_deref(), Some("go"));
        }
        _ => panic!("expected Lsp variant"),
    }
}

// Daemon variant is defined in the binary (not Commands), so each binary
// (open vs extended) can define its own args. The daemon logic is tested
// via the socket tests above.

// ---------------------------------------------------------------------------
// Go ref extraction + mache schema compat tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_parse_produces_go_refs() {
    let src = tempfile::TempDir::new().unwrap();
    let out_dir = tempfile::TempDir::new().unwrap();
    let db_path = out_dir.path().join("refs-test.db");

    std::fs::write(
        src.path().join("main.go"),
        b"package main\n\nimport (\n\t\"fmt\"\n\tauth \"github.com/foo/auth\"\n)\n\nfunc main() {\n\tfmt.Println(\"hi\")\n\tauth.Validate()\n\thelper()\n}\n\nfunc helper() {}\n",
    ).unwrap();

    let cmd = leyline_cli_lib::Commands::Parse {
        source: src.path().to_path_buf(),
        output: db_path.clone(),
        lang: Some("go".to_string()),
    };
    leyline_cli_lib::run(cmd).await.unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();

    // node_defs: should have "main" and "helper"
    let def_count: i64 = conn.query_row("SELECT COUNT(*) FROM node_defs", [], |r| r.get(0)).unwrap();
    assert!(def_count >= 2, "should have at least 2 defs, got {def_count}");

    let main_def: i64 = conn.query_row(
        "SELECT COUNT(*) FROM node_defs WHERE token = 'main'", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(main_def, 1, "should have 'main' def");

    let helper_def: i64 = conn.query_row(
        "SELECT COUNT(*) FROM node_defs WHERE token = 'helper'", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(helper_def, 1, "should have 'helper' def");

    // node_refs: should have calls
    let ref_count: i64 = conn.query_row("SELECT COUNT(*) FROM node_refs", [], |r| r.get(0)).unwrap();
    assert!(ref_count >= 3, "should have at least 3 refs, got {ref_count}");

    let println_ref: i64 = conn.query_row(
        "SELECT COUNT(*) FROM node_refs WHERE token = 'fmt.Println'", [], |r| r.get(0),
    ).unwrap();
    assert!(println_ref >= 1, "should have 'fmt.Println' ref");

    let helper_ref: i64 = conn.query_row(
        "SELECT COUNT(*) FROM node_refs WHERE token = 'helper'", [], |r| r.get(0),
    ).unwrap();
    assert!(helper_ref >= 1, "should have 'helper' call ref");

    // _imports
    let import_count: i64 = conn.query_row("SELECT COUNT(*) FROM _imports", [], |r| r.get(0)).unwrap();
    assert_eq!(import_count, 2, "should have 2 imports");

    let auth_import: String = conn.query_row(
        "SELECT path FROM _imports WHERE alias = 'auth'", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(auth_import, "github.com/foo/auth");
}

#[tokio::test]
async fn test_refs_tables_match_mache_schema() {
    let src = tempfile::TempDir::new().unwrap();
    let out_dir = tempfile::TempDir::new().unwrap();
    let db_path = out_dir.path().join("mache-compat.db");

    std::fs::write(
        src.path().join("main.go"),
        b"package main\n\nfunc main() {}\n",
    ).unwrap();

    let cmd = leyline_cli_lib::Commands::Parse {
        source: src.path().to_path_buf(),
        output: db_path.clone(),
        lang: Some("go".to_string()),
    };
    leyline_cli_lib::run(cmd).await.unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();

    // Verify all tables mache expects exist
    let tables: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"
        ).unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };

    assert!(tables.contains(&"nodes".to_string()), "missing nodes: {tables:?}");
    assert!(tables.contains(&"_ast".to_string()), "missing _ast: {tables:?}");
    assert!(tables.contains(&"_source".to_string()), "missing _source: {tables:?}");
    assert!(tables.contains(&"node_refs".to_string()), "missing node_refs: {tables:?}");
    assert!(tables.contains(&"node_defs".to_string()), "missing node_defs: {tables:?}");
    assert!(tables.contains(&"_imports".to_string()), "missing _imports: {tables:?}");

    // Verify mache fast-path trigger
    let nodes_exists: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='nodes'",
        [], |r| r.get(0),
    ).unwrap();
    assert_eq!(nodes_exists, 1);

    // Verify node_refs columns match mache's expected query pattern
    // mache does: SELECT node_id FROM node_refs WHERE token = ?
    conn.execute("SELECT token, node_id, source_id FROM node_refs LIMIT 0", []).unwrap();
    conn.execute("SELECT token, node_id, source_id FROM node_defs LIMIT 0", []).unwrap();
    conn.execute("SELECT alias, path, source_id FROM _imports LIMIT 0", []).unwrap();
}

/// Verify that `lsp` subcommand is parseable via clap when the feature is enabled.
#[cfg(feature = "lsp")]
#[test]
fn test_lsp_clap_parsing() {
    use clap::Parser;

    #[derive(Parser)]
    #[command(name = "test")]
    struct TestCli {
        #[command(subcommand)]
        command: Commands,
    }

    // Note: --server-args must come last because num_args=0.. consumes remaining values.
    let cli = TestCli::try_parse_from([
        "test",
        "lsp",
        "--server",
        "pyright-langserver",
        "--input",
        "/tmp/test.py",
        "--output",
        "/tmp/out.db",
        "--language-id",
        "python",
        "--server-args",
        "--stdio",
    ])
    .expect("should parse lsp subcommand");

    match cli.command {
        Commands::Lsp {
            server,
            server_args,
            input,
            language_id,
            ..
        } => {
            assert_eq!(server, "pyright-langserver");
            assert_eq!(server_args, vec!["--stdio"]);
            assert_eq!(input, Path::new("/tmp/test.py"));
            assert_eq!(language_id.as_deref(), Some("python"));
        }
        _ => panic!("expected Lsp variant"),
    }
}
