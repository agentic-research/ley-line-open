//! End-to-end LSP enrichment against a scripted in-process language server
//! (bead `ley-line-open-fb7d73`).
//!
//! `enrich_files_with_client` — the loop that opens files, polls symbols,
//! waits for readiness, and projects hover/def/refs into the `_lsp*` tables —
//! had NO test observing its output: the diff-scoped mutation gate proved it
//! by stubbing the whole function to `Ok((0,0))`, `Ok((1,1))`, `Ok((1,0))`
//! and `Ok((0,1))`, and all four survived. This test kills all four by
//! asserting both the returned counts (via `EnrichmentStats.items_added`)
//! and the rows the real function writes (`_lsp`, `_lsp_hover`) — a stub
//! can fake one or the other, never both.
//!
//! ## How the fake server works
//!
//! `harness = false`: this file owns `main`. Run plainly it is the test;
//! invoked as `rust-analyzer` (or with `--fake-ls`) it becomes a minimal
//! LSP server speaking Content-Length framed JSON-RPC on stdio. The test
//! symlinks `rust-analyzer` in a tempdir to this same binary, prepends the
//! tempdir to PATH, and drives the REAL `LspEnrichmentPass` — pool,
//! handshake, readiness wait, probe, per-symbol loop, projection — with no
//! real language server and no network. (A symlink, not a shell wrapper:
//! see the shim comment in `run_test` for the SIP/DYLD reason.)
//!
//! The fake's `$/progress` token is titled "Reticulating splines" on
//! purpose: the readiness tracker must derive readiness from the token
//! lifecycle (begin/end pairing), never from recognizing anyone's UI
//! prose, so this also pins bead `fb7d73`'s contract through the full
//! client wire path.

use std::io::{BufRead, Write};

// ─────────────────────────────────────────────────────────────────────────
// Fake language server (child mode)
// ─────────────────────────────────────────────────────────────────────────

fn read_frame(stdin: &mut impl BufRead) -> Option<serde_json::Value> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if stdin.read_line(&mut line).ok()? == 0 {
            return None; // EOF — client went away.
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(v) = trimmed.strip_prefix("Content-Length: ") {
            content_length = v.parse().ok();
        }
    }
    let mut buf = vec![0u8; content_length?];
    stdin.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

fn write_frame(stdout: &mut impl Write, msg: &serde_json::Value) {
    let body = serde_json::to_string(msg).expect("serialize frame");
    write!(stdout, "Content-Length: {}\r\n\r\n{}", body.len(), body).expect("write frame");
    stdout.flush().expect("flush frame");
}

fn notify(stdout: &mut impl Write, method: &str, params: serde_json::Value) {
    write_frame(
        stdout,
        &serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params }),
    );
}

fn respond(stdout: &mut impl Write, id: &serde_json::Value, result: serde_json::Value) {
    write_frame(
        stdout,
        &serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }),
    );
}

/// One symbol, `fn answer` at line 0. `kind: 12` = Function.
fn document_symbols() -> serde_json::Value {
    serde_json::json!([{
        "name": "answer",
        "kind": 12,
        "range": {
            "start": { "line": 0, "character": 0 },
            "end":   { "line": 2, "character": 1 }
        },
        "selectionRange": {
            "start": { "line": 0, "character": 3 },
            "end":   { "line": 0, "character": 9 }
        }
    }])
}

/// Minimal scripted LSP server: answers by METHOD, so it is robust to the
/// client's request ordering and id assignment. Exits on `exit` or EOF.
fn fake_ls_main() -> ! {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = std::io::BufReader::new(stdin.lock());
    let mut writer = stdout.lock();

    while let Some(msg) = read_frame(&mut reader) {
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        match (msg.get("id"), method) {
            (Some(id), "initialize") => {
                respond(&mut writer, id, serde_json::json!({ "capabilities": {} }));
            }
            (None, "initialized") => {
                // One full work-done-progress cycle, under a title no
                // string-matcher has ever heard of. The client must reach
                // readiness from the begin/end pairing alone.
                notify(
                    &mut writer,
                    "$/progress",
                    serde_json::json!({
                        "token": 1,
                        "value": { "kind": "begin", "title": "Reticulating splines" }
                    }),
                );
                notify(
                    &mut writer,
                    "$/progress",
                    serde_json::json!({ "token": 1, "value": { "kind": "end" } }),
                );
            }
            (Some(id), "textDocument/documentSymbol") => {
                respond(&mut writer, id, document_symbols());
            }
            (Some(id), "textDocument/hover") => {
                respond(
                    &mut writer,
                    id,
                    serde_json::json!({ "contents": "fn answer() -> i32" }),
                );
            }
            // Empty defs/refs: legal server answers; the hover row is the
            // enrichment signal this test asserts on.
            (Some(id), "textDocument/definition" | "textDocument/references") => {
                respond(&mut writer, id, serde_json::json!([]));
            }
            (Some(id), "shutdown") => {
                respond(&mut writer, id, serde_json::Value::Null);
            }
            (None, "exit") => std::process::exit(0),
            // Any other request gets a bare ack so nothing deadlocks.
            (Some(id), _) => respond(&mut writer, id, serde_json::Value::Null),
            (None, _) => {}
        }
    }
    std::process::exit(0);
}

