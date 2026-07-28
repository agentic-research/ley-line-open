//! MCP over a Unix domain socket (bead `ley-line-open-6569de`).
//!
//! ## Why this exists
//!
//! The daemon's `--control` socket carries the OPS protocol, not MCP, and the
//! MCP surface was TCP-only. An attested caller therefore had nothing to dial:
//! cloister runs in workerd, which cannot speak UDS directly, so it reaches
//! bundles through notme-proxy in the "cloister-companion" role (cloister
//! ADR-0005) — that proxy receives `X-Cloister-Transport: uds` and connects
//! `AF_UNIX`. rosary and mache are already reached that way; LLO was not.
//!
//! ## What this gate holds
//!
//! The acceptance condition is not "a socket exists". It is a client completing
//! `tools/list` over that socket **with the ADR-0022 token gate ENABLED** — so
//! cloister's `task smoke` can reach the daemon without `--mcp-no-auth`. A UDS
//! that only works with auth disabled would move the problem rather than fix it.
//!
//! The 401 case is asserted too. A transport that silently skipped the gate
//! would pass a happy-path-only test while shipping an open surface, which is
//! precisely the shape of `ley-line-open-2607d2`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use leyline_cli_lib::daemon::{
    DaemonContext, DaemonState, EventRouter, NoExt, sheaf_ops::SheafState,
};
use leyline_core::{Controller, create_arena};

/// Same shape as `event_push_blackbox_test.rs`'s helper, deliberately — a
/// regression in shared daemon bring-up should surface in both files at once
/// rather than in whichever happens to run first.
fn build_ctx(dir: &Path) -> Arc<DaemonContext> {
    use parking_lot::{Mutex, RwLock};

    let arena_path = dir.join("uds.arena");
    let ctrl_path = dir.join("uds.ctrl");
    let _mmap = create_arena(&arena_path, 2 * 1024 * 1024).expect("create arena");
    let mut ctrl = Controller::open_or_create(&ctrl_path).expect("open ctrl");
    ctrl.set_arena(&arena_path.to_string_lossy(), 2 * 1024 * 1024)
        .expect("set arena");
    drop(ctrl);

    let router = EventRouter::new(16);
    let sheaf = Arc::new(SheafState::new());
    sheaf.set_emitter(router.emitter());
    let live_db_path = ctrl_path.with_extension("live.db");
    let live_db = leyline_cli_lib::daemon::db_pool::LiveDb::open_fresh_for_test(&live_db_path);

    Arc::new(DaemonContext {
        ctrl_path,
        ext: Arc::new(NoExt),
        router,
        live_db,
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
        #[cfg(feature = "text-search")]
        text_search: Arc::new(leyline_text_search::null::NullEngine::new()),
        sheaf,
    })
}

/// Minimal HTTP/1.1 POST over an already-connected UDS. Written by hand
/// because the point is to prove a RAW socket client works — the same thing
/// notme-proxy does — not that a particular Rust HTTP crate can.
fn post_over_uds(sock: &PathBuf, body: &str, token: Option<&str>) -> (u16, String) {
    let mut stream = UnixStream::connect(sock).expect("connect MCP uds");
    let auth = token
        .map(|t| format!("x-leyline-token: {t}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         {auth}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    stream.write_all(req.as_bytes()).expect("write request");
    stream.flush().expect("flush");

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).expect("read status");
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);

    // Drain headers, then the rest is the body.
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
    }
    let mut body_out = String::new();
    let mut chunk = String::new();
    while reader.read_line(&mut chunk).unwrap_or(0) > 0 {
        body_out.push_str(&chunk);
        chunk.clear();
    }
    (code, body_out)
}

#[test]
fn mcp_tools_list_completes_over_uds_with_the_token_gate_enabled() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sock: PathBuf = tmp.path().join("llo-mcp.sock");
    let token = "a".repeat(64);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let ctx = build_ctx(tmp.path());
    leyline_cli_lib::daemon::mcp::spawn_uds(ctx, sock.clone(), Some(Arc::new(token.clone())))
        .expect("spawn MCP over uds");

    // Give the listener a moment to accept.
    std::thread::sleep(std::time::Duration::from_millis(200));

    assert!(
        sock.exists(),
        "the socket file must exist at the configured path"
    );

    // Owner-only: a UDS is a filesystem object, so its mode IS its
    // reachability. The token gate is a second layer, not the only one.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "MCP socket must be owner-only; got {mode:o}");

    let req = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;

    // Without the token the gate must refuse — otherwise the new transport
    // would be an open surface that a happy-path test would never notice.
    let (code, _) = post_over_uds(&sock, req, None);
    assert_eq!(code, 401, "unauthenticated tools/list must be rejected");

    let (code, body) = post_over_uds(&sock, req, Some(&token));
    assert_eq!(code, 200, "authenticated tools/list must succeed: {body}");
    assert!(
        body.contains("\"tools\""),
        "tools/list must return a tools array over UDS; got {body}",
    );
}
