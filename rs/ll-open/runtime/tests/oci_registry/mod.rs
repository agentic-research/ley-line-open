//! A hermetic, in-process OCI distribution registry for the registry-pull
//! tests (bead `ley-line-open-97b636`).
//!
//! It serves one repository from an OCI image layout directory over plain
//! HTTP on a loopback port, the way ghcr.io and Docker Hub serve a public
//! image: every `/v2/` request without the bearer token gets a `401` whose
//! `WWW-Authenticate` challenge names the token endpoint, `/token` hands out
//! an anonymous token, and a blob request is redirected to a "CDN" path
//! (`/cdn/…`) that serves the bytes and expects no `Authorization`.
//!
//! Every request it parses is recorded, so a test can state exactly what a
//! pull fetched. `Behavior` lets a test lie: serve other bytes for a digest,
//! redirect too often or to another origin, challenge for credentials, or
//! stall a body.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// The token `/token` issues. A test asserts it never appears in an error.
pub const TOKEN: &str = "anonymous-test-token-5f3c";

/// One request the registry parsed.
#[derive(Debug, Clone)]
pub struct Seen {
    pub path: String,
    pub authorization: Option<String>,
    pub accept: Option<String>,
}

/// How the registry misbehaves, if at all.
#[derive(Debug, Clone)]
pub struct Behavior {
    /// Bytes served for a digest instead of the layout's blob.
    pub tamper: BTreeMap<String, Vec<u8>>,
    /// How many redirects a blob request takes before its bytes are served
    /// (`/v2/…/blobs/<d>` → `/cdn/1/<d>` → … → `/cdn/<n>/<d>`).
    pub blob_redirects: u32,
    /// Redirect every blob request here instead (an absolute URL prefix the
    /// digest is appended to).
    pub redirect_blobs_to: Option<String>,
    /// The `WWW-Authenticate` challenge a token-less `/v2/` request gets.
    /// `None` serves without any token.
    pub challenge: Option<String>,
    /// How a blob's bytes are sent.
    pub slow_blobs: Slow,
    /// Serve only this digest slowly (every blob when `None`).
    pub slow_only: Option<String>,
    /// `/token` answers with this status line and body instead of the
    /// token (a `401` carries a challenge naming `/token` again).
    pub token_reply: Option<(String, Vec<u8>)>,
    /// Challenge every `/v2/` request, the token's included.
    pub refuse_token: bool,
}

/// How slowly a blob is served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slow {
    /// At once.
    No,
    /// Never send the response headers.
    Headers,
    /// Send the headers, then never the body.
    Body,
    /// Send the body one byte at a time, this far apart.
    Drip(Duration),
}

impl Default for Behavior {
    fn default() -> Self {
        Self {
            tamper: BTreeMap::new(),
            blob_redirects: 1,
            redirect_blobs_to: None,
            challenge: Some(String::new()),
            slow_blobs: Slow::No,
            slow_only: None,
            token_reply: None,
            refuse_token: false,
        }
    }
}

struct State {
    layout: PathBuf,
    repository: String,
    behavior: Behavior,
    seen: Vec<Seen>,
    /// Connections that never yielded a parseable HTTP request (a TLS
    /// handshake sent to this plaintext server, for one).
    unparsed: usize,
}

pub struct TestRegistry {
    /// `127.0.0.1:<port>`.
    pub authority: String,
    state: Arc<Mutex<State>>,
}

impl TestRegistry {
    /// Serve `layout` as `repository`, behaving per `behavior`.
    pub fn serve(layout: &Path, repository: &str, behavior: Behavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the test registry");
        let authority = listener.local_addr().expect("local address").to_string();
        let state = Arc::new(Mutex::new(State {
            layout: layout.to_path_buf(),
            repository: repository.to_string(),
            behavior,
            seen: Vec::new(),
            unparsed: 0,
        }));
        let shared = Arc::clone(&state);
        let origin = authority.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let state = Arc::clone(&shared);
                let origin = origin.clone();
                thread::spawn(move || handle(stream, &state, &origin));
            }
        });
        Self { authority, state }
    }

    /// `<authority>/<repository>@<digest>`.
    pub fn reference(&self, digest: &str) -> String {
        let repository = self.state.lock().unwrap().repository.clone();
        format!("{}/{repository}@{digest}", self.authority)
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.state.lock().unwrap().seen.clone()
    }

    pub fn paths(&self) -> Vec<String> {
        self.seen().into_iter().map(|seen| seen.path).collect()
    }

    pub fn unparsed(&self) -> usize {
        self.state.lock().unwrap().unparsed
    }
}

