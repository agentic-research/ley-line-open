//! macOS: `posix_spawn` with `POSIX_SPAWN_CLOEXEC_DEFAULT` (bead
//! `ley-line-open-528525`). One file per platform, so the mutation gate
//! mutates each where it compiles (`tools/mutants_diff.sh`).

use super::{ChildIo, Spawn, Spawned};
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;

// SAFETY: the declaration matches `<spawn.h>` on macOS 10.7+:
// `int posix_spawn_file_actions_addinherit_np(posix_spawn_file_actions_t
// *, int)`. It is called only with an initialized `Actions` and a
// descriptor number, and its return value is checked.
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

/// A libc object and the function that destroys it, run exactly once, when
/// this drops: on every return from [`spawn`], an early error among them.
/// One guard for both `posix_spawn` objects, with the destroyer a value, so
/// a test can see it run (the block libc frees is not otherwise observable).
struct Destroyed<T>(T, unsafe extern "C" fn(*mut T) -> libc::c_int);
impl<T> Drop for Destroyed<T> {
    fn drop(&mut self) {
        // SAFETY: the object was initialized before it was wrapped (`spawn`
        // wraps each only after its init call succeeded) and is destroyed
        // here only.
        unsafe { (self.1)(&mut self.0) };
    }
}

pub(super) fn spawn(request: &Spawn<'_>) -> io::Result<Spawned> {
    let program = CString::new(request.program.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "program holds NUL"))?;
    let mut argv_owned = vec![program.clone()];
    for arg in &request.args {
        argv_owned.push(
            CString::new(arg.as_bytes()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "an argument holds NUL")
            })?,
        );
    }
    let mut argv: Vec<*mut libc::c_char> = argv_owned
        .iter()
        .map(|arg| arg.as_ptr() as *mut _)
        .collect();
    argv.push(std::ptr::null_mut());

    // SAFETY: zeroed storage the init calls overwrite. Each is wrapped, and
    // so destroyed, only once its init has succeeded.
    let mut actions: libc::posix_spawn_file_actions_t = unsafe { std::mem::zeroed() };
    check(unsafe { libc::posix_spawn_file_actions_init(&mut actions) })?;
    let mut actions = Destroyed(actions, libc::posix_spawn_file_actions_destroy);
    let mut attributes: libc::posix_spawnattr_t = unsafe { std::mem::zeroed() };
    check(unsafe { libc::posix_spawnattr_init(&mut attributes) })?;
    let mut attributes = Destroyed(attributes, libc::posix_spawnattr_destroy);

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
    // Applied in order, after the standard streams: `Spawn::spawn` refuses a
    // source that is a standard stream or that an earlier entry overwrites,
    // so each target receives the descriptor its entry names.
    for &(from, to) in &request.inherit {
        let from = from.as_raw_fd();
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

#[cfg(test)]
mod tests {
    use super::Destroyed;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A destroyer that counts its calls in the counter its object points at.
    unsafe extern "C" fn count(object: *mut *const AtomicUsize) -> libc::c_int {
        // SAFETY: the test below wraps a pointer to a counter that outlives
        // the guard.
        unsafe { (**object).fetch_add(1, Ordering::SeqCst) };
        0
    }

    /// The guard destroys its object when it drops, once, and not before:
    /// what keeps a spawn from leaking its file actions and attributes.
    #[test]
    fn the_guard_destroys_its_object_exactly_once_when_it_drops() {
        let destroyed = AtomicUsize::new(0);
        let guard = Destroyed(&destroyed as *const AtomicUsize, count);
        assert_eq!(destroyed.load(Ordering::SeqCst), 0, "alive: not destroyed");
        drop(guard);
        assert_eq!(
            destroyed.load(Ordering::SeqCst),
            1,
            "dropped: destroyed once"
        );
    }
}
