//! A lifeline pipe: a child learns that its supervisor is gone (ADR-0046 S7;
//! beads `ley-line-open-f963e6`, `ley-line-open-f6b525`).
//!
//! macOS has no `PR_SET_PDEATHSIG`, and a child reparented to launchd keeps
//! running. A lifeline gives the same guarantee on every platform. The
//! supervisor holds the only write end ([`Lifeline`]) and the child inherits
//! the read end. Nothing is ever written. When the supervisor exits, for any
//! reason including `SIGKILL`, the kernel closes the write end, the child's
//! read returns end-of-file, and the child ends itself ([`watch`]).
//!
//! The guarantee holds only while no other process holds a copy of the write
//! end. Both ends are created close-on-exec, and the read end crosses exactly
//! one exec, the child's ([`ReadEnd::pass_at`] or the spawner's own inherit
//! list). A concurrent spawn between `pipe(2)` and `FD_CLOEXEC` can still
//! inherit a copy until every daemon spawn uses `POSIX_SPAWN_CLOEXEC_DEFAULT`
//! (`ley-line-open-528525`).
//!
//! Used by the connection broker (`--lifeline-fd`), the libkrun worker
//! (`--lifeline-fd`) and the daemon a launcher starts (`ley-line-open-f6b525`).

use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::process::Command;

/// The supervisor's end. Holding it keeps the child alive; dropping it, or
/// this process ending, ends the child.
#[derive(Debug)]
pub struct Lifeline {
    _write_end: OwnedFd,
}

/// The end the child inherits. The spawner keeps it only until the child is
/// spawned, then drops it, so the child holds the only read end.
#[derive(Debug)]
pub struct ReadEnd {
    fd: OwnedFd,
}

/// A new lifeline: both ends close-on-exec. On Linux the pipe is created
/// close-on-exec (`pipe2(O_CLOEXEC)`); on macOS, which has no `pipe2`, the
/// flag is set just after, the window `ley-line-open-528525` closes.
pub fn pair() -> io::Result<(Lifeline, ReadEnd)> {
    let (read_end, write_end) = io::pipe()?;
    let (read_end, write_end) = (OwnedFd::from(read_end), OwnedFd::from(write_end));
    // Already set by the standard library on every platform; checked here
    // because the guarantee above rests on it.
    set_cloexec(read_end.as_raw_fd(), true)?;
    set_cloexec(write_end.as_raw_fd(), true)?;
    Ok((
        Lifeline {
            _write_end: write_end,
        },
        ReadEnd { fd: read_end },
    ))
}

/// The descriptor number a `--lifeline-fd` argument names: decimal digits
/// only, within `RawFd`. Anything else is refused by name, never read as "no
/// lifeline".
pub fn parse_fd(value: &str) -> Result<RawFd, String> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "--lifeline-fd {value:?} is not a file descriptor number"
        ));
    }
    value
        .parse()
        .map_err(|_| format!("--lifeline-fd {value:?} is not a file descriptor number"))
}

