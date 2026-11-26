//! The broker in a run's lifecycle, through `ExecutionService` (ADR-0046
//! Slice 1, S7, S8; bead `ley-line-open-5308b7`).
//!
//! - a cancel during a stalled ready wait, and a failure at step 5, each
//!   leave no broker process and no endpoint;
//! - `SIGKILL` of the supervising process ends the broker within a bound;
//! - a broker killed mid-run fails the run, cancels its workload, and the
//!   receipt carries `complete: false`;
//! - a connection open across the workload's exit is in the receipt with
//!   `complete: true`;
//! - the daemon's capability entry and the receipt's JSON carry what the
//!   service holds.
//!
//! Every run but the embedder's is authorized from a real RunGrant carrying
//! `connectionsDigest @16`, `connectionsDocument @17` and the marker, so the
//! service path is the one under test; `broker_cross_document` drives the
//! same layer through `Backend::start`.
//!
//! Synchronization is by event: a FIFO a stalled broker writes, a process's
//! exit as a kernel event, a line an embedding process prints, bytes read
//! back from the echo. Timeouts only bound a failure.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use leyline_runtime::authorization::{
    AuthorizationPolicy, MetadataOnlyEvidenceVerifier, authorize_with_verifier,
};
use leyline_runtime::backends::native_backend::NativeWorkerBackend;
use leyline_runtime::confinement::ConfinementManifest;
use leyline_runtime::transport::{capabilities_json, collect_json};
use leyline_runtime::{
    BackendClass, BackendRunStatus, BrokerConfig, ErrorCode, ExecutionError, ExecutionService,
    ResourceLimits, RunRecord, RunState, VerifiedConnections,
};

mod broker_fixture;
mod common;
mod conformance;

use broker_fixture::{
    BOUND, Host, channel_dir, connections, echo, endpoint, fifo, manifest, native_grant, request,
};
use conformance::{GrantSpec, GuestResolver, Observed, Waitable, alive, spec_and_grant};

impl Waitable for NativeWorkerBackend {
    fn wait(&self, run_id: &str) -> Result<BackendRunStatus, ExecutionError> {
        self.wait_for_completion(run_id)
    }
}

/// A worker that runs until its supervisor cancels it or is gone: its
/// standard input is its lifeline.
const UNTIL_CANCELLED: &str = "exec cat > /dev/null";

/// One provisioned service with a broker, and one declared connection `api`
/// with its matching §6 grant.
struct Run {
    service: ExecutionService<Observed>,
    backend: Observed,
    declared: VerifiedConnections,
    granted: ConfinementManifest,
}

/// A spec and a grant carrying the connections, and the run they derive.
struct Authority {
    spec: Vec<u8>,
    grant: Vec<u8>,
    policy: AuthorizationPolicy,
    run_id: String,
}

impl Run {
    fn new(backend: NativeWorkerBackend, broker: BrokerConfig, dir: &Path, port: u16) -> Self {
        let backend = Observed::new(backend);
        let service = ExecutionService::new(backend.clone()).with_connection_broker(broker);
        service
            .provision(BackendClass::Native, "provision-broker")
            .expect("provision");
        Self {
            service,
            backend,
            declared: connections(dir, &["api"], "127.0.0.1", port),
            granted: manifest(vec![native_grant(dir, "api")]),
        }
    }

    fn authority(&self, replay_key: &str) -> Authority {
        let document = self.granted.to_canonical_json().expect("manifest");
        let digest = self.granted.confinement_digest().expect("digest");
        let declared =
            String::from_utf8(self.declared.document().canonical_bytes()).expect("ASCII");
        let mut authority = GrantSpec::new(BackendClass::Native, replay_key, &digest, &document);
        authority.connections = Some(&declared);
        let (spec, grant) = spec_and_grant(&authority);
        let policy = AuthorizationPolicy {
            now_unix_ms: None,
            required_backend: BackendClass::Native,
            required_confinement_digest: Some(digest),
        };
        let run_id = authorize_with_verifier(&spec, &grant, &policy, &MetadataOnlyEvidenceVerifier)
            .expect("the grant authorizes")
            .run_id;
        Authority {
            spec,
            grant,
            policy,
            run_id,
        }
    }