// ─────────────────────────────────────────────────────────────────────────
// The test (parent mode)
// ─────────────────────────────────────────────────────────────────────────

#[cfg(unix)]
fn run_test() {
    use leyline_cli_lib::daemon::enrichment::EnrichmentPass;

    let tmp = tempfile::TempDir::new().expect("tempdir");

    // The source file the pass will enrich.
    let source_dir = tmp.path().join("src-root");
    std::fs::create_dir(&source_dir).expect("mk source dir");
    std::fs::write(
        source_dir.join("lib.rs"),
        "fn answer() -> i32 {\n    42\n}\n",
    )
    .expect("write lib.rs");

    // PATH shim: `rust-analyzer` IS this binary, via symlink — the fake
    // mode triggers on argv[0]. A `#!/bin/sh` wrapper script does not
    // survive here: when the workspace-unified feature set links this test
    // against a dylib, cargo makes it loadable through `DYLD_*` variables,
    // and macOS SIP strips `DYLD_*` across the exec of protected binaries
    // like /bin/sh — the re-exec'd fake then dies in dyld ("no LC_RPATH's
    // found") before reaching main, which the client can only see as
    // "server closed connection". A symlink keeps the spawn a direct exec
    // of this binary, so the loader environment survives intact.
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir(&bin_dir).expect("mk shim dir");
    let this_exe = std::env::current_exe().expect("current_exe");
    let shim = bin_dir.join("rust-analyzer");
    std::os::unix::fs::symlink(&this_exe, &shim).expect("symlink shim");
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    // SAFETY: single-threaded here — the tokio runtime is built after this.
    unsafe { std::env::set_var("PATH", &path) };

    // Minimal `_source` row so the pass discovers the file. Everything else
    // (`_lsp`, `_lsp_hover`, …) the pass creates itself.
    let conn = rusqlite::Connection::open_in_memory().expect("open sqlite");
    conn.execute_batch(
        "CREATE TABLE _source (id TEXT PRIMARY KEY, language TEXT NOT NULL, path TEXT);",
    )
    .expect("create _source");
    conn.execute(
        "INSERT INTO _source (id, language) VALUES ('lib.rs', 'rust')",
        [],
    )
    .expect("insert _source row");

    // Multi-thread runtime: `LspEnrichmentPass::run` bridges into the
    // runtime via `block_in_place`, which is only legal on a multi-thread
    // worker — same shape as `lsp_enrich_skip_surface_test`'s
    // `#[tokio::test(flavor = "multi_thread")]`.
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let stats = rt.block_on(async {
        let pass = leyline_cli_lib::daemon::lsp_pass::LspEnrichmentPass::new();
        let files = ["lib.rs".to_string()];
        pass.run(&conn, &source_dir, Some(&files))
            .expect("pass runs")
    });

    // The scripted server produced one symbol and one hover, so a healthy
    // pipeline must report work AND have written it. A stubbed
    // `enrich_files_with_client` can fake the counts or the rows, never
    // both:
    //   Ok((0,0))          → items_added == 0            → first assert
    //   Ok((1,1))/(1,0)/(0,1) → counts lie, but no rows were written
    //                                                    → row asserts
    assert!(
        stats.items_added > 0,
        "enrichment reported zero items against a server that returned \
         a symbol and a hover; stats: {stats:?}"
    );
    let lsp_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM _lsp", [], |r| r.get(0))
        .expect("_lsp count");
    assert!(
        lsp_rows > 0,
        "no _lsp rows written — the symbol merge never ran"
    );
    let hover_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM _lsp_hover", [], |r| r.get(0))
        .expect("_lsp_hover count");
    assert!(
        hover_rows > 0,
        "no _lsp_hover rows written — the per-symbol enrichment never ran"
    );

    eprintln!(
        "lsp-enrich-pipeline: ok — items_added={}, _lsp={lsp_rows}, _lsp_hover={hover_rows}",
        stats.items_added
    );
}

fn main() {
    // Fake-server mode triggers on the NAME this binary was invoked as —
    // the PATH symlink created by the test — because the enrichment pass
    // spawns `rust-analyzer` with no arguments, so argv[0] is the only
    // channel. (`--fake-ls` kept as an explicit spelling for hand-runs.)
    let invoked_as = std::env::args().next().unwrap_or_default();
    let basename = invoked_as.rsplit('/').next().unwrap_or("");
    if basename == "rust-analyzer" || std::env::args().nth(1).as_deref() == Some("--fake-ls") {
        fake_ls_main();
    }
    #[cfg(unix)]
    run_test();
    #[cfg(not(unix))]
    eprintln!("lsp-enrich-pipeline: skipped (unix-only: PATH-symlink shim)");
}
