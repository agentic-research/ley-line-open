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
    /// Add the one vsock device, with an explicit TSI feature mask.
    ///
    /// `0` means no Transparent Socket Impersonation: the guest reaches the
    /// host only over ports handed to it by [`KrunApi::add_vsock_port`], and
    /// its ordinary `AF_INET` sockets are not rerouted. Hijacking is
    /// expressible — `KRUN_TSI_HIJACK_INET` — but never the default, because it
    /// converts the boundary from "the guest can only use channels it was
    /// given" into "the guest's sockets are silently carried out".
    ///
    /// libkrun supports exactly one vsock device and errors on a second call.
    /// That is a limit on the DEVICE, not on ports: `add_vsock_port` may be
    /// called repeatedly against it.
    fn add_vsock(&self, context: u32, tsi_features: u32) -> Result<(), ExecutionError>;
    /// Pair a guest vsock port with a host UNIX socket path.
    ///
    /// `listen` is true when the host initiates — a service inside the guest
    /// that the host connects to. The host side being a filesystem path is what
    /// makes this expressible in the confinement manifest as an ordinary
    /// `fs.allow` grant, so the channel is covered by the attested digest
    /// rather than sitting outside it.
    fn add_vsock_port(
        &self,
        context: u32,
        port: u32,
        host_path: &CStr,
        listen: bool,
    ) -> Result<(), ExecutionError>;
    /// Declare exactly which guest TCP ports are exposed to the host.
    ///
    /// libkrun distinguishes "not called" from "called with an empty array":
    /// the first attempts to expose every listening guest port to the host, the
    /// second exposes none. So the empty call is the closed default and is
    /// always made.
    ///
    /// MEASURED, because the obvious story was wrong. Removing this call
    /// entirely and re-running `tests/libkrun_guest_listener.rs` leaves the
    /// guest's listener STILL unreachable — under `tsi_features: 0` there is no
    /// `AF_INET` path for a port map to govern, so what holds the boundary on
    /// the default path is the absence of socket impersonation, not this call.
    ///
    /// It becomes load-bearing on the opt-in path. With `KRUN_TSI_HIJACK_INET`
    /// the guest's INET sockets ARE carried over vsock, and then this map is
    /// the only thing deciding which of them the host can reach — where "not
    /// called" would mean all of them. Keeping the empty call unconditional
    /// means enabling hijacking cannot silently also publish every guest port.
    fn set_port_map(&self, context: u32, entries: &[CString]) -> Result<(), ExecutionError>;
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
/// `KRUN_TSI_HIJACK_INET` from `libkrun.h` — carry the guest's `AF_INET`
/// sockets over vsock instead of leaving them unreachable.
///
/// Named here rather than written as a bare `1` at the call site so the one
/// place that weakens the boundary says what it is doing.
pub const KRUN_TSI_HIJACK_INET: u32 = 1 << 0;

