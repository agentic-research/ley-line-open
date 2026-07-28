//! The invoked contract — `ley-line-open-4ec276`.
//!
//! The library half is reachable from Rust only, so mache (Go) keeps its own
//! emitter and the coverage rules live twice in two languages. The binary is
//! what unifies them: JSON in, JSON out, exit code carries the verdict. These
//! tests exercise it the way a Go, TypeScript or Taskfile caller does — as a
//! subprocess — because that is the interface being promised.

use std::io::Write;
use std::process::{Command, Stdio};

fn descriptor(extra_group_tool: Option<&str>) -> String {
    let ghost = extra_group_tool
        .map(|t| format!(", \"{t}\""))
        .unwrap_or_default();
    format!(
        r#"{{
  "meta": {{
    "name": "io.github.example/thing",
    "description": "A thing.",
    "version": "1.2.3",
    "repository_url": "https://github.com/example/thing.git",
    "repository_source": "github",
    "packages": [
      {{
        "oci_image": "ghcr.io/example/thing",
        "oci_version": "v1.2.3",
        "transport": {{ "type": "streamable-http", "url": "http://localhost:1234/mcp" }}
      }}
    ]
  }},
  "tools": ["a"],
  "groups": [{{ "name": "g", "advertised_prefix": "g_", "upstream_names": ["a"{ghost}] }}]
}}"#
    )
}

fn run(input: &str) -> (Option<i32>, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_leyline-mcp-descriptor"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn emitter");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write descriptor");
    let out = child.wait_with_output().expect("wait");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn valid_descriptor_renders_and_exits_zero() {
    let (code, stdout, stderr) = run(&descriptor(None));
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(
        stdout.contains("\"identifier\": \"ghcr.io/example/thing\""),
        "{stdout}"
    );
    assert!(stdout.contains("\"version\": \"v1.2.3\""), "{stdout}");
}

/// The property that makes `emitter > server.json` safe.
///
/// A caller redirecting stdout into a committed artifact must never end up with
/// a truncated or half-written file when validation fails — a drift gate would
/// then compare against garbage. So nothing is printed until `render` succeeds,
/// and the reason goes to stderr.
#[test]
fn invalid_descriptor_writes_nothing_to_stdout_and_explains_on_stderr() {
    let (code, stdout, stderr) = run(&descriptor(Some("ghost")));
    assert_eq!(code, Some(1), "must exit non-zero");
    assert!(
        stdout.is_empty(),
        "stdout must stay EMPTY so a redirect cannot truncate a good artifact; got {stdout:?}",
    );
    assert!(
        stderr.contains("ghost"),
        "stderr must name the offending tool: {stderr}",
    );
}

#[test]
fn unparseable_input_fails_without_output() {
    let (code, stdout, stderr) = run("not json");
    assert_eq!(code, Some(1));
    assert!(stdout.is_empty(), "got {stdout:?}");
    assert!(!stderr.is_empty(), "a parse failure must say so");
}
