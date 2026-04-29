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
        live_db: std::sync::Mutex::new(rusqlite::Connection::open_in_memory().unwrap()),
        source_dir: None,
        lang_filter: None,
        enrichment_passes: vec![],
        state: Arc::new(std::sync::RwLock::new(
            leyline_cli_lib::daemon::DaemonState::initializing(),
        )),
        #[cfg(feature = "vec")]
        vec_index: {
            leyline_cli_lib::daemon::vec_index::register_vec();
            Arc::new(leyline_cli_lib::daemon::vec_index::VectorIndex::new(4, None).unwrap())
        },
        #[cfg(feature = "vec")]
        embedder: Arc::new(leyline_cli_lib::daemon::embed::ZeroEmbedder { dim: 4 }),
        #[cfg(feature = "vec")]
        embed_queue: Arc::new(std::sync::Mutex::new(std::collections::BinaryHeap::new())),
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
        live_db: std::sync::Mutex::new(rusqlite::Connection::open_in_memory().unwrap()),
        source_dir: None,
        lang_filter: None,
        enrichment_passes: vec![],
        state: Arc::new(std::sync::RwLock::new(
            leyline_cli_lib::daemon::DaemonState::initializing(),
        )),
        #[cfg(feature = "vec")]
        vec_index: {
            leyline_cli_lib::daemon::vec_index::register_vec();
            Arc::new(leyline_cli_lib::daemon::vec_index::VectorIndex::new(4, None).unwrap())
        },
        #[cfg(feature = "vec")]
        embedder: Arc::new(leyline_cli_lib::daemon::embed::ZeroEmbedder { dim: 4 }),
        #[cfg(feature = "vec")]
        embed_queue: Arc::new(std::sync::Mutex::new(std::collections::BinaryHeap::new())),
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
// MCP HTTP transport tests
// ---------------------------------------------------------------------------

fn twoway_find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

fn parse_content_length(headers: &str) -> Option<usize> {
    for line in headers.split("\r\n") {
        let mut parts = line.splitn(2, ':');
        if parts.next()?.eq_ignore_ascii_case("content-length") {
            return parts.next()?.trim().parse().ok();
        }
    }
    None
}

/// Round-trip a JSON-RPC request through the MCP HTTP transport. Returns the
/// parsed response body. Uses a raw TcpStream to keep the dev-dep set lean.
#[cfg(test)]
async fn mcp_post(port: u16, body: &str) -> serde_json::Value {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
    let req = format!(
        "POST /mcp HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        body.len(),
        body,
    );
    stream.write_all(req.as_bytes()).await.expect("write");
    stream.flush().await.expect("flush");

    let mut buf = Vec::new();
    // Read until the server closes the connection (Connection: close).
    loop {
        let mut chunk = [0u8; 4096];
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
        // Heuristic stop: full HTTP/1.1 200 response with body fits in the
        // first read for these small payloads. If we have headers + a body,
        // we're done.
        if let Some(idx) = twoway_find(&buf, b"\r\n\r\n") {
            let headers = std::str::from_utf8(&buf[..idx]).unwrap_or("");
            if let Some(cl) = parse_content_length(headers)
                && buf.len() >= idx + 4 + cl
            {
                break;
            }
        }
    }
    let raw = String::from_utf8_lossy(&buf);

    // Split off headers; everything after the blank line is the JSON body.
    let body_start = match raw.find("\r\n\r\n") {
        Some(i) => i + 4,
        None => panic!("no HTTP body separator in response: {raw:?}"),
    };
    let body = &raw[body_start..];
    // axum may emit chunked transfer encoding; strip the chunk header if so.
    // In our case Content-Length is set, so body is a single chunk.
    let body = body.trim_start_matches(|c: char| c.is_ascii_hexdigit());
    let body = body.trim_start_matches("\r\n");
    let body = body.trim_end_matches("\r\n0\r\n\r\n");
    serde_json::from_str(body)
        .unwrap_or_else(|e| panic!("response is not valid JSON: {e}\nbody: {body:?}\nraw: {raw:?}"))
}

