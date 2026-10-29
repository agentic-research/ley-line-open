//! The execution/v1 conformance matrix (bead `ley-line-open-f8ebcf`): one set
//! of rows, every row run against every backend.
//!
//! Each row is a function over a [`Column`] — one backend, configured with a
//! real worker and a real guest — and the [`conformance_rows!`] macro expands
//! the full set of rows into `#[test]`s for one column. Two binaries invoke
//! it:
//!
//! - `runtime/tests/conformance_matrix.rs`: the **native nono** column, with
//!   the first-party `leyline-native-worker` applying Seatbelt or Landlock.
//! - `krun-worker/tests/conformance_matrix.rs`: the **embedded libkrun**
//!   column, with the real `leyline-krun-worker` booting a guest. It lives in
//!   that crate because only that crate builds the worker.
//!
//! A row written here therefore runs on both backends or on neither: a column
//! cannot drift by skipping a vector the other one runs, and the expected
//! outcome of every vector is written once. Where the tiers legitimately
//! differ (the channel each delivers, the errno a denial carries) the column
//! names its tier's mechanism and the row asserts the shared property.
//!
//! The rows and what they prove are listed in `runtime/README.md`
//! ("Conformance matrix").
//!
//! ## Skips are named, never passes
//!
//! A column the host cannot run reports `SKIPPED (NOT a pass)` from its
//! constructor. `LEYLINE_REQUIRE_HYPERVISOR=1` turns a missing hypervisor into
//! a failure and `LEYLINE_REQUIRE_SANDBOX=1` a native sandbox that cannot be
//! applied, the same contract every guest and confinement suite already has.
//!
//! ## Synchronization is by event
//!
//! A started guest proves it is running by dialing the host over its granted
//! channel; the host's `accept` returning is the event. Natural completion is
//! the backend's completion condition variable (`Observed::settle`). The only
//! timed waits are failure bounds on an `accept` that never comes.

#![allow(dead_code)] // each column binary, and libkrun_guest_cancel, uses a subset

pub mod guest;

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use capnp::message::Builder;
use leyline_public_schema::execution_capnp;
use leyline_runtime::authorization::{
    AuthorizationPolicy, AuthorizedExecution, EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION,
    canonical_digest,
};
use leyline_runtime::backends::libkrun::confinement::{VSOCK_UNIX_BASE_PORT, confinement_manifest};
use leyline_runtime::confinement::{ConfinementManifest, UnixSocketGrant};
use leyline_runtime::{
    Backend, BackendCapabilities, BackendClass, BackendRun, BackendRunStatus, DigestRef, ErrorCode,
    ExecutionError, ExecutionRequest, ExecutionResolver, ExecutionService, ResourceLimits,
    RunReceiptData, RunState, WorkspaceGrant,
};
use tempfile::TempDir;

/// The guest program's source, for the column that compiles it.
pub const GUEST_SOURCE: &str = include_str!("guest.rs");

/// The file every published rootfs carries beside its executable: the
/// positive control a guest must be able to read.
pub const GRANTED_FILE: &str = "granted.txt";
pub const GRANTED_CONTENT: &str = "granted-rootfs-content";
/// What every decoy holds. A probe detail containing it is a leak.
pub const SECRET: &str = "SIMULATED PRIVATE KEY - must never be readable by a workload";

/// A bound on a failure only: a guest that dials is accepted at once, and one
/// that never does fails the row here instead of hanging it.
pub const DIAL_BOUND: Duration = Duration::from_secs(90);

// ---------------------------------------------------------------------------
// A column: one backend under test.

/// How a backend reports completion without a poll loop. Both first-party
/// backends publish terminal status behind a condition variable after their
/// cleanup and diagnostics; this is that wait.
pub trait Waitable: Backend {
    fn wait(&self, run_id: &str) -> Result<BackendRunStatus, ExecutionError>;
}

/// A backend wrapped so a row can wait for completion by EVENT and the
/// service still projects it through its ordinary `poll`.
///
/// The backends' own `wait_for_completion` consumes the terminal status the
/// service's `poll` would read, so calling it directly would leave the run
/// `Running` forever. [`Observed::settle`] waits, keeps the status, and hands
/// it to the next `poll` — the service's projection runs unchanged.
#[derive(Clone)]
pub struct Observed(Arc<ObservedInner>);

struct ObservedInner {
    backend: Box<dyn Waitable>,
    settled: Mutex<HashMap<String, BackendRunStatus>>,
}

impl Observed {
    pub fn new(backend: impl Waitable) -> Self {
        Self(Arc::new(ObservedInner {
            backend: Box::new(backend),
            settled: Mutex::new(HashMap::new()),
        }))
    }

    /// Block until the run's worker has exited and its run root is gone, and
    /// return how it ended.
    pub fn settle(&self, run_id: &str) -> BackendRunStatus {
        let status = self
            .0
            .backend
            .wait(run_id)
            .unwrap_or_else(|error| panic!("wait for run {run_id}: {error}"));
        self.0
            .settled
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(run_id.to_owned(), status.clone());
        status
    }
}

impl Backend for Observed {
    fn capabilities(&self) -> BackendCapabilities {
        self.0.backend.capabilities()
    }

    fn provision(&self) -> Result<BackendCapabilities, ExecutionError> {
        self.0.backend.provision()
    }

    fn start(&self, request: &ExecutionRequest) -> Result<BackendRun, ExecutionError> {
        self.0.backend.start(request)
    }

    fn poll(&self, run_id: &str) -> Result<Option<BackendRunStatus>, ExecutionError> {
        let settled = self
            .0
            .settled
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(run_id);
        match settled {
            Some(status) => Ok(Some(status)),
            None => self.0.backend.poll(run_id),
        }
    }

    fn cancel(&self, run_id: &str) -> Result<bool, ExecutionError> {
        self.0.backend.cancel(run_id)
    }
}

/// The channel a tier delivers for a guest to reach the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    /// §3 `connectLocal`: one loopback TCP port (the native tier).
    TcpLocal,
    /// §6 `unixSocket.allow` connect, delivered as a vsock port (the microVM
    /// tier).
    UnixVsock,
}

/// How a tier keeps a host path from its workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denial {
    /// The path is visible and the kernel refuses it: EPERM or EACCES.
    Kernel,
    /// The path is not in the workload's namespace at all: ENOENT. The
    /// microVM guest has its own root; no host path is mapped into it.
    Absent,
}

/// The host-visible process that holds a running guest, which the cancel
/// and crash rows require gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holder {
    /// The guest's parent: the native worker that applied the sandbox and
    /// spawned it. The guest reports it.
    GuestParent,
    /// The peer of the host end of the guest's channel: the libkrun VMM,
    /// which is the worker (libkrun's VMM runs in-process).
    ChannelPeer,
}

/// Everything about a column that is not the backend itself.
pub struct ColumnSpec {
    pub name: &'static str,
    pub class: BackendClass,
    pub backend_id: &'static str,
    /// The published guest rootfs (see [`Fixture::publish_guest`]).
    pub rootfs: String,
    pub executable: String,
    pub arguments: Vec<String>,
    /// The `--runtime-file`s the backend grants read-only. The base manifest
    /// is built from them exactly as the worker builds it.
    pub runtime_files: Vec<PathBuf>,
    pub channel: ChannelKind,
    pub denial: Denial,
    pub holder: Holder,
    pub limits: ResourceLimits,
    /// Add this tier's undeliverable channel dimension to a manifest: a
    /// grant the tier cannot enforce, which must be refused before the
    /// workload runs rather than dropped from the policy it applies.
    pub undeliverable: fn(ConfinementManifest) -> Result<ConfinementManifest, ExecutionError>,
    pub undeliverable_name: &'static str,
    /// Whether a start error means the host could not apply the native
    /// sandbox at all (a named skip), as opposed to a failure.
    pub sandbox_refusal: fn(&ExecutionError) -> bool,
}

