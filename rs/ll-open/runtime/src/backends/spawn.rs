//! The one way the daemon starts a child: workers and the connection broker
//! (ADR-0046 Slice 1 S7; bead `ley-line-open-528525`).
//!
//! A child inherits exactly what [`Spawn`] names: its three standard streams
//! and the descriptors in its inherit list, each at the number given. Nothing
//! else this process holds crosses the exec, whatever its close-on-exec flag.
//! That matters because the daemon starts runs concurrently, and on macOS
//! (which has no `pipe2`) every pipe is created first and marked close-on-exec
//! a moment later: a child spawned by another thread in that moment would
//! otherwise inherit, say, another run's lifeline write end and keep that
//! run's worker or broker alive after its supervisor died.
//!
//! - **macOS:** `posix_spawn` with `POSIX_SPAWN_CLOEXEC_DEFAULT`, so no
//!   descriptor is inherited unless a file action names it: `dup2` for the
//!   standard streams and for a descriptor moved to another number,
//!   `posix_spawn_file_actions_addinherit_np` for one kept at its own number.
//! - **Linux:** every pipe and socket the standard library makes is created
//!   close-on-exec atomically (`pipe2`, `SOCK_CLOEXEC`), so the window does
//!   not exist; the child marks every other descriptor it holds close-on-exec
//!   before exec, to the same effect for a descriptor some other code opened
//!   without the flag.
//!
//! Both keep the process-group setup (`POSIX_SPAWN_SETPGROUP` /
//! `Command::process_group`), reset `SIGPIPE` to its default in the child,
//! and give it an empty signal mask, as the standard library does.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

/// What a child's standard stream is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildIo {
    /// `/dev/null`.
    Null,
    /// A new pipe; this process keeps the other end ([`Spawned`]).
    Piped,
    /// This process's own stream.
    Inherit,
}

/// A child to start: program, arguments, standard streams, the descriptors
/// it inherits and whether it leads a process group of its own.
#[derive(Debug, Clone)]
pub struct Spawn {
    program: PathBuf,
    args: Vec<OsString>,
    stdio: [ChildIo; 3],
    inherit: Vec<(RawFd, RawFd)>,
    process_group: bool,
}

impl Spawn {
    /// `program`, by path (never searched for on `PATH`), with no arguments,
    /// its standard streams at `/dev/null`, inheriting nothing and in this
    /// process's group.
    pub fn new(program: impl AsRef<Path>) -> Self {
        Self {
            program: program.as_ref().to_path_buf(),
            args: Vec::new(),
            stdio: [ChildIo::Null; 3],
            inherit: Vec::new(),
            process_group: false,
        }
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    pub fn stdin(&mut self, io: ChildIo) -> &mut Self {
        self.stdio[0] = io;
        self
    }

    pub fn stdout(&mut self, io: ChildIo) -> &mut Self {
        self.stdio[1] = io;
        self
    }

    pub fn stderr(&mut self, io: ChildIo) -> &mut Self {
        self.stdio[2] = io;
        self
    }

    /// Hand the child this process's descriptor `fd` as its descriptor
    /// `child_fd` (the same number or another above 2). `fd` must stay open
    /// until [`Self::spawn`] returns.
    pub fn inherit(&mut self, fd: RawFd, child_fd: RawFd) -> &mut Self {
        self.inherit.push((fd, child_fd));
        self
    }

    /// Make the child the leader of a new process group, so the whole group
    /// can be signalled.
    pub fn process_group(&mut self) -> &mut Self {
        self.process_group = true;
        self
    }

    /// Start the child. Refused before anything is spawned when an inherited
    /// descriptor is a standard stream's number or named twice.
    pub fn spawn(&self) -> io::Result<Spawned> {
        let mut targets: Vec<RawFd> = self.inherit.iter().map(|&(_, to)| to).collect();
        targets.sort_unstable();
        let unique = {
            let mut seen = targets.clone();
            seen.dedup();
            seen.len()
        };
        if targets.iter().any(|&to| to <= 2) || unique != targets.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "a child inherits each descriptor once, above its standard streams: {:?}",
                    self.inherit
                ),
            ));
        }
        platform::spawn(self)
    }
}

/// A started child: its pid, the parent's ends of its piped standard
/// streams, and its status once reaped.
#[derive(Debug)]
pub struct Spawned {
    pid: u32,
    pub stdin: Option<File>,
    pub stdout: Option<File>,
    pub stderr: Option<File>,
    status: Option<ExitStatus>,
}

