//! Integration tests for leyline-cli-lib.

use std::fs;
use std::path::{Path, PathBuf};

use leyline_cli_lib::{Commands, EDITION};
use tempfile::TempDir;

// ── Shared test scaffolding ──────────────────────────────────────────────
//
// 14 of the integration tests construct an arena + controller + DaemonContext
// from scratch with mostly identical boilerplate. The two helpers below
// collapse that to two short calls per test:
//
//   let dir = TempDir::new().unwrap();
//   let (_arena, ctrl_path) = fresh_arena(dir.path());
//   let ctx = Arc::new(default_test_ctx(ctrl_path));
//
// Tests that need a non-default field (custom live_db, source_dir,
// state) construct via struct-update syntax:
//
//   let ctx = Arc::new(DaemonContext {
//       source_dir: Some(src.path().to_path_buf()),
//       ..default_test_ctx(ctrl_path)
//   });
//
// `..` covers all default fields, including the cfg-gated `vec_index`,
// `embedder`, and `embed_queue` — no need to spell them out per-test.

/// Create a fresh arena + controller pair under `dir`. Returns
/// `(arena_path, ctrl_path)`. The arena is sized at 2 MiB which is
/// enough headroom for any of our integration fixtures.
#[allow(dead_code)] // used by tests; rust sees only public surface
fn fresh_arena(dir: &Path) -> (PathBuf, PathBuf) {
    use leyline_core::{Controller, create_arena};
    let arena_path = dir.join("test.arena");
    let ctrl_path = dir.join("test.ctrl");
    let _mmap = create_arena(&arena_path, 2 * 1024 * 1024).unwrap();
    let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
    ctrl.set_arena(&arena_path.to_string_lossy(), 2 * 1024 * 1024)
        .unwrap();
    drop(ctrl);
    (arena_path, ctrl_path)
}

/// Build a vanilla `DaemonContext` for tests. Returns the value (not an
/// Arc) so callers can use struct-update syntax to override fields and
/// then wrap in `Arc::new(...)` themselves.
///
/// Defaults:
/// - `ext`: `NoExt`
/// - `router`: capacity 16 (small, tests don't need a big log)
/// - `live_db`: fresh `:memory:` connection (no schema)
/// - `source_dir`: None — tests that drive reparse override this
/// - `lang_filter`: None
/// - `enrichment_passes`: empty
/// - `state`: `DaemonState::initializing()`
/// - vec fields: 4-dim `ZeroEmbedder` + 4-dim VectorIndex + empty queue
#[allow(dead_code)]
fn default_test_ctx(ctrl_path: PathBuf) -> leyline_cli_lib::daemon::DaemonContext {
    use leyline_cli_lib::daemon::{DaemonContext, DaemonState, EventRouter, NoExt};
    use std::sync::{Arc, Mutex, RwLock};

    DaemonContext {
        ctrl_path,
        ext: Arc::new(NoExt),
        router: EventRouter::new(16),
        live_db: Mutex::new(rusqlite::Connection::open_in_memory().unwrap()),
        enrich_inflight: Arc::new(Mutex::new(std::collections::HashSet::new())),
        source_dir: None,
        lang_filter: None,
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
        sheaf: Arc::new(leyline_cli_lib::daemon::sheaf_ops::SheafState::new()),
    }
}

/// Spawn the daemon's UDS listener and wait until it accepts connections.
///
/// Replaces the previous `socket::spawn(...) + sleep(50ms)` pair scattered
/// across tests. The fixed sleep was timing-flaky on overloaded CI, and
/// arbitrary on healthy machines (bind() is synchronous, so the listener
/// is in the kernel queue immediately on return). This helper polls
/// `connect().await` instead — fast machines see ~1ms, slow machines wait
/// only as long as needed, and the 2s ceiling turns a wedged listener
/// into a clean test failure rather than a hang.
#[allow(dead_code)]
async fn spawn_test_socket(
    ctx: std::sync::Arc<leyline_cli_lib::daemon::DaemonContext>,
    sock_path: PathBuf,
) -> PathBuf {
    use tokio::net::UnixStream;

    let path = leyline_cli_lib::daemon::socket::spawn(ctx, sock_path);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if UnixStream::connect(&path).await.is_ok() {
            return path;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!(
        "UDS listener at {} did not accept within 2s",
        path.display()
    );
}

/// One UDS round trip: connect, write `body` (with trailing newline if
/// missing), read one response line, parse JSON. Centralizes the connect
/// + write + read + parse boilerplate that several tests duplicate.
///
/// Tests that drive multiple requests on one connection should still use
/// raw `UnixStream` — the helper is for single round-trip cases, which is
/// the common shape.
#[allow(dead_code)]
async fn uds_round_trip(sock: &Path, body: &str) -> serde_json::Value {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;
    let stream = UnixStream::connect(sock).await.expect("connect");
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    writer.write_all(body.as_bytes()).await.unwrap();
    if !body.ends_with('\n') {
        writer.write_all(b"\n").await.unwrap();
    }
    let line = lines.next_line().await.unwrap().expect("response");
    serde_json::from_str(&line).expect("response is JSON")
}

/// Write a Go file at `dir/<file>` containing a single empty function:
///   package <pkg>
///
///   func <fn_name>() {}
///
/// Centralizes the most-common test fixture so a future tweak to the
/// stub shape (Go syntax version, `package _` convention, etc.) is one
/// site, not 12+. For non-trivial fixtures (real LSP-style tests with
/// imports, bodies, multiple decls), inline `fs::write` is still fine.
#[allow(dead_code)]
fn write_empty_go_func(dir: &Path, file: &str, pkg: &str, fn_name: &str) {
    let content = format!("package {pkg}\n\nfunc {fn_name}() {{}}\n");
    fs::write(dir.join(file), content).expect("write go fixture");
}

/// Snapshot the `_file_index` table into `{path -> (mtime, size)}`.
/// Used by reparse-scope tests to compare before/after states — four
/// sites previously inlined this 8-line query+decode chain. Future
/// schema changes (e.g. adding hash column to _file_index) are now
/// one site, not four.
#[allow(dead_code)]
fn file_index_snapshot(
    conn: &rusqlite::Connection,
) -> std::collections::HashMap<String, (i64, i64)> {
    conn.prepare("SELECT path, mtime, size FROM _file_index")
        .unwrap()
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?),
            ))
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect()
}

/// Cold-parse a Go source directory into a fresh `:memory:` Connection.
/// The dominant test setup for "cold-parse, then assert" — 4+ sites
/// previously inlined this exact pair. Returns just the connection
/// because that's all most tests need; sites that want the
/// `ParseResult` keep calling `parse_into_conn` directly.
#[allow(dead_code)]
fn cold_parse_go(src_dir: &Path) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    leyline_cli_lib::cmd_parse::parse_into_conn(&conn, src_dir, Some("go"), None)
        .expect("cold parse Go fixture");
    conn
}

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

    leyline_cli_lib::run(cmd)
        .await
        .expect("parse should succeed");

    // Verify the database was created.
    assert!(db_path.exists(), "database file should exist");

    // Open and verify tables + row counts.
    let conn = rusqlite::Connection::open(&db_path).expect("open db");

    let nodes_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
        .expect("query nodes");
    // At minimum: root + 2 files + their AST children.
    assert!(
        nodes_count >= 3,
        "nodes should have at least 3 rows (root + 2 files), got {nodes_count}"
    );

    let source_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM _source", [], |r| r.get(0))
        .expect("query _source");
    assert_eq!(
        source_count, 2,
        "_source should have 2 rows (one per .go file)"
    );

    let ast_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM _ast", [], |r| r.get(0))
        .expect("query _ast");
    assert!(
        ast_count >= 2,
        "_ast should have at least 2 rows, got {ast_count}"
    );
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
    leyline_cli_lib::run(cmd)
        .await
        .expect("parse should succeed");

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
    leyline_cli_lib::run(cmd)
        .await
        .expect("splice should succeed");

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
    leyline_cli_lib::run(cmd)
        .await
        .expect("parse should succeed");
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
        )
        .expect("set arena in controller");
    }

    // Step 4: Run load.
    let cmd = Commands::Load {
        db: db_path.clone(),
        control: ctrl_path.clone(),
    };
    leyline_cli_lib::run(cmd)
        .await
        .expect("load should succeed");

    // Step 5: T2.4 — verify load advanced current_root from sentinel.
    let ctrl = Controller::open_or_create(&ctrl_path).expect("reopen controller");
    assert_ne!(
        ctrl.current_root(),
        [0u8; 32],
        "current_root should advance from zero sentinel after load"
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

    let serialized = source_conn.serialize("main").expect("serialize db");
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
        "", // root node id
        &arena_path,
        None, // no controller
        None, // node-lookup mode
    );
    assert!(
        result.is_ok(),
        "inspect root node should succeed: {:?}",
        result.err()
    );

    // Step 4: Test SQL mode.
    let result = leyline_cli_lib::cmd_inspect::cmd_inspect(
        "",
        &arena_path,
        None,
        Some("SELECT COUNT(*) FROM nodes"),
    );
    assert!(
        result.is_ok(),
        "inspect SQL mode should succeed: {:?}",
        result.err()
    );

    // Step 5: Verify a missing node returns an error.
    let result = leyline_cli_lib::cmd_inspect::cmd_inspect("nonexistent", &arena_path, None, None);
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
    let returned_ctrl =
        leyline_cli_lib::cmd_serve::setup_arena(&arena_path, arena_bytes, Some(&ctrl_path))
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

    // T2.4: verify controller state — current_root at zero sentinel +
    // arena path set. Pre-T2.4 used `generation() == 0` as the
    // "fresh" check; current_root sentinel is the new equivalent.
    let ctrl = Controller::open_or_create(&ctrl_path).expect("open controller");
    assert_eq!(
        ctrl.current_root(),
        [0u8; 32],
        "fresh controller should have zero-root sentinel"
    );
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

    let ctrl_path = leyline_cli_lib::cmd_serve::setup_arena(&arena_path, 2 * 1024 * 1024, None)
        .expect("setup_arena should succeed");

    // Should derive .ctrl extension from .arena
    let expected = dir.path().join("my.ctrl");
    assert_eq!(
        ctrl_path, expected,
        "derived ctrl path should use .ctrl extension"
    );
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
    use std::sync::Arc;

    let dir = TempDir::new().unwrap();
    let (_arena, ctrl_path) = fresh_arena(dir.path());
    let ctx = Arc::new(default_test_ctx(ctrl_path));

    let sock_path = dir.path().join("test.sock");
    spawn_test_socket(ctx, sock_path.clone()).await;

    let parsed = uds_round_trip(&sock_path, r#"{"op":"status"}"#).await;
    assert_eq!(parsed["ok"], true);
    // T2.4: status emits `current_root` (hex). Fresh ctrl → zero sentinel.
    assert_eq!(parsed["current_root"], "0".repeat(64));
}