pub struct Column {
    pub spec: ColumnSpec,
    pub backend: Observed,
    pub service: ExecutionService<Observed>,
    pub fixture: Fixture,
    trap: SubprocessTrap,
    sockets: usize,
}

impl Column {
    pub fn new(fixture: Fixture, spec: ColumnSpec, backend: impl Waitable) -> Self {
        let trap = SubprocessTrap::install(&fixture.root.join("trap"));
        let backend = Observed::new(backend);
        Self {
            service: ExecutionService::new(backend.clone()),
            backend,
            spec,
            fixture,
            trap,
            sockets: 0,
        }
    }

    pub fn run_roots(&self) -> usize {
        run_roots(&self.fixture.runs)
    }

    /// The tier's base confinement document plus, when given, the channel
    /// grant — the same document the worker compiles, so the digest the grant
    /// names is the digest the worker attests.
    pub fn manifest(&self, channel: Option<&Channel>) -> ConfinementManifest {
        let base = confinement_manifest(&self.spec.runtime_files, &[], None)
            .expect("the tier's base confinement document");
        match channel {
            None => base,
            Some(channel) => channel.grant(base),
        }
    }

    /// Bind the host end of a fresh channel of this tier's kind.
    pub fn open_channel(&mut self) -> Channel {
        match self.spec.channel {
            ChannelKind::TcpLocal => {
                let listener = TcpListener::bind("127.0.0.1:0").expect("loopback channel");
                let port = listener.local_addr().expect("channel address").port();
                Channel {
                    listener: Listener::Tcp(listener),
                    dial: format!("tcp:127.0.0.1:{port}"),
                }
            }
            ChannelKind::UnixVsock => {
                self.sockets += 1;
                // Short: a UNIX socket path is capped near 104 bytes on macOS.
                let path = self.fixture.root.join(format!("c{}.sock", self.sockets));
                let listener = UnixListener::bind(&path).expect("UNIX channel");
                Channel {
                    listener: Listener::Unix(listener, path),
                    // Grant 0's dial port: the manifest carries one §6 grant.
                    dial: format!("vsock:{VSOCK_UNIX_BASE_PORT}"),
                }
            }
        }
    }

    pub fn policy(&self, digest: &str) -> AuthorizationPolicy {
        AuthorizationPolicy {
            now_unix_ms: None,
            required_backend: self.spec.class,
            required_confinement_digest: Some(digest.to_owned()),
        }
    }

    pub fn resolver(&self, environment: &[(&str, String)]) -> GuestResolver {
        GuestResolver {
            rootfs: self.spec.rootfs.clone(),
            executable: self.spec.executable.clone(),
            arguments: self.spec.arguments.clone(),
            environment: environment
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect(),
            limits: self.spec.limits,
        }
    }

    /// Provision the column's backend class, as every caller must first.
    pub fn provision(&self) {
        let capabilities = self
            .service
            .provision(self.spec.class, "conformance-provision")
            .expect("provision the column's backend");
        assert_eq!(capabilities.backend_id, self.spec.backend_id);
    }

    /// Authorize and start one guest through execution/v1. `None` means the
    /// host could not apply the native sandbox and the named skip has been
    /// reported.
    pub fn start(
        &self,
        replay_key: &str,
        manifest: &ConfinementManifest,
        environment: &[(&str, String)],
    ) -> Option<Started> {
        let document = manifest.to_canonical_json().expect("canonical document");
        let digest = manifest.confinement_digest().expect("confinement digest");
        let (spec, grant) = spec_and_grant(&GrantSpec::new(
            self.spec.class,
            replay_key,
            &digest,
            &document,
        ));
        match self.service.start_authorized(
            &spec,
            &grant,
            &self.policy(&digest),
            &self.resolver(environment),
        ) {
            Ok(record) => {
                assert_eq!(record.state, RunState::Running, "{}", self.spec.name);
                assert_eq!(record.backend_id, self.spec.backend_id);
                Some(Started {
                    run_id: record.run_id,
                    spec,
                    grant,
                    digest,
                })
            }
            Err(error) if (self.spec.sandbox_refusal)(&error) => {
                sandbox_unavailable(self.spec.name, &error);
                None
            }
            Err(error) => panic!(
                "{}: start a guest through execution/v1: {error}",
                self.spec.name
            ),
        }
    }

    /// The row passed without reaching for a forbidden subprocess.
    pub fn finish(self) {
        self.trap.assert_untouched();
    }
}

/// One started run and the authority it was started under.
pub struct Started {
    pub run_id: String,
    pub spec: Vec<u8>,
    pub grant: Vec<u8>,
    pub digest: String,
}

pub enum Listener {
    Tcp(TcpListener),
    Unix(UnixListener, PathBuf),
}

/// The host end of a guest's granted channel.
pub struct Channel {
    pub listener: Listener,
    /// The guest's `CONFORMANCE_DIAL`.
    pub dial: String,
}

impl Channel {
    fn grant(&self, base: ConfinementManifest) -> ConfinementManifest {
        match &self.listener {
            Listener::Tcp(listener) => base
                .with_connect_local(listener.local_addr().expect("channel address").port())
                .expect("connectLocal grant"),
            Listener::Unix(_, path) => base
                .with_unix_socket(UnixSocketGrant::connect(path.display().to_string()))
                .expect("unixSocket grant"),
        }
    }

    /// Wait for the guest to dial. The bound is a failure bound only.
    pub fn accept(self, column: &Column, run_id: &str) -> Accepted {
        let (sender, receiver) = mpsc::channel();
        match self.listener {
            Listener::Tcp(listener) => {
                std::thread::spawn(move || {
                    let _ =
                        sender.send(listener.accept().map(|(stream, _)| Connection::Tcp(stream)));
                });
            }
            Listener::Unix(listener, _) => {
                std::thread::spawn(move || {
                    let _ = sender.send(
                        listener
                            .accept()
                            .map(|(stream, _)| Connection::Unix(stream)),
                    );
                });
            }
        }
        let connection = match receiver.recv_timeout(DIAL_BOUND) {
            Ok(Ok(connection)) => connection,
            Ok(Err(error)) => panic!("{}: accept the guest's dial: {error}", column.spec.name),
            Err(_) => panic!(
                "{}: the guest did not dial the host within {DIAL_BOUND:?}; run state: {:?}",
                column.spec.name,
                column.service.status(run_id)
            ),
        };
        let peer = match &connection {
            Connection::Unix(stream) => Some(peer_pid(stream)),
            Connection::Tcp(_) => None,
        };
        Accepted {
            reader: BufReader::new(connection),
            peer,
        }
    }
}

pub enum Connection {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl Read for Connection {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.read(buffer),
            Self::Unix(stream) => stream.read(buffer),
        }
    }
}

/// The host end of a dialed channel.
pub struct Accepted {
    pub reader: BufReader<Connection>,
    /// The process holding the far end, where the channel can say (a UNIX
    /// socket's peer credentials).
    pub peer: Option<libc::pid_t>,
}

impl Accepted {
    pub fn line(&mut self) -> String {
        let mut line = String::new();
        self.reader
            .read_line(&mut line)
            .expect("read the guest's line");
        line.trim_end().to_owned()
    }

    /// The channel ended: end of stream, or a reset — either is the kernel
    /// saying the far end is closed.
    pub fn assert_closed(&mut self) {
        let mut rest = Vec::new();
        match self.reader.read_to_end(&mut rest) {
            Ok(_) => assert!(rest.is_empty(), "the guest wrote after blocking: {rest:?}"),
            Err(error) => assert_eq!(
                error.kind(),
                ErrorKind::ConnectionReset,
                "the guest's channel did not end with its run: {error}"
            ),
        }
    }
}

/// A blocked guest's report, and the host process that holds it.
pub struct Blocked {
    pub guest_pid: libc::pid_t,
    pub holder: libc::pid_t,
}

