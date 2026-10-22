use std::ffi::{CStr, CString};
use std::sync::{Arc, Mutex};

use leyline_krun_worker::api::{
    DynFileSystem, KrunApi, LinkedKrunApi, VIRTIOFS_TAG_MAX, check_virtiofs_tag, prepare_vm,
};
use leyline_runtime::backends::libkrun::confinement::{VSOCK_UNIX_BASE_PORT, VsockUnixMapping};
use leyline_runtime::backends::libkrun::plan::{KrunConfig, ResolvedRootfs};
use leyline_runtime::{DigestRef, ErrorCode, ExecutionError};
use tempfile::TempDir;

mod memfs;

#[derive(Default)]
struct RecordingApi {
    calls: Mutex<Vec<String>>,
    fail_at: Option<&'static str>,
}

impl RecordingApi {
    fn record(&self, call: impl Into<String>) -> Result<(), ExecutionError> {
        let call = call.into();
        self.calls.lock().expect("calls lock").push(call.clone());
        if self.fail_at == Some(call.split(':').next().expect("call name")) {
            return Err(ExecutionError {
                code: ErrorCode::BackendFailed,
                retryable: false,
                detail: format!("injected {call} failure"),
            });
        }
        Ok(())
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls lock").clone()
    }
}

impl KrunApi for RecordingApi {
    fn create_context(&self) -> Result<u32, ExecutionError> {
        self.record("create")?;
        Ok(42)
    }

    fn free_context(&self, context: u32) {
        self.calls
            .lock()
            .expect("calls lock")
            .push(format!("free:{context}"));
    }

    fn set_vm_config(&self, context: u32, vcpus: u8, ram_mib: u32) -> Result<(), ExecutionError> {
        self.record(format!("vm:{context}:{vcpus}:{ram_mib}"))
    }

    fn add_rootfs(
        &self,
        context: u32,
        rootfs: &CStr,
        read_only: bool,
    ) -> Result<(), ExecutionError> {
        self.record(format!(
            "root:{context}:{read_only}:{}",
            rootfs.to_string_lossy()
        ))
    }

    fn add_virtiofs_backend(
        &self,
        context: u32,
        tag: &str,
        dax_window: u64,
        _backend: Arc<dyn DynFileSystem>,
    ) -> Result<(), ExecutionError> {
        self.record(format!("virtiofs:{context}:{tag}:{dax_window}"))
    }

    fn disable_implicit_vsock(&self, context: u32) -> Result<(), ExecutionError> {
        self.record(format!("no-vsock:{context}"))
    }

    fn add_vsock(&self, context: u32, tsi_features: u32) -> Result<(), ExecutionError> {
        self.record(format!("vsock:{context}:tsi={tsi_features}"))
    }

    fn add_vsock_port(
        &self,
        context: u32,
        port: u32,
        host_path: &CStr,
        listen: bool,
    ) -> Result<(), ExecutionError> {
        self.record(format!(
            "vsock-port:{context}:{port}:{}:listen={listen}",
            host_path.to_string_lossy()
        ))
    }

