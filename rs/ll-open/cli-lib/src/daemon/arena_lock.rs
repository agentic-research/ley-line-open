//! Arena admission control via advisory lockfile.
//!
//! Prevents two `leyline daemon --arena X` invocations from concurrently
//! mounting the same arena. Uses `flock(2)` with `LOCK_EX | LOCK_NB` so
//! the OS releases the lock automatically on process exit even if the
//! daemon crashes without running `Drop`.
//!
//! Bead: `ley-line-open-0cba88`.
//!
//! ## Why not idempotent-start via control-socket-ping
//!
//! The alternative was: connect to `--control`; if a live daemon
//! answers, no-op instead of failing. That's friendlier UX for
//! `task install`-style reruns but requires round-trip IPC before the
//! new daemon knows whether to proceed. flock is one syscall, no IPC,
//! and OS-managed cleanup on crash. Idempotent-start can layer on top
//! later — this is the correctness gate.
//!
//! ## Cross-process guarantee
//!
//! `flock` is a per-open-file-description lock. Two `open()` calls
//! from separate processes (or the same process) create separate
//! file descriptions and thus contend correctly. Kernel releases the
//! lock automatically when the file description is dropped, which
//! includes process exit (segfault, SIGKILL, panic without unwind).

#[cfg(unix)]
use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::io::{Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(unix)]
use std::path::{Path, PathBuf};

#[cfg(unix)]
use anyhow::{Context, Result, bail};

/// Guard object holding the arena admission lock. Drop releases the
/// lock (via file-descriptor drop) and unlinks the lockfile.
///
/// Keep this bound to a local variable for the daemon's entire runtime;
/// if the guard drops mid-run, another daemon can start.
#[cfg(unix)]
#[derive(Debug)]
pub struct ArenaLock {
    // Held open so the flock persists. Field is intentionally
    // unused after `try_acquire` — the drop side does the work.
    _file: File,
    path: PathBuf,
}

#[cfg(unix)]
impl ArenaLock {
    /// Try to acquire an exclusive advisory lock on `<arena>.lock`.
    ///
    /// On success, writes the current process's PID to the lockfile so
    /// subsequent failed attempts can identify the holder in the error
    /// message.
    ///
    /// On failure (another process holds the lock), reads the PID from
    /// the lockfile and returns an error naming that PID.
    pub fn try_acquire(arena: &Path) -> Result<Self> {
        let lock_path = lock_path_for(arena);

        // Ensure parent dir exists. If the arena's parent doesn't exist,
        // the daemon will fail to open the arena itself anyway — but
        // opening the lock file needs the parent up-front.
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create arena lock parent {}", parent.display()))?;
        }

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("open arena lock file {}", lock_path.display()))?;

        // LOCK_EX | LOCK_NB: exclusive, non-blocking. Returns -1 with
        // errno=EWOULDBLOCK if another process holds the lock.
        // SAFETY: `flock(2)` takes an owned raw fd and two int flags; no
        // pointers, no aliasing. `file.as_raw_fd()` is valid for the
        // duration of the `File` binding, which outlives this call.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let holding_pid = read_holding_pid(&lock_path);
            let holder = match holding_pid {
                Some(pid) => format!("PID {pid}"),
                None => "another daemon (PID unknown; lockfile empty or unreadable)".to_string(),
            };
            bail!(
                "arena {} is already held by {}; refusing to start a second daemon. \
                 See bead ley-line-open-0cba88.",
                arena.display(),
                holder,
            );
        }

        // We hold the lock. Write our PID so future attempts can
        // identify us. Failures here are non-fatal — the lock is held
        // even if we can't write the PID; the error message just
        // degrades to "another daemon (PID unknown)".
        let _ = file.set_len(0);
        let _ = file.seek(SeekFrom::Start(0));
        let _ = write!(file, "{}", std::process::id());
        let _ = file.flush();

        Ok(Self {
            _file: file,
            path: lock_path,
        })
    }
}