    fn start(&self, authority: &Authority) -> Result<RunRecord, ExecutionError> {
        self.service.start_authorized(
            &authority.spec,
            &authority.grant,
            &authority.policy,
            &GuestResolver {
                rootfs: "a".repeat(64),
                executable: "bin/workload".into(),
                arguments: Vec::new(),
                environment: Default::default(),
                limits: ResourceLimits {
                    wall_time_ms: 120_000,
                    ..ResourceLimits::default()
                },
            },
        )
    }

    /// The run's receipt, as the daemon's `collect` writes it.
    fn receipt(&self, run_id: &str) -> serde_json::Value {
        let text = collect_json(
            &self.service,
            &serde_json::json!({ "runId": run_id }).to_string(),
        )
        .expect("collect");
        serde_json::from_str::<serde_json::Value>(&text).expect("receipt JSON")["receipt"].clone()
    }
}

fn gone(pid: libc::pid_t, what: &str) {
    common::await_exit(pid, BOUND).unwrap_or_else(|error| panic!("{what} is still alive: {error}"));
}

/// Dial the endpoint as the workload would and round-trip `payload`.
fn round_trip(endpoint: &str, payload: &[u8]) -> UnixStream {
    let mut stream = UnixStream::connect(endpoint).expect("dial the broker's endpoint");
    stream.set_read_timeout(Some(BOUND)).expect("read bound");
    stream.write_all(payload).expect("write");
    let mut echoed = vec![0u8; payload.len()];
    stream.read_exact(&mut echoed).expect("the echo");
    assert_eq!(echoed, payload);
    stream
}

#[test]
fn cancel_during_a_stalled_ready_wait_leaves_no_broker_and_no_endpoint() {
    let _serial = common::serial();
    let _watchdog = broker_fixture::watchdog(
        "cancel_during_a_stalled_ready_wait_leaves_no_broker_and_no_endpoint",
    );
    let host = Host::new();
    let (_keep, dir) = channel_dir();
    let stalled = host.path("stalled");
    fifo(&stalled);
    let run = Run::new(
        host.backend(UNTIL_CANCELLED),
        host.broker(Some(&stalled)),
        &dir,
        echo(),
    );
    let authority = run.authority("stalled");
    let socket = endpoint(&dir, "api");
    std::thread::scope(|scope| {
        let starting = scope.spawn(|| run.start(&authority));
        // EVENT: the broker has bound its endpoint and stalled before ready.
        let mut line = String::new();
        BufReader::new(std::fs::File::open(&stalled).expect("open the FIFO"))
            .read_line(&mut line)
            .expect("the broker's stall report");
        let broker: libc::pid_t = line.trim().parse().expect("broker pid");
        assert!(alive(broker), "the broker is waiting to report ready");
        assert!(Path::new(&socket).exists(), "it bound the endpoint first");
        assert_eq!(
            run.service.status(&authority.run_id).expect("status"),
            None,
            "a starting run is not yet a run"
        );

        let record = run.service.cancel(&authority.run_id).expect("cancel");
        assert_eq!(record.run_id, authority.run_id);
        assert_eq!(record.state, RunState::Cancelled);
        // Before the start thread is joined: the cancel itself waited.
        assert!(!alive(broker), "the cancel returned with the broker reaped");
        assert!(!Path::new(&socket).exists(), "no endpoint is left");

        let error = starting
            .join()
            .expect("start thread")
            .expect_err("cancelled");
        assert_eq!(error.code, ErrorCode::Cancelled, "{error}");
        // The answer is the cancel, and it keeps what the wait saw.
        assert!(
            error.detail.contains("cancelled while it was starting")
                && error.detail.contains("exited before it reported ready"),
            "{error}"
        );
    });
    assert_eq!(host.worker_pid(), None, "no worker was ever spawned");
    assert_eq!(
        run.service.status(&authority.run_id).expect("status"),
        None,
        "a cancelled start is not recorded"
    );
    // Nothing of the run is left in the registry: the same endpoint and
    // grant start again, on a broker that reports ready.
    let again = Run::new(
        host.backend(UNTIL_CANCELLED),
        host.broker(None),
        &dir,
        echo(),
    );
    let authority = again.authority("after-the-cancel");
    let record = again.start(&authority).expect("the endpoint is free");
    again.service.cancel(&record.run_id).expect("cancel");
}