/// Verify `tools/list` returns the expected core tools (status / lsp_diagnostics)
/// and that `tools/call → status` round-trips through the dispatch table.
#[tokio::test]
async fn test_mcp_http_tools_list_and_status_call() {
    use leyline_cli_lib::daemon::{DaemonContext, EventRouter, NoExt};
    use leyline_core::{Controller, create_arena};
    use std::sync::Arc;

    let dir = TempDir::new().unwrap();
    let arena_path = dir.path().join("test.arena");
    let ctrl_path = dir.path().join("test.ctrl");
    let _mmap = create_arena(&arena_path, 2 * 1024 * 1024).unwrap();
    let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
    ctrl.set_arena(&arena_path.to_string_lossy(), 2 * 1024 * 1024, 0).unwrap();
    drop(ctrl);

    let ctx = Arc::new(DaemonContext {
        ctrl_path,
        ext: Arc::new(NoExt),
        router: EventRouter::new(16),
        live_db: std::sync::Mutex::new(rusqlite::Connection::open_in_memory().unwrap()),
        source_dir: None,
        lang_filter: None,
        enrichment_passes: vec![],
        state: Arc::new(std::sync::RwLock::new(
            leyline_cli_lib::daemon::DaemonState::initializing(),
        )),
        #[cfg(feature = "vec")]
        vec_index: {
            leyline_cli_lib::daemon::vec_index::register_vec();
            Arc::new(leyline_cli_lib::daemon::vec_index::VectorIndex::new(4, None).unwrap())
        },
        #[cfg(feature = "vec")]
        embedder: Arc::new(leyline_cli_lib::daemon::embed::ZeroEmbedder { dim: 4 }),
        #[cfg(feature = "vec")]
        embed_queue: Arc::new(std::sync::Mutex::new(std::collections::BinaryHeap::new())),
    });

    // Bind to port 0 (any free port), then read the assigned port. We pick
    // it by binding once, dropping, and using the same port; that's racy
    // but acceptable for this test. Instead, hand the port choice to the
    // OS via a quick probe.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let handle = leyline_cli_lib::daemon::mcp::spawn(ctx, port).expect("spawn MCP HTTP");
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;

    // 1. tools/list
    let listing = mcp_post(
        port,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
    )
    .await;
    assert_eq!(listing["jsonrpc"], "2.0");
    assert_eq!(listing["id"], 1);
    let tools = listing["result"]["tools"].as_array().expect("tools array");
    let names: std::collections::HashSet<&str> =
        tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for required in [
        "status",
        "snapshot",
        "reparse",
        "lsp_hover",
        "lsp_diagnostics",
        "find_callers",
        "get_node",
    ] {
        assert!(names.contains(required), "tools/list missing {required}");
    }

    // 2. tools/call → status
    let call = mcp_post(
        port,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"status","arguments":{}}}"#,
    )
    .await;
    assert_eq!(call["jsonrpc"], "2.0");
    assert_eq!(call["id"], 2);
    let content = call["result"]["content"].as_array().expect("content array");
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "text");
    let text = content[0]["text"].as_str().expect("text payload");
    let inner: serde_json::Value = serde_json::from_str(text).expect("inner JSON");
    assert_eq!(inner["ok"], true);
    assert_eq!(inner["phase"], "initializing");

    // 3. tools/call with unknown tool → error response
    let bad = mcp_post(
        port,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"does_not_exist","arguments":{}}}"#,
    )
    .await;
    assert!(bad["error"].is_object(), "expected error response");
    assert_eq!(bad["error"]["code"], -32601);

    // 4. tools/call → lsp_diagnostics on empty db should set isError=true
    //    (the inner op returns {ok: false, error: ...} when _lsp table is missing).
    let lsp = mcp_post(
        port,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"lsp_diagnostics","arguments":{"file":"/tmp/no.rs"}}}"#,
    )
    .await;
    assert_eq!(
        lsp["result"]["isError"], true,
        "isError must be true when inner op returns ok:false (got {lsp})",
    );

    handle.abort();
}

// ---------------------------------------------------------------------------
// Warm start + crash recovery stress tests
// ---------------------------------------------------------------------------

/// Full lifecycle: cold start → parse → snapshot → "crash" → warm start
/// → verify data survived → modify file → incremental reparse → verify
/// only the changed file was updated.
#[test]
fn test_warm_start_crash_recovery() {
    use leyline_core::{Controller, create_arena};
    use leyline_cli_lib::cmd_parse::parse_into_conn;
    use leyline_cli_lib::cmd_daemon::snapshot_to_arena;

    let src = create_go_fixture();
    let arena_dir = TempDir::new().unwrap();
    let arena_path = arena_dir.path().join("test.arena");
    let ctrl_path = arena_dir.path().join("test.ctrl");

    // --- Phase 1: Cold start — parse into :memory:, snapshot to arena ---

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let result = parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();
    assert!(result.parsed >= 2, "should parse at least main.go + util.go");
    assert_eq!(result.unchanged, 0, "cold start has no unchanged files");

    // Count nodes before snapshot.
    let node_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
        .unwrap();
    assert!(node_count > 0, "should have nodes after parse");

    // Count refs.
    let ref_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM node_refs", [], |r| r.get(0))
        .unwrap();

    // Set up arena + controller.
    let arena_size = 4 * 1024 * 1024; // 4MB
    let _mmap = create_arena(&arena_path, arena_size).unwrap();
    let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
    ctrl.set_arena(&arena_path.to_string_lossy(), arena_size, 0)
        .unwrap();

    // Snapshot living db to arena.
    snapshot_to_arena(&conn, &ctrl_path).unwrap();

    let gen_after_snapshot = Controller::open_or_create(&ctrl_path)
        .unwrap()
        .generation();
    assert_eq!(gen_after_snapshot, 1, "generation should be 1 after snapshot");

    // --- Phase 2: Simulate crash — drop the connection ---

    drop(conn);

    // --- Phase 3: Warm start — deserialize arena into new :memory: ---

    // Read arena active buffer.
    let file = std::fs::File::open(&arena_path).unwrap();
    let mmap = unsafe { memmap2::Mmap::map(&file).unwrap() };
    let header: &leyline_core::ArenaHeader =
        bytemuck::from_bytes(&mmap[..std::mem::size_of::<leyline_core::ArenaHeader>()]);

    let file_size = mmap.len() as u64;
    let offset = header.active_buffer_offset(file_size).unwrap();
    let buf_size = leyline_core::ArenaHeader::buffer_size(file_size);
    let buf = &mmap[offset as usize..(offset + buf_size) as usize];

    // Deserialize as writable.
    let mut recovered = rusqlite::Connection::open_in_memory().unwrap();
    recovered
        .deserialize_read_exact(
            rusqlite::DatabaseName::Main,
            std::io::Cursor::new(buf),
            buf.len(),
            false, // writable
        )
        .expect("warm start: sqlite3_deserialize should succeed");

    // Verify data survived the crash.
    let recovered_nodes: i64 = recovered
        .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        recovered_nodes, node_count,
        "warm start should recover all nodes"
    );

    let recovered_refs: i64 = recovered
        .query_row("SELECT COUNT(*) FROM node_refs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        recovered_refs, ref_count,
        "warm start should recover all refs"
    );

    // Verify specific file exists.
    let main_exists: bool = recovered
        .query_row(
            "SELECT COUNT(*) > 0 FROM _source WHERE id = 'main.go'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(main_exists, "main.go should exist after warm start");

    // --- Phase 4: Modify a file, incremental reparse ---

    fs::write(
        src.path().join("main.go"),
        b"package main\n\nfunc main() {\n\tprintln(\"modified!\")\n}\n\nfunc newFunc() {}\n",
    )
    .unwrap();

    let result2 = parse_into_conn(&recovered, src.path(), Some("go"), None).unwrap();
    assert_eq!(result2.parsed, 1, "only main.go should be reparsed");
    assert_eq!(result2.unchanged, 1, "util.go should be unchanged");
    assert!(
        result2.changed_files.contains(&"main.go".to_string()),
        "changed_files should contain main.go"
    );

    // Verify we have more nodes for main.go after adding a function.
    // The file now has 2 functions (main + newFunc), so there should be
    // at least 2 function_declaration AST entries for main.go.
    let func_count: i64 = recovered
        .query_row(
            "SELECT COUNT(*) FROM _ast WHERE source_id = 'main.go' AND node_kind = 'function_declaration'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        func_count >= 2,
        "main.go should have at least 2 function_declarations after adding newFunc, got {func_count}"
    );

    // --- Phase 5: Snapshot again and verify generation bumped ---

    snapshot_to_arena(&recovered, &ctrl_path).unwrap();

    let gen_after_reparse = Controller::open_or_create(&ctrl_path)
        .unwrap()
        .generation();
    assert_eq!(
        gen_after_reparse, 2,
        "generation should be 2 after second snapshot"
    );
}

