//! What only a Mac can answer: its product version, which
//! [`super::unpatched_nfs_client`] decides the NFS mount on. Compiled on
//! macOS alone (`sysctlbyname(3)` is a BSD call), so the mutation gate
//! excludes this file on every other platform's runner and mutates it on
//! macOS (`tools/mutants_diff.sh`).

use anyhow::{Context, Result};

/// This Mac's product version (`kern.osproductversion`, as `sw_vers
/// -productVersion` reports it).
pub fn product_version() -> Result<String> {
    let name = c"kern.osproductversion";
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    // SAFETY: `buf` is valid for `len` bytes and `sysctlbyname` writes at
    // most `len` bytes into it, updating `len` to the count written; the
    // name is NUL-terminated and no new value is set.
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return Err(std::io::Error::last_os_error()).context("read kern.osproductversion");
    }
    let bytes = &buf[..len];
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    Ok(String::from_utf8_lossy(bytes).into_owned())
}
