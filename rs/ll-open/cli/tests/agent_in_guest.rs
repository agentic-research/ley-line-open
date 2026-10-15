//! ADR-0044 phase C (bead `ley-line-open-f210c2`): an agent works through a
//! filesystem, with no LLO tool, inside a libkrun guest whose `/work` is the
//! daemon's store.
//!
//! The host runs the real `leyline execution-daemon --backend micro-vm
//! --source <checkout>`; the test starts an execution/v1 run whose RunGrant
//! carries a WorkspaceGrant over the socket, exactly as a caller would. The
//! daemon's libkrun backend spawns the real `leyline-krun-worker`, which
//! attaches the share; the guest init mounts it at `/work` and starts the
//! guest program (`agent_in_guest/guest.rs`) there. The guest and this test
//! coordinate through files in the store (the guest writes
//! `sync/guest.md`; the test proposes `sync/host.md` over the socket), so
//! every coordination step is itself a share write or a socket proposal.
//!
//! Scenarios (guest syscall + errno, then host verification over the
//! socket and in the checkout directory):
//!
//! - (a) a read is the stored bytes;
//! - (b) an open/write/close edit lands in the store and the checkout;
//! - (c) a socket proposal to a file the guest holds open: write(2) ESTALE,
//!   nothing lands;
//! - (d) an unparseable edit: fsync(2) and close(2) EIO, nothing lands;
//! - (e) create, unlink, and write-temp-then-rename land and reach the
//!   checkout;
//! - (f) `git status` and `rg` (Alpine aarch64 binaries in the guest rootfs,
//!   from `tools/guest_tools.sh`) over `/work`;
//! - (g) after a host proposal the guest reads the new bytes at its next
//!   open, and a cached `stat` turns over within the settled timeout;
//! - (h) the daemon is killed with SIGKILL and restarted: the accepted
//!   guest edits persist in the store and the checkout.
//!
//! Gated like the runtime's hypervisor tests: it runs where
//! `LEYLINE_LIBKRUNFW_PATH` names libkrunfw and reports a skip (not a pass)
//! elsewhere; `LEYLINE_REQUIRE_HYPERVISOR` turns the skip into a failure.
//! It also needs `LEYLINE_GUEST_TOOLS` (the rootfs fragment
//! `tools/guest_tools.sh` writes) and the worker binary
//! (`cargo build -p leyline-runtime --bin leyline-krun-worker`, or
//! `LEYLINE_KRUN_WORKER`).

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use capnp::message::{Builder, ReaderOptions};
use leyline_public_schema::execution_capnp;
use leyline_runtime::authorization::{
    EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION, canonical_digest,
};
use serde_json::{Value, json};
use tempfile::TempDir;

#[path = "../../runtime/tests/hypervisor/mod.rs"]
mod hypervisor;
#[path = "agent_in_guest/seed.rs"]
mod seed;

const GUEST: &str = include_str!("agent_in_guest/guest.rs");
const SEED: &str = include_str!("agent_in_guest/seed.rs");
const MEDIA_TYPE: &str = "application/vnd.leyline.executable";
const STEP: Duration = Duration::from_secs(150);

/// The worker binary cargo built beside this test's target directory.
fn worker_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("LEYLINE_KRUN_WORKER") {
        return PathBuf::from(path);
    }
    let exe = std::env::current_exe().expect("test binary");
    let path = exe
        .parent()
        .and_then(Path::parent)
        .expect("target directory")
        .join("leyline-krun-worker");
    assert!(
        path.is_file(),
        "{} is missing: build it with `cargo build -p leyline-runtime --bin \
         leyline-krun-worker`, or set LEYLINE_KRUN_WORKER",
        path.display()
    );
    path
}

/// The guest tools rootfs fragment, or `None` (a skip, not a pass).
fn guest_tools() -> Option<PathBuf> {
    match std::env::var_os("LEYLINE_GUEST_TOOLS") {
        Some(dir) => Some(PathBuf::from(dir)),
        None => {
            assert!(
                std::env::var_os("LEYLINE_REQUIRE_HYPERVISOR").is_none(),
                "LEYLINE_REQUIRE_HYPERVISOR is set and LEYLINE_GUEST_TOOLS is not: run \
                 tools/guest_tools.sh <dir> and point it at <dir>/rootfs"
            );
            eprintln!(
                "SKIPPED (NOT a pass): LEYLINE_GUEST_TOOLS is not set; run \
                 tools/guest_tools.sh <dir> and point it at <dir>/rootfs"
            );
            None
        }
    }
}

