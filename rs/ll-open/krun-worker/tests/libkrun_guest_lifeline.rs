//! A real guest's VMM ends when the process embedding its backend is killed
//! with `SIGKILL` (bead `ley-line-open-f963e6`).
//!
//! The embedding process here is this test binary run again as
//! [`lifeline_embedder`]: it builds an `ExecutionService` over
//! `KrunWorkerBackend`, starts one guest through execution/v1 with the worker
//! a user runs, and supervises it until it is killed. Nothing in it cancels
//! anything, and `SIGKILL` runs no destructor: the only thing that can end
//! the worker is its lifeline, whose only write end died with the embedder.
//!
//! ## Synchronization is by event, never by sleep
//!
//! As in `libkrun_guest_cancel.rs`, the guest proves it is up by dialing a
//! UNIX socket this test bound (a `confinement/v1` §6 `connect` grant, which
//! the microVM tier delivers over vsock), writing one line and blocking
//! forever. The accepted connection's peer is the VMM (libkrun runs the guest
//! in-process). After the kill, the VMM's exit is the kernel's exit event for
//! that pid (`common::await_exit`), bounded only to fail by name.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
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
#[path = "../../runtime/tests/conformance/mod.rs"]
mod conformance;
mod hypervisor;

use conformance::{GrantSpec, GuestResolver, alive, peer_pid};

/// The line the guest writes once it is up, before it blocks.
const BLOCKED: &str = "guest-blocked";

/// Only bounds on a failure: a guest that dials is answered at once, and a
/// VMM whose lifeline ended exits at once.
const DIAL_BOUND: Duration = Duration::from_secs(60);
const EXIT_BOUND: Duration = Duration::from_secs(30);

/// Set in the embedding process: the fixture root. Beside it, the paths the
/// embedder needs from the test.
const EMBEDDER_ROOT: &str = "LEYLINE_TEST_GUEST_LIFELINE_ROOT";
const EMBEDDER_WORKER: &str = "LEYLINE_TEST_GUEST_LIFELINE_WORKER";
const EMBEDDER_LIBKRUNFW: &str = "LEYLINE_TEST_GUEST_LIFELINE_LIBKRUNFW";
const EMBEDDER_ROOTFS: &str = "LEYLINE_TEST_GUEST_LIFELINE_ROOTFS";

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

fn main() {{
    // SAFETY: plain syscalls on a descriptor this process owns, with a live
    // address of the size passed.
    let fd = unsafe {{ socket(40, 1, 0) }};
    if fd < 0 {{
        println!("lifeline-guest: socket failed: {{}}", std::io::Error::last_os_error());
        std::process::exit(2);
    }}
    let address = SockaddrVm {{
        svm_family: 40,
        svm_reserved1: 0,
        svm_port: {port},
        svm_cid: 2,
        svm_zero: [0; 4],
    }};
    if unsafe {{ connect(fd, &address, std::mem::size_of::<SockaddrVm>() as u32) }} != 0 {{
        println!("lifeline-guest: connect failed: {{}}", std::io::Error::last_os_error());
        std::process::exit(3);
    }}
    let line = b"{BLOCKED}\n";
    if unsafe {{ write(fd, line.as_ptr(), line.len()) }} != line.len() as isize {{
        println!("lifeline-guest: write failed: {{}}", std::io::Error::last_os_error());
        std::process::exit(4);
    }}
    loop {{
        std::thread::park();
    }}
}}"#,
        port = VSOCK_UNIX_BASE_PORT,
    )
}