#[cfg(unix)]
impl Drop for ArenaLock {
    fn drop(&mut self) {
        // flock is released automatically when `_file` drops. Best-effort
        // unlink so leftover files don't accumulate; not fatal if it
        // fails (another daemon may have grabbed it in the tiny window
        // between our lock release and unlink).
        let _ = std::fs::remove_file(&self.path);
    }
}

// -----------------------------------------------------------------------------
// Non-unix stub — Windows doesn't have flock. Fall back to a no-op that
// always succeeds. Windows daemon support is out of scope for this bead;
// admission control there would need a different primitive (LockFileEx).
// -----------------------------------------------------------------------------

#[cfg(not(unix))]
pub struct ArenaLock;

#[cfg(not(unix))]
impl ArenaLock {
    pub fn try_acquire(_arena: &std::path::Path) -> anyhow::Result<Self> {
        Ok(Self)
    }
}

#[cfg(unix)]
fn lock_path_for(arena: &Path) -> PathBuf {
    let mut s = arena.as_os_str().to_owned();
    s.push(".lock");
    PathBuf::from(s)
}

#[cfg(unix)]
fn read_holding_pid(lock_path: &Path) -> Option<u32> {
    std::fs::read_to_string(lock_path).ok()?.trim().parse().ok()
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(all(unix, test))]
mod tests {
    use super::*;

    #[test]
    fn first_acquire_succeeds_on_fresh_arena() {
        let tmp = tempfile::TempDir::new().unwrap();
        let arena = tmp.path().join("fresh.arena");
        let _lock = ArenaLock::try_acquire(&arena).expect("first acquire");
    }

    #[test]
    fn second_acquire_fails_while_first_held() {
        let tmp = tempfile::TempDir::new().unwrap();
        let arena = tmp.path().join("contested.arena");
        let _first = ArenaLock::try_acquire(&arena).expect("first acquire");
        let second = ArenaLock::try_acquire(&arena);
        assert!(
            second.is_err(),
            "second acquire should fail while first held"
        );
        let msg = format!("{:#}", second.unwrap_err());
        assert!(
            msg.contains("already held"),
            "expected 'already held' in error, got: {msg}"
        );
    }

    #[test]
    fn error_message_names_holding_pid() {
        let tmp = tempfile::TempDir::new().unwrap();
        let arena = tmp.path().join("pid.arena");
        let _first = ArenaLock::try_acquire(&arena).expect("first acquire");
        let second = ArenaLock::try_acquire(&arena);
        let msg = format!("{:#}", second.unwrap_err());
        let our_pid = std::process::id();
        assert!(
            msg.contains(&format!("PID {our_pid}")),
            "expected 'PID {our_pid}' in error, got: {msg}"
        );
    }

    #[test]
    fn second_acquire_succeeds_after_first_drops() {
        let tmp = tempfile::TempDir::new().unwrap();
        let arena = tmp.path().join("released.arena");
        {
            let _first = ArenaLock::try_acquire(&arena).expect("first acquire");
        } // first drops here, releasing the flock
        let _second = ArenaLock::try_acquire(&arena).expect("second after first drop");
    }

    #[test]
    fn lockfile_unlinked_on_drop() {
        let tmp = tempfile::TempDir::new().unwrap();
        let arena = tmp.path().join("unlink.arena");
        let lock_path = lock_path_for(&arena);
        {
            let _lock = ArenaLock::try_acquire(&arena).expect("acquire");
            assert!(lock_path.exists(), "lockfile should exist while held");
        }
        assert!(
            !lock_path.exists(),
            "lockfile should be unlinked after drop"
        );
    }

    #[test]
    fn lock_path_is_arena_with_lock_suffix() {
        let arena = std::path::Path::new("/tmp/foo/bar.arena");
        let lock = lock_path_for(arena);
        assert_eq!(lock, std::path::PathBuf::from("/tmp/foo/bar.arena.lock"));
    }
}