/// One JSON request over the daemon's socket, one JSON reply.
fn call(sock: &Path, request: Value) -> Value {
    let mut stream = UnixStream::connect(sock)
        .unwrap_or_else(|error| panic!("connect {}: {error}", sock.display()));
    stream
        .write_all(format!("{request}\n").as_bytes())
        .expect("send request");
    stream
        .shutdown(std::net::Shutdown::Write)
        .expect("end request");
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .expect("read reply");
    serde_json::from_str(&line).unwrap_or_else(|error| panic!("reply {line:?}: {error}"))
}

/// `read_source`: the stored text and the store's tree root, or `None`
/// when the store holds no such file.
fn read_source(sock: &Path, path: &str) -> Option<(String, String)> {
    let reply = call(sock, json!({"op": "read_source", "path": path}));
    (reply["ok"] == true).then(|| {
        (
            reply["content"].as_str().unwrap_or_default().to_owned(),
            reply["tree_root"].as_str().unwrap_or_default().to_owned(),
        )
    })
}

fn stored(sock: &Path, path: &str) -> Option<String> {
    read_source(sock, path).map(|(text, _)| text)
}

fn propose(sock: &Path, path: &str, content: &str) {
    let reply = call(
        sock,
        json!({"op": "propose", "writes": [{"path": path, "base": "any", "content": content}]}),
    );
    assert_eq!(reply["ok"], true, "propose {path}: {reply}");
}

struct Daemon {
    child: Child,
    sock: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Host {
    dir: TempDir,
    leyline: PathBuf,
    worker: PathBuf,
    libkrunfw: PathBuf,
    checkout: PathBuf,
}

impl Host {
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Start the execution daemon over the checkout, and wait until its
    /// socket answers `status` with phase `ready`.
    fn daemon(&self, lifetime: &str) -> Daemon {
        let control = self.path("daemon.ctrl");
        let (library_var, library_dir) = hypervisor::library_path_env(&self.libkrunfw);
        let log = fs::File::create(self.path(&format!("daemon-{lifetime}.log"))).expect("log");
        let child = Command::new(&self.leyline)
            .arg("execution-daemon")
            .arg("--backend")
            .arg("micro-vm")
            .arg("--arena")
            .arg(self.path("daemon.arena"))
            .arg("--control")
            .arg(&control)
            .arg("--cas-root")
            .arg(self.path("cas"))
            .arg("--run-root")
            .arg(self.path("runs"))
            .arg("--worker")
            .arg(&self.worker)
            .arg("--catalog")
            .arg(self.path("catalog.json"))
            .arg("--runtime-file")
            .arg(&self.libkrunfw)
            .arg("--ready-timeout-ms")
            .arg("30000")
            .arg("--allow-unverified-evidence")
            .arg("--source")
            .arg(&self.checkout)
            .arg("--workspace-ttl-ms")
            .arg(seed::SETTLED_MS.to_string())
            .arg("--workspace-changed-ttl-ms")
            .arg(seed::CHANGED_MS.to_string())
            .arg("--workspace-change-window-ms")
            .arg("30000")
            // The daemon publishes a discovery link under $HOME/.mache.
            .env("HOME", self.path("home"))
            .env(library_var, library_dir)
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(log.try_clone().expect("log"))
            .stderr(log)
            .spawn()
            .expect("spawn leyline execution-daemon");
        let daemon = Daemon {
            child,
            sock: control.with_extension("sock"),
        };
        let deadline = Instant::now() + STEP;
        loop {
            if daemon.sock.exists()
                && let Ok(stream) = UnixStream::connect(&daemon.sock)
            {
                drop(stream);
                if call(&daemon.sock, json!({"op": "status"}))["phase"] == "ready" {
                    return daemon;
                }
            }
            assert!(
                Instant::now() < deadline,
                "the daemon did not become ready; log:\n{}",
                self.log(lifetime)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn log(&self, lifetime: &str) -> String {
        fs::read_to_string(self.path(&format!("daemon-{lifetime}.log"))).unwrap_or_default()
    }

    fn on_disk(&self, path: &str) -> Option<String> {
        fs::read_to_string(self.checkout.join(path)).ok()
    }
}

/// Copy `from` into `to`, regular files and directories only.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("directory");
    for entry in fs::read_dir(from).expect("read directory") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        let kind = entry.file_type().expect("file type");
        assert!(!kind.is_symlink(), "{}: no links", entry.path().display());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}

/// Every regular file under `root`, relative, with its mode and BLAKE3.
fn manifest_files(root: &Path, dir: &Path, out: &mut Vec<Value>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .expect("read directory")
        .map(|e| e.expect("entry"))
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).expect("metadata");
        if meta.is_dir() {
            manifest_files(root, &path, out);
        } else {
            let bytes = fs::read(&path).expect("file bytes");
            out.push(json!({
                "path": path.strip_prefix(root).unwrap().to_str().unwrap(),
                "mode": meta.permissions().mode() & 0o7777,
                "blake3": blake3::hash(&bytes).to_hex().to_string(),
            }));
        }
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "agent")
        .env("GIT_AUTHOR_EMAIL", "agent@example.invalid")
        .env("GIT_COMMITTER_NAME", "agent")
        .env("GIT_COMMITTER_EMAIL", "agent@example.invalid")
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?}");
}

