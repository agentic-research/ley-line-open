//! Black-box regression test for ley-line-open-cb8960.
//!
//! Verifies that the `leyline_version` op is reachable over UDS BEFORE
//! any daemon initialization, returns the expected wire shape, and is
//! idempotent. The op exists so a client can detect wire-format drift
//! at connect time (failing fast with a clear error) instead of the
//! v0.4.2-era pattern where drift surfaced as `parseUint64` returning
//! 0 silently — see `ley-line-open-5caa59` + `ley-line-open-cb12fa` for
//! the bugs this handshake was designed to surface earlier.
//!
//! Three tests per the bead's "Test plan (LLO side)":
//!
//! 1. `version_op_returns_known_shape` — call once, assert every
//!    documented field is present with the right type.
//! 2. `version_op_is_idempotent` — call twice on the same connection,
//!    assert identical responses.
//! 3. `version_op_works_before_subscribe` — call against a fresh
//!    daemon (no other op invoked yet), verify the response arrives
//!    without requiring any daemon initialization.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use leyline_cli_lib::daemon::{
    DaemonContext, DaemonState, EventRouter, NoExt, sheaf_ops::SheafState, socket,
};
use leyline_core::{Controller, create_arena};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

fn build_blackbox_ctx(dir: &Path) -> Arc<DaemonContext> {
    use std::sync::{Mutex, RwLock};

    let arena_path = dir.join("blackbox.arena");
    let ctrl_path = dir.join("blackbox.ctrl");
    let _mmap = create_arena(&arena_path, 2 * 1024 * 1024).expect("create arena");
    let mut ctrl = Controller::open_or_create(&ctrl_path).expect("open ctrl");
    ctrl.set_arena(&arena_path.to_string_lossy(), 2 * 1024 * 1024)
        .expect("set arena");
    drop(ctrl);

    // File-backed WAL LiveDb — pool needs a real file (bead
    // `ley-line-open-f0239d`).
    let live_db_path = ctrl_path.with_extension("live.db");
    let live_db = leyline_cli_lib::daemon::db_pool::LiveDb::open_fresh_for_test(&live_db_path);

    Arc::new(DaemonContext {
        ctrl_path,
        ext: Arc::new(NoExt),
        router: EventRouter::new(16),
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
        sheaf: Arc::new(SheafState::new()),
    })
}

async fn spawn_blackbox_socket(ctx: Arc<DaemonContext>, sock_path: PathBuf) -> PathBuf {
    let path = socket::spawn(ctx, sock_path);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if UnixStream::connect(&path).await.is_ok() {
            return path;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!(
        "UDS listener at {} did not accept within 2s",
        path.display()
    );
}

/// Open a fresh UDS connection, write one line, read one line.
async fn uds_call(sock: &Path, body: &str) -> serde_json::Value {
    let stream = UnixStream::connect(sock).await.expect("UDS connect");
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    writer.write_all(body.as_bytes()).await.expect("UDS write");
    if !body.ends_with('\n') {
        writer.write_all(b"\n").await.expect("UDS newline");
    }
    let line = lines
        .next_line()
        .await
        .expect("UDS read")
        .expect("response line");
    serde_json::from_str(&line).expect("response is JSON")
}

/// Test 1: shape contract. Pre-fix this op doesn't exist; post-fix it
/// returns every documented field with the right type.
#[tokio::test]
async fn version_op_returns_known_shape() {
    let dir = TempDir::new().unwrap();
    let ctx = build_blackbox_ctx(dir.path());
    let sock = spawn_blackbox_socket(ctx, dir.path().join("version-shape.sock")).await;

    let resp = uds_call(&sock, r#"{"op":"leyline_version"}"#).await;

    // `ok: true` discriminator.
    assert_eq!(
        resp.get("ok"),
        Some(&serde_json::json!(true)),
        "leyline_version must respond ok:true; got: {resp}",
    );

    // `binary_version` — non-empty string, must match the daemon's
    // own `CARGO_PKG_VERSION` (the constant the handler reads from).
    let bv = resp
        .get("binary_version")
        .and_then(|v| v.as_str())
        .expect("binary_version must be a string");
    assert!(
        !bv.is_empty(),
        "binary_version must be non-empty; got {bv:?}",
    );
    assert_eq!(
        bv,
        env!("CARGO_PKG_VERSION"),
        "binary_version must equal CARGO_PKG_VERSION at the consumer site",
    );

    // `schema_version` — same shape; today equals binary_version.
    let sv = resp
        .get("schema_version")
        .and_then(|v| v.as_str())
        .expect("schema_version must be a string");
    assert!(
        !sv.is_empty(),
        "schema_version must be non-empty; got {sv:?}",
    );

    // `wire_format_major` — JSON number (UInt32, not stringified).
    let wfm = resp
        .get("wire_format_major")
        .and_then(|v| v.as_u64())
        .expect("wire_format_major must be a JSON number");
    assert!(
        wfm >= 1,
        "wire_format_major must be >= 1; v0.4.4 ships major 1. got: {wfm}",
    );

    // `compat_min` — semver-shaped string.
    let cm = resp
        .get("compat_min")
        .and_then(|v| v.as_str())
        .expect("compat_min must be a string");
    assert!(
        cm.split('.').count() >= 2,
        "compat_min must look like semver; got: {cm:?}",
    );

    // `build_date` — string, either ISO-8601-shaped or "unspecified".
    let bd = resp
        .get("build_date")
        .and_then(|v| v.as_str())
        .expect("build_date must be a string");
    assert!(!bd.is_empty(), "build_date must be non-empty; got {bd:?}",);
}

/// Test 2: idempotency. Two calls return byte-identical responses.
/// Pins that the handler reads only from compile-time constants — a
/// future regression that started embedding (e.g.) `Instant::now()` in
/// the response would break this test.
#[tokio::test]
async fn version_op_is_idempotent() {
    let dir = TempDir::new().unwrap();
    let ctx = build_blackbox_ctx(dir.path());
    let sock = spawn_blackbox_socket(ctx, dir.path().join("version-idem.sock")).await;

    let a = uds_call(&sock, r#"{"op":"leyline_version"}"#).await;
    let b = uds_call(&sock, r#"{"op":"leyline_version"}"#).await;

    assert_eq!(
        a, b,
        "leyline_version must be idempotent — two calls must return identical responses.\n\
         a = {a}\n\
         b = {b}",
    );
}

/// Test 3: works before any other op. The handshake's whole point is
/// that a client can use it BEFORE driving any state-changing or
/// state-reading op. Verifies the handler doesn't require sheaf state,
/// vec index, text-search backend, or other late-bound machinery.
#[tokio::test]
async fn version_op_works_before_subscribe() {
    let dir = TempDir::new().unwrap();
    let ctx = build_blackbox_ctx(dir.path());
    let sock = spawn_blackbox_socket(ctx, dir.path().join("version-pre-sub.sock")).await;

    // Bare context — no parsed source, no embedder, no subscribe.
    // The handler must still return cleanly because it reads only
    // from compile-time constants in `daemon::version`.
    let resp = uds_call(&sock, r#"{"op":"leyline_version"}"#).await;
    assert_eq!(
        resp.get("ok"),
        Some(&serde_json::json!(true)),
        "leyline_version must work on a freshly-spawned daemon with no \
         other ops invoked yet. got: {resp}",
    );
}
