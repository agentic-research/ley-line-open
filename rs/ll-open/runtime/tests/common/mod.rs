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
