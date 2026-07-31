use std::ffi::{CStr, CString};
use std::fmt;
use std::os::raw::c_char;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::ExecutionError;
use libloading::Library;

use super::plan::KrunConfig;

/// Safe, narrow projection of the libkrun C API used by Leyline.
///
/// The dynamic implementation owns the unsafe ABI boundary. Tests can replace
/// it at this seam without loading firmware or entering a VMM.
pub trait KrunApi: Send + Sync {
    fn create_context(&self) -> Result<u32, ExecutionError>;
    fn free_context(&self, context: u32);
    fn set_vm_config(&self, context: u32, vcpus: u8, ram_mib: u32) -> Result<(), ExecutionError>;
    fn add_rootfs(
        &self,
        context: u32,
        rootfs: &CStr,
        read_only: bool,
    ) -> Result<(), ExecutionError>;
    fn disable_implicit_vsock(&self, context: u32) -> Result<(), ExecutionError>;
    fn set_workdir(&self, context: u32, workdir: &CStr) -> Result<(), ExecutionError>;
    fn set_exec(
        &self,
        context: u32,
        executable: &CStr,
        arguments: &[CString],
        environment: &[CString],
    ) -> Result<(), ExecutionError>;
    fn start_enter(&self, context: u32) -> Result<(), ExecutionError>;
}

type CreateContextFn = unsafe extern "C" fn() -> i32;
type FreeContextFn = unsafe extern "C" fn(u32) -> i32;
type SetVmConfigFn = unsafe extern "C" fn(u32, u8, u32) -> i32;
type AddVirtiofs3Fn = unsafe extern "C" fn(u32, *const c_char, *const c_char, u64, bool) -> i32;
type ContextOnlyFn = unsafe extern "C" fn(u32) -> i32;
type StringOptionFn = unsafe extern "C" fn(u32, *const c_char) -> i32;
type SetExecFn =
    unsafe extern "C" fn(u32, *const c_char, *const *const c_char, *const *const c_char) -> i32;

/// Dynamically loaded libkrun ABI. Loading at runtime keeps the rest of the
/// first-party CLI usable on hosts where the optional microVM backend is not
/// installed.
pub struct DynamicKrunApi {
    _library: Library,
    create_context: CreateContextFn,
    free_context: FreeContextFn,
    set_vm_config: SetVmConfigFn,
    add_virtiofs3: AddVirtiofs3Fn,
    disable_implicit_vsock: ContextOnlyFn,
    set_workdir: StringOptionFn,
    set_exec: SetExecFn,
    start_enter: ContextOnlyFn,
}

impl fmt::Debug for DynamicKrunApi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DynamicKrunApi")
            .finish_non_exhaustive()
    }
}

impl DynamicKrunApi {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ExecutionError> {
        let path = path.as_ref();
        // SAFETY: the library stays owned by the returned object for at least
        // as long as every copied function pointer. Each symbol is loaded with
        // the signature declared by the corresponding libkrun C header.
        let library = unsafe { Library::new(path) }.map_err(|error| {
            ExecutionError::backend(format!(
                "load libkrun shared library {}: {error}",
                path.display()
            ))
        })?;
        let create_context = load_symbol(&library, b"krun_create_ctx\0")?;
        let free_context = load_symbol(&library, b"krun_free_ctx\0")?;
        let set_vm_config = load_symbol(&library, b"krun_set_vm_config\0")?;
        let add_virtiofs3 = load_symbol(&library, b"krun_add_virtiofs3\0")?;
        let disable_implicit_vsock = load_symbol(&library, b"krun_disable_implicit_vsock\0")?;
        let set_workdir = load_symbol(&library, b"krun_set_workdir\0")?;
        let set_exec = load_symbol(&library, b"krun_set_exec\0")?;
        let start_enter = load_symbol(&library, b"krun_start_enter\0")?;

        Ok(Self {
            _library: library,
            create_context,
            free_context,
            set_vm_config,
            add_virtiofs3,
            disable_implicit_vsock,
            set_workdir,
            set_exec,
            start_enter,
        })
    }
}

impl KrunApi for DynamicKrunApi {
    fn create_context(&self) -> Result<u32, ExecutionError> {
        // SAFETY: symbol signature was checked when the library was loaded.
        let result = unsafe { (self.create_context)() };
        if result < 0 {
            return Err(ffi_error("krun_create_ctx", result));
        }
        Ok(result as u32)
    }

    fn free_context(&self, context: u32) {
        // SAFETY: context was returned by this libkrun instance. Drop cannot
        // report cleanup failures, so the return code is intentionally ignored.
        let _ = unsafe { (self.free_context)(context) };
    }

    fn set_vm_config(&self, context: u32, vcpus: u8, ram_mib: u32) -> Result<(), ExecutionError> {
        // SAFETY: scalar arguments match the loaded C signature.
        check_ffi("krun_set_vm_config", unsafe {
            (self.set_vm_config)(context, vcpus, ram_mib)
        })
    }