#[tokio::test]
async fn test_daemon_ext_dispatches_to_extension() {
    use leyline_cli_lib::daemon::DaemonContext;
    use leyline_cli_lib::daemon::ext::DaemonExt;
    use std::sync::Arc;

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
    let (_arena, ctrl_path) = fresh_arena(dir.path());

    let ctx = Arc::new(DaemonContext {
        ext: Arc::new(TestExt),
        ..default_test_ctx(ctrl_path)
    });

    let sock_path = dir.path().join("ext_test.sock");
    spawn_test_socket(ctx, sock_path.clone()).await;

    // Custom op routes to extension.
    let parsed = uds_round_trip(&sock_path, r#"{"op":"custom_op"}"#).await;
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["custom"], "hello from extension");

    // Unknown op surfaces error string. Daemon is stateless across connections,
    // so a fresh round-trip suffices for the second probe.
    let parsed = uds_round_trip(&sock_path, r#"{"op":"nonexistent"}"#).await;
    assert!(parsed.get("error").is_some());
    let err_str = parsed["error"].as_str().unwrap();
    assert!(
        err_str.contains("unknown op"),
        "error should mention 'unknown op', got: {err_str}"
    );
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
    haystack.windows(needle.len()).position(|w| w == needle)
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

    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
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
    use std::sync::Arc;

    let dir = TempDir::new().unwrap();
    let (_arena, ctrl_path) = fresh_arena(dir.path());

    let ctx = Arc::new(default_test_ctx(ctrl_path));

    // Bind to port 0 (any free port), then read the assigned port. We pick
    // it by binding once, dropping, and using the same port; that's racy
    // but acceptable for this test. Instead, hand the port choice to the
    // OS via a quick probe.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let handle = leyline_cli_lib::daemon::mcp::spawn(ctx, None, port).expect("spawn MCP HTTP");

    // Wait for the HTTP listener to accept connections. Same discipline
    // as `spawn_test_socket` for UDS — a fixed sleep races on overloaded
    // CI; polling connect-readiness is robust on fast and slow machines
    // alike. (Caught by iter-35 adversarial review — magic sleep
    // surviving from before the spawn_test_socket pattern.)
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    // 1. tools/list
    let listing = mcp_post(port, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).await;
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

    // 4. tools/call → lsp_diagnostics on empty db (pre-enrichment, no
    //    `_lsp` table yet). Returns ok:true with an empty diagnostics
    //    array — the `query_lsp_rows_for_file` table-existence guard
    //    treats "table missing" as "no rows yet" rather than as an
    //    error. This matches `lsp_defs` / `lsp_refs` behavior on the
    //    same pre-enrichment state. Behavioral asymmetry between op
    //    families was caught + fixed by iter-35 adversarial review.
    let lsp = mcp_post(
        port,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"lsp_diagnostics","arguments":{"file":"/tmp/no.rs"}}}"#,
    )
    .await;
    assert_eq!(
        lsp["result"]["isError"], false,
        "pre-enrichment lsp_diagnostics must be ok:true (got {lsp})",
    );
    let inner: serde_json::Value =
        serde_json::from_str(lsp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(inner["ok"], true);
    assert_eq!(
        inner["diagnostics"].as_array().map(|a| a.len()),
        Some(0),
        "pre-enrichment must yield empty diagnostics array, got {inner}",
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
    use leyline_cli_lib::cmd_daemon::snapshot_to_arena;
    use leyline_cli_lib::cmd_parse::parse_into_conn;
    use leyline_core::{Controller, create_arena};

    let src = create_go_fixture();
    let arena_dir = TempDir::new().unwrap();
    let arena_path = arena_dir.path().join("test.arena");
    let ctrl_path = arena_dir.path().join("test.ctrl");

    // --- Phase 1: Cold start — parse into :memory:, snapshot to arena ---

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let result = parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();
    assert!(
        result.parsed >= 2,
        "should parse at least main.go + util.go"
    );
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
    ctrl.set_arena(&arena_path.to_string_lossy(), arena_size)
        .unwrap();

    // Snapshot living db to arena.
    snapshot_to_arena(&conn, &ctrl_path).unwrap();

    // T2.4: snapshot advances current_root from sentinel to BLAKE3.
    let root_after_snapshot = Controller::open_or_create(&ctrl_path)
        .unwrap()
        .current_root();
    assert_ne!(
        root_after_snapshot, [0u8; 32],
        "current_root should advance from sentinel after snapshot"
    );

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
            "main",
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

    // --- Phase 5: T2.4 — Snapshot again, verify root advanced ---

    snapshot_to_arena(&recovered, &ctrl_path).unwrap();

    let root_after_reparse = Controller::open_or_create(&ctrl_path)
        .unwrap()
        .current_root();
    assert_ne!(
        root_after_reparse, root_after_snapshot,
        "T2.4: current_root must advance after second snapshot (db changed)"
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
    ctrl.set_arena(&arena_path.to_string_lossy(), arena_size)
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
    let result = conn.deserialize_read_exact("main", std::io::Cursor::new(buf), buf.len(), false);

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
            Ok(0) => {}  // empty db, fine
            Err(_) => {} // not a valid db, fine
            Ok(n) => panic!("expected 0 tables in empty arena, got {n}"),
        }
    }
    // If deserialize itself failed, that's also fine — cold start path.
}