impl ReadEnd {
    /// The descriptor number in this process. A spawner that keeps
    /// descriptor numbers across exec (an inherit list) passes this number
    /// and adds it to that list.
    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Make the child of `command` hold the read end at `child_fd`, with
    /// close-on-exec cleared, and nothing else of this pipe. `self` must
    /// outlive `command.spawn()`.
    pub fn pass_at(&self, command: &mut Command, child_fd: RawFd) {
        let raw = self.fd.as_raw_fd();
        // SAFETY: dup2 and fcntl only, both async-signal-safe, in the child
        // before exec. dup2 clears close-on-exec on the new descriptor; when
        // the read end already is `child_fd`, dup2 does nothing, so the flag
        // is cleared explicitly.
        unsafe {
            use std::os::unix::process::CommandExt;
            command.pre_exec(move || {
                if raw == child_fd {
                    set_cloexec(child_fd, false)
                } else if libc::dup2(raw, child_fd) == child_fd {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            });
        }
    }
}

/// Whether one read of a lifeline means the supervisor is gone: end-of-file,
/// or any error but an interrupted call. A byte (which no supervisor writes)
/// changes nothing.
pub fn ended(read: &io::Result<usize>) -> bool {
    match read {
        Ok(0) => true,
        Ok(_) => false,
        Err(error) => error.kind() != io::ErrorKind::Interrupted,
    }
}

/// Watch the inherited read end `fd` on a thread of its own and run `on_end`
/// once the supervisor is gone.
///
/// Refused, and left untouched, unless `fd` is open for reading and is a pipe
/// or a socket: a wrong number fails at startup instead of reading as an
/// immediate end, and so does a descriptor that could never be a lifeline (a
/// regular file or `/dev/null` reads end-of-file at once, a write end fails
/// at once).
///
/// Once accepted, the watcher owns `fd` for the rest of the process's life.
pub fn watch(fd: RawFd, on_end: impl FnOnce() + Send + 'static) -> io::Result<()> {
    check_usable(fd)?;
    // The child's own exec must not hand the lifeline on.
    set_cloexec(fd, true)?;
    // SAFETY: the caller hands `fd` over; nothing else in this process uses
    // it from here on.
    let mut lifeline = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    std::thread::Builder::new()
        .name("lifeline".into())
        .spawn(move || {
            let mut byte = [0u8; 1];
            while !ended(&lifeline.read(&mut byte)) {}
            on_end();
        })?;
    Ok(())
}

/// [`watch`], ending this process at once (`_exit(status)`, no destructors,
/// no atexit handlers) when the supervisor is gone. Every thread, a VMM's
/// included, ends with it.
pub fn exit_when_ended(fd: RawFd, status: i32) -> io::Result<()> {
    watch(fd, move || {
        // SAFETY: _exit ends the process; it takes no pointers.
        unsafe { libc::_exit(status) }
    })
}

/// `Ok` when `fd` is open for reading and is a pipe (FIFO) or a socket.
fn check_usable(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on a descriptor number; failure is reported.
    let status = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if status < 0 {
        return Err(io::Error::last_os_error());
    }
    if status & libc::O_ACCMODE == libc::O_WRONLY {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("descriptor {fd} is not open for reading"),
        ));
    }
    // SAFETY: an out-parameter of the size fstat writes.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstat on a descriptor number into a live stat.
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    match stat.st_mode & libc::S_IFMT {
        libc::S_IFIFO | libc::S_IFSOCK => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("descriptor {fd} is not a pipe or socket"),
        )),
    }
}