impl Blocked {
    pub fn read(column: &Column, accepted: &mut Accepted) -> Self {
        let line = accepted.line();
        let fields: BTreeMap<&str, &str> = line
            .strip_prefix("blocked ")
            .unwrap_or_else(|| panic!("{}: the guest's first line: {line:?}", column.spec.name))
            .split(' ')
            .filter_map(|field| field.split_once('='))
            .collect();
        let pid = |name: &str| -> libc::pid_t {
            fields
                .get(name)
                .and_then(|value| value.parse().ok())
                .unwrap_or_else(|| panic!("blocked line without {name}: {line:?}"))
        };
        let holder = match column.spec.holder {
            Holder::GuestParent => pid("ppid"),
            Holder::ChannelPeer => accepted.peer.expect("a UNIX channel names its peer"),
        };
        Self {
            guest_pid: pid("pid"),
            holder,
        }
    }

    /// Nothing the run started is left on the host.
    pub fn assert_gone(&self, column: &Column, when: &str) {
        assert!(
            !alive(self.holder),
            "{}: the process {} holding the guest survived {when}",
            column.spec.name,
            self.holder
        );
        assert!(
            !group_alive(self.holder),
            "{}: a process in the worker's group {} survived {when}",
            column.spec.name,
            self.holder
        );
        if column.spec.holder == Holder::GuestParent {
            assert!(
                !alive(self.guest_pid),
                "{}: the guest process {} survived {when}",
                column.spec.name,
                self.guest_pid
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Fixture: the directories a column's backend owns, and the decoys a row
// plants outside them.

pub struct Fixture {
    _base: TempDir,
    /// Canonical.
    pub root: PathBuf,
    pub cas: PathBuf,
    pub runs: PathBuf,
}

impl Fixture {
    /// Under `base`, never the system temp directory, where a default-allowed
    /// decoy proves nothing (`native_confinement.rs`, claim (a)).
    pub fn new_in(base: &Path) -> Self {
        let dir = TempDir::new_in(base).expect("fixture base");
        let root = dir.path().canonicalize().expect("canonical fixture base");
        let cas = root.join("cas");
        let runs = root.join("runs");
        fs::create_dir(&cas).expect("CAS root");
        fs::create_dir(&runs).expect("ephemeral root");
        Self {
            _base: dir,
            root,
            cas,
            runs,
        }
    }

    /// Publish `executable` and the granted positive-control file as a
    /// pinned CAS rootfs; return its digest.
    pub fn publish_guest(&self, executable: &str, bytes: &[u8]) -> String {
        publish_rootfs(
            &self.cas,
            &[
                (executable, bytes, 0o755),
                (GRANTED_FILE, GRANTED_CONTENT.as_bytes(), 0o644),
            ],
        )
    }

    /// A file outside every grant, holding [`SECRET`], read back unconfined
    /// so a denial of it cannot be an absent file in disguise.
    pub fn decoy(&self, relative: &str) -> PathBuf {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().expect("decoy parent")).expect("decoy directory");
        fs::write(&path, SECRET).expect("decoy");
        assert_eq!(fs::read_to_string(&path).expect("unconfined read"), SECRET);
        path
    }
}

/// Publish `files` as a CAS rootfs under `cas` and return its digest: the
/// BLAKE3 of the `rootfs.manifest.json` that pins each file's bytes and mode.
pub fn publish_rootfs(cas: &Path, files: &[(&str, &[u8], u32)]) -> String {
    let entries: Vec<String> = files
        .iter()
        .map(|(path, bytes, mode)| {
            format!(
                "{{\"path\":\"{path}\",\"mode\":{mode},\"blake3\":\"{}\"}}",
                blake3::hash(bytes).to_hex()
            )
        })
        .collect();
    let manifest = format!("{{\"version\":1,\"files\":[{}]}}", entries.join(","));
    let digest = blake3::hash(manifest.as_bytes()).to_hex().to_string();
    let rootfs = cas.join(&digest);
    for (path, bytes, mode) in files {
        let target = rootfs.join(path);
        fs::create_dir_all(target.parent().expect("rootfs file parent")).expect("rootfs tree");
        fs::write(&target, bytes).expect("rootfs file");
        fs::set_permissions(&target, fs::Permissions::from_mode(*mode)).expect("rootfs mode");
    }
    fs::write(rootfs.join("rootfs.manifest.json"), manifest).expect("rootfs manifest");
    digest
}

pub fn run_roots(path: &Path) -> usize {
    fs::read_dir(path).expect("enumerate run roots").count()
}

// ---------------------------------------------------------------------------
// No krunvm, no Taskfile.

/// `krunvm`, `task` and `go-task` on the front of `PATH`, each recording that
/// it ran. A backend that shelled out to any of them — by name, through
/// `PATH`, as a convenience wrapper would — leaves a record the row's
/// [`Column::finish`] fails on. The workers are spawned with this process's
/// environment, so the trap reaches them and anything they spawn.
struct SubprocessTrap {
    dir: PathBuf,
    previous: Option<std::ffi::OsString>,
}

const TRAPPED: [&str; 3] = ["krunvm", "task", "go-task"];

impl SubprocessTrap {
    fn install(dir: &Path) -> Self {
        fs::create_dir_all(dir).expect("subprocess trap");
        for name in TRAPPED {
            let path = dir.join(name);
            fs::write(
                &path,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 97\n",
                    dir.join(format!("invoked-{name}")).display()
                ),
            )
            .expect("trap script");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("trap mode");
        }
        let previous = std::env::var_os("PATH");
        let mut path = dir.as_os_str().to_owned();
        if let Some(previous) = &previous {
            path.push(":");
            path.push(previous);
        }
        // SAFETY: every row holds its binary's serial lock, and nothing else
        // in a column binary reads the environment concurrently.
        unsafe { std::env::set_var("PATH", path) };
        Self {
            dir: dir.to_path_buf(),
            previous,
        }
    }

    fn assert_untouched(&self) {
        for name in TRAPPED {
            let record = self.dir.join(format!("invoked-{name}"));
            assert!(
                !record.exists(),
                "the execution path ran `{name}` ({}): normal operation must not shell \
                 out to krunvm or a Taskfile target",
                fs::read_to_string(&record).unwrap_or_default().trim()
            );
        }
    }
}

impl Drop for SubprocessTrap {
    fn drop(&mut self) {
        // SAFETY: as in `install`.
        unsafe {
            match &self.previous {
                Some(previous) => std::env::set_var("PATH", previous),
                None => std::env::remove_var("PATH"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// execution/v1 fixtures.

/// Maps every authorized run to the column's guest, carrying every
/// grant-owned field through unchanged (the service refuses a resolver that
/// drops or alters them).
pub struct GuestResolver {
    pub rootfs: String,
    pub executable: String,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub limits: ResourceLimits,
}

impl ExecutionResolver for GuestResolver {
    fn resolve(
        &self,
        authorized: &AuthorizedExecution,
    ) -> Result<ExecutionRequest, ExecutionError> {
        Ok(ExecutionRequest {
            run_id: authorized.run_id.clone(),
            replay_key: authorized.replay_key.clone(),
            rootfs: DigestRef {
                algorithm: "blake3-256".into(),
                value: self.rootfs.clone(),
            },
            executable: self.executable.clone(),
            arguments: self.arguments.clone(),
            public_environment: self.environment.clone(),
            allowed_egress: authorized.allowed_egress.clone(),
            confinement_digest: authorized.confinement_digest.clone(),
            confinement_manifest: authorized.confinement_manifest.clone(),
            workspaces: authorized.workspaces.clone(),
            limits: self.limits,
        })
    }
}

/// What a RunGrant authorizes, for [`spec_and_grant`].
pub struct GrantSpec<'a> {
    pub class: BackendClass,
    pub grant_id: String,
    pub replay_key: String,
    /// `blake3-256:<hex>`.
    pub confinement_digest: &'a str,
    pub confinement_document: &'a str,
    pub expires_at_unix_ms: u64,
    pub allowed_egress: Vec<String>,
}

impl<'a> GrantSpec<'a> {
    /// A grant valid for an hour from now, with no egress.
    pub fn new(
        class: BackendClass,
        replay_key: &str,
        confinement_digest: &'a str,
        confinement_document: &'a str,
    ) -> Self {
        Self {
            class,
            grant_id: format!("grant-{replay_key}"),
            replay_key: replay_key.to_owned(),
            confinement_digest,
            confinement_document,
            expires_at_unix_ms: now_unix_ms() + 3_600_000,
            allowed_egress: Vec::new(),
        }
    }
}

pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64
}

/// A RunSpec and a RunGrant authorizing it, committing to the confinement
/// document the grant names.
pub fn spec_and_grant(authority: &GrantSpec<'_>) -> (Vec<u8>, Vec<u8>) {
    let mut message = Builder::new_default();
    let mut spec = message.init_root::<execution_capnp::run_spec::Builder<'_>>();
    spec.set_schema_version(EXECUTION_SCHEMA_VERSION);
    let mut executable = spec.init_executable();
    executable.set_media_type("application/vnd.leyline.executable");
    set_digest(executable.init_digest(), &"c".repeat(64));
    let mut spec_bytes = Vec::new();
    capnp::serialize::write_message(&mut spec_bytes, &message).expect("serialize spec");

    let spec_digest = canonical_digest(&spec_bytes).expect("spec digest");
    let mut message = Builder::new_default();
    let mut grant = message.init_root::<execution_capnp::run_grant::Builder<'_>>();
    grant.set_grant_id(&authority.grant_id);
    grant.set_expires_at_unix_ms(authority.expires_at_unix_ms);
    grant.set_replay_key(&authority.replay_key);
    set_digest(
        grant.reborrow().init_run_spec_digest(),
        spec_digest
            .strip_prefix("blake3-256:")
            .expect("digest prefix"),
    );
    placeholder_evidence(grant.reborrow().init_issuer_evidence());
    placeholder_evidence(grant.reborrow().init_workload_identity_evidence());
    placeholder_evidence(grant.reborrow().init_actor_provenance_evidence());
    set_digest(
        grant.reborrow().init_confinement_digest(),
        authority
            .confinement_digest
            .strip_prefix("blake3-256:")
            .expect("digest prefix"),
    );
    grant.set_confinement_manifest(authority.confinement_document);
    grant.set_backend_class(match authority.class {
        BackendClass::Native => execution_capnp::BackendClass::Native,
        BackendClass::MicroVm => execution_capnp::BackendClass::MicroVm,
    });
    if !authority.allowed_egress.is_empty() {
        let mut egress = grant
            .reborrow()
            .init_allowed_egress(authority.allowed_egress.len() as u32);
        for (index, entry) in authority.allowed_egress.iter().enumerate() {
            egress.set(index as u32, entry);
        }
    }
    let mut capability = grant.reborrow().init_capabilities(1).get(0);
    capability.set_grant(EXECUTION_CAPABILITY);
    capability.set_interface(EXECUTION_SCHEMA_VERSION);
    let mut grant_bytes = Vec::new();
    capnp::serialize::write_message(&mut grant_bytes, &message).expect("serialize grant");
    (spec_bytes, grant_bytes)
}

fn set_digest(mut digest: execution_capnp::digest_ref::Builder<'_>, value: &str) {
    digest.set_algorithm("blake3-256");
    digest.set_value(value);
}

/// Unsigned evidence: accepted only by the metadata-only verifier
/// `start_authorized` uses, which is a test adapter and not a trust boundary.
fn placeholder_evidence(mut evidence: execution_capnp::evidence_ref::Builder<'_>) {
    evidence.set_media_type("application/vnd.leyline.test-evidence");
    set_digest(evidence.init_digest(), &"a".repeat(64));
}

// ---------------------------------------------------------------------------
// Process probes: asked of the kernel, never waited on.

/// Whether a process with this pid exists. `kill(pid, 0)` delivers nothing; it
/// only asks the kernel.
pub fn alive(pid: libc::pid_t) -> bool {
    signal_zero(unsafe { libc::kill(pid, 0) })
}

/// Whether any process remains in the process group `pgid`. Both backends
/// make their worker a group leader, so its pid names the group.
pub fn group_alive(pgid: libc::pid_t) -> bool {
    signal_zero(unsafe { libc::killpg(pgid, 0) })
}

fn signal_zero(rc: i32) -> bool {
    if rc == 0 {
        return true;
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => false,
        // Exists, but is not ours to signal.
        Some(libc::EPERM) => true,
        _ => panic!("kill(.., 0) failed unexpectedly: {error}"),
    }
}

/// The pid of the process at the other end of a connected UNIX socket.
pub fn peer_pid(stream: &UnixStream) -> libc::pid_t {
    #[cfg(target_os = "macos")]
    {
        let mut pid: libc::pid_t = 0;
        let mut length = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: a live socket and an out-parameter of the size passed.
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut length,
            )
        };
        assert_eq!(rc, 0, "LOCAL_PEERPID: {}", std::io::Error::last_os_error());
        pid
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut credentials = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: a live socket and an out-parameter of the size passed.
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            )
        };
        assert_eq!(rc, 0, "SO_PEERCRED: {}", std::io::Error::last_os_error());
        credentials.pid
    }
}

/// The native fail-closed gate: returns normally only to report a named skip.
pub fn sandbox_unavailable(column: &str, error: &ExecutionError) {
    assert!(
        std::env::var_os("LEYLINE_REQUIRE_SANDBOX").is_none(),
        "LEYLINE_REQUIRE_SANDBOX is set, so this host claims it can apply the native \
         sandbox, but the {column} worker could not: {error:?}. Refusing to report a pass \
         for a conformance row that confined nothing."
    );
    eprintln!(
        "SKIPPED (NOT a pass): the {column} worker could not apply nono confinement on \
         this host ({}). Most often the test is already inside an outer sandbox \
         (Seatbelt refuses to nest) or the kernel lacks Landlock. Nothing about this \
         conformance row was verified on {column}. Set LEYLINE_REQUIRE_SANDBOX=1 to make \
         this a failure.",
        error.detail
    );
}

// ---------------------------------------------------------------------------
// Receipts.

/// What every collected receipt must say, on every backend.
pub fn assert_receipt(
    column: &Column,
    started: &Started,
    receipt: &RunReceiptData,
    terminal: RunState,
) {
    let name = column.spec.name;
    assert_eq!(receipt.run_id, started.run_id, "{name}");
    assert_eq!(receipt.terminal_state, terminal, "{name}");
    assert_eq!(receipt.backend_id, column.spec.backend_id, "{name}");
    assert_eq!(receipt.context.backend_class, column.spec.class, "{name}");
    assert_eq!(
        receipt.context.run_spec_digest,
        canonical_digest(&started.spec).expect("spec digest"),
        "{name}: the receipt binds the spec that was started"
    );
    assert_eq!(
        receipt.context.run_grant_digest,
        canonical_digest(&started.grant).expect("grant digest"),
        "{name}: the receipt binds the grant that authorized it"
    );
    assert_eq!(
        receipt.context.confinement_digest, started.digest,
        "{name}: the receipt names the policy the worker attested"
    );
    assert!(receipt.context.input_roots.is_empty(), "{name}");
    assert_eq!(receipt.backend_evidence.backend_id, column.spec.backend_id);
    assert!(
        receipt.backend_evidence.channels.is_empty(),
        "{name}: a run with no workspace holds no channel beyond its manifest"
    );
    assert_eq!(
        receipt.enforced,
        column.service.capabilities().enforced,
        "{name}: the receipt records the mechanism the tier applied"
    );
    // The event-log root is the BLAKE3 of the log the inspect surface
    // returns, so a consumer can verify one against the other.
    let events = column
        .service
        .inspect(&started.run_id, 0)
        .expect("inspect")
        .events;
    let bytes = serde_json::to_vec(&events).expect("encode event log");
    assert_eq!(
        receipt.event_log_root,
        format!("blake3-256:{}", blake3::hash(&bytes).to_hex()),
        "{name}: the receipt's event-log root is the log inspect returns"
    );
    assert!(receipt.started_at_unix_ms <= receipt.completed_at_unix_ms);
}

pub fn states(column: &Column, run_id: &str) -> Vec<RunState> {
    column
        .service
        .inspect(run_id, 0)
        .expect("inspect")
        .events
        .into_iter()
        .map(|event| event.state)
        .collect()
}

const RUNNING: [RunState; 4] = [
    RunState::Accepted,
    RunState::Provisioning,
    RunState::Ready,
    RunState::Running,
];

fn lifecycle(terminal: RunState) -> Vec<RunState> {
    let mut states = RUNNING.to_vec();
    states.push(terminal);
    states
}

fn guest_env(mode: &str, channel: Option<&Channel>) -> Vec<(&'static str, String)> {
    let mut environment = vec![("CONFORMANCE_MODE", mode.to_owned())];
    if let Some(channel) = channel {
        environment.push(("CONFORMANCE_DIAL", channel.dial.clone()));
    }
    environment
}

// ---------------------------------------------------------------------------
// The rows.

/// Expand every row into a `#[test]` for one column. `$column` is an
/// expression evaluating to `Option<Column>`; `None` means the column
/// reported a named skip. `$graph` runs the Graph I/O row. Declare the
/// module `#[macro_use]` to reach it.
#[allow(unused_macros)] // libkrun_guest_cancel shares the helpers, not the rows
macro_rules! conformance_rows {
    (column: $column:expr, serial: $serial:expr, graph: $graph:expr $(,)?) => {
        #[test]
        fn capabilities_and_status_are_read_only() {
            let _serial = $serial;
            if let Some(column) = $column {
                conformance::row_capabilities_and_status_are_read_only(column);
            }
        }

        #[test]
        fn refusals_are_identical_and_precede_backend_work() {
            let _serial = $serial;
            if let Some(column) = $column {
                conformance::row_refusals_are_identical_and_precede_backend_work(column);
            }
        }

        #[test]
        fn a_guest_that_exits_zero_is_succeeded_with_a_bound_receipt() {
            let _serial = $serial;
            if let Some(column) = $column {
                conformance::row_a_guest_that_exits_zero_is_succeeded_with_a_bound_receipt(column);
            }
        }

        #[test]
        fn a_guest_that_exits_nonzero_is_failed_with_a_content_addressed_detail() {
            let _serial = $serial;
            if let Some(column) = $column {
                conformance::row_a_guest_that_exits_nonzero_is_failed_with_a_content_addressed_detail(column);
            }
        }

        #[test]
        fn cancel_ends_a_blocked_guest_and_everything_it_ran() {
            let _serial = $serial;
            if let Some(column) = $column {
                conformance::row_cancel_ends_a_blocked_guest_and_everything_it_ran(column);
            }
        }

        #[test]
        fn a_killed_worker_is_failed_reaped_and_cleaned() {
            let _serial = $serial;
            if let Some(column) = $column {
                conformance::row_a_killed_worker_is_failed_reaped_and_cleaned(column);
            }
        }

        #[test]
        fn undeclared_host_access_and_the_raw_arena_are_unreachable() {
            let _serial = $serial;
            if let Some(column) = $column {
                conformance::row_undeclared_host_access_and_the_raw_arena_are_unreachable(column);
            }
        }

        #[test]
        fn a_workspace_grant_nothing_serves_is_refused_before_spawn() {
            let _serial = $serial;
            if let Some(column) = $column {
                conformance::row_a_workspace_grant_nothing_serves_is_refused_before_spawn(column);
            }
        }

        #[test]
        fn graph_io_goes_through_the_mediated_workspace_share() {
            let _serial = $serial;
            $graph;
        }
    };
}

/// Capabilities name the tier; status, inspect, collect, cleanup and cancel
/// of an unknown run are the same typed refusal everywhere; and none of
/// them writes anything or provisions anything.
pub fn row_capabilities_and_status_are_read_only(column: Column) {
    let name = column.spec.name;
    let capabilities = column.service.capabilities();
    assert_eq!(capabilities.backend_id, column.spec.backend_id, "{name}");
    assert_eq!(capabilities.backend_class, column.spec.class, "{name}");
    assert!(capabilities.available, "{name}: {capabilities:?}");
    let advertised: serde_json::Value = serde_json::from_str(
        &leyline_runtime::transport::capabilities_json(&column.service).expect("capabilities"),
    )
    .expect("capabilities JSON");
    let backend_entry = match column.spec.class {
        BackendClass::Native => "backend/native",
        BackendClass::MicroVm => "backend/microvm",
    };
    assert!(
        advertised["capabilities"]
            .as_array()
            .expect("capability list")
            .iter()
            .any(|entry| entry["name"] == backend_entry
                && entry["version"] == column.spec.backend_id),
        "{name}: {advertised}"
    );

    let unknown = "run-that-was-never-started";
    assert_eq!(column.service.status(unknown).expect("status"), None);
    for (operation, error) in [
        ("inspect", column.service.inspect(unknown, 0).err()),
        ("collect", column.service.collect(unknown).err()),
        ("cleanup", column.service.cleanup(unknown).err()),
        ("cancel", column.service.cancel(unknown).err()),
    ] {
        let error = error.unwrap_or_else(|| panic!("{name}: {operation} of an unknown run"));
        assert_eq!(error.code, ErrorCode::InvalidSpec, "{name}: {operation}");
        assert_eq!(error.detail, "run_id not found", "{name}: {operation}");
    }
    assert!(
        !column.service.is_provisioned(),
        "{name}: reads must not provision"
    );
    assert_eq!(column.run_roots(), 0, "{name}: reads must not write");
    column.finish();
}

/// Every refusal carries the same code on every backend and happens before
/// the backend materializes anything: the ephemeral root stays empty and the
/// refused run never exists.
pub fn row_refusals_are_identical_and_precede_backend_work(column: Column) {
    let name = column.spec.name;
    let manifest = column.manifest(None);
    let document = manifest.to_canonical_json().expect("document");
    let digest = manifest.confinement_digest().expect("digest");
    let resolver = column.resolver(&guest_env("exit:0", None));
    let other_class = match column.spec.class {
        BackendClass::Native => BackendClass::MicroVm,
        BackendClass::MicroVm => BackendClass::Native,
    };

    let refused = |label: &str, authority: GrantSpec<'_>, policy: AuthorizationPolicy| {
        let (spec, grant) = spec_and_grant(&authority);
        let error = column
            .service
            .start_authorized(&spec, &grant, &policy, &resolver)
            .expect_err(label);
        assert_eq!(column.run_roots(), 0, "{name}: {label} reached the backend");
        error
    };
    let expect = |label: &str, error: ExecutionError, code: ErrorCode, retryable: bool| {
        assert_eq!(error.code, code, "{name}: {label}: {error}");
        assert_eq!(error.retryable, retryable, "{name}: {label}: {error}");
    };