/// Stress test: multiple parse-snapshot cycles to verify no corruption.
#[test]
fn test_multiple_snapshot_cycles() {
    use leyline_cli_lib::cmd_daemon::snapshot_to_arena;
    use leyline_cli_lib::cmd_parse::parse_into_conn;
    use leyline_core::{Controller, create_arena};

    let src = create_go_fixture();
    let arena_dir = TempDir::new().unwrap();
    let arena_path = arena_dir.path().join("cycle.arena");
    let ctrl_path = arena_dir.path().join("cycle.ctrl");

    let arena_size = 4 * 1024 * 1024;
    let _mmap = create_arena(&arena_path, arena_size).unwrap();
    let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
    ctrl.set_arena(&arena_path.to_string_lossy(), arena_size)
        .unwrap();

    let conn = cold_parse_go(src.path());

    // T2.4: track root advancement across cycles. Each modified-and-snapshot
    // round must yield a strictly different root from the previous one.
    let mut prev_root = [0u8; 32];

    // Run 5 snapshot cycles, modifying the file each time.
    for i in 0..5 {
        fs::write(
            src.path().join("main.go"),
            format!("package main\n\nfunc main() {{\n\tprintln(\"iteration {i}\")\n}}\n"),
        )
        .unwrap();

        let result = parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();
        assert_eq!(result.parsed, 1, "cycle {i}: only main.go should reparse");

        snapshot_to_arena(&conn, &ctrl_path).unwrap();

        let cur_root = Controller::open_or_create(&ctrl_path)
            .unwrap()
            .current_root();
        assert_ne!(cur_root, [0u8; 32], "cycle {i}: root must leave sentinel");
        assert_ne!(
            cur_root, prev_root,
            "cycle {i}: root must advance from prior cycle"
        );
        prev_root = cur_root;
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
        .deserialize_read_exact("main", std::io::Cursor::new(buf), buf.len(), false)
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
// T2.2 — current_root content-addressing tests
// ---------------------------------------------------------------------------

/// T2.2: snapshot writes `current_root = BLAKE3(serialized db bytes)`
/// in the same atomic flip as generation. A fresh Controller opened
/// after the snapshot returns sees the matching root.
#[test]
fn snapshot_populates_current_root_with_blake3_of_db_bytes() {
    use leyline_cli_lib::cmd_daemon::snapshot_to_arena;
    use leyline_cli_lib::cmd_parse::parse_into_conn;
    use leyline_core::{Controller, create_arena};

    let src = create_go_fixture();
    let arena_dir = TempDir::new().unwrap();
    let arena_path = arena_dir.path().join("t22.arena");
    let ctrl_path = arena_dir.path().join("t22.ctrl");

    let arena_size = 4 * 1024 * 1024;
    let _mmap = create_arena(&arena_path, arena_size).unwrap();
    let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
    ctrl.set_arena(&arena_path.to_string_lossy(), arena_size)
        .unwrap();
    drop(ctrl);

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();

    // Independently compute the expected root: BLAKE3 of the bytes
    // snapshot_to_arena would serialize.
    let expected_db_bytes = conn.serialize("main").unwrap();
    let expected_root: [u8; 32] = blake3::hash(&expected_db_bytes).into();

    // Snapshot.
    snapshot_to_arena(&conn, &ctrl_path).unwrap();

    // Re-open Controller and read current_root. T2.4: generation removed
    // from public API; current_root is the sole identity.
    let r = Controller::open_or_create(&ctrl_path).unwrap();
    assert_eq!(
        r.current_root(),
        expected_root,
        "T2.2: current_root must equal BLAKE3(serialized db bytes)",
    );
    assert_ne!(
        r.current_root(),
        [0u8; 32],
        "T2.2: post-snapshot current_root must not be the zero sentinel",
    );
}

/// T2.2: a snapshot of an unchanged db produces the *same* current_root.
/// Pin idempotency — `current_root` is a pure function of the bytes.
#[test]
fn snapshot_idempotent_root_for_same_db_state() {
    use leyline_cli_lib::cmd_daemon::snapshot_to_arena;
    use leyline_cli_lib::cmd_parse::parse_into_conn;
    use leyline_core::{Controller, create_arena};

    let src = create_go_fixture();
    let arena_dir = TempDir::new().unwrap();
    let arena_path = arena_dir.path().join("t22-idem.arena");
    let ctrl_path = arena_dir.path().join("t22-idem.ctrl");

    let arena_size = 4 * 1024 * 1024;
    let _mmap = create_arena(&arena_path, arena_size).unwrap();
    let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
    ctrl.set_arena(&arena_path.to_string_lossy(), arena_size)
        .unwrap();
    drop(ctrl);

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();

    snapshot_to_arena(&conn, &ctrl_path).unwrap();
    let root_after_first = Controller::open_or_create(&ctrl_path)
        .unwrap()
        .current_root();

    // Snapshot again with no db changes. T2.4: root stays — purely
    // a function of bytes; no generation surface to observe.
    snapshot_to_arena(&conn, &ctrl_path).unwrap();
    let r = Controller::open_or_create(&ctrl_path).unwrap();
    let root_after_second = r.current_root();

    assert_eq!(
        root_after_second, root_after_first,
        "T2.2: re-snapshotting unchanged db preserves current_root \
         (pure function of bytes)",
    );
}

/// T2.2: db state changes → current_root changes. Negative pin: if the
/// hash were keyed on something other than db bytes (or computed at a
/// different stage), this would fail.
#[test]
fn snapshot_root_advances_when_db_changes() {
    use leyline_cli_lib::cmd_daemon::snapshot_to_arena;
    use leyline_cli_lib::cmd_parse::parse_into_conn;
    use leyline_core::{Controller, create_arena};

    let src = create_go_fixture();
    let arena_dir = TempDir::new().unwrap();
    let arena_path = arena_dir.path().join("t22-advance.arena");
    let ctrl_path = arena_dir.path().join("t22-advance.ctrl");

    let arena_size = 4 * 1024 * 1024;
    let _mmap = create_arena(&arena_path, arena_size).unwrap();
    let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
    ctrl.set_arena(&arena_path.to_string_lossy(), arena_size)
        .unwrap();
    drop(ctrl);

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();

    snapshot_to_arena(&conn, &ctrl_path).unwrap();
    let root_v1 = Controller::open_or_create(&ctrl_path)
        .unwrap()
        .current_root();

    // Modify a file, reparse, snapshot again.
    fs::write(
        src.path().join("main.go"),
        "package main\n\nfunc main() {\n\tprintln(\"changed\")\n}\n",
    )
    .unwrap();
    parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();
    snapshot_to_arena(&conn, &ctrl_path).unwrap();

    let root_v2 = Controller::open_or_create(&ctrl_path)
        .unwrap()
        .current_root();

    assert_ne!(
        root_v2, root_v1,
        "T2.2: db change must produce different current_root",
    );
    assert_ne!(
        root_v2, [0u8; 32],
        "post-change root must not be zero sentinel"
    );
}

// ---------------------------------------------------------------------------
// T2.3 — reader-side hash verification tests
// ---------------------------------------------------------------------------

/// T2.3 happy path: post-T2.2 snapshot writes a non-zero current_root;
/// reader's `SqliteGraph::from_arena` recomputes BLAKE3 of the active
/// buffer, finds it matches, and proceeds to deserialize. End-to-end
/// pin: producer publishes content-addressed bytes, consumer verifies
/// before use, both sides agree.
#[test]
fn t23_reader_accepts_arena_when_root_matches() {
    use leyline_cli_lib::cmd_daemon::snapshot_to_arena;
    use leyline_cli_lib::cmd_parse::parse_into_conn;
    use leyline_core::{Controller, create_arena};
    use leyline_fs::SqliteGraph;

    let src = create_go_fixture();
    let arena_dir = TempDir::new().unwrap();
    let arena_path = arena_dir.path().join("t23-ok.arena");
    let ctrl_path = arena_dir.path().join("t23-ok.ctrl");

    let arena_size = 4 * 1024 * 1024;
    let _mmap = create_arena(&arena_path, arena_size).unwrap();
    let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
    ctrl.set_arena(&arena_path.to_string_lossy(), arena_size)
        .unwrap();
    drop(ctrl);

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();
    snapshot_to_arena(&conn, &ctrl_path).unwrap();

    // Reader path — should succeed because the writer published the
    // matching root via set_arena_with_root.
    let graph = SqliteGraph::from_arena(&ctrl_path)
        .expect("T2.3: reader must accept arena when current_root matches BLAKE3(buffer)");
    // Sanity: graph is usable.
    let _ = graph.conn();
}

/// T2.3 failure path: corrupt the arena buffer between the writer's
/// publish and the reader's load. Reader must refuse to deserialize
/// — the substrate's content-addressed correctness pin in action.
#[test]
fn t23_reader_refuses_arena_when_buffer_corrupted() {
    use leyline_cli_lib::cmd_daemon::snapshot_to_arena;
    use leyline_cli_lib::cmd_parse::parse_into_conn;
    use leyline_core::{ArenaHeader, Controller, create_arena};
    use leyline_fs::SqliteGraph;

    let src = create_go_fixture();
    let arena_dir = TempDir::new().unwrap();
    let arena_path = arena_dir.path().join("t23-corrupt.arena");
    let ctrl_path = arena_dir.path().join("t23-corrupt.ctrl");

    let arena_size = 4 * 1024 * 1024;
    let _mmap = create_arena(&arena_path, arena_size).unwrap();
    let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
    ctrl.set_arena(&arena_path.to_string_lossy(), arena_size)
        .unwrap();
    drop(ctrl);

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, src.path(), Some("go"), None).unwrap();
    snapshot_to_arena(&conn, &ctrl_path).unwrap();

    // Corrupt one byte in the active buffer. Choose an offset deep
    // inside the SQLite payload (past the file header, so the reader
    // still recognizes it as SQLite — only the hash should fail).
    {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&arena_path)
            .unwrap();
        let mut mmap = unsafe { memmap2::MmapMut::map_mut(&file).unwrap() };
        let header: ArenaHeader =
            *bytemuck::from_bytes(&mmap[..std::mem::size_of::<ArenaHeader>()]);
        let offset = header
            .active_buffer_offset(arena_size)
            .expect("valid header") as usize;
        // Flip a byte deep inside the buffer (past SQLite header @ ~100B).
        mmap[offset + 500] ^= 0xFF;
        mmap.flush().unwrap();
    }

    let result = SqliteGraph::from_arena(&ctrl_path);
    let err = match result {
        Ok(_) => panic!("T2.3: reader must refuse arena when buffer hash != current_root"),
        Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("arena root mismatch") || msg.contains("substrate corruption"),
        "T2.3 error must clearly identify the substrate-corruption failure mode (got: {msg})",
    );
}

/// T2.4 hardening: post-cutover, a zero `current_root` paired with a
/// **non-empty** payload (data_size > 0) is treated as a downgrade
/// attempt and rejected. Pre-T2.4 the reader silently skipped
/// verification on the zero sentinel — that left a hole where a
/// process able to write 32 bytes could disable content verification
/// while leaving arbitrary bytes for sqlite3_deserialize. The hard
/// V2 cutover removes any legacy producer of "data + no root", so
/// the skip is no longer needed and is gone.
#[test]
fn t24_reader_rejects_zero_root_with_data() {
    use leyline_core::{Controller, create_arena, layout::write_to_arena};
    use leyline_fs::SqliteGraph;

    let arena_dir = TempDir::new().unwrap();
    let arena_path = arena_dir.path().join("t24-zero-with-data.arena");
    let ctrl_path = arena_dir.path().join("t24-zero-with-data.ctrl");

    let arena_size = 4 * 1024 * 1024;
    let mut mmap = create_arena(&arena_path, arena_size).unwrap();

    // Real SQLite db in the buffer (data_size > 0 after write_to_arena).
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
        .unwrap();
    let db_bytes = conn.serialize("main").unwrap();
    write_to_arena(&mut mmap, &db_bytes).unwrap();

    let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
    // set_arena (no root) — leaves current_root at zero sentinel.
    // Pre-T2.4 readers skipped verification here; T2.4 rejects.
    ctrl.set_arena(&arena_path.to_string_lossy(), arena_size)
        .unwrap();
    assert_eq!(
        ctrl.current_root(),
        [0u8; 32],
        "set_arena leaves current_root at zero sentinel",
    );
    drop(ctrl);

    let result = SqliteGraph::from_arena(&ctrl_path);
    let err = match result {
        Ok(_) => panic!(
            "T2.4: reader must reject zero-root + non-empty data \
             (downgrade hole closed)"
        ),
        Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("zero sentinel") || msg.contains("substrate identity"),
        "T2.4 error must identify the missing-root downgrade case (got: {msg})",
    );
}

/// T2.4 fresh-arena path: a brand-new arena with no payload (data_size
/// == 0) and zero current_root is still legal — there's nothing to
/// verify and nothing to deserialize. The reader passes verification
/// (the `data_size == 0` carve-out in `verify_arena_root`) but then
/// errors at `sqlite3_deserialize` because empty bytes aren't a
/// valid SQLite database. The pin: must reach the SQLite layer with
/// a non-substrate error message, NOT the downgrade rejection.
#[test]
fn t24_reader_accepts_zero_root_with_empty_data() {
    use leyline_core::{Controller, create_arena};
    use leyline_fs::SqliteGraph;

    let arena_dir = TempDir::new().unwrap();
    let arena_path = arena_dir.path().join("t24-fresh.arena");
    let ctrl_path = arena_dir.path().join("t24-fresh.ctrl");

    let arena_size = 4 * 1024 * 1024;
    let _mmap = create_arena(&arena_path, arena_size).unwrap();

    let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
    ctrl.set_arena(&arena_path.to_string_lossy(), arena_size)
        .unwrap();
    drop(ctrl);

    // Fresh arena: header.data_size == 0, current_root == [0;32].
    // The verifier returns Ok(empty slice); SQLite then errors on
    // empty bytes — that's a from_bytes-level failure, not the
    // "downgrade" path tested above. Three assertions:
    //   1. The result MUST be an error (empty bytes ≠ valid SQLite).
    //      A future change that made empty deserialize succeed would
    //      slip past the prior "if let Err" check tautologically;
    //      forcing Err here keeps the test honest.
    //   2. The error MUST NOT be the substrate-identity rejection.
    //   3. The error MUST be from the SQLite layer (recognizable
    //      via the canonical `sqlite3_deserialize` context string).
    let result = SqliteGraph::from_arena(&ctrl_path);
    let err = result.err().expect(
        "T2.4: fresh empty arena must error at sqlite3_deserialize \
         (empty bytes are not a valid SQLite db); a future Ok here \
         would mean the verifier silently accepted unverified data",
    );
    let msg = format!("{err:#}");
    assert!(
        !(msg.contains("zero sentinel") || msg.contains("substrate identity")),
        "T2.4: fresh empty arena must not trip the downgrade rejection \
         (data_size == 0 is the legitimate empty case); got: {msg}",
    );
    assert!(
        msg.contains("sqlite3_deserialize"),
        "T2.4: fresh-arena failure must come from the SQLite layer, \
         not from the substrate verifier; got: {msg}",
    );
}

