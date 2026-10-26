//! [`super::order`] off macOS: `fsync(2)`, the only ordering primitive
//! POSIX offers.

use std::fs::File;
use std::io;

pub(super) fn barrier(file: &File) -> io::Result<()> {
    file.sync_all()
}