type AddVsockFn = unsafe extern "C" fn(u32, u32) -> i32;
type AddVsockPort2Fn = unsafe extern "C" fn(u32, u32, *const c_char, bool) -> i32;
type SetPortMapFn = unsafe extern "C" fn(u32, *const *const c_char) -> i32;

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
    add_vsock: AddVsockFn,
    add_vsock_port2: AddVsockPort2Fn,
    set_port_map: SetPortMapFn,
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
        let add_vsock = load_symbol(&library, b"krun_add_vsock\0")?;
        let add_vsock_port2 = load_symbol(&library, b"krun_add_vsock_port2\0")?;
        let set_port_map = load_symbol(&library, b"krun_set_port_map\0")?;
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
            add_vsock,
            add_vsock_port2,
            set_port_map,
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

    fn add_vsock(&self, context: u32, tsi_features: u32) -> Result<(), ExecutionError> {
        // SAFETY: scalar arguments match the loaded C signature.
        check_ffi("krun_add_vsock", unsafe {
            (self.add_vsock)(context, tsi_features)
        })
    }

    fn add_vsock_port(
        &self,
        context: u32,
        port: u32,
        host_path: &CStr,
        listen: bool,
    ) -> Result<(), ExecutionError> {
        // SAFETY: the path outlives the call; libkrun copies it.
        check_ffi("krun_add_vsock_port2", unsafe {
            (self.add_vsock_port2)(context, port, host_path.as_ptr(), listen)
        })
    }

    fn set_port_map(&self, context: u32, entries: &[CString]) -> Result<(), ExecutionError> {
        // A NULL-terminated `char*` array. The terminator is the entire point:
        // libkrun reads a NULL *array pointer* as "expose every listening guest
        // port", and a pointer to an immediately-terminated array as "expose
        // none". An empty `entries` therefore builds `[NULL]` — one element,
        // the terminator — which is the closed default, not the open one.
        let pointers: Vec<*const c_char> = entries
            .iter()
            .map(|entry| entry.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();
        // SAFETY: `entries` owns the strings and `pointers` the array, both
        // alive across the call; libkrun copies what it needs.
        check_ffi("krun_set_port_map", unsafe {
            (self.set_port_map)(context, pointers.as_ptr())
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
        // A missing symbol here is almost always a libkrun older than this
        // build expects, and the raw dlopen error does not say so. Naming the
        // cause is the difference between "upgrade libkrun" and an afternoon
        // spent on a linker message.
        //
        // `krun_add_vsock` is the newest of the three the vsock listener work
        // added (`ley-line-open-17536d`) and is therefore the usual one to
        // fail. Tested against libkrun 1.19.4; the true floor is whichever
        // release first exported it, which we have not measured.
        let hint = match name.as_str() {
            "krun_add_vsock" | "krun_add_vsock_port2" | "krun_set_port_map" => {
                " — this build needs a libkrun exporting the explicit vsock and \
                 port-map API (tested against 1.19.4). An older libkrun cannot \
                 be used: without krun_set_port_map the guest's listening ports \
                 would be exposed to the host by default, so the backend refuses \
                 rather than running with a weaker boundary than it reports."
            }
            _ => "",
        };
        ExecutionError::backend(format!("load libkrun symbol {name}: {error}{hint}"))
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

    // The implicit device is gone; this is the explicit replacement, and the
    // TSI mask is the whole safety argument. `0` = no socket impersonation, so
    // the guest reaches the host only over ports handed to it below. Hijacking
    // stays expressible (`KRUN_TSI_HIJACK_INET`) but is never chosen here — it
    // converts the boundary from "only channels the guest was given" into "the
    // guest's ordinary sockets are silently carried out", which is a decision
    // an operator makes explicitly, not a default anyone inherits.
    api.add_vsock(context, config.tsi_features)?;

    // §6 on this tier: one explicit channel per mapping, and ONLY those. The
    // guest can reach a vsock port precisely when a mapping constructed it —
    // "only what was constructed exists" is the enforcement mechanism, not a
    // profile rule — and a `listen=true` port additionally answers a
    // guest-originated request with a reset, which is what makes
    // serve-without-dial (§6 `bind`) a real state here. Ports are the pure
    // function of document order defined at `vsock_unix_mappings`; empty means
    // the manifest declared no sockets, and nothing is mapped.
    for mapping in &config.vsock_unix_map {
        api.add_vsock_port(context, mapping.port, &mapping.host_path, mapping.listen)?;
    }

    // Always called, including with nothing to map: libkrun reads "never
    // called" as expose-EVERY-listening-guest-port and "called with an empty
    // array" as expose-none.
    //
    // Not what holds the boundary today, and I checked rather than assumed.
    // Deleting this line and re-running tests/libkrun_guest_listener.rs still
    // leaves the guest's listener unreachable, because `tsi_features: 0` means
    // no AF_INET path exists for a port map to govern. The default path is held
    // by the absence of hijacking above.
    //
    // It is the guard for the OPT-IN path. Turn on KRUN_TSI_HIJACK_INET and the
    // guest's INET sockets are carried over vsock; this map then decides which
    // the host may reach, and its absence would mean all of them. Unconditional
    // here so that enabling hijacking can never also silently publish every
    // port the guest happens to bind.
    api.set_port_map(context, &config.port_map)?;

    api.set_workdir(context, &config.workdir)?;
    api.set_exec(
        context,
        &config.executable,
        &config.arguments,
        &config.environment,
    )?;

    Ok(vm)
}