/// T2.3: when current_root is non-zero AND the buffer's bytes don't
/// hash to it, the reader returns a clear "arena root mismatch" error
/// rather than crashing or running sqlite3_deserialize on corrupted
/// data. Catches substrate / non-substrate confusion (e.g., wrong
/// arena file passed in or post-publish tampering).
#[test]
fn t23_reader_errors_clearly_on_non_sqlite_buffer() {
    use leyline_core::{ArenaHeader, Controller, create_arena, layout::write_to_arena};
    use leyline_fs::SqliteGraph;

    let arena_dir = TempDir::new().unwrap();
    let arena_path = arena_dir.path().join("t23-nonsqlite.arena");
    let ctrl_path = arena_dir.path().join("t23-nonsqlite.ctrl");

    let arena_size = 4 * 1024 * 1024;
    let mut mmap = create_arena(&arena_path, arena_size).unwrap();
    // Write some valid SQLite bytes via write_to_arena so data_size > 0.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
        .unwrap();
    let db_bytes = conn.serialize("main").unwrap();
    write_to_arena(&mut mmap, &db_bytes).unwrap();
    drop(mmap);

    // Now corrupt the active buffer's prefix in-place, mimicking
    // post-publish tampering. data_size is unchanged, so the verifier
    // hashes data_size bytes (including the corruption) and the
    // computed BLAKE3 will not match the published root.
    {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&arena_path)
            .unwrap();
        let mut mmap = unsafe { memmap2::MmapMut::map_mut(&file).unwrap() };
        let header: ArenaHeader =
            *bytemuck::from_bytes(&mmap[..std::mem::size_of::<ArenaHeader>()]);
        let offset = header
            .active_buffer_offset(arena_size)
            .expect("valid header") as usize;
        mmap[offset..offset + 16].copy_from_slice(b"NOT-A-SQLITE-DB!");
        mmap.flush().unwrap();
    }

    let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
    // Publish a non-zero root that does NOT match the (now-corrupt)
    // buffer hash → verifier must reject.
    ctrl.set_arena_with_root(&arena_path.to_string_lossy(), arena_size, [0xAB; 32])
        .unwrap();
    drop(ctrl);

    let result = SqliteGraph::from_arena(&ctrl_path);
    let err = match result {
        Ok(_) => panic!("T2.3/T2.4: must error on non-SQLite buffer"),
        Err(e) => e,
    };
    let msg = format!("{err:#}");
    // T2.4: post-VERSION-bump the verifier hashes buf[..data_size]
    // and compares against current_root before any SQLite parse.
    // Garbage bytes hashed against a hand-set non-zero root produce
    // a content-addressed mismatch, not an SQLite-format error.
    assert!(
        msg.contains("arena root mismatch"),
        "T2.4 error must identify content-addressed mismatch (got: {msg})",
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

    write_empty_go_func(src.path(), "a.go", "main", "A");
    write_empty_go_func(src.path(), "b.go", "main", "B");

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

    write_empty_go_func(src.path(), "main.go", "main", "Old");

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

    write_empty_go_func(src.path(), "keep.go", "main", "Keep");
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

/// End-to-end pin for the schema-bloat fix: delete_file_rows must
/// clean up _lsp* table rows when a file is removed AND the
/// integration with cmd_parse must trigger that cleanup.
///
/// The unit test in leyline-ts (delete_file_rows_cleans_lsp_tables_
/// when_present) pins the helper behavior. This integration test pins
/// that the helper actually fires from the reparse path on a
/// real-deletion-then-reparse cycle: without the integration coverage
/// a refactor that stopped calling delete_file_rows from cmd_parse
/// would silently regress at registry scale (orphan _lsp rows
/// accumulating across file churn).
#[tokio::test]
async fn test_incremental_deletion_cleans_lsp_orphans() {
    let src = tempfile::TempDir::new().unwrap();
    let out_dir = tempfile::TempDir::new().unwrap();
    let db_path = out_dir.path().join("incr-lsp-cleanup.db");

    write_empty_go_func(src.path(), "keep.go", "main", "Keep");
    std::fs::write(
        src.path().join("remove.go"),
        b"package main\n\nfunc Remove() {}\n",
    )
    .unwrap();

    // First parse to populate nodes.
    let cmd = leyline_cli_lib::Commands::Parse {
        source: src.path().to_path_buf(),
        output: db_path.clone(),
        lang: Some("go".to_string()),
    };
    leyline_cli_lib::run(cmd).await.unwrap();

    // Simulate LSP enrichment having run: create the leyline-lsp
    // schema directly + insert rows for both files. We use the same
    // shape as leyline_lsp::project::create_lsp_schema (no cross-crate
    // dep needed; the schema is a small constant string).
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE _lsp (
                node_id TEXT PRIMARY KEY,
                symbol_kind TEXT,
                detail TEXT,
                start_line INTEGER NOT NULL,
                start_col INTEGER NOT NULL,
                end_line INTEGER NOT NULL,
                end_col INTEGER NOT NULL,
                diagnostics TEXT
            );
            CREATE TABLE _lsp_hover (node_id TEXT PRIMARY KEY, hover_text TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO _lsp (node_id, symbol_kind, detail, start_line, start_col, end_line, end_col) \
             VALUES ('keep.go/func', 'function', 'k', 0, 0, 1, 0), \
                    ('remove.go/func', 'function', 'r', 0, 0, 1, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO _lsp_hover (node_id, hover_text) VALUES \
             ('keep.go/func', 'k-doc'), ('remove.go/func', 'r-doc')",
            [],
        )
        .unwrap();
    }

    // Delete the file from disk.
    std::fs::remove_file(src.path().join("remove.go")).unwrap();

    // Reparse — this must trigger delete_file_rows, which must in turn
    // clean up the _lsp* rows for remove.go.
    let cmd = leyline_cli_lib::Commands::Parse {
        source: src.path().to_path_buf(),
        output: db_path.clone(),
        lang: Some("go".to_string()),
    };
    leyline_cli_lib::run(cmd).await.unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();

    // remove.go's _lsp rows: gone.
    let remove_lsp: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM _lsp WHERE node_id LIKE 'remove.go%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(remove_lsp, 0, "_lsp rows for remove.go must be cleaned up");
    let remove_hover: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM _lsp_hover WHERE node_id LIKE 'remove.go%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        remove_hover, 0,
        "_lsp_hover rows for remove.go must be cleaned up",
    );

    // keep.go's _lsp rows: intact.
    let keep_lsp: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM _lsp WHERE node_id LIKE 'keep.go%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(keep_lsp, 1, "_lsp rows for keep.go must NOT be cleaned up");
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
    let def_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM node_defs", [], |r| r.get(0))
        .unwrap();
    assert!(
        def_count >= 2,
        "should have at least 2 defs, got {def_count}"
    );

    let main_def: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM node_defs WHERE token = 'main'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(main_def, 1, "should have 'main' def");

    let helper_def: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM node_defs WHERE token = 'helper'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(helper_def, 1, "should have 'helper' def");

    // node_refs: should have calls
    let ref_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM node_refs", [], |r| r.get(0))
        .unwrap();
    assert!(
        ref_count >= 3,
        "should have at least 3 refs, got {ref_count}"
    );

    let println_ref: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM node_refs WHERE token = 'fmt.Println'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(println_ref >= 1, "should have 'fmt.Println' ref");

    let helper_ref: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM node_refs WHERE token = 'helper'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(helper_ref >= 1, "should have 'helper' call ref");

    // _imports
    let import_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM _imports", [], |r| r.get(0))
        .unwrap();
    assert_eq!(import_count, 2, "should have 2 imports");

    let auth_import: String = conn
        .query_row("SELECT path FROM _imports WHERE alias = 'auth'", [], |r| {
            r.get(0)
        })
        .unwrap();
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
    )
    .unwrap();

    let cmd = leyline_cli_lib::Commands::Parse {
        source: src.path().to_path_buf(),
        output: db_path.clone(),
        lang: Some("go".to_string()),
    };
    leyline_cli_lib::run(cmd).await.unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();

    // Verify all tables mache expects exist
    let tables: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };

    assert!(
        tables.contains(&"nodes".to_string()),
        "missing nodes: {tables:?}"
    );
    assert!(
        tables.contains(&"_ast".to_string()),
        "missing _ast: {tables:?}"
    );
    assert!(
        tables.contains(&"_source".to_string()),
        "missing _source: {tables:?}"
    );
    assert!(
        tables.contains(&"node_refs".to_string()),
        "missing node_refs: {tables:?}"
    );
    assert!(
        tables.contains(&"node_defs".to_string()),
        "missing node_defs: {tables:?}"
    );
    assert!(
        tables.contains(&"_imports".to_string()),
        "missing _imports: {tables:?}"
    );

    // Verify mache fast-path trigger
    let nodes_exists: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='nodes'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(nodes_exists, 1);

    // Verify node_refs columns match mache's expected query pattern
    // mache does: SELECT node_id FROM node_refs WHERE token = ?
    conn.execute(
        "SELECT token, node_id, source_id FROM node_refs LIMIT 0",
        [],
    )
    .unwrap();
    conn.execute(
        "SELECT token, node_id, source_id FROM node_defs LIMIT 0",
        [],
    )
    .unwrap();
    conn.execute("SELECT alias, path, source_id FROM _imports LIMIT 0", [])
        .unwrap();
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
    use leyline_cli_lib::daemon::DaemonContext;
    use leyline_cli_lib::daemon::embed::{self, EmbedTask, Embedder, ZeroEmbedder};
    use leyline_cli_lib::daemon::vec_index::{VectorIndex, register_vec};

    use std::sync::{Arc, Mutex};

    register_vec();

    let dir = TempDir::new().unwrap();
    let (_arena, ctrl_path) = fresh_arena(dir.path());

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

    let queue: embed::EmbedQueue = Arc::new(Mutex::new(std::collections::BinaryHeap::new()));

    let ctx = Arc::new(DaemonContext {
        live_db: std::sync::Mutex::new(conn),
        vec_index: index.clone(),
        embedder,
        embed_queue: queue.clone(),
        sheaf: Arc::new(leyline_cli_lib::daemon::sheaf_ops::SheafState::new()),
        ..default_test_ctx(ctrl_path)
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
    use leyline_cli_lib::daemon::DaemonContext;
    use leyline_cli_lib::daemon::embed::{Embedder, ZeroEmbedder};
    use leyline_cli_lib::daemon::vec_index::{VectorIndex, register_vec};
    use std::sync::Arc;

    register_vec();

    let dir = TempDir::new().unwrap();
    let (_arena, ctrl_path) = fresh_arena(dir.path());

    let dim = 4;
    let index = Arc::new(VectorIndex::new(dim, None).unwrap());
    // Pre-populate so the test doesn't depend on the enrichment pipeline.
    index.insert("file/a.go", &[0.0_f32; 4]).unwrap();
    index.insert("file/b.go", &[0.0_f32; 4]).unwrap();
    index.insert("file/c.go", &[0.0_f32; 4]).unwrap();

    let embedder: Arc<dyn Embedder> = Arc::new(ZeroEmbedder { dim });

    let ctx = Arc::new(DaemonContext {
        vec_index: index.clone(),
        embedder,
        embed_queue: Arc::new(std::sync::Mutex::new(std::collections::BinaryHeap::new())),
        sheaf: Arc::new(leyline_cli_lib::daemon::sheaf_ops::SheafState::new()),
        ..default_test_ctx(ctrl_path)
    });

    let sock_path = dir.path().join("vec_search.sock");
    spawn_test_socket(ctx, sock_path.clone()).await;

    let parsed = uds_round_trip(&sock_path, r#"{"op":"vec_search","query":"hello","k":3}"#).await;

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
    use leyline_cli_lib::daemon::{DaemonContext, DaemonPhase, DaemonState, PassStatus};
    use std::sync::{Arc, RwLock};

    let dir = TempDir::new().unwrap();
    let (_arena, ctrl_path) = fresh_arena(dir.path());

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
        state: Arc::new(RwLock::new(state)),
        ..default_test_ctx(ctrl_path)
    });

    let sock_path = dir.path().join("status_phase.sock");
    spawn_test_socket(ctx, sock_path.clone()).await;

    let parsed = uds_round_trip(&sock_path, r#"{"op":"status"}"#).await;

    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["phase"], "ready");
    assert_eq!(parsed["head_sha"], "abc1234");
    // Post-b0ea2e: capnp-json codec emits Int64 as JSON strings.
    assert_eq!(parsed["last_reparse_at_ms"], "1700000000000");
    // Post-b0ea2e (b0ea2e reshape): `enrichment_typed` is a typed JSON
    // array of `{name, status: PassStatus}` entries. No double parse.
    // The legacy `enrichment :Text` field is deliberately not emitted.
    let typed = parsed["enrichment_typed"]
        .as_array()
        .expect("enrichment_typed is a JSON array");
    let entry = typed
        .iter()
        .find(|e| e["name"] == "tree-sitter")
        .expect("tree-sitter pass present");
    // Int64 fields ride as JSON strings under capnp-json.
    assert_eq!(entry["status"]["basis"], "1");
    assert_eq!(entry["status"]["last_run_at_ms"], "1700000000000");
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

    let dir = TempDir::new().unwrap();
    write_empty_go_func(dir.path(), "a.go", "m", "A");
    write_empty_go_func(dir.path(), "b.go", "m", "B");
    write_empty_go_func(dir.path(), "c.go", "m", "C");

    let conn = Connection::open_in_memory().unwrap();
    let r1 = parse_into_conn(&conn, dir.path(), Some("go"), None).unwrap();
    assert_eq!(r1.parsed, 3, "cold parse should hit all 3 files");

    let snapshot = file_index_snapshot(&conn);
    assert_eq!(snapshot.len(), 3);

    // Wait long enough that mtime resolution actually advances.
    std::thread::sleep(std::time::Duration::from_millis(50));

    fs::write(
        dir.path().join("a.go"),
        b"package m\n\nfunc A() { let _ = 1; }\n",
    )
    .unwrap();

    let r2 = parse_into_conn(&conn, dir.path(), Some("go"), Some(&["a.go".to_string()])).unwrap();
    assert_eq!(r2.parsed, 1, "scoped reparse should only parse a.go");
    assert_eq!(r2.deleted, 0);
    assert_eq!(r2.changed_files, vec!["a.go".to_string()]);

    let after = file_index_snapshot(&conn);

    assert_eq!(after.len(), 3, "all 3 files should still be tracked");
    assert_ne!(
        after["a.go"], snapshot["a.go"],
        "a.go mtime/size must change"
    );
    assert_eq!(after["b.go"], snapshot["b.go"], "b.go must be untouched");
    assert_eq!(after["c.go"], snapshot["c.go"], "c.go must be untouched");
}