    // Before provisioning, nothing starts.
    expect(
        "unprovisioned start",
        refused(
            "unprovisioned start",
            GrantSpec::new(
                column.spec.class,
                "refuse-unprovisioned",
                &digest,
                &document,
            ),
            column.policy(&digest),
        ),
        ErrorCode::NotProvisioned,
        true,
    );
    let error = column
        .service
        .provision(other_class, "refuse-provision")
        .expect_err("provisioning another tier");
    expect(
        "provision another tier",
        error,
        ErrorCode::UnsupportedBackend,
        false,
    );
    column.provision();

    let other = GrantSpec::new(other_class, "refuse-class", &digest, &document);
    expect(
        "grant for another tier",
        refused("grant for another tier", other, column.policy(&digest)),
        ErrorCode::UnsupportedBackend,
        false,
    );
    expect(
        "grant for another policy",
        refused(
            "grant for another policy",
            GrantSpec::new(column.spec.class, "refuse-policy", &digest, &document),
            column.policy(&format!("blake3-256:{}", "0".repeat(64))),
        ),
        ErrorCode::IdentityPolicyMismatch,
        false,
    );
    let mut expired = GrantSpec::new(column.spec.class, "refuse-expired", &digest, &document);
    expired.expires_at_unix_ms = 1;
    expect(
        "expired grant",
        refused("expired grant", expired, column.policy(&digest)),
        ErrorCode::InvalidSpec,
        false,
    );
    let mut egress = GrantSpec::new(column.spec.class, "refuse-egress", &digest, &document);
    egress.allowed_egress = vec!["example.com:443".into()];
    expect(
        "egress grant",
        refused("egress grant", egress, column.policy(&digest)),
        ErrorCode::UnsupportedBackend,
        false,
    );

