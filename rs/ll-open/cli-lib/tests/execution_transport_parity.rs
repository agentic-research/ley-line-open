//! Receipt parity across every first-party execution/v1 surface (beads
//! `ley-line-open-f8ebcf`, `ley-line-open-f8a079`).
//!
//! One execution request, four ways in:
//!
//! - **Rust**: the `ExecutionHandler` an embedder calls in-process (the
//!   `RuntimeExecutionHandler` over one `ExecutionService`), plus the
//!   service's own `RunReceiptData` for the receipt;
//! - **daemon**: the execution daemon's control socket, through
//!   `ExecutionClient` — newline-delimited JSON over UDS;
//! - **CLI**: a `leyline execution …` command line, parsed by clap into the
//!   subcommand the binary dispatches and run through `cmd_execution`, the
//!   code that prints what the user sees;
//! - **MCP**: `tools/call` over the MCP UDS transport, with the ADR-0022
//!   token gate enabled, unwrapped from its `content` envelope.
//!
//! Two properties are asserted. **Lifecycle parity**: driving the same
//! request through each surface on a fresh service produces the same answer
//! at every step, identical except for the wall-clock fields
//! (`timestampMs`, `startedAtUnixMs`, `completedAtUnixMs`, the usage derived
//! from them, and the event-log root, which hashes the timestamped log).
//! **Read parity**: one run, read through all four surfaces, yields
//! byte-for-byte the same status, inspection, receipt and errors, with no
//! normalization at all.
//!
//! The backend is the canonical fixture's (`execution_fixture`): this test is
//! about the transports. Backend parity is the conformance matrix's
//! (`runtime/tests/conformance_matrix.rs`, `krun-worker/tests/
//! conformance_matrix.rs`).

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser;
use leyline_cli_lib::daemon::execution::{ExecutionDaemonExt, ExecutionHandler};
use leyline_cli_lib::daemon::{client::ExecutionClient, execution_contract as contract, socket};
use leyline_cli_lib::{Commands, cmd_execution};
use leyline_runtime::{BackendClass, ExecutionService, RunState};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

mod common;
mod execution_fixture;

use execution_fixture::{
    CompletingBackend, grant_bytes, grant_bytes_for, grant_json, spec_bytes, spec_json,
};

/// What a surface answered: the operation's JSON, or its error message.
#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Ok(Value),
    Err(String),
}

/// A daemon reply is an error when it is the `ErrorResponse` envelope.
fn outcome(response: Value) -> Outcome {
    if response.get("ok") == Some(&Value::Bool(false)) {
        Outcome::Err(
            response["error"]
                .as_str()
                .expect("an error envelope carries its message")
                .to_owned(),
        )
    } else {
        Outcome::Ok(response)
    }
}

/// One execution/v1 operation, as each surface spells it.
#[derive(Debug, Clone)]
enum Op {
    Capabilities,
    Provision {
        class: &'static str,
        key: &'static str,
    },
    Status {
        run_id: Option<String>,
    },
    Start {
        spec: Value,
        grant: Value,
    },
    Inspect {
        run_id: String,
    },
    Cancel {
        run_id: String,
    },
    Collect {
        run_id: String,
    },
    Cleanup {
        run_id: String,
    },
}

impl Op {
    /// The daemon wire envelope, from the shared contract constructors.
    fn envelope(&self) -> Value {
        match self {
            Self::Capabilities => contract::capabilities(),
            Self::Provision { class, key } => contract::provision(class, key),
            Self::Status { run_id } => contract::status(run_id.as_deref()),
            Self::Start { spec, grant } => contract::start(spec.clone(), grant.clone()),
            Self::Inspect { run_id } => contract::inspect(run_id, 0),
            Self::Cancel { run_id } => contract::cancel(run_id, Some("parity-cancel")),
            Self::Collect { run_id } => contract::collect(run_id),
            Self::Cleanup { run_id } => contract::cleanup(run_id, Some("parity-cleanup")),
        }
    }
}

