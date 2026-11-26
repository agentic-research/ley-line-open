//! Every daemon spawn hands a child exactly what it names (ADR-0046 Slice 1
//! S7; bead `ley-line-open-528525`).
//!
//! N children are spawned concurrently, each handed the write end of a pipe
//! of its own. Every write end is held WITHOUT close-on-exec while the
//! children are spawned, the worst case of the macOS window between `pipe`
//! and `fcntl(FD_CLOEXEC)`: a spawn that let anything unnamed cross the exec
//! would hand every sibling's write end to every child. Once every child has
//! exec'd, each write end is held only by this process and its own child,
//! a descriptor this process holds without close-on-exec, and names to no
//! child, is absent in every child, and every child's descriptors are its
//! standard streams and the one it was named, nothing more (ADR-0046 S11).

use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Barrier};

use leyline_runtime::backends::spawn::{ChildIo, Spawn, Spawned};

mod common;

const CHILDREN: usize = 16;
/// The number each child holds its write end at.
const CHILD_FD: RawFd = 3;
/// The lowest number the unnamed descriptor is placed at: far from
/// [`CHILD_FD`], so a child's `dup2` onto [`CHILD_FD`] never replaces it and
/// its absence in a child is the spawn's doing.
const STRAY_MIN_FD: RawFd = 64;

/// An identifier for one END of the pipe `fd` is, the same in every process
/// that holds that end. macOS: `lsof`'s address for the end, which differs
/// between the two ends of one pipe. Linux: `/proc/<pid>/fd/<fd>` reads
/// `pipe:[inode]` for BOTH ends, so the access mode from `fdinfo` is added:
/// without it, this process's read end of a pipe counts as a holder of its
/// write end.
fn pipe_id(pid: u32, fd: RawFd) -> String {
    pipe_id_of(pid, fd).unwrap_or_else(|| panic!("no pipe at descriptor {fd} in {pid}"))
}

/// [`pipe_id`], or `None` when `pid` holds no pipe at `fd`.
fn pipe_id_of(pid: u32, fd: RawFd) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let link = std::fs::read_link(format!("/proc/{pid}/fd/{fd}")).ok()?;
        let link = link.display().to_string();
        if !link.starts_with("pipe:") {
            return None;
        }
        let info = std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}")).ok()?;
        let flags = info
            .lines()
            .find_map(|line| line.strip_prefix("flags:"))
            .and_then(|flags| i32::from_str_radix(flags.trim(), 8).ok())?;
        Some(format!("{link} accmode={}", flags & libc::O_ACCMODE))
    }
    #[cfg(target_os = "macos")]
    {
        descriptors(&[pid])
            .into_iter()
            .find(|&(_, at, _)| at == fd)
            .and_then(|(_, _, pipe)| pipe)
    }
}

/// Every (pid, fd) among `pids` holding the pipe end `id`.
fn holders(id: &str, pids: &[u32]) -> Vec<(u32, RawFd)> {
    descriptors(pids)
        .into_iter()
        .filter(|(_, fd, pipe)| *fd > 2 && pipe.as_deref() == Some(id))
        .map(|(pid, fd, _)| (pid, fd))
        .collect()
}

/// Every numbered descriptor of `pids`, with its pipe-end identifier
/// ([`pipe_id`]) when it is one end of a pipe.
fn descriptors(pids: &[u32]) -> Vec<(u32, RawFd, Option<String>)> {
    #[cfg(target_os = "linux")]
    {
        let mut all = Vec::new();
        for &pid in pids {
            for fd in open_fds(pid) {
                all.push((pid, fd, pipe_id_of(pid, fd)));
            }
        }
        all
    }
    #[cfg(target_os = "macos")]
    {
        let list = pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let output = std::process::Command::new("/usr/sbin/lsof")
            .args(["-a", "-n", "-P", "-p", &list, "-F", "pftd"])
            .output()
            .expect("run lsof");
        let mut all = Vec::new();
        let mut pid = 0u32;
        let mut current: Option<(RawFd, bool, Option<String>)> = None;
        let flush = |pid: u32, current: Option<(RawFd, bool, Option<String>)>, all: &mut Vec<_>| {
            if let Some((fd, pipe, device)) = current {
                all.push((pid, fd, device.filter(|_| pipe)));
            }
        };
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let (tag, value) = line.split_at(1);
            match tag {
                "p" => {
                    flush(pid, current.take(), &mut all);
                    pid = value.parse().unwrap();
                }
                "f" => {
                    flush(pid, current.take(), &mut all);
                    current = value.parse().ok().map(|fd| (fd, false, None));
                }
                "t" => {
                    if let Some(entry) = current.as_mut() {
                        entry.1 = value == "PIPE";
                    }
                }
                "d" => {
                    if let Some(entry) = current.as_mut() {
                        entry.2 = Some(value.to_owned());
                    }
                }
                _ => {}
            }
        }
        flush(pid, current, &mut all);
        all
    }
}

