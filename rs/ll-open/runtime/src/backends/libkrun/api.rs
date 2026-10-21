use std::ffi::{CStr, CString};
use std::fmt;
#[cfg(feature = "vmm")]
use std::os::raw::c_char;
use std::os::unix::ffi::OsStrExt;
#[cfg(feature = "vmm")]
use std::sync::Arc;

use crate::ExecutionError;

use super::plan::KrunConfig;

/// The virtio-fs request types a custom share implements: `DynFileSystem` and
/// the FUSE argument and reply types its methods take (`filesystem::Context`,
/// `Entry`, `FsOptions`, ...). Re-exported from the linked libkrun so a
/// backend names them through this crate and cannot drift from the VMM that
/// dispatches to it.
#[cfg(feature = "vmm")]
pub use krun::virtio_fs;
#[cfg(feature = "vmm")]
pub use krun::virtio_fs::dyn_fs::DynFileSystem;

/// The virtio-fs tag libkrun gives the guest's root filesystem.
const ROOT_TAG: &CStr = c"/dev/root";

/// Bytes in the virtio-fs device configuration's `tag` field (virtio spec
/// 5.11.4). libkrun copies the tag into it unchecked and panics inside
/// `krun_start_enter` on a longer one, so the length is checked here instead.
pub const VIRTIOFS_TAG_MAX: usize = 36;

/// Safe, narrow projection of the libkrun API used by Leyline.
///
/// [`LinkedKrunApi`] owns the unsafe ABI boundary. Tests replace it at this
/// seam without loading firmware or entering a VMM.
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
    /// Attach a virtio-fs device whose FUSE requests `backend` answers, in
    /// this process, instead of a host directory (ADR-0044 V1).
    ///
    /// `dax_window` is the guest DAX window in bytes; `0` attaches the device
    /// without one. The guest mounts it with `mount -t virtiofs <tag> <dir>`.
    /// Replies carry Linux errno values whatever the host is: the guest is
    /// Linux.
    #[cfg(feature = "vmm")]
    fn add_virtiofs_backend(
        &self,
        context: u32,
        tag: &str,
        dax_window: u64,
        backend: Arc<dyn DynFileSystem>,
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

/// `KRUN_TSI_HIJACK_INET` from `libkrun.h` — carry the guest's `AF_INET`
/// sockets over vsock instead of leaving them unreachable.
///
/// Named here rather than written as a bare `1` at the call site so the one
/// place that weakens the boundary says what it is doing.
pub const KRUN_TSI_HIJACK_INET: u32 = 1 << 0;

/// libkrun linked into this binary, built from `rs/third_party/libkrun`: upstream
/// v1.19.6 plus LLO's patches (ADR-0044). LLO builds the VMM it runs; nothing
/// is loaded from an installed libkrun. The guest kernel is still the
/// installed libkrunfw, which libkrun dlopens at `krun_create_ctx`.
#[cfg(feature = "vmm")]
#[derive(Debug, Default, Clone, Copy)]
pub struct LinkedKrunApi;

#[cfg(feature = "vmm")]
impl KrunApi for LinkedKrunApi {
    fn create_context(&self) -> Result<u32, ExecutionError> {
        let result = krun::krun_create_ctx();
        if result < 0 {
            return Err(ffi_error("krun_create_ctx", result));
        }
        Ok(result as u32)
    }

    fn free_context(&self, context: u32) {
        // Drop cannot report cleanup failures, so the status is ignored.
        let _ = krun::krun_free_ctx(context);
    }

    fn set_vm_config(&self, context: u32, vcpus: u8, ram_mib: u32) -> Result<(), ExecutionError> {
        check_ffi(
            "krun_set_vm_config",
            krun::krun_set_vm_config(context, vcpus, ram_mib),
        )
    }

    fn add_rootfs(
        &self,
        context: u32,
        rootfs: &CStr,
        read_only: bool,
    ) -> Result<(), ExecutionError> {
        // SAFETY: both C strings live through the call; DAX is disabled and
        // the caller explicitly chooses the guest mount's write policy.
        check_ffi("krun_add_virtiofs3", unsafe {
            krun::krun_add_virtiofs3(context, ROOT_TAG.as_ptr(), rootfs.as_ptr(), 0, read_only)
        })
    }

    fn add_virtiofs_backend(
        &self,
        context: u32,
        tag: &str,
        dax_window: u64,
        backend: Arc<dyn DynFileSystem>,
    ) -> Result<(), ExecutionError> {
        check_ffi(
            "krun_add_virtiofs_backend",
            krun::krun_add_virtiofs_backend(context, tag, dax_window, backend),
        )
    }

    fn add_vsock(&self, context: u32, tsi_features: u32) -> Result<(), ExecutionError> {
        check_ffi(
            "krun_add_vsock",
            krun::krun_add_vsock(context, tsi_features),
        )
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
            krun::krun_add_vsock_port2(context, port, host_path.as_ptr(), listen)
        })
    }

    fn set_port_map(&self, context: u32, entries: &[CString]) -> Result<(), ExecutionError> {
        // A NULL-terminated `char*` array. The terminator is the entire point:
        // libkrun reads a NULL *array pointer* as "expose every listening guest
        // port", and a pointer to an immediately-terminated array as "expose
        // none". An empty `entries` therefore builds `[NULL]` — one element,
        // the terminator — which is the closed default, not the open one.
        let pointers = null_terminated(entries);
        // SAFETY: `entries` owns the strings and `pointers` the array, both
        // alive across the call; libkrun copies what it needs.
        check_ffi("krun_set_port_map", unsafe {
            krun::krun_set_port_map(context, pointers.as_ptr())
        })
    }

    fn disable_implicit_vsock(&self, context: u32) -> Result<(), ExecutionError> {
        check_ffi(
            "krun_disable_implicit_vsock",
            krun::krun_disable_implicit_vsock(context),
        )
    }

    fn set_workdir(&self, context: u32, workdir: &CStr) -> Result<(), ExecutionError> {
        // SAFETY: the C string lives through the call.
        check_ffi("krun_set_workdir", unsafe {
            krun::krun_set_workdir(context, workdir.as_ptr())
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
            krun::krun_set_exec(
                context,
                executable.as_ptr(),
                arguments.as_ptr(),
                environment.as_ptr(),
            )
        })
    }

    fn start_enter(&self, context: u32) -> Result<(), ExecutionError> {
        // On success libkrun runs the guest and exits this process with the
        // guest's status, so this returns only for a pre-entry error.
        check_ffi("krun_start_enter", krun::krun_start_enter(context))
    }
}

