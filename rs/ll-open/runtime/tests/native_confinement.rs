//! Kernel-level proof that the native nono tier confines its workload
//! (beads `ley-line-open-704853`, `ley-line-open-f81567`).
//!
//! Every other native test checks the policy the runtime COMPILES. These run a
//! workload under the policy the kernel APPLIES: the real first-party
//! `leyline-native-worker` binary (the nono library linked into LLO, not an
//! independently installed nono CLI) verifies a content-addressed rootfs,
//! materializes it, applies Seatbelt (macOS) or Landlock (Linux)
//! irreversibly, and only then launches the guest. The guest is a copy of this
//! test binary, re-entered at [`confined_guest_entry_point`], and it reports
//! what each system call actually returned.
//!
//! Four claims, each checked against the errno the kernel returned:
//!
//! (a) An EXISTING file outside the grant is unreadable, with EPERM or EACCES
//!     and never ENOENT. The decoy is planted by the parent and read back
//!     unconfined first, so a denial cannot be an absent file in disguise.
//!     It lives under `CARGO_TARGET_TMPDIR`, not the system temp directory:
//!     cloister's harness learned that a decoy in a default-allowed temp
//!     directory proves nothing.
//! (b) Siblings of the granted root are denied: a directory beside the run
//!     root, a directory beside the rootfs whose name shares its prefix
//!     (`rootfs-sibling`, the string-prefix confusion a naive subpath rule
//!     would leak), and the run root itself, both listing and writing.
//! (c) With no network grant, an outbound connect to a loopback listener this
//!     test owns is refused at the kernel, and the listener never sees a
//!     connection.
//! (d) The positive control: the granted rootfs stays readable and writable.
//!     Without it, (a)-(c) would pass on a guest that could do nothing at all.
//!
//! Fail closed: when the host cannot apply the sandbox (an outer sandbox
//! already confines the test, or the kernel lacks the mechanism), the worker
//! reports `apply native nono confinement` and the test prints a named
//! `SKIPPED (NOT a pass)`. `LEYLINE_REQUIRE_SANDBOX=1` turns that into a
//! failure, the same contract `LEYLINE_REQUIRE_HYPERVISOR` gives the microVM
//! suites. Every other failure is a failure.
//!
//! Synchronization is by events only: the worker's readiness line, then one
//! line the parent writes to the guest's stdin, then process exit.
//!
//! Platforms: on macOS the guest runs with the rootfs as its only grant. On
//! Linux the dynamically linked guest also needs its loader and shared
//! libraries, granted read-only as `--runtime-file`s (see
//! [`guest_runtime_files`]); without them `execve` itself is EACCES and every
//! test fails rather than passing.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use leyline_runtime::backends::native::WorkerEvent;
use leyline_runtime::backends::native_backend::{NativeWorkerBackend, NativeWorkerConfig};
use leyline_runtime::{
    Backend, BackendRunStatus, DigestRef, ExecutionError, ExecutionRequest, ResourceLimits,
};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

mod common;

const WORKER: &str = env!("CARGO_BIN_EXE_leyline-native-worker");
const RUN_ID: &str = "native-confinement-01";
const GUEST_EXECUTABLE: &str = "bin/probe";
const GRANTED_FILE: &str = "granted.txt";
const GRANTED_CONTENT: &str = "granted-rootfs-content";
const SECRET: &str = "SIMULATED PRIVATE KEY - must never be readable confined";
const REPORT_PREFIX: &str = "LEYLINE-CONFINEMENT-PROBE ";
const ENTRY_POINT: &str = "confined_guest_entry_point";
/// The prefix-sharing sibling of the materialized rootfs, planted by the
/// parent between readiness and the go signal.
const PREFIX_SIBLING: &str = "rootfs-sibling";

