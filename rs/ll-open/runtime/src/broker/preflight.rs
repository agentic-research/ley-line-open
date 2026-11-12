//! Host checks on each endpoint before the broker binds it (ADR-0046 S4).
//!
//! The issuer chooses every path; LLO chooses none. These checks make a reused
//! or hostile path a refusal by name rather than a shared or hijacked socket.
//! They go by path, not by directory descriptor: a same-uid process could swap
//! the parent between check and bind (S4, S10 residual).

use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::ExecutionError;

/// This platform's `sun_path` capacity, including the terminating NUL.
pub const SUN_PATH_BYTES: usize = {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    {
        104
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
    {
        108
    }
};

/// Check one endpoint for the connection `name`, in the order of S4's table.
/// `euid` is the daemon's effective uid, which must own the parent.
pub fn check_endpoint(name: &str, endpoint: &Path, euid: u32) -> Result<(), ExecutionError> {
    let refuse = |reason: &str| {
        ExecutionError::invalid_grant(format!(
            "connection `{name}`: endpoint {}: {reason}",
            endpoint.display()
        ))
    };
    match std::fs::symlink_metadata(endpoint) {
        Ok(_) => {
            return Err(ExecutionError::resource_conflict(format!(
                "connection `{name}`: endpoint {}: endpoint exists",
                endpoint.display()
            )));
        }
        // ENOTDIR: a component of the parent is not a directory, which the
        // parent checks below refuse by name.
        Err(error)
            if error.kind() == ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ENOTDIR) => {}
        Err(error) => return Err(refuse(&format!("cannot inspect the endpoint: {error}"))),
    }
    let parent = endpoint
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| refuse("endpoint parent missing"))?;
    let metadata = match std::fs::symlink_metadata(parent) {
        Ok(metadata) => metadata,
        Err(_) => return Err(refuse("endpoint parent missing")),
    };
    if metadata.file_type().is_symlink() {
        return Err(refuse("endpoint parent is reached through a symlink"));
    }
    if !metadata.is_dir() {
        return Err(refuse("endpoint parent missing"));
    }
    match std::fs::canonicalize(parent) {
        Ok(real) if real == parent => {}
        _ => return Err(refuse("endpoint parent is reached through a symlink")),
    }
    if metadata.uid() != euid || metadata.mode() & 0o7777 != 0o700 {
        return Err(refuse("endpoint parent not private"));
    }
    if endpoint.as_os_str().len() >= SUN_PATH_BYTES {
        return Err(refuse("endpoint path too long"));
    }
    Ok(())
}
