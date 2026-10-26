//! [`super::order`] on macOS: `F_BARRIERFSYNC` writes the file's data and
//! orders it before later I/O without flushing the drive's cache.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

pub(super) fn barrier(file: &File) -> io::Result<()> {
    // SAFETY: `fcntl` with `F_BARRIERFSYNC` takes no pointer argument and
    // reads only the descriptor, which `file` keeps open for the call.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_BARRIERFSYNC) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