#[test]
fn a_failure_at_step_five_leaves_no_broker_and_no_endpoint() {
    let _serial = common::serial();
    let host = Host::new();
    let (_keep, dir) = channel_dir();
    let run = Run::new(host.failing_backend(), host.broker(None), &dir, echo());
    let authority = run.authority("step-five");
    let error = run.start(&authority).expect_err("the worker never readies");
    assert_eq!(error.code, ErrorCode::BackendFailed, "{error}");
    assert!(error.detail.contains("native worker"), "{error}");
    // The broker had reported ready (the worker is spawned only after), and
    // is reaped before the start returns.
    let broker = host.broker_pid().expect("a broker was spawned");
    assert!(!alive(broker), "the start returned with the broker reaped");
    assert!(!Path::new(&endpoint(&dir, "api")).exists());
    assert_eq!(run.service.status(&authority.run_id).expect("status"), None);

    // The registry entry went with it: the same endpoint starts again.
    let again = Run::new(
        host.backend(UNTIL_CANCELLED),
        host.broker(None),
        &dir,
        echo(),
    );
    let record = again
        .start(&again.authority("after-the-failure"))
        .expect("the endpoint is free");
    again.service.cancel(&record.run_id).expect("cancel");
}

#[test]
fn a_broker_killed_mid_run_fails_the_run_with_an_incomplete_receipt() {
    let _serial = common::serial();
    let host = Host::new();
    let (_keep, dir) = channel_dir();
    let run = Run::new(
        host.backend(UNTIL_CANCELLED),
        host.broker(None),
        &dir,
        echo(),
    );
    let record = run.start(&run.authority("killed")).expect("running");
    assert_eq!(record.state, RunState::Running);
    let socket = endpoint(&dir, "api");
    // One connection, ended, so its record reached the supervisor before
    // the kill; the receipt must keep it.
    drop(round_trip(&socket, b"before the kill"));
    let broker = host.broker_pid().expect("broker pid");
    let worker = host.worker_pid().expect("worker pid");

    // SAFETY: a signal to the broker this test's service spawned.
    assert_eq!(unsafe { libc::kill(broker, libc::SIGKILL) }, 0);
    // EVENT: the workload is cancelled because the broker ended, with no
    // poll from this test (D7 rule 5).
    gone(worker, "the workload of a run whose broker was killed");
    gone(broker, "the killed broker");

    let status = run
        .service
        .status(&record.run_id)
        .expect("status")
        .expect("recorded");
    assert_eq!(status.state, RunState::Failed);
    assert!(!Path::new(&socket).exists(), "the endpoint is unlinked");
    let receipt = run.receipt(&record.run_id);
    assert_eq!(receipt["terminalState"], "failed");
    assert_eq!(
        receipt["connectionsDigest"]["value"],
        run.declared.document().digest_hex()
    );
    let connection = &receipt["connections"][0];
    assert_eq!(connection["name"], "api");
    assert_eq!(connection["complete"], false, "{receipt}");
    // Lower bounds: at most the one connection made before the kill.
    assert!(
        connection["accepted"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            <= 1,
        "{receipt}"
    );
    run.service.cleanup(&record.run_id).expect("cleanup");
}

#[test]
fn a_connection_open_across_workload_exit_is_in_the_receipt_complete() {
    let _serial = common::serial();
    let host = Host::new();
    let (_keep, dir) = channel_dir();
    let exit = host.path("exit");
    fifo(&exit);
    // The workload ends when the test opens and closes the FIFO.
    let run = Run::new(
        host.backend(&format!("cat '{}' > /dev/null\nexit 0", exit.display())),
        host.broker(None),
        &dir,
        echo(),
    );
    let capabilities: serde_json::Value =
        serde_json::from_str(&capabilities_json(&run.service).expect("capabilities")).unwrap();
    assert!(
        capabilities["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "cloister/execution/v1/connections"
                && entry["version"] == "1"),
        "a service with a broker advertises connections: {capabilities}"
    );

    let record = run
        .start(&run.authority("open-across-exit"))
        .expect("running");
    let socket = endpoint(&dir, "api");
    drop(round_trip(&socket, b"closed before exit"));
    // Held open for the rest of the run, and past the workload's exit.
    let held = round_trip(&socket, b"open across exit");
    let broker = host.broker_pid().expect("broker pid");

    drop(
        std::fs::OpenOptions::new()
            .write(true)
            .open(&exit)
            .expect("release the workload"),
    );
    // EVENT: the worker has exited and its completion is published.
    assert_eq!(
        run.backend.settle(&record.run_id),
        BackendRunStatus::Succeeded
    );
    let status = run.service.status(&record.run_id).expect("status").unwrap();
    assert_eq!(status.state, RunState::Succeeded);
    // The terminal transition drained the broker and tore it down.
    assert!(!alive(broker), "the broker is reaped at the terminal state");
    assert!(!Path::new(&socket).exists(), "the endpoint is unlinked");

    let receipt = run.receipt(&record.run_id);
    assert_eq!(receipt["terminalState"], "succeeded");
    assert_eq!(receipt["connectionsDigest"]["algorithm"], "blake3-256");
    assert_eq!(
        receipt["connectionsDigest"]["value"],
        run.declared.document().digest_hex()
    );
    let recorded = receipt["connections"].as_array().expect("connections");
    assert_eq!(recorded.len(), 1, "{receipt}");
    let connection = &recorded[0];
    assert_eq!(connection["name"], "api");
    assert_eq!(
        connection["target"],
        run.declared.document().connections()[0].target_text()
    );
    assert_eq!(connection["complete"], true, "{receipt}");
    // UInt64 is a decimal string in the JSON carrier.
    assert_eq!(connection["accepted"], "2", "{receipt}");
    assert_eq!(connection["dialed"], "2");
    assert!(
        connection.get("refused").is_none_or(|v| v == "0"),
        "{receipt}"
    );
    let sent = (b"closed before exit".len() + b"open across exit".len()) as u64;
    for key in ["bytesToTarget", "bytesFromTarget"] {
        assert_eq!(
            connection[key].as_str().unwrap().parse::<u64>().unwrap(),
            sent,
            "{key}: {receipt}"
        );
    }
    drop(held);

    // A cancelled run's receipt is assembled the same way.
    let (_keep, dir) = channel_dir();
    let run = Run::new(
        host.backend(UNTIL_CANCELLED),
        host.broker(None),
        &dir,
        echo(),
    );
    let record = run.start(&run.authority("cancelled")).expect("running");
    let held = round_trip(&endpoint(&dir, "api"), b"open at the cancel");
    let cancelled = run.service.cancel(&record.run_id).expect("cancel");
    assert_eq!(cancelled.state, RunState::Cancelled);
    assert!(!Path::new(&endpoint(&dir, "api")).exists());
    let receipt = run.receipt(&record.run_id);
    assert_eq!(receipt["terminalState"], "cancelled");
    assert_eq!(receipt["connections"][0]["accepted"], "1", "{receipt}");
    assert_eq!(receipt["connections"][0]["complete"], true, "{receipt}");
    drop(held);
}

/// A run without connections on a service with a broker: its receipt
/// carries neither new field, and a service without a broker advertises no
/// connections capability.
#[test]
fn a_run_without_connections_has_neither_receipt_field() {
    let _serial = common::serial();
    let host = Host::new();
    let (_keep, dir) = channel_dir();
    let run = Run::new(host.backend("exit 0"), host.broker(None), &dir, echo());
    let document = ConfinementManifest::new().to_canonical_json().unwrap();
    let digest = ConfinementManifest::new().confinement_digest().unwrap();
    let (spec, grant) = spec_and_grant(&GrantSpec::new(
        BackendClass::Native,
        "no-connections",
        &digest,
        &document,
    ));
    let authority = Authority {
        policy: AuthorizationPolicy {
            now_unix_ms: None,
            required_backend: BackendClass::Native,
            required_confinement_digest: Some(digest),
        },
        run_id: String::new(),
        spec,
        grant,
    };
    let record = run.start(&authority).expect("running");
    assert_eq!(
        run.backend.settle(&record.run_id),
        BackendRunStatus::Succeeded
    );
    run.service.status(&record.run_id).expect("status");
    assert_eq!(
        host.broker_pid(),
        None,
        "no broker for a run without connections"
    );
    let receipt = run.receipt(&record.run_id);
    for key in ["connectionsDigest", "connections"] {
        assert!(receipt.get(key).is_none(), "no {key}: {receipt}");
    }

    let bare = ExecutionService::new(Observed::new(host.backend("exit 0")));
    let capabilities = capabilities_json(&bare).expect("capabilities");
    assert!(
        !capabilities.contains("cloister/execution/v1/connections"),
        "a service without a broker must not advertise one: {capabilities}"
    );
}

// ---------------------------------------------------------------------------
// SIGKILL of the supervising process.

/// Set in the process [`broker_lifeline_embedder`] runs as: the host
/// directory, the channel directory and the echo's port, `:`-separated.
const EMBEDDER_ENV: &str = "LEYLINE_TEST_BROKER_LIFELINE_EMBEDDER";

/// The process embedding the service for
/// `the_broker_ends_when_the_process_supervising_it_is_killed`: starts one
/// run with a connection, says so, and supervises it until it is killed. Not
/// a test on its own.
#[test]
#[ignore = "the embedding process of a lifeline test; run by that test"]
fn broker_lifeline_embedder() {
    let Some(fixture) = std::env::var_os(EMBEDDER_ENV) else {
        return;
    };
    let fixture = fixture.into_string().expect("UTF-8");
    let mut parts = fixture.split(':');
    let host = Host {
        dir: tempfile::TempDir::with_prefix_in("embedder", parts.next().unwrap()).unwrap(),
    };
    let dir = PathBuf::from(parts.next().unwrap());
    let port: u16 = parts.next().unwrap().parse().unwrap();
    std::fs::create_dir(host.path("cas")).unwrap();
    std::fs::create_dir(host.path("runs")).unwrap();
    let service = ExecutionService::new(host.backend(UNTIL_CANCELLED))
        .with_connection_broker(host.broker(None));
    let granted = manifest(vec![native_grant(&dir, "api")]);
    service
        .start_with_connections(
            request("run-embedded", Some(&granted)),
            connections(&dir, &["api"], "127.0.0.1", port),
        )
        .expect("the embedded run starts");
    println!(
        "EMBEDDER-STARTED {} {}",
        host.broker_pid().expect("broker pid"),
        host.worker_pid().expect("worker pid")
    );
    loop {
        std::thread::park();
    }
}

struct KillOnDrop(libc::pid_t);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        // SAFETY: a signal to a process this test started.
        unsafe { libc::kill(self.0, libc::SIGKILL) };
    }
}