/// Guest configuration travels in the request's public environment — the
/// worker clears everything else.
const ENV_PROBE: &str = "LEYLINE_CONFINEMENT_PROBE";
const ENV_DECOY: &str = "LEYLINE_PROBE_DECOY";
const ENV_RUN_SIBLING: &str = "LEYLINE_PROBE_RUN_SIBLING";
const ENV_CONNECT: &str = "LEYLINE_PROBE_CONNECT";
const ENV_SSH: &str = "LEYLINE_PROBE_SSH";
const ENV_WAIT: &str = "LEYLINE_PROBE_WAIT_FOR_GO";
const ENV_STRICT: &str = "LEYLINE_PROBE_STRICT";

// ---------------------------------------------------------------------------
// The shared contract: which probe must be allowed and which must be denied.
// Both sides use it — the parent to assert, and the guest in strict mode, where
// its exit status is the only thing the lifecycle backend carries back.

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Observation {
    probe: String,
    ok: bool,
    errno: Option<i32>,
    detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// Positive control: must succeed (and return the expected bytes).
    Allowed,
    /// Must fail with EPERM or EACCES — a kernel denial, never ENOENT.
    KernelDenied,
}

fn expectation(probe: &str) -> Expect {
    match probe {
        "granted_read" | "granted_write" => Expect::Allowed,
        _ => Expect::KernelDenied,
    }
}

fn violations(observations: &[Observation], required: &[&str]) -> Vec<String> {
    let mut problems = Vec::new();
    for name in required {
        if !observations.iter().any(|o| o.probe == *name) {
            problems.push(format!("probe {name} never reported"));
        }
    }
    for observation in observations {
        if observation.detail.contains(SECRET) {
            problems.push(format!(
                "probe {} leaked the decoy secret: {:?}",
                observation.probe, observation.detail
            ));
        }
        match expectation(&observation.probe) {
            Expect::Allowed if !observation.ok => problems.push(format!(
                "positive control {} failed, so the guest could not use its own grant: {observation:?}",
                observation.probe
            )),
            Expect::Allowed => {}
            Expect::KernelDenied if observation.ok => problems.push(format!(
                "{} SUCCEEDED under confinement — the sandbox did not deny it: {observation:?}",
                observation.probe
            )),
            Expect::KernelDenied => match observation.errno {
                Some(errno) if errno == libc::EPERM || errno == libc::EACCES => {}
                Some(errno) if errno == libc::ENOENT => problems.push(format!(
                    "{} failed with ENOENT: the target looked absent rather than denied, \
                     so this proves nothing about the sandbox: {observation:?}",
                    observation.probe
                )),
                _ => problems.push(format!(
                    "{} failed, but not with a kernel denial (EPERM/EACCES): {observation:?}",
                    observation.probe
                )),
            },
        }
    }
    problems
}

// ---------------------------------------------------------------------------
// Guest side.

