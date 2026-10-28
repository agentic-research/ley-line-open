//! A real guest, started, cancelled and collected through execution/v1 on the
//! libkrun backend (bead `ley-line-open-f839c1`).
//!
//! The backend's lifecycle is otherwise proven only against fake workers
//! (`runtime/tests/krun_worker_backend.rs`): shell scripts that speak the
//! worker protocol and never enter a hypervisor. This test runs the same path
//! with the worker a user runs — `ExecutionService::start_authorized` over
//! `KrunWorkerBackend`, the real `leyline-krun-worker`, a booted guest — and
//! checks the four things cancellation promises:
//!
//!   1. the guest was running, and blocked, when the cancel arrived;
//!   2. `cancel` ends the run in `Cancelled`;
//!   3. `collect` then hands back a receipt whose terminal state is
//!      `Cancelled`;
//!   4. the VMM process (and its process group) is gone, and the run's
//!      ephemeral rootfs is removed.
//!
//! ## Synchronization is by event, never by sleep
//!
//! The backend reports readiness BEFORE the guest runs, so "start returned"
//! says nothing about a booted guest. The guest proves it is up by dialing
//! the host: the run's grant carries a `confinement/v1` §6 `connect` grant on a
//! UNIX socket this test binds, which the microVM tier delivers as a vsock
//! port muxed to that socket (`vsock_unix_mappings`). The guest connects,
//! writes one line, and blocks forever. The host's `accept` returning, and that
//! line arriving, is the event the cancel waits for.
//!
//! The same connection identifies the VMM: its peer is the process holding the
//! host end, which is the worker (libkrun's VMM runs in-process). After
//! `cancel` returns, that pid must no longer exist and the connection must read
//! end of stream — both observed without waiting on anything but the kernel.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use capnp::message::Builder;
use leyline_public_schema::execution_capnp;
use leyline_runtime::authorization::{
    AuthorizationPolicy, AuthorizedExecution, EXECUTION_CAPABILITY, EXECUTION_SCHEMA_VERSION,
    canonical_digest,
};
use leyline_runtime::backends::libkrun::backend::{KrunWorkerBackend, KrunWorkerConfig};
use leyline_runtime::backends::libkrun::confinement::{VSOCK_UNIX_BASE_PORT, confinement_manifest};
use leyline_runtime::confinement::UnixSocketGrant;
use leyline_runtime::{
    BackendClass, DigestRef, ExecutionError, ExecutionRequest, ExecutionResolver, ExecutionService,
    ResourceLimits, RunState,
};
use tempfile::TempDir;

#[path = "../../runtime/tests/common/mod.rs"]
mod common;
mod hypervisor;

/// The line the guest writes once it is up, before it blocks.
const BLOCKED: &str = "guest-blocked";

/// Only a bound on a failure: a guest that dials is answered at once, and one
/// that never does fails the test here instead of hanging it.
const DIAL_BOUND: Duration = Duration::from_secs(60);