    // A ceiling no tier enforces (`pids` needs a delegated cgroup, which
    // neither column has) is a rejection, not a no-op (ADR-0035 §4).
    let mut unenforceable = column.resolver(&guest_env("exit:0", None));
    unenforceable.limits.pids.limit = 64;
    let (spec, grant) = spec_and_grant(&GrantSpec::new(
        column.spec.class,
        "refuse-ceiling",
        &digest,
        &document,
    ));
    let error = column
        .service
        .start_authorized(&spec, &grant, &column.policy(&digest), &unenforceable)
        .expect_err("an unenforceable ceiling");
    expect(
        "unenforceable ceiling",
        error,
        ErrorCode::UnsupportedBackend,
        false,
    );
    assert_eq!(column.run_roots(), 0, "{name}: a refused ceiling");

    // A channel this tier cannot deliver is refused before the workload
    // runs, never dropped from the policy the worker applies.
    let undeliverable = (column.spec.undeliverable)(column.manifest(None))
        .expect("the undeliverable dimension is a valid confinement/v1 document");
    let undeliverable_document = undeliverable.to_canonical_json().expect("document");
    let undeliverable_digest = undeliverable.confinement_digest().expect("digest");
    let (spec, grant) = spec_and_grant(&GrantSpec::new(
        column.spec.class,
        "refuse-undeliverable",
        &undeliverable_digest,
        &undeliverable_document,
    ));
    match column.service.start_authorized(
        &spec,
        &grant,
        &column.policy(&undeliverable_digest),
        &resolver,
    ) {
        Err(error) if (column.spec.sandbox_refusal)(&error) => sandbox_unavailable(name, &error),
        Err(error) => {
            expect(
                column.spec.undeliverable_name,
                error,
                ErrorCode::InvalidSpec,
                false,
            );
        }
        Ok(record) => panic!(
            "{name}: {} was accepted and the run started: {record:?}",
            column.spec.undeliverable_name
        ),
    }
    assert_eq!(
        column.run_roots(),
        0,
        "{name}: a refused channel left its run root"
    );
    column.finish();
}