/// Not a test: the confined guest's entry point. The parent copies this test
/// binary into the rootfs and launches it through the native worker with
/// `--ignored --exact confined_guest_entry_point`; run unconfined it refuses,
/// because its observations only mean something inside the sandbox.
#[test]
#[ignore = "guest-side entry point, launched confined by the native worker; not a harness test"]
fn confined_guest_entry_point() {
    assert!(
        std::env::var_os(ENV_PROBE).is_some(),
        "{ENTRY_POINT} is the confined guest's entry point; it is meaningless unconfined"
    );
    if std::env::var_os(ENV_WAIT).is_some() {
        // The go signal: the parent plants the prefix sibling after the
        // worker reports readiness, then writes one line here.
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).expect("go signal");
        assert_eq!(line.trim(), "go", "unexpected go signal");
    }

    let mut observations = Vec::new();
    let mut record = |probe: &str, result: std::io::Result<String>| {
        let observation = match result {
            Ok(detail) => Observation {
                probe: probe.into(),
                ok: true,
                errno: None,
                detail,
            },
            Err(error) => Observation {
                probe: probe.into(),
                ok: false,
                errno: error.raw_os_error(),
                detail: error.to_string(),
            },
        };
        println!(
            "{REPORT_PREFIX}{}",
            serde_json::to_string(&observation).expect("serialize observation")
        );
        observations.push(observation);
    };

    // (d) positive control. The cwd is the materialized rootfs.
    record(
        "granted_read",
        fs::read_to_string(GRANTED_FILE).and_then(|content| {
            if content == GRANTED_CONTENT {
                Ok(content)
            } else {
                Err(std::io::Error::other(format!("wrong content: {content:?}")))
            }
        }),
    );
    record(
        "granted_write",
        fs::write("guest-wrote.txt", b"written-confined")
            .and_then(|()| fs::read_to_string("guest-wrote.txt")),
    );

    // (a) an existing out-of-grant decoy.
    let decoy = std::env::var(ENV_DECOY).expect("decoy path");
    record("decoy_read", fs::read_to_string(&decoy));

    // (b) siblings of the granted root.
    let run_sibling = std::env::var(ENV_RUN_SIBLING).expect("run sibling path");
    record("run_root_sibling_read", fs::read_to_string(&run_sibling));
    record(
        "run_root_list",
        fs::read_dir("..").map(|entries| format!("{} entries", entries.count())),
    );
    record(
        "run_root_write",
        fs::write("../escaped.txt", b"escaped").map(|()| "wrote".into()),
    );
    if std::env::var_os(ENV_WAIT).is_some() {
        record(
            "rootfs_prefix_sibling_read",
            fs::read_to_string(Path::new("..").join(PREFIX_SIBLING).join("secret.txt")),
        );
    }
    if let Ok(ssh) = std::env::var(ENV_SSH) {
        record(
            "home_ssh_list",
            fs::read_dir(&ssh).map(|entries| format!("{} entries", entries.count())),
        );
    }

    // (c) no network grant: an outbound connect to the parent's listener.
    let address = std::env::var(ENV_CONNECT).expect("connect address");
    record(
        "loopback_connect",
        TcpStream::connect(&address).map(|_| "connected".into()),
    );

    if std::env::var_os(ENV_STRICT).is_some() {
        let problems = violations(&observations, &[]);
        if !problems.is_empty() {
            for problem in &problems {
                eprintln!("confinement violation: {problem}");
            }
            std::process::exit(3);
        }
    }
}

// ---------------------------------------------------------------------------
// Parent side.

struct Fixture {
    _base: TempDir,
    cas: PathBuf,
    runs: PathBuf,
    digest: String,
    decoy: PathBuf,
    run_sibling: PathBuf,
    listener: TcpListener,
}

