//! The one way the daemon starts a child: workers, the connection broker and
//! the tools it runs (`codesign`, `hdiutil`) (ADR-0046 Slice 1 S7; bead
//! `ley-line-open-528525`).
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
//! The program is run by its absolute path, never looked up on `PATH` or in
//! the working directory, and the inherit list is checked before anything is
//! spawned (each refusal names what is wrong): no target on a standard
//! stream's number or named twice, no source that is a standard stream (the
//! child's new one by then), and no source that is another entry's target,
//! since the moves are applied in order.
//!
//! - **macOS** (`spawn/macos.rs`): `posix_spawn` with
//!   `POSIX_SPAWN_CLOEXEC_DEFAULT`, so no descriptor is inherited unless a
//!   file action names it: `dup2` for the standard streams and for a
//!   descriptor moved to another number,
//!   `posix_spawn_file_actions_addinherit_np` for one kept at its own number.
//!   The kernel builds the child from the file actions; the child never runs
//!   holding anything else.
//! - **Linux** (`spawn/linux.rs`): every pipe and socket the standard library
//!   makes is created close-on-exec atomically (`pipe2`, `SOCK_CLOEXEC`), so
//!   there is no moment in which such a descriptor exists without the flag.
//!   Before exec the child marks every descriptor from 3 up close-on-exec
//!   with `close_range(3, ~0, CLOSE_RANGE_CLOEXEC)`, then clears the flag on,
//!   or `dup2`s, exactly the named ones: the same effect as macOS for a
//!   descriptor some other code opened without the flag. Where the kernel
//!   refuses `close_range` (before 5.11, or under a seccomp filter that does
//!   not know it) the child marks instead the descriptors this process held
//!   when it listed `/proc/self/fd` just before the spawn; one opened
//!   without the flag by another thread after that listing and before the
//!   fork is then not covered.
//!
//! **The Linux fork-to-exec hold.** A Linux spawn forks (the standard
//! library's `pre_exec` path). From the fork until its exec, the child is a
//! copy of this process and holds every descriptor this process held at the
//! fork, other runs' lifeline write ends among them. Those copies are
//! close-on-exec and close at the child's exec (or its exit, if the exec
//! fails). [`Spawn::spawn`] returns only after that (the standard library
//! blocks on a close-on-exec socket until the child execs), except in one
//! case: an entry moved to a number free in this process can land on that
//! socket's number in the child and replace it, and the spawn then returns
//! at that `dup2` instead, reporting a failed exec as the child's abnormal
//! exit rather than as an error. The hold is transient and ends at the
//! exec, but it is real: a write end closed (or a supervisor that died)
//! while children forked before that are still between fork and exec is
//! seen at its far end only once the last of them has exec'd. After every
//! exec, a child holds exactly what it was named, as on macOS.
//!
//! Both keep the process-group setup (`POSIX_SPAWN_SETPGROUP` /
//! `Command::process_group`), reset `SIGPIPE` to its default in the child,
//! and give it an empty signal mask, as the standard library does.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{BorrowedFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output};

#[cfg(not(target_os = "macos"))]
mod linux;
#[cfg(not(target_os = "macos"))]
use linux as platform;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;

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
/// it inherits and whether it leads a process group of its own. Each
/// inherited descriptor is borrowed for `'fd`, so it stays open until the
/// spawn is done with it.
#[derive(Debug, Clone)]
pub struct Spawn<'fd> {
    program: PathBuf,
    args: Vec<OsString>,
    stdio: [ChildIo; 3],
    inherit: Vec<(BorrowedFd<'fd>, RawFd)>,
    process_group: bool,
}

impl<'fd> Spawn<'fd> {
    /// `program`, which must be an absolute path (it is never looked up on
    /// `PATH` or in the working directory; [`Self::spawn`] refuses another),
    /// with no arguments, its standard streams at `/dev/null`, inheriting
    /// nothing and in this process's group.
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