/// The guest dials grant 0's dial port (`VSOCK_UNIX_BASE_PORT + 2 * 0`), CID 2
/// (the host), writes [`BLOCKED`], and then never exits on its own.
fn guest_source() -> String {
    format!(
        r#"use std::ffi::c_int;

#[repr(C)]
struct SockaddrVm {{
    svm_family: u16,
    svm_reserved1: u16,
    svm_port: u32,
    svm_cid: u32,
    svm_zero: [u8; 4],
}}

extern "C" {{
    fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    fn connect(fd: c_int, address: *const SockaddrVm, length: u32) -> c_int;
    fn write(fd: c_int, buffer: *const u8, count: usize) -> isize;
}}

const AF_VSOCK: c_int = 40;
const SOCK_STREAM: c_int = 1;
const VMADDR_CID_HOST: u32 = 2;

fn main() {{
    println!("cancel-guest-start");
    // SAFETY: plain syscalls on a descriptor this process owns, with a live
    // address of the size passed.
    let fd = unsafe {{ socket(AF_VSOCK, SOCK_STREAM, 0) }};
    if fd < 0 {{
        println!("cancel-guest: socket failed: {{}}", std::io::Error::last_os_error());
        std::process::exit(2);
    }}
    let address = SockaddrVm {{
        svm_family: AF_VSOCK as u16,
        svm_reserved1: 0,
        svm_port: {port},
        svm_cid: VMADDR_CID_HOST,
        svm_zero: [0; 4],
    }};
    let rc = unsafe {{ connect(fd, &address, std::mem::size_of::<SockaddrVm>() as u32) }};
    if rc != 0 {{
        println!("cancel-guest: connect failed: {{}}", std::io::Error::last_os_error());
        std::process::exit(3);
    }}
    let line = b"{BLOCKED}\n";
    let written = unsafe {{ write(fd, line.as_ptr(), line.len()) }};
    if written != line.len() as isize {{
        println!("cancel-guest: write failed: {{}}", std::io::Error::last_os_error());
        std::process::exit(4);
    }}
    // Blocked until the host ends the VM. Nothing here ever returns.
    loop {{
        std::thread::park();
    }}
}}"#,
        port = VSOCK_UNIX_BASE_PORT,
    )
}

/// Maps every authorized run to the one rootfs this test published, carrying
/// every grant-owned field through unchanged (the service refuses a resolver
/// that drops or alters them).
struct FixtureResolver {
    rootfs: String,
}

impl ExecutionResolver for FixtureResolver {
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
            executable: "bin/guest".into(),
            arguments: vec!["guest".into()],
            public_environment: BTreeMap::new(),
            allowed_egress: authorized.allowed_egress.clone(),
            confinement_digest: authorized.confinement_digest.clone(),
            confinement_manifest: authorized.confinement_manifest.clone(),
            workspaces: authorized.workspaces.clone(),
            limits: ResourceLimits {
                // Far beyond the test: the run must end by cancellation, and a
                // wall-clock kill would also remove the run root.
                wall_time_ms: 600_000,
                memory: leyline_runtime::MemoryLimits { limit: 256 << 20 },
                cpu: leyline_runtime::CpuLimits {
                    quota: 100_000,
                    period: 100_000,
                },
                ..ResourceLimits::default()
            },
        })
    }
}