#[test]
fn the_broker_ends_when_the_process_supervising_it_is_killed() {
    let _serial = common::serial();
    let scratch = tempfile::TempDir::new().expect("scratch");
    let (_keep, dir) = channel_dir();
    let mut embedder = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--ignored",
            "--exact",
            "broker_lifeline_embedder",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(
            EMBEDDER_ENV,
            format!("{}:{}:{}", scratch.path().display(), dir.display(), echo()),
        )
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the embedding process");
    let _embedder_cleanup = KillOnDrop(embedder.id() as libc::pid_t);

    // EVENT: `start` has returned in the embedder, with a live broker.
    let (sender, started) = std::sync::mpsc::channel();
    let stdout = embedder.stdout.take().expect("piped stdout");
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            // libtest prints its own "test <name> ... " before the line.
            if let Some((_, pids)) = line.split_once("EMBEDDER-STARTED ") {
                let _ = sender.send(pids.to_owned());
                return;
            }
        }
    });
    let pids = started
        .recv_timeout(BOUND)
        .expect("TIMEOUT: the embedder's start never returned");
    let mut pids = pids
        .split(' ')
        .map(|pid| pid.parse::<libc::pid_t>().unwrap());
    let (broker, worker) = (pids.next().unwrap(), pids.next().unwrap());
    let _broker_cleanup = KillOnDrop(broker);
    let _worker_cleanup = KillOnDrop(worker);
    let socket = endpoint(&dir, "api");
    // The positive control: the broker relays while its supervisor lives.
    drop(round_trip(&socket, b"alive"));

    // SAFETY: a signal to the embedder this test spawned.
    assert_eq!(
        unsafe { libc::kill(embedder.id() as libc::pid_t, libc::SIGKILL) },
        0
    );
    embedder.wait().expect("reap the embedder");
    // EVENT: the broker's exit, bounded.
    gone(broker, "the broker of a supervisor that was killed");
    gone(worker, "the worker of a supervisor that was killed");
}