impl Fixture {
    fn new() -> Self {
        // Not the system temp directory: see (a) in the module docs.
        let base = TempDir::new_in(env!("CARGO_TARGET_TMPDIR")).expect("fixture base");
        let root = base.path().canonicalize().expect("canonical fixture base");

        let guest = fs::read(std::env::current_exe().expect("test executable"))
            .expect("read test executable");
        let granted = GRANTED_CONTENT.as_bytes();
        let manifest = format!(
            "{{\"version\":1,\"files\":[\
             {{\"path\":\"{GUEST_EXECUTABLE}\",\"mode\":493,\"blake3\":\"{}\"}},\
             {{\"path\":\"{GRANTED_FILE}\",\"mode\":420,\"blake3\":\"{}\"}}]}}",
            blake3::hash(&guest).to_hex(),
            blake3::hash(granted).to_hex(),
        );
        let digest = blake3::hash(manifest.as_bytes()).to_hex().to_string();
        let cas = root.join("cas");
        let rootfs = cas.join(&digest);
        fs::create_dir_all(rootfs.join("bin")).expect("rootfs directories");
        let executable = rootfs.join(GUEST_EXECUTABLE);
        fs::write(&executable, &guest).expect("guest executable");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
            .expect("guest executable mode");
        let granted_path = rootfs.join(GRANTED_FILE);
        fs::write(&granted_path, granted).expect("granted file");
        fs::set_permissions(&granted_path, fs::Permissions::from_mode(0o644))
            .expect("granted file mode");
        fs::write(rootfs.join("rootfs.manifest.json"), manifest).expect("rootfs manifest");

        let runs = root.join("runs");
        fs::create_dir(&runs).expect("ephemeral root");
        let run_sibling_dir = runs.join("sibling-of-run-root");
        fs::create_dir(&run_sibling_dir).expect("run-root sibling");
        let run_sibling = run_sibling_dir.join("secret.txt");
        fs::write(&run_sibling, SECRET).expect("run-root sibling secret");

        let outside = root.join("outside-the-grant");
        fs::create_dir(&outside).expect("decoy directory");
        let decoy = outside.join("decoy-secret.txt");
        fs::write(&decoy, SECRET).expect("decoy");

        // The decoys provably exist and are readable WITHOUT the sandbox, so a
        // confined failure on them is a denial, not an absence.
        for path in [&decoy, &run_sibling] {
            assert_eq!(fs::read_to_string(path).expect("unconfined read"), SECRET);
        }

        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        // ...and the listener is reachable unconfined, so a confined refusal is
        // the sandbox, not a dead port.
        let probe = TcpStream::connect(listener.local_addr().expect("listener address"))
            .expect("unconfined connect");
        let (accepted, _) = listener.accept().expect("unconfined accept");
        drop((probe, accepted));

        Self {
            _base: base,
            cas,
            runs,
            digest,
            decoy,
            run_sibling,
            listener,
        }
    }

    fn request(&self, environment: &[(&str, &str)]) -> ExecutionRequest {
        let mut public_environment = BTreeMap::from([
            (ENV_PROBE.to_owned(), "1".to_owned()),
            (ENV_DECOY.to_owned(), self.decoy.display().to_string()),
            (
                ENV_RUN_SIBLING.to_owned(),
                self.run_sibling.display().to_string(),
            ),
            (
                ENV_CONNECT.to_owned(),
                self.listener
                    .local_addr()
                    .expect("listener address")
                    .to_string(),
            ),
        ]);
        if let Some(ssh) = home_ssh() {
            public_environment.insert(ENV_SSH.to_owned(), ssh.display().to_string());
        }
        for (key, value) in environment {
            public_environment.insert((*key).to_owned(), (*value).to_owned());
        }
        ExecutionRequest {
            run_id: RUN_ID.into(),
            replay_key: "native-confinement-replay-01".into(),
            rootfs: DigestRef {
                algorithm: "blake3-256".into(),
                value: self.digest.clone(),
            },
            executable: GUEST_EXECUTABLE.into(),
            arguments: vec![
                "--ignored".into(),
                "--exact".into(),
                ENTRY_POINT.into(),
                "--nocapture".into(),
                "--test-threads=1".into(),
            ],
            public_environment,
            allowed_egress: Vec::new(),
            confinement_digest: String::new(),
            confinement_manifest: None,
            workspaces: Vec::new(),
            limits: ResourceLimits {
                vcpus: 1,
                memory_mib: 256,
                wall_time_ms: 60_000,
            },
        }
    }

    /// No connection may have reached the listener while confined. The
    /// guest's connect has returned (the process exited), so a connection
    /// that got through would already be queued; nonblocking accept reads
    /// the queue without waiting.
    fn assert_listener_untouched(&self) {
        self.listener.set_nonblocking(true).expect("nonblocking");
        match self.listener.accept() {
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Ok((_, peer)) => panic!("a confined guest reached the loopback listener from {peer}"),
            Err(error) => panic!("accept on the loopback listener: {error}"),
        }
    }

    fn assert_decoys_intact(&self) {
        for path in [&self.decoy, &self.run_sibling] {
            assert_eq!(
                fs::read_to_string(path).expect("decoy after run"),
                SECRET,
                "decoy {} changed under confinement",
                path.display()
            );
        }
    }
}