#[test]
fn a_real_guest_is_cancelled_and_collected_with_a_cancelled_receipt() {
    let _serial = common::serial();
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    let fixture = TempDir::new().expect("fixture");
    let root = fixture.path().canonicalize().expect("canonical fixture");

    let rootfs = publish_guest(&root, &guest_source());
    let worker = hypervisor::worker_under_test(
        Path::new(env!("CARGO_BIN_EXE_leyline-krun-worker")),
        &root.join("leyline-krun-worker"),
    );

    // The backend spawns the worker with this process's environment, as the
    // execution daemon does; the worker's libkrun loads libkrunfw by name.
    let (library_var, library_dir) = hypervisor::library_path_env(&libkrunfw);
    // SAFETY: this binary holds one test, serialized above, and nothing else
    // in it reads the environment concurrently.
    unsafe { std::env::set_var(library_var, library_dir) };

    // The host end of the guest's dial. It must exist before the run starts:
    // a §6 connect grant names an endpoint someone else already bound.
    let socket = root.join("g.sock");
    let listener = UnixListener::bind(&socket).expect("bind the guest's dial socket");

    let runtime_files = vec![libkrunfw.clone()];
    let manifest = confinement_manifest(&runtime_files, &[], None)
        .and_then(|base| {
            base.with_unix_socket(UnixSocketGrant::connect(socket.display().to_string()))
        })
        .expect("the run's confinement document");
    let document = manifest.to_canonical_json().expect("canonical document");
    let digest = manifest.confinement_digest().expect("confinement digest");

    let cas_root = root.join("cas");
    let ephemeral_root = root.join("runs");
    fs::create_dir(&ephemeral_root).expect("ephemeral root");
    let service = ExecutionService::new(KrunWorkerBackend::new(KrunWorkerConfig {
        worker,
        cas_root,
        ephemeral_root: ephemeral_root.clone(),
        runtime_files,
        devices: Vec::new(),
        ready_timeout: Duration::from_secs(30),
        tsi_hijack_inet: false,
    }));
    let capabilities = service
        .provision(BackendClass::MicroVm, "provision-cancel")
        .expect("the installed worker can enter the hypervisor");
    assert_eq!(capabilities.backend_id, "libkrun/1");

    let (spec, grant) = spec_and_grant(&digest, &document);
    let record = service
        .start_authorized(
            &spec,
            &grant,
            &AuthorizationPolicy {
                now_unix_ms: None,
                required_backend: BackendClass::MicroVm,
                required_confinement_digest: Some(digest.clone()),
            },
            &FixtureResolver { rootfs },
        )
        .expect("start a real guest through execution/v1");
    assert_eq!(record.state, RunState::Running);
    assert_eq!(record.backend_id, "libkrun/1");
    assert_eq!(
        run_roots(&ephemeral_root),
        1,
        "a started run owns exactly one ephemeral rootfs"
    );

    // EVENT 1: the guest dials the host and says it is blocked.
    let (accepted_tx, accepted_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = accepted_tx.send(listener.accept().map(|(stream, _)| stream));
    });
    let stream = match accepted_rx.recv_timeout(DIAL_BOUND) {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => panic!("accept on the guest's dial socket failed: {error}"),
        Err(_) => panic!(
            "FAILED: the guest did not dial the host within {DIAL_BOUND:?}; run state: {:?}",
            service.status(&record.run_id)
        ),
    };
    let vmm = peer_pid(&stream);
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read the guest's line");
    assert_eq!(line.trim_end(), BLOCKED, "the guest's first line");
    assert!(
        alive(vmm),
        "the VMM {vmm} holding the guest's channel is running"
    );
    assert_eq!(
        service
            .status(&record.run_id)
            .expect("status")
            .map(|run| run.state),
        Some(RunState::Running),
        "a guest blocked forever is still running when the cancel arrives"
    );

    // Not collectable while it runs: the receipt is terminal evidence.
    let early = service
        .collect(&record.run_id)
        .expect_err("a running run has no receipt");
    assert!(early.detail.contains("not terminal"), "{early}");

    let cancelled = service.cancel(&record.run_id).expect("cancel the guest");
    assert_eq!(cancelled.state, RunState::Cancelled);

    // EVENT 2: `cancel` returns only after the supervisor reaped the worker
    // and removed the run root, so all of this holds the moment it returns.
    assert!(
        !alive(vmm),
        "the VMM process {vmm} survived the cancel of its run"
    );
    assert!(
        !group_alive(vmm),
        "a process in the VMM's group {vmm} survived the cancel"
    );
    assert_eq!(
        run_roots(&ephemeral_root),
        0,
        "the cancelled run's ephemeral rootfs was not removed"
    );
    // The VMM held the host end of the guest's channel; with it gone the
    // channel ends. Either reading is the kernel saying the peer is closed.
    let mut rest = Vec::new();
    match reader.read_to_end(&mut rest) {
        Ok(_) => assert!(rest.is_empty(), "the guest wrote after blocking: {rest:?}"),
        Err(error) => assert_eq!(
            error.kind(),
            std::io::ErrorKind::ConnectionReset,
            "the guest's channel did not end with its VMM: {error}"
        ),
    }

    let receipt = service
        .collect(&record.run_id)
        .expect("collect the cancelled run's receipt");
    assert_eq!(receipt.run_id, record.run_id);
    assert_eq!(receipt.terminal_state, RunState::Cancelled);
    assert_eq!(receipt.backend_id, "libkrun/1");
    assert_eq!(receipt.context.confinement_digest, digest);
    assert_eq!(receipt.context.backend_class, BackendClass::MicroVm);
    assert!(receipt.event_log_root.starts_with("blake3-256:"));
    let states: Vec<RunState> = service
        .inspect(&record.run_id, 0)
        .expect("inspect")
        .events
        .into_iter()
        .map(|event| event.state)
        .collect();
    assert_eq!(
        states,
        [
            RunState::Accepted,
            RunState::Provisioning,
            RunState::Ready,
            RunState::Running,
            RunState::Cancelled,
        ],
        "the run's event log"
    );

    // A repeated cancel is idempotent on a terminal run, and cleanup closes
    // the lifecycle without a worker to stop.
    assert_eq!(
        service.cancel(&record.run_id).expect("repeat cancel").state,
        RunState::Cancelled
    );
    assert_eq!(
        service.cleanup(&record.run_id).expect("cleanup").state,
        RunState::Cleaned
    );
    assert_eq!(run_roots(&ephemeral_root), 0);
}