/// The `leyline` binary's command line, as far as `Commands` goes.
#[derive(Parser)]
#[command(name = "leyline")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// One service, reachable through every surface at once.
struct Surfaces {
    _dir: TempDir,
    service: Arc<ExecutionService<CompletingBackend>>,
    handler: Arc<dyn ExecutionHandler>,
    control: PathBuf,
    mcp: PathBuf,
    token: String,
    /// Where the CLI's `start` reads its spec and grant from.
    files: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Surface {
    Rust,
    Daemon,
    Cli,
    Mcp,
}

const SURFACES: [Surface; 4] = [Surface::Rust, Surface::Daemon, Surface::Cli, Surface::Mcp];

impl Surfaces {
    /// Must run inside a tokio runtime: both listeners are tokio tasks.
    fn new() -> Self {
        let dir = TempDir::new().expect("surface directory");
        let service = Arc::new(ExecutionService::new(CompletingBackend));
        let handler: Arc<dyn ExecutionHandler> = Arc::new(execution_fixture::handler(&service));
        let ctx = common::daemon_context_with_ext(
            dir.path(),
            Arc::new(ExecutionDaemonExt::new(Arc::clone(&handler))),
        );
        let control = dir.path().join("execution.sock");
        socket::spawn(
            Arc::clone(&ctx),
            control.clone(),
            socket::SocketDiscovery::Unpublished,
        )
        .expect("bind the execution control socket");
        let mcp = dir.path().join("mcp.sock");
        let token = "c".repeat(64);
        leyline_cli_lib::daemon::mcp::spawn_uds(ctx, mcp.clone(), Some(Arc::new(token.clone())))
            .expect("bind the MCP socket");
        let files = dir.path().join("cli");
        std::fs::create_dir(&files).expect("CLI input directory");
        Self {
            _dir: dir,
            service,
            handler,
            control,
            mcp,
            token,
            files,
        }
    }

    async fn call(&self, surface: Surface, op: &Op) -> Outcome {
        match surface {
            Surface::Rust => self.rust(op),
            Surface::Daemon => outcome(
                ExecutionClient::new(&self.control)
                    .call(op.envelope())
                    .await
                    .expect("daemon reply"),
            ),
            Surface::Cli => {
                let Commands::Execution { command } = Cli::try_parse_from(self.argv(op))
                    .expect("the command line parses")
                    .command
                else {
                    panic!("`leyline execution` parsed as another command");
                };
                // The request a command line sends is the contract's envelope.
                let (control, request) = cmd_execution::request(&command).expect("CLI request");
                assert_eq!(control, self.control.as_path());
                assert_eq!(request, op.envelope(), "the CLI's request for {op:?}");
                outcome(cmd_execution::respond(&command).await.expect("CLI reply"))
            }
            Surface::Mcp => self.mcp(op).await,
        }
    }

    /// In-process: the handler method the daemon's dispatch calls, given the
    /// envelope's arguments.
    fn rust(&self, op: &Op) -> Outcome {
        let mut input = op.envelope();
        input.as_object_mut().expect("envelope object").remove("op");
        let handler = &self.handler;
        let result = match op {
            Op::Capabilities => handler.capabilities(),
            Op::Provision { .. } => handler.provision(&input),
            Op::Status { .. } => handler.status(&input),
            Op::Start { .. } => handler.start(&input),
            Op::Inspect { .. } => handler.inspect(&input),
            Op::Cancel { .. } => handler.cancel(&input),
            Op::Collect { .. } => handler.collect(&input),
            Op::Cleanup { .. } => handler.cleanup(&input),
        };
        match result {
            Ok(json) => Outcome::Ok(serde_json::from_str(&json).expect("handler JSON")),
            Err(error) => Outcome::Err(format!("{error:#}")),
        }
    }

