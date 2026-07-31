use std::ffi::CStr;
use std::sync::Mutex;

use leyline_runtime::backends::libkrun::api::{DynamicKrunApi, KrunApi, prepare_vm};
use leyline_runtime::backends::libkrun::plan::{KrunConfig, ResolvedRootfs};
use leyline_runtime::{DigestRef, ErrorCode, ExecutionError};
use tempfile::TempDir;

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

    fn add_read_only_rootfs(&self, context: u32, rootfs: &CStr) -> Result<(), ExecutionError> {
        self.record(format!("root:{context}:{}", rootfs.to_string_lossy()))
    }

    fn disable_implicit_vsock(&self, context: u32) -> Result<(), ExecutionError> {
        self.record(format!("no-vsock:{context}"))
    }

    fn disable_host_port_exposure(&self, context: u32) -> Result<(), ExecutionError> {
        self.record(format!("no-ports:{context}"))
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
    }
}

#[test]
fn prepares_a_read_only_networkless_vm_without_entering_it() {
    // Catches accidental TSI/network exposure and writable rootfs mounting at
    // the C API mapping seam.
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
            &format!("root:42:{}", canonical_rootfs.display()),
            "no-vsock:42",
            "no-ports:42",
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

#[test]
fn missing_shared_library_is_a_first_party_backend_error() {
    // Catches regressions back to a wrapper that recommends an internal task
    // or shells out to the krunvm development CLI.
    let error = DynamicKrunApi::load("/definitely/missing/libkrun")
        .expect_err("missing shared library must fail");

    assert_eq!(error.code, ErrorCode::BackendFailed);
    assert!(
        error.detail.contains("load libkrun shared library"),
        "{error}"
    );
    assert!(!error.detail.contains("krunvm"), "{error}");
    assert!(!error.detail.contains("task "), "{error}");
}

#[test]
#[ignore = "requires LEYLINE_LIBKRUN_PATH to point at an installed libkrun"]
fn installed_libkrun_exports_the_embedded_api() {
    let path = std::env::var_os("LEYLINE_LIBKRUN_PATH").expect("LEYLINE_LIBKRUN_PATH");
    let api = DynamicKrunApi::load(path).expect("load installed libkrun");

    let context = api.create_context().expect("create libkrun context");
    api.free_context(context);
}
