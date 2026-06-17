//! Shared-secret token authn for the MCP HTTP wire (ADR-0022, bead
//! `ley-line-open-b885d1`).
//!
//! Adopts lectio's `auth.rs` pattern: a 32-byte random token hex-encoded
//! at `~/.local/share/leyline/daemon.token` (mode `0600`). Every
//! `/mcp` request must include `x-leyline-token: <hex>`; comparison is
//! constant-time via `subtle::ConstantTimeEq`.
//!
//! Threat closed: DNS-rebinding probes against `127.0.0.1` and same-user
//! local processes that can't read the token file. The UDS path is NOT
//! gated here — the socket's parent directory is `0600` already, so any
//! process that can `connect(2)` to the socket is a process that can
//! read the token file.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use rand::RngCore;
use serde_json::json;
use subtle::ConstantTimeEq;

/// Header name carrying the shared secret on every `/mcp` request.
pub const TOKEN_HEADER: &str = "x-leyline-token";

/// Default on-disk path for the token file. Resolved against `dirs::data_dir()`
/// (`~/.local/share` on Linux, `~/Library/Application Support` on macOS) so
/// the location is XDG-friendly and matches lectio's convention.
pub fn default_token_path() -> Result<PathBuf> {
    let base = dirs::data_dir().context("resolve XDG data dir for leyline token")?;
    Ok(base.join("leyline").join("daemon.token"))
}

/// Load the token from `path` if it exists; otherwise generate a fresh
/// 32-byte random token, hex-encode it, and write to `path` with mode
/// `0600`. Returns the in-memory token string.
///
/// On-disk format: lower-case hex, no trailing newline. `lectio init`'s
/// shape — easy to `cat` + paste into a header.
pub fn load_or_generate(path: &Path) -> Result<String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let token = existing.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
        // File exists but is empty — fall through to generation.
    }

    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let token = hex::encode(bytes);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create token dir {}", parent.display()))?;
    }

    write_token_file(path, &token)
        .with_context(|| format!("write token file {}", path.display()))?;

    Ok(token)
}

#[cfg(unix)]
fn write_token_file(path: &Path, token: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(token.as_bytes())?;
    f.flush()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_token_file(path: &Path, token: &str) -> Result<()> {
    std::fs::write(path, token)?;
    Ok(())
}

/// Axum middleware: rejects requests whose `x-leyline-token` header
/// doesn't match the daemon's expected token. Missing header → 401.
/// Length mismatch → 401 (constant-time-friendly: the lengths-match
/// branch is evaluated before the bytes compare).
pub async fn require_token(
    State(expected): State<Arc<String>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let provided = req
        .headers()
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let lengths_match = provided.len() == expected.len();
    // ConstantTimeEq operates on equal-length byte slices; only compare
    // when lengths match. The length check is intentionally before the
    // byte compare — leaking the length of the (always 64-char hex)
    // expected token is fine since it's a public constant in this ADR.
    let bytes_match = if lengths_match {
        provided.as_bytes().ct_eq(expected.as_bytes()).unwrap_u8() == 1
    } else {
        false
    };
    if lengths_match && bytes_match {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_or_generate_creates_file_when_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("daemon.token");
        assert!(!path.exists());

        let token = load_or_generate(&path).unwrap();
        assert_eq!(token.len(), 64, "32 random bytes hex-encoded = 64 chars");
        assert!(path.exists());

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            on_disk.trim(),
            token,
            "on-disk content must match returned token"
        );
    }

    #[test]
    fn load_or_generate_reuses_existing_token() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("daemon.token");
        let first = load_or_generate(&path).unwrap();
        let second = load_or_generate(&path).unwrap();
        assert_eq!(
            first, second,
            "second call must reuse the existing token, not regenerate"
        );
    }

    #[test]
    fn load_or_generate_treats_empty_file_as_missing() {
        // If a stale (empty) token file is left over from a botched
        // install, the daemon should regenerate rather than refuse.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("daemon.token");
        std::fs::write(&path, "").unwrap();
        let token = load_or_generate(&path).unwrap();
        assert_eq!(token.len(), 64);
    }

    #[test]
    fn load_or_generate_trims_trailing_whitespace() {
        // Hand-edited token files often have a trailing newline. The
        // load path tolerates it without leaking the whitespace into
        // the constant-time compare.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("daemon.token");
        let raw = "deadbeef".repeat(8);
        std::fs::write(&path, format!("{raw}\n")).unwrap();
        let token = load_or_generate(&path).unwrap();
        assert_eq!(token, raw, "trailing whitespace must be trimmed");
    }

    #[cfg(unix)]
    #[test]
    fn load_or_generate_writes_file_with_0600_perms() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("daemon.token");
        load_or_generate(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        // Lower bits hold rwx for owner/group/other; mask off file-type.
        assert_eq!(mode & 0o777, 0o600, "token file must be owner-only RW");
    }

    #[test]
    fn token_header_name_is_x_leyline_token() {
        // Drift guard: clients hand-write this header. Renaming it is
        // a wire break; pin the constant.
        assert_eq!(TOKEN_HEADER, "x-leyline-token");
    }

    // -----------------------------------------------------------------
    // Middleware end-to-end — drive a minimal axum Router that wraps
    // the gate around a noop handler. Exercises the same code path as
    // the production wiring in `mcp::spawn` (route → middleware →
    // handler) without spinning up a TCP listener.
    // -----------------------------------------------------------------

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::middleware as axum_middleware;
    use axum::routing::post;
    use tower::ServiceExt;

    fn build_test_router(token: &str) -> Router {
        let expected: Arc<String> = Arc::new(token.to_string());
        Router::new()
            .route("/mcp", post(noop_handler))
            .layer(axum_middleware::from_fn_with_state(expected, require_token))
    }

    async fn noop_handler() -> &'static str {
        "ok"
    }

    fn make_request(token: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().method("POST").uri("/mcp");
        if let Some(t) = token {
            builder = builder.header(TOKEN_HEADER, t);
        }
        builder.body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn middleware_accepts_request_with_correct_token() {
        let router = build_test_router("deadbeef");
        let response = router
            .oneshot(make_request(Some("deadbeef")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn middleware_rejects_request_without_token_header() {
        let router = build_test_router("deadbeef");
        let response = router.oneshot(make_request(None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn middleware_rejects_request_with_wrong_token() {
        let router = build_test_router("deadbeef");
        let response = router
            .oneshot(make_request(Some("0000beef")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn middleware_rejects_request_with_length_mismatch() {
        // The lengths-match short-circuit must reject WITHOUT calling
        // ConstantTimeEq (which would panic on unequal slice lengths).
        // A regression where the bytes are compared without the length
        // check first would surface as a panic, not a 401.
        let router = build_test_router("deadbeef");
        let response = router
            .oneshot(make_request(Some("deadbeefdeadbeef")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn middleware_rejects_when_provided_token_is_empty_string() {
        // Edge case: client sends the header with an empty value. Must
        // 401, not pass through as "no length to compare".
        let router = build_test_router("deadbeef");
        let response = router.oneshot(make_request(Some(""))).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn middleware_401_body_is_json_with_unauthorized_message() {
        // The 401 body shape is part of the contract (mache/cloister
        // discover the auth requirement by parsing the structured
        // error rather than only the status). Pin the shape.
        let router = build_test_router("deadbeef");
        let response = router.oneshot(make_request(None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body_bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(parsed["error"], "unauthorized");
    }
}