/// Scoped reparse must NOT trigger sweep_orphaned_dirs — that sweep
/// walks the full _file_index and would incorrectly drop dir nodes
/// whose out-of-scope file siblings weren't reloaded into this run.
/// At registry scale (50k+ files, 5 dirty), the sweep would catastrophic
/// ally delete dir nodes whose only file in this scoped pass was the
/// dirty one. Pin: after a scoped reparse, dir nodes for non-edited
/// siblings still exist.
#[test]
fn test_scoped_reparse_preserves_sibling_dir_nodes() {
    use leyline_cli_lib::cmd_parse::parse_into_conn;
    use rusqlite::Connection;

    let dir = TempDir::new().unwrap();
    let pkg = dir.path().join("pkg");
    fs::create_dir(&pkg).unwrap();
    write_empty_go_func(&pkg, "a.go", "pkg", "A");
    write_empty_go_func(&pkg, "b.go", "pkg", "B");

    let conn = Connection::open_in_memory().unwrap();
    parse_into_conn(&conn, dir.path(), Some("go"), None).unwrap();

    // Verify pkg/ dir node exists after cold parse.
    let pkg_dir_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM nodes WHERE id = 'pkg' AND kind = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        pkg_dir_count, 1,
        "pkg/ dir node must exist after cold parse"
    );

    // Modify only a.go and run a SCOPED reparse.
    std::thread::sleep(std::time::Duration::from_millis(50));
    fs::write(
        pkg.join("a.go"),
        b"package pkg\n\nfunc A() { let _ = 1; }\n",
    )
    .unwrap();
    parse_into_conn(
        &conn,
        dir.path(),
        Some("go"),
        Some(&["pkg/a.go".to_string()]),
    )
    .unwrap();

    // Critical pin: pkg/ dir node must STILL exist. If sweep_orphaned_
    // dirs ran during the scoped pass, it would have deleted pkg/
    // because b.go (the only other child) wasn't reloaded into the
    // scoped run's _file_index addition.
    let pkg_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM nodes WHERE id = 'pkg' AND kind = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pkg_after, 1, "pkg/ dir node must survive scoped reparse");
    // And b.go's file row also.
    let b_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM nodes WHERE id = 'pkg/b.go'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(b_count, 1, "pkg/b.go file row must survive scoped reparse");
}

/// Verify scoped reparse handles a deleted file: vanished files in the scope
/// are removed from the index, files outside the scope remain.
#[test]
fn test_scoped_reparse_handles_deletion_in_scope() {
    use leyline_cli_lib::cmd_parse::parse_into_conn;
    use rusqlite::Connection;

    let dir = TempDir::new().unwrap();
    write_empty_go_func(dir.path(), "a.go", "m", "A");
    write_empty_go_func(dir.path(), "b.go", "m", "B");

    let conn = Connection::open_in_memory().unwrap();
    let r1 = parse_into_conn(&conn, dir.path(), Some("go"), None).unwrap();
    assert_eq!(r1.parsed, 2);

    fs::remove_file(dir.path().join("a.go")).unwrap();

    let r2 = parse_into_conn(&conn, dir.path(), Some("go"), Some(&["a.go".to_string()])).unwrap();
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
    use leyline_cli_lib::daemon::DaemonContext;
    use std::sync::{Arc, Mutex};

    // Source tree with two go files. The hook will only "edit" one.
    let src = TempDir::new().unwrap();
    write_empty_go_func(src.path(), "a.go", "m", "A");
    write_empty_go_func(src.path(), "b.go", "m", "B");

    // Cold-parse so _file_index is populated.
    let conn = cold_parse_go(src.path());
    let snapshot = file_index_snapshot(&conn);
    assert_eq!(snapshot.len(), 2);

    // Modify a.go and let the daemon serve.
    std::thread::sleep(std::time::Duration::from_millis(50));
    fs::write(
        src.path().join("a.go"),
        "package m\n\nfunc A() { /* edited */ }\n",
    )
    .unwrap();

    let dir = TempDir::new().unwrap();
    let (_arena, ctrl_path) = fresh_arena(dir.path());

    let ctx = Arc::new(DaemonContext {
        live_db: Mutex::new(conn),
        source_dir: Some(src.path().to_path_buf()),
        lang_filter: Some("go".to_string()),
        ..default_test_ctx(ctrl_path)
    });

    let sock_path = dir.path().join("reparse.sock");
    spawn_test_socket(ctx.clone(), sock_path.clone()).await;

    // The hook posts {source: "<absolute file path>"} — what tool_input.file_path looks like.
    let edited = src.path().join("a.go");
    let body = serde_json::json!({
        "op": "reparse",
        "source": edited.to_string_lossy().to_string(),
    });

    let parsed = uds_round_trip(&sock_path, &body.to_string()).await;

    assert_eq!(
        parsed["ok"], true,
        "single-file reparse should succeed: {parsed}"
    );
    assert_eq!(parsed["parsed"], "1", "only a.go should be reparsed");
    let changed = parsed["changed_files"]
        .as_array()
        .expect("changed_files array");
    let names: Vec<&str> = changed.iter().filter_map(|v| v.as_str()).collect();
    assert_eq!(names, vec!["a.go"], "scope should be exactly [a.go]");

    // Verify b.go was NOT touched (its mtime/size in _file_index is unchanged).
    let after = file_index_snapshot(&ctx.live_db.lock().unwrap());
    assert_eq!(
        after.get("b.go"),
        snapshot.get("b.go"),
        "b.go must be untouched"
    );
    assert_ne!(
        after.get("a.go"),
        snapshot.get("a.go"),
        "a.go must be updated"
    );
}

