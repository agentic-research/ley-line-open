//! Linux (and other non-macOS) halves of the broker's platform queries. The
//! macOS halves are `sys_macos.rs`; the mutation gate mutates each where it
//! compiles (`tools/mutants_diff.sh`, runtime slice).

use std::ffi::CStr;
use std::io;
use std::os::fd::RawFd;

/// Whether `path` carries a POSIX ACL (an access or default ACL xattr).
pub(crate) fn has_acl(path: &CStr) -> bool {
    [c"system.posix_acl_access", c"system.posix_acl_default"]
        .iter()
        .any(|name| {
            // SAFETY: a size query (null buffer, length 0) on NUL-terminated strings.
            unsafe { libc::lgetxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) > 0 }
        })
}

/// The connecting peer's uid and pid (`SO_PEERCRED`).
pub(crate) fn peer_credentials(fd: RawFd) -> io::Result<(u32, Option<i32>)> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: valid out-pointer and length for SO_PEERCRED.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((credentials.uid, Some(credentials.pid)))
}