impl Spawned {
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Wait for the child to exit, and reap it. Again after that, the same
    /// status.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        let status = waitpid(self.pid, 0)?.expect("a blocking waitpid answers a status");
        self.status = Some(status);
        Ok(status)
    }

    /// The child's status if it has exited (and reaps it), `None` while it
    /// runs.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        let status = waitpid(self.pid, libc::WNOHANG)?;
        self.status = status;
        Ok(status)
    }

    /// `SIGKILL` the child, unless it was already reaped.
    pub fn kill(&mut self) -> io::Result<()> {
        if self.status.is_some() {
            return Ok(());
        }
        // SAFETY: a signal to this process's own, unreaped child.
        if unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGKILL) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// `waitpid(pid, options)`, retried on `EINTR`: `None` when `WNOHANG` found
/// the child still running.
fn waitpid(pid: u32, options: libc::c_int) -> io::Result<Option<ExitStatus>> {
    use std::os::unix::process::ExitStatusExt;
    loop {
        let mut status = 0;
        // SAFETY: waitpid on this process's own child, into a live int.
        let rc = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, options) };
        match rc {
            -1 => {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
            0 => return Ok(None),
            _ => return Ok(Some(ExitStatus::from_raw(status))),
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{ChildIo, Spawn, Spawned};
    use std::ffi::CString;
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        /// Keep `fd` open, at its own number, in a child spawned with
        /// `POSIX_SPAWN_CLOEXEC_DEFAULT` (`<spawn.h>`, macOS 10.7+; not in
        /// the `libc` crate).
        fn posix_spawn_file_actions_addinherit_np(
            actions: *mut libc::posix_spawn_file_actions_t,
            fd: libc::c_int,
        ) -> libc::c_int;
    }

    /// A `posix_spawn*` return value: 0 or the error number.
    fn check(rc: libc::c_int) -> io::Result<()> {
        match rc {
            0 => Ok(()),
            errno => Err(io::Error::from_raw_os_error(errno)),
        }
    }

    struct Actions(libc::posix_spawn_file_actions_t);
    impl Drop for Actions {
        fn drop(&mut self) {
            // SAFETY: initialized in `spawn`, destroyed once.
            unsafe { libc::posix_spawn_file_actions_destroy(&mut self.0) };
        }
    }

    struct Attributes(libc::posix_spawnattr_t);
    impl Drop for Attributes {
        fn drop(&mut self) {
            // SAFETY: initialized in `spawn`, destroyed once.
            unsafe { libc::posix_spawnattr_destroy(&mut self.0) };
        }
    }

    pub(super) fn spawn(request: &Spawn) -> io::Result<Spawned> {
        let program = CString::new(request.program.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "program holds NUL"))?;
        let mut argv_owned = vec![program.clone()];
        for arg in &request.args {
            argv_owned.push(CString::new(arg.as_bytes()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "an argument holds NUL")
            })?);
        }
        let mut argv: Vec<*mut libc::c_char> = argv_owned
            .iter()
            .map(|arg| arg.as_ptr() as *mut _)
            .collect();
        argv.push(std::ptr::null_mut());

        // SAFETY: zeroed storage the init calls overwrite; each is destroyed
        // by its guard.
        let mut actions = Actions(unsafe { std::mem::zeroed() });
        check(unsafe { libc::posix_spawn_file_actions_init(&mut actions.0) })?;
        let mut attributes = Attributes(unsafe { std::mem::zeroed() });
        check(unsafe { libc::posix_spawnattr_init(&mut attributes.0) })?;

        // The standard streams: the parent's ends are kept, the child's are
        // moved onto 0, 1, 2 (a dup2 action makes its target inherited).
        let mut parent_ends: [Option<File>; 3] = [None, None, None];
        let mut child_ends: Vec<OwnedFd> = Vec::new();
        let null = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")?;
        for (number, io) in request.stdio.iter().enumerate() {
            let number = number as libc::c_int;
            let source = match io {
                ChildIo::Null => null.as_raw_fd(),
                ChildIo::Inherit => number,
                ChildIo::Piped => {
                    let (reader, writer) = io::pipe()?;
                    let (parent, child): (OwnedFd, OwnedFd) = if number == 0 {
                        (writer.into(), reader.into())
                    } else {
                        (reader.into(), writer.into())
                    };
                    parent_ends[number as usize] = Some(File::from(parent));
                    let raw = child.as_raw_fd();
                    child_ends.push(child);
                    raw
                }
            };
            // SAFETY: valid actions and descriptor numbers.
            check(unsafe {
                if source == number {
                    posix_spawn_file_actions_addinherit_np(&mut actions.0, number)
                } else {
                    libc::posix_spawn_file_actions_adddup2(&mut actions.0, source, number)
                }
            })?;
        }
        for &(from, to) in &request.inherit {
            // SAFETY: valid actions and descriptor numbers.
            check(unsafe {
                if from == to {
                    posix_spawn_file_actions_addinherit_np(&mut actions.0, to)
                } else {
                    libc::posix_spawn_file_actions_adddup2(&mut actions.0, from, to)
                }
            })?;
        }

        let mut flags = libc::POSIX_SPAWN_CLOEXEC_DEFAULT
            | libc::POSIX_SPAWN_SETSIGDEF
            | libc::POSIX_SPAWN_SETSIGMASK;
        if request.process_group {
            flags |= libc::POSIX_SPAWN_SETPGROUP;
            // SAFETY: valid attributes; 0 means "the child's own pid".
            check(unsafe { libc::posix_spawnattr_setpgroup(&mut attributes.0, 0) })?;
        }
        // SAFETY: sigset_t storage the calls initialize.
        unsafe {
            let mut default: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut default);
            libc::sigaddset(&mut default, libc::SIGPIPE);
            check(libc::posix_spawnattr_setsigdefault(
                &mut attributes.0,
                &default,
            ))?;
            let mut empty: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut empty);
            check(libc::posix_spawnattr_setsigmask(&mut attributes.0, &empty))?;
            check(libc::posix_spawnattr_setflags(
                &mut attributes.0,
                flags as libc::c_short,
            ))?;
        }

        let mut pid: libc::pid_t = 0;
        // SAFETY: argv is NUL-terminated and outlives the call; the
        // environment is this process's, as `Command` passes it by default.
        check(unsafe {
            libc::posix_spawn(
                &mut pid,
                program.as_ptr(),
                &actions.0,
                &attributes.0,
                argv.as_ptr(),
                *libc::_NSGetEnviron(),
            )
        })?;
        drop(child_ends);
        let [stdin, stdout, stderr] = parent_ends;
        Ok(Spawned {
            pid: pid as u32,
            stdin,
            stdout,
            stderr,
            status: None,
        })
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::{ChildIo, Spawn, Spawned};
    use std::fs::File;
    use std::io;
    use std::os::fd::OwnedFd;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    fn stdio(io: ChildIo) -> Stdio {
        match io {
            ChildIo::Null => Stdio::null(),
            ChildIo::Piped => Stdio::piped(),
            ChildIo::Inherit => Stdio::inherit(),
        }
    }

    /// Every descriptor open in this process above the standard streams.
    fn held() -> io::Result<Vec<i32>> {
        Ok(std::fs::read_dir("/proc/self/fd")?
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
            .filter(|fd: &i32| *fd > 2)
            .collect())
    }

    pub(super) fn spawn(request: &Spawn) -> io::Result<Spawned> {
        let mut command = Command::new(&request.program);
        command
            .args(&request.args)
            .stdin(stdio(request.stdio[0]))
            .stdout(stdio(request.stdio[1]))
            .stderr(stdio(request.stdio[2]));
        if request.process_group {
            command.process_group(0);
        }
        let held = held()?;
        let inherit = request.inherit.clone();
        // SAFETY: the closure runs in the forked child before exec and makes
        // only fcntl and dup2 calls, which are async-signal-safe.
        unsafe {
            command.pre_exec(move || {
                for &fd in &held {
                    if !inherit.iter().any(|&(_, to)| to == fd) {
                        let flags = libc::fcntl(fd, libc::F_GETFD);
                        if flags >= 0 {
                            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                        }
                    }
                }
                for &(from, to) in &inherit {
                    if from == to {
                        let flags = libc::fcntl(to, libc::F_GETFD);
                        if flags < 0
                            || libc::fcntl(to, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0
                        {
                            return Err(io::Error::last_os_error());
                        }
                    } else if libc::dup2(from, to) != to {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let spawned = Spawned {
            pid: child.id(),
            stdin: child.stdin.take().map(|end| File::from(OwnedFd::from(end))),
            stdout: child
                .stdout
                .take()
                .map(|end| File::from(OwnedFd::from(end))),
            stderr: child
                .stderr
                .take()
                .map(|end| File::from(OwnedFd::from(end))),
            status: None,
        };
        // Reaped through `Spawned`; dropping a `Child` neither waits nor
        // signals.
        drop(child);
        Ok(spawned)
    }
}