/// The host files the guest needs to execute at all, granted read-only as the
/// worker's `--runtime-file`s.
///
/// macOS: none. dyld and the system libraries come from the shared cache,
/// which Seatbelt does not mediate as file reads, so the guest runs with the
/// rootfs as its only grant.
///
/// Linux: the guest is a copy of this dynamically linked test binary, and
/// Landlock denies the loader and libc like any other path, so the guest
/// could not start without them. The shared objects mapped into THIS process
/// are exactly what the identical guest binary loads; they are granted
/// file-by-file, which leaves every decoy (none is a mapped library) denied.
fn guest_runtime_files() -> Vec<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let exe = std::env::current_exe()
            .and_then(|path| path.canonicalize())
            .expect("test executable");
        let maps = fs::read_to_string("/proc/self/maps").expect("/proc/self/maps");
        let mut files = std::collections::BTreeSet::new();
        for line in maps.lines() {
            let Some(path) = line.split_whitespace().nth(5) else {
                continue;
            };
            let path = Path::new(path);
            if path.is_absolute()
                && path.is_file()
                && let Ok(canonical) = path.canonicalize()
                && canonical != exe
            {
                files.insert(canonical);
            }
        }
        // The loader consults its cache first; without it, it still finds the
        // libraries on its default path, but the denied open is noise.
        if Path::new("/etc/ld.so.cache").is_file() {
            files.insert(PathBuf::from("/etc/ld.so.cache"));
        }
        files.into_iter().collect()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

/// `~/.ssh`, when this host has one: the AC's named example of undeclared
/// host access.
fn home_ssh() -> Option<PathBuf> {
    let ssh = PathBuf::from(std::env::var_os("HOME")?).join(".ssh");
    ssh.is_dir().then_some(ssh)
}

/// The fail-closed gate. Returns normally only to report a named skip.
fn sandbox_unavailable(error: &ExecutionError) {
    assert!(
        std::env::var_os("LEYLINE_REQUIRE_SANDBOX").is_none(),
        "LEYLINE_REQUIRE_SANDBOX is set, so this host claims it can apply the native \
         sandbox, but the native worker could not: {error:?}. Refusing to report a pass \
         for a confinement test that confined nothing."
    );
    eprintln!(
        "SKIPPED (NOT a pass): the native worker could not apply nono confinement on \
         this host ({}). Most often the test is already inside an outer sandbox \
         (Seatbelt refuses to nest) or the kernel lacks Landlock. Nothing about \
         native confinement was verified by this run. Set LEYLINE_REQUIRE_SANDBOX=1 \
         to make this a failure.",
        error.detail
    );
}

fn is_apply_failure(error: &ExecutionError) -> bool {
    error.detail.contains("apply native nono confinement")
}

