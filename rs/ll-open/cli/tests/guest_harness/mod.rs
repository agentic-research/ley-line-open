//! The host side of a libkrun guest run against the real execution daemon,
//! shared by `agent_in_guest.rs` (the behavior scenarios) and
//! `share_bench.rs` (the shipped-path benchmark, bead
//! `ley-line-open-39d3a4`).
//!
//! The host runs the real `leyline execution-daemon --backend micro-vm
//! --source <checkout>`; a run starts over the socket with an execution/v1
//! RunSpec and a RunGrant carrying a WorkspaceGrant, exactly as a caller
//! would. The daemon's libkrun backend spawns the real `leyline-krun-worker`,
//! which attaches the workspace share; the guest init mounts it at `/work`.
//!
//! Each test crate declares `mod hypervisor` (the runtime's test helpers)
//! before this module.
#![allow(dead_code)]

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

use crate::hypervisor;

pub const MEDIA_TYPE: &str = "application/vnd.leyline.executable";
pub const STEP: Duration = Duration::from_secs(150);

/// The worker binary cargo built beside this test's target directory.
/// `LEYLINE_KRUN_WORKER` (an installed worker) is handled by
/// `hypervisor::worker_under_test`, which checks and runs it as installed.
pub fn worker_binary() -> PathBuf {
    let exe = std::env::current_exe().expect("test binary");
    let path = exe
        .parent()
        .and_then(Path::parent)
        .expect("target directory")
        .join("leyline-krun-worker");
    assert!(
        path.is_file(),
        "{} is missing: build it with `cargo build -p leyline-krun-worker`, \
         or set LEYLINE_KRUN_WORKER",
        path.display()
    );
    path
}