/// Verify that warm start from an empty/invalid arena returns None
/// and falls through to cold start gracefully.
#[test]
fn test_warm_start_empty_arena_falls_through() {
    let dir = TempDir::new().unwrap();
    let arena_path = dir.path().join("empty.arena");
    let ctrl_path = dir.path().join("empty.ctrl");

    // Create an arena with no data loaded.
    let arena_size = 2 * 1024 * 1024;
    let _mmap = leyline_core::create_arena(&arena_path, arena_size).unwrap();
    let mut ctrl = leyline_core::Controller::open_or_create(&ctrl_path).unwrap();
    ctrl.set_arena(&arena_path.to_string_lossy(), arena_size, 0)
        .unwrap();

    // Read the empty arena — active buffer is all zeros.
    let file = std::fs::File::open(&arena_path).unwrap();
    let mmap = unsafe { memmap2::Mmap::map(&file).unwrap() };
    let header: &leyline_core::ArenaHeader =
        bytemuck::from_bytes(&mmap[..std::mem::size_of::<leyline_core::ArenaHeader>()]);

    let file_size = mmap.len() as u64;
    let offset = header.active_buffer_offset(file_size).unwrap();
    let buf_size = leyline_core::ArenaHeader::buffer_size(file_size);
    let buf = &mmap[offset as usize..(offset + buf_size) as usize];

    // All zeros — deserialize should fail or produce empty db.
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    let result = conn.deserialize_read_exact(
        rusqlite::DatabaseName::Main,
        std::io::Cursor::new(buf),
        buf.len(),
        false,
    );

    // sqlite3_deserialize accepts any buffer (even zeros), but querying
    // will fail because it's not a valid SQLite database.
    if result.is_ok() {
        let tables = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table'",
            [],
            |r| r.get::<_, i64>(0),
        );
        // Either the query fails (not a valid db) or returns 0 tables.
        match tables {
            Ok(0) => {} // empty db, fine
            Err(_) => {} // not a valid db, fine
            Ok(n) => panic!("expected 0 tables in empty arena, got {n}"),
        }
    }
    // If deserialize itself failed, that's also fine — cold start path.
}

/// Stress test: multiple parse-snapshot cycles to verify no corruption.
#[test]
fn test_multiple_snapshot_cycles() {
    use leyline_core::{Controller, create_arena};
    use leyline_cli_lib::cmd_parse::parse_into_conn;
    use leyline_cli_lib::cmd_daemon::snapshot_to_arena;

    let src = create_go_fixture();
    let arena_dir = TempDir::new().unwrap();
    let arena_path = arena_dir.path().join("cycle.arena");
    let ctrl_path = arena_dir.path().join("cycle.ctrl");

    let arena_size = 4 * 1024 * 1024;
    let _mmap = create_arena(&arena_path, arena_size).unwrap();
    let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
    ctrl.set_arena(&arena_path.to_string_lossy(), arena_size, 0).unwrap();

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();

    // Run 5 snapshot cycles, modifying the file each time.
    for i in 0..5 {
        fs::write(
            src.path().join("main.go"),
            format!(
                "package main\n\nfunc main() {{\n\tprintln(\"iteration {i}\")\n}}\n"
            ),
        )
        .unwrap();

        let result = parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();
        assert_eq!(result.parsed, 1, "cycle {i}: only main.go should reparse");

        snapshot_to_arena(&conn, &ctrl_path).unwrap();

        let current_gen = Controller::open_or_create(&ctrl_path).unwrap().generation();
        assert_eq!(current_gen, (i + 1) as u64, "cycle {i}: generation should match");
    }

    // Verify final state is consistent.
    let final_nodes: i64 = conn
        .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
        .unwrap();
    assert!(final_nodes > 0, "should have nodes after 5 cycles");

    // Warm start from final arena and verify.
    drop(conn);

    let file = std::fs::File::open(&arena_path).unwrap();
    let mmap = unsafe { memmap2::Mmap::map(&file).unwrap() };
    let header: &leyline_core::ArenaHeader =
        bytemuck::from_bytes(&mmap[..std::mem::size_of::<leyline_core::ArenaHeader>()]);
    let offset = header.active_buffer_offset(mmap.len() as u64).unwrap();
    let buf_size = leyline_core::ArenaHeader::buffer_size(mmap.len() as u64);
    let buf = &mmap[offset as usize..(offset + buf_size) as usize];

    let mut recovered = rusqlite::Connection::open_in_memory().unwrap();
    recovered
        .deserialize_read_exact(
            rusqlite::DatabaseName::Main,
            std::io::Cursor::new(buf),
            buf.len(),
            false,
        )
        .unwrap();

    let recovered_nodes: i64 = recovered
        .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        recovered_nodes, final_nodes,
        "warm start after 5 cycles should recover all nodes"
    );
}