/// The descriptor numbers open in `pid`.
fn open_fds(pid: u32) -> Vec<RawFd> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_dir(format!("/proc/{pid}/fd"))
            .expect("list the process's descriptors")
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
            .collect()
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("/usr/sbin/lsof")
            .args(["-a", "-n", "-P", "-p", &pid.to_string(), "-F", "f"])
            .output()
            .expect("run lsof");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.strip_prefix('f')?.parse().ok())
            .collect()
    }
}

/// Those of `in_child`, the descriptors `pid` (a `cat` spawned with
/// [`CHILD_FD`]) holds, that it was not named and that came from this
/// process, each with what it is.
///
/// macOS: every one beyond the standard streams and [`CHILD_FD`]. The
/// kernel builds the child from the file actions and `/bin/cat` opens
/// nothing itself.
///
/// Linux: those that are also something this process holds (the same
/// `/proc/<pid>/fd` link: the same pipe, socket or path). `cat` may open
/// descriptors of its own after its exec (the Rust coreutils `cat` of
/// Ubuntu 26.04 makes a pipe to `splice` through), and `/proc` does not say
/// which descriptors crossed the exec; a pipe `cat` made is one this
/// process never held.
fn inherited_without_being_named(pid: u32, in_child: &[RawFd]) -> Vec<(RawFd, String)> {
    let not_named = in_child
        .iter()
        .copied()
        .filter(|fd| ![0, 1, 2, CHILD_FD].contains(fd));
    #[cfg(target_os = "linux")]
    {
        let link = |pid: u32, fd: RawFd| std::fs::read_link(format!("/proc/{pid}/fd/{fd}")).ok();
        let me = std::process::id();
        let mine: Vec<std::path::PathBuf> = open_fds(me)
            .into_iter()
            .filter(|fd| *fd > 2)
            .filter_map(|fd| link(me, fd))
            .collect();
        not_named
            .filter_map(|fd| {
                let held = link(pid, fd)?;
                mine.contains(&held)
                    .then(|| (fd, held.display().to_string()))
            })
            .collect()
    }
    #[cfg(target_os = "macos")]
    {
        let _ = pid;
        not_named
            .map(|fd| (fd, "cat opens nothing itself".to_owned()))
            .collect()
    }
}

/// Clear `fd`'s close-on-exec flag.
fn clear_cloexec(fd: RawFd) {
    // SAFETY: fcntl on a descriptor the caller owns; no memory is passed.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        assert!(flags >= 0, "F_GETFD {fd}");
        assert!(libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) >= 0);
    }
}

/// Wait until `child` (a `cat` on piped stdin and stdout) has exec'd: it
/// echoes a line only once it runs `cat`, after its exec. Every descriptor
/// it holds from then on crossed that exec.
fn await_exec(child: &mut Spawned) {
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"exec'd\n")
        .unwrap();
    let mut echo = [0u8; 7];
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut echo)
        .unwrap();
    assert_eq!(&echo, b"exec'd\n", "child {}", child.id());
}

#[test]
fn concurrent_spawns_each_inherit_only_their_own_write_end() {
    let _serial = common::serial();
    // A descriptor this process holds WITHOUT close-on-exec and names to no
    // child: no child may hold it.
    let stray = {
        let null = std::fs::File::open("/dev/null").unwrap();
        // SAFETY: F_DUPFD duplicates a live descriptor; the result is owned
        // below. F_DUPFD (not F_DUPFD_CLOEXEC) leaves close-on-exec clear.
        let raw = unsafe { libc::fcntl(null.as_raw_fd(), libc::F_DUPFD, STRAY_MIN_FD) };
        assert!(raw >= STRAY_MIN_FD, "F_DUPFD");
        // SAFETY: `raw` is a fresh descriptor nothing else owns.
        unsafe { OwnedFd::from_raw_fd(raw) }
    };
    let barrier = Arc::new(Barrier::new(CHILDREN));
    let mut spawned: Vec<(Spawned, OwnedFd, OwnedFd)> = (0..CHILDREN)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let (reader, writer) = std::io::pipe().unwrap();
                let (reader, writer) = (OwnedFd::from(reader), OwnedFd::from(writer));
                // Every write end crosses an exec unless the spawn itself
                // keeps it from doing so.
                clear_cloexec(writer.as_raw_fd());
                barrier.wait();
                let mut spawn = Spawn::new("/bin/cat");
                spawn
                    .stdin(ChildIo::Piped)
                    .stdout(ChildIo::Piped)
                    .inherit(writer.as_fd(), CHILD_FD)
                    .process_group();
                let child = spawn.spawn().expect("spawn");
                (child, reader, writer)
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();

    // Sample only once every child has exec'd. Until a child execs, it is a
    // fork of this process and holds a copy of every descriptor this process
    // held at the fork, its siblings' write ends among them (on Linux, where
    // the spawn forks; see `backends::spawn`). That copy closes at the exec.
    for (child, _, _) in &mut spawned {
        await_exec(child);
    }

    let me = std::process::id();
    let mut pids: Vec<u32> = spawned.iter().map(|(child, _, _)| child.id()).collect();
    pids.push(me);
    for (child, _, writer) in &spawned {
        let id = pipe_id(me, writer.as_raw_fd());
        let mut held = holders(&id, &pids);
        held.sort();
        let mut expected = vec![(me, writer.as_raw_fd()), (child.id(), CHILD_FD)];
        expected.sort();
        assert_eq!(
            held,
            expected,
            "write end {id} must be held only by this process and child {}",
            child.id()
        );
        // The whole of what the child holds, with sixteen spawns in flight
        // at once: its three standard streams, the one descriptor it was
        // named, and nothing else that came from this process.
        let in_child = open_fds(child.id());
        for given in [0, 1, 2, CHILD_FD] {
            assert!(
                in_child.contains(&given),
                "child {} lacks descriptor {given}: {in_child:?}",
                child.id()
            );
        }
        let not_given = inherited_without_being_named(child.id(), &in_child);
        assert!(
            not_given.is_empty(),
            "child {} holds descriptors it was not given: {not_given:?} (all: {in_child:?})",
            child.id()
        );
        assert!(
            !in_child.contains(&stray.as_raw_fd()),
            "child {} holds descriptor {}, which nothing named: {in_child:?}",
            child.id(),
            stray.as_raw_fd()
        );
    }
    for (mut child, _, _) in spawned {
        drop(child.stdin.take());
        let status = child.wait().unwrap();
        assert!(status.success(), "{status}");
    }
}