/// The guest tools rootfs fragment, or `None` (a skip, not a pass).
pub fn guest_tools() -> Option<PathBuf> {
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

/// The guest tools fragment when libkrunfw is present too, else `None` (a
/// reported skip, or a failure under `LEYLINE_REQUIRE_HYPERVISOR`).
pub fn guest_tools_if_hypervisor() -> Option<PathBuf> {
    hypervisor::libkrunfw_or_skip()?;
    guest_tools()
}

/// One JSON request over the daemon's socket, one JSON reply.
pub fn call(sock: &Path, request: Value) -> Value {
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
pub fn read_source(sock: &Path, path: &str) -> Option<(String, String)> {
    let reply = call(sock, json!({"op": "read_source", "path": path}));
    (reply["ok"] == true).then(|| {
        (
            reply["content"].as_str().unwrap_or_default().to_owned(),
            reply["tree_root"].as_str().unwrap_or_default().to_owned(),
        )
    })
}

pub fn stored(sock: &Path, path: &str) -> Option<String> {
    read_source(sock, path).map(|(text, _)| text)
}

pub fn propose(sock: &Path, path: &str, content: &str) {
    let reply = call(
        sock,
        json!({"op": "propose", "writes": [{"path": path, "base": "any", "content": content}]}),
    );
    assert_eq!(reply["ok"], true, "propose {path}: {reply}");
}

pub struct Daemon {
    pub child: Child,
    pub sock: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct Host {
    pub dir: TempDir,
    pub leyline: PathBuf,
    pub worker: PathBuf,
    pub libkrunfw: PathBuf,
    pub checkout: PathBuf,
    /// The share's settled entry and attribute timeout
    /// (`--workspace-ttl-ms`) and the timeout of a recently changed path
    /// (`--workspace-changed-ttl-ms`).
    pub settled_ms: u64,
    pub changed_ms: u64,
    /// How long a daemon may take to parse the checkout and become ready.
    pub ready_within: Duration,
}

impl Host {
    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Start the execution daemon over the checkout, and wait until its
    /// socket answers `status` with phase `ready`.
    pub fn daemon(&self, lifetime: &str) -> Daemon {
        let control = self.path("daemon.ctrl");
        let (library_var, library_dir) = hypervisor::library_path_env(&self.libkrunfw);
        let log = fs::File::create(self.path(&format!("daemon-{lifetime}.log"))).expect("log");
        // The daemon's temporary directory: where each run's scratch for
        // derived paths is made (ADR-0045 T9), so a test can see it go.
        fs::create_dir_all(self.path("tmp")).expect("daemon temp dir");
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
            .arg(self.settled_ms.to_string())
            .arg("--workspace-changed-ttl-ms")
            .arg(self.changed_ms.to_string())
            .arg("--workspace-change-window-ms")
            .arg("30000")
            // The daemon publishes a discovery link under $HOME/.mache.
            .env("HOME", self.path("home"))
            .env("TMPDIR", self.path("tmp"))
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
        let deadline = Instant::now() + self.ready_within;
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
            std::hint::spin_loop();
        }
    }

    pub fn log(&self, lifetime: &str) -> String {
        fs::read_to_string(self.path(&format!("daemon-{lifetime}.log"))).unwrap_or_default()
    }

    pub fn on_disk(&self, path: &str) -> Option<String> {
        fs::read_to_string(self.checkout.join(path)).ok()
    }

    /// Move a staged rootfs directory into the CAS under the BLAKE3 of its
    /// manifest, and return that digest.
    pub fn publish_rootfs(&self, staging: &Path) -> String {
        let mut files = Vec::new();
        manifest_files(staging, staging, &mut files);
        let manifest = serde_json::to_vec(&json!({"version": 1, "files": files})).unwrap();
        let digest = blake3::hash(&manifest).to_hex().to_string();
        fs::write(staging.join("rootfs.manifest.json"), &manifest).expect("manifest");
        fs::create_dir_all(self.path("cas")).expect("cas");
        fs::rename(staging, self.path("cas").join(&digest)).expect("publish rootfs");
        digest
    }

    /// The tree root the daemon holds for the checkout, as a grant names it.
    /// Starts and stops a daemon with an empty catalog; `probe` is any file
    /// the checkout holds.
    pub fn graph_root(&self, probe: &str) -> String {
        fs::write(self.path("catalog.json"), r#"{"entries":[]}"#).expect("catalog");
        let daemon = self.daemon("seed");
        let (_, tree_root) = read_source(&daemon.sock, probe).expect("the probe file is stored");
        format!("blake3-256:{tree_root}")
    }

    /// Write the catalog entry naming the agent binary inside `rootfs`, as
    /// the run's executable over workspace `repo` at `graph_root`.
    pub fn write_catalog(&self, rootfs: &str, agent_digest: &str, graph_root: &str) {
        let catalog = json!({"entries": [{
            "artifactDigest": format!("blake3-256:{agent_digest}"),
            "mediaType": MEDIA_TYPE,
            "rootfs": {"algorithm": "blake3-256", "value": rootfs},
            "executable": "bin/agent",
            "workspaceInputs": [{"name": "repo", "graphRoot": graph_root}],
        }]});
        fs::write(self.path("catalog.json"), catalog.to_string()).expect("catalog");
    }

    /// Provision the micro-VM backend and start the run; returns its id.
    pub fn start_run(
        &self,
        daemon: &Daemon,
        lifetime: &str,
        name: &str,
        agent_digest: &str,
        graph_root: &str,
        limits: Limits,
    ) -> String {
        let confinement = leyline_runtime::backends::libkrun::confinement::confinement_manifest(
            std::slice::from_ref(&self.libkrunfw),
            &[],
            None,
        )
        .and_then(|manifest| manifest.confinement_digest())
        .expect("the worker's confinement digest");
        let provision = call(
            &daemon.sock,
            json!({"op": "llo_execution_provision", "backendClass": "microVm", "idempotencyKey": name}),
        );
        assert_eq!(provision["provisioned"], true, "{provision}");
        let (spec, grant) = spec_and_grant(
            name,
            agent_digest,
            graph_root.strip_prefix("blake3-256:").unwrap(),
            &confinement,
            limits,
        );
        let started = call(
            &daemon.sock,
            json!({"op": "llo_execution_start", "spec": spec, "grant": grant}),
        );
        started["runId"]
            .as_str()
            .unwrap_or_else(|| {
                panic!(
                    "the run did not start: {started}\nlog:\n{}",
                    self.log(lifetime)
                )
            })
            .to_owned()
    }

    /// Poll the run's status until it ends; returns the terminal status.
    /// `lifetime` names the daemon log reported when the run outlives
    /// `within`.
    pub fn await_end(
        &self,
        daemon: &Daemon,
        lifetime: &str,
        run_id: &str,
        within: Duration,
    ) -> Value {
        let deadline = Instant::now() + within;
        loop {
            let status = call(
                &daemon.sock,
                json!({"op": "llo_execution_status", "runId": run_id}),
            );
            if status.to_string().contains("succeeded") || status.to_string().contains("failed") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "the run did not end: {status}\nevents: {}\nlog:\n{}",
                call(
                    &daemon.sock,
                    json!({"op": "llo_execution_inspect", "runId": run_id, "afterSequence": 0}),
                ),
                self.log(lifetime)
            );
            std::hint::spin_loop();
        }
    }
}

/// Copy `from` into `to`, regular files and directories only.
pub fn copy_tree(from: &Path, to: &Path) {
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
pub fn manifest_files(root: &Path, dir: &Path, out: &mut Vec<Value>) {
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

pub fn git(dir: &Path, args: &[&str]) {
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

/// A bare-template git repository of `tree` (one commit, gc'd), copied to
/// `dest` as a git directory.
pub fn git_dir_of(host: &Host, tree: &Path, dest: &Path) {
    let empty_template = host.path("git-template");
    fs::create_dir_all(&empty_template).expect("template");
    git(
        tree,
        &[
            "init",
            "-q",
            "-b",
            "main",
            &format!("--template={}", empty_template.display()),
        ],
    );
    git(tree, &["add", "-A"]);
    git(tree, &["commit", "-q", "-m", "seed"]);
    git(tree, &["gc", "-q"]);
    copy_tree(&tree.join(".git"), dest);
}

fn set_digest(mut digest: execution_capnp::digest_ref::Builder<'_>, value: &str) {
    digest.set_algorithm("blake3-256");
    digest.set_value(value);
}

fn evidence(mut evidence: execution_capnp::evidence_ref::Builder<'_>) {
    evidence.set_media_type("application/vnd.leyline.test-evidence");
    set_digest(evidence.init_digest(), &"a".repeat(64));
}

/// A run's resource limits: wall time in milliseconds, OCI `memory.limit` in
/// bytes, and OCI `cpu.quota` per `cpu.period` in microseconds.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub wall_time_ms: u64,
    pub memory_limit: u64,
    pub cpu_quota: u64,
    pub cpu_period: u64,
}

/// The behavior scenarios' limits: two vCPUs, 512 MiB.
pub const LIMITS: Limits = Limits {
    wall_time_ms: 300_000,
    memory_limit: 512 << 20,
    cpu_quota: 200_000,
    cpu_period: 100_000,
};

fn set_limits(mut builder: execution_capnp::resource_limits::Builder<'_>, limits: Limits) {
    builder.set_wall_time_ms(limits.wall_time_ms);
    builder
        .reborrow()
        .init_memory()
        .set_limit(limits.memory_limit);
    let mut cpu = builder.init_cpu();
    cpu.set_quota(limits.cpu_quota);
    cpu.set_period(limits.cpu_period);
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
/// `graph_root`, read and mutate. `name` keys the grant id and replay key.
pub fn spec_and_grant(
    name: &str,
    agent_digest: &str,
    graph_root: &str,
    confinement: &str,
    limits: Limits,
) -> (Value, Value) {
    let mut message = Builder::new_default();
    let mut spec = message.init_root::<execution_capnp::run_spec::Builder<'_>>();
    spec.set_schema_version(EXECUTION_SCHEMA_VERSION);
    let mut executable = spec.reborrow().init_executable();
    executable.set_media_type(MEDIA_TYPE);
    set_digest(executable.init_digest(), agent_digest);
    let mut workspace = spec.reborrow().init_workspace_inputs(1).get(0);
    workspace.set_name("repo");
    set_digest(workspace.init_graph_root(), graph_root);
    set_limits(spec.init_requested_limits(), limits);
    let mut spec_bytes = Vec::new();
    capnp::serialize::write_message(&mut spec_bytes, &message).unwrap();

    let spec_digest = canonical_digest(&spec_bytes).unwrap();
    let mut message = Builder::new_default();
    let mut grant = message.init_root::<execution_capnp::run_grant::Builder<'_>>();
    grant.set_grant_id(format!("grant-{name}"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    grant.set_expires_at_unix_ms(now + 3_600_000);
    grant.set_replay_key(format!("{name}-{now}"));
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
    set_limits(grant.reborrow().init_limits(), limits);
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

/// A new host: a temp directory with the entitled worker copy, the run
/// root and a home, around `checkout`. `None` (a skip) without libkrunfw.
pub fn host(
    checkout_of: impl FnOnce(&Path) -> PathBuf,
    settled_ms: u64,
    changed_ms: u64,
) -> Option<Host> {
    let libkrunfw = hypervisor::libkrunfw_or_skip()?;
    let dir = TempDir::new().expect("fixture");
    let checkout = checkout_of(dir.path());
    // The installed worker when LEYLINE_KRUN_WORKER names one, checked for
    // the entitlement and run as installed; else a signed cargo-built copy.
    let worker =
        hypervisor::worker_under_test(&worker_binary(), &dir.path().join("leyline-krun-worker"));
    fs::create_dir_all(dir.path().join("runs")).expect("run root");
    fs::create_dir_all(dir.path().join("home")).expect("home");
    Some(Host {
        leyline: PathBuf::from(env!("CARGO_BIN_EXE_leyline")),
        worker,
        libkrunfw,
        checkout,
        dir,
        settled_ms,
        changed_ms,
        ready_within: STEP,
    })
}

/// Stage a guest rootfs: the tools fragment, `/bin/agent` compiled from
/// `agent_source` for the guest, and `/tmp`. Returns the staging directory
/// (publish it with [`Host::publish_rootfs`] once complete) and the agent
/// binary's BLAKE3.
pub fn stage_rootfs(host: &Host, tools: &Path, agent_source: &str) -> (PathBuf, String) {
    let staging = host.path("rootfs");
    copy_tree(tools, &staging);
    fs::create_dir_all(staging.join("bin")).expect("bin");
    fs::create_dir_all(staging.join("tmp")).expect("tmp");
    let source = host.path("agent.rs");
    fs::write(&source, agent_source).expect("guest source");
    let agent = staging.join("bin/agent");
    hypervisor::compile_guest(&source, &agent);
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).expect("agent mode");
    let digest = blake3::hash(&fs::read(&agent).expect("agent"))
        .to_hex()
        .to_string();
    (staging, digest)
}