// ---------------------------------------------------------------------------
// Incremental reparse tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_incremental_skip_unchanged() {
    let src = tempfile::TempDir::new().unwrap();
    let out_dir = tempfile::TempDir::new().unwrap();
    let db_path = out_dir.path().join("incr.db");

    std::fs::write(src.path().join("a.go"), b"package main\n\nfunc A() {}\n").unwrap();
    std::fs::write(src.path().join("b.go"), b"package main\n\nfunc B() {}\n").unwrap();

    // First parse
    let cmd = leyline_cli_lib::Commands::Parse {
        source: src.path().to_path_buf(),
        output: db_path.clone(),
        lang: Some("go".to_string()),
    };
    leyline_cli_lib::run(cmd).await.unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let defs_first: i64 = conn
        .query_row("SELECT COUNT(*) FROM node_defs", [], |r| r.get(0))
        .unwrap();
    assert!(defs_first >= 2);
    drop(conn);

    // Second parse — no changes
    let cmd = leyline_cli_lib::Commands::Parse {
        source: src.path().to_path_buf(),
        output: db_path.clone(),
        lang: Some("go".to_string()),
    };
    leyline_cli_lib::run(cmd).await.unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let defs_second: i64 = conn
        .query_row("SELECT COUNT(*) FROM node_defs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(defs_first, defs_second, "no changes = same data");

    let index_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM _file_index", [], |r| r.get(0))
        .unwrap();
    assert_eq!(index_count, 2);
}

#[tokio::test]
async fn test_incremental_reparse_changed_file() {
    let src = tempfile::TempDir::new().unwrap();
    let out_dir = tempfile::TempDir::new().unwrap();
    let db_path = out_dir.path().join("incr-change.db");

    std::fs::write(src.path().join("main.go"), b"package main\n\nfunc Old() {}\n").unwrap();

    // First parse
    let cmd = leyline_cli_lib::Commands::Parse {
        source: src.path().to_path_buf(),
        output: db_path.clone(),
        lang: Some("go".to_string()),
    };
    leyline_cli_lib::run(cmd).await.unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let old_def: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM node_defs WHERE token = 'Old'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(old_def, 1);
    drop(conn);

    // Modify file — sleep to ensure mtime changes
    std::thread::sleep(std::time::Duration::from_millis(50));
    std::fs::write(
        src.path().join("main.go"),
        b"package main\n\nfunc New() {}\nfunc Extra() {}\n",
    )
    .unwrap();

    // Second parse
    let cmd = leyline_cli_lib::Commands::Parse {
        source: src.path().to_path_buf(),
        output: db_path.clone(),
        lang: Some("go".to_string()),
    };
    leyline_cli_lib::run(cmd).await.unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let old_def: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM node_defs WHERE token = 'Old'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(old_def, 0, "Old should be gone");
    let new_def: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM node_defs WHERE token = 'New'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(new_def, 1);
    let extra_def: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM node_defs WHERE token = 'Extra'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(extra_def, 1);
}

#[tokio::test]
async fn test_incremental_deleted_file() {
    let src = tempfile::TempDir::new().unwrap();
    let out_dir = tempfile::TempDir::new().unwrap();
    let db_path = out_dir.path().join("incr-delete.db");

    std::fs::write(src.path().join("keep.go"), b"package main\n\nfunc Keep() {}\n").unwrap();
    std::fs::write(
        src.path().join("remove.go"),
        b"package main\n\nfunc Remove() {}\n",
    )
    .unwrap();

    // First parse
    let cmd = leyline_cli_lib::Commands::Parse {
        source: src.path().to_path_buf(),
        output: db_path.clone(),
        lang: Some("go".to_string()),
    };
    leyline_cli_lib::run(cmd).await.unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let remove_def: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM node_defs WHERE token = 'Remove'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(remove_def, 1);
    drop(conn);

    // Delete file from disk
    std::fs::remove_file(src.path().join("remove.go")).unwrap();

    // Second parse
    let cmd = leyline_cli_lib::Commands::Parse {
        source: src.path().to_path_buf(),
        output: db_path.clone(),
        lang: Some("go".to_string()),
    };
    leyline_cli_lib::run(cmd).await.unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let remove_def: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM node_defs WHERE token = 'Remove'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(remove_def, 0, "Remove should be deleted");
    let remove_source: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM _source WHERE id = 'remove.go'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(remove_source, 0);
    let keep_def: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM node_defs WHERE token = 'Keep'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(keep_def, 1, "Keep should still exist");
    let index_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM _file_index", [], |r| r.get(0))
        .unwrap();
    assert_eq!(index_count, 1);
}

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

