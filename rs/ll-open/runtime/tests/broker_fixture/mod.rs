//! Shared fixtures for the broker lifecycle and cross-document suites (bead
//! `ley-line-open-5308b7`).
//!
//! The broker is the real `leyline-conn-broker-unconfined` child process (the
//! shipped binary refuses off macOS at its profile, which `broker_profile`
//! covers), started through a wrapper that records its pid. Workers are shell
//! fixtures that speak the native worker's readiness line.

#![allow(dead_code)] // each suite uses a subset

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use leyline_runtime::backends::native_backend::{NativeWorkerBackend, NativeWorkerConfig};
use leyline_runtime::confinement::{ConfinementManifest, UnixSocketGrant};
use leyline_runtime::{
    BrokerConfig, DigestRef, ExecutionRequest, ResourceLimits, VerifiedConnections,
};
use leyline_schema_spec::connections::{Connection, ConnectionsDocument};
use tempfile::TempDir;

pub const UNCONFINED: &str = env!("CARGO_BIN_EXE_leyline-conn-broker-unconfined");

/// Only a bound on a failure: every wait below is on an event.
pub const BOUND: Duration = Duration::from_secs(30);

/// A fresh private channel directory, as an issuer makes one per run: mode
/// 0700, named by its real path.
pub fn channel_dir() -> (TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("ch")
        .tempdir_in("/tmp")
        .expect("channel directory");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).expect("mode");
    let real = std::fs::canonicalize(dir.path()).expect("real path");
    (dir, real)
}

/// A loopback TCP echo; returns its port.
pub fn echo() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("echo listener");
    let port = listener.local_addr().expect("echo address").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            std::thread::spawn(move || {
                let mut buffer = [0u8; 1024];
                while let Ok(n) = stream.read(&mut buffer) {
                    if n == 0 || stream.write_all(&buffer[..n]).is_err() {
                        break;
                    }
                }
                let _ = stream.shutdown(Shutdown::Write);
            });
        }
    });
    port
}

pub fn endpoint(dir: &Path, name: &str) -> String {
    dir.join(format!("b.{name}.sock"))
        .to_str()
        .expect("UTF-8 path")
        .to_owned()
}

/// One connection per name, each at `<dir>/b.<name>.sock`, to `host:port`.
pub fn connections(dir: &Path, names: &[&str], host: &str, port: u16) -> VerifiedConnections {
    let document = ConnectionsDocument::new(
        names
            .iter()
            .map(|name| Connection {
                name: (*name).to_owned(),
                endpoint_unix: endpoint(dir, name),
                host: host.parse().expect("address"),
                port,
            })
            .collect(),
    )
    .expect("connections document");
    VerifiedConnections::new(document)
}

/// A manifest whose §6 grants are `grants`, in order.
pub fn manifest(grants: Vec<UnixSocketGrant>) -> ConfinementManifest {
    grants
        .into_iter()
        .fold(ConfinementManifest::new(), |manifest, grant| {
            manifest.with_unix_socket(grant).expect("§6 grant")
        })
}

/// The grant a tier spells a broker endpoint with: the bare string on the
/// native tier.
pub fn native_grant(dir: &Path, name: &str) -> UnixSocketGrant {
    UnixSocketGrant::connect(endpoint(dir, name))
}

pub fn request(run_id: &str, manifest: Option<&ConfinementManifest>) -> ExecutionRequest {
    ExecutionRequest {
        run_id: run_id.to_owned(),
        replay_key: format!("replay-{run_id}"),
        rootfs: DigestRef {
            algorithm: "blake3-256".into(),
            value: "a".repeat(64),
        },
        executable: "bin/workload".into(),
        arguments: Vec::new(),
        public_environment: Default::default(),
        allowed_egress: Vec::new(),
        confinement_digest: manifest
            .map(|manifest| manifest.confinement_digest().expect("digest"))
            .unwrap_or_default(),
        confinement_manifest: manifest
            .map(|manifest| manifest.to_canonical_json().expect("canonical manifest")),
        workspaces: Vec::new(),
        limits: ResourceLimits {
            wall_time_ms: 120_000,
            ..ResourceLimits::default()
        },
    }
}

/// One host: a directory holding the worker fixture, its roots, and the
/// broker wrapper with the pid it records.
pub struct Host {
    pub dir: TempDir,
}

fn publish(path: &Path, body: &str) {
    std::fs::write(path, body).expect("fixture");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("mode");
}