/// A guest that exits 0 ends `Succeeded`; its receipt binds the spec, grant
/// and attested policy; cleanup closes the lifecycle; and the same authority
/// started again names the same run instead of a second one.
pub fn row_a_guest_that_exits_zero_is_succeeded_with_a_bound_receipt(column: Column) {
    let name = column.spec.name;
    column.provision();
    let manifest = column.manifest(None);
    let Some(started) = column.start("succeed", &manifest, &guest_env("exit:0", None)) else {
        return;
    };
    assert_eq!(
        column.run_roots(),
        1,
        "{name}: a started run owns one run root"
    );
    assert_eq!(
        column.backend.settle(&started.run_id),
        BackendRunStatus::Succeeded,
        "{name}"
    );
    assert_eq!(
        column.run_roots(),
        0,
        "{name}: completion removes the run root"
    );
    assert_eq!(
        column
            .service
            .status(&started.run_id)
            .expect("status")
            .map(|run| run.state),
        Some(RunState::Succeeded),
        "{name}"
    );
    assert_eq!(
        states(&column, &started.run_id),
        lifecycle(RunState::Succeeded)
    );
    let receipt = column.service.collect(&started.run_id).expect("collect");
    assert_receipt(&column, &started, &receipt, RunState::Succeeded);
    // Collection is read-only: a second collect is the same receipt.
    assert_eq!(
        column
            .service
            .collect(&started.run_id)
            .expect("collect again"),
        receipt,
        "{name}"
    );

    let cleaned = column.service.cleanup(&started.run_id).expect("cleanup");
    assert_eq!(cleaned.state, RunState::Cleaned);
    let mut expected = lifecycle(RunState::Succeeded);
    expected.extend([RunState::Cleaning, RunState::Cleaned]);
    assert_eq!(states(&column, &started.run_id), expected, "{name}");
    assert_eq!(
        column
            .service
            .cleanup(&started.run_id)
            .expect("repeat cleanup")
            .state,
        RunState::Cleaned
    );

    // Replay: the same spec and grant again is the same run, not a second
    // guest.
    let replayed = column
        .service
        .start_authorized(
            &started.spec,
            &started.grant,
            &column.policy(&started.digest),
            &column.resolver(&guest_env("exit:0", None)),
        )
        .expect("replay");
    assert_eq!(replayed.run_id, started.run_id, "{name}");
    assert_eq!(replayed.state, RunState::Cleaned, "{name}");
    assert_eq!(column.run_roots(), 0, "{name}: a replay started nothing");
    column.finish();
}

/// A guest that exits non-zero ends `Failed`, and the terminal event carries
/// the BLAKE3 of the typed backend error rather than the error text.
pub fn row_a_guest_that_exits_nonzero_is_failed_with_a_content_addressed_detail(column: Column) {
    let name = column.spec.name;
    column.provision();
    let manifest = column.manifest(None);
    let Some(started) = column.start("fail", &manifest, &guest_env("exit:7", None)) else {
        return;
    };
    let error = match column.backend.settle(&started.run_id) {
        BackendRunStatus::Failed(error) => error,
        other => panic!("{name}: a guest exiting 7 ended {other:?}"),
    };
    assert_eq!(error.code, ErrorCode::BackendFailed, "{name}: {error}");
    assert_eq!(column.run_roots(), 0, "{name}");
    let inspection = column.service.inspect(&started.run_id, 0).expect("inspect");
    assert_eq!(inspection.state, RunState::Failed, "{name}");
    let terminal = inspection.events.last().expect("terminal event");
    assert_eq!(terminal.state, RunState::Failed);
    assert_eq!(
        terminal.detail_digest.as_deref(),
        Some(
            format!(
                "blake3-256:{}",
                blake3::hash(&serde_json::to_vec(&error).expect("encode error")).to_hex()
            )
            .as_str()
        ),
        "{name}: the failure detail is content-addressed"
    );
    assert!(
        inspection.events[..inspection.events.len() - 1]
            .iter()
            .all(|event| event.detail_digest.is_none()),
        "{name}: only the terminal failure carries a detail"
    );
    let receipt = column.service.collect(&started.run_id).expect("collect");
    assert_receipt(&column, &started, &receipt, RunState::Failed);
    column.finish();
}