/// Verify the embed-queue drainer: when a node is promoted to the queue,
/// the background loop picks it up, embeds the node's content via the
/// active Embedder, and writes the vector into the sidecar VectorIndex.
///
/// We seed a tiny living db with one file node, manually push to the queue
/// (skipping the MCP op path), spawn the drainer, and wait for the result.
#[cfg(feature = "vec")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_embed_queue_drainer_refreshes_index() {
    use leyline_cli_lib::daemon::{
        DaemonContext, DaemonState, EventRouter, NoExt,
    };
    use leyline_cli_lib::daemon::embed::{self, EmbedTask, Embedder, ZeroEmbedder};
    use leyline_cli_lib::daemon::vec_index::{register_vec, VectorIndex};
    use leyline_core::{Controller, create_arena};
    use std::sync::{Arc, Mutex, RwLock};

    register_vec();

    let dir = TempDir::new().unwrap();
    let arena_path = dir.path().join("test.arena");
    let ctrl_path = dir.path().join("test.ctrl");
    let _mmap = create_arena(&arena_path, 2 * 1024 * 1024).unwrap();
    let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
    ctrl.set_arena(&arena_path.to_string_lossy(), 2 * 1024 * 1024, 0).unwrap();
    drop(ctrl);

    let dim = 4;
    let index = Arc::new(VectorIndex::new(dim, None).unwrap());
    let embedder: Arc<dyn Embedder> = Arc::new(ZeroEmbedder { dim });

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE nodes (
            id TEXT PRIMARY KEY,
            parent_id TEXT,
            name TEXT,
            kind INTEGER,
            size INTEGER,
            mtime INTEGER,
            record TEXT
        );
        INSERT INTO nodes VALUES ('a.go', '', 'a.go', 0, 9, 1, 'package a');",
    )
    .unwrap();

    let queue: embed::EmbedQueue =
        Arc::new(Mutex::new(std::collections::BinaryHeap::new()));

    let ctx = Arc::new(DaemonContext {
        ctrl_path,
        ext: Arc::new(NoExt),
        router: EventRouter::new(16),
        live_db: std::sync::Mutex::new(conn),
        source_dir: None,
        lang_filter: None,
        enrichment_passes: vec![],
        state: Arc::new(RwLock::new(DaemonState::initializing())),
        vec_index: index.clone(),
        embedder,
        embed_queue: queue.clone(),
    });

    // Pre-condition: index is empty.
    assert_eq!(index.len().unwrap(), 0);

    // Push directly to bypass any other side-effect of MCP ops.
    queue.lock().unwrap().push(EmbedTask {
        priority: 1,
        node_id: "a.go".to_string(),
    });

    embed::start_drain(ctx);

    // Wait up to 2.5s (drain runs every 1s) for the index to populate.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2_500);
    let mut populated = false;
    while std::time::Instant::now() < deadline {
        if index.len().unwrap() == 1 {
            populated = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(populated, "drainer should have embedded a.go within 2.5s");
    assert!(
        index.get("a.go").unwrap().is_some(),
        "a.go embedding should be present",
    );
}

/// Verify that EmbeddingPass + op_vec_search round-trip through the daemon
/// with the default ZeroEmbedder. We build a DaemonContext directly with a
/// pre-populated VectorIndex (skipping the parse pass) and confirm:
/// 1. `op_vec_search` returns k results (zero distance — same vector).
/// 2. Result `node_id`s match what we inserted.
#[cfg(feature = "vec")]
#[tokio::test]
async fn test_op_vec_search_round_trip() {
    use leyline_cli_lib::daemon::{
        DaemonContext, DaemonState, EventRouter, NoExt,
    };
    use leyline_cli_lib::daemon::embed::{Embedder, ZeroEmbedder};
    use leyline_cli_lib::daemon::vec_index::{register_vec, VectorIndex};
    use leyline_core::{Controller, create_arena};
    use std::sync::{Arc, RwLock};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    register_vec();

    let dir = TempDir::new().unwrap();
    let arena_path = dir.path().join("test.arena");
    let ctrl_path = dir.path().join("test.ctrl");
    let _mmap = create_arena(&arena_path, 2 * 1024 * 1024).unwrap();
    let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
    ctrl.set_arena(&arena_path.to_string_lossy(), 2 * 1024 * 1024, 0).unwrap();
    drop(ctrl);

    let dim = 4;
    let index = Arc::new(VectorIndex::new(dim, None).unwrap());
    // Pre-populate so the test doesn't depend on the enrichment pipeline.
    index.insert("file/a.go", &[0.0_f32; 4]).unwrap();
    index.insert("file/b.go", &[0.0_f32; 4]).unwrap();
    index.insert("file/c.go", &[0.0_f32; 4]).unwrap();

    let embedder: Arc<dyn Embedder> = Arc::new(ZeroEmbedder { dim });

    let ctx = Arc::new(DaemonContext {
        ctrl_path,
        ext: Arc::new(NoExt),
        router: EventRouter::new(16),
        live_db: std::sync::Mutex::new(rusqlite::Connection::open_in_memory().unwrap()),
        source_dir: None,
        lang_filter: None,
        enrichment_passes: vec![],
        state: Arc::new(RwLock::new(DaemonState::initializing())),
        vec_index: index.clone(),
        embedder,
        embed_queue: Arc::new(std::sync::Mutex::new(std::collections::BinaryHeap::new())),
    });

    let sock_path = dir.path().join("vec_search.sock");
    leyline_cli_lib::daemon::socket::spawn(ctx, sock_path.clone());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let stream = UnixStream::connect(&sock_path).await.expect("connect");
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    writer
        .write_all(b"{\"op\":\"vec_search\",\"query\":\"hello\",\"k\":3}\n")
        .await
        .unwrap();

    let response = lines.next_line().await.unwrap().expect("response");
    let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();

    assert_eq!(parsed["ok"], true);
    let results = parsed["results"].as_array().expect("results array");
    assert_eq!(results.len(), 3);
    let ids: Vec<&str> = results
        .iter()
        .map(|r| r["node_id"].as_str().unwrap())
        .collect();
    for id in ["file/a.go", "file/b.go", "file/c.go"] {
        assert!(ids.contains(&id), "expected {id} in results, got {ids:?}");
    }
    // ZeroEmbedder + zero vectors → distance 0 for all matches.
    for r in results {
        let d = r["distance"].as_f64().unwrap();
        assert!(d < f64::EPSILON, "expected zero distance, got {d}");
    }
}

/// Verify that `op_status` reports lifecycle phase, head_sha, and per-pass enrichment status.
///
/// We construct a DaemonContext directly (skipping run_daemon) so we can
/// drive the state machine deterministically. The socket+UDS path is
/// already covered by `test_daemon_socket_dispatches_status_op`; this test
/// focuses on the readiness signal payload.
#[tokio::test]
async fn test_status_reports_phase_and_enrichment() {
    use leyline_cli_lib::daemon::{
        DaemonContext, DaemonPhase, DaemonState, EventRouter, NoExt, PassStatus,
    };
    use leyline_core::{Controller, create_arena};
    use std::sync::{Arc, RwLock};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let dir = TempDir::new().unwrap();
    let arena_path = dir.path().join("test.arena");
    let ctrl_path = dir.path().join("test.ctrl");
    let _mmap = create_arena(&arena_path, 2 * 1024 * 1024).unwrap();
    let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
    ctrl.set_arena(&arena_path.to_string_lossy(), 2 * 1024 * 1024, 0).unwrap();
    drop(ctrl);

    // Pre-populate state with one enrichment record + a head SHA + Ready phase.
    let mut state = DaemonState::initializing();
    state.phase = DaemonPhase::Ready;
    state.head_sha = Some("abc1234".to_string());
    state.last_reparse_at_ms = Some(1_700_000_000_000);
    state.enrichment.insert(
        "tree-sitter".to_string(),
        PassStatus {
            last_run_at_ms: Some(1_700_000_000_000),
            basis: Some(1),
            error: None,
        },
    );

    let ctx = Arc::new(DaemonContext {
        ctrl_path,
        ext: Arc::new(NoExt),
        router: EventRouter::new(16),
        live_db: std::sync::Mutex::new(rusqlite::Connection::open_in_memory().unwrap()),
        source_dir: None,
        lang_filter: None,
        enrichment_passes: vec![],
        state: Arc::new(RwLock::new(state)),
        #[cfg(feature = "vec")]
        vec_index: {
            leyline_cli_lib::daemon::vec_index::register_vec();
            Arc::new(leyline_cli_lib::daemon::vec_index::VectorIndex::new(4, None).unwrap())
        },
        #[cfg(feature = "vec")]
        embedder: Arc::new(leyline_cli_lib::daemon::embed::ZeroEmbedder { dim: 4 }),
        #[cfg(feature = "vec")]
        embed_queue: Arc::new(std::sync::Mutex::new(std::collections::BinaryHeap::new())),
    });

    let sock_path = dir.path().join("status_phase.sock");
    leyline_cli_lib::daemon::socket::spawn(ctx, sock_path.clone());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let stream = UnixStream::connect(&sock_path).await.expect("connect");
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    writer.write_all(b"{\"op\":\"status\"}\n").await.unwrap();

    let response = lines.next_line().await.unwrap().expect("response");
    let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();

    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["phase"], "ready");
    assert_eq!(parsed["head_sha"], "abc1234");
    assert_eq!(parsed["last_reparse_at_ms"], 1_700_000_000_000_i64);
    assert!(parsed["enrichment"].is_object());
    assert_eq!(parsed["enrichment"]["tree-sitter"]["basis"], 1);
    assert_eq!(
        parsed["enrichment"]["tree-sitter"]["last_run_at_ms"],
        1_700_000_000_000_i64,
    );
}