fn handle(stream: TcpStream, state: &Mutex<State>, origin: &str) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(stream.try_clone().expect("clone the stream"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || !request_line.ends_with("\r\n") {
        state.lock().unwrap().unparsed += 1;
        return;
    }
    let mut parts = request_line.split_whitespace();
    let (Some("GET"), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        state.lock().unwrap().unparsed += 1;
        return;
    };
    if !version.starts_with("HTTP/1.") {
        state.lock().unwrap().unparsed += 1;
        return;
    }
    let mut headers = BTreeMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            return;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let seen = Seen {
        path: target.to_string(),
        authorization: headers.get("authorization").cloned(),
        accept: headers.get("accept").cloned(),
    };
    let (layout, repository, behavior) = {
        let mut state = state.lock().unwrap();
        state.seen.push(seen.clone());
        (
            state.layout.clone(),
            state.repository.clone(),
            state.behavior.clone(),
        )
    };
    let mut stream = stream;
    let response = route(&seen, &layout, &repository, &behavior, origin);
    match response {
        Response::Stall(headers) => {
            let _ = stream.write_all(headers.as_bytes());
            let _ = stream.flush();
            thread::sleep(Duration::from_secs(20));
        }
        Response::Drip(headers, bytes, gap) => {
            let _ = stream.write_all(headers.as_bytes());
            for byte in bytes {
                let _ = stream.flush();
                thread::sleep(gap);
                if stream.write_all(&[byte]).is_err() {
                    return;
                }
            }
            let _ = stream.flush();
        }
        Response::Full(bytes) => {
            let _ = stream.write_all(&bytes);
            let _ = stream.flush();
        }
    }
    // Drain nothing further: one request per connection.
    let _ = reader.read(&mut [0; 1]);
}

enum Response {
    Full(Vec<u8>),
    Stall(String),
    Drip(String, Vec<u8>, Duration),
}

fn reply(status: &str, headers: &[(&str, String)], body: &[u8]) -> Response {
    let mut out = format!("HTTP/1.1 {status}\r\n");
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    Response::Full(bytes)
}

fn blob_bytes(layout: &Path, behavior: &Behavior, digest: &str) -> Option<Vec<u8>> {
    if let Some(bytes) = behavior.tamper.get(digest) {
        return Some(bytes.clone());
    }
    let hex = digest.strip_prefix("sha256:")?;
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    std::fs::read(layout.join("blobs/sha256").join(hex)).ok()
}

fn serve_blob(layout: &Path, behavior: &Behavior, digest: &str, content_type: &str) -> Response {
    match blob_bytes(layout, behavior, digest) {
        Some(bytes)
            if behavior.slow_blobs != Slow::No
                && content_type == "application/octet-stream"
                && behavior
                    .slow_only
                    .as_deref()
                    .is_none_or(|only| only == digest) =>
        {
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            );
            match behavior.slow_blobs {
                Slow::Headers => Response::Stall(String::new()),
                Slow::Drip(gap) => Response::Drip(headers, bytes, gap),
                _ => Response::Stall(headers),
            }
        }
        Some(bytes) => reply("200 OK", &[("Content-Type", content_type.into())], &bytes),
        None => reply(
            "404 Not Found",
            &[],
            b"{\"errors\":[{\"code\":\"BLOB_UNKNOWN\"}]}",
        ),
    }
}

fn route(
    seen: &Seen,
    layout: &Path,
    repository: &str,
    behavior: &Behavior,
    origin: &str,
) -> Response {
    let path = seen.path.as_str();
    if path.starts_with("/token?") {
        if let Some((status, body)) = &behavior.token_reply {
            let challenge = format!("Bearer realm=\"http://{origin}/token\",service=\"again\"");
            return reply(status, &[("WWW-Authenticate", challenge)], body);
        }
        return reply(
            "200 OK",
            &[("Content-Type", "application/json".into())],
            format!("{{\"token\":\"{TOKEN}\",\"expires_in\":300}}").as_bytes(),
        );
    }
    if let Some(rest) = path.strip_prefix("/cdn/") {
        if seen.authorization.is_some() {
            // A CDN must never be handed the registry's token.
            return reply("400 Bad Request", &[], b"authorization sent to the CDN");
        }
        let Some((hop, digest)) = rest.split_once('/') else {
            return reply("404 Not Found", &[], b"");
        };
        let hop: u32 = hop.parse().unwrap_or(u32::MAX);
        if hop < behavior.blob_redirects {
            // A relative redirect, as some CDNs send.
            return reply(
                "307 Temporary Redirect",
                &[("Location", format!("/cdn/{}/{digest}", hop + 1))],
                b"",
            );
        }
        return serve_blob(layout, behavior, digest, "application/octet-stream");
    }
    let Some(rest) = path.strip_prefix("/v2/") else {
        return reply("404 Not Found", &[], b"");
    };
    if let Some(challenge) = &behavior.challenge
        && (behavior.refuse_token
            || seen.authorization.as_deref() != Some(&format!("Bearer {TOKEN}")))
    {
        let challenge = if challenge.is_empty() {
            format!(
                "Bearer realm=\"http://{origin}/token\",service=\"test-registry\",scope=\"repository:{repository}:pull\""
            )
        } else {
            challenge.clone()
        };
        return reply(
            "401 Unauthorized",
            &[("WWW-Authenticate", challenge)],
            b"{\"errors\":[{\"code\":\"UNAUTHORIZED\"}]}",
        );
    }
    let Some(rest) = rest.strip_prefix(&format!("{repository}/")) else {
        return reply(
            "404 Not Found",
            &[],
            b"{\"errors\":[{\"code\":\"NAME_UNKNOWN\"}]}",
        );
    };
    if let Some(digest) = rest.strip_prefix("manifests/") {
        let content_type = blob_bytes(layout, behavior, digest)
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|document| document["mediaType"].as_str().map(str::to_string))
            .unwrap_or_else(|| "application/vnd.oci.image.manifest.v1+json".into());
        return serve_blob(layout, behavior, digest, &content_type);
    }
    if let Some(digest) = rest.strip_prefix("blobs/") {
        if let Some(prefix) = &behavior.redirect_blobs_to {
            return reply(
                "307 Temporary Redirect",
                &[("Location", format!("{prefix}{digest}"))],
                b"",
            );
        }
        if behavior.blob_redirects > 0 {
            // An absolute redirect to the "CDN", as ghcr.io and Docker Hub send.
            return reply(
                "307 Temporary Redirect",
                &[("Location", format!("http://{origin}/cdn/1/{digest}"))],
                b"",
            );
        }
        return serve_blob(layout, behavior, digest, "application/octet-stream");
    }
    reply("404 Not Found", &[], b"")
}