/// Drive the compiled worker binary directly, so the guest's stdout — its
/// report — reaches the test. `None` means the named skip was reported.
fn run_worker_directly(fixture: &Fixture) -> Option<DirectRun> {
    let run_root = fixture.runs.join("run-01");
    fs::create_dir(&run_root).expect("run root");
    let mut command = Command::new(WORKER);
    command
        .arg("--cas-root")
        .arg(&fixture.cas)
        .arg("--run-root")
        .arg(&run_root);
    for path in guest_runtime_files() {
        command.arg("--runtime-file").arg(path);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the first-party native worker");
    let mut stdin = child.stdin.take().expect("worker stdin");
    let mut stdout = child.stdout.take().expect("worker stdout");
    let mut stderr = BufReader::new(child.stderr.take().expect("worker stderr"));

    let request = fixture.request(&[(ENV_WAIT, "1")]);
    serde_json::to_writer(&mut stdin, &request).expect("send request");
    stdin.write_all(b"\n").expect("terminate request");
    stdin.flush().expect("flush request");

    // Event 1: readiness, emitted only after the sandbox is irreversibly
    // applied — or the failure that says it was not.
    // Lines before the event are foreign (libsandbox writes its own refusal
    // to fd 2 ahead of the worker's `failed` event), exactly as the backend's
    // readiness reader treats them.
    let mut line = String::new();
    let mut preamble = String::new();
    loop {
        line.clear();
        let bytes = stderr.read_line(&mut line).expect("readiness line");
        if bytes == 0 || line.trim_start().starts_with('{') {
            break;
        }
        preamble.push_str(&line);
    }
    match serde_json::from_str::<WorkerEvent>(line.trim()) {
        Ok(WorkerEvent::Ready { run_id, .. }) => assert_eq!(run_id, RUN_ID),
        Ok(WorkerEvent::Failed { error }) if is_apply_failure(&error) => {
            let _ = child.wait();
            sandbox_unavailable(&error);
            return None;
        }
        other => {
            let mut rest = String::new();
            let _ = stderr.read_to_string(&mut rest);
            panic!("native worker did not become ready: {other:?} {preamble}{line}{rest}");
        }
    }

    // Between readiness and the go signal the guest has not probed anything.
    // Plant the sibling whose name shares the rootfs's prefix.
    let prefix_sibling = run_root.join(PREFIX_SIBLING);
    fs::create_dir(&prefix_sibling).expect("prefix sibling");
    fs::write(prefix_sibling.join("secret.txt"), SECRET).expect("prefix sibling secret");

    // Event 2: go.
    stdin.write_all(b"go\n").expect("go signal");
    drop(stdin);

    let stdout_reader = std::thread::spawn(move || {
        let mut output = String::new();
        stdout.read_to_string(&mut output).expect("guest stdout");
        output
    });
    let mut diagnostics = String::new();
    stderr
        .read_to_string(&mut diagnostics)
        .expect("worker stderr");
    // Event 3: exit.
    let status = child.wait().expect("worker exit");
    let output = stdout_reader.join().expect("stdout reader");
    assert!(
        status.success(),
        "native worker or guest failed ({status}); stdout:\n{output}\nstderr:\n{diagnostics}"
    );

    // Host-side effects are asserted by the test that owns each claim, so a
    // widened filesystem grant fails the filesystem tests and not the
    // network one.
    Some(DirectRun {
        run_root,
        observations: output
            .lines()
            // Not `strip_prefix`: under `--nocapture` libtest prints
            // `test <name> ... ` without a newline, so the first report shares
            // its line.
            .filter_map(|line| {
                line.find(REPORT_PREFIX)
                    .map(|start| &line[start + REPORT_PREFIX.len()..])
            })
            .map(|json| serde_json::from_str(json).expect("observation"))
            .collect(),
    })
}

struct DirectRun {
    /// The parent-owned run root; it outlives the worker, so the guest's
    /// host-side effects can be inspected.
    run_root: PathBuf,
    observations: Vec<Observation>,
}

/// Judge only the probes this test owns, so each test fails for its own
/// claim. The lifecycle-backend test judges every probe.
fn assert_observations(observations: &[Observation], required: &[&str]) {
    let owned: Vec<Observation> = observations
        .iter()
        .filter(|o| required.contains(&o.probe.as_str()))
        .cloned()
        .collect();
    let problems = violations(&owned, required);
    assert!(
        problems.is_empty(),
        "confinement violations:\n  {}\nobservations: {observations:#?}",
        problems.join("\n  ")
    );
}

fn observation<'a>(observations: &'a [Observation], probe: &str) -> &'a Observation {
    observations
        .iter()
        .find(|o| o.probe == probe)
        .unwrap_or_else(|| panic!("probe {probe} never reported: {observations:#?}"))
}