/// Verify scoped reparse only touches the files in `scope`.
///
/// Cold-parse three Go files into an in-memory db. Modify file A on disk.
/// Call `parse_into_conn` with `scope=Some(&["a.go"])`. Confirm:
///   - file A's _file_index row reflects the new mtime/size.
///   - files B and C are untouched (mtime/size match the cold-parse values).
///   - the scoped pass parses 1 (the others were never visited).
#[test]
fn test_scoped_reparse_only_touches_scoped_files() {
    use leyline_cli_lib::cmd_parse::parse_into_conn;
    use rusqlite::Connection;
    use std::collections::HashMap;

    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.go"), b"package m\n\nfunc A() {}\n").unwrap();
    fs::write(dir.path().join("b.go"), b"package m\n\nfunc B() {}\n").unwrap();
    fs::write(dir.path().join("c.go"), b"package m\n\nfunc C() {}\n").unwrap();

    let conn = Connection::open_in_memory().unwrap();
    let r1 = parse_into_conn(&conn, dir.path(), Some("go"), None).unwrap();
    assert_eq!(r1.parsed, 3, "cold parse should hit all 3 files");

    let snapshot: HashMap<String, (i64, i64)> = conn
        .prepare("SELECT path, mtime, size FROM _file_index")
        .unwrap()
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert_eq!(snapshot.len(), 3);

    // Wait long enough that mtime resolution actually advances.
    std::thread::sleep(std::time::Duration::from_millis(50));

    fs::write(dir.path().join("a.go"), b"package m\n\nfunc A() { let _ = 1; }\n").unwrap();

    let r2 = parse_into_conn(
        &conn,
        dir.path(),
        Some("go"),
        Some(&["a.go".to_string()]),
    )
    .unwrap();
    assert_eq!(r2.parsed, 1, "scoped reparse should only parse a.go");
    assert_eq!(r2.deleted, 0);
    assert_eq!(r2.changed_files, vec!["a.go".to_string()]);

    let after: HashMap<String, (i64, i64)> = conn
        .prepare("SELECT path, mtime, size FROM _file_index")
        .unwrap()
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect();

    assert_eq!(after.len(), 3, "all 3 files should still be tracked");
    assert_ne!(after["a.go"], snapshot["a.go"], "a.go mtime/size must change");
    assert_eq!(after["b.go"], snapshot["b.go"], "b.go must be untouched");
    assert_eq!(after["c.go"], snapshot["c.go"], "c.go must be untouched");
}

