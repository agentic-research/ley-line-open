//! Bead `ley-line-open-3bf5dc`: sqlite-vec reaches the daemon's reader pool.
//!
//! `register_vec` installs sqlite-vec with `sqlite3_auto_extension`, which
//! applies only to connections opened AFTER it runs. The daemon used to call
//! it while building the vector index, after `LiveDb::new` had already opened
//! every pooled reader, so no reader had sqlite-vec's modules (`vec_each`, ...)
//! and a `query` naming one failed.
//!
//! This file is its own test binary, so nothing registers sqlite-vec in the
//! process before the daemon's own startup does.
#![cfg(feature = "vec")]

use std::path::Path;
use std::sync::Arc;

use leyline_cli_lib::cmd_daemon::{DaemonConfig, DaemonOptions, run_daemon_with_options};
use leyline_cli_lib::daemon::DaemonExt;
use leyline_cli_lib::daemon::embed::{Embedder, ZeroEmbedder};
use leyline_cli_lib::daemon::socket::SocketDiscovery;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Supplies the deterministic embedder, so startup downloads no model.
struct ZeroExt;

impl DaemonExt for ZeroExt {
    fn embedder(&self) -> Option<Arc<dyn Embedder>> {
        Some(Arc::new(ZeroEmbedder { dim: 4 }))
    }
}

fn config(dir: &Path) -> DaemonConfig {
    DaemonConfig {
        arena: dir.join("v.arena"),
        arena_size_mib: 4,
        control: None,
        mount: None,
        backend: "sqlite".to_string(),
        nfs_port: 0,
        language: None,
        timeout: None,
        source: None,
        mcp_port: None,
        mcp_bind: None,
        mcp_allow_public: false,
        mcp_no_auth: true,
        mcp_uds: None,
        mcp_groups: None,
        reset_arena: false,
    }
}

async fn call(sock: &Path, request: serde_json::Value) -> serde_json::Value {
    let stream = UnixStream::connect(sock).await.expect("connect");
    let (read, mut write) = stream.into_split();
    write
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).expect("a JSON reply")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pooled_reader_has_sqlite_vec_modules() {
    let dir = tempfile::TempDir::new().unwrap();
    let sock = dir.path().join("v.sock");
    let daemon = tokio::spawn(run_daemon_with_options(
        config(dir.path()),
        Arc::new(ZeroExt),
        DaemonOptions {
            discovery: SocketDiscovery::Unpublished,
            ..Default::default()
        },
    ));
    // The socket is bound after the reader pool is built: wait on it, not a
    // clock.
    while UnixStream::connect(&sock).await.is_err() {
        assert!(!daemon.is_finished(), "the daemon exited before serving");
        tokio::task::yield_now().await;
    }

    // `query` runs on a pooled reader.
    let reply = call(
        &sock,
        serde_json::json!({
            "op": "query",
            "sql": "SELECT value FROM vec_each('[1.0, 2.0]')",
        }),
    )
    .await;
    assert_eq!(
        reply["rows"],
        serde_json::json!([{"value": "1"}, {"value": "2"}]),
        "a pooled reader must have sqlite-vec's vec_each: {reply}"
    );
    daemon.abort();
}