impl Host {
    pub fn new() -> Self {
        let dir = TempDir::new().expect("host directory");
        std::fs::create_dir(dir.path().join("cas")).expect("cas");
        std::fs::create_dir(dir.path().join("runs")).expect("runs");
        Self { dir }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// A native backend whose worker is a shell fixture: it records that it
    /// was spawned and its pid, reads its request, reports ready for that
    /// run with the digest the request commits to, then runs `tail`.
    pub fn backend(&self, tail: &str) -> NativeWorkerBackend {
        let worker = self.path("worker");
        publish(
            &worker,
            &format!(
                "#!/bin/sh\n\
                 echo $$ > '{pid}'\n\
                 echo $$ >> '{pid}s'\n\
                 IFS= read -r request\n\
                 run=$(printf '%s' \"$request\" | sed 's/.*\"runId\":\"\\([^\"]*\\)\".*/\\1/')\n\
                 digest=$(printf '%s' \"$request\" | sed 's/.*\"confinementDigest\":\"\\([^\"]*\\)\".*/\\1/')\n\
                 printf '{{\"type\":\"ready\",\"run_id\":\"%s\",\"confinement_digest\":\"%s\"}}\\n' \"$run\" \"$digest\" >&2\n\
                 {tail}\n",
                pid = self.path("worker.pid").display(),
            ),
        );
        self.backend_with(worker)
    }

    /// A native backend whose worker is the executable at `worker`.
    pub fn backend_with(&self, worker: PathBuf) -> NativeWorkerBackend {
        NativeWorkerBackend::new(NativeWorkerConfig {
            worker,
            cas_root: self.path("cas"),
            ephemeral_root: self.path("runs"),
            runtime_files: Vec::new(),
            ready_timeout: BOUND,
        })
    }

    /// A worker that never becomes ready: it exits at once (a failure at S7
    /// step 5, after the broker is ready).
    pub fn failing_backend(&self) -> NativeWorkerBackend {
        let worker = self.path("worker");
        publish(&worker, "#!/bin/sh\nexit 3\n");
        self.backend_with(worker)
    }

    /// The broker this host's runs get: the unconfined test broker behind a
    /// wrapper that records its pid. With `stall`, the broker binds every
    /// endpoint, writes its pid to that FIFO and never reports ready.
    pub fn broker(&self, stall: Option<&Path>) -> BrokerConfig {
        self.broker_script("", stall)
    }

    /// [`Self::broker`], with the shell lines `before` run in the wrapper
    /// after it records its pid and before it becomes the broker.
    pub fn broker_with(&self, before: &str) -> BrokerConfig {
        self.broker_script(before, None)
    }

    fn broker_script(&self, before: &str, stall: Option<&Path>) -> BrokerConfig {
        let wrapper = self.path("broker");
        let stall = stall
            .map(|fifo| format!("LEYLINE_TEST_BROKER_STALL='{}' ", fifo.display()))
            .unwrap_or_default();
        publish(
            &wrapper,
            &format!(
                "#!/bin/sh\necho $$ > '{pid}'\necho $$ >> '{pid}s'\n{before}\n{stall}exec '{UNCONFINED}' \"$@\"\n",
                pid = self.path("broker.pid").display()
            ),
        );
        let mut config = BrokerConfig::new(wrapper).with_unverified_delivery();
        // Generous: a stalled wait must be ended by the cancel under test,
        // never by this bound.
        config.ready_timeout = Duration::from_secs(120);
        config
    }

    fn pid(&self, file: &str) -> Option<libc::pid_t> {
        std::fs::read_to_string(self.path(file))
            .ok()
            .and_then(|text| text.trim().parse().ok())
    }

    /// The pid the broker wrapper recorded, if a broker was ever spawned.
    pub fn broker_pid(&self) -> Option<libc::pid_t> {
        self.pid("broker.pid")
    }

    /// Every pid the wrapper of that kind recorded, one per spawn
    /// (`broker` or `worker`).
    pub fn pids(&self, kind: &str) -> Vec<libc::pid_t> {
        std::fs::read_to_string(self.path(&format!("{kind}.pids")))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect()
    }

    /// The worker fixture's pid, if a worker was ever spawned.
    pub fn worker_pid(&self) -> Option<libc::pid_t> {
        self.pid("worker.pid")
    }
}

/// A FIFO at `path`.
pub fn fifo(path: &Path) {
    let text = std::ffi::CString::new(path.to_str().expect("UTF-8 path")).expect("no NUL");
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(text.as_ptr(), 0o600) }, 0, "mkfifo");
}

/// Ends the whole test binary, failed, when a test that can only hang on a
/// defect (a cancel that waits on a start that never ends) outlives its
/// bound. Dropping it disarms it. The bound is a failure's: nothing waits on
/// it when the code is right.
pub struct Watchdog(std::sync::mpsc::Sender<()>);

pub fn watchdog(test: &'static str) -> Watchdog {
    let (disarm, armed) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        if armed.recv_timeout(Duration::from_secs(45))
            == Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        {
            eprintln!("TIMEOUT: {test} is stuck; a cancel or a start never returned");
            std::process::exit(101);
        }
    });
    Watchdog(disarm)
}
