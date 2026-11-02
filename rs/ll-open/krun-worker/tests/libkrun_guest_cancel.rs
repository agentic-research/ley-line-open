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
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use leyline_runtime::authorization::AuthorizationPolicy;
use leyline_runtime::backends::libkrun::backend::{KrunWorkerBackend, KrunWorkerConfig};
use leyline_runtime::backends::libkrun::confinement::{VSOCK_UNIX_BASE_PORT, confinement_manifest};
use leyline_runtime::confinement::UnixSocketGrant;
use leyline_runtime::{BackendClass, ExecutionService, ResourceLimits, RunState};
use tempfile::TempDir;

#[path = "../../runtime/tests/common/mod.rs"]
mod common;
// The execution/v1 fixtures and kernel probes this test shares with the
// conformance matrix (bead ley-line-open-f8ebcf).
#[path = "../../runtime/tests/conformance/mod.rs"]
mod conformance;
mod hypervisor;

use conformance::{GrantSpec, GuestResolver, alive, group_alive, peer_pid, run_roots};

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
fn fixture_resolver(rootfs: String) -> GuestResolver {
    GuestResolver {
        rootfs,
        executable: "bin/guest".into(),
        arguments: vec!["guest".into()],
        environment: BTreeMap::new(),
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
            &fixture_resolver(rootfs),
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
    conformance::publish_rootfs(&root.join("cas"), &[("bin/guest", &bytes, 0o755)])
}

/// A RunSpec and a RunGrant authorizing it, on the microVM tier, committing to
/// the confinement document `document` (whose digest is `digest`).
fn spec_and_grant(digest: &str, document: &str) -> (Vec<u8>, Vec<u8>) {
    let mut authority = GrantSpec::new(
        BackendClass::MicroVm,
        &format!("cancel-{}", conformance::now_unix_ms()),
        digest,
        document,
    );
    authority.grant_id = "grant-cancel".into();
    conformance::spec_and_grant(&authority)
}