/// Verify explicit `files: [...]` arg takes precedence over auto-derivation
/// and works with a directory `source`. This is the shape the cloister hook
/// will eventually use.
#[tokio::test]
async fn test_op_reparse_accepts_files_scope_with_dir_source() {
    use leyline_cli_lib::daemon::DaemonContext;
    use std::sync::{Arc, Mutex};

    let src = TempDir::new().unwrap();
    write_empty_go_func(src.path(), "a.go", "m", "A");
    write_empty_go_func(src.path(), "b.go", "m", "B");
    write_empty_go_func(src.path(), "c.go", "m", "C");

    let conn = cold_parse_go(src.path());

    std::thread::sleep(std::time::Duration::from_millis(50));
    fs::write(
        src.path().join("b.go"),
        "package m\n\nfunc B() { /* edit */ }\n",
    )
    .unwrap();

    let dir = TempDir::new().unwrap();
    let (_arena, ctrl_path) = fresh_arena(dir.path());

    let ctx = Arc::new(DaemonContext {
        live_db: Mutex::new(conn),
        source_dir: Some(src.path().to_path_buf()),
        lang_filter: Some("go".to_string()),
        ..default_test_ctx(ctrl_path)
    });

    let sock_path = dir.path().join("reparse-files.sock");
    spawn_test_socket(ctx, sock_path.clone()).await;

    let body = serde_json::json!({
        "op": "reparse",
        "source": src.path().to_string_lossy().to_string(),
        "files": ["b.go"],
    });

    let parsed = uds_round_trip(&sock_path, &body.to_string()).await;

    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["parsed"], "1");
    let changed: Vec<&str> = parsed["changed_files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(changed, vec!["b.go"]);
}

/// `op_reparse` against a source path that doesn't exist on disk should
/// return a clean JSON-RPC-shaped error (`ok: false`, error message
/// present), AND the daemon's phase machine should land in `Error(...)`
/// with the failure visible via `op_status`. This locks in the
/// observability contract so a future regression that swallows the error
/// or leaves phase stuck at `Parsing` is caught.
///
/// First test from `5f7100-12: edge-case test sweep` — covers:
///   - op_reparse with a nonexistent source path
///   - error response shape
///   - phase persists as Error in the next status call
///   - subsequent successful reparse resets phase to Ready (recovery)
#[tokio::test]
async fn test_op_reparse_nonexistent_source_sets_error_phase_and_recovers() {
    use std::sync::Arc;

    let dir = TempDir::new().unwrap();
    let (_arena, ctrl_path) = fresh_arena(dir.path());

    let ctx = Arc::new(default_test_ctx(ctrl_path));

    let sock_path = dir.path().join("reparse-bad.sock");
    spawn_test_socket(ctx, sock_path.clone()).await;

    // Helper: send a JSON op + receive one line, parse. Delegates to the
    // shared `uds_round_trip` so the two test-local duplicates of this
    // pattern stay in sync.
    async fn round_trip(sock: &std::path::Path, body: serde_json::Value) -> serde_json::Value {
        uds_round_trip(sock, &body.to_string()).await
    }

    // 1. op_reparse with a path that doesn't exist anywhere on disk.
    let bogus = "/tmp/cloister-e2e-no-such-thing-xyzzy";
    let resp = round_trip(
        &sock_path,
        serde_json::json!({"op": "reparse", "source": bogus}),
    )
    .await;
    assert_eq!(resp["ok"], false, "expected error response, got {resp}");
    assert!(
        resp["error"]
            .as_str()
            .unwrap_or("")
            .contains("not a directory")
            || resp["error"]
                .as_str()
                .unwrap_or("")
                .contains("No such file"),
        "error should mention the path problem; got: {resp:?}",
    );

    // 2. op_status now reports phase: error with the message preserved.
    let status = round_trip(&sock_path, serde_json::json!({"op": "status"})).await;
    assert_eq!(status["phase"], "error", "phase should be error: {status}");
    assert!(
        status["error"]
            .as_str()
            .unwrap_or("")
            .contains("reparse failed"),
        "status error field should describe the failure; got: {status:?}",
    );

    // 3. Recovery: a successful reparse against a real (empty) dir flips
    //    phase back to Ready and clears the error state.
    let good = TempDir::new().unwrap();
    let resp = round_trip(
        &sock_path,
        serde_json::json!({"op": "reparse", "source": good.path().to_string_lossy().to_string()}),
    )
    .await;
    assert_eq!(resp["ok"], true, "successful reparse: {resp}");

    let status = round_trip(&sock_path, serde_json::json!({"op": "status"})).await;
    assert_eq!(
        status["phase"], "ready",
        "phase should recover to ready: {status}"
    );
    assert!(
        status["error"].is_null() || status["error"].as_str() == Some(""),
        "error field should be cleared after successful reparse; got: {status:?}",
    );
}

/// `op_query` is a SQL injection foot-gun (cf. ley-line-open-6213d4 — MCP
/// trust boundary doc). This test pins the *current* behavior so a future
/// fix (gate destructive verbs, restrict to SELECT-only, require auth) is
/// caught as an intentional change rather than a silent one.
///
/// Specifically: today, sending arbitrary DDL via `op_query` succeeds and
/// runs against the living db. That's the foot-gun; this test documents it.
#[tokio::test]
async fn test_op_query_destructive_runs_today_pin_for_6213d4() {
    use leyline_cli_lib::daemon::DaemonContext;
    use std::sync::{Arc, Mutex};

    let dir = TempDir::new().unwrap();
    let (_arena, ctrl_path) = fresh_arena(dir.path());

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE doomed (id INTEGER PRIMARY KEY); INSERT INTO doomed VALUES (1);",
    )
    .unwrap();

    let ctx = Arc::new(DaemonContext {
        live_db: Mutex::new(conn),
        ..default_test_ctx(ctrl_path)
    });

    let sock_path = dir.path().join("query-destructive.sock");
    spawn_test_socket(ctx.clone(), sock_path.clone()).await;

    let parsed = uds_round_trip(&sock_path, r#"{"op":"query","sql":"DROP TABLE doomed"}"#).await;

    // Today: the DROP succeeds. This pins that fact so 6213d4 can land an
    // intentional behavior change (gate / SELECT-only / require auth) and
    // this test will fail loudly, prompting an update.
    assert_eq!(
        parsed["ok"], true,
        "today op_query runs raw DDL — see 6213d4 for the lockdown plan; got: {parsed}",
    );

    let table_gone: bool = ctx
        .live_db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) = 0 FROM sqlite_master WHERE type='table' AND name='doomed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(table_gone, "DROP TABLE actually executed");
}

/// `vec_search` with a dim-mismatched embedder must surface a clean error,
/// not panic. This can happen if a private extension swaps in a different
/// embedder after the VectorIndex was already created (cmd_daemon sizes
/// the index from the embedder at startup, but the trait API allows later
/// substitution by tests / experiments / hot-reload).
///
/// Bead: ley-line-open-5f7100-12 (item #6 — vec_search dim mismatch).
#[cfg(feature = "vec")]
#[tokio::test]
async fn test_op_vec_search_dim_mismatch_returns_clean_error() {
    use leyline_cli_lib::daemon::DaemonContext;
    use leyline_cli_lib::daemon::embed::{Embedder, ZeroEmbedder};
    use leyline_cli_lib::daemon::vec_index::{VectorIndex, register_vec};
    use std::sync::{Arc, Mutex};

    register_vec();

    let dir = TempDir::new().unwrap();
    let (_arena, ctrl_path) = fresh_arena(dir.path());

    // Mismatch on purpose: 4-dim index, 8-dim embedder. The query vector
    // will be 8-dim, but the index will refuse anything that isn't 4-dim.
    let index = Arc::new(VectorIndex::new(4, None).unwrap());
    let embedder: Arc<dyn Embedder> = Arc::new(ZeroEmbedder { dim: 8 });

    let ctx = Arc::new(DaemonContext {
        vec_index: index,
        embedder,
        embed_queue: Arc::new(Mutex::new(std::collections::BinaryHeap::new())),
        sheaf: Arc::new(leyline_cli_lib::daemon::sheaf_ops::SheafState::new()),
        ..default_test_ctx(ctrl_path)
    });

    let sock_path = dir.path().join("vec_dim_mismatch.sock");
    spawn_test_socket(ctx, sock_path.clone()).await;

    let parsed = uds_round_trip(&sock_path, r#"{"op":"vec_search","query":"hello","k":3}"#).await;

    assert_eq!(
        parsed["ok"], false,
        "dim mismatch must surface as ok:false: {parsed}"
    );
    let err = parsed["error"].as_str().unwrap_or("");
    assert!(
        err.contains("dim") || err.contains("expected") || err.contains("4"),
        "error should describe the dim mismatch; got: {err:?}",
    );
}

/// Concurrent UDS load: spin up the daemon socket, hit it with N parallel
/// clients each issuing a mix of `status` + `query` ops, assert all complete
/// in bounded time and every response is well-formed.
///
/// This is the test that would have flagged ley-line-open-5fea4e
/// (mutex-held-across-await deadlock) earlier — under contention the
/// existing std::sync::Mutex serializes everything, but we *should* still
/// make forward progress and never starve a connection. If the daemon
/// deadlocks, this test will hang and CI will time out (5s wall ceiling
/// makes it visible).
///
/// Bead: ley-line-open-5f7100-12 (item #1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_uds_load_completes_bounded() {
    use leyline_cli_lib::daemon::{DaemonContext, EventRouter};
    use std::sync::{Arc, Mutex};

    let dir = TempDir::new().unwrap();
    let (_arena, ctrl_path) = fresh_arena(dir.path());

    // Pre-populate a tiny living db so query ops have something to hit.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE _meta (key TEXT PRIMARY KEY, value TEXT);
         INSERT INTO _meta VALUES ('parse_version', '1');",
    )
    .unwrap();

    let ctx = Arc::new(DaemonContext {
        router: EventRouter::new(64),
        live_db: Mutex::new(conn),
        ..default_test_ctx(ctrl_path)
    });

    let sock_path = dir.path().join("concurrent.sock");
    spawn_test_socket(ctx, sock_path.clone()).await;

    /// One client: open, send one op, read one response, parse, return.
    /// Wraps the shared `uds_round_trip` so this test's owned-PathBuf +
    /// `'static str` calling convention stays usable inside `tokio::spawn`.
    async fn one_call(sock: std::path::PathBuf, body: &'static str) -> serde_json::Value {
        uds_round_trip(&sock, body).await
    }

    // Spawn 20 concurrent clients: half issue status, half issue a SELECT.
    // Each completes independently. Wrap the whole thing in a 5s tokio
    // timeout so a deadlock turns into a test failure rather than a hang.
    let n = 20;
    let deadline = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut tasks = Vec::with_capacity(n);
        for i in 0..n {
            let sp = sock_path.clone();
            tasks.push(tokio::spawn(async move {
                let body = if i % 2 == 0 {
                    r#"{"op":"status"}"#
                } else {
                    r#"{"op":"query","sql":"SELECT key, value FROM _meta"}"#
                };
                one_call(sp, body).await
            }));
        }
        let mut results = Vec::with_capacity(n);
        for t in tasks {
            results.push(t.await.expect("task panic"));
        }
        results
    })
    .await;

    let results = deadline.expect("daemon deadlocked under concurrent load");
    assert_eq!(results.len(), n);
    for (i, r) in results.iter().enumerate() {
        if i % 2 == 0 {
            assert_eq!(r["ok"], true, "status #{i} failed: {r}");
            assert!(r.get("phase").is_some(), "status #{i} missing phase: {r}");
        } else {
            assert_eq!(r["ok"], true, "query #{i} failed: {r}");
            assert!(r.get("rows").is_some(), "query #{i} missing rows: {r}");
        }
    }
}

