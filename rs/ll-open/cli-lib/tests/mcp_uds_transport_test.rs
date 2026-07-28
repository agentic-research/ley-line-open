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

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;

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
    let ctx = common::daemon_context(tmp.path());
    leyline_cli_lib::daemon::mcp::spawn_uds(ctx, sock.clone(), Some(Arc::new(token.clone())))
        .expect("spawn MCP over uds");

    // Poll the real condition rather than sleeping a fixed interval — a
    // wall-clock sleep as a stop condition races the scheduler and flakes under
    // load with nothing actually broken (`sleep_in_tests`).
    drop(common::wait_for_uds(&sock, 10_000));

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
