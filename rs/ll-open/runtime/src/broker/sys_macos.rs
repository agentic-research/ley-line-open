//! macOS halves of the broker's platform queries. The other platforms' halves
//! are `sys_other.rs`; the mutation gate mutates each where it compiles
//! (`tools/mutants_diff.sh`, runtime slice).

use std::ffi::CStr;
use std::io;
use std::os::fd::RawFd;

// SAFETY: these are libSystem's ACL functions with their documented C
// signatures (acl(3)); `acl_get_link_np` returns an ACL the caller frees with
// `acl_free`, or NULL with errno set.
unsafe extern "C" {
    fn acl_get_link_np(path: *const libc::c_char, kind: libc::c_int) -> *mut libc::c_void;
    fn acl_free(object: *mut libc::c_void) -> libc::c_int;
}

const ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;

/// Whether `path` carries an extended ACL. Only `ENOENT` ("no ACL") reads as
/// none; any other failure is not proof of privacy, so it counts as one.
pub(crate) fn has_acl(path: &CStr) -> bool {
    // SAFETY: a NUL-terminated path; the returned ACL is freed below.
    let acl = unsafe { acl_get_link_np(path.as_ptr(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        return io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT);
    }
    // SAFETY: freeing the ACL acl_get_link_np returned.
    unsafe { acl_free(acl) };
    true
}

/// The connecting peer's effective uid (`getpeereid`) and pid (`LOCAL_PEERPID`).
pub(crate) fn peer_credentials(fd: RawFd) -> io::Result<(u32, Option<i32>)> {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: valid out-pointers on a connected AF_UNIX socket.
    if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: valid out-pointer and length for LOCAL_PEERPID.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut len,
        )
    };
    Ok((uid, (rc == 0).then_some(pid)))
}
