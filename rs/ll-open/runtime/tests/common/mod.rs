//! Shared fixtures for the runtime integration-test binaries.
//!
//! Lives in `tests/common/` rather than `tests/`: cargo builds a test binary
//! per top-level file in `tests/`, and a subdirectory module is compiled INTO
//! each binary that declares it instead of becoming a binary of its own.

/// Run one test at a time within the CALLING test binary.
///
/// `execve` fails with `ETXTBSY` ("Text file busy") when any process holds the
/// target inode open for writing. These suites write executable fixtures and
/// spawn workers from them, libtest runs tests on parallel threads, and the
/// runtime forks children constantly. So:
///
///   1. thread A calls `fs::write` on its fixture — a write fd is open on
///      inode X;
///   2. thread B forks, and the child inherits A's descriptor to inode X;
///   3. `O_CLOEXEC` closes that descriptor when the child execs — but the
///      ETXTBSY check happens DURING `execve`, and other children can still be
///      between `fork` and `exec`;
///   4. A's own spawn of inode X then finds a live writer and fails.
///
/// The window is (a write fd open) x (any other thread's fork). Serializing the
/// binary removes the second factor, the only one under our control.
///
/// **Each binary gets its own lock, and that is correct, not a limitation.**
/// The module is compiled separately into every test binary, so the `static`
/// below is per-binary — and descriptors are inherited across `fork` within ONE
/// process, while cargo runs each test binary as its own process. A lock shared
/// across binaries would serialize suites that cannot affect each other.
///
/// A previous attempt published the fixture by `rename`, on the theory that the
/// exec target would then be an inode nobody had opened for writing. That was
/// wrong: `rename(2)` rewrites a directory entry and PRESERVES the inode, so
/// the file being exec'd is the very one just written. It narrowed the window
/// and was reported as closing it (bead `ley-line-open-cdfaf4`).
///
/// Poison is deliberately ignored. If one test panics while holding the lock,
/// re-panicking here would replace every subsequent failure with a
/// `PoisonError` and hide the assertion that actually broke.
///
/// Cheap enough not to reason about: the largest of these suites runs in ~3s.
pub fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Wait, at most `bound`, for the process `pid` to exit, as a kernel event:
/// `EVFILT_PROC`/`NOTE_EXIT` on macOS, a pidfd turning readable on Linux.
/// For a process that is not this one's child, whose exit `waitpid` cannot
/// see: a worker orphaned when the process that spawned it was killed.
/// `Ok` once it has exited (or no longer exists); `Err` names the timeout.
#[allow(dead_code)]
pub fn await_exit(pid: libc::pid_t, bound: std::time::Duration) -> Result<(), String> {
    let timeout = libc::timespec {
        tv_sec: bound.as_secs() as libc::time_t,
        tv_nsec: bound.subsec_nanos() as _,
    };
    #[cfg(target_os = "macos")]
    {
        // SAFETY: kqueue takes no arguments; the descriptor is closed below.
        let queue = unsafe { libc::kqueue() };
        assert!(queue >= 0, "kqueue: {}", std::io::Error::last_os_error());
        let change = libc::kevent {
            ident: pid as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        let mut event = change;
        let result = loop {
            // SAFETY: one change and one event slot, both live; a timeout of
            // the size kevent reads.
            let n = unsafe { libc::kevent(queue, &change, 1, &mut event, 1, &timeout) };
            let error = std::io::Error::last_os_error();
            break match n {
                1 if event.flags & libc::EV_ERROR != 0 && event.data == libc::ESRCH as isize => {
                    Ok(())
                }
                1 if event.flags & libc::EV_ERROR != 0 => Err(format!(
                    "kevent on pid {pid}: {}",
                    std::io::Error::from_raw_os_error(event.data as i32)
                )),
                1 => Ok(()),
                0 => Err(format!("TIMEOUT: pid {pid} did not exit within {bound:?}")),
                _ if error.raw_os_error() == Some(libc::EINTR) => continue,
                _ if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
                _ => Err(format!("kevent on pid {pid}: {error}")),
            };
        };
        // SAFETY: the queue this function opened.
        unsafe { libc::close(queue) };
        result
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: pidfd_open takes a pid and flags; the descriptor is closed
        // below.
        let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
        if pidfd < 0 {
            let error = std::io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(libc::ESRCH) => Ok(()),
                _ => Err(format!("pidfd_open({pid}): {error}")),
            };
        }
        let millis = i32::try_from(bound.as_millis()).unwrap_or(i32::MAX);
        let _ = timeout;
        let result = loop {
            let mut poll = libc::pollfd {
                fd: pidfd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one live pollfd.
            let n = unsafe { libc::poll(&mut poll, 1, millis) };
            let error = std::io::Error::last_os_error();
            break match n {
                1 => Ok(()),
                0 => Err(format!("TIMEOUT: pid {pid} did not exit within {bound:?}")),
                _ if error.raw_os_error() == Some(libc::EINTR) => continue,
                _ => Err(format!("poll on the pidfd of {pid}: {error}")),
            };
        };
        // SAFETY: the pidfd this function opened.
        unsafe { libc::close(pidfd) };
        result
    }
}