/// The guest rootfs: the guest program, the tools fragment, and a git
/// repository of the seed tree at `/repo.git`. Returns its content digest.
fn guest_rootfs(host: &Host, tools: &Path) -> String {
    let staging = host.path("rootfs");
    copy_tree(tools, &staging);
    fs::create_dir_all(staging.join("bin")).expect("bin");
    fs::create_dir_all(staging.join("tmp")).expect("tmp");
    let source = host.path("agent.rs");
    fs::write(&source, format!("mod seed {{\n{SEED}\n}}\n{GUEST}")).expect("guest source");
    let agent = staging.join("bin/agent");
    hypervisor::compile_guest(&source, &agent);
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).expect("agent mode");

    // The repository `git status` in the guest compares /work against.
    let seed_tree = host.path("seed-tree");
    for (path, text) in seed::TREE {
        let file = seed_tree.join(path);
        fs::create_dir_all(file.parent().unwrap()).expect("seed directory");
        fs::write(file, text).expect("seed file");
    }
    let empty_template = host.path("git-template");
    fs::create_dir_all(&empty_template).expect("template");
    git(
        &seed_tree,
        &[
            "init",
            "-q",
            "-b",
            "main",
            &format!("--template={}", empty_template.display()),
        ],
    );
    git(&seed_tree, &["add", "-A"]);
    git(&seed_tree, &["commit", "-q", "-m", "seed"]);
    git(&seed_tree, &["gc", "-q"]);
    copy_tree(&seed_tree.join(".git"), &staging.join("repo.git"));

    let mut files = Vec::new();
    manifest_files(&staging, &staging, &mut files);
    let manifest = serde_json::to_vec(&json!({"version": 1, "files": files})).unwrap();
    let digest = blake3::hash(&manifest).to_hex().to_string();
    fs::write(staging.join("rootfs.manifest.json"), &manifest).expect("manifest");
    fs::create_dir_all(host.path("cas")).expect("cas");
    fs::rename(&staging, host.path("cas").join(&digest)).expect("publish rootfs");
    digest
}

fn set_digest(mut digest: execution_capnp::digest_ref::Builder<'_>, value: &str) {
    digest.set_algorithm("blake3-256");
    digest.set_value(value);
}

fn evidence(mut evidence: execution_capnp::evidence_ref::Builder<'_>) {
    evidence.set_media_type("application/vnd.leyline.test-evidence");
    set_digest(evidence.init_digest(), &"a".repeat(64));
}

const LIMITS: (u64, u64, u64) = (300_000, 512 << 20, 2_000);

fn set_limits(mut limits: execution_capnp::resource_limits::Builder<'_>) {
    limits.set_wall_time_ms(LIMITS.0);
    limits.set_memory_bytes(LIMITS.1);
    limits.set_cpu_millis(LIMITS.2);
}

fn spec_json(bytes: &[u8]) -> Value {
    let mut input = bytes;
    let message = capnp::serialize::read_message(&mut input, ReaderOptions::new()).unwrap();
    let root = message
        .get_root::<execution_capnp::run_spec::Reader<'_>>()
        .unwrap();
    serde_json::from_str(&capnp_json::to_json(root).unwrap()).unwrap()
}