/// The embedding process: starts the guest under `KrunWorkerBackend`, then
/// supervises it until it is killed. Not a test on its own.
#[test]
#[ignore = "the embedding process of a_guest_ends_when_the_process_embedding_its_backend_is_killed"]
fn lifeline_embedder() {
    let Some(root) = std::env::var_os(EMBEDDER_ROOT).map(PathBuf::from) else {
        return;
    };
    let path = |name: &str| PathBuf::from(std::env::var_os(name).expect(name));
    let worker = path(EMBEDDER_WORKER);
    let libkrunfw = path(EMBEDDER_LIBKRUNFW);
    let rootfs = std::env::var(EMBEDDER_ROOTFS).expect(EMBEDDER_ROOTFS);

    let runtime_files = vec![libkrunfw];
    let manifest = confinement_manifest(&runtime_files, &[], None)
        .and_then(|base| {
            base.with_unix_socket(UnixSocketGrant::connect(
                root.join("g.sock").display().to_string(),
            ))
        })
        .expect("the run's confinement document");
    let document = manifest.to_canonical_json().expect("canonical document");
    let digest = manifest.confinement_digest().expect("confinement digest");
    let service = ExecutionService::new(KrunWorkerBackend::new(KrunWorkerConfig {
        worker,
        cas_root: root.join("cas"),
        ephemeral_root: root.join("runs"),
        runtime_files,
        devices: Vec::new(),
        ready_timeout: Duration::from_secs(30),
        tsi_hijack_inet: false,
    }));
    service
        .provision(BackendClass::MicroVm, "provision-lifeline")
        .expect("the worker can enter the hypervisor");
    let mut authority = GrantSpec::new(
        BackendClass::MicroVm,
        &format!("lifeline-{}", conformance::now_unix_ms()),
        &digest,
        &document,
    );
    authority.grant_id = "grant-lifeline".into();
    let (spec, grant) = conformance::spec_and_grant(&authority);
    let record = service
        .start_authorized(
            &spec,
            &grant,
            &AuthorizationPolicy {
                now_unix_ms: None,
                required_backend: BackendClass::MicroVm,
                required_confinement_digest: Some(digest),
            },
            &GuestResolver {
                rootfs,
                executable: "bin/guest".into(),
                arguments: vec!["guest".into()],
                environment: BTreeMap::new(),
                limits: ResourceLimits {
                    // Far beyond the test: only the lifeline may end the run.
                    wall_time_ms: 600_000,
                    memory: leyline_runtime::MemoryLimits { limit: 256 << 20 },
                    cpu: leyline_runtime::CpuLimits {
                        quota: 100_000,
                        period: 100_000,
                    },
                    ..ResourceLimits::default()
                },
            },
        )
        .expect("start a real guest through execution/v1");
    assert_eq!(record.state, RunState::Running);
    // Supervising, until this process is killed.
    loop {
        std::thread::park();
    }
}

#[test]
fn a_guest_ends_when_the_process_embedding_its_backend_is_killed() {
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
    fs::create_dir(root.join("runs")).expect("ephemeral root");
    // The host end of the guest's dial, bound before the run starts.
    let listener = UnixListener::bind(root.join("g.sock")).expect("bind the guest's dial socket");

    let (library_var, library_dir) = hypervisor::library_path_env(&libkrunfw);
    let mut embedder = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "lifeline_embedder",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(EMBEDDER_ROOT, &root)
        .env(EMBEDDER_WORKER, &worker)
        .env(EMBEDDER_LIBKRUNFW, &libkrunfw)
        .env(EMBEDDER_ROOTFS, &rootfs)
        // The worker loads libkrunfw by name, from the embedder's environment.
        .env(library_var, library_dir)
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn the embedding process");

    // EVENT 1: the guest dials the host and says it is blocked.
    let (accepted_tx, accepted_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = accepted_tx.send(listener.accept().map(|(stream, _)| stream));
    });
    let stream = match accepted_rx.recv_timeout(DIAL_BOUND) {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => panic!("accept on the guest's dial socket failed: {error}"),
        Err(_) => {
            let _ = embedder.kill();
            let _ = embedder.wait();
            panic!("TIMEOUT: the guest did not dial the host within {DIAL_BOUND:?}")
        }
    };
    let vmm = peer_pid(&stream);
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read the guest's line");
    assert_eq!(line.trim_end(), BLOCKED, "the guest's first line");
    assert!(alive(vmm), "the VMM {vmm} holding the guest's channel runs");
    assert_ne!(
        vmm as u32,
        embedder.id(),
        "the VMM is the embedder's worker, not the embedder"
    );

    // SAFETY: a signal to the child this test spawned.
    assert_eq!(
        unsafe { libc::kill(embedder.id() as libc::pid_t, libc::SIGKILL) },
        0
    );
    embedder.wait().expect("reap the embedder");

    // EVENT 2: the orphaned VMM exits.
    if let Err(error) = common::await_exit(vmm, EXIT_BOUND) {
        // SAFETY: a signal to the leftover worker, so a failure leaves no VM.
        unsafe { libc::kill(vmm, libc::SIGKILL) };
        panic!("the VMM outlived the process embedding its backend: {error}");
    }
    // The VMM held the host end of the guest's channel; with it gone the
    // channel ends.
    let mut rest = Vec::new();
    match reader.read_to_end(&mut rest) {
        Ok(_) => assert!(rest.is_empty(), "the guest wrote after blocking: {rest:?}"),
        Err(error) => assert_eq!(
            error.kind(),
            std::io::ErrorKind::ConnectionReset,
            "the guest's channel did not end with its VMM: {error}"
        ),
    }
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