/// A guest blocked forever is running when the cancel arrives; `cancel`
/// returns only after its worker, the guest and the run root are gone; the
/// receipt is `Cancelled`; a repeated cancel is idempotent.
pub fn row_cancel_ends_a_blocked_guest_and_everything_it_ran(mut column: Column) {
    let name = column.spec.name;
    column.provision();
    let channel = column.open_channel();
    let manifest = column.manifest(Some(&channel));
    let environment = guest_env("block", Some(&channel));
    let Some(started) = column.start("cancel", &manifest, &environment) else {
        return;
    };
    let mut accepted = channel.accept(&column, &started.run_id);
    let blocked = Blocked::read(&column, &mut accepted);
    assert!(alive(blocked.holder), "{name}: the worker is running");
    assert_eq!(
        column
            .service
            .status(&started.run_id)
            .expect("status")
            .map(|run| run.state),
        Some(RunState::Running),
        "{name}: a guest blocked forever is still running"
    );
    let early = column
        .service
        .collect(&started.run_id)
        .expect_err("a running run has no receipt");
    assert!(early.detail.contains("not terminal"), "{name}: {early}");

    let cancelled = column.service.cancel(&started.run_id).expect("cancel");
    assert_eq!(cancelled.state, RunState::Cancelled, "{name}");
    blocked.assert_gone(&column, "cancel");
    assert_eq!(column.run_roots(), 0, "{name}: cancel removes the run root");
    accepted.assert_closed();

    let receipt = column.service.collect(&started.run_id).expect("collect");
    assert_receipt(&column, &started, &receipt, RunState::Cancelled);
    assert_eq!(
        states(&column, &started.run_id),
        lifecycle(RunState::Cancelled)
    );
    assert_eq!(
        column
            .service
            .cancel(&started.run_id)
            .expect("repeat cancel")
            .state,
        RunState::Cancelled
    );
    assert_eq!(
        column
            .service
            .cleanup(&started.run_id)
            .expect("cleanup")
            .state,
        RunState::Cleaned
    );
    column.finish();
}

/// Crash recovery: SIGKILL the process holding a running guest. The
/// supervisor reaps it, kills what it spawned, removes the run root and
/// reports `Failed`; the lifecycle then collects and cleans as usual.
pub fn row_a_killed_worker_is_failed_reaped_and_cleaned(mut column: Column) {
    let name = column.spec.name;
    column.provision();
    let channel = column.open_channel();
    let manifest = column.manifest(Some(&channel));
    let environment = guest_env("block", Some(&channel));
    let Some(started) = column.start("crash", &manifest, &environment) else {
        return;
    };
    let mut accepted = channel.accept(&column, &started.run_id);
    let blocked = Blocked::read(&column, &mut accepted);
    // SAFETY: a signal to a process this test's backend spawned and owns.
    assert_eq!(unsafe { libc::kill(blocked.holder, libc::SIGKILL) }, 0);

    let error = match column.backend.settle(&started.run_id) {
        BackendRunStatus::Failed(error) => error,
        other => panic!("{name}: a killed worker ended {other:?}"),
    };
    assert_eq!(error.code, ErrorCode::BackendFailed, "{name}: {error}");
    blocked.assert_gone(&column, "its worker's crash");
    assert_eq!(column.run_roots(), 0, "{name}: a crash leaves no run root");
    accepted.assert_closed();
    assert_eq!(
        column
            .service
            .status(&started.run_id)
            .expect("status")
            .map(|run| run.state),
        Some(RunState::Failed),
        "{name}"
    );
    let receipt = column.service.collect(&started.run_id).expect("collect");
    assert_receipt(&column, &started, &receipt, RunState::Failed);
    assert_eq!(
        column
            .service
            .cancel(&started.run_id)
            .expect("cancel after crash")
            .state,
        RunState::Failed,
        "{name}: cancelling a crashed run is an idempotent no-op"
    );
    assert_eq!(
        column
            .service
            .cleanup(&started.run_id)
            .expect("cleanup")
            .state,
        RunState::Cleaned
    );
    column.finish();
}

/// One probe's report.
#[derive(Debug)]
pub struct Probe {
    pub name: String,
    pub ok: bool,
    pub errno: Option<i32>,
    pub detail: String,
}

/// Read `probe` lines up to `done`.
pub fn read_probes(accepted: &mut Accepted) -> Vec<Probe> {
    let mut probes = Vec::new();
    loop {
        let line = accepted.line();
        if line == "done" {
            return probes;
        }
        let mut fields = line.splitn(4, ' ');
        assert_eq!(
            fields.next(),
            Some("probe"),
            "unexpected guest line {line:?}"
        );
        let name = fields.next().expect("probe name").to_owned();
        let outcome = fields.next().expect("probe outcome");
        let rest = fields.next().unwrap_or_default();
        probes.push(match outcome {
            "ok" => Probe {
                name,
                ok: true,
                errno: None,
                detail: rest.to_owned(),
            },
            "err" => {
                let (errno, detail) = rest.split_once(' ').unwrap_or((rest, ""));
                Probe {
                    name,
                    ok: false,
                    errno: errno.parse().ok(),
                    detail: detail.to_owned(),
                }
            }
            other => panic!("unexpected probe outcome {other:?} in {line:?}"),
        });
    }
}

fn probe<'a>(probes: &'a [Probe], name: &str) -> &'a Probe {
    probes
        .iter()
        .find(|probe| probe.name == name)
        .unwrap_or_else(|| panic!("probe {name} never reported: {probes:#?}"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Denied file access, on the tier's own terms: the kernel refuses a visible
/// path (EPERM/EACCES, never ENOENT, which would prove nothing), or the path
/// is not in the workload's namespace at all (ENOENT).
fn assert_denied(column: &Column, probe: &Probe) {
    let name = column.spec.name;
    assert!(!probe.ok, "{name}: {} SUCCEEDED: {probe:?}", probe.name);
    assert!(
        !probe.detail.contains(SECRET),
        "{name}: {} leaked the decoy: {probe:?}",
        probe.name
    );
    match column.spec.denial {
        Denial::Kernel => assert!(
            matches!(probe.errno, Some(errno) if errno == libc::EPERM || errno == libc::EACCES),
            "{name}: {} must be a kernel denial (EPERM/EACCES), not an absence: {probe:?}",
            probe.name
        ),
        Denial::Absent => assert_eq!(
            probe.errno,
            Some(libc::ENOENT),
            "{name}: {} must not exist in the guest's namespace: {probe:?}",
            probe.name
        ),
    }
}

/// The workload reaches its own rootfs and its granted channel, and nothing
/// else: not an existing host file outside the grant, not the daemon's raw
/// SQLite arena or living database, not `~/.ssh`, and not an ungranted
/// loopback listener, which never sees a connection.
pub fn row_undeclared_host_access_and_the_raw_arena_are_unreachable(mut column: Column) {
    let name = column.spec.name;
    column.provision();
    let decoy = column.fixture.decoy("outside-the-grant/decoy-secret.txt");
    // The daemon's state as it lies on the host: the published arena and the
    // living database. A workload sees Graph content only through a granted
    // workspace share, never these files.
    let arena = column.fixture.decoy("daemon/graph.arena");
    let live_db = column.fixture.decoy("daemon/graph.live.db");
    let ungranted = TcpListener::bind("127.0.0.1:0").expect("ungranted listener");
    let ungranted_address = ungranted.local_addr().expect("ungranted address");
    let ssh = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".ssh"))
        .filter(|ssh| ssh.is_dir());

    let mut probes = vec![
        format!("granted_read=read:{GRANTED_FILE}"),
        format!(
            "granted_write=write:guest-wrote.txt:{}",
            hex(b"written-confined")
        ),
        format!("decoy_read=read:{}", decoy.display()),
        format!("arena_read=read:{}", arena.display()),
        format!("live_db_read=read:{}", live_db.display()),
        format!("ungranted_connect=connect:{ungranted_address}"),
    ];
    if let Some(ssh) = &ssh {
        probes.push(format!("ssh_list=list:{}", ssh.display()));
    }
    let channel = column.open_channel();
    let manifest = column.manifest(Some(&channel));
    let mut environment = guest_env("probe", Some(&channel));
    environment.push(("CONFORMANCE_PROBES", probes.join(";")));
    let Some(started) = column.start("probe", &manifest, &environment) else {
        return;
    };
    let mut accepted = channel.accept(&column, &started.run_id);
    let reports = read_probes(&mut accepted);
    assert_eq!(
        column.backend.settle(&started.run_id),
        BackendRunStatus::Succeeded,
        "{name}: the probing guest exits 0; reports: {reports:#?}"
    );

    let granted = probe(&reports, "granted_read");
    assert!(
        granted.ok && granted.detail == GRANTED_CONTENT,
        "{name}: the positive control must read its own rootfs: {granted:?}"
    );
    let written = probe(&reports, "granted_write");
    assert!(
        written.ok && written.detail == "written-confined",
        "{name}: the positive control must write its own rootfs: {written:?}"
    );
    for denied in ["decoy_read", "arena_read", "live_db_read"] {
        assert_denied(&column, probe(&reports, denied));
    }
    if ssh.is_some() {
        assert_denied(&column, probe(&reports, "ssh_list"));
    }
    let connect = probe(&reports, "ungranted_connect");
    assert!(
        !connect.ok,
        "{name}: an ungranted connect SUCCEEDED: {connect:?}"
    );
    if column.spec.denial == Denial::Kernel {
        assert!(
            matches!(connect.errno, Some(errno) if errno == libc::EPERM || errno == libc::EACCES),
            "{name}: an ungranted connect must be a kernel denial: {connect:?}"
        );
    }
    // The guest has exited, so a connection that got through is already
    // queued; a nonblocking accept reads the queue without waiting.
    ungranted.set_nonblocking(true).expect("nonblocking");
    match ungranted.accept() {
        Err(error) if error.kind() == ErrorKind::WouldBlock => {}
        Ok((_, peer)) => panic!("{name}: the guest reached an ungranted listener from {peer}"),
        Err(error) => panic!("accept on the ungranted listener: {error}"),
    }
    for path in [&decoy, &arena, &live_db] {
        assert_eq!(
            fs::read_to_string(path).expect("decoy after the run"),
            SECRET,
            "{name}: {} changed under confinement",
            path.display()
        );
    }
    column.finish();
}