/// Verify the error phase is surfaced through `op_status`.
#[tokio::test]
async fn test_status_reports_error_phase() {
    use leyline_cli_lib::daemon::{DaemonContext, DaemonPhase, DaemonState};
    use std::sync::{Arc, RwLock};

    let dir = TempDir::new().unwrap();
    let (_arena, ctrl_path) = fresh_arena(dir.path());

    let mut state = DaemonState::initializing();
    state.phase = DaemonPhase::Error("boom: parse failed".to_string());

    let ctx = Arc::new(DaemonContext {
        state: Arc::new(RwLock::new(state)),
        ..default_test_ctx(ctrl_path)
    });

    let sock_path = dir.path().join("status_error.sock");
    spawn_test_socket(ctx, sock_path.clone()).await;

    let parsed = uds_round_trip(&sock_path, r#"{"op":"status"}"#).await;
    assert_eq!(parsed["phase"], "error");
    assert_eq!(parsed["error"], "boom: parse failed");
}

/// Race `op_reparse` against `op_snapshot` and a periodic snapshot timer.
/// Verifies the chain of guarantees that ley-line-open-5fea4e (mutex held
/// across await) is meant to preserve:
///   - every response surfaces a well-formed `current_root` (T2.4)
///   - at least one snapshot publishes a non-zero root (race made progress)
///   - no response panics or returns ok:false unexpectedly
///   - phase ends at "ready" — not stuck mid-reparse
///   - the live db lock never deadlocks (5s outer timeout catches it)
///
/// Bead: ley-line-open-5f7100-12 (item #8 — reparse/snapshot race).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_op_reparse_snapshot_race_publishes_well_formed_root() {
    use leyline_cli_lib::daemon::{DaemonContext, EventRouter};
    use leyline_core::{Controller, create_arena};
    use std::sync::{Arc, Mutex};

    // Source: 4 files. Reparse will rebuild the whole tree each time
    // (no scope), so two concurrent reparses both touch every row.
    let src = TempDir::new().unwrap();
    for i in 0..4 {
        std::fs::write(
            src.path().join(format!("f{i}.go")),
            format!("package m\n\nfunc F{i}() {{}}\n"),
        )
        .unwrap();
    }

    let dir = TempDir::new().unwrap();
    let arena_path = dir.path().join("test.arena");
    let ctrl_path = dir.path().join("test.ctrl");
    let _mmap = create_arena(&arena_path, 4 * 1024 * 1024).unwrap();
    let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
    ctrl.set_arena(&arena_path.to_string_lossy(), 4 * 1024 * 1024)
        .unwrap();
    drop(ctrl);

    // Cold-parse so _file_index is populated and reparse calls have
    // something to diff against.
    let conn = cold_parse_go(src.path());

    let ctx = Arc::new(DaemonContext {
        ctrl_path: ctrl_path.clone(),
        router: EventRouter::new(64),
        live_db: Mutex::new(conn),
        source_dir: Some(src.path().to_path_buf()),
        lang_filter: Some("go".to_string()),
        ..default_test_ctx(ctrl_path)
    });

    let sock_path = dir.path().join("reparse_snapshot_race.sock");
    spawn_test_socket(ctx, sock_path.clone()).await;

    /// One round-trip — wraps the shared `uds_round_trip` so the
    /// owned-PathBuf + `'static str` calling convention works inside
    /// `tokio::spawn`.
    async fn round_trip(sock: std::path::PathBuf, body: &'static str) -> serde_json::Value {
        uds_round_trip(&sock, body).await
    }

    // Fire 6 reparses + 6 snapshots concurrently, plus 6 status reads
    // that should always succeed regardless of contention. Wrap in 10s
    // outer timeout so a deadlock is a test failure, not a CI hang.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut tasks = Vec::with_capacity(18);
        for _ in 0..6 {
            let sp = sock_path.clone();
            tasks.push(tokio::spawn(async move {
                round_trip(sp, r#"{"op":"reparse"}"#).await
            }));
            let sp = sock_path.clone();
            tasks.push(tokio::spawn(async move {
                round_trip(sp, r#"{"op":"snapshot"}"#).await
            }));
            let sp = sock_path.clone();
            tasks.push(tokio::spawn(async move {
                round_trip(sp, r#"{"op":"status"}"#).await
            }));
        }
        let mut results = Vec::with_capacity(tasks.len());
        for t in tasks {
            results.push(t.await.expect("task panic"));
        }
        results
    })
    .await
    .expect("daemon deadlocked under reparse/snapshot contention");

    // Every response must have `ok: true`.
    for (i, r) in outcome.iter().enumerate() {
        assert_eq!(r["ok"], true, "task #{i} failed: {r}");
    }

    // T2.4: collect `current_root` hex strings from reparse + snapshot
    // responses. Each response surfaces the controller's current root —
    // a 64-char hex BLAKE3, or the zero sentinel if no snapshot has yet
    // landed. With idempotent root semantics (same bytes → same root),
    // unchanged dbs collapse to one root rather than 6 distinct ints.
    // Race-safety assertion: every response is well-formed hex AND at
    // least one snapshot completed and published a non-zero root.
    let roots: Vec<String> = outcome
        .iter()
        .filter_map(|r| {
            r.get("current_root")
                .and_then(|s| s.as_str())
                .map(str::to_string)
        })
        .collect();
    assert!(
        roots.len() >= 12,
        "expected ≥12 current_root values from 6 reparse + 6 snapshot, got {}: {roots:?}",
        roots.len(),
    );
    let zero = "0".repeat(64);
    for r in &roots {
        assert_eq!(r.len(), 64, "current_root must be 64-char hex (got {r:?})");
    }
    assert!(
        roots.iter().any(|r| r != &zero),
        "race must publish at least one non-zero current_root \
         (snapshots ran but never advanced state): {roots:?}",
    );

    // Final status check: phase must be Ready (not stuck at Parsing
    // or Error). This is the daemon-state-machine consistency assert.
    let final_status = round_trip(sock_path.clone(), r#"{"op":"status"}"#).await;
    assert_eq!(
        final_status["phase"], "ready",
        "phase should land at ready after race; got {final_status}",
    );
}

// ---------------------------------------------------------------------------
// Daemon protocol drift gate, Rust half (bead ley-line-open-b5a77b / A-1)
//
// THIS gate (Rust side): spawn the daemon, send each fixture's request,
// assert the live response contains every required key. Pins handler ↔
// fixture agreement. Loose superset match — state-dependent values vary
// by test environment; only the SHAPE is contractual.
//
// The companion Go gate at
// `clients/go/leyline-schema/daemon/daemon_protocol_test.go` validates a
// DIFFERENT edge: it strict-unmarshals each fixture's `response` payload
// into the matching typed Go binding (no daemon round-trip on the Go side).
// That pins fixture ↔ schema agreement.
//
// Composing the two:
//   handler ↔ fixture (Rust gate, this one) + fixture ↔ schema (Go gate)
//   ⇒ handler ↔ schema (transitively)
//
// Either gate failing means the chain broke. Together they extend T8.10's
// cross-runtime fixture pattern (bead 6b7d43) from the substrate (capnp
// segment files; byte-equal direct decode) to the daemon protocol (JSON
// wire; two-step chain through the fixture).
//
// See `docs/TABLE_CONTRACT.md` for the substrate-vs-daemon-protocol layering.
// ---------------------------------------------------------------------------

/// Load `tests/fixtures/daemon-protocol.json` and return all op fixtures
/// keyed by op name. The top-level `_doc` field is filtered out — it's
/// documentation, not a fixture.
fn load_daemon_protocol_fixtures() -> serde_json::Map<String, serde_json::Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/daemon-protocol.json");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let raw: serde_json::Value =
        serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
    let mut map = raw
        .as_object()
        .unwrap_or_else(|| panic!("{} must be a JSON object", path.display()))
        .clone();
    map.remove("_doc");
    map
}

#[tokio::test]
async fn daemon_protocol_gate_handlers_emit_required_keys() {
    use std::sync::Arc;

    let dir = TempDir::new().unwrap();
    let (_arena, ctrl_path) = fresh_arena(dir.path());
    let ctx = Arc::new(default_test_ctx(ctrl_path));

    // The gate exercises the SUCCESS paths of each op. Empty in-memory db
    // means the relevant tables don't exist, which would cause queries to
    // bubble `no such table` errors and the gate would catch them as
    // unexpected ok=false. Pre-create the tables once (idempotent DDL) so
    // ops can return empty success shapes instead of erroring.
    {
        let guard = ctx.live_db.lock().unwrap();
        leyline_schema::create_schema(&guard).expect("nodes schema");
        guard
            .execute_batch(leyline_ts::schema::REFS_DDL)
            .expect("node_refs schema");
        guard
            .execute_batch(leyline_ts::schema::DEFS_DDL)
            .expect("node_defs schema");
    }

    let sock_path = dir.path().join("test.sock");
    spawn_test_socket(ctx, sock_path.clone()).await;

    let fixtures = load_daemon_protocol_fixtures();
    assert!(
        !fixtures.is_empty(),
        "expected at least one op fixture in daemon-protocol.json"
    );

    let mut failures: Vec<String> = Vec::new();

    for (op_name, fixture) in &fixtures {
        let request = fixture
            .get("request")
            .unwrap_or_else(|| panic!("fixture {op_name} missing `request`"));
        let required_keys = fixture
            .get("response_required_keys")
            .and_then(|v| v.as_array())
            .unwrap_or_else(|| panic!("fixture {op_name} missing `response_required_keys` array"));

        let body = serde_json::to_string(request).unwrap();
        let response = uds_round_trip(&sock_path, &body).await;

        let obj = match response.as_object() {
            Some(o) => o,
            None => {
                failures.push(format!(
                    "op={op_name}: response is not a JSON object: {response}"
                ));
                continue;
            }
        };

        // Some ops legitimately return {"ok": false, "error": "..."} on a
        // fresh empty test daemon (e.g. read_content for a missing node).
        // Those fixtures OPT INTO this behavior by setting
        // `response_required_keys: ["ok"]` exactly — the only required key
        // in the error branch. For fixtures with a richer required-key set
        // (like list_roots requiring "children"), an unexpected ok=false
        // is a real failure: the handler should have returned the success
        // shape and didn't. Don't paper over it.
        let opt_in_error_branch = required_keys.len() == 1
            && required_keys
                .first()
                .and_then(|k| k.as_str())
                .map(|s| s == "ok")
                .unwrap_or(false);
        let is_error = obj.get("ok").and_then(|v| v.as_bool()) == Some(false);

        if is_error && !opt_in_error_branch {
            failures.push(format!(
                "op={op_name}: response is ok=false but fixture requires {required_keys:?} \
                 (handler should have returned the success shape): {response}"
            ));
            continue;
        }

        for key in required_keys {
            let key_str = key
                .as_str()
                .unwrap_or_else(|| panic!("fixture {op_name} has non-string required key"));

            if !obj.contains_key(key_str) {
                failures.push(format!(
                    "op={op_name}: required key `{key_str}` missing from response: {response}"
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "daemon protocol drift gate caught {} failure(s):\n  {}",
        failures.len(),
        failures.join("\n  "),
    );
}

// =====================================================================
// E2E sheaf ops over real UDS daemon (ley-line-open-ae7a35)
// =====================================================================

/// Spin up a daemon UDS listener, send `sheaf_set_topology` with f32
/// stalk data + agreement_dim, then send `sheaf_invalidate` with a
/// projected-away change. Confirms the δ⁰-driven invalidation contract
/// engages end-to-end — schema → handler → wire JSON → cache state →
/// response — not just at the Rust API layer.
///
/// Pins gap #1 from the post-bench audit (`cb15ada` commit message):
/// the daemon op surface must actually drive δ⁰ mode for a real UDS
/// consumer, not just the in-process `SheafCache::with_complex` path.
///
/// **Post-d03e7d:** this test no longer touches `ctx.sheaf` directly.
/// The earlier version reached into the in-process cache to `put(id, ())`
/// entries so the BFS cascade had something to mark — a back-door that
/// masked the empty-`invalidated` bug for UDS consumers. With the
/// cascade fix in place (cache returns regions whose boundary projection
/// moved, not just regions with local entries), the same assertions hold
/// using only daemon ops over the wire.
#[tokio::test]
async fn e2e_sheaf_ops_drive_delta_zero_mode_over_real_uds() {
    use std::sync::Arc;

    let dir = TempDir::new().unwrap();
    let (_arena, ctrl_path) = fresh_arena(dir.path());

    let ctx = Arc::new(default_test_ctx(ctrl_path));
    let sock_path = dir.path().join("sheaf-e2e.sock");
    spawn_test_socket(ctx.clone(), sock_path.clone()).await;

    // ── Step 1: sheaf_set_topology with f32 data → δ⁰ mode engages ──
    //
    // Two regions, one edge, agreement_dim=2. Both stalks share the
    // first two coords [1.0, 0.5] — the agreement subspace — and
    // differ only in private coords [2..4]. After set_topology, the
    // cache should be in δ⁰ mode AND its baseline should reflect the
    // (zero) initial defect on the shared subspace.
    let topology_resp = uds_round_trip(
        &sock_path,
        r#"{"op":"sheaf_set_topology","node_stalk_dim":4,"regions":[{"id":0,"hash":"aa","data":[1.0,0.5,0.0,0.0]},{"id":1,"hash":"bb","data":[1.0,0.5,9.0,9.0]}],"restrictions":[{"a":0,"b":1,"boundary_hash":"11","co_change_rate":0.5,"weights":[1.0],"agreement_dim":2}]}"#,
    )
    .await;
    assert_eq!(
        topology_resp["ok"], true,
        "sheaf_set_topology must succeed: {topology_resp}"
    );
    assert_eq!(
        topology_resp["delta_zero_mode"], true,
        "δ⁰ mode must engage when node_stalk_dim>0 + f32 data + agreement_dim>0: {topology_resp}"
    );
    assert_eq!(topology_resp["regions"], 2);
    assert_eq!(topology_resp["restrictions"], 1);

    // ── Step 2: sheaf_invalidate with projected-away change ──
    //
    // Region 0's new stalk = [1.0, 0.5, 42.0, 7.0]. The agreement
    // coords [0..2] are unchanged from baseline. δ⁰ stays zero → the
    // cascade must NOT include region 1, only the changed region 0.
    let invalidate_resp = uds_round_trip(
        &sock_path,
        r#"{"op":"sheaf_invalidate","regions":[0],"stalks":[{"id":0,"hash":"ff","data":[1.0,0.5,42.0,7.0]}]}"#,
    )
    .await;
    let invalidated: Vec<u32> = invalidate_resp["invalidated"]
        .as_array()
        .expect("invalidated is an array")
        .iter()
        .map(|v| v.as_u64().unwrap() as u32)
        .collect();
    assert_eq!(
        invalidated,
        vec![0],
        "δ⁰ mode must hold region 1 out of cascade when agreement coords unchanged; got {invalidated:?} (full: {invalidate_resp})"
    );

    // ── Step 3: sheaf_invalidate with agreement-breaking change ──
    //
    // Region 0's stalk now flips coord [0] (in the agreement subspace).
    // δ⁰ moves → cascade must reach region 1. With the d03e7d fix the
    // wire response surfaces the cascade regardless of whether the
    // in-process cache has entries for these regions.
    let invalidate_resp2 = uds_round_trip(
        &sock_path,
        r#"{"op":"sheaf_invalidate","regions":[0],"stalks":[{"id":0,"hash":"ee","data":[99.0,0.5,42.0,7.0]}]}"#,
    )
    .await;
    let invalidated2: Vec<u32> = invalidate_resp2["invalidated"]
        .as_array()
        .expect("invalidated is an array")
        .iter()
        .map(|v| v.as_u64().unwrap() as u32)
        .collect();
    assert!(
        invalidated2.contains(&0),
        "changed region must appear in cascade; got {invalidated2:?} (full: {invalidate_resp2})"
    );
    assert!(
        invalidated2.contains(&1),
        "δ⁰ mode must cascade when agreement coord moves; got {invalidated2:?} (full: {invalidate_resp2})"
    );

    // ── Step 4: sheaf_status surfaces the current cache state ──
    let status_resp = uds_round_trip(&sock_path, r#"{"op":"sheaf_status"}"#).await;
    assert!(
        status_resp["generation"].as_str().is_some(),
        "sheaf_status must return generation as Int64-as-string: {status_resp}"
    );
    let generation: u64 = status_resp["generation"]
        .as_str()
        .unwrap()
        .parse()
        .expect("generation parses as u64");
    assert!(
        generation >= 2,
        "generation must reflect both invalidate calls; got {generation}"
    );
}

/// Heuristic-only path: no f32 data, no agreement_dim — the daemon op
/// must still work AND report `delta_zero_mode: false` so callers know
/// they're on the XOR-cascade path. Confirms backward-compat: any
/// pre-cb15ada caller sending just {id, hash} per stalk still gets a
/// well-formed response.
#[tokio::test]
async fn e2e_sheaf_set_topology_heuristic_only_keeps_working() {
    use std::sync::Arc;

    let dir = TempDir::new().unwrap();
    let (_arena, ctrl_path) = fresh_arena(dir.path());
    let ctx = Arc::new(default_test_ctx(ctrl_path));
    let sock_path = dir.path().join("sheaf-heuristic.sock");
    spawn_test_socket(ctx.clone(), sock_path.clone()).await;

    let resp = uds_round_trip(
        &sock_path,
        r#"{"op":"sheaf_set_topology","regions":[{"id":0,"hash":"aa"},{"id":1,"hash":"bb"}],"restrictions":[{"a":0,"b":1,"boundary_hash":"11","co_change_rate":0.5,"weights":[1.0]}]}"#,
    )
    .await;
    assert_eq!(resp["ok"], true);
    assert_eq!(
        resp["delta_zero_mode"], false,
        "no node_stalk_dim → must report heuristic-only: {resp}"
    );
}