// ---------------------------------------------------------------------------
// The lifeline under concurrent starts (ADR-0046 S7, S11; review C5).

/// How many runs the concurrent embedder starts at once.
const CONCURRENT: usize = 8;

/// Set in the process [`broker_concurrent_embedder`] runs as: the host
/// directory and the echo's port, `:`-separated.
const CONCURRENT_ENV: &str = "LEYLINE_TEST_BROKER_CONCURRENT_EMBEDDER";

/// The embedding process of
/// `under_concurrent_starts_every_broker_still_ends_with_its_supervisor`:
/// starts [`CONCURRENT`] runs with connections at the same moment, each with
/// its own channel directory, says so, and supervises them until it is
/// killed. The workers IGNORE their own lifeline (they `exec sleep`), so one
/// that inherited a broker's lifeline write end would hold it open after
/// this process is gone. Not a test on its own.
#[test]
#[ignore = "the embedding process of a lifeline test; run by that test"]
fn broker_concurrent_embedder() {
    let Some(fixture) = std::env::var_os(CONCURRENT_ENV) else {
        return;
    };
    let fixture = fixture.into_string().expect("UTF-8");
    let (root, port) = fixture.split_once(':').expect("root:port");
    let port: u16 = port.parse().expect("port");
    let host = Host {
        dir: tempfile::TempDir::with_prefix_in("embedder", root).unwrap(),
    };
    std::fs::create_dir(host.path("cas")).unwrap();
    std::fs::create_dir(host.path("runs")).unwrap();
    let service = ExecutionService::new(host.backend("exec sleep 600"))
        .with_connection_broker(host.broker(None));
    let channels: Vec<_> = (0..CONCURRENT).map(|_| channel_dir()).collect();
    let barrier = std::sync::Barrier::new(CONCURRENT);
    std::thread::scope(|scope| {
        for (index, (_, dir)) in channels.iter().enumerate() {
            let (service, barrier) = (&service, &barrier);
            scope.spawn(move || {
                let granted = manifest(vec![native_grant(dir, "api")]);
                // Every start leaves the barrier together, so brokers and
                // workers are spawned on overlapping threads.
                barrier.wait();
                service
                    .start_with_connections(
                        request(&format!("run-{index}"), Some(&granted)),
                        connections(dir, &["api"], "127.0.0.1", port),
                    )
                    .expect("a concurrent run starts");
            });
        }
    });
    println!("EMBEDDER-STARTED {}", host.dir.path().display());
    loop {
        std::thread::park();
    }
}