fn set_cloexec(fd: RawFd, on: bool) -> io::Result<()> {
    // SAFETY: fcntl on a descriptor number; failure is reported.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let flags = if on {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::IntoRawFd;
    use std::sync::mpsc;

    #[test]
    fn a_read_ends_the_lifeline_on_eof_or_error_but_not_on_a_byte_or_eintr() {
        assert!(ended(&Ok(0)));
        assert!(!ended(&Ok(1)));
        assert!(ended(&Err(io::Error::from(io::ErrorKind::BrokenPipe))));
        assert!(!ended(&Err(io::Error::from(io::ErrorKind::Interrupted))));
    }

    #[test]
    fn both_ends_are_close_on_exec() {
        let (lifeline, read_end) = pair().unwrap();
        for fd in [lifeline._write_end.as_raw_fd(), read_end.raw_fd()] {
            // SAFETY: fcntl on a descriptor this test holds.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert_eq!(flags & libc::FD_CLOEXEC, libc::FD_CLOEXEC, "fd {fd}");
        }
    }

    #[test]
    fn dropping_the_lifeline_runs_on_end_and_holding_it_does_not() {
        let (lifeline, read_end) = pair().unwrap();
        // The watcher takes a descriptor number, as a child does.
        let fd = read_end.fd.as_raw_fd();
        std::mem::forget(read_end);
        let (sender, ended) = mpsc::channel();
        watch(fd, move || sender.send(()).unwrap()).unwrap();
        // SAFETY: fcntl on the descriptor the watcher now owns.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_eq!(flags & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
        assert!(
            ended.try_recv().is_err(),
            "ended while the lifeline is held"
        );
        drop(lifeline);
        ended.recv().expect("the watcher saw the lifeline end");
    }

    #[test]
    fn set_cloexec_sets_and_clears_only_that_flag_and_refuses_a_closed_fd() {
        let (_lifeline, read_end) = pair().unwrap();
        let fd = read_end.raw_fd();
        // SAFETY: fcntl on a descriptor this test holds.
        let flags = || unsafe { libc::fcntl(fd, libc::F_GETFD) };
        set_cloexec(fd, false).unwrap();
        assert_eq!(flags(), 0);
        set_cloexec(fd, false).unwrap();
        assert_eq!(flags(), 0, "clearing a clear flag keeps it clear");
        set_cloexec(fd, true).unwrap();
        set_cloexec(fd, true).unwrap();
        assert_eq!(flags(), libc::FD_CLOEXEC, "setting twice keeps one flag");
        assert!(set_cloexec(-1, true).is_err());
        assert!(set_cloexec(-1, false).is_err());
    }

    /// The child holds the read end at the number it was promised, without
    /// close-on-exec, whether or not the read end already had that number;
    /// and the child's hold is what keeps the pipe open.
    #[test]
    fn pass_at_places_the_read_end_at_the_child_number() {
        for same_number in [false, true] {
            let (lifeline, read_end) = pair().unwrap();
            let child_fd = if same_number { read_end.raw_fd() } else { 9 };
            let mut command = Command::new("/bin/sh");
            // Prints the flags the child sees on child_fd: open and inherited
            // is "ok"; the check fails if close-on-exec closed it at exec.
            command
                .arg("-c")
                .arg(format!("[ -e /dev/fd/{child_fd} ] && echo ok"))
                .stdout(std::process::Stdio::piped());
            read_end.pass_at(&mut command, child_fd);
            let output = command.output().unwrap();
            assert_eq!(
                String::from_utf8_lossy(&output.stdout).trim(),
                "ok",
                "same number: {same_number}"
            );
            drop(lifeline);
        }
    }

    /// A watched read end is not handed on: a process the child spawns after
    /// [`watch`] does not hold it, so the child's own children cannot keep
    /// its supervisor's lifeline open or read it.
    #[test]
    fn a_process_spawned_after_watch_does_not_inherit_the_lifeline() {
        let (lifeline, read_end) = pair().unwrap();
        let fd = read_end.raw_fd();
        // As the child holds it after exec: the read end without close-on-exec.
        set_cloexec(fd, false).unwrap();
        std::mem::forget(read_end);
        watch(fd, || {}).unwrap();
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("[ -e /dev/fd/{fd} ] && echo held || echo absent"))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "absent");
        drop(lifeline);
    }

    /// The write end never crosses an exec: a child given the read end does
    /// not hold the write end, so its exit is not what keeps it alive.
    #[test]
    fn a_child_given_the_read_end_does_not_hold_the_write_end() {
        let (lifeline, read_end) = pair().unwrap();
        let write_fd = lifeline._write_end.as_raw_fd();
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(format!(
            "[ -e /dev/fd/{write_fd} ] && echo held || echo absent"
        ));
        read_end.pass_at(&mut command, read_end.raw_fd());
        let output = command.output().unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "absent");
    }

    #[test]
    fn a_lifeline_argument_is_a_descriptor_number_or_refused_by_name() {
        assert_eq!(parse_fd("3"), Ok(3));
        assert_eq!(parse_fd("0"), Ok(0));
        for bad in ["", "-1", "three", "3 ", "+3", "99999999999"] {
            let error = parse_fd(bad).unwrap_err();
            assert!(error.contains("--lifeline-fd"), "{bad:?}: {error}");
        }
    }

    /// A descriptor that cannot be a lifeline's read end is refused, not
    /// watched: a regular file or `/dev/null` reads end-of-file at once, and a
    /// write end reads an error at once, so either would end the child the
    /// moment it started. Each stays open and unclaimed.
    #[test]
    fn watching_a_descriptor_that_is_not_a_readable_pipe_or_socket_is_refused() {
        let null = std::fs::File::open("/dev/null").unwrap();
        let error = watch(null.as_raw_fd(), || {}).unwrap_err();
        assert!(error.to_string().contains("pipe or socket"), "{error}");
        let (lifeline, read_end) = pair().unwrap();
        let error = watch(lifeline._write_end.as_raw_fd(), || {}).unwrap_err();
        assert!(
            error.to_string().contains("not open for reading"),
            "{error}"
        );
        // A socket's end is accepted, as a pipe's read end is.
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let (sender, ended) = mpsc::channel();
        watch(theirs.into_raw_fd(), move || sender.send(()).unwrap()).unwrap();
        drop(ours);
        ended
            .recv()
            .expect("a socket lifeline ends when its peer closes");
        // Refused descriptors are left as they were: still open.
        for fd in [null.as_raw_fd(), lifeline._write_end.as_raw_fd()] {
            // SAFETY: fcntl on descriptors this test holds.
            assert!(unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0, "fd {fd}");
        }
        drop(read_end);
    }

    #[test]
    fn watching_a_descriptor_that_is_not_open_is_refused() {
        // Numbers no process can hold: a closed number could be reused by a
        // concurrent test between the close and the check.
        for fd in [-1, i32::MAX] {
            assert!(watch(fd, || {}).is_err(), "fd {fd}");
        }
    }
}