/// A workspace grant is Graph authority. A backend with nothing to serve it
/// refuses the run before spawning anything, on every tier, rather than
/// running the workload without the workspace its receipt would claim.
pub fn row_a_workspace_grant_nothing_serves_is_refused_before_spawn(column: Column) {
    let name = column.spec.name;
    let request = workspace_request(&column, "unserved", workspace_grant(&"e".repeat(64)));
    let error = column
        .service
        .start(request)
        .expect_err("a workspace grant nothing serves must be refused");
    assert_eq!(error.code, ErrorCode::UnsupportedBackend, "{name}: {error}");
    assert!(
        error.detail.contains("repo"),
        "{name}: names the workspace: {error}"
    );
    assert_eq!(column.run_roots(), 0, "{name}: refused before spawn");
    assert_eq!(
        column
            .service
            .status("conformance-unserved")
            .expect("status"),
        None,
        "{name}: the refused run never existed"
    );
    column.finish();
}

pub const WORKSPACE: &str = "repo";

/// A read, list and mutate grant of the `repo` workspace at `graph_root`
/// (64 hex digits).
pub fn workspace_grant(graph_root: &str) -> WorkspaceGrant {
    WorkspaceGrant {
        name: WORKSPACE.into(),
        graph_root: format!("blake3-256:{graph_root}"),
        expected_generation: 0,
        operations: vec![
            leyline_runtime::WorkspaceOperation::Read,
            leyline_runtime::WorkspaceOperation::List,
            leyline_runtime::WorkspaceOperation::Mutate,
        ],
    }
}

/// A direct request for the column's guest, granted `grant`.
pub fn workspace_request(column: &Column, label: &str, grant: WorkspaceGrant) -> ExecutionRequest {
    ExecutionRequest {
        run_id: format!("conformance-{label}"),
        replay_key: format!("conformance-{label}-replay"),
        rootfs: DigestRef {
            algorithm: "blake3-256".into(),
            value: column.spec.rootfs.clone(),
        },
        executable: column.spec.executable.clone(),
        arguments: column.spec.arguments.clone(),
        public_environment: BTreeMap::from([("CONFORMANCE_MODE".to_owned(), "exit:0".to_owned())]),
        allowed_egress: Vec::new(),
        confinement_digest: String::new(),
        confinement_manifest: None,
        workspaces: vec![grant],
        limits: column.spec.limits,
    }
}

/// The bytes the store holds for a path, read on the host.
pub type StoredBytes = Box<dyn Fn(&str) -> Option<Vec<u8>>>;

/// What the Graph I/O row needs from a column that serves workspaces.
pub struct GraphFixture {
    pub grant: WorkspaceGrant,
    /// A path the share holds, relative to the guest's `/work`, and its
    /// bytes.
    pub read: (&'static str, &'static str),
    /// The bytes the guest writes over `read.0`.
    pub write: &'static str,
    /// The host path of the store's living database.
    pub live_db: PathBuf,
    pub stored: StoredBytes,
}

/// Graph I/O: a guest granted a workspace reads the store's bytes through
/// the mediated share, lists it, writes through it, and the write lands in
/// the store on the host. The share is the only Graph surface: the store's
/// living database is not in the guest's namespace and not in the listing.
pub fn row_graph_io_goes_through_the_mediated_workspace_share(
    mut column: Column,
    graph: GraphFixture,
) {
    let name = column.spec.name;
    let channel = column.open_channel();
    let manifest = column.manifest(Some(&channel));
    let (path, expected) = graph.read;
    let probes = [
        format!("graph_read=read:{path}"),
        "graph_list=list:.".to_owned(),
        format!("graph_write=write:{path}:{}", hex(graph.write.as_bytes())),
        format!("live_db_read=read:{}", graph.live_db.display()),
    ];
    let mut request = workspace_request(&column, "graph", graph.grant.clone());
    request.public_environment = BTreeMap::from([
        ("CONFORMANCE_MODE".to_owned(), "probe".to_owned()),
        ("CONFORMANCE_DIAL".to_owned(), channel.dial.clone()),
        ("CONFORMANCE_PROBES".to_owned(), probes.join(";")),
    ]);
    request.confinement_manifest = Some(manifest.to_canonical_json().expect("document"));
    request.confinement_digest = manifest.confinement_digest().expect("digest");
    let record = column.service.start(request).expect("start a Graph guest");
    let mut accepted = channel.accept(&column, &record.run_id);
    let reports = read_probes(&mut accepted);
    assert_eq!(
        column.backend.settle(&record.run_id),
        BackendRunStatus::Succeeded,
        "{name}: the Graph guest exits 0; reports: {reports:#?}"
    );

    let read = probe(&reports, "graph_read");
    assert!(
        read.ok && read.detail == expected.replace('\n', "\\n"),
        "{name}: the guest reads the store's bytes through the share: {read:?}"
    );
    let listing = probe(&reports, "graph_list");
    assert!(listing.ok, "{name}: {listing:?}");
    let names: Vec<&str> = listing.detail.split(',').collect();
    assert!(
        names.contains(&path),
        "{name}: the share lists {path}: {listing:?}"
    );
    assert!(
        !names
            .iter()
            .any(|entry| entry.ends_with(".db") || entry.ends_with(".arena")),
        "{name}: the share exposes no database or arena file: {listing:?}"
    );
    let written = probe(&reports, "graph_write");
    assert!(
        written.ok && written.detail == graph.write.replace('\n', "\\n"),
        "{name}: the guest writes through the share: {written:?}"
    );
    let raw = probe(&reports, "live_db_read");
    assert!(
        !raw.ok,
        "{name}: the guest read the store's database: {raw:?}"
    );
    assert_eq!(
        raw.errno,
        Some(libc::ENOENT),
        "{name}: the store's database is not in the guest's namespace: {raw:?}"
    );
    assert_eq!(
        (graph.stored)(path).as_deref(),
        Some(graph.write.as_bytes()),
        "{name}: the guest's write landed in the store"
    );
    assert_eq!(column.run_roots(), 0, "{name}");
    column.finish();
}