    fn add_rootfs(
        &self,
        context: u32,
        rootfs: &CStr,
        read_only: bool,
    ) -> Result<(), ExecutionError> {
        const ROOT_TAG: &CStr = c"/dev/root";
        // SAFETY: both C strings live through the call; DAX is disabled and
        // the caller explicitly chooses the guest mount's write policy.
        check_ffi("krun_add_virtiofs3", unsafe {
            (self.add_virtiofs3)(context, ROOT_TAG.as_ptr(), rootfs.as_ptr(), 0, read_only)
        })
    }

    fn disable_implicit_vsock(&self, context: u32) -> Result<(), ExecutionError> {
        // SAFETY: scalar argument matches the loaded C signature.
        check_ffi("krun_disable_implicit_vsock", unsafe {
            (self.disable_implicit_vsock)(context)
        })
    }

    fn set_workdir(&self, context: u32, workdir: &CStr) -> Result<(), ExecutionError> {
        // SAFETY: the C string lives through the call.
        check_ffi("krun_set_workdir", unsafe {
            (self.set_workdir)(context, workdir.as_ptr())
        })
    }

    fn set_exec(
        &self,
        context: u32,
        executable: &CStr,
        arguments: &[CString],
        environment: &[CString],
    ) -> Result<(), ExecutionError> {
        let arguments = null_terminated(arguments);
        let environment = null_terminated(environment);
        // SAFETY: all strings and both NULL-terminated pointer arrays live
        // through the call.
        check_ffi("krun_set_exec", unsafe {
            (self.set_exec)(
                context,
                executable.as_ptr(),
                arguments.as_ptr(),
                environment.as_ptr(),
            )
        })
    }

    fn start_enter(&self, context: u32) -> Result<(), ExecutionError> {
        // SAFETY: context is consumed by libkrun. On success this call exits
        // the worker process and therefore does not return.
        check_ffi("krun_start_enter", unsafe { (self.start_enter)(context) })
    }
}

fn load_symbol<T: Copy>(library: &Library, name: &[u8]) -> Result<T, ExecutionError> {
    // SAFETY: callers provide the exact function-pointer type from libkrun.h;
    // the copied pointer cannot outlive `library` because it is stored beside
    // the owning Library in DynamicKrunApi.
    let symbol = unsafe { library.get::<T>(name) }.map_err(|error| {
        let name = String::from_utf8_lossy(name)
            .trim_end_matches('\0')
            .to_owned();
        ExecutionError::backend(format!("load libkrun symbol {name}: {error}"))
    })?;
    Ok(*symbol)
}

fn null_terminated(values: &[CString]) -> Vec<*const c_char> {
    values
        .iter()
        .map(|value| value.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect()
}

fn check_ffi(operation: &str, result: i32) -> Result<(), ExecutionError> {
    if result == 0 {
        Ok(())
    } else {
        Err(ffi_error(operation, result))
    }
}

fn ffi_error(operation: &str, result: i32) -> ExecutionError {
    ExecutionError::backend(format!("{operation} failed with libkrun status {result}"))
}

pub struct PreparedVm<'api> {
    api: &'api dyn KrunApi,
    context: Option<u32>,
}

impl fmt::Debug for PreparedVm<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedVm")
            .field("context", &self.context)
            .finish_non_exhaustive()
    }
}

impl PreparedVm<'_> {
    pub fn context_id(&self) -> u32 {
        self.context.expect("prepared VM always owns a context")
    }

    /// Enter the VMM. A successful libkrun invocation takes over and exits the
    /// worker process; this returns only for a pre-entry error.
    pub fn enter(mut self) -> Result<(), ExecutionError> {
        let context = self.context.take().expect("prepared VM owns context");
        self.api.start_enter(context)
    }
}

impl Drop for PreparedVm<'_> {
    fn drop(&mut self) {
        if let Some(context) = self.context.take() {
            self.api.free_context(context);
        }
    }
}

pub fn prepare_vm<'api>(
    api: &'api dyn KrunApi,
    config: &KrunConfig,
) -> Result<PreparedVm<'api>, ExecutionError> {
    let rootfs = CString::new(config.rootfs.canonical_path.as_os_str().as_bytes())
        .map_err(|_| ExecutionError::invalid("rootfs host path contains an interior NUL byte"))?;
    let context = api.create_context()?;
    let vm = PreparedVm {
        api,
        context: Some(context),
    };

    api.set_vm_config(context, config.vcpus, config.ram_mib)?;
    api.add_rootfs(context, &rootfs, false)?;
    api.disable_implicit_vsock(context)?;
    api.set_workdir(context, &config.workdir)?;
    api.set_exec(
        context,
        &config.executable,
        &config.arguments,
        &config.environment,
    )?;

    Ok(vm)
}
