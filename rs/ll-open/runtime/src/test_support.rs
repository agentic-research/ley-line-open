//! Fixtures shared by this crate's unit tests.

use std::path::Path;

/// Write `contents` to `path`, mode 0755, without this process ever holding
/// the file open for writing. `execve` fails with `ETXTBSY` while any
/// process holds the file open for writing, and libtest runs the unit tests
/// beside others that spawn: a child forked by another thread while this
/// one wrote with `fs::write` would hold a copy of the write descriptor
/// until its own exec (bead `ley-line-open-cdfaf4`, where the integration
/// binaries take a lock instead). Here a `/bin/sh` child writes the file and
/// exits before anything runs it, and nothing forked from this process can
/// hold that child's descriptor.
pub(crate) fn write_executable(path: &Path, contents: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut writer = Command::new("/bin/sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    writer
        .stdin
        .take()
        .unwrap()
        .write_all(contents.as_bytes())
        .unwrap();
    assert!(writer.wait().unwrap().success(), "write {}", path.display());
}