/// Compile the guest, publish it as a pinned CAS rootfs under `root/cas`, and
/// return the rootfs digest.
fn publish_guest(root: &Path, source: &str) -> String {
    let source_path = root.join("guest.rs");
    fs::write(&source_path, source).expect("guest source");
    let staged = root.join("guest");
    hypervisor::compile_guest(&source_path, &staged);
    let bytes = fs::read(&staged).expect("guest bytes");
    let manifest = format!(
        "{{\"version\":1,\"files\":[{{\"path\":\"bin/guest\",\"mode\":493,\"blake3\":\"{}\"}}]}}",
        blake3::hash(&bytes).to_hex()
    );
    let digest = blake3::hash(manifest.as_bytes()).to_hex().to_string();
    let immutable = root.join("cas").join(&digest);
    fs::create_dir_all(immutable.join("bin")).expect("CAS rootfs");
    fs::write(immutable.join("bin/guest"), &bytes).expect("install guest");
    fs::set_permissions(
        immutable.join("bin/guest"),
        fs::Permissions::from_mode(0o755),
    )
    .expect("guest mode");
    fs::write(immutable.join("rootfs.manifest.json"), manifest).expect("rootfs manifest");
    digest
}

/// A RunSpec and a RunGrant authorizing it, on the microVM tier, committing to
/// the confinement document `document` (whose digest is `digest`).
fn spec_and_grant(digest: &str, document: &str) -> (Vec<u8>, Vec<u8>) {
    let mut message = Builder::new_default();
    let mut spec = message.init_root::<execution_capnp::run_spec::Builder<'_>>();
    spec.set_schema_version(EXECUTION_SCHEMA_VERSION);
    let mut executable = spec.init_executable();
    executable.set_media_type("application/vnd.leyline.executable");
    set_digest(executable.init_digest(), &"c".repeat(64));
    let mut spec_bytes = Vec::new();
    capnp::serialize::write_message(&mut spec_bytes, &message).expect("serialize spec");

    let spec_digest = canonical_digest(&spec_bytes).expect("spec digest");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64;
    let mut message = Builder::new_default();
    let mut grant = message.init_root::<execution_capnp::run_grant::Builder<'_>>();
    grant.set_grant_id("grant-cancel");
    grant.set_expires_at_unix_ms(now + 3_600_000);
    grant.set_replay_key(format!("cancel-{now}"));
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
        digest.strip_prefix("blake3-256:").expect("digest prefix"),
    );
    grant.set_confinement_manifest(document);
    grant.set_backend_class(execution_capnp::BackendClass::MicroVm);
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

fn run_roots(path: &Path) -> usize {
    fs::read_dir(path).expect("enumerate run roots").count()
}

/// Whether a process with this pid exists. `kill(pid, 0)` delivers nothing; it
/// only asks the kernel.
fn alive(pid: libc::pid_t) -> bool {
    signal_zero(unsafe { libc::kill(pid, 0) })
}

/// Whether any process remains in the process group `pgid`. The backend makes
/// the worker a group leader, so its pid names the group.
fn group_alive(pgid: libc::pid_t) -> bool {
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
fn peer_pid(stream: &UnixStream) -> libc::pid_t {
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