/// Verify scoped reparse handles a deleted file: vanished files in the scope
/// are removed from the index, files outside the scope remain.
#[test]
fn test_scoped_reparse_handles_deletion_in_scope() {
    use leyline_cli_lib::cmd_parse::parse_into_conn;
    use rusqlite::Connection;

    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.go"), b"package m\n\nfunc A() {}\n").unwrap();
    fs::write(dir.path().join("b.go"), b"package m\n\nfunc B() {}\n").unwrap();

    let conn = Connection::open_in_memory().unwrap();
    let r1 = parse_into_conn(&conn, dir.path(), Some("go"), None).unwrap();
    assert_eq!(r1.parsed, 2);

    fs::remove_file(dir.path().join("a.go")).unwrap();

    let r2 = parse_into_conn(
        &conn,
        dir.path(),
        Some("go"),
        Some(&["a.go".to_string()]),
    )
    .unwrap();
    assert_eq!(r2.parsed, 0);
    assert_eq!(r2.deleted, 1);

    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM _file_index", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1, "only b.go should remain in _file_index");
    let remaining: String = conn
        .query_row("SELECT path FROM _file_index", [], |r| r.get(0))
        .unwrap();
    assert_eq!(remaining, "b.go");
}

/// Verify `op_reparse` accepts a single-file `source` (the shape Claude
/// Code's PostToolUse hook produces) and auto-rewrites it to (parent,
/// scope=[basename]) instead of erroring with "not a directory".
#[tokio::test]
async fn test_op_reparse_accepts_single_file_source() {
    use leyline_cli_lib::daemon::{DaemonContext, DaemonState, EventRouter, NoExt};
    use leyline_core::{Controller, create_arena};
    use std::sync::{Arc, Mutex, RwLock};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    // Source tree with two go files. The hook will only "edit" one.
    let src = TempDir::new().unwrap();
    fs::write(src.path().join("a.go"), "package m\n\nfunc A() {}\n").unwrap();
    fs::write(src.path().join("b.go"), "package m\n\nfunc B() {}\n").unwrap();

    // Cold-parse so _file_index is populated.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    leyline_cli_lib::cmd_parse::parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();
    let snapshot: std::collections::HashMap<String, (i64, i64)> = conn
        .prepare("SELECT path, mtime, size FROM _file_index")
        .unwrap()
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert_eq!(snapshot.len(), 2);

    // Modify a.go and let the daemon serve.
    std::thread::sleep(std::time::Duration::from_millis(50));
    fs::write(src.path().join("a.go"), "package m\n\nfunc A() { /* edited */ }\n").unwrap();

    let dir = TempDir::new().unwrap();
    let arena_path = dir.path().join("test.arena");
    let ctrl_path = dir.path().join("test.ctrl");
    let _mmap = create_arena(&arena_path, 2 * 1024 * 1024).unwrap();
    let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
    ctrl.set_arena(&arena_path.to_string_lossy(), 2 * 1024 * 1024, 0).unwrap();
    drop(ctrl);

    let ctx = Arc::new(DaemonContext {
        ctrl_path,
        ext: Arc::new(NoExt),
        router: EventRouter::new(16),
        live_db: Mutex::new(conn),
        source_dir: Some(src.path().to_path_buf()),
        lang_filter: Some("go".to_string()),
        enrichment_passes: vec![],
        state: Arc::new(RwLock::new(DaemonState::initializing())),
        #[cfg(feature = "vec")]
        vec_index: {
            leyline_cli_lib::daemon::vec_index::register_vec();
            Arc::new(leyline_cli_lib::daemon::vec_index::VectorIndex::new(4, None).unwrap())
        },
        #[cfg(feature = "vec")]
        embedder: Arc::new(leyline_cli_lib::daemon::embed::ZeroEmbedder { dim: 4 }),
        #[cfg(feature = "vec")]
        embed_queue: Arc::new(Mutex::new(std::collections::BinaryHeap::new())),
    });

    let sock_path = dir.path().join("reparse.sock");
    leyline_cli_lib::daemon::socket::spawn(ctx.clone(), sock_path.clone());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // The hook posts {source: "<absolute file path>"} — what tool_input.file_path looks like.
    let edited = src.path().join("a.go");
    let body = serde_json::json!({
        "op": "reparse",
        "source": edited.to_string_lossy().to_string(),
    });

    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    writer.write_all(format!("{body}\n").as_bytes()).await.unwrap();
    let response = lines.next_line().await.unwrap().expect("response");
    let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();

    assert_eq!(parsed["ok"], true, "single-file reparse should succeed: {parsed}");
    assert_eq!(parsed["parsed"], 1, "only a.go should be reparsed");
    let changed = parsed["changed_files"].as_array().expect("changed_files array");
    let names: Vec<&str> = changed.iter().filter_map(|v| v.as_str()).collect();
    assert_eq!(names, vec!["a.go"], "scope should be exactly [a.go]");

    // Verify b.go was NOT touched (its mtime/size in _file_index is unchanged).
    let after: std::collections::HashMap<String, (i64, i64)> = ctx
        .live_db
        .lock()
        .unwrap()
        .prepare("SELECT path, mtime, size FROM _file_index")
        .unwrap()
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert_eq!(after.get("b.go"), snapshot.get("b.go"), "b.go must be untouched");
    assert_ne!(after.get("a.go"), snapshot.get("a.go"), "a.go must be updated");
}