/// Every pipe end `pid` holds, as the kernel names it: on macOS `lsof`'s
/// address of the end itself; on Linux the pipe's inode, which both ends
/// share.
fn pipe_ends(pid: libc::pid_t) -> Vec<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_dir(format!("/proc/{pid}/fd"))
            .unwrap_or_else(|error| panic!("list the descriptors of {pid}: {error}"))
            .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
            .map(|link| link.display().to_string())
            .filter(|link| link.starts_with("pipe:"))
            .collect()
    }
    #[cfg(not(target_os = "linux"))]
    {
        lsof_pipes(pid).into_iter().map(|(_, end, _)| end).collect()
    }
}

/// `lsof`'s PIPE rows for `pid`: the descriptor, this end's address and the
/// other end's.
#[cfg(not(target_os = "linux"))]
fn lsof_pipes(pid: libc::pid_t) -> Vec<(String, String, String)> {
    let output = std::process::Command::new("/usr/sbin/lsof")
        .args(["-n", "-P", "-p", &pid.to_string()])
        .output()
        .expect("run lsof");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let columns: Vec<&str> = line.split_whitespace().collect();
            // COMMAND PID USER FD TYPE DEVICE SIZE/OFF NAME, NAME `->0x…`.
            (columns.get(4) == Some(&"PIPE")).then(|| {
                (
                    columns[3].trim_end_matches(char::is_alphabetic).to_owned(),
                    columns[5].to_owned(),
                    columns.last().unwrap().trim_start_matches("->").to_owned(),
                )
            })
        })
        .collect()
}