#[cfg(feature = "vmm")]
fn null_terminated(values: &[CString]) -> Vec<*const c_char> {
    values
        .iter()
        .map(|value| value.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect()
}

/// A virtio-fs tag the guest can mount and libkrun can attach: 1 to
/// [`VIRTIOFS_TAG_MAX`] bytes, no NUL, and not the root filesystem's tag, which
/// would put a second device under the name the guest boots from.
pub fn check_virtiofs_tag(tag: &str) -> Result<(), ExecutionError> {
    if tag.is_empty() || tag.len() > VIRTIOFS_TAG_MAX || tag.contains('\0') {
        return Err(ExecutionError::invalid(format!(
            "virtio-fs tag {tag:?} must be 1 to {VIRTIOFS_TAG_MAX} bytes with no NUL"
        )));
    }
    if tag.as_bytes() == ROOT_TAG.to_bytes() {
        return Err(ExecutionError::invalid(format!(
            "virtio-fs tag {tag:?} is the guest root filesystem's"
        )));
    }
    Ok(())
}

#[cfg(feature = "vmm")]
fn check_ffi(operation: &str, result: i32) -> Result<(), ExecutionError> {
    if result == 0 {
        Ok(())
    } else {
        Err(ffi_error(operation, result))
    }
}

#[cfg(feature = "vmm")]
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

    /// Give the guest a virtio-fs share served by `backend` from this process
    /// (ADR-0044 V1). Called between [`prepare_vm`] and [`PreparedVm::enter`];
    /// the backend lives as long as the VMM, which is the rest of the process.
    #[cfg(feature = "vmm")]
    pub fn add_virtiofs_backend(
        &self,
        tag: &str,
        dax_window: u64,
        backend: Arc<dyn DynFileSystem>,
    ) -> Result<(), ExecutionError> {
        check_virtiofs_tag(tag)?;
        self.api
            .add_virtiofs_backend(self.context_id(), tag, dax_window, backend)
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