    fn argv(&self, op: &Op) -> Vec<String> {
        let control = self.control.display().to_string();
        let mut argv: Vec<String> = vec!["leyline".into(), "execution".into()];
        let mut push = |values: &[&str]| argv.extend(values.iter().map(|v| (*v).to_owned()));
        match op {
            Op::Capabilities => push(&["capabilities", "--control", &control]),
            Op::Provision { class, key } => push(&[
                "provision",
                "--control",
                &control,
                "--backend-class",
                class,
                "--idempotency-key",
                key,
            ]),
            Op::Status { run_id: None } => push(&["status", "--control", &control]),
            Op::Status {
                run_id: Some(run_id),
            } => push(&["status", "--control", &control, "--run-id", run_id]),
            Op::Start { spec, grant } => {
                let spec_path = write(&self.files, "spec.json", spec);
                let grant_path = write(&self.files, "grant.json", grant);
                push(&[
                    "start",
                    "--control",
                    &control,
                    "--spec",
                    &spec_path,
                    "--grant",
                    &grant_path,
                ]);
            }
            Op::Inspect { run_id } => push(&[
                "inspect",
                "--control",
                &control,
                "--run-id",
                run_id,
                "--after-sequence",
                "0",
            ]),
            Op::Cancel { run_id } => push(&[
                "cancel",
                "--control",
                &control,
                "--run-id",
                run_id,
                "--idempotency-key",
                "parity-cancel",
            ]),
            Op::Collect { run_id } => push(&["collect", "--control", &control, "--run-id", run_id]),
            Op::Cleanup { run_id } => push(&[
                "cleanup",
                "--control",
                &control,
                "--run-id",
                run_id,
                "--idempotency-key",
                "parity-cleanup",
            ]),
        }
        argv
    }

    /// `tools/call` over the MCP socket, with the token. The tool name is
    /// the envelope's `op`; its arguments are the rest of the envelope.
    async fn mcp(&self, op: &Op) -> Outcome {
        let mut arguments = op.envelope();
        let name = arguments
            .as_object_mut()
            .expect("envelope object")
            .remove("op")
            .expect("envelope op");
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        })
        .to_string();
        let mut stream = tokio::net::UnixStream::connect(&self.mcp)
            .await
            .expect("connect MCP socket");
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
             x-leyline-token: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.token,
            body.len()
        );
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write MCP request");
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .expect("read MCP response");
        let response = String::from_utf8(response).expect("UTF-8 response");
        let (head, body) = response
            .split_once("\r\n\r\n")
            .expect("an HTTP response has a head and a body");
        assert!(head.starts_with("HTTP/1.1 200"), "MCP status: {head}");
        let reply: Value = serde_json::from_str(body).expect("JSON-RPC reply");
        let result = &reply["result"];
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("tools/call result without text content: {reply}"));
        let outcome = outcome(serde_json::from_str(text).expect("tool text is JSON"));
        assert_eq!(
            result["isError"],
            Value::Bool(matches!(outcome, Outcome::Err(_))),
            "MCP isError must say whether the operation failed: {reply}"
        );
        outcome
    }
}

fn write(dir: &Path, name: &str, value: &Value) -> String {
    let path = dir.join(name);
    std::fs::write(&path, value.to_string()).expect("CLI input file");
    path.display().to_string()
}

/// Remove the wall-clock fields, after checking each is present and
/// well-formed: they are the only fields two runs of one request may differ
/// in.
fn without_clock(outcome: &Outcome) -> Outcome {
    let Outcome::Ok(value) = outcome else {
        return outcome.clone();
    };
    let mut value = value.clone();
    if let Some(events) = value.get_mut("events").and_then(Value::as_array_mut) {
        for event in events {
            let event = event.as_object_mut().expect("event object");
            assert!(
                event
                    .remove("timestampMs")
                    .is_some_and(|t| t.is_string() || t.is_u64()),
                "an event without a timestamp"
            );
        }
    }
    if let Some(receipt) = value.get_mut("receipt").and_then(Value::as_object_mut) {
        for field in ["startedAtUnixMs", "completedAtUnixMs"] {
            assert!(receipt.remove(field).is_some(), "receipt without {field}");
        }
        let root = receipt
            .remove("eventLogRoot")
            .expect("receipt without eventLogRoot");
        assert_eq!(root["algorithm"], "blake3-256");
        assert!(
            root["value"]
                .as_str()
                .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())),
            "eventLogRoot is a BLAKE3 digest: {root}"
        );
        if let Some(usage) = receipt.get_mut("usage").and_then(Value::as_object_mut) {
            usage.remove("wallTimeMs");
        }
    }
    Outcome::Ok(value)
}

fn ok(outcome: &Outcome) -> &Value {
    match outcome {
        Outcome::Ok(value) => value,
        Outcome::Err(error) => panic!("expected success, got {error}"),
    }
}