/// What names the WRITE end of `broker`'s lifeline (its descriptor 3 is the
/// read end): on macOS that end's own address, on Linux the pipe's inode.
fn lifeline_write_end(broker: libc::pid_t) -> String {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{broker}/fd/3"))
            .unwrap_or_else(|error| panic!("the lifeline of broker {broker}: {error}"))
            .display()
            .to_string()
    }
    #[cfg(not(target_os = "linux"))]
    {
        lsof_pipes(broker)
            .into_iter()
            .find(|(descriptor, _, _)| descriptor == "3")
            .map(|(_, _, other_end)| other_end)
            .unwrap_or_else(|| panic!("broker {broker} holds no pipe at descriptor 3"))
    }
}

/// N concurrent starts leave each broker's lifeline write end held by the
/// supervising process alone (ADR-0046 S7, S11). Asked of the kernel: no
/// worker and no other broker holds the write end of any broker's lifeline.
/// The workers ignore their own lifeline, so they are still there to be
/// asked, and to hold open whatever they inherited. Then the supervising
/// process is killed and every broker ends, with every worker still alive.
///
/// The second half alone would not prove the first: a broker also ends when
/// its control pipe closes, so a leaked lifeline would go unseen.
#[test]
fn under_concurrent_starts_every_broker_still_ends_with_its_supervisor() {
    let _serial = common::serial();
    let scratch = tempfile::TempDir::new().expect("scratch");
    let mut embedder = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--ignored",
            "--exact",
            "broker_concurrent_embedder",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(
            CONCURRENT_ENV,
            format!("{}:{}", scratch.path().display(), echo()),
        )
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the embedding process");
    let _embedder_cleanup = KillOnDrop(embedder.id() as libc::pid_t);

    // EVENT: every start has returned in the embedder.
    let (sender, started) = std::sync::mpsc::channel();
    let stdout = embedder.stdout.take().expect("piped stdout");
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some((_, directory)) = line.split_once("EMBEDDER-STARTED ") {
                let _ = sender.send(directory.to_owned());
                return;
            }
        }
    });
    let directory = PathBuf::from(
        started
            .recv_timeout(BOUND)
            .expect("TIMEOUT: the embedder's starts never returned"),
    );
    let pids = |kind: &str| -> Vec<libc::pid_t> {
        std::fs::read_to_string(directory.join(format!("{kind}.pids")))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect()
    };
    let (brokers, workers) = (pids("broker"), pids("worker"));
    let _cleanup: Vec<KillOnDrop> = brokers
        .iter()
        .chain(&workers)
        .map(|pid| KillOnDrop(*pid))
        .collect();
    assert_eq!(brokers.len(), CONCURRENT, "one broker per run");
    assert_eq!(workers.len(), CONCURRENT, "one worker per run");
    for pid in brokers.iter().chain(&workers) {
        assert!(alive(*pid), "{pid} is running under its supervisor");
    }
    // One listing per process, taken while all of them are alive.
    let held: Vec<(libc::pid_t, Vec<String>)> = workers
        .iter()
        .chain(&brokers)
        .map(|pid| (*pid, pipe_ends(*pid)))
        .collect();
    let supervisor = pipe_ends(embedder.id() as libc::pid_t);
    for broker in &brokers {
        let write_end = lifeline_write_end(*broker);
        for (other, ends) in held.iter().filter(|(other, _)| other != broker) {
            assert!(
                !ends.contains(&write_end),
                "process {other} inherited the write end ({write_end}) of broker {broker}'s \
                 lifeline; only the supervisor may hold it"
            );
        }
        // The control: the supervisor does hold it.
        assert!(
            supervisor.contains(&write_end),
            "the supervisor holds the write end of broker {broker}'s lifeline"
        );
    }

    // SAFETY: a signal to the embedder this test spawned.
    assert_eq!(
        unsafe { libc::kill(embedder.id() as libc::pid_t, libc::SIGKILL) },
        0
    );
    embedder.wait().expect("reap the embedder");
    // EVENT: each broker's exit, bounded.
    for broker in &brokers {
        gone(*broker, "a broker whose supervisor was killed");
    }
    // The workers outlived it all: none of them was what ended a broker, and
    // none of them held a broker's lifeline open.
    for worker in &workers {
        assert!(
            alive(*worker),
            "worker {worker} ignores its lifeline and must still be running"
        );
    }
}