    pub fn args<I: IntoIterator<Item = A>, A: AsRef<OsStr>>(&mut self, args: I) -> &mut Self {
        for arg in args {
            self.arg(arg);
        }
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
    /// `child_fd`: the same number, or another above the standard streams.
    pub fn inherit(&mut self, fd: BorrowedFd<'fd>, child_fd: RawFd) -> &mut Self {
        self.inherit.push((fd, child_fd));
        self
    }

    /// Make the child the leader of a new process group, so the whole group
    /// can be signalled.
    pub fn process_group(&mut self) -> &mut Self {
        self.process_group = true;
        self
    }

    /// Start the child. Refused, by name and before anything is spawned,
    /// when the program is no absolute path or the inherit list would not
    /// hand over exactly what it names (see the module documentation).
    pub fn spawn(&self) -> io::Result<Spawned> {
        self.refusal().map_or(Ok(()), |reason| {
            Err(io::Error::new(io::ErrorKind::InvalidInput, reason))
        })?;
        platform::spawn(self)
    }

    /// Start the child with its standard output and error piped, collect
    /// both and wait for it, as `std::process::Command::output` does.
    pub fn output(&self) -> io::Result<Output> {
        use std::io::Read;
        let mut spawn = self.clone();
        spawn.stdout(ChildIo::Piped).stderr(ChildIo::Piped);
        let mut child = spawn.spawn()?;
        // On its own thread, so neither stream blocks the child while the
        // other is read.
        let stderr = child.stderr.take().map(|mut pipe| {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                pipe.read_to_end(&mut bytes).map(|_| bytes)
            })
        });
        let mut stdout = Vec::new();
        let read = child
            .stdout
            .take()
            .map_or(Ok(0), |mut pipe| pipe.read_to_end(&mut stdout));
        let stderr = match stderr {
            Some(reader) => reader
                .join()
                .unwrap_or_else(|_| Err(io::Error::other("stderr reader panicked"))),
            None => Ok(Vec::new()),
        };
        let status = child.wait()?;
        read?;
        Ok(Output {
            status,
            stdout,
            stderr: stderr?,
        })
    }

    /// Why this spawn would not run exactly `program` holding exactly what
    /// it names, or `None`.
    fn refusal(&self) -> Option<String> {
        use std::os::fd::AsRawFd;
        if !self.program.is_absolute() {
            return Some(format!(
                "program {} is not an absolute path: a spawn never looks one up on PATH or in \
                 the working directory",
                self.program.display()
            ));
        }
        for (index, &(from, to)) in self.inherit.iter().enumerate() {
            let from = from.as_raw_fd();
            if to <= 2 {
                return Some(format!(
                    "descriptor {from} cannot be inherited as {to}, a standard stream's number"
                ));
            }
            if from <= 2 {
                return Some(format!(
                    "descriptor {from} cannot be inherited: it is a standard stream, which the \
                     child's own replaces first"
                ));
            }
            for (other, &(_, other_to)) in self.inherit.iter().enumerate() {
                if other == index {
                    continue;
                }
                if other_to == to {
                    return Some(format!("child descriptor {to} is named twice"));
                }
                if other_to == from {
                    return Some(format!(
                        "descriptor {from} cannot be inherited as {to}: another entry places a \
                         descriptor at {from}, and the moves are applied in order"
                    ));
                }
            }
        }
        None
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether `pid` leads its own process group.
    fn leads_its_group(pid: u32) -> bool {
        // SAFETY: getpgid reads a process's group.
        unsafe { libc::getpgid(pid as libc::pid_t) == pid as libc::pid_t }
    }

    #[test]
    fn a_process_group_child_leads_its_own_group_and_another_does_not() {
        for own_group in [true, false] {
            let mut spawn = Spawn::new("/bin/cat");
            spawn.stdin(ChildIo::Piped);
            if own_group {
                spawn.process_group();
            }
            let mut child = spawn.spawn().unwrap();
            assert_eq!(
                leads_its_group(child.id()),
                own_group,
                "own group: {own_group}"
            );
            drop(child.stdin.take());
            assert!(child.wait().unwrap().success());
        }
    }

    /// Piped streams reach the child, and the child's exit status comes
    /// back; waiting again answers the same status.
    #[test]
    fn piped_streams_carry_bytes_and_wait_reports_the_status() {
        use std::io::{Read, Write};
        let mut spawn = Spawn::new("/bin/sh");
        spawn
            .arg("-c")
            .arg("read line; echo \"got $line\"; echo err >&2; exit 3")
            .stdin(ChildIo::Piped)
            .stdout(ChildIo::Piped)
            .stderr(ChildIo::Piped);
        let mut child = spawn.spawn().unwrap();
        child.stdin.take().unwrap().write_all(b"hello\n").unwrap();
        let mut out = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        let mut err = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut err)
            .unwrap();
        assert_eq!(out, "got hello\n");
        assert_eq!(err, "err\n");
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(3));
        assert_eq!(child.wait().unwrap().code(), Some(3));
        assert_eq!(child.try_wait().unwrap().map(|s| s.code()), Some(Some(3)));
    }

    /// `try_wait` answers `None` while the child runs, `kill` ends it, and a
    /// kill after the reap does nothing.
    #[test]
    fn try_wait_kill_and_reap() {
        use std::os::unix::process::ExitStatusExt;
        let mut spawn = Spawn::new("/bin/cat");
        spawn.stdin(ChildIo::Piped);
        let mut child = spawn.spawn().unwrap();
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        child.kill().unwrap();
    }

    /// A child that cannot be started is an error, not a pid.
    #[test]
    fn a_program_that_cannot_run_is_an_error() {
        assert!(Spawn::new("/nonexistent/program").spawn().is_err());
    }

    /// The child's standard input is `/dev/null` unless asked otherwise, and
    /// `SIGPIPE` is at its default: a child writing into a pipe nobody reads
    /// dies of it, as one started by the standard library does.
    #[test]
    fn a_child_gets_dev_null_and_the_default_sigpipe() {
        use std::io::Read;
        use std::os::unix::process::ExitStatusExt;
        let mut spawn = Spawn::new("/bin/sh");
        spawn
            .arg("-c")
            .arg("if read x; then echo read; else echo eof; fi")
            .stdout(ChildIo::Piped);
        let mut child = spawn.spawn().unwrap();
        let mut out = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        assert_eq!(out, "eof\n");
        child.wait().unwrap();

        let mut spawn = Spawn::new("/bin/sh");
        spawn
            .arg("-c")
            .arg("while :; do echo x; done")
            .stdout(ChildIo::Piped);
        let mut child = spawn.spawn().unwrap();
        drop(child.stdout.take());
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGPIPE));
    }

    /// `output` collects both streams and the status, as
    /// `Command::output` does, with the standard input at `/dev/null`.
    #[test]
    fn output_collects_both_streams_and_the_status() {
        let mut spawn = Spawn::new("/bin/sh");
        spawn.args([
            "-c",
            "if read x; then echo read; else echo eof; fi; echo err >&2; exit 4",
        ]);
        let output = spawn.output().unwrap();
        assert_eq!(output.stdout, b"eof\n");
        assert_eq!(output.stderr, b"err\n");
        assert_eq!(output.status.code(), Some(4));
        assert!(Spawn::new("/nonexistent/program").output().is_err());
    }

    /// The listing the Linux fallback marks: every descriptor open here
    /// above the standard streams, and none of those.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_listing_holds_every_descriptor_above_the_standard_streams() {
        use std::os::fd::AsRawFd;
        let stray = unnamed_descriptor();
        let held = linux::held().unwrap();
        assert!(held.contains(&stray.as_raw_fd()), "{held:?}");
        for standard in 0..=2 {
            assert!(!held.contains(&standard), "{held:?}");
        }
    }

    /// A descriptor held here without close-on-exec, at a number nothing
    /// names, far above the standard streams.
    #[cfg(not(target_os = "macos"))]
    fn unnamed_descriptor() -> std::os::fd::OwnedFd {
        use std::os::fd::{AsRawFd, FromRawFd};
        let null = std::fs::File::open("/dev/null").unwrap();
        // SAFETY: F_DUPFD duplicates a live descriptor, leaving
        // close-on-exec clear; the result is owned below.
        let raw = unsafe { libc::fcntl(null.as_raw_fd(), libc::F_DUPFD, 64) };
        assert!(raw >= 64);
        // SAFETY: a fresh descriptor nothing else owns.
        unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) }
    }

    /// Whether a child spawned with this sweep holds `fd` after its exec,
    /// while a descriptor it is named at its own number still reaches it.
    #[cfg(not(target_os = "macos"))]
    fn child_holds(
        fd: RawFd,
        close_range: Option<(libc::c_uint, libc::c_uint)>,
        listing: Vec<RawFd>,
    ) -> bool {
        use std::io::Read;
        use std::os::fd::{AsFd, AsRawFd};
        let (_reader, named) = std::io::pipe().unwrap();
        // Without close-on-exec, so naming it must leave it that way.
        // SAFETY: fcntl on a live descriptor this test owns.
        unsafe { libc::fcntl(named.as_raw_fd(), libc::F_SETFD, 0) };
        let mut spawn = Spawn::new("/bin/sh");
        spawn
            .arg("-c")
            .arg(format!(
                "[ -e /proc/self/fd/{} ] || exit 9; [ -e /proc/self/fd/{fd} ] && echo held || echo absent",
                named.as_raw_fd()
            ))
            .stdout(ChildIo::Piped)
            .inherit(named.as_fd(), named.as_raw_fd());
        let mut child = linux::spawn_with(&spawn, close_range, listing).unwrap();
        let mut out = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        assert!(
            child.wait().unwrap().success(),
            "the named descriptor reached the child"
        );
        match out.as_str() {
            "held\n" => true,
            "absent\n" => false,
            other => panic!("{other:?}"),
        }
    }

    /// Linux: `close_range` keeps from the child a descriptor the listing
    /// missed (one opened after it); without `close_range`, or when the
    /// kernel refuses it, the listing keeps one it holds; with neither, the
    /// descriptor crosses the exec, which is the gap the module documentation
    /// names. Needs Linux 5.11+ and no seccomp filter refusing `close_range`.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn close_range_and_the_listing_each_keep_an_unnamed_descriptor_from_the_child() {
        use std::os::fd::AsRawFd;
        let every = Some(linux::EVERY_DESCRIPTOR_ABOVE_THE_STANDARD_STREAMS);
        // First past last: `close_range` answers EINVAL and marks nothing.
        let refused = Some((libc::c_uint::MAX, 3));
        let stray = unnamed_descriptor();
        let fd = stray.as_raw_fd();
        assert!(
            !child_holds(fd, every, Vec::new()),
            "close_range (Linux 5.11+) marks what no listing named"
        );
        assert!(
            !child_holds(fd, None, vec![fd]),
            "the listing marks what it names"
        );
        assert!(
            !child_holds(fd, refused, vec![fd]),
            "a refused close_range falls back to the listing"
        );
        assert!(
            child_holds(fd, None, Vec::new()),
            "nothing marked it: it crosses"
        );
        assert!(
            child_holds(fd, refused, Vec::new()),
            "a refused close_range marked nothing"
        );
        // One already close-on-exec stays so when the listing names it.
        let (_reader, cloexec) = std::io::pipe().unwrap();
        let cloexec = cloexec.as_raw_fd();
        assert!(
            !child_holds(cloexec, None, vec![cloexec]),
            "kept close-on-exec"
        );
    }

    /// Reap `child` behind its back, so its pid names no child of this
    /// process any more.
    fn reap_behind_its_back(child: &Spawned) {
        let mut status = 0;
        // SAFETY: waitpid on this process's own child, into a live int.
        let rc = unsafe { libc::waitpid(child.id() as libc::pid_t, &mut status, 0) };
        assert_eq!(rc, child.id() as libc::pid_t);
    }

    /// `wait` and `try_wait` report `waitpid`'s error (`ECHILD` for a pid
    /// that is no child of this process) instead of a status, and `kill`
    /// reports `kill`'s (`ESRCH` once the pid is reaped).
    #[test]
    fn wait_try_wait_and_kill_report_the_system_call_errors() {
        let mut child = Spawn::new("/bin/cat").spawn().unwrap();
        reap_behind_its_back(&child);
        let error = child.wait().expect_err("no such child");
        assert_eq!(error.raw_os_error(), Some(libc::ECHILD), "{error}");
        let error = child.try_wait().expect_err("no such child");
        assert_eq!(error.raw_os_error(), Some(libc::ECHILD), "{error}");
        let error = child.kill().expect_err("no such process");
        assert_eq!(error.raw_os_error(), Some(libc::ESRCH), "{error}");
    }
}