    fn set_port_map(&self, context: u32, entries: &[CString]) -> Result<(), ExecutionError> {
        // Recorded with its arity because the empty case is the load-bearing
        // one: libkrun reads "never called" as expose-everything and "called
        // with an empty array" as expose-nothing, so a test must be able to
        // tell those apart. `port-map:<ctx>:` with nothing after it is the
        // closed default; absence of this record entirely is the open one.
        self.record(format!(
            "port-map:{context}:{}",
            entries
                .iter()
                .map(|entry| entry.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(",")
        ))
    }

    fn set_workdir(&self, context: u32, workdir: &CStr) -> Result<(), ExecutionError> {
        self.record(format!("workdir:{context}:{}", workdir.to_string_lossy()))
    }

    fn set_exec(
        &self,
        context: u32,
        executable: &CStr,
        arguments: &[std::ffi::CString],
        environment: &[std::ffi::CString],
    ) -> Result<(), ExecutionError> {
        self.record(format!(
            "exec:{context}:{}:{}:{}",
            executable.to_string_lossy(),
            arguments.len(),
            environment.len()
        ))
    }

    fn start_enter(&self, _context: u32) -> Result<(), ExecutionError> {
        panic!("unit tests must not enter the VMM")
    }
}

fn config(rootfs: &TempDir) -> KrunConfig {
    KrunConfig {
        run_id: "run-api-01".into(),
        rootfs: ResolvedRootfs {
            digest: DigestRef {
                algorithm: "blake3-256".into(),
                value: "a".repeat(64),
            },
            canonical_path: rootfs.path().canonicalize().expect("canonical rootfs"),
        },
        executable: std::ffi::CString::new("usr/bin/probe").expect("exec"),
        arguments: vec![std::ffi::CString::new("probe").expect("arg")],
        environment: vec![std::ffi::CString::new("CI=true").expect("env")],
        workdir: std::ffi::CString::new("/").expect("workdir"),
        vcpus: 2,
        ram_mib: 1024,
        wall_time_ms: 1_000,
        tsi_features: 0,
        port_map: Vec::new(),
        vsock_unix_map: Vec::new(),
    }
}

#[test]
fn prepares_a_writable_ephemeral_networkless_vm_without_entering_it() {
    // Catches accidental TSI/network exposure and regression to a read-only
    // guest mount at the C API mapping seam.
    let rootfs = TempDir::new().expect("rootfs");
    let api = RecordingApi::default();
    let canonical_rootfs = rootfs.path().canonicalize().expect("canonical rootfs");

    let vm = prepare_vm(&api, &config(&rootfs)).expect("prepared VM");

    assert_eq!(vm.context_id(), 42);
    assert_eq!(
        api.calls(),
        vec![
            "create",
            "vm:42:2:1024",
            &format!("root:42:false:{}", canonical_rootfs.display()),
            "no-vsock:42",
            // The implicit device is dropped, then re-added explicitly with
            // tsi=0 — no socket impersonation. A future change that enabled
            // hijacking by default would show up here as a non-zero mask
            // rather than as silently rerouted guest sockets.
            "vsock:42:tsi=0",
            // Empty port map, and its PRESENCE is the assertion. libkrun reads
            // "never called" as expose-every-listening-guest-port and "called
            // with an empty array" as expose-none, so deleting this line would
            // not fail as a missing feature — it would open every guest
            // listener to the host while every other test stayed green.
            "port-map:42:",
            "workdir:42:/",
            "exec:42:usr/bin/probe:1:1",
        ]
    );
    drop(vm);
    assert_eq!(api.calls().last().expect("final call"), "free:42");
}

#[test]
fn frees_the_libkrun_context_when_configuration_fails() {
    // Catches leaked libkrun contexts on any fallible setup call.
    let rootfs = TempDir::new().expect("rootfs");
    let api = RecordingApi {
        fail_at: Some("no-vsock"),
        ..RecordingApi::default()
    };

    let error = prepare_vm(&api, &config(&rootfs)).expect_err("setup must fail");

    assert_eq!(error.code, ErrorCode::BackendFailed);
    assert_eq!(api.calls().last().expect("final call"), "free:42");
}

fn memfs() -> Arc<dyn DynFileSystem> {
    Arc::new(memfs::MemFs::new([(
        "hello.txt".to_owned(),
        b"hi\n".to_vec(),
    )]))
}

/// The linked library answers without a hypervisor: a context is configuration
/// state, and libkrunfw is loaded but not required until `krun_start_enter`.
/// So this exercises the real libkrun on every host, CI included.
#[test]
fn the_linked_libkrun_attaches_a_custom_share_to_a_live_context() {
    let api = LinkedKrunApi;
    let context = api.create_context().expect("create libkrun context");

    api.add_virtiofs_backend(context, "llo", 0, memfs())
        .expect("attach a backend to a live context");
    api.add_virtiofs_backend(context, "llodax", 1 << 26, memfs())
        .expect("attach a backend with a DAX window");

    api.free_context(context);
    let error = api
        .add_virtiofs_backend(context, "llo", 0, memfs())
        .expect_err("a freed context has nothing to attach to");
    assert_eq!(error.code, ErrorCode::BackendFailed);
    assert!(
        error.detail.contains("krun_add_virtiofs_backend"),
        "the failing call must be named: {error}"
    );
}

/// Each linked call reaches libkrun with its own arguments. On contexts libkrun
/// knows, creation hands out distinct ids and the whole `prepare_vm` sequence
/// succeeds; on one it has freed, every call fails and names itself. No
/// hypervisor is involved, so this observes each `LinkedKrunApi` method on
/// every host — the guest-boot tests that exercise the same calls end to end
/// run only where libkrunfw is installed.
#[test]
fn every_linked_call_reaches_libkrun() {
    let api = LinkedKrunApi;
    let first = api.create_context().expect("first context");
    let second = api.create_context().expect("second context");
    assert_ne!(first, second, "libkrun hands out one id per context");
    api.free_context(first);
    api.free_context(second);

    let rootfs = TempDir::new().expect("rootfs");
    let vm = prepare_vm(&api, &config(&rootfs)).expect("libkrun accepts the prepared VM");
    let freed = vm.context_id();
    drop(vm);

    let results = [
        ("krun_set_vm_config", api.set_vm_config(freed, 1, 256)),
        ("krun_add_virtiofs3", api.add_rootfs(freed, c"/", false)),
        (
            "krun_add_virtiofs_backend",
            api.add_virtiofs_backend(freed, "llo", 0, memfs()),
        ),
        (
            "krun_disable_implicit_vsock",
            api.disable_implicit_vsock(freed),
        ),
        ("krun_add_vsock", api.add_vsock(freed, 0)),
        (
            "krun_add_vsock_port2",
            api.add_vsock_port(freed, VSOCK_UNIX_BASE_PORT, c"/run/llo.sock", false),
        ),
        ("krun_set_port_map", api.set_port_map(freed, &[])),
        ("krun_set_workdir", api.set_workdir(freed, c"/")),
        ("krun_set_exec", api.set_exec(freed, c"/bin/true", &[], &[])),
        ("krun_start_enter", api.start_enter(freed)),
    ];
    for (call, result) in results {
        let error = result.expect_err(call);
        assert_eq!(error.code, ErrorCode::BackendFailed, "{call}: {error}");
        assert!(error.detail.contains(call), "{call} must be named: {error}");
    }
}

/// The share is attached to the context `prepare_vm` built, after the root
/// filesystem and with the window the caller chose — and a tag libkrun would
/// panic on, or one that shadows the root, never reaches libkrun.
#[test]
fn a_prepared_vm_attaches_a_custom_share_to_its_own_context() {
    let rootfs = TempDir::new().expect("rootfs");
    let api = RecordingApi::default();
    let vm = prepare_vm(&api, &config(&rootfs)).expect("prepared VM");

    vm.add_virtiofs_backend("llo", 1 << 21, memfs())
        .expect("valid tag");
    let longest = "t".repeat(VIRTIOFS_TAG_MAX);
    vm.add_virtiofs_backend(&longest, 0, memfs())
        .expect("a tag that fills the config field");
    for refused in [
        "",
        "/dev/root",
        "nul\0tag",
        &"t".repeat(VIRTIOFS_TAG_MAX + 1),
    ] {
        let error = vm
            .add_virtiofs_backend(refused, 0, memfs())
            .expect_err("tag must be refused");
        assert_eq!(error.code, ErrorCode::InvalidSpec, "{refused:?}: {error}");
    }
    drop(vm);

    let calls = api.calls();
    let shares: Vec<_> = calls
        .iter()
        .filter(|call| call.starts_with("virtiofs:"))
        .cloned()
        .collect();
    assert_eq!(
        shares,
        vec![
            "virtiofs:42:llo:2097152".to_owned(),
            format!("virtiofs:42:{longest}:0")
        ],
        "only the valid tags reach libkrun, on the prepared context: {calls:?}"
    );
    assert_eq!(calls.last().expect("final call"), "free:42");
}

#[test]
fn virtiofs_tags_are_checked_against_the_device_config_field() {
    assert!(check_virtiofs_tag("a").is_ok());
    assert!(check_virtiofs_tag(&"a".repeat(VIRTIOFS_TAG_MAX)).is_ok());
    assert!(check_virtiofs_tag(&"a".repeat(VIRTIOFS_TAG_MAX + 1)).is_err());
    assert!(check_virtiofs_tag("").is_err());
    assert!(check_virtiofs_tag("/dev/root").is_err());
    assert!(check_virtiofs_tag("/dev/root2").is_ok());
    assert!(check_virtiofs_tag("a\0").is_err());
}

/// §6's vsock mappings reach libkrun, in order, between the vsock device and
/// the port map — the first caller `add_vsock_port` has ever had.
///
/// The positions matter: after `add_vsock` because the ports belong to that
/// device, and the recorded sequence pins that a mapping can never be added
/// to the implicit device this module deliberately disables.
#[test]
fn socket_mappings_reach_the_vsock_device_in_document_order() {
    let rootfs = TempDir::new().expect("rootfs");
    let api = RecordingApi::default();
    let mut with_sockets = config(&rootfs);
    with_sockets.vsock_unix_map = vec![
        VsockUnixMapping {
            port: VSOCK_UNIX_BASE_PORT,
            host_path: std::ffi::CString::new("/run/cloister/shim.sock").expect("path"),
            listen: false,
        },
        VsockUnixMapping {
            port: VSOCK_UNIX_BASE_PORT + 3,
            host_path: std::ffi::CString::new("/run/llo/served.sock").expect("path"),
            listen: true,
        },
    ];

    let vm = prepare_vm(&api, &with_sockets).expect("prepared VM");
    drop(vm);

    let calls = api.calls();
    let vsock = calls
        .iter()
        .position(|c| c.starts_with("vsock:"))
        .expect("vsock device added");
    let first = calls
        .iter()
        .position(|c| {
            c == &format!(
                "vsock-port:42:{}:/run/cloister/shim.sock:listen=false",
                VSOCK_UNIX_BASE_PORT
            )
        })
        .expect("dial mapping recorded");
    let second = calls
        .iter()
        .position(|c| {
            c == &format!(
                "vsock-port:42:{}:/run/llo/served.sock:listen=true",
                VSOCK_UNIX_BASE_PORT + 3
            )
        })
        .expect("serve mapping recorded");
    let port_map = calls
        .iter()
        .position(|c| c.starts_with("port-map:"))
        .expect("port map still always called");
    assert!(
        vsock < first && first < second && second < port_map,
        "mappings must follow their device and precede the port map, in \
         document order: {calls:?}"
    );
}