/// The krun worker's shape: a descriptor that is close-on-exec here,
/// inherited at its own number (`posix_spawn_file_actions_addinherit_np` on
/// macOS, the flag cleared in the child on Linux). The child holds it, at
/// that number, after its exec.
#[test]
fn a_close_on_exec_descriptor_inherited_at_its_own_number_reaches_the_child() {
    let _serial = common::serial();
    let (_reader, writer) = std::io::pipe().unwrap();
    let writer = OwnedFd::from(writer);
    // SAFETY: F_GETFD on a live descriptor.
    let flags = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFD) };
    assert_ne!(
        flags & libc::FD_CLOEXEC,
        0,
        "the pipe is close-on-exec here"
    );
    let mut spawn = Spawn::new("/bin/cat");
    spawn
        .stdin(ChildIo::Piped)
        .stdout(ChildIo::Piped)
        .inherit(writer.as_fd(), writer.as_raw_fd());
    let mut child = spawn.spawn().expect("spawn");
    await_exec(&mut child);
    let me = std::process::id();
    let id = pipe_id(me, writer.as_raw_fd());
    let mut held = holders(&id, &[me, child.id()]);
    held.sort();
    let mut expected = vec![(me, writer.as_raw_fd()), (child.id(), writer.as_raw_fd())];
    expected.sort();
    assert_eq!(held, expected, "write end {id}");
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
}

/// Each refusal happens before anything is spawned and names what is
/// wrong: an inherited descriptor placed on a standard stream's number or
/// named twice, a source that is a standard stream (the child's new one by
/// then), a source that is another entry's target (applied in order, the
/// moves could hand over that entry's descriptor instead), and a program
/// that is no absolute path
/// (it would be looked up on `PATH` or in the working directory).
#[test]
fn a_spawn_that_would_not_hand_over_what_it_names_is_refused_by_name() {
    let (a, b) = std::io::pipe().unwrap();
    let (a, b) = (OwnedFd::from(a), OwnedFd::from(b));
    let (fa, fb) = (a.as_raw_fd(), b.as_raw_fd());
    let (stdin, stderr) = (std::io::stdin(), std::io::stderr());
    let cases: Vec<(Vec<(std::os::fd::BorrowedFd<'_>, RawFd)>, &str)> = vec![
        (vec![(a.as_fd(), 0)], "standard stream"),
        (vec![(a.as_fd(), 2)], "standard stream"),
        (vec![(a.as_fd(), 40), (b.as_fd(), 40)], "named twice"),
        (vec![(stdin.as_fd(), 40)], "standard stream"),
        (vec![(stderr.as_fd(), 40)], "standard stream"),
        // fa -> fb runs first and overwrites fb, so fb -> 41 would hand the
        // child fa's pipe end as 41.
        (vec![(a.as_fd(), fb), (b.as_fd(), 41)], "another entry"),
        (vec![(b.as_fd(), 41), (a.as_fd(), fb)], "another entry"),
    ];
    for (inherit, reason) in cases {
        let mut spawn = Spawn::new("/bin/cat");
        for &(from, to) in &inherit {
            spawn.inherit(from, to);
        }
        let error = spawn.spawn().expect_err("refused");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::InvalidInput,
            "{inherit:?}"
        );
        assert!(
            error.to_string().contains(reason),
            "{inherit:?}: {error} (expected {reason:?}; descriptors {fa}, {fb})"
        );
    }
    for program in ["true", "bin/true", "./true"] {
        let error = Spawn::new(program).spawn().expect_err("refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{program}");
        assert!(
            error.to_string().contains("absolute path") && error.to_string().contains(program),
            "{program}: {error}"
        );
    }
    // The same descriptor at its own number, and moved, is accepted.
    let mut spawn = Spawn::new("/bin/cat");
    spawn.inherit(a.as_fd(), fa).inherit(b.as_fd(), 40);
    assert!(spawn.spawn().unwrap().wait().unwrap().success());
}