fn grant_json(bytes: &[u8]) -> Value {
    let mut input = bytes;
    let message = capnp::serialize::read_message(&mut input, ReaderOptions::new()).unwrap();
    let root = message
        .get_root::<execution_capnp::run_grant::Reader<'_>>()
        .unwrap();
    serde_json::from_str(&capnp_json::to_json(root).unwrap()).unwrap()
}

/// The RunSpec and RunGrant of the agent's run: workspace `repo` at
/// `graph_root`, read and mutate.
fn spec_and_grant(agent_digest: &str, graph_root: &str, confinement: &str) -> (Value, Value) {
    let mut message = Builder::new_default();
    let mut spec = message.init_root::<execution_capnp::run_spec::Builder<'_>>();
    spec.set_schema_version(EXECUTION_SCHEMA_VERSION);
    let mut executable = spec.reborrow().init_executable();
    executable.set_media_type(MEDIA_TYPE);
    set_digest(executable.init_digest(), agent_digest);
    let mut workspace = spec.reborrow().init_workspace_inputs(1).get(0);
    workspace.set_name("repo");
    set_digest(workspace.init_graph_root(), graph_root);
    set_limits(spec.init_requested_limits());
    let mut spec_bytes = Vec::new();
    capnp::serialize::write_message(&mut spec_bytes, &message).unwrap();

    let spec_digest = canonical_digest(&spec_bytes).unwrap();
    let mut message = Builder::new_default();
    let mut grant = message.init_root::<execution_capnp::run_grant::Builder<'_>>();
    grant.set_grant_id("grant-agent-in-guest");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    grant.set_expires_at_unix_ms(now + 3_600_000);
    grant.set_replay_key(&format!("agent-in-guest-{now}"));
    set_digest(
        grant.reborrow().init_run_spec_digest(),
        spec_digest.strip_prefix("blake3-256:").unwrap(),
    );
    evidence(grant.reborrow().init_issuer_evidence());
    evidence(grant.reborrow().init_workload_identity_evidence());
    evidence(grant.reborrow().init_actor_provenance_evidence());
    set_digest(
        grant.reborrow().init_confinement_digest(),
        confinement.strip_prefix("blake3-256:").unwrap(),
    );
    grant.set_backend_class(execution_capnp::BackendClass::MicroVm);
    set_limits(grant.reborrow().init_limits());
    let mut capability = grant.reborrow().init_capabilities(1).get(0);
    capability.set_grant(EXECUTION_CAPABILITY);
    capability.set_interface(EXECUTION_SCHEMA_VERSION);
    let mut workspace = grant.init_workspaces(1).get(0);
    workspace.set_name("repo");
    set_digest(workspace.reborrow().init_graph_root(), graph_root);
    let mut operations = workspace.init_operations(2);
    operations.set(0, execution_capnp::WorkspaceOperation::Read);
    operations.set(1, execution_capnp::WorkspaceOperation::Mutate);
    let mut grant_bytes = Vec::new();
    capnp::serialize::write_message(&mut grant_bytes, &message).unwrap();
    (spec_json(&spec_bytes), grant_json(&grant_bytes))
}