#[test]
fn an_existing_file_outside_the_grant_is_kernel_denied_not_absent() {
    let _serial = common::serial();
    let fixture = Fixture::new();
    let Some(DirectRun { observations, .. }) = run_worker_directly(&fixture) else {
        return;
    };
    let mut required = vec!["decoy_read"];
    if home_ssh().is_some() {
        required.push("home_ssh_list");
    }
    assert_observations(&observations, &required);
    let decoy = observation(&observations, "decoy_read");
    assert!(
        !decoy.ok
            && matches!(decoy.errno, Some(errno) if errno == libc::EPERM || errno == libc::EACCES),
        "an existing decoy outside the grant must be EPERM/EACCES: {decoy:?}"
    );
    fixture.assert_decoys_intact();
}

#[test]
fn siblings_of_the_granted_root_are_kernel_denied() {
    let _serial = common::serial();
    let fixture = Fixture::new();
    let Some(DirectRun {
        observations,
        run_root,
    }) = run_worker_directly(&fixture)
    else {
        return;
    };
    assert_observations(
        &observations,
        &[
            "run_root_sibling_read",
            "rootfs_prefix_sibling_read",
            "run_root_list",
            "run_root_write",
        ],
    );
    assert!(
        !run_root.join("escaped.txt").exists(),
        "the guest wrote beside its rootfs"
    );
    assert_eq!(
        fs::read_to_string(run_root.join(PREFIX_SIBLING).join("secret.txt"))
            .expect("prefix sibling after run"),
        SECRET
    );
    fixture.assert_decoys_intact();
}

#[test]
fn an_outbound_connect_without_a_network_grant_is_kernel_denied() {
    let _serial = common::serial();
    let fixture = Fixture::new();
    let Some(DirectRun { observations, .. }) = run_worker_directly(&fixture) else {
        return;
    };
    assert_observations(&observations, &["loopback_connect"]);
    fixture.assert_listener_untouched();
}

#[test]
fn the_granted_rootfs_stays_readable_and_writable() {
    let _serial = common::serial();
    let fixture = Fixture::new();
    let Some(DirectRun {
        observations,
        run_root,
    }) = run_worker_directly(&fixture)
    else {
        return;
    };
    assert_observations(&observations, &["granted_read", "granted_write"]);
    assert!(observation(&observations, "granted_read").ok);
    assert_eq!(
        observation(&observations, "granted_write").detail,
        "written-confined"
    );
    assert_eq!(
        fs::read_to_string(run_root.join("rootfs").join("guest-wrote.txt"))
            .expect("the positive-control write landed in the granted rootfs"),
        "written-confined"
    );
}

/// The same guest through the lifecycle backend the service uses, with the
/// guest judging itself: the backend carries only success or failure back, so
/// strict mode exits non-zero on any violation. This pins that
/// `NativeWorkerBackend` launches the confining worker — a backend that ran the
/// guest some other way would let the guest read the decoy and fail here.
#[test]
fn the_lifecycle_backend_runs_the_guest_under_the_same_confinement() {
    let _serial = common::serial();
    let fixture = Fixture::new();
    let backend = NativeWorkerBackend::new(NativeWorkerConfig {
        worker: PathBuf::from(WORKER),
        cas_root: fixture.cas.clone(),
        ephemeral_root: fixture.runs.clone(),
        runtime_files: guest_runtime_files(),
        // Materialization copies and re-hashes the guest twice before
        // readiness; the budget is generous, and readiness is still an event.
        ready_timeout: Duration::from_secs(60),
    });
    let request = fixture.request(&[(ENV_STRICT, "1")]);
    match backend.start(&request) {
        Ok(_) => {}
        Err(error) if is_apply_failure(&error) => {
            sandbox_unavailable(&error);
            return;
        }
        Err(error) => panic!("native backend did not start the confined guest: {error:?}"),
    }
    match backend.wait_for_completion(RUN_ID).expect("completion") {
        BackendRunStatus::Succeeded => {}
        other => panic!(
            "the confined guest reported a violation (or could not run) through the \
             lifecycle backend: {other:?}"
        ),
    }
    fixture.assert_decoys_intact();
    fixture.assert_listener_untouched();
}
