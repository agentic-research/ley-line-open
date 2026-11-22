//! Linux (and any other non-macOS target): the standard library's
//! `Command` with a `pre_exec` hook (bead `ley-line-open-528525`). One file
//! per platform, so the mutation gate mutates each where it compiles
//! (`tools/mutants_diff.sh`).

use super::{ChildIo, Spawn, Spawned};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

fn stdio(io: ChildIo) -> Stdio {
    match io {
        ChildIo::Null => Stdio::null(),
        ChildIo::Piped => Stdio::piped(),
        ChildIo::Inherit => Stdio::inherit(),
    }
}

/// Every descriptor open in this process above the standard streams: the
/// fallback the child marks close-on-exec where `close_range` is missing.
pub(super) fn held() -> io::Result<Vec<RawFd>> {
    Ok(std::fs::read_dir("/proc/self/fd")?
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .filter(|fd: &RawFd| *fd > 2)
        .collect())
}

/// Mark every descriptor from 3 up close-on-exec with one
/// `close_range(3, ~0, CLOSE_RANGE_CLOEXEC)` (Linux 5.11+), a raw system
/// call and so async-signal-safe. `false` when the kernel refuses it
/// (`ENOSYS` before 5.11, `EINVAL` before 5.11's flag, `EPERM` under a
/// seccomp filter that does not know it): the caller falls back to its
/// listing.
fn close_range_cloexec() -> bool {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: a system call taking three integers and no memory.
        unsafe {
            libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                libc::c_uint::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            ) == 0
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

pub(super) fn spawn(request: &Spawn<'_>) -> io::Result<Spawned> {
    spawn_with(request, true, held()?)
}

/// [`spawn`], with the sweep chosen: `close_range` first when
/// `try_close_range` and the kernel has it, else `listing` (the descriptors
/// this process held just before the spawn). Split out so a test can tell
/// the two apart: only `close_range` reaches a descriptor the listing missed.
pub(super) fn spawn_with(
    request: &Spawn<'_>,
    try_close_range: bool,
    listing: Vec<RawFd>,
) -> io::Result<Spawned> {
    let mut command = Command::new(&request.program);
    command
        .args(&request.args)
        .stdin(stdio(request.stdio[0]))
        .stdout(stdio(request.stdio[1]))
        .stderr(stdio(request.stdio[2]));
    if request.process_group {
        command.process_group(0);
    }
    let inherit: Vec<(RawFd, RawFd)> = request
        .inherit
        .iter()
        .map(|&(from, to)| (from.as_raw_fd(), to))
        .collect();
    // SAFETY: the closure runs in the forked child before exec, allocates
    // nothing, and makes only the close_range system call, fcntl and dup2,
    // which are async-signal-safe. The standard streams are already in
    // place (the standard library sets them up before this hook).
    unsafe {
        command.pre_exec(move || {
            // Everything unnamed is close-on-exec from here on: by one
            // system call, or by the listing where the kernel lacks it.
            if !(try_close_range && close_range_cloexec()) {
                for &fd in &listing {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    // Closed since the listing: nothing to inherit.
                    if flags >= 0 {
                        libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                    }
                }
            }
            // Then exactly the named ones cross the exec. `Spawn::spawn`
            // refused a source that is another entry's target, so each
            // target receives the descriptor its entry names; `dup2` leaves
            // the new descriptor without close-on-exec.
            for &(from, to) in &inherit {
                if from == to {
                    let flags = libc::fcntl(to, libc::F_GETFD);
                    if flags < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    // Unchecked: `EBADF` is `F_SETFD`'s only failure, and
                    // `F_GETFD` has just found `to` open in this
                    // single-threaded child.
                    libc::fcntl(to, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
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