/// Waits for the guest's `st-<step>` in the store, failing fast when the
/// run ends first.
fn await_guest(host: &Host, daemon: &Daemon, run_id: &str, step: char) {
    let want = format!("st-{step}\n");
    let deadline = Instant::now() + STEP;
    loop {
        if stored(&daemon.sock, seed::GUEST_SYNC).as_deref() == Some(want.as_str()) {
            return;
        }
        let status = call(
            &daemon.sock,
            json!({"op": "llo_execution_status", "runId": run_id}),
        );
        let state = status.to_string();
        assert!(
            Instant::now() < deadline && !state.contains("succeeded") && !state.contains("failed"),
            "the guest never reached step {step}: {status}\nevents: {}\nreport: {:?}\nlog:\n{}",
            call(
                &daemon.sock,
                json!({"op": "llo_execution_inspect", "runId": run_id, "afterSequence": 0}),
            ),
            stored(&daemon.sock, seed::REPORT),
            host.log("run")
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn go(daemon: &Daemon, step: char) {
    propose(&daemon.sock, seed::HOST_SYNC, &format!("go-{step}\n"));
}

#[derive(Default)]
struct Results(Vec<String>);

impl Results {
    fn check(&mut self, scenario: &str, ok: bool, what: String) {
        self.0.push(format!(
            "{} ({scenario}) host: {what}",
            if ok { "PASS" } else { "FAIL" }
        ));
    }
}

#[test]
fn an_agent_in_a_microvm_works_through_the_store_backed_filesystem() {
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    let Some(tools) = guest_tools() else {
        return;
    };
    let dir = TempDir::new().expect("fixture");
    let checkout = dir.path().join("checkout");
    for (path, text) in seed::TREE {
        let file = checkout.join(path);
        fs::create_dir_all(file.parent().unwrap()).expect("checkout directory");
        fs::write(file, text).expect("checkout file");
    }
    let worker = dir.path().join("leyline-krun-worker");
    hypervisor::entitled_copy(&worker_binary(), &worker);
    fs::create_dir_all(dir.path().join("runs")).expect("run root");
    fs::create_dir_all(dir.path().join("home")).expect("home");
    let host = Host {
        leyline: PathBuf::from(env!("CARGO_BIN_EXE_leyline")),
        worker,
        libkrunfw,
        checkout,
        dir,
    };
    let rootfs = guest_rootfs(&host, &tools);
    let agent_digest =
        blake3::hash(&fs::read(host.path("cas").join(&rootfs).join("bin/agent")).expect("agent"))
            .to_hex()
            .to_string();

    // Lifetime 1: learn the tree root the grant authorizes, with an empty
    // catalog.
    fs::write(host.path("catalog.json"), r#"{"entries":[]}"#).expect("catalog");
    let graph_root = {
        let daemon = host.daemon("seed");
        let (_, tree_root) = read_source(&daemon.sock, "a.go").expect("a.go is stored");
        format!("blake3-256:{tree_root}")
    };
    let catalog = json!({"entries": [{
        "artifactDigest": format!("blake3-256:{agent_digest}"),
        "mediaType": MEDIA_TYPE,
        "rootfs": {"algorithm": "blake3-256", "value": rootfs},
        "executable": "bin/agent",
        "workspaceInputs": [{"name": "repo", "graphRoot": graph_root}],
    }]});
    fs::write(host.path("catalog.json"), catalog.to_string()).expect("catalog");
    let confinement = leyline_runtime::backends::libkrun::confinement::confinement_manifest(
        std::slice::from_ref(&host.libkrunfw),
        &[],
        None,
    )
    .and_then(|manifest| manifest.confinement_digest())
    .expect("the worker's confinement digest");

    // Lifetime 2: the run.
    let mut results = Results::default();
    let daemon = host.daemon("run");
    let (_, root_now) = read_source(&daemon.sock, "a.go").expect("a.go");
    assert_eq!(
        format!("blake3-256:{root_now}"),
        graph_root,
        "a restart keeps the tree root"
    );
    let provision = call(
        &daemon.sock,
        json!({"op": "llo_execution_provision", "backendClass": "microVm", "idempotencyKey": "agent-in-guest"}),
    );
    assert_eq!(provision["provisioned"], true, "{provision}");
    let (spec, grant) = spec_and_grant(
        &agent_digest,
        graph_root.strip_prefix("blake3-256:").unwrap(),
        &confinement,
    );
    let started = call(
        &daemon.sock,
        json!({"op": "llo_execution_start", "spec": spec, "grant": grant}),
    );
    let run_id = started["runId"]
        .as_str()
        .unwrap_or_else(|| {
            panic!(
                "the run did not start: {started}\nlog:\n{}",
                host.log("run")
            )
        })
        .to_owned();

    // (b) the guest's edit is in the store and the checkout.
    await_guest(&host, &daemon, &run_id, 'b');
    results.check(
        "b",
        stored(&daemon.sock, "a.go").as_deref() == Some(seed::A_EDIT)
            && host.on_disk("a.go").as_deref() == Some(seed::A_EDIT),
        "read_source a.go and checkout a.go hold the guest's edit".into(),
    );
    go(&daemon, 'b');

    // (c) a socket proposal moves b.go under the guest's open handle.
    await_guest(&host, &daemon, &run_id, 'c');
    let (_, before) = read_source(&daemon.sock, "b.go").expect("b.go");
    propose(&daemon.sock, "b.go", seed::B_HOST);
    go(&daemon, 'c');
    await_guest(&host, &daemon, &run_id, 'C');
    results.check(
        "c",
        stored(&daemon.sock, "b.go").as_deref() == Some(seed::B_HOST)
            && host.on_disk("b.go").as_deref() == Some(seed::B_HOST),
        format!(
            "b.go holds the socket proposal, not the guest's write (tree root before it: {before})"
        ),
    );
    go(&daemon, 'C');

    // (d) the unparseable edit landed nowhere.
    await_guest(&host, &daemon, &run_id, 'd');
    results.check(
        "d",
        stored(&daemon.sock, "c.go").as_deref() == Some(seed::C)
            && host.on_disk("c.go").as_deref() == Some(seed::C),
        "c.go is unchanged in the store and the checkout".into(),
    );
    go(&daemon, 'd');

    // (e) create, unlink, rename.
    await_guest(&host, &daemon, &run_id, 'e');
    results.check(
        "e",
        stored(&daemon.sock, "n.go").as_deref() == Some(seed::N)
            && host.on_disk("n.go").as_deref() == Some(seed::N),
        "n.go created in the store and the checkout".into(),
    );
    results.check(
        "e",
        stored(&daemon.sock, "d.go").is_none() && host.on_disk("d.go").is_none(),
        "d.go deleted from the store and the checkout".into(),
    );
    results.check(
        "e",
        stored(&daemon.sock, "a.go").as_deref() == Some(seed::A_RENAMED)
            && host.on_disk("a.go").as_deref() == Some(seed::A_RENAMED)
            && stored(&daemon.sock, seed::A_TEMP).is_none()
            && host.on_disk(seed::A_TEMP).is_none(),
        "a.go holds the renamed save; the temp file reached neither".into(),
    );
    go(&daemon, 'e');

    // (g) a host proposal the guest sees at its next open.
    await_guest(&host, &daemon, &run_id, 'g');
    propose(&daemon.sock, "g.go", seed::G_HOST);
    propose(&daemon.sock, "t.go", seed::T_HOST);
    go(&daemon, 'g');

    await_guest(&host, &daemon, &run_id, 'z');
    let deadline = Instant::now() + STEP;
    let terminal = loop {
        let status = call(
            &daemon.sock,
            json!({"op": "llo_execution_status", "runId": run_id}),
        );
        if status.to_string().contains("succeeded") || status.to_string().contains("failed") {
            break status;
        }
        assert!(Instant::now() < deadline, "the run did not end: {status}");
        std::thread::sleep(Duration::from_millis(50));
    };
    results.check(
        "run",
        terminal.to_string().contains("succeeded"),
        format!("the execution/v1 run ended: {terminal}"),
    );
    let report = stored(&daemon.sock, seed::REPORT).unwrap_or_default();
    results.check(
        "report",
        host.on_disk(seed::REPORT).as_deref() == Some(report.as_str()),
        "the guest's report.md is in the store and the checkout".into(),
    );

    // (h) SIGKILL the daemon, restart it: the accepted guest edits persist.
    drop(daemon);
    let daemon = host.daemon("restart");
    let expected: BTreeMap<&str, Option<&str>> = BTreeMap::from([
        ("a.go", Some(seed::A_RENAMED)),
        ("b.go", Some(seed::B_HOST)),
        ("c.go", Some(seed::C)),
        ("d.go", None),
        ("n.go", Some(seed::N)),
        ("g.go", Some(seed::G_HOST)),
        ("t.go", Some(seed::T_HOST)),
    ]);
    for (path, text) in &expected {
        results.check(
            "h",
            stored(&daemon.sock, path).as_deref() == *text
                && host.on_disk(path).as_deref() == *text,
            format!(
                "after kill -9 and restart, {path} is {}",
                text.map_or("absent", |_| "the accepted bytes")
            ),
        );
    }
    drop(daemon);

    let mut lines: Vec<String> = report.lines().map(str::to_owned).collect();
    lines.extend(results.0);
    eprintln!("agent-in-guest results:\n{}", lines.join("\n"));
    let scenarios = ["a", "b", "c", "d", "e", "f", "g", "h"];
    for scenario in scenarios {
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with(&format!("PASS ({scenario})"))),
            "scenario ({scenario}) has no PASS:\n{}",
            lines.join("\n")
        );
    }
    let failed: Vec<&String> = lines.iter().filter(|l| l.starts_with("FAIL")).collect();
    assert!(
        failed.is_empty(),
        "failures:\n{failed:#?}\nall:\n{}",
        lines.join("\n")
    );
}