/// The lifecycle one surface drives: discover, provision, start, read, cancel,
/// collect, clean up — the order a caller follows.
async fn drive(surfaces: &Surfaces, surface: Surface) -> Vec<(String, Outcome)> {
    let spec = spec_bytes();
    let grant = grant_bytes(&spec);
    let mut transcript = Vec::new();
    let mut step = |label: &str, outcome: Outcome| {
        transcript.push((label.to_owned(), outcome.clone()));
        outcome
    };
    step(
        "capabilities",
        surfaces.call(surface, &Op::Capabilities).await,
    );
    step(
        "status before start",
        surfaces.call(surface, &Op::Status { run_id: None }).await,
    );
    step(
        "provision",
        surfaces
            .call(
                surface,
                &Op::Provision {
                    class: "microVm",
                    key: "parity-provision",
                },
            )
            .await,
    );
    let start = Op::Start {
        spec: spec_json(&spec),
        grant: grant_json(&grant),
    };
    let started = step("start", surfaces.call(surface, &start).await);
    let run_id = ok(&started)["runId"]
        .as_str()
        .expect("start names its run")
        .to_owned();
    let id = || run_id.clone();
    step(
        "status",
        surfaces
            .call(surface, &Op::Status { run_id: Some(id()) })
            .await,
    );
    step(
        "collect while running",
        surfaces.call(surface, &Op::Collect { run_id: id() }).await,
    );
    step(
        "inspect",
        surfaces.call(surface, &Op::Inspect { run_id: id() }).await,
    );
    step(
        "cancel",
        surfaces.call(surface, &Op::Cancel { run_id: id() }).await,
    );
    step(
        "collect",
        surfaces.call(surface, &Op::Collect { run_id: id() }).await,
    );
    step(
        "cleanup",
        surfaces.call(surface, &Op::Cleanup { run_id: id() }).await,
    );
    step("replayed start", surfaces.call(surface, &start).await);
    step(
        "status after cleanup",
        surfaces
            .call(surface, &Op::Status { run_id: Some(id()) })
            .await,
    );
    transcript
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_request_through_every_surface_yields_the_same_lifecycle_and_receipt() {
    let mut transcripts = Vec::new();
    for surface in SURFACES {
        let surfaces = Surfaces::new();
        transcripts.push((surface, drive(&surfaces, surface).await));
    }
    let (_, rust) = &transcripts[0];
    for (label, outcome) in rust {
        match label.as_str() {
            "collect while running" => assert!(
                matches!(outcome, Outcome::Err(error) if error.contains("not terminal")),
                "{label}: {outcome:?}"
            ),
            "collect" => {
                let receipt = &ok(outcome)["receipt"];
                assert_eq!(receipt["terminalState"], "cancelled", "{receipt}");
                assert_eq!(receipt["backend"]["backendId"], "transport-test/1");
            }
            "status after cleanup" => assert_eq!(ok(outcome)["state"], "cleaned"),
            _ => {
                ok(outcome);
            }
        }
    }
    for (surface, transcript) in &transcripts[1..] {
        assert_eq!(transcript.len(), rust.len());
        for ((label, theirs), (_, ours)) in transcript.iter().zip(rust) {
            assert_eq!(
                without_clock(theirs),
                without_clock(ours),
                "{surface:?} disagrees with Rust at {label:?}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_run_reads_identically_through_every_surface() {
    let surfaces = Surfaces::new();
    let spec = spec_bytes();
    let grant = grant_bytes(&spec);

    // Refusals before provisioning are the same everywhere.
    let start = Op::Start {
        spec: spec_json(&spec),
        grant: grant_json(&grant),
    };
    let wrong_tier = Op::Provision {
        class: "native",
        key: "parity-provision",
    };
    for op in [&start, &wrong_tier] {
        assert_identical(&surfaces, op, |outcome| {
            assert!(matches!(outcome, Outcome::Err(_)), "{op:?}: {outcome:?}");
        })
        .await;
    }
    assert_eq!(
        ok(&surfaces
            .call(
                Surface::Rust,
                &Op::Provision {
                    class: "microVm",
                    key: "parity-provision",
                },
            )
            .await)["provisioned"],
        true
    );

    // A grant for the other tier is refused identically.
    let other = Op::Start {
        spec: spec_json(&spec),
        grant: grant_json(&grant_bytes_for(&spec, BackendClass::Native)),
    };
    assert_identical(&surfaces, &other, |outcome| {
        assert!(
            matches!(outcome, Outcome::Err(error) if error.contains("UnsupportedBackend")),
            "{outcome:?}"
        );
    })
    .await;

    // Started once, through the daemon. Every surface's start of the same
    // authority then names that same run: replay, not a second run.
    let started = ok(&surfaces.call(Surface::Daemon, &start).await).clone();
    let run_id = started["runId"].as_str().expect("run id").to_owned();
    assert_identical(&surfaces, &start, |outcome| {
        assert_eq!(ok(outcome)["runId"], run_id);
    })
    .await;

    let unknown = Op::Status {
        run_id: Some("run-that-was-never-started".into()),
    };
    let status = Op::Status {
        run_id: Some(run_id.clone()),
    };
    let inspect = Op::Inspect {
        run_id: run_id.clone(),
    };
    let collect = Op::Collect {
        run_id: run_id.clone(),
    };
    assert_identical(&surfaces, &unknown, |_| {}).await;
    assert_identical(&surfaces, &status, |outcome| {
        assert_eq!(ok(outcome)["state"], "running");
    })
    .await;
    assert_identical(&surfaces, &collect, |outcome| {
        assert!(matches!(outcome, Outcome::Err(error) if error.contains("not terminal")));
    })
    .await;

    // Cancelled through MCP; read through everything.
    assert_eq!(
        ok(&surfaces
            .call(
                Surface::Mcp,
                &Op::Cancel {
                    run_id: run_id.clone()
                }
            )
            .await)["state"],
        "cancelled"
    );
    assert_identical(&surfaces, &inspect, |outcome| {
        let states: Vec<&str> = ok(outcome)["events"]
            .as_array()
            .expect("events")
            .iter()
            .map(|event| event["state"].as_str().expect("state"))
            .collect();
        assert_eq!(
            states,
            ["accepted", "provisioning", "ready", "running", "cancelled"]
        );
    })
    .await;
    let receipt = assert_identical(&surfaces, &collect, |_| {}).await;

    // The JSON receipt every transport returns is the service's own.
    let data = surfaces.service.collect(&run_id).expect("Rust receipt");
    let receipt = &ok(&receipt)["receipt"];
    assert_eq!(data.terminal_state, RunState::Cancelled);
    assert_eq!(receipt["runId"], data.run_id);
    assert_eq!(receipt["terminalState"], "cancelled");
    let digest = |value: &Value| {
        format!(
            "{}:{}",
            value["algorithm"].as_str().expect("algorithm"),
            value["value"].as_str().expect("value")
        )
    };
    assert_eq!(digest(&receipt["eventLogRoot"]), data.event_log_root);
    assert_eq!(
        digest(&receipt["runSpecDigest"]),
        data.context.run_spec_digest
    );
    assert_eq!(
        digest(&receipt["runGrantDigest"]),
        data.context.run_grant_digest
    );
    assert_eq!(
        digest(&receipt["confinementDigest"]),
        data.context.confinement_digest
    );
    assert_eq!(receipt["backend"]["backendId"], data.backend_id);
    assert_eq!(
        digest(&receipt["backend"]["evidence"]["digest"]),
        data.backend_evidence.digest()
    );
    assert_eq!(
        receipt["startedAtUnixMs"].to_string().trim_matches('"'),
        data.started_at_unix_ms.to_string()
    );
    assert_eq!(
        receipt["completedAtUnixMs"].to_string().trim_matches('"'),
        data.completed_at_unix_ms.to_string()
    );
}

/// Send `op` through every surface; every answer must be identical, with no
/// normalization. `check` judges the (shared) answer, which is returned.
async fn assert_identical(surfaces: &Surfaces, op: &Op, check: impl Fn(&Outcome)) -> Outcome {
    let rust = surfaces.call(Surface::Rust, op).await;
    check(&rust);
    for surface in &SURFACES[1..] {
        assert_eq!(
            surfaces.call(*surface, op).await,
            rust,
            "{surface:?} disagrees with Rust for {op:?}"
        );
    }
    rust
}