/// Verify explicit `files: [...]` arg takes precedence over auto-derivation
/// and works with a directory `source`. This is the shape the cloister hook
/// will eventually use.
#[tokio::test]
async fn test_op_reparse_accepts_files_scope_with_dir_source() {
    use leyline_cli_lib::daemon::{DaemonContext, DaemonState, EventRouter, NoExt};
    use leyline_core::{Controller, create_arena};
    use std::sync::{Arc, Mutex, RwLock};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let src = TempDir::new().unwrap();
    fs::write(src.path().join("a.go"), "package m\n\nfunc A() {}\n").unwrap();
    fs::write(src.path().join("b.go"), "package m\n\nfunc B() {}\n").unwrap();
    fs::write(src.path().join("c.go"), "package m\n\nfunc C() {}\n").unwrap();

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    leyline_cli_lib::cmd_parse::parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();

    std::thread::sleep(std::time::Duration::from_millis(50));
    fs::write(src.path().join("b.go"), "package m\n\nfunc B() { /* edit */ }\n").unwrap();

    let dir = TempDir::new().unwrap();
    let arena_path = dir.path().join("test.arena");
    let ctrl_path = dir.path().join("test.ctrl");
    let _mmap = create_arena(&arena_path, 2 * 1024 * 1024).unwrap();
    let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
    ctrl.set_arena(&arena_path.to_string_lossy(), 2 * 1024 * 1024, 0).unwrap();
    drop(ctrl);

    let ctx = Arc::new(DaemonContext {
        ctrl_path,
        ext: Arc::new(NoExt),
        router: EventRouter::new(16),
        live_db: Mutex::new(conn),
        source_dir: Some(src.path().to_path_buf()),
        lang_filter: Some("go".to_string()),
        enrichment_passes: vec![],
        state: Arc::new(RwLock::new(DaemonState::initializing())),
        #[cfg(feature = "vec")]
        vec_index: {
            leyline_cli_lib::daemon::vec_index::register_vec();
            Arc::new(leyline_cli_lib::daemon::vec_index::VectorIndex::new(4, None).unwrap())
        },
        #[cfg(feature = "vec")]
        embedder: Arc::new(leyline_cli_lib::daemon::embed::ZeroEmbedder { dim: 4 }),
        #[cfg(feature = "vec")]
        embed_queue: Arc::new(Mutex::new(std::collections::BinaryHeap::new())),
    });

    let sock_path = dir.path().join("reparse-files.sock");
    leyline_cli_lib::daemon::socket::spawn(ctx, sock_path.clone());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let body = serde_json::json!({
        "op": "reparse",
        "source": src.path().to_string_lossy().to_string(),
        "files": ["b.go"],
    });

    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    writer.write_all(format!("{body}\n").as_bytes()).await.unwrap();
    let response = lines.next_line().await.unwrap().expect("response");
    let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();

    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["parsed"], 1);
    let changed: Vec<&str> = parsed["changed_files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(changed, vec!["b.go"]);
}

/// Verify the error phase is surfaced through `op_status`.
#[tokio::test]
async fn test_status_reports_error_phase() {
    use leyline_cli_lib::daemon::{DaemonContext, DaemonPhase, DaemonState, EventRouter, NoExt};
    use leyline_core::{Controller, create_arena};
    use std::sync::{Arc, RwLock};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let dir = TempDir::new().unwrap();
    let arena_path = dir.path().join("test.arena");
    let ctrl_path = dir.path().join("test.ctrl");
    let _mmap = create_arena(&arena_path, 2 * 1024 * 1024).unwrap();
    let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
    ctrl.set_arena(&arena_path.to_string_lossy(), 2 * 1024 * 1024, 0).unwrap();
    drop(ctrl);

    let mut state = DaemonState::initializing();
    state.phase = DaemonPhase::Error("boom: parse failed".to_string());

    let ctx = Arc::new(DaemonContext {
        ctrl_path,
        ext: Arc::new(NoExt),
        router: EventRouter::new(16),
        live_db: std::sync::Mutex::new(rusqlite::Connection::open_in_memory().unwrap()),
        source_dir: None,
        lang_filter: None,
        enrichment_passes: vec![],
        state: Arc::new(RwLock::new(state)),
        #[cfg(feature = "vec")]
        vec_index: {
            leyline_cli_lib::daemon::vec_index::register_vec();
            Arc::new(leyline_cli_lib::daemon::vec_index::VectorIndex::new(4, None).unwrap())
        },
        #[cfg(feature = "vec")]
        embedder: Arc::new(leyline_cli_lib::daemon::embed::ZeroEmbedder { dim: 4 }),
        #[cfg(feature = "vec")]
        embed_queue: Arc::new(std::sync::Mutex::new(std::collections::BinaryHeap::new())),
    });

    let sock_path = dir.path().join("status_error.sock");
    leyline_cli_lib::daemon::socket::spawn(ctx, sock_path.clone());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let stream = UnixStream::connect(&sock_path).await.expect("connect");
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    writer.write_all(b"{\"op\":\"status\"}\n").await.unwrap();

    let response = lines.next_line().await.unwrap().expect("response");
    let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();

    assert_eq!(parsed["phase"], "error");
    assert_eq!(parsed["error"], "boom: parse failed");
}
